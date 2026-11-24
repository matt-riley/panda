"""Exercise the released CLI entry point with a PTY and a loopback streaming provider."""
import base64
import fcntl
import json
import os
import pty
import re
import select
import subprocess
import struct
import sys
import tempfile
import termios
import threading
import time
import zlib
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

binary = str(Path(sys.argv[1]).resolve())
release_stream = threading.Event()
release_cancel = threading.Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        last = request["messages"][-1]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def delta(value, reason=None):
            packet = {"choices": [{"index": 0, "delta": value, "finish_reason": reason}]}
            self.wfile.write(("data: " + json.dumps(packet) + "\n\n").encode())
            self.wfile.flush()

        if last.get("content") == "wait for cancellation":
            delta({"content": "CANCELLABLE"})
            release_cancel.wait(10)
            return
        if any(m["role"] == "tool" for m in request["messages"]):
            delta({"content": "FIRST-"})
            release_stream.wait(10)
            delta({"content": "SECOND"}, "stop")
        else:
            delta({"tool_calls": [{"index": 0, "id": "picture", "type": "function",
                   "function": {"name": "read", "arguments": '{"path":"pixel.png"}'}}]}, "tool_calls")
        self.wfile.write(b"data: [DONE]\n\n")


@contextmanager
def terminal(root, env, *args):
    fd, slave = pty.openpty()
    def resize(rows, cols):
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    resize(24, 80)
    # Acquire the slave as the controlling terminal in a fresh process so Ctrl-C
    # exercises the kernel's signal path, including on macOS.
    launch = """
import fcntl, signal, subprocess, sys, termios
fcntl.ioctl(0, termios.TIOCSCTTY, 0)
# The supervising shell survives Ctrl-C while Panda handles it.
signal.signal(signal.SIGINT, lambda *_: None)
original = termios.tcgetattr(0)
code = subprocess.call(sys.argv[1:])
assert termios.tcgetattr(0) == original, "terminal attributes changed"
print("terminal restored", flush=True)
sys.exit(code)
"""
    child = subprocess.Popen([sys.executable, "-c", launch, binary, "--scrollback", *args],
                             stdin=slave, stdout=slave, stderr=slave, cwd=root,
                             env=env, start_new_session=True)
    output = bytearray()
    cursor = 0
    def expect(marker):
        nonlocal cursor
        end = time.monotonic() + 10
        while marker not in output[cursor:]:
            assert time.monotonic() < end, (marker, bytes(output[-1000:]))
            if select.select([fd], [], [], 0.1)[0]:
                chunk = os.read(fd, 65536)
                assert chunk, (marker, bytes(output[-1000:]))
                output.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[12;1R")
        cursor = output.index(marker, cursor) + len(marker)

    try:
        yield expect, lambda data: os.write(fd, data), resize, output
        os.write(fd, b":q\r")
        expect(b"terminal restored")
        assert child.wait(timeout=5) == 0
        primary = re.sub(rb"\x1b\[\?1049h.*?\x1b\[\?1049l", b"", output, flags=re.S)
        for sequence in (b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[2J", b"\x1b[3J"):
            assert sequence not in primary, sequence
    finally:
        os.close(fd)
        os.close(slave)
        if child.poll() is None:
            child.kill()
        child.wait(timeout=5)



with tempfile.TemporaryDirectory() as tmp, ThreadingHTTPServer(("127.0.0.1", 0), Provider) as server:
    threading.Thread(target=server.serve_forever, daemon=True).start()
    root = Path(tmp)
    repo = root / "workspace"
    repo.mkdir()
    state = root / "state"
    state.mkdir(mode=0o700)
    pixel = (Path(__file__).parent / "images/pixel.png").read_bytes()
    # A valid ancillary PNG chunk forces multiple Kitty packets.
    comment = b"tEXt" + b"Comment\0" + b"p" * 4096
    chunk = struct.pack(">I", len(comment) - 4) + comment + struct.pack(">I", zlib.crc32(comment))
    pixel = pixel[:-12] + chunk + pixel[-12:]
    (repo / "pixel.png").write_bytes(pixel)
    payload = base64.b64encode(pixel)
    env = {**os.environ, "HOME": tmp, "TERM": "xterm-kitty", "PANDA_HOME": str(state),
           "TERM_PROGRAM": "", "LOCAL_KEY": "fixture", "NO_COLOR": "1"}
    for key in ("TMUX", "STY", "TYPESAFE_API_KEY", "LORE_TYPESAFE_API_KEY"):
        env.pop(key, None)
    (state / "models.json").write_text(json.dumps({"providers": {"local": {
        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "apiKey": "LOCAL_KEY",
        "models": [{"id": "vision", "contextWindow": 32768, "maxTokens": 1024,
                    "input": ["text", "image"]}]}}}))
    (state / "model.json").write_text('["local","vision"]')
    prompt = "❯ ".encode()

    with terminal(repo, env, "inspect @pixel.png") as (expect, send, resize, output):
        for _ in range(2):  # user attachment and the tool read
            expect(b"a=T,f=100,q=2,c=1,r=1,m=1;" + payload[:4096] + b"\x1b\\")
            expect(b"\x1b_Gm=0;" + payload[4096:] + b"\x1b\\")
        expect(b"FIRST-")
        resize(16, 52)
        release_stream.set()
        expect(b"SECOND")
        expect(prompt)
        assert output.count(b"FIRST-") == output.count(b"SECOND") == 1
        assert output.count(b"\x1b_Ga=T") == 2 and b"\x1b[?1049h" not in output
        send(b"wait for cancellation\r")
        expect(b"CANCELLABLE")
        send(b"\x03")
        expect(b"cancelled")
        expect(prompt)
    release_cancel.set()

    for program in ("iTerm.app", "unknown"):
        with terminal(repo, {**env, "TERM": "xterm-256color", "TERM_PROGRAM": program},
                      "--continue") as (expect, send, _, output):
            expect(b"FIRST-SECOND")
            expect(prompt)
            if program == "iTerm.app":
                assert output.count(b"\x1b]1337;File=inline=1") == output.count(payload) == 2
                send(b":tool picture\r")
                expect(b"Read image/png")
                expect(prompt)
                send(b":thinking\r")
                expect(b"\x1b[?1049h")
                expect(b"Reasoning effort")
                send(b"\x1b")
                expect(b"\x1b[?1049l")
                expect(prompt)
                assert output.count(payload) == 2
            else:
                assert output.count(b"preview unavailable") == 2
                assert b"\x1b_G" not in output and b"\x1b]1337" not in output and payload not in output
