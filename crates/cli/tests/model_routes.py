"""Exercise model routing, scoped cycling, credentials, attribution and metering over real HTTP."""
import base64
import fcntl
import json
import os
import pty
import select
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

binary = str(Path(sys.argv[1]).resolve())
requests = []
fail_once = False


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        global fail_once
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append((self.path, self.headers.get("Authorization"), body))
        if self.path == "/jev/systemone":
            assert self.headers["Authorization"] == "Bearer fixture-judge-key"
            answers = {}
            for key, question in body["questions"].items():
                if question["type"] == "noul":
                    answers[key] = {"type": "noul", "noul": 0.9 if key.startswith("s") else 0.1}
                else:
                    answers[key] = {"type": "choice", "choice": "drop", "confidence": 1.0}
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"answers": answers, "usage": {"input_tokens": 11, "output_tokens": 2}}).encode())
            return
        if body["messages"][-1].get("content") == "partial timeout":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b'data: {"choices":[{"delta":{"content":"partial"}}]}\n\n')
            self.wfile.flush()
            time.sleep(1)
            return
        if fail_once:
            fail_once = False
            self.send_response(503)
            self.send_header("Retry-After", "0")
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"temporarily unavailable"},"usage":{"cost":0.001}}')
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        usage = {"prompt_tokens": 100, "completion_tokens": 10,
                 "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 10}}
        if "Summarize" in body["messages"][0]["content"]:
            usage["cost"] = 0.125
        packet = {"choices": [{"delta": {"content": "fixture answer"}, "finish_reason": "stop"}],
                  "usage": usage}
        if body["model"] == "remote":
            packet["model"] = "router-backend-v2"
        self.wfile.write(("data: " + json.dumps(packet) + "\n\ndata: [DONE]\n\n").encode())


def exercise(port, root):
    repo, state = root / "repo", root / "state"
    repo.mkdir()
    state.mkdir(mode=0o700)
    env = {k: v for k, v in os.environ.items()
           if k not in {"OPENROUTER_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY", "TYPESAFE_API_KEY", "LORE_TYPESAFE_API_KEY"}}
    env.update(PANDA_HOME=str(state), HOME=str(root), TERM="xterm-256color",
               MODELS_A_KEY="key-a", MODELS_B_KEY="key-b")
    repo.joinpath("test.png").write_bytes(base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jS1kAAAAASUVORK5CYII="))
    price = {"input": 2, "output": 10, "cacheRead": 1, "cacheWrite": 5}
    text = {"id": "gpt-text", "name": "Fixture Text", "contextWindow": 32768,
            "maxTokens": 1000, "input": ["text"], "cost": price}
    image = dict(text, id="vision", maxTokens=2000, input=["text", "image"])
    config = {"providers": {
        "local-a": {"baseUrl": f"http://127.0.0.1:{port}/a/v1", "apiKey": "MODELS_A_KEY",
                    "models": [text, image]},
        "local-b": {"baseUrl": f"http://127.0.0.1:{port}/b/v1", "apiKey": "MODELS_B_KEY",
                    "models": [dict(text, id="remote", cost=None)]}}}
    catalog = state / "models.json"
    catalog.write_text(json.dumps(config))
    (state / "model.json").write_text(json.dumps(["local-a", "gpt-text"]))

    def run(*args, stdin="", error=None):
        result = subprocess.run([binary, *map(str, args)], cwd=repo, env=env, input=stdin,
                                capture_output=True, text=True, timeout=30)
        if error:
            assert result.returncode and error in result.stderr, (result.stdout, result.stderr)
        else:
            assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout + result.stderr

    def snapshot(sid):
        with sqlite3.connect(state / "sessions.sqlite3") as database:
            return json.loads(database.execute("SELECT data FROM sessions WHERE id=?", (sid,)).fetchone()[0])

    def check_route(provider, model, image_input=None):
        path, key, body = requests[-1]
        assert (path, key) == (f"/{provider}/v1/chat/completions", f"Bearer key-{provider}")
        assert body["model"] == model and body["max_tokens"] == (2000 if model == "vision" else 1000)
        if image_input is not None:
            content = body["messages"][-1]["content"]
            if image_input:
                assert any(p.get("image_url", {}).get("url", "").startswith("data:image/png;") for p in content)
            else:
                assert "text-only model" in content and "data:image" not in content
        return body

    assert ":keys" in run("--help") and ":theme" in run("--help")
    listed = run("--list-models", "local-a/*")
    assert "Fixture Text" in listed and "32768" in listed and "text+image" in listed
    run(stdin=":model local-a gpt-text\n:effort low\nfirst @test.png\n:q\n")
    assert check_route("a", "gpt-text", False)["reasoning"]["effort"] == "low"
    run("--continue", stdin=":fork 1 B\n:model local-a vision\n:effort high\nsecond @test.png\n:q\n")
    assert check_route("a", "vision", True)["reasoning"]["effort"] == "high"
    (state / "model.json").write_text(json.dumps(["local-b", "remote"]))
    for sid, model, effort in [(1, "gpt-text", "low"), (2, "vision", "high")]:
        run("--print", "--session", sid, "reopen @test.png")
        assert check_route("a", model, model == "vision")["reasoning"]["effort"] == effort
    run("--print", "--session", 2, "--provider", "local-b", "--model", "remote", "transient override")
    check_route("b", "remote")
    assert snapshot(2)["model"]["model"] == "vision"
    archive = root / "roundtrip.jsonl"
    run("--export-session", 2, archive)
    records = [json.loads(row) for row in archive.read_text().splitlines()]
    responders = [(e["message"]["provider"], e["message"]["model"]) for e in records
                  if e.get("message", {}).get("role") == "assistant"]
    assert responders == [("local-a", "gpt-text"), ("local-a", "vision"),
                          ("local-a", "vision"), ("local-b", "router-backend-v2")], responders
    imported = json.loads(run("--import-session", archive))["session_id"]
    assert [item["model"] for item in snapshot(imported)["responders"].values()] == [m for _, m in responders]
    assert snapshot(imported)["model"]["model"] == "vision"
    run("--print", "--session", imported, "next")
    assert check_route("a", "vision")["reasoning"]["effort"] == "high"

    global fail_once
    fail_once = True
    compacted = run(stdin=":help\n:model local-a gpt-text\nretry fixture\n:compact\n:context\n:usage\n:q\n")
    assert ":keys" in compacted and ":theme" in compacted
    pointers = [line.split("Private transcript · ", 1)[1] for line in compacted.splitlines()
                if "Private transcript · " in line]
    assert len(pointers) == 2 and pointers[0] == pointers[1], pointers
    sid = imported + 1
    before = snapshot(sid)["attempts"]
    assert [(a["purpose"], a["outcome"], a["cost_source"]) for a in before] == [
        ("turn", "failed", "reported"), ("turn", "ok", "catalog"), ("compaction", "ok", "reported")], before
    assert abs(before[1]["usage"]["cost"] - 0.0003) < 1e-10
    assert before[1]["usage"]["cache_read"] == 30 and before[1]["usage"]["cache_write"] == 10
    assert before[0]["cost"] == 0.001 and before[0]["usage"] is None
    assert before[2]["usage"]["cost"] == 0.125
    report = run("--continue", stdin=":usage\n:q\n")
    assert "known $0.126300" in report and "0.001000 (reported)" in report and "compaction" in report, report
    assert snapshot(sid)["attempts"] == before
    assert snapshot(2)["attempts"][-1]["cost_source"] == "unknown"

    warmed = run("--continue", stdin=":warm\n:usage\n:q\n")
    after = snapshot(sid)
    assert len(after["attempts"]) == len(before) + 1, warmed
    assert after["attempts"][-1]["purpose"] == "warm" and abs(after["attempts"][-1]["cost"] - 0.0003) < 1e-10
    assert "warm" in warmed and after["attempts"][:-1] == before

    (state / "settings.json").write_text(json.dumps({"request": {"maxRetries": 0, "timeoutMs": 100}}))
    fail_once = True
    previous = len(requests)
    run("--print", "--provider", "local-a", "--model", "gpt-text", "no retry", error="503")
    assert len(requests) == previous + 1
    (state / "settings.json").write_text(json.dumps({"request": {"maxRetries": 2, "timeoutMs": 150}}))
    previous = len(requests)
    run("--print", "--no-session", "--provider", "local-a", "--model", "gpt-text", "partial timeout", error="timed out")
    assert len(requests) == previous + 1
    (state / "settings.json").unlink()

    # Start inside the requested scope and cycle using the real key binding.
    (state / "model.json").write_text(json.dumps(["local-b", "remote"]))
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.execve(binary, [binary, "--models", "local-a/gpt-text,local-a/vision:high"], env)
    pending = bytearray()
    def expect(marker):
        deadline = time.monotonic() + 10
        while marker not in pending:
            assert time.monotonic() < deadline, (marker, bytes(pending[-3000:]))
            if select.select([fd], [], [], 0.1)[0]:
                chunk = os.read(fd, 65536)
                pending.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
        del pending[:pending.index(marker) + len(marker)]
    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        expect(b"local-a/gpt-text")
        os.write(fd, b"\x10")
        expect(b"local-a/vision")
        os.write(fd, b"cycle wire\r")
        expect(b"fixture answer")
        assert check_route("a", "vision")["reasoning"]["effort"] == "high"
        os.write(fd, b":logout local-a\r")
        expect(b"not configured")
        os.write(fd, b":q\r")
        expect(b"\x1b[?1049l")
    finally:
        os.close(fd)
        os.waitpid(pid, 0)
    count = len(requests)
    run("--print", "--no-session", "--provider", "local-a", "--model", "vision", "blocked", error="logged out")
    assert len(requests) == count
    run("--print", "--no-session", "--provider", "local-b", "--model", "remote", "still works")
    check_route("b", "remote")
    run(stdin=":login local-a\n:q\n")
    run("--print", "--no-session", "--provider", "local-a", "--model", "vision", "restored")
    check_route("a", "vision")
    # Default compaction keeps the wrapper's resolved model identity and price table. A real
    # native checkpoint records a recoverable transcript and the active model override.
    env.update(TYPESAFE_API_KEY="fixture-judge-key",
               TYPESAFE_BASE_URL=f"http://127.0.0.1:{port}/jev")
    (state / "settings.json").write_text(json.dumps({"compaction": {
        "keepRecentTokens": 24000, "modelOverrides": {"local-a/gpt-text": {"keepRecentTokens": 0}}}}))
    run("--print", "--session", 1, "native boundary " + "猫" * 12000)
    run("--print", "--session", 1, "keep this exchange")
    native = run("--continue", stdin=":resume 1\n:compact\n:context\n:q\n")
    saved = snapshot(1)
    assert "Compacted through" in native and "Reported usage:" in native, native
    assert saved["keepRecentTokens"] == 0 and saved["checkpoint"]["recovery"]
    assert saved["attempts"][-1]["model"]["provider"] == "local-a"
    assert saved["attempts"][-1]["cost_source"] == "catalog"
    assert any(path == "/jev/systemone" for path, _, _ in requests)
    del env["TYPESAFE_API_KEY"]
    env["LORE_TYPESAFE_API_KEY"] = "fixture-lore-key"
    count = len(requests)
    run("--print", "--session", 1, "local compaction " + "猫" * 12000)
    run("--print", "--session", 1, "retain the latest exchange")
    run("--continue", stdin=":resume 1\n:compact\n:q\n")
    assert snapshot(1)["checkpoint"]["through"] > saved["checkpoint"]["through"]
    assert all(path != "/jev/systemone" for path, _, _ in requests[count:])

    text["maxTokens"] = -1
    count = len(requests)
    catalog.write_text(json.dumps(config))
    run("--print", "--no-session", "invalid", error="invalid models.json")
    assert len(requests) == count


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), Endpoint) as stub, tempfile.TemporaryDirectory() as tmp:
        thread = threading.Thread(target=stub.serve_forever, daemon=True)
        thread.start()
        try:
            exercise(stub.server_port, Path(tmp))
        finally:
            stub.shutdown()
