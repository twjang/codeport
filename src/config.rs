use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    ChatCompletions,
    Responses,
    Anthropic,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub backends: BTreeMap<String, Backend>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentBinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub url: String,
    pub protocol: Protocol,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub access: Option<Access>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    Bearer { token: String },
    Basic { username: String, password: String },
}
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer { .. } => f.write_str("Bearer { token: [redacted] }"),
            Self::Basic { .. } => {
                f.write_str("Basic { username: [redacted], password: [redacted] }")
            }
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub command: String,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default)]
    pub cleanup: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}
fn default_timeout() -> u64 {
    30
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBinding {
    pub backend: String,
    #[serde(default)]
    pub model: Option<String>,
}

pub fn config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("HOME is not set; cannot locate codeport configuration")?;
    Ok(PathBuf::from(home).join(".config/codeport/credential.json"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).context("Cannot inspect configuration file"),
            Ok(metadata) => {
                if !metadata.is_file() {
                    bail!("Configuration must be a regular file: {}", path.display());
                }
                #[cfg(unix)]
                if metadata.permissions().mode() & 0o777 != 0o600 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                        .context("Cannot secure configuration permissions to 0600")?;
                }
            }
        }
        let config: Self = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?,
        )
        .with_context(|| format!("Invalid configuration in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, backend) in &self.backends {
            if name.trim().is_empty() {
                bail!("Backend names cannot be empty");
            }
            let url = reqwest::Url::parse(&backend.url)
                .with_context(|| format!("Backend {name}: invalid URL"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                bail!("Backend {name}: URL must use HTTP or HTTPS and include a host");
            }
            if !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || url.query().is_some()
            {
                bail!("Backend {name}: use a base URL without credentials, query parameters, or fragments");
            }
            if let Some(auth) = &backend.auth {
                match auth {
                    Auth::Bearer { token }
                        if token.is_empty()
                            || reqwest::header::HeaderValue::from_str(&format!(
                                "Bearer {token}"
                            ))
                            .is_err() =>
                    {
                        bail!("Backend {name}: invalid bearer token")
                    }
                    Auth::Basic { username, .. } if username.contains(':') => {
                        bail!("Backend {name}: Basic authentication username cannot contain ':'")
                    }
                    _ => {}
                }
            }
            if let Some(access) = &backend.access {
                if access.command.trim().is_empty() {
                    bail!("Backend {name}: access command cannot be empty");
                }
                if access.timeout_secs == 0 {
                    bail!("Backend {name}: readiness timeout must be greater than zero");
                }
            }
        }
        for (agent, binding) in &self.agents {
            if !["pi", "opencode", "codex", "claude"].contains(&agent.as_str()) {
                bail!("Unknown agent binding: {agent}");
            }
            if !self.backends.contains_key(&binding.backend) {
                bail!(
                    "Agent {agent} refers to missing backend {}",
                    binding.backend
                );
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.save_at(&config_path()?)
    }

    pub fn save_at(&self, path: &std::path::Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).context("Cannot create configuration directory")?;
        #[cfg(unix)]
        if !parent_existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .context("Cannot secure configuration directory")?;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = parent.join(format!(".credential.{}.{}.tmp", std::process::id(), nonce));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temporary)
                .context("Cannot create private configuration file")?;
            let mut bytes = serde_json::to_vec_pretty(self)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path).context("Cannot save configuration")?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| format!("Could not save {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_dangling_bindings_and_embedded_secrets() {
        let mut config = Config::default();
        config.agents.insert(
            "codex".into(),
            AgentBinding {
                backend: "missing".into(),
                model: None,
            },
        );
        assert!(config.validate().is_err());
        config.backends.insert(
            "missing".into(),
            Backend {
                url: "https://user:secret@example.com/v1".into(),
                protocol: Protocol::Responses,
                model: None,
                auth: None,
                access: None,
            },
        );
        assert!(config.validate().is_err());
        config.backends.get_mut("missing").unwrap().url = "https://example.com/v1".into();
        assert!(config.validate().is_ok());
    }
    #[test]
    #[cfg(unix)]
    fn saves_credentials_privately_and_replaces_existing_file() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "codeport-config-test-{}-{nonce}",
            std::process::id()
        ));
        let path = directory.join("credential.json");
        let mut config = Config::default();
        config.save_at(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        config.backends.insert(
            "local".into(),
            Backend {
                url: "http://localhost:8000/v1".into(),
                protocol: Protocol::ChatCompletions,
                model: None,
                auth: Some(Auth::Bearer {
                    token: "test-secret".into(),
                }),
                access: None,
            },
        );
        config.save_at(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let restored: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(restored.backends.contains_key("local"));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn secrets_do_not_appear_in_debug_output() {
        assert!(!format!(
            "{:?}",
            Auth::Bearer {
                token: "secret-token".into()
            }
        )
        .contains("secret-token"));
        assert!(!format!(
            "{:?}",
            Auth::Basic {
                username: "alice".into(),
                password: "secret-password".into()
            }
        )
        .contains("secret-password"));
    }

    #[test]
    fn alternate_config_does_not_chmod_an_existing_parent_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        Config::default()
            .save_at(&directory.path().join("custom.json"))
            .unwrap();
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(directory.path().join("custom.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
