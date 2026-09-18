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
use std::{io::Write, process::Command};
use tempfile::NamedTempFile;

use crate::config::Protocol;

/// Keep this value alive until the child exits: Pi loads its extension lazily.
pub struct AgentLaunch {
    pub command: Command,
    _extension: Option<NamedTempFile>,
}

impl AgentLaunch {
    /// Move the process configuration out while retaining temporary resources.
    pub fn take_command(&mut self) -> Command {
        std::mem::replace(
            &mut self.command,
            Command::new("launchcoder-command-already-taken"),
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
    command.env("LAUNCHCODER_API_KEY", token);
    match agent {
        "codex" => {
            if upstream_protocol != Protocol::Responses {
                eprintln!("launchcoder: protocol conversion disables Codex reasoning and provider-hosted search for this session");
                for setting in [
                    "model_reasoning_effort=\"none\"",
                    "model_reasoning_summary=\"none\"",
                    "web_search=\"disabled\"",
                    "features.tool_search=false",
                ] {
                    command.args(["-c", setting]);
                }
            }
            // CLI overrides retain the user's sessions, tools, permissions and settings.
            // JSON quoted strings are also valid TOML basic strings for these values.
            for setting in [
                "model_provider=\"launchcoder\"".to_owned(),
                "model_providers.launchcoder.name=\"launchcoder\"".to_owned(),
                format!("model_providers.launchcoder.base_url={}", json!(api)),
                "model_providers.launchcoder.env_key=\"LAUNCHCODER_API_KEY\"".to_owned(),
                "model_providers.launchcoder.wire_api=\"responses\"".to_owned(),
                "model_providers.launchcoder.requires_openai_auth=false".to_owned(),
                "model_providers.launchcoder.supports_websockets=false".to_owned(),
            ] {
                command.args(["-c", &setting]);
            }
            if let Some(model) = model {
                command.args(["--model", model]);
            }
        }
        "claude" | "claude-code" => {
            if upstream_protocol != Protocol::Anthropic {
                eprintln!("launchcoder: protocol conversion disables Claude extended thinking for this session");
                command
                    .env("MAX_THINKING_TOKENS", "0")
                    .env("CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING", "1");
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
                .prefix("launchcoder-")
                .suffix(".ts")
                .tempfile()
                .context("Creating temporary Pi provider extension")?;
            let script = pi_extension(base, &api, token, model);
            file.write_all(script.as_bytes())?;
            file.flush()?;
            command.arg("--extension").arg(file.path());
            if let Some(model) = model {
                command.args(["--provider", "launchcoder", "--model", model]);
            }
            extension = Some(file);
        }
        _ => unreachable!(),
    }
    command.args(args);
    Ok(AgentLaunch {
        command,
        _extension: extension,
    })
}

fn opencode_config(api: &str, token: &str, model: Option<&str>) -> Value {
    if let Some(model) = model {
        json!({
            "$schema":"https://opencode.ai/config.json",
            "enabled_providers":["launchcoder"],
            "model":format!("launchcoder/{model}"),
            "small_model":format!("launchcoder/{model}"),
            "provider":{"launchcoder":{
                "npm":"@ai-sdk/openai-compatible", "name":"launchcoder",
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
        script.push_str(&format!(
            "  pi.registerProvider('launchcoder', {provider});\n"
        ));
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
    process.stderr.write(`launchcoder: ${error instanceof Error ? error.message : error}\n`);
    process.exit(2);
  };
  const route = async (_event, ctx) => {
    if (routing || !ctx.model || ctx.model.provider === 'launchcoder') return;
    routing = true;
    try {
      const selected = ctx.model;
      if (!['openai-completions', 'openai-responses', 'anthropic-messages'].includes(selected.api)) {
        fail(`Cannot route Pi API ${selected.api}; select an OpenAI/Anthropic compatible model or configure a model in launchcoder`);
      }
      const baseUrl = selected.api === 'anthropic-messages' ? origin : api;
      pi.registerProvider('launchcoder', {
        baseUrl, apiKey: token, api: selected.api,
        headers: { Authorization: `Bearer ${token}` },
        models: [{ ...selected, provider: 'launchcoder', baseUrl }]
      });
      const model = ctx.modelRegistry.find('launchcoder', selected.id);
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
    fn opencode_registers_custom_model() {
        let config = opencode_config("http://localhost/v1", "token", Some("local/model"));
        assert_eq!(config["model"], "launchcoder/local/model");
        assert_eq!(
            config["provider"]["launchcoder"]["models"]["local/model"]["tool_call"],
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
