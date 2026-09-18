# launchcoder

Launch Pi, OpenCode, Codex, or Claude Code against your own OpenAI- or Anthropic-compatible model backend. A local API bridge adapts protocols, while optional access commands prepare a tunnel, proxy, or other connection before the agent starts.

`launchcoder` is a Rust executable for Linux and macOS. Coding agents and any access utilities must already be installed and available on `PATH`.

## Quick start

Build with Rust 1.86 or newer:

```sh
cargo build --locked --release
./target/release/launchcoder -cfg
./target/release/launchcoder codex
```

In the configuration UI, add a backend with its API base URL and protocol, then bind an agent to that backend. Credentials and settings are saved after each completed action. The UI displays the configuration file location.

After placing the binary on your `PATH`:

```sh
launchcoder pi
launchcoder opencode --backend home-gpu
launchcoder codex --model my-coding-model
launchcoder claude -- --continue
launchcoder -cfg
launchcoder --config ./credential.json -cfg
launchcoder --config ./credential.json codex
```

Use `--` to forward arguments to the installed agent. Agent sessions run in the current directory. Process-specific settings and temporary files connect agents to the local bridge without rewriting their persistent configuration.

The launcher gives interactive agents the foreground terminal and restores it afterward. It owns the agent's process group, forwards externally received termination signals, and terminates remaining tool subprocesses when the session ends.

## Configuration and credentials

On both Linux and macOS, the file is:

```text
~/.config/launchcoder/credential.json
```

`--config PATH` selects an alternate credential file for both configuration UI and launching. Without it, the default path above is used.

The file contains both configuration and credentials. Writes use an atomic replacement with file permissions `0600`; newly created configuration directories use `0700`. Existing parent directory permissions are preserved, including when using `--config`. Loading an existing regular file corrects its permissions to `0600`; a symbolic link is rejected. Credentials are plaintext, protected by filesystem permissions, and are masked in interactive password prompts.

Example configuration:

```json
{
  "backends": {
    "local": {
      "url": "http://127.0.0.1:8000/v1",
      "protocol": "chat_completions",
      "model": "my-coding-model",
      "auth": {
        "type": "bearer",
        "token": "replace-with-your-token"
      },
      "access": null
    },
    "home-gpu": {
      "url": "http://127.0.0.1:18000/v1",
      "protocol": "anthropic",
      "model": "",
      "auth": {
        "type": "basic",
        "username": "developer",
        "password": "replace-with-your-password"
      },
      "access": {
        "command": "ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:18000:127.0.0.1:8000 home-gpu",
        "persistent": true,
        "cleanup": null,
        "timeout_secs": 30
      }
    }
  },
  "agents": {
    "pi": { "backend": "local", "model": null },
    "opencode": { "backend": "local", "model": null },
    "codex": { "backend": "local", "model": null },
    "claude": { "backend": "home-gpu", "model": "" }
  }
}
```

Protocol values are `chat_completions`, `responses`, and `anthropic`. Set the base URL to the API prefix expected by the server, usually ending in `/v1`, rather than a complete `/chat/completions`, `/responses`, or `/messages` endpoint.

Authentication can be `null`, a bearer token, or HTTP Basic. Only one authentication method is used per backend. Upstream credentials remain in the bridge; agents receive a separate random credential for the loopback connection.

Model precedence is CLI `--model`, agent binding, then backend default. Missing or `null` fields inherit the next default. An explicit empty string suppresses a model override and preserves the model requested by the agent. In the UI, leaving a model field blank stores `null`; leave both the binding and backend model blank to preserve the agent's selection.

## Backend access

Set `access` to `null` for direct access, or configure a command:

- `command` runs through `/bin/sh -c`, inherits the environment and current directory, and receives no interactive stdin. Authenticate SSH, Teleport, VPN clients, or other tools beforehand.
- `persistent: false` waits for the command to finish successfully before checking readiness.
- `persistent: true` keeps the command running during the session. Its unexpected exit stops the launched agent.
- `timeout_secs` bounds startup and readiness. The URL must be fixed; dynamically returned URLs are not supported.
- `cleanup` is an optional shell command used after a started access session, including startup failures and handled termination signals.

Readiness checks whether the configured host and port accept TCP connections. It does not validate authentication, model availability, or API behavior.

On exit, the cleanup command runs first, with a ten-second timeout. The launcher then terminates its owned persistent access process group, including descendants. Once a preparation command has exited and been reaped, the launcher no longer retains its process-group ID; use the cleanup command to stop any background service it started. Persistent commands should stay in the foreground rather than daemonizing. A cleanup error is reported without replacing the agent's exit status. Cleanup cannot run after an uncatchable process termination such as `SIGKILL`.

## Protocol compatibility

The bridge accepts OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages. Same-protocol requests are forwarded to preserve native features. Cross-protocol translation targets text conversations, streamed responses, function/tool calls, and tool results used in repeated agent turns.

For cross-protocol launches, the Codex adapter disables its default reasoning and hosted web-search features, and the Claude Code adapter disables thinking; the launcher announces these compatibility settings. Same-protocol launches retain native behavior.

Translation is not complete API emulation. Opaque reasoning state, multimodal content, built-in hosted tools, and other features without a supported mapping produce explicit errors instead of silently losing data. An agent or backend that requires those features may need a matching protocol or different agent settings.

Real clients have passed local mock API smoke checks for streamed text and tool roundtrips: Codex 0.155.0, Claude Code 2.1.208, Pi 0.85.1, and OpenCode 1.18.31. These checks exercise the launcher and actual agent processes, but no live or paid model backend has been tested. Compatibility with other agent releases or backend implementations still needs verification.

With no explicit model, OpenCode preserves saved selections from its native OpenAI or Anthropic providers. Configure a model override for other provider selections.

To reproduce the Pi/OpenCode checks with isolated package installation:

```sh
npm --prefix /tmp/launchcoder-smoke-tools install --no-save --ignore-scripts @earendil-works/pi-coding-agent@0.85.1 opencode-ai@1.18.31
cargo build --locked
python3 scripts/smoke_open_agents.py --launcher target/debug/launchcoder --agent-bin /tmp/launchcoder-smoke-tools/node_modules/.bin
```

For installed Codex and Claude Code clients, run `python3 scripts/smoke_agents.py --launcher target/debug/launchcoder`.

The smoke harness creates temporary per-agent configuration directories, uses fake credentials and a local streaming backend, and asks each agent to read a temporary fixture before completing. HTTP proxy settings reject external proxy requests; no upstream model credentials are inherited. Pi's current package name follows its [official installation documentation](https://github.com/earendil-works/pi/tree/main/packages/coding-agent).

## Development and platform builds

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
python3 scripts/smoke_tui.py
```

CI runs these checks on Linux and macOS, plus an MSRV check on Rust 1.86. Release-mode artifact jobs target:

| Platform | Rust target | Build method |
| --- | --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-musl` | `cross` on Linux |
| Linux ARM64 | `aarch64-unknown-linux-musl` | `cross` on Linux |
| macOS Intel | `x86_64-apple-darwin` | Native Intel runner |
| macOS Apple Silicon | `aarch64-apple-darwin` | Native ARM64 runner |

Linux builds use musl for static binaries; a normal native Linux `cargo build` may target glibc instead. macOS builds use the native system runtime. Rustls handles outbound TLS without requiring an OpenSSL installation.

The workflow stores compressed build artifacts and does not publish releases. Its platform matrix follows the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners); Linux cross-compilation uses [actions-rust-cross](https://github.com/houseabsolute/actions-rust-cross).
