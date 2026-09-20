#!/usr/bin/env python3
"""Exercise installed Pi/OpenCode through codeport and a local mock backend.

Usage: python3 scripts/smoke_open_agents.py --launcher target/debug/codeport \
  --agent-bin /private/tmp/codeport-smoke-tools/node_modules/.bin [pi|opencode|all]
No real credentials are inherited; agent state is isolated with application-specific and XDG directories.
The mock requests only a read of a temporary fixture, then checks its tool result.
"""
import argparse
import http.server
import json
import os
import signal
from pathlib import Path
import subprocess
import tempfile
import threading

TOKEN = "codeport-smoke-upstream-token"
ANSWER = "CODEPORT_OPEN_AGENT_OK"
FIXTURE = "codeport-tool-fixture-content"


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_CONNECT(self):
        self.send_error(403, "External access disabled")

    def do_GET(self):
        self.send_error(404)

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            self.send_error(401)
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        tools = [t.get("function", {}) for t in body.get("tools", [])]
        read_tool = next((t for t in tools if t.get("name", "").lower() == "read"), None)
        results = [m for m in body.get("messages", []) if m.get("role") == "tool"]
        successful_result = any(FIXTURE in json.dumps(m.get("content")) for m in results)
        self.server.captured.append({"model": body.get("model"), "stream": body.get("stream"),
                                     "tool_count": len(tools), "read_tool": bool(read_tool),
                                     "tool_result": successful_result})
        tool_call = None
        if read_tool and not successful_result and self.server.test_tools:
            properties = read_tool.get("parameters", {}).get("properties", {})
            key = "filePath" if "filePath" in properties else "path"
            tool_call = {"id": "call_local_read", "type": "function", "function": {
                "name": read_tool["name"], "arguments": json.dumps({key: str(self.server.fixture)})}}
        message = {"role": "assistant", "content": None if tool_call else ANSWER}
        if tool_call:
            message["tool_calls"] = [tool_call]
        reason = "tool_calls" if tool_call else "stop"
        common = {"id": "chatcmpl_local_smoke", "created": 1, "model": body.get("model", "smoke-model")}
        if body.get("stream"):
            delta = {"role": "assistant"}
            if tool_call:
                delta["tool_calls"] = [{"index": 0, **tool_call}]
            else:
                delta["content"] = ANSWER
            chunks = [{**common, "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                      {**common, "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                       "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}]
            data = "".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n"
            content_type = "text/event-stream"
        else:
            data = json.dumps({**common, "object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": reason}],
                               "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})
            content_type = "application/json"
        encoded = data.encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def run(agent, launcher, agent_bin, test_tools):
    with tempfile.TemporaryDirectory(prefix="codeport-open-smoke-", dir="/private/tmp" if Path("/private/tmp").is_dir() else "/tmp") as directory:
        root = Path(directory).resolve()
        fixture = root / "fixture.txt"
        fixture.write_text(FIXTURE)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        server.captured, server.fixture, server.test_tools = [], fixture, test_tools
        threading.Thread(target=server.serve_forever, daemon=True).start()
        origin = f"http://127.0.0.1:{server.server_port}"
        config = root / "credential.json"
        config.write_text(json.dumps({"backends": {"smoke": {"url": origin + "/v1", "protocol": "chat_completions",
                                         "model": "smoke-model", "auth": {"type": "bearer", "token": TOKEN}}},
                                      "agents": {agent: {"backend": "smoke"}}}))
        config.chmod(0o600)
        env = {key: os.environ[key] for key in ("PATH", "TMPDIR", "LANG") if key in os.environ}
        if agent_bin:
            env["PATH"] = str(agent_bin) + os.pathsep + env.get("PATH", "")
        env.update({"PI_CODING_AGENT_DIR": str(root / "pi"), "OPENCODE_CONFIG_DIR": str(root / "opencode"),
                    "XDG_CONFIG_HOME": str(root / "config"), "XDG_CACHE_HOME": str(root / "cache"),
                    "XDG_DATA_HOME": str(root / "data"), "XDG_STATE_HOME": str(root / "state"),
                    "HTTP_PROXY": origin, "HTTPS_PROXY": origin, "ALL_PROXY": origin,
                    "http_proxy": origin, "https_proxy": origin, "NO_PROXY": "127.0.0.1,localhost",
                    "no_proxy": "127.0.0.1,localhost", "OPENCODE_DISABLE_MODELS_FETCH": "true",
                    "OPENCODE_DISABLE_AUTOUPDATE": "true", "PI_SKIP_VERSION_CHECK": "1"})
        prompt = "Read fixture.txt, then reply CODEPORT_OPEN_AGENT_OK." if test_tools else "Reply CODEPORT_OPEN_AGENT_OK."
        args = ["--print", "--no-session", prompt] if agent == "pi" else ["run", "--format", "json", prompt]
        command = [str(launcher), "--config", str(config), agent, "--", *args]
        try:
            process = subprocess.Popen(command, cwd=directory, env=env, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True, start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate()
                raise
            result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
            tool_ok = not test_tools or any(r["tool_result"] for r in server.captured)
            ok = result.returncode == 0 and ANSWER in result.stdout and bool(server.captured) and tool_ok
            print(json.dumps({"agent": agent, "mode": "tool" if test_tools else "text", "pass": ok,
                              "returncode": result.returncode, "requests": server.captured}, indent=2))
            if not ok:
                print("Diagnostics:", result.stderr[-4000:], result.stdout[-2000:])
            return ok
        except subprocess.TimeoutExpired as error:
            print(f"FAIL {agent}: timeout, requests={server.captured}")
            print("Diagnostics:", (error.stderr or b"")[-3000:], (error.stdout or b"")[-1000:])
            return False
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("agent", choices=["pi", "opencode", "all"], nargs="?", default="all")
    parser.add_argument("--launcher", type=Path, default=Path("target/debug/codeport"))
    parser.add_argument("--agent-bin", type=Path)
    options = parser.parse_args()
    agents = ["pi", "opencode"] if options.agent == "all" else [options.agent]
    outcomes = [run(agent, options.launcher.resolve(), options.agent_bin, tools) for agent in agents for tools in [False, True]]
    raise SystemExit(0 if all(outcomes) else 1)
