# codeport

Run your preferred coding agent against a local or remote model backend. Codeport configures and launches the agent, translates API requests, and can set up an SSH tunnel or proxy for the session.

![Codeport connects your coding agent to a local or remote model, with an optional managed SSH tunnel or proxy.](docs/overview.svg)

Configure your backends once, then choose how to work:

| What you want to do | Command | What Codeport handles |
| --- | --- | --- |
| Code with a local model | `codeport opencode --backend local` | Connects the agent to your saved backend and model. |
| Try another agent on the same model | `codeport codex --backend local` | Adapts supported API requests to the backend's protocol. |
| Use a model on a remote GPU | `codeport opencode --backend home-gpu` | Starts the configured SSH tunnel or proxy, waits for connectivity, and cleans up on exit. |
| Try a different model for one session | `codeport opencode --model unsloth/Qwen3.8-27B-GGUF` | Overrides the model for that run without changing your saved default. |

Supported coding agents: **Pi**, **OpenCode**, **Codex (experimental)**, and **Claude Code (experimental)**.

Codex and Claude Code integrations are still under development; some tools and workflows may fail.

`local` and `home-gpu` are example backend names you create in the configuration UI. Launch Codeport from the project directory you want to work in.

## Quick start

You need Linux or macOS, Rust 1.86 or newer, an installed coding agent, and a running model API server. Have your server's URL, model ID, and any required credentials ready. Ensure the agent and Cargo's bin directory (usually `~/.cargo/bin`) are on `PATH`.

Install from this repository, then open the configuration UI:

```sh
cargo install --locked --path .
codeport -cfg
```

Choose **Add backend**, give it a name such as `local`, and enter its connection details:

- **URL:** the API base URL, such as `http://127.0.0.1:8000/v1`.
- **Protocol:** OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages, matching your backend.
- **Model:** the exact model ID advertised by your backend, including any namespace.
- **Authentication:** a bearer token, HTTP Basic credentials, or none, as required by your backend.

Choose **Direct** for **Backend access** if the server is already reachable. For a remote server that needs a tunnel, follow [SSH tunnels and proxies](#ssh-tunnels-and-proxies).

Choose **Bind agent**, select your agent and backend, then **Exit**. From your project directory, start the agent:

```sh
codeport opencode
```

Replace `opencode` with `pi`, `codex`, or `claude` to launch another agent. Forward agent options after `--`:

```sh
codeport claude -- --continue
```

## Configuration

Run `codeport -cfg` to edit backends, agent defaults, credentials, and web-search settings.

A model supplied with `--model` overrides the agent's saved model, which overrides the backend's default. Leave both model fields blank in the UI to keep the agent's own selection. For OpenCode, set a model explicitly unless you use its native OpenAI or Anthropic provider.

Using a local model through Codeport leaves your ordinary Codex model default unchanged. Your existing Codex sessions and skills remain accessible, but model and settings changes made inside a Codeport-launched Codex session are discarded on exit. To change your usual Codex defaults, launch Codex directly.

Configuration and credentials are stored in `~/.config/codeport/credential.json`. Credentials are plaintext, protected by restricted file permissions. To use a separate configuration:

```sh
codeport --config ./credential.json -cfg
codeport --config ./credential.json opencode
```

## SSH tunnels and proxies

Codeport can start a connection command before launching your agent and stop it when the session ends. For a backend already reachable from your machine, choose **Direct** under **Backend access**.

For example, to reach a model server listening on port `8000` on an SSH host named `home-gpu`, edit the backend in `codeport -cfg`:

1. Set the backend URL to `http://127.0.0.1:18000/v1`.
2. Choose **Command** under **Backend access** and enter:

   ```sh
   ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:18000:127.0.0.1:8000 home-gpu
   ```

3. Choose **Stay running during the agent session** for the command lifecycle.
4. Save the backend, then launch with `codeport opencode --backend home-gpu`.

Replace `home-gpu` and the ports with your own connection details. SSH must be installed and able to authenticate without a password prompt. Codeport waits for the local port to become reachable before launching the agent. If the tunnel exits unexpectedly, the agent session stops too.

You can configure a proxy or another access utility the same way. Install and authenticate it beforehand; access commands cannot prompt for input. Commands that only prepare a connection can use **Finish before launching the agent**, with an optional cleanup command to stop any background service they start.

## Web search for Claude Code

Claude's built-in `WebSearch` works with public DuckDuckGo search by default when using a Chat Completions or Responses backend. No search API key is needed. Native Anthropic backends keep their own search service.

To change providers, open `codeport -cfg` → **Configure web search**:

- **Public:** DuckDuckGo, with no setup or API key.
- **SearXNG:** enter your instance's base URL and enable JSON results in its `search.formats` setting.
- **Brave:** enter your Brave Search API key.

Search queries go to the selected provider directly, without using the model backend's SSH tunnel or proxy. Public search may encounter rate limits or bot challenges; Codeport reports failures without switching providers automatically.

Codeport enables Claude's built-in `WebFetch` across domains for the session. Explicit deny rules and managed policies still apply. Websites may reject requests, require login, or depend on JavaScript, so some pages cannot be fetched.

## Compatibility and limitations

Codex and Claude Code support is experimental. Text conversations and tool calls are supported across API protocols, but some agent features require a matching backend protocol:

- **Codex:** reasoning and hosted web search are disabled when translating to another protocol.
- **Claude Code:** extended thinking and explicit reasoning effort are disabled when translating to another protocol.
- **Images and other advanced features:** may be unavailable during protocol translation.

Claude auto mode is available with Chat Completions and Responses backends:

```sh
codeport claude -- --permission-mode auto
```

Codeport uses your configured model to review proposed actions, so permission decisions depend on that model and may add latency. Native web searches and fetches are automatically allowed by Codeport's reviewer; other actions remain blocked if their review fails. This review differs from Anthropic's own classifier.

Codex defaults to a 32,768-token context window. If your backend supports a different size, set it explicitly:

```sh
codeport codex -- -c model_context_window=65536
```

## Troubleshooting

- **Agent or `codeport` not found:** check that the executable is installed and on `PATH`.
- **401 Unauthorized:** update the backend's credentials with `codeport -cfg`.
- **Model not found:** check the exact model ID, including its namespace, and confirm the server has loaded it.
- **Connection timeout:** check the backend URL, server availability, and any SSH or proxy command. A reachable port alone does not confirm valid credentials or a loaded model.
- **WebFetch blocked or rejected:** restart Codeport after updating it. Website errors such as HTTP 402 or 403 can still prevent fetching even when Codeport allows the request.
