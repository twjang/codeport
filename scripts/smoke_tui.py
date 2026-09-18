#!/usr/bin/env python3
"""Exercise terminal configuration against a disposable credential file."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import tempfile
import time


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/launchcoder").resolve()
    with tempfile.TemporaryDirectory(prefix="launchcoder-tui-") as directory:
        config = Path(directory) / "settings" / "credential.json"
        master, slave = pty.openpty()
        child = subprocess.Popen([str(binary), "--config", str(config), "-cfg"],
                                 stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        buffer = b""

        def expect(text):
            nonlocal buffer
            deadline = time.monotonic() + 10
            needle = text.encode()
            while needle not in buffer:
                if time.monotonic() > deadline:
                    raise AssertionError(f"Expected {text!r}; received {buffer!r}")
                if select.select([master], [], [], 0.2)[0]:
                    try:
                        block = os.read(master, 65536)
                    except OSError as exc:
                        if exc.errno == errno.EIO:
                            block = b""
                        else:
                            raise
                    if not block:
                        raise AssertionError(f"TUI exited while waiting for {text!r}: {buffer!r}")
                    buffer += block
            buffer = buffer.split(needle, 1)[1]

        def send(data=b"\r"):
            os.write(master, data)
            time.sleep(0.05)

        try:
            expect("Add backend")
            send()
            expect("Backend name")
            send(b"smoke\r")
            expect("Backend base URL")
            send()
            expect("Backend API protocol")
            send()
            expect("Default model")
            send()
            expect("Authentication")
            send()
            expect("Backend access")
            send()
            expect("Saved to")
            expect("Add backend")
            send(b"\x1b[B" * 3 + b"\r")
            expect("Agent")
            send()
            expect("Backend")
            send()
            expect("Model override")
            send()
            expect("Saved to")
            expect("Add backend")
            send(b"\x1b[B" * 5 + b"\r")
            deadline = time.monotonic() + 5
            while child.poll() is None:
                assert time.monotonic() < deadline, f"TUI did not exit: {buffer!r}"
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        buffer += os.read(master, 65536)
                    except OSError as exc:
                        if exc.errno != errno.EIO:
                            raise
            assert child.returncode == 0
            data = json.loads(config.read_text())
            assert data["agents"]["pi"]["backend"] == "smoke"
            assert data["backends"]["smoke"]["model"] is None
            assert config.stat().st_mode & 0o777 == 0o600
            assert config.parent.stat().st_mode & 0o777 == 0o700
            print("PASS TUI: add backend, blank model, bind Pi, save 0600 credentials, exit")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)


if __name__ == "__main__":
    main()
