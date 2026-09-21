//! Process-scoped adapters. No persistent agent configuration is overwritten.
//!
//! References checked during implementation:
//! https://developers.openai.com/codex/config-advanced
//! https://code.claude.com/docs/en/llm-gateway-connect
//! https://opencode.ai/docs/config
//! https://opencode.ai/docs/providers
//! https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/custom-provider.md

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{io::Write, path::Path, process::Command};
use tempfile::{NamedTempFile, TempDir};

use crate::config::Protocol;

/// Keep temporary agent configuration alive until the child exits.
pub struct AgentLaunch {
    pub command: Command,
    _extension: Option<NamedTempFile>,
    _codex_home: Option<TempDir>,
}

/// CLI overrides do not isolate writes made by Codex itself. Give each launch
/// private settings and caches, while retaining access to existing sessions,
/// skills and other persistent resources. Never restore a global config after
/// exit: another Codex process may have legitimately changed it meanwhile.
fn isolated_codex_home(source: &Path) -> Result<TempDir> {
    let home = tempfile::Builder::new()
        .prefix("codeport-codex-")
        .tempdir()?;
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(home),
        Err(error) => return Err(error).context("Reading Codex home"),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        // Runtime IPC must not connect this launch to the ordinary Codex daemon.
        // Model catalogs fetched through the bridge must also stay private.
        if matches!(
            name.to_str(),
            Some("models_cache.json" | "cache" | "ipc" | "tmp" | ".tmp")
        ) {
            continue;
        }
        let source = entry.path();
        let target = home.path().join(&name);
        if matches!(
            source.extension().and_then(|ext| ext.to_str()),
            Some("toml" | "json")
        ) {
            std::fs::copy(&source, &target)
                .with_context(|| format!("Copying Codex settings {}", source.display()))?;
        } else {
            std::os::unix::fs::symlink(std::path::absolute(&source)?, &target)
                .with_context(|| format!("Linking Codex resource {}", source.display()))?;
        }
    }
    Ok(home)
}

impl AgentLaunch {
    /// Move the process configuration out while retaining temporary resources.
    pub fn take_command(&mut self) -> Command {
        std::mem::replace(
            &mut self.command,
            Command::new("codeport-command-already-taken"),
        )
    }
}

pub fn protocol(agent: &str) -> Result<Protocol> {
    match agent {
        "codex" => Ok(Protocol::Responses),
        "claude" | "claude-code" => Ok(Protocol::Anthropic),
        "pi" | "opencode" => Ok(Protocol::ChatCompletions),
        _ => bail!("Unknown coding agent {agent:?}; choose pi, opencode, codex, or claude"),
    }
}

/// OpenCode 2's shared server cannot inherit this launch's bridge configuration.
/// Probe the CLI so OpenCode 1, which has no --standalone flag, still works.
pub async fn opencode_args(args: &[String]) -> Result<Vec<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("opencode")
            .arg("--help")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Timed out checking OpenCode's supported launch options")?
    .context("could not launch opencode; install the agent and ensure it is on PATH")?;
    anyhow::ensure!(output.status.success(), "OpenCode --help failed");
    Ok(opencode_session_args(
        args,
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|word| word == "--standalone"),
    ))
}

fn opencode_session_args(args: &[String], standalone: bool) -> Vec<String> {
    let mut args = args.to_vec();
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    if standalone && !args[..end].iter().any(|arg| arg == "--standalone") {
        // Put the flag after the subcommand, but before any literal arguments.
        // OpenCode 2 rejects `opencode --standalone run ...`.
        args.insert(end, "--standalone".to_owned());
    }
    args
}

/// `base_url` is the bridge origin (e.g. http://127.0.0.1:1234), without /v1.
/// Credentials here authenticate only to the local bridge, never the upstream.
pub fn prepare(
    agent: &str,
    base_url: &str,
    token: &str,
    model: Option<&str>,
    args: &[String],
    upstream_protocol: Protocol,
) -> Result<AgentLaunch> {
    protocol(agent)?;
    let base = base_url.trim_end_matches('/');
    let api = format!("{base}/v1");
    let model = model.filter(|m| !m.trim().is_empty());
    let mut command = Command::new(if agent == "claude-code" {
        "claude"
    } else {
        agent
    });
    let mut extension = None;
    let mut codex_home = None;
    command.env("CODEPORT_API_KEY", token);
    match agent {
        "codex" => {
            let source = std::env::var_os("CODEX_HOME")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".codex")))
                .context("HOME and CODEX_HOME are unset; cannot locate Codex settings")?;
            let home = isolated_codex_home(&source)?;
            command.env("CODEX_HOME", home.path());
            codex_home = Some(home);
            if upstream_protocol != Protocol::Responses {
                eprintln!("codeport: protocol conversion disables Codex reasoning and provider-hosted search for this session");
                for setting in [
                    "model_reasoning_effort=\"none\"",
                    "model_reasoning_summary=\"none\"",
                    "web_search=\"disabled\"",
                    "features.tool_search=false",
                ] {
                    command.args(["-c", setting]);
                }
            }
            // CLI overrides apply on top of the private copy of user settings.
            // JSON quoted strings are also valid TOML basic strings for these values.
            for setting in [
                "model_provider=\"codeport\"".to_owned(),
                "model_providers.codeport.name=\"codeport\"".to_owned(),
                format!("model_providers.codeport.base_url={}", json!(api)),
                "model_providers.codeport.env_key=\"CODEPORT_API_KEY\"".to_owned(),
                "model_providers.codeport.wire_api=\"responses\"".to_owned(),
                "model_providers.codeport.requires_openai_auth=false".to_owned(),
                "model_providers.codeport.supports_websockets=false".to_owned(),
            ] {
                command.args(["-c", &setting]);
            }
            if let Some(model) = model {
                command.args(["--model", model]);
            }
        }
        "claude" | "claude-code" => {
            // Allow native WebFetch across domains without depending on Anthropic's
            // external domain preflight service. This is scoped to this launch.
            command.args([
                "--settings",
                r#"{"skipWebFetchPreflight":true}"#,
                "--allowedTools",
                "WebFetch",
            ]);
            if upstream_protocol != Protocol::Anthropic {
                eprintln!("codeport: protocol conversion disables Claude extended thinking and explicit effort for this session");
                command
                    .env("MAX_THINKING_TOKENS", "0")
                    .env("CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING", "1")
                    // Thinking and effort are separate controls. `auto` prevents
                    // recent Claude Code releases sending output_config.effort.
                    .env("CLAUDE_CODE_EFFORT_LEVEL", "auto");
            }
            command
                .env("ANTHROPIC_BASE_URL", base)
                .env("ANTHROPIC_AUTH_TOKEN", token)
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
                .env_remove("CLAUDE_CODE_USE_BEDROCK")
                .env_remove("CLAUDE_CODE_USE_VERTEX")
                .env_remove("CLAUDE_CODE_USE_FOUNDRY");
            if let Some(model) = model {
                command.args(["--model", model]);
                // Auxiliary requests should use the explicit backend model as well.
                for key in [
                    "ANTHROPIC_MODEL",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                    "CLAUDE_CODE_SUBAGENT_MODEL",
                ] {
                    command.env(key, model);
                }
            }
        }
        "opencode" => {
            let config = opencode_config(&api, token, model);
            command.env("OPENCODE_CONFIG_CONTENT", serde_json::to_string(&config)?);
        }
        "pi" => {
            let mut file = tempfile::Builder::new()
                .prefix("codeport-")
                .suffix(".ts")
                .tempfile()
                .context("Creating temporary Pi provider extension")?;
            let script = pi_extension(base, &api, token, model);
            file.write_all(script.as_bytes())?;
            file.flush()?;
            command.arg("--extension").arg(file.path());
            if let Some(model) = model {
                command.args(["--provider", "codeport", "--model", model]);
            }
            extension = Some(file);
        }
        _ => unreachable!(),
    }
    command.args(args);
    Ok(AgentLaunch {
        command,
        _extension: extension,
        _codex_home: codex_home,
    })
}

fn opencode_config(api: &str, token: &str, model: Option<&str>) -> Value {
    if let Some(model) = model {
        json!({
            "$schema":"https://opencode.ai/config.json",
            "enabled_providers":["codeport"],
            "model":format!("codeport/{model}"),
            "small_model":format!("codeport/{model}"),
            "provider":{"codeport":{
                "npm":"@ai-sdk/openai-compatible", "name":"codeport",
                "options":{"baseURL":api,"apiKey":token},
                "models":{(model):{"name":model,"tool_call":true}}
            }}
        })
    } else {
        // Keep the configured model ID. Only protocols handled by the bridge are
        // enabled, preventing a saved Google/Bedrock model bypassing the launcher.
        json!({
            "$schema":"https://opencode.ai/config.json",
            "enabled_providers":["openai","anthropic"],
            "provider":{
                "openai":{"options":{"baseURL":api,"apiKey":token}},
                "anthropic":{"options":{"baseURL":api,"apiKey":token}}
            }
        })
    }
}

fn pi_extension(base: &str, api: &str, token: &str, model: Option<&str>) -> String {
    let headers = json!({"Authorization":format!("Bearer {token}")});
    let mut script = format!(
        "export default function(pi) {{\n  pi.registerProvider('openai', {});\n  pi.registerProvider('anthropic', {});\n",
        json!({"baseUrl":api,"apiKey":token,"headers":headers}),
        json!({"baseUrl":base,"apiKey":token,"headers":headers}),
    );
    if let Some(model) = model {
        let provider = json!({
            "baseUrl":api,"apiKey":token,"headers":headers,"api":"openai-completions",
            "models":[{"id":model,"name":model,"reasoning":false,"input":["text"],
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                "contextWindow":128000,"maxTokens":16384}]
        });
        script.push_str(&format!("  pi.registerProvider('codeport', {provider});\n"));
    }
    // Event exceptions are caught by Pi, so routing failures must terminate the
    // child explicitly. Re-register the selected model under our provider while
    // retaining its exact ID and capability metadata. This also handles saved
    // compatible providers beyond native OpenAI/Anthropic without stale URLs.
    script.push_str(&format!(
        "  const origin = {};\n  const api = {};\n  const token = {};\n",
        json!(base),
        json!(api),
        json!(token)
    ));
    script.push_str(r#"  let routing = false;
  const fail = (error) => {
    process.stderr.write(`codeport: ${error instanceof Error ? error.message : error}\n`);
    process.exit(2);
  };
  const route = async (_event, ctx) => {
    if (routing || !ctx.model || ctx.model.provider === 'codeport') return;
    routing = true;
    try {
      const selected = ctx.model;
      if (!['openai-completions', 'openai-responses', 'anthropic-messages'].includes(selected.api)) {
        fail(`Cannot route Pi API ${selected.api}; select an OpenAI/Anthropic compatible model or configure a model in codeport`);
      }
      const baseUrl = selected.api === 'anthropic-messages' ? origin : api;
      pi.registerProvider('codeport', {
        baseUrl, apiKey: token, api: selected.api,
        headers: { Authorization: `Bearer ${token}` },
        models: [{ ...selected, provider: 'codeport', baseUrl }]
      });
      const model = ctx.modelRegistry.find('codeport', selected.id);
      if (!model || !(await pi.setModel(model))) fail('Unable to configure selected Pi model for the local bridge');
    } catch (error) {
      fail(error);
    } finally {
      routing = false;
    }
  };
  pi.on('session_start', route);
  pi.on('model_select', route);
  pi.on('before_agent_start', route);
}
"#);
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    #[test]
    fn codex_settings_are_private_but_sessions_and_skills_remain_accessible() {
        let original = tempfile::tempdir().unwrap();
        let root = original.path();
        std::fs::write(root.join("config.toml"), "model = \"original\"").unwrap();
        std::fs::write(root.join("work.config.toml"), "model = \"work\"").unwrap();
        std::fs::write(root.join("models_cache.json"), "original catalog").unwrap();
        for name in ["sessions", "skills", "ipc", "cache"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let first = isolated_codex_home(root).unwrap();
        let second = isolated_codex_home(root).unwrap();
        for name in ["config.toml", "work.config.toml"] {
            assert!(!first.path().join(name).is_symlink());
            std::fs::write(first.path().join(name), "model = \"qwen\"").unwrap();
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                std::fs::read(second.path().join(name)).unwrap()
            );
        }
        for name in ["models_cache.json", "ipc", "cache"] {
            assert!(!first.path().join(name).exists());
        }
        for name in ["sessions", "skills"] {
            std::fs::write(first.path().join(name).join("new"), "shared").unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join(name).join("new")).unwrap(),
                "shared"
            );
        }
        // A concurrent ordinary Codex write must survive cleanup.
        std::fs::write(root.join("config.toml"), "model = \"changed\"").unwrap();
        drop(first);
        assert_eq!(
            std::fs::read_to_string(root.join("config.toml")).unwrap(),
            "model = \"changed\""
        );
    }

    #[test]
    fn opencode_standalone_preserves_subcommands_and_literal_arguments() {
        let args = ["run", "--", "literal prompt"].map(str::to_owned);
        assert_eq!(opencode_session_args(&args, false), args);
        assert_eq!(
            opencode_session_args(&args, true),
            ["run", "--standalone", "--", "literal prompt"]
        );
        assert_eq!(opencode_session_args(&[], true), ["--standalone"]);
        let explicit = ["run", "--standalone", "prompt"].map(str::to_owned);
        assert_eq!(opencode_session_args(&explicit, true), explicit);
    }

    fn env(command: &Command, name: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }
    #[test]
    fn empty_model_preserves_agent_choice() {
        for agent in ["codex", "claude", "pi", "opencode"] {
            let launch = prepare(
                agent,
                "http://127.0.0.1:1234",
                "test",
                Some(""),
                &[],
                Protocol::Responses,
            )
            .unwrap();
            assert!(!launch.command.get_args().any(|a| a == "--model"));
            if agent == "opencode" {
                let value: Value =
                    serde_json::from_str(&env(&launch.command, "OPENCODE_CONFIG_CONTENT").unwrap())
                        .unwrap();
                assert!(value.get("model").is_none());
            }
        }
    }
    #[test]
    fn explicit_model_and_arguments_are_literal() {
        let launch = prepare(
            "codex",
            "http://127.0.0.1:1234/",
            "secret",
            Some("model with spaces"),
            &["exec".into(), "$(no shell)".into()],
            Protocol::Responses,
        )
        .unwrap();
        let args: Vec<_> = launch
            .command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|a| a == ["--model", "model with spaces"]));
        assert_eq!(args.last().unwrap(), "$(no shell)");
        assert!(args.iter().any(|a| a.contains("http://127.0.0.1:1234/v1")));
        assert!(!args.iter().any(|a| a.contains("secret")));
    }
    #[test]
    fn pi_extension_survives_until_launch_is_dropped() {
        let launch = prepare(
            "pi",
            "http://127.0.0.1:1234",
            "token",
            Some("local/model"),
            &[],
            Protocol::Responses,
        )
        .unwrap();
        let path = launch._extension.as_ref().unwrap().path().to_owned();
        let script = std::fs::read_to_string(&path).unwrap();
        assert!(script.contains("local/model"));
        assert!(script.contains("openai-completions"));
        drop(launch);
        assert!(!path.exists());
    }
    #[test]
    fn taking_command_keeps_pi_extension_alive() {
        let mut launch = prepare(
            "pi",
            "http://localhost:1234",
            "token",
            None,
            &[],
            Protocol::Responses,
        )
        .unwrap();
        let path = launch._extension.as_ref().unwrap().path().to_owned();
        let command = launch.take_command();
        assert_eq!(command.get_program(), "pi");
        assert!(path.exists());
        drop(command);
        assert!(path.exists());
        drop(launch);
        assert!(!path.exists());
    }

    #[test]
    fn claude_uses_origin_and_clears_competing_auth() {
        let launch = prepare(
            "claude",
            "http://localhost:1234/",
            "token",
            None,
            &[],
            Protocol::Responses,
        )
        .unwrap();
        assert_eq!(
            env(&launch.command, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://localhost:1234")
        );
        assert_eq!(
            env(&launch.command, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("token")
        );
        assert!(launch
            .command
            .get_envs()
            .any(|(key, val)| key == "ANTHROPIC_API_KEY" && val.is_none()));
    }
    #[test]
    fn claude_preserves_default_search_tool() {
        for protocol in [
            Protocol::ChatCompletions,
            Protocol::Responses,
            Protocol::Anthropic,
        ] {
            let launch = prepare(
                "claude",
                "http://localhost:1234",
                "token",
                None,
                &[],
                protocol,
            )
            .unwrap();
            let args: Vec<_> = launch
                .command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(!args.iter().any(|arg| arg == "--disallowedTools"));
            assert!(args
                .windows(2)
                .any(|args| args == ["--allowedTools", "WebFetch"]));
            let settings = args
                .windows(2)
                .find(|args| args[0] == "--settings")
                .unwrap();
            let settings: Value = serde_json::from_str(&settings[1]).unwrap();
            assert_eq!(settings["skipWebFetchPreflight"], true);
            assert!(!args.iter().any(|arg| arg.contains("skip-permissions")));
            assert!(!args.iter().any(|arg| arg == "--mcp-config"));
            assert!(launch._extension.is_none());
        }
    }

    #[test]
    fn opencode_registers_custom_model() {
        let config = opencode_config("http://localhost/v1", "token", Some("local/model"));
        assert_eq!(config["model"], "codeport/local/model");
        assert_eq!(
            config["provider"]["codeport"]["models"]["local/model"]["tool_call"],
            true
        );
    }
    #[test]
    fn conversion_defaults_are_scoped_to_cross_protocol_launches() {
        let native = prepare(
            "codex",
            "http://localhost",
            "t",
            None,
            &[],
            Protocol::Responses,
        )
        .unwrap();
        let converted = prepare(
            "codex",
            "http://localhost",
            "t",
            None,
            &[],
            Protocol::ChatCompletions,
        )
        .unwrap();
        assert!(!native
            .command
            .get_args()
            .any(|a| a == "model_reasoning_effort=\"none\""));
        assert!(converted
            .command
            .get_args()
            .any(|a| a == "model_reasoning_effort=\"none\""));
        let native = prepare(
            "claude",
            "http://localhost",
            "t",
            None,
            &[],
            Protocol::Anthropic,
        )
        .unwrap();
        let converted = prepare(
            "claude",
            "http://localhost",
            "t",
            None,
            &[],
            Protocol::ChatCompletions,
        )
        .unwrap();
        assert!(env(&native.command, "MAX_THINKING_TOKENS").is_none());
        assert!(env(&native.command, "CLAUDE_CODE_EFFORT_LEVEL").is_none());
        assert_eq!(
            env(&converted.command, "CLAUDE_CODE_EFFORT_LEVEL").as_deref(),
            Some("auto")
        );
        assert_eq!(
            env(&converted.command, "MAX_THINKING_TOKENS").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn unsupported_agent_is_rejected() {
        assert!(prepare(
            "unknown",
            "http://localhost",
            "t",
            None,
            &[],
            Protocol::Responses
        )
        .is_err());
    }
}
