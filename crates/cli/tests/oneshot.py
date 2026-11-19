"""Capture the CLI's serialized provider requests over loopback."""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

requests = []
next_calls = []


class Provider(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        requests.append((self.path, json.loads(body)))
        if next_calls:
            name, args = next_calls.pop(0)
            call = {"index": 0, "id": "fixture-call", "type": "function",
                    "function": {"name": name, "arguments": json.dumps(args)}}
            choice = {"delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}
        else:
            choice = {"delta": {"content": "reviewed"}, "finish_reason": "stop"}
        response = ("data: " + json.dumps({"choices": [choice]}) + "\n\ndata: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, *_):
        pass


with tempfile.TemporaryDirectory() as tmp, HTTPServer(("127.0.0.1", 0), Provider) as server:
    threading.Thread(target=server.serve_forever, daemon=True).start()
    root = Path(tmp)
    repo = root / "repo"
    state = root / "state"
    repo.mkdir()
    state.mkdir(mode=0o700)
    (state / "model.json").write_text('["deepseek","saved-model"]')

    def run(args, stdin):
        env = dict(os.environ, PANDA_HOME=str(state), HOME=str(root),
                   PANDA_ONESHOT_ARGS=json.dumps(args),
                   PANDA_ONESHOT_SERVER=f"http://127.0.0.1:{server.server_port}")
        result = subprocess.run([sys.argv[1], "--exact", "automation::tests::oneshot_fixture", "--nocapture"],
                                cwd=repo, env=env, input=stdin, text=True,
                                capture_output=True, timeout=20)
        assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout.splitlines()

    # A fresh state directory should never acquire a database or lock for an ephemeral run.
    run(["--mode", "json", "--model", "one-off", "--no-session"], "stdin-only prompt")
    path, body = requests.pop()
    assert path == "/deepseek" and body["model"] == "one-off"
    assert body["messages"][-1]["content"] == "stdin-only prompt"
    assert {p.name for p in state.iterdir()} == {"model.json"}

    # Resolve the Codex alias and validate the effort using its model catalogue.
    effort_cache = state / "codex-efforts.json"
    effort_cache.write_text('{"codex-test":["high"]}')
    run(["--print", "--provider", "codex", "--model", "codex-test",
         "--no-session", "--thinking", "high", "probe"], "")
    path, body = requests.pop()
    assert path == "/openai-codex" and body["model"] == "codex-test"
    assert effort_cache.read_text() == '{"codex-test":["high"]}'

    # A pre-existing conversation and a different saved model expose accidental writes.
    run(["--print", "seed conversation"], "")
    assert requests.pop()[0] == "/deepseek"

    def snapshot():
        return {p.name: p.read_bytes() for p in state.iterdir()}

    original = snapshot()
    assert "sessions.sqlite3" in original
    diff = ("diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\n"
            "diff --git a/b b/b\n@@ -2,1 +2,0 @@\n-removed\n")
    prompt = "Review this change."
    for interface in (["--print"], ["--mode", "json"]):
        for codemode in (False, True):
            args = interface + (["--codemode"] if codemode else []) + [
                "--provider", "openrouter", "--model", "openai/gpt-fixture",
                "--thinking", "high", "--no-session", prompt]
            output = run(args, diff)
            assert len(requests) == 1
            path, body = requests.pop()
            assert path == "/openrouter" and body["model"] == "openai/gpt-fixture"
            assert body["reasoning"]["effort"] == "high"
            users = [m["content"] for m in body["messages"] if m["role"] == "user"]
            assert users == [diff + "\n" + prompt], users
            tools = [tool["function"]["name"] for tool in body["tools"]]
            assert ("codemode" in tools) == codemode
            if interface == ["--print"]:
                assert "reviewed" in output
            else:
                events = [json.loads(line) for line in output if line.startswith("{")]
                assert events[-1] == {"type": "done", "session_id": None, "text": "reviewed"}
            assert snapshot() == original

    target = repo / "review.txt"
    target.write_text("original")
    for interface in (["--print"], ["--mode", "json"]):
        run(interface + ["--no-session", "--no-tools", prompt], diff)
        assert len(requests) == 1
        _, request = requests.pop()
        assert "tools" not in request
        assert [m["content"] for m in request["messages"] if m["role"] == "user"] == [diff + "\n" + prompt]
        for selection, expected in [
            (["--tools", "ls,read"], {"ls", "read"}),
            (["--tools", "read", "--codemode"], {"read"}),
            (["--tools", "codemode,read"], {"codemode", "read"}),
            (["--codemode", "--no-tools"], set()),
        ]:
            denied = ("write", {"path": target.name, "text": "modified"}) if expected else ("codemode", {"code": "text(1)"})
            next_calls.append(denied)
            run(interface + ["--no-session"] + selection + [prompt], diff)
            assert len(requests) == 2
            for _, request in requests:
                assert {t["function"]["name"] for t in request.get("tools", [])} == expected
                if not expected:
                    assert "tools" not in request
                if "codemode" in expected:
                    desc = next(t["function"]["description"] for t in request["tools"]
                                if t["function"]["name"] == "codemode")
                    assert "tools.read(" in desc and "tools.search(" not in desc
                assert [m["content"] for m in request["messages"] if m["role"] == "user"] == [diff + "\n" + prompt]
            result = requests[-1][1]["messages"][-1]
            assert result["role"] == "tool" and denied[0] in result["content"] and "unavailable" in result["content"]
            assert target.read_text() == "original"
            assert snapshot() == original
            requests.clear()

    # Persisted runs can override thinking too, without changing a later continuation.
    run(["--continue", "--print", "--thinking", "high", "follow up"], "")
    _, body = requests.pop()
    assert body["reasoning"]["effort"] == "high"
    run(["--continue", "--print", "last turn"], "")
    _, body = requests.pop()
    assert body["model"] == "saved-model" and "reasoning" not in body
    assert [m["content"] for m in body["messages"] if m["role"] == "user"] == [
        "seed conversation", "follow up", "last turn"]
