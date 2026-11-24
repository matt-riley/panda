"""Exercise credentialed inference through a real isolated Codemode worker."""
import base64
import json
import random
import struct
import zlib
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

BIN = str(Path(sys.argv[1]).resolve())
def fixture_png():
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))
    pixels = random.Random(41)
    scanlines = b"".join(b"\0" + pixels.randbytes(288) for _ in range(96))
    header = struct.pack(">IIBBBBB", 96, 96, 8, 2, 0, 0, 0)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b"")
    return base64.b64encode(data).decode()


PNG = fixture_png()
SCRIPT = """// @options: {"timeout_ms":60000}
const rows = tools.list_models({});
if (!rows.some(r => r.id === "fixture/vision" && r.available)) throw Error("catalog");
if (tools.list_models({model:"dormant/vision"})[0].available) throw Error("expected missing credential");
if (tools.list_models({model:"fixture/missing"}).length) throw Error("missing");
if (typeof process !== "undefined" || rows.some(r => Object.keys(r).some(k => /key|token|secret/i.test(k)))) throw Error("credential isolation");
const labels = ["yes", "no"];
let missing = false;
try { tools.classify({model:"fixture/missing", labels, input:{}}); } catch (e) { missing = true; }
if (!missing) throw Error("unavailable model accepted");
text(tools.classify({model:"fixture/vision", labels, input:{count:2}}));
const block = tools.generate_image({model:"fixture/art", prompt:"Draw a dot"});
text(tools.classify({model:"fixture/vision", labels, image:block}));
image(block);
"""
calls = []
blocked = threading.Event()
release = threading.Event()


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        assert self.headers["Authorization"] == "Bearer fixture-secret"
        data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        model = data["model"]
        self.send_response(200)
        if model == "chat":
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            history = data["messages"]
            if any(m["role"] == "tool" for m in history):
                delta = {"content": "finished"}
                reason = "stop"
            else:
                code = SCRIPT
                if "cancel generation" in json.dumps(history):
                    code = 'tools.generate_image({model:"fixture/art",prompt:"wait"})'
                delta = {"tool_calls": [{"index": 0, "id": "script1", "function": {
                    "name": "codemode", "arguments": json.dumps({"code": code})}}]}
                reason = "tool_calls"
            event = {"choices": [{"delta": delta, "finish_reason": reason}]}
            self.wfile.write(("data: " + json.dumps(event) + "\n\ndata: [DONE]\n\n").encode())
            return
        calls.append(model)
        if model == "art":
            assert self.path == "/v1/images/generations" and data["n"] == 1
            if data["prompt"] == "wait":
                blocked.set()
                release.wait(8)
                return
            result = {"data": [{"b64_json": PNG}], "usage": {"input_tokens": 4, "output_tokens": 8, "cost": 0.03}}
        else:
            assert model == "vision" and self.path == "/v1/chat/completions" and data["max_tokens"] == 100
            assert data["response_format"]["json_schema"]["schema"]["properties"]["label"]["enum"] == ["yes","no"]
            content = data["messages"][-1]["content"]
            if isinstance(content, list):
                assert any(p.get("image_url", {}).get("url") == "data:image/png;base64," + PNG for p in content)
            else:
                assert json.loads(content) == {"count": 2}
            result = {"choices": [{"message": {"content": '{"label":"yes"}'}}],
                      "usage": {"prompt_tokens": 100, "completion_tokens": 5}}
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(result).encode())


def exercise(root, port):
    repo, state = root / "repo", root / "state"
    repo.mkdir()
    state.mkdir(mode=0o700)
    models = [{"id": name, "contextWindow": 10000, "maxTokens": 100, "input":["text","image"],
               "inference": tasks, "cost":{"input":1,"output":2}}
              for name, tasks in [("chat", []), ("art", ["generate_image"]), ("vision", ["classify"])]]
    endpoint = {"models": models, "baseUrl": f"http://127.0.0.1:{port}/v1", "apiKey": "FIXTURE_KEY"}
    (state / "models.json").write_text(json.dumps({"providers": {
        "fixture": endpoint, "dormant": dict(endpoint, apiKey="MISSING_FIXTURE_CREDENTIAL")}}))
    (state / "model.json").write_text(json.dumps(["fixture","chat"]))
    env = dict(os.environ, HOME=str(root), PANDA_HOME=str(state), FIXTURE_KEY="fixture-secret")
    run = subprocess.run([BIN,"--mode","json","--codemode-only","classify and draw"], cwd=repo, env=env,
                         capture_output=True, text=True, timeout=20)
    assert run.returncode == 0, run.stderr
    assert calls == ["vision", "art", "vision"], (calls, run.stdout)
    with sqlite3.connect(state / "sessions.sqlite3") as db:
        saved = json.loads(db.execute("SELECT data FROM sessions").fetchone()[0])
    attempts = [a for a in saved["attempts"] if a["purpose"] != "turn"]
    assert [(a["purpose"], a["cost"]) for a in attempts] == [("classify",0.00011),("generate_image",0.03),("classify",0.00011)], attempts
    assert len(list(repo.rglob("*.png"))) == 1
    assert "fixture-secret" not in run.stdout and "fixture-secret" not in json.dumps(saved)
    proc = subprocess.Popen([BIN, "--mode","rpc"], cwd=repo, env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        proc.stdin.write('{"type":"prompt","id":1,"text":"cancel generation"}\n')
        proc.stdin.flush()
        assert blocked.wait(5)
        out, err = proc.communicate('{"type":"cancel","id":2}\n',timeout=3)
        assert "cancelled" in out and proc.returncode == 0, (out, err)
        assert calls.count("art") == 2
        with sqlite3.connect(state / "sessions.sqlite3") as db:
            latest = json.loads(db.execute("SELECT data FROM sessions ORDER BY id DESC LIMIT 1").fetchone()[0])
        cancelled = [a for a in latest["attempts"] if a["purpose"] == "generate_image"]
        assert len(cancelled) == 1 and cancelled[0]["outcome"] == "failed", cancelled
    finally:
        release.set()
        if proc.poll() is None:
            proc.kill()
            proc.wait()


with ThreadingHTTPServer(("127.0.0.1",0), Endpoint) as server, tempfile.TemporaryDirectory() as tmp:
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        exercise(Path(tmp),server.server_port)
    finally:
        server.shutdown()
