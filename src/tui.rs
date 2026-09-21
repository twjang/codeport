use crate::config::{
    config_path, Access, AgentBinding, Auth, Backend, Config, Protocol, SearchProvider,
};
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};

pub fn run(alternate_path: Option<&std::path::Path>) -> Result<()> {
    let path = match alternate_path {
        Some(path) => path.to_owned(),
        None => config_path()?,
    };
    let mut config = Config::load_from(&path)?;
    let theme = ColorfulTheme::default();
    println!("\n  codeport · backend configuration\n");
    println!("Credentials: {} (permissions: 0600)", path.display());
    println!("Changes are saved after each completed action.\n");
    loop {
        let summary = format!(
            "{} backends · {} agent bindings",
            config.backends.len(),
            config.agents.len()
        );
        let choice = Select::with_theme(&theme)
            .with_prompt(summary)
            .items(&[
                "Add backend",
                "Edit backend",
                "Delete backend",
                "Bind agent",
                "Configure web search",
                "View configuration",
                "Exit",
            ])
            .default(0)
            .interact_opt()?;
        let mut updated = config.clone();
        match choice {
            Some(0) => {
                let name = Input::<String>::with_theme(&theme)
                    .with_prompt("Backend name")
                    .validate_with(|v: &String| -> std::result::Result<(), &str> {
                        if v.trim().is_empty() {
                            Err("Enter a backend name")
                        } else if config.backends.contains_key(v.trim()) {
                            Err("That backend already exists")
                        } else {
                            Ok(())
                        }
                    })
                    .interact_text()?;
                updated
                    .backends
                    .insert(name.trim().to_string(), edit_backend(None)?);
            }
            Some(1) => {
                let Some(name) = select_backend(&config)? else {
                    continue;
                };
                let backend = edit_backend(config.backends.get(&name))?;
                updated.backends.insert(name, backend);
            }
            Some(2) => {
                let Some(name) = select_backend(&config)? else {
                    continue;
                };
                let bindings = config
                    .agents
                    .iter()
                    .filter(|(_, b)| b.backend == name)
                    .map(|(a, _)| a.as_str())
                    .collect::<Vec<_>>();
                let prompt = if bindings.is_empty() {
                    format!("Delete backend {name}?")
                } else {
                    format!(
                        "Delete {name} and remove bindings for {}?",
                        bindings.join(", ")
                    )
                };
                if !Confirm::with_theme(&theme)
                    .with_prompt(prompt)
                    .default(false)
                    .interact()?
                {
                    continue;
                }
                updated.backends.remove(&name);
                updated.agents.retain(|_, binding| binding.backend != name);
            }
            Some(3) => {
                let agents = ["pi", "opencode", "codex", "claude"];
                let Some(index) = Select::with_theme(&theme)
                    .with_prompt("Agent")
                    .items(&agents)
                    .default(0)
                    .interact_opt()?
                else {
                    continue;
                };
                let Some(backend) = select_backend(&config)? else {
                    continue;
                };
                let existing = config
                    .agents
                    .get(agents[index])
                    .and_then(|b| b.model.as_deref());
                let model = optional_input(
                    "Model override (empty: use backend default or agent selection)",
                    existing,
                )?;
                updated
                    .agents
                    .insert(agents[index].to_string(), AgentBinding { backend, model });
            }
            Some(4) => {
                updated.web_search = edit_search(&config.web_search)?;
            }
            Some(5) => {
                show(&config);
                continue;
            }
            _ => return Ok(()),
        }
        let saved = match alternate_path {
            Some(_) => updated.save_at(&path),
            None => updated.save(),
        };
        match saved {
            Ok(()) => {
                config = updated;
                println!("Saved to {}\n", path.display());
            }
            Err(error) => eprintln!("Changes were not saved: {error:#}"),
        }
    }
}

fn select_backend(config: &Config) -> Result<Option<String>> {
    if config.backends.is_empty() {
        println!("Add a backend first.\n");
        return Ok(None);
    }
    let names: Vec<_> = config.backends.keys().cloned().collect();
    Ok(Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Backend")
        .items(&names)
        .default(0)
        .interact_opt()?
        .map(|i| names[i].clone()))
}

fn optional_input(prompt: &str, existing: Option<&str>) -> Result<Option<String>> {
    let value: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .with_initial_text(existing.unwrap_or(""))
        .allow_empty(true)
        .interact_text()?;
    Ok((!value.trim().is_empty()).then(|| value.trim().to_string()))
}

fn edit_backend(existing: Option<&Backend>) -> Result<Backend> {
    let theme = ColorfulTheme::default();
    let url: String = Input::with_theme(&theme)
        .with_prompt("Backend base URL (include /v1 if required)")
        .with_initial_text(
            existing
                .map(|b| b.url.as_str())
                .unwrap_or("http://127.0.0.1:8000/v1"),
        )
        .validate_with(|value: &String| -> std::result::Result<(), &str> {
            match reqwest::Url::parse(value) {
                Ok(u)
                    if matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some()
                        && u.username().is_empty()
                        && u.password().is_none()
                        && u.query().is_none()
                        && u.fragment().is_none() =>
                {
                    Ok(())
                }
                _ => Err(
                    "Enter an HTTP(S) base URL without credentials, query parameters, or fragments",
                ),
            }
        })
        .interact_text()?;
    let protocols = [
        Protocol::ChatCompletions,
        Protocol::Responses,
        Protocol::Anthropic,
    ];
    let default = existing
        .and_then(|b| protocols.iter().position(|p| *p == b.protocol))
        .unwrap_or(0);
    let protocol = protocols[Select::with_theme(&theme)
        .with_prompt("Backend API protocol")
        .items(&[
            "OpenAI Chat Completions",
            "OpenAI Responses",
            "Anthropic Messages",
        ])
        .default(default)
        .interact()?];
    let model = optional_input(
        "Default model (empty: preserve agent selection)",
        existing.and_then(|b| b.model.as_deref()),
    )?;
    let auth = edit_auth(existing.and_then(|b| b.auth.as_ref()))?;
    let default = usize::from(existing.is_some_and(|b| b.access.is_some()));
    let access = match Select::with_theme(&theme)
        .with_prompt("Backend access")
        .items(&["Direct", "Command"])
        .default(default)
        .interact()?
    {
        0 => None,
        _ => {
            let old = existing.and_then(|b| b.access.as_ref());
            println!("Commands run via /bin/sh -c without interactive input.");
            let command = Input::<String>::with_theme(&theme)
                .with_prompt("Access command")
                .with_initial_text(old.map(|a| a.command.as_str()).unwrap_or(""))
                .validate_with(|s: &String| {
                    if s.trim().is_empty() {
                        Err("Command cannot be empty")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()?;
            let persistent = Select::with_theme(&theme)
                .with_prompt("Command lifecycle")
                .items(&[
                    "Finish before launching the agent",
                    "Stay running during the agent session",
                ])
                .default(usize::from(old.is_some_and(|a| a.persistent)))
                .interact()?
                == 1;
            let timeout_secs = Input::<u64>::with_theme(&theme)
                .with_prompt("Readiness timeout in seconds")
                .default(old.map(|a| a.timeout_secs).unwrap_or(30))
                .validate_with(|n: &u64| {
                    if *n > 0 {
                        Ok(())
                    } else {
                        Err("Timeout must be positive")
                    }
                })
                .interact_text()?;
            let cleanup = optional_input(
                "Cleanup command (optional)",
                old.and_then(|a| a.cleanup.as_deref()),
            )?;
            Some(Access {
                command,
                persistent,
                cleanup,
                timeout_secs,
            })
        }
    };
    Ok(Backend {
        url,
        protocol,
        model,
        auth,
        access,
    })
}

fn edit_auth(existing: Option<&Auth>) -> Result<Option<Auth>> {
    let theme = ColorfulTheme::default();
    if existing.is_some()
        && Confirm::with_theme(&theme)
            .with_prompt("Keep existing authentication credentials?")
            .default(true)
            .interact()?
    {
        return Ok(existing.cloned());
    }
    Ok(
        match Select::with_theme(&theme)
            .with_prompt("Authentication")
            .items(&["None", "Bearer token", "HTTP Basic"])
            .default(0)
            .interact()?
        {
            1 => Some(Auth::Bearer {
                token: Password::with_theme(&theme)
                    .with_prompt("Bearer token")
                    .interact()?,
            }),
            2 => {
                let username = Input::<String>::with_theme(&theme)
                    .with_prompt("Username")
                    .validate_with(|s: &String| {
                        if s.contains(':') {
                            Err("Username cannot contain ':'")
                        } else {
                            Ok(())
                        }
                    })
                    .interact_text()?;
                let password = Password::with_theme(&theme)
                    .with_prompt("Password")
                    .allow_empty_password(true)
                    .interact()?;
                Some(Auth::Basic { username, password })
            }
            _ => None,
        },
    )
}

fn edit_search(existing: &SearchProvider) -> Result<SearchProvider> {
    let theme = ColorfulTheme::default();
    let default = match existing {
        SearchProvider::Public => 0,
        SearchProvider::Searxng { .. } => 1,
        SearchProvider::Brave { .. } => 2,
    };
    let Some(choice) = Select::with_theme(&theme)
        .with_prompt("Web search provider")
        .items(&[
            "Public search (DuckDuckGo, no API key)",
            "SearXNG instance",
            "Brave Search API",
        ])
        .default(default)
        .interact_opt()?
    else {
        return Ok(existing.clone());
    };
    let provider = match choice {
        0 => SearchProvider::Public,
        1 => {
            let current = match existing {
                SearchProvider::Searxng { url } => url.as_str(),
                _ => "http://localhost:8080",
            };
            let url: String = Input::with_theme(&theme)
                .with_prompt("SearXNG base URL (JSON output must be enabled)")
                .with_initial_text(current)
                .validate_with(|url: &String| {
                    SearchProvider::Searxng {
                        url: url.trim().to_owned(),
                    }
                    .validate()
                    .map_err(|err| err.to_string())
                })
                .interact_text()?;
            SearchProvider::Searxng {
                url: url.trim().to_owned(),
            }
        }
        _ => {
            let current = match existing {
                SearchProvider::Brave { api_key } => Some(api_key),
                _ => None,
            };
            let key = Password::with_theme(&theme)
                .with_prompt(if current.is_some() {
                    "Brave API key (empty: keep existing)"
                } else {
                    "Brave API key"
                })
                .allow_empty_password(current.is_some())
                .interact()?;
            SearchProvider::Brave {
                api_key: if key.is_empty() {
                    current.unwrap().clone()
                } else {
                    key
                },
            }
        }
    };
    provider.validate()?;
    Ok(provider)
}

fn show(config: &Config) {
    println!("\nWeb search: {:?}", config.web_search);
    for (name, backend) in &config.backends {
        let auth = match &backend.auth {
            None => "none",
            Some(Auth::Bearer { .. }) => "bearer (hidden)",
            Some(Auth::Basic { .. }) => "HTTP Basic (hidden)",
        };
        let access = match &backend.access {
            None => "direct",
            Some(a) if a.persistent => "persistent command",
            Some(_) => "preparation command",
        };
        println!(
            "\n{name}: {}\n  {:?} · model: {}\n  authentication: {auth} · access: {access}",
            backend.url,
            backend.protocol,
            backend
                .model
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or("agent selection")
        );
    }
    for (agent, binding) in &config.agents {
        println!(
            "\n{agent} → {} · model: {}",
            binding.backend,
            binding
                .model
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or("backend default / agent selection")
        );
    }
    println!();
}
