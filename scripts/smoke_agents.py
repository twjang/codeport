#!/usr/bin/env python3
"""Opt-in installed-agent smoke checks using loopback mock APIs, no real tokens.

Usage: python3 scripts/smoke_agents.py [codex|claude|all]
       python3 scripts/smoke_agents.py --launcher target/debug/codeport
Records sanitized request shapes (not user prompts/config) to stdout.
No model backend or paid API is contacted. HTTP proxy settings also point to the
mock, which refuses external proxy requests. Pi/OpenCode require separate tests.
"""
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading

TOKEN = "codeport-local-smoke-token"
ANSWER = "CODEPORT_SMOKE_OK"


def event(name, data):
    return f"event: {name}\ndata: {json.dumps(data)}\n\n"


def responses():
    msg = {"id": "msg_smoke", "type": "message", "role": "assistant", "status": "completed",
           "content": [{"type": "output_text", "text": ANSWER, "annotations": []}]}
    response = {"id": "resp_smoke", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-5.4", "output": [msg],
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}
    return (event("response.created", {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}})
            + event("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": {**msg, "status": "in_progress", "content": []}})
            + event("response.content_part.added", {"type": "response.content_part.added", "output_index": 0, "content_index": 0, "item_id": msg["id"], "part": {"type": "output_text", "text": "", "annotations": []}})
            + event("response.output_text.delta", {"type": "response.output_text.delta", "item_id": msg["id"], "output_index": 0, "content_index": 0, "delta": ANSWER})
            + event("response.output_text.done", {"type": "response.output_text.done", "item_id": msg["id"], "output_index": 0, "content_index": 0, "text": ANSWER})
            + event("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": msg})
            + event("response.completed", {"type": "response.completed", "response": response}))


def anthropic():
    msg = {"id": "msg_smoke", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
           "content": [], "stop_reason": None, "stop_sequence": None,
           "usage": {"input_tokens": 10, "output_tokens": 0}}
    return (event("message_start", {"type": "message_start", "message": msg})
            + event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
            + event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ANSWER}})
            + event("content_block_stop", {"type": "content_block_stop", "index": 0})
            + event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 5}})
            + event("message_stop", {"type": "message_stop"}))


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_CONNECT(self):
        self.send_error(403, "External access disabled for local smoke test")

    def do_GET(self):
        self.send_error(404)

    def do_POST(self):
        if self.path.startswith(("http://", "https://")):
            self.send_error(403, "External access disabled")
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        if self.path.endswith("count_tokens"):
            data, content_type = json.dumps({"input_tokens": 10}), "application/json"
        elif any(path in self.path for path in ("/responses", "/messages", "/chat/completions")):
            auth = self.headers.get("Authorization") == f"Bearer {TOKEN}" or self.headers.get("x-api-key") == TOKEN
            if not auth:
                self.send_error(401, "Expected local smoke token")
                return
            self.server.captured.append({
                "path": self.path, "keys": sorted(body), "model": body.get("model"),
                "stream": body.get("stream"), "reasoning": body.get("reasoning"),
                "thinking": body.get("thinking"), "output_config": body.get("output_config"),
                "tool_choice": body.get("tool_choice"), "context_management": body.get("context_management"),
                "tools": [{"type": t.get("type"), "name": t.get("name") or t.get("function", {}).get("name"),
                           "keys": sorted(t)} for t in body.get("tools", [])],
            })
            if "/chat/completions" in self.path:
                messages = body.get("messages", [])
                results = [m for m in messages if m.get("role") == "tool"]
                self.server.tool_returned = len(results) >= 2 and all("CODEPORT_TOOL_OK" in json.dumps(result) for result in results)
                if len(results) >= 2:
                    delta, reason = {"content": ANSWER}, "stop"
                else:
                    if self.server.agent == "codex":
                        names = [t.get("function", {}).get("name", "") for t in body.get("tools", [])]
                        name = next((n for n in names if n == "exec_command" or (n.startswith("cpns_") and n.endswith("_exec_command"))), None)
                        if name is None:
                            self.send_error(400, "Expected a declared exec_command tool")
                            return
                        arguments = {"cmd": "printf CODEPORT_TOOL_OK", "max_output_tokens": 50}
                    else:
                        name, arguments = "Read", {"file_path": self.server.fixture if not results else self.server.fixture + ".second"}
                    delta, reason = {"tool_calls": [{"index": 0, "id": f"call_smoke_{len(results)}", "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}, "tool_calls"
                def chunk(delta, finish):
                    return "data: " + json.dumps({"id": "chatcmpl_smoke", "object": "chat.completion.chunk", "created": 1, "model": body["model"], "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}) + "\n\n"
                data = chunk({"role": "assistant"}, None) + chunk(delta, None) + chunk({}, reason) + "data: [DONE]\n\n"
            else:
                data = responses() if "/responses" in self.path else anthropic()
            content_type = "text/event-stream"
        else:
            self.send_error(404)
            return
        data = data.encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def run_agent(agent, launcher=None):
    binary = shutil.which(agent)
    if not binary:
        print(f"SKIP {agent}: executable not installed")
        return True
    with tempfile.TemporaryDirectory(prefix="codeport-smoke-") as directory:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        server.captured = []
        server.agent = agent
        server.fixture = str(Path(directory) / "fixture.txt")
        server.tool_returned = False
        Path(server.fixture).write_text("CODEPORT_TOOL_OK")
        Path(server.fixture + ".second").write_text("CODEPORT_TOOL_OK")
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin = f"http://127.0.0.1:{server.server_port}"
        # Only retain OS execution variables; never inherit API keys/tokens.
        env = {k: os.environ[k] for k in ("PATH", "HOME", "USER", "TMPDIR", "LANG") if k in os.environ}
        env.update({"HTTP_PROXY": origin, "HTTPS_PROXY": origin, "ALL_PROXY": origin,
                    "NO_PROXY": "127.0.0.1,localhost", "CODEPORT_API_KEY": TOKEN})
        if agent == "codex":
            settings = {"model_provider": "codeport", "model_providers.codeport.name": "codeport",
                        "model_providers.codeport.base_url": origin + "/v1",
                        "model_providers.codeport.env_key": "CODEPORT_API_KEY",
                        "model_providers.codeport.wire_api": "responses",
                        "model_providers.codeport.requires_openai_auth": False,
                        "model_providers.codeport.supports_websockets": False}
            command = [binary, "exec", "--ignore-user-config", "--ignore-rules", "--ephemeral",
                       "--skip-git-repo-check", "--model", "gpt-5.4", "--json"]
            for key, value in settings.items():
                command.extend(["-c", f"{key}={json.dumps(value)}"])
        else:
            env.update({"CLAUDE_CONFIG_DIR": directory + "/claude", "ANTHROPIC_BASE_URL": origin,
                        "ANTHROPIC_AUTH_TOKEN": TOKEN, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"})
            command = [binary, "-p", "--safe-mode", "--setting-sources", "", "--no-session-persistence",
                       "--model", "claude-sonnet-4-5", "--output-format", "json"]
        if launcher:
            config_path = Path(directory) / "credential.json"
            config_path.write_text(json.dumps({"backends": {"smoke": {"url": origin, "protocol": "chat_completions", "model": "codeport-smoke-model", "auth": {"type": "bearer", "token": TOKEN}}}, "agents": {agent: {"backend": "smoke"}}}))
            config_path.chmod(0o600)
            if agent == "codex":
                forwarded = ["exec", "--ignore-user-config", "--ignore-rules", "--ephemeral", "--skip-git-repo-check", "--json"]
            else:
                forwarded = command[1:] + ["--allowedTools", "Read"]
            command = [str(Path(launcher).resolve()), "--config", str(config_path), agent, "--"] + forwarded
            command.extend(["--", "Use the requested local tool, then reply CODEPORT_SMOKE_OK."])
        else:
            command.append("Reply with CODEPORT_SMOKE_OK. Do not use tools.")
        try:
            result = subprocess.run(command, env=env, cwd=directory, text=True, capture_output=True, timeout=45)
            ok = result.returncode == 0 and ANSWER in result.stdout and bool(server.captured) and (not launcher or server.tool_returned)
            if launcher and agent == "codex":
                ok = ok and "failed to decode models response" not in result.stderr and "Defaulting to fallback metadata" not in result.stdout
            print(json.dumps({"agent": agent, "pass": ok, "returncode": result.returncode,
                              "tool_roundtrip": server.tool_returned, "requests": server.captured}, indent=2))
            if not ok:
                print("Agent diagnostics:", result.stderr[-3000:], result.stdout[-1000:])
            return ok
        except subprocess.TimeoutExpired:
            print(f"FAIL {agent}: timed out; captured {json.dumps(server.captured)}")
            return False
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("agent", choices=["all", "codex", "claude"], nargs="?", default="all")
    parser.add_argument("--launcher", help="Test built codeport through Chat Completions, including a local tool roundtrip")
    options = parser.parse_args()
    choice = options.agent
    if choice not in ("all", "codex", "claude"):
        raise SystemExit("Choose all, codex, or claude")
    outcomes = [run_agent(agent, options.launcher) for agent in (["codex", "claude"] if choice == "all" else [choice])]
    raise SystemExit(0 if all(outcomes) else 1)
