"""Capture the CLI's serialized provider requests over loopback."""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import sqlite3
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

    def run(args, stdin, *, cwd=repo, error=None):
        env = dict(os.environ, PANDA_HOME=str(state), HOME=str(root),
                   PANDA_ONESHOT_ARGS=json.dumps(args),
                   PANDA_ONESHOT_SERVER=f"http://127.0.0.1:{server.server_port}")
        result = subprocess.run([sys.argv[1], "--exact", "automation::tests::oneshot_fixture", "--nocapture"],
                                cwd=cwd, env=env, input=stdin, text=True,
                                capture_output=True, timeout=20)
        if error is not None:
            assert result.returncode != 0 and error in result.stderr, (result.stdout, result.stderr)
            assert not requests, requests
            return
        assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout.splitlines()

    # A fresh state directory should never acquire a database or lock for an ephemeral run.
    run(["--print", "--session", "1", "probe"], "", error="unknown session")
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

    (state / "AGENTS.md").write_text("Preserve fixture conventions.")
    base_file = root / "review base.md"
    base_file.write_text("Review as a maintainer.\nExplain risks.")
    rubric = repo / "review rubric.md"
    rubric.write_text("Check boundary cases.\nCheck retries.\n")
    for interface in (["--print"], ["--mode", "json"]):
        for codemode in (False, True):
            common = interface + (["--codemode"] if codemode else []) + ["--no-session"]

            def capture(flags):
                run(common + flags + [prompt], diff)
                assert len(requests) == 1
                _, body = requests.pop()
                system, user = body["messages"]
                assert system["role"] == "system"
                assert user == {"role": "user", "content": diff + "\n" + prompt}
                names = {tool["function"]["name"] for tool in body.get("tools", [])}
                if "--no-tools" in flags:
                    assert "tools" not in body
                else:
                    assert ("codemode" in names) == codemode
                return system["content"]

            default = capture([])
            assert default.startswith("You are Panda, a coding assistant.")
            assert default.count("Preserve fixture conventions.") == 1
            assert ("start with one codemode call" in default) == codemode
            context = default[default.index("\nInstructions from "):]
            appended = ["--append-system-prompt", rubric.name,
                        "--append-system-prompt", "Report findings first."]
            suffix = "\n\n" + rubric.read_text() + "\n\nReport findings first."
            assert capture(appended) == default + suffix
            assert capture(appended + ["--system-prompt", "Review independently."]) == (
                "Review independently." + context + suffix)
            assert capture(["--system-prompt", str(base_file)] + appended) == (
                base_file.read_text() + context + suffix)
            plain = capture(["--no-tools"])
            plain_context = plain[plain.index("\nInstructions from "):]
            assert "start with one codemode call" not in plain
            assert capture(["--no-tools", "--system-prompt", str(base_file)] + appended) == (
                base_file.read_text() + plain_context + suffix)
    (state / "AGENTS.md").unlink()
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

    # Prompt and thinking overrides apply only to this invocation of a saved session.
    run(["--continue", "--print", "--thinking", "high", "--system-prompt", "Temporary base.",
         "--append-system-prompt", "Temporary rubric.", "follow up"], "")
    _, body = requests.pop()
    assert body["reasoning"]["effort"] == "high"
    assert body["messages"][0]["content"] == "Temporary base.\n\nTemporary rubric."
    run(["--continue", "--print", "last turn"], "")
    _, body = requests.pop()
    assert body["model"] == "saved-model" and "reasoning" not in body
    assert body["messages"][0]["content"].startswith("You are Panda, a coding assistant.")
    assert "Temporary" not in body["messages"][0]["content"]
    assert [m["content"] for m in body["messages"] if m["role"] == "user"] == [
        "seed conversation", "follow up", "last turn"]

    def done(output):
        events = [json.loads(line) for line in output if line.startswith("{")]
        assert events[-1]["type"] == "done"
        return events[-1]

    def new_session(prompt):
        result = done(run(["--mode", "json", prompt], ""))
        assert len(requests) == 1
        requests.pop()
        return result["session_id"]

    a = new_session("Review A")
    b = new_session("Review B")
    assert a < b
    with sqlite3.connect(state / "sessions.sqlite3") as db:
        def saved_others():
            return db.execute("SELECT id, revision, data FROM sessions WHERE id != ? ORDER BY id", (a,)).fetchall()

        others = saved_others()
        history = [{"role": "user", "content": "Review A"},
                   {"role": "assistant", "content": "reviewed"}]
        for interface, follow_up in [(["--print"], "First follow-up"), (["--mode", "json"], "Second follow-up")]:
            output = run(interface + ["--session", str(a), follow_up], "")
            assert len(requests) == 1
            _, body = requests.pop()
            history.append({"role": "user", "content": follow_up})
            assert body["messages"][0]["role"] == "system"
            assert body["messages"][1:] == history, body["messages"]
            if interface == ["--print"]:
                assert "reviewed" in output
            else:
                assert done(output) == {"type": "done", "session_id": a, "text": "reviewed"}
            history.append({"role": "assistant", "content": "reviewed"})
            assert saved_others() == others

    foreign = root / "other-repo"
    foreign.mkdir()
    before = snapshot()
    for interface in (["--print"], ["--mode", "json"]):
        run(interface + ["--session", str(b + 1), "probe"], "", error="unknown session")
        run(interface + ["--session", str(a), "probe"], "", cwd=foreign, error="another workspace")
        for conflict in ("--continue", "--no-session"):
            run(interface + [conflict, "--session", str(a), "probe"], "", error="conflicts")
    assert snapshot() == before
