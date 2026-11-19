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
        payload = json.loads(body)
        requests.append((self.path, payload))
        if payload["messages"][0]["content"].startswith("Summarize the supplied conversation"):
            choice = {"delta": {"content": "Saved old exchange"}, "finish_reason": "stop"}
        elif next_calls:
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

    def done(output):
        events = [json.loads(line) for line in output if line.startswith("{")]
        assert events[-1]["type"] == "done"
        return events[-1]

    # A fresh state directory should never acquire a database or lock for an ephemeral run.
    run(["--print", "--session", "1", "probe"], "", error="unknown session")
    run(["--mode", "json", "--model", "one-off", "--no-session"], "stdin-only prompt")
    path, body = requests.pop()
    assert path == "/deepseek" and body["model"] == "one-off"
    assert body["messages"][-1]["content"] == "stdin-only prompt"
    ephemeral = root / "unused store"
    for interface in (["--print"], ["--mode", "json"]):
        output = run(interface + ["--session-dir", str(ephemeral), "--no-session", "ephemeral"], "")
        assert len(requests) == 1
        _, body = requests.pop()
        assert body["messages"][-1]["content"] == "ephemeral"
        if interface == ["--mode", "json"]:
            assert done(output)["session_id"] is None
        assert not ephemeral.exists()
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

    def new_session(prompt, store_flags=(), cwd=repo):
        result = done(run(["--mode", "json", *store_flags, prompt], "", cwd=cwd))
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

    # Session 1 belongs to a different conversation in each store.
    custom = root / "custom store"
    selected = ["--session-dir", "../custom store"]
    output = run(["--mode", "json"] + selected + ["Custom review"], "")
    assert done(output)["session_id"] == 1
    assert len(requests) == 1
    _, body = requests.pop()
    assert body["model"] == "saved-model"
    assert {p.name for p in custom.iterdir()} == {"sessions.sqlite3", "session-1.lock"}
    assert snapshot() == before

    def followups(store_flags, selector, session_id, users):
        for interface in (["--print"], ["--mode", "json"]):
            follow_up = f"Follow-up {len(users)} on {users[0]}"
            output = run(interface + store_flags + selector + [follow_up], "")
            assert len(requests) == 1
            _, body = requests.pop()
            users.append(follow_up)
            assert [m["content"] for m in body["messages"] if m["role"] == "user"] == users
            if interface == ["--mode", "json"]:
                assert done(output)["session_id"] == session_id
            else:
                assert "reviewed" in output

    custom_users = ["Custom review"]
    followups(selected, ["--session", "1"], 1, custom_users)
    followups(selected, ["--continue"], 1, custom_users)
    run(["--print"] + selected + ["--session", str(b), "probe"], "", error="unknown session")
    run(["--print", "--session-dir", str(custom), "--session", "1", "probe"], "",
        cwd=foreign, error="another workspace")
    assert snapshot() == before
    custom_db = custom / "sessions.sqlite3"
    custom_before = custom_db.read_bytes()
    default_users = ["seed conversation", "follow up", "last turn"]
    followups([], ["--session", "1"], 1, default_users)
    followups([], ["--continue"], 1, default_users)
    assert custom_db.read_bytes() == custom_before

    for store, flags in [(state, []), (custom, ["--session-dir", str(custom)])]:
        for interface in (["--print"], ["--mode", "json"]):
            a = new_session("Start A", flags)
            b = new_session("Start B", flags)
            assert a < b
            with sqlite3.connect(store / "sessions.sqlite3") as db:
                def saved_b():
                    return db.execute("SELECT * FROM sessions WHERE id=?", (b,)).fetchone()

                original_b = saved_b()
                run(interface + flags + ["--session", str(a), "Update A"], "")
                assert len(requests) == 1
                requests.pop()
                archive = root / f"{store.name}-{b}.jsonl"
                run(["--export-session", str(b), str(archive)] + flags, "")
                assert archive.is_file() and not requests
                new_session("Other workspace", flags, cwd=foreign)

                output = run(interface + flags + ["--continue", "Continue A"], "")
                assert len(requests) == 1
                messages = requests.pop()[1]["messages"]
                assert messages[0]["role"] == "system"
                assert messages[1:] == [
                    {"role": "user", "content": "Start A"},
                    {"role": "assistant", "content": "reviewed"},
                    {"role": "user", "content": "Update A"},
                    {"role": "assistant", "content": "reviewed"},
                    {"role": "user", "content": "Continue A"},
                ], messages
                if interface == ["--print"]:
                    assert "reviewed" in output
                else:
                    assert done(output) == {"type": "done", "session_id": a, "text": "reviewed"}
                assert saved_b() == original_b

    workspace_store = repo / "unsafe-store"
    workspace_store.mkdir(mode=0o700)
    alias = root / "alias"
    alias.symlink_to(workspace_store)
    run(["--print", "--session-dir", str(alias), "probe"], "", error="workspace overlaps")
    assert not list(workspace_store.iterdir())
    shared = root / "shared-store"
    shared.mkdir()
    shared.chmod(0o755)
    run(["--print", "--session-dir", str(shared), "probe"], "", error="private directory")
    assert not list(shared.iterdir())

    output = run(["Review this diff"], diff)
    assert "reviewed" in output and len(requests) == 1
    _, body = requests.pop()
    assert body["messages"][1:] == [{"role": "user", "content": diff + "\nReview this diff"}]

    fixture = Path(__file__).resolve().parent / "fixtures" / "pi-v3.jsonl"
    imported = run(["--import-session", str(fixture)], "")
    sid = next(json.loads(line)["session_id"] for line in imported if line.startswith("{"))
    assert not requests
    run(["--print", "--session", str(sid), "continue selected branch"], "")
    assert len(requests) == 1
    messages = requests.pop()[1]["messages"][1:]
    assert [m["content"] for m in messages] == [
        "Earlier conversation summary:\nunique summary", "kept prompt", "",
        "unique tool output", "active answer", "continue selected branch",
    ], messages
    assert messages[2]["tool_calls"][0]["id"] == messages[3]["tool_call_id"] == "tool-id"
    exported = root / "roundtrip.jsonl"
    run(["--export-session", str(sid), str(exported)], "")
    assert not requests
    original = [json.loads(line) for line in fixture.read_text().splitlines()]
    records = [json.loads(line) for line in exported.read_text().splitlines()]
    assert records[0] == dict(original[0], cwd=str(repo.resolve()))
    assert records[1:-2] == original[1:]
    assert records[-2]["parentId"] == "a-answer" and records[-1]["parentId"] == records[-2]["id"]
    assert records[-1]["message"]["content"][0]["text"] == "reviewed"
    saved = exported.read_bytes()
    run(["--export-session", str(sid), str(exported)], "", error="creating export")
    assert exported.read_bytes() == saved

    if os.environ.get("PANDA_TEST_PI"):
        env = dict(PATH=os.environ["PATH"], HOME=str(root), PI_CODING_AGENT_DIR=str(root / "pi-agent"))
        cmd = [os.environ["PANDA_TEST_PI"], "--offline", "-ne", "-ns", "-np", "-nc", "--no-themes",
               "--mode", "rpc", "--session", str(exported)]
        commands = ["get_entries", "get_tree", "get_messages"]
        pi = subprocess.run(cmd, cwd=repo, env=env, capture_output=True, text=True, timeout=20,
                            input="".join(json.dumps({"type": c}) + "\n" for c in commands))
        assert pi.returncode == 0, pi.stderr
        replies = {e["command"]: e for e in map(json.loads, pi.stdout.splitlines()) if e["type"] == "response"}
        assert all(replies[c]["success"] for c in commands), replies
        loaded = replies["get_entries"]["data"]["entries"]
        assert loaded[:len(records)-1] == records[1:]
        # Pi can record its local thinking/model setting when opening the file.
        leaf = records[-1]["id"]
        for added in loaded[len(records)-1:]:
            assert added["parentId"] == leaf and added["type"] in {"thinking_level_change", "model_change"}
            leaf = added["id"]
        tree = replies["get_tree"]["data"]
        assert tree["leafId"] == leaf
        assert {c["entry"]["id"] for c in tree["tree"][0]["children"]} == {"sibling", "keep"}
        active = json.dumps(replies["get_messages"]["data"]["messages"])
        for term in ["unique summary", "kept prompt", "unique tool output", "continue selected branch"]:
            assert active.count(term) == 1, active
        assert "sibling only" not in active

    # The tool exchange exceeds the model's default retention (1,500 tokens),
    # but fits the configured allowance. Its result crosses the byte threshold.
    (state / "catalog-deepseek.json").write_text(json.dumps([["saved-model", 8000]]))
    (state / "settings.json").write_text(json.dumps({"compaction": {"keepRecentTokens": 2400}}))
    compiler_error = "src/lib.rs:7: error[E0308]: expected usize, found String"
    (repo / "compiler.log").write_text("build detail\n" * 520 + compiler_error + "\n")
    old = "Old exchange " + "x" * 3600
    recent = "Run the compiler"
    follow_up = "Explain the exact error"
    for automatic in (False, True):
        sid = new_session(old)
        if automatic:
            run(["--continue"], ":context 8192\n:q\n")
        next_calls.append(("shell", {"command": "cat compiler.log >&2; exit 7"}))
        run(["--print", "--continue", recent], "")
        if not automatic:
            run(["--continue"], ":compact\n:q\n")

        def saved():
            with sqlite3.connect(state / "sessions.sqlite3") as db:
                return json.loads(db.execute("SELECT data FROM sessions WHERE id=?", (sid,)).fetchone()[0])

        session = saved()
        raw = session["history"]
        assert len(raw) == 6 and raw[:3] == [
            {"User": old}, {"Assistant": {"text": "reviewed", "calls": []}}, {"User": recent}]
        call, = raw[3]["Assistant"]["calls"]
        result = raw[4]["Result"]
        assert call["name"] == "shell" and result["id"] == call["id"] and result["isError"]
        assert result["text"].count(compiler_error) == 1 and raw[5] == raw[1]
        assert session["checkpoint"] == {"through": 2, "summary": "Saved old exchange"}
        initial, second, third = [req["messages"] for _, req in requests]
        assert initial[-1]["content"] == recent
        summary, tool_exchange = (second, third) if automatic else (third, second)
        assert len(summary) == 2 and summary[0]["content"].startswith("Summarize")
        prefix, transcript = summary[1]["content"].split("Transcript data:\n")
        assert prefix == "Prior summary:\n\n" and list(map(json.loads, transcript.splitlines())) == raw[:2]
        requests.clear()

        # The entire remaining exchange fits. A second compact makes no request.
        run(["--continue"], ":compact\n:q\n")
        assert not requests and saved() == session
        run(["--print", "--continue", follow_up], "")
        (_, request), = requests
        summary_message = {"role": "user", "content": "Earlier conversation summary:\nSaved old exchange"}
        tool_call, tool_result = tool_exchange[-2:]
        assert tool_call["tool_calls"][0]["id"] == call["id"]
        assert tool_result == {"role": "tool", "content": result["text"], "tool_call_id": call["id"]}
        assert request["messages"][1:] == [
            summary_message, {"role": "user", "content": recent}, tool_call, tool_result,
            {"role": "assistant", "content": "reviewed"}, {"role": "user", "content": follow_up}]
        assert saved()["history"] == raw + [{"User": follow_up}, raw[5]]
        requests.clear()

    # Zero is explicit, and removing the setting restores manual compact-all
    # for a session previously opened with retention enabled.
    for setting in ({"compaction": {"keepRecentTokens": 0}}, {}):
        path = state / "settings.json"
        path.write_text(json.dumps({"compaction": {"keepRecentTokens": 2400}}))
        sid = new_session("Short exchange")
        run(["--continue"], ":compact\n:q\n")
        assert not requests
        path.write_text(json.dumps(setting))
        run(["--continue"], ":compact\n:q\n")
        (_, request), = requests
        assert "Short exchange" in request["messages"][-1]["content"]
        requests.clear()
    path.write_text('{"compaction":{"keepRecentTokens":-1}}')
    run(["--print", "--no-session", "probe"], "", error="settings.json")
