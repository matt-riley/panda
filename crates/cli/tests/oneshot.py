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
length_stops = []


class Provider(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        payload = json.loads(body)
        requests.append((self.path, payload))
        usage = None
        if payload["messages"][0]["content"].startswith("Summarize the supplied conversation"):
            choice = {"delta": {"content": "Saved old exchange"}, "finish_reason": "stop"}
        elif next_calls:
            name, args = next_calls.pop(0)
            call = {"index": 0, "id": "fixture-call", "type": "function",
                    "function": {"name": name, "arguments": json.dumps(args)}}
            choice = {"delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}
        elif length_stops:
            partial, used, *override = length_stops.pop(0)
            name, args = override or ("shell", {"command": "touch cut-off-tool-ran"})
            delta = {"content": partial, "tool_calls": [{
                "index": 0, "id": "cut-off", "function": {"name": name,
                "arguments": json.dumps(args)}}]}
            choice = {"delta": delta, "finish_reason": "length"}
            if used is not None:
                usage = {"prompt_tokens": 80, "completion_tokens": used}
        else:
            choice = {"delta": {"content": "reviewed"}, "finish_reason": "stop"}
        events = [{"choices": [choice]}]
        if usage is not None:
            events.append({"choices": [], "usage": usage})
        response = ("".join("data: " + json.dumps(e) + "\n\n" for e in events) + "data: [DONE]\n\n").encode()
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

    def run(args, stdin, *, cwd=repo, error=None, expected_requests=0):
        env = dict(os.environ, PANDA_HOME=str(state), HOME=str(root),
                   PANDA_ONESHOT_ARGS=json.dumps(args),
                   PANDA_ONESHOT_SERVER=f"http://127.0.0.1:{server.server_port}")
        result = subprocess.run([sys.argv[1], "--exact", "automation::tests::oneshot_fixture", "--nocapture"],
                                cwd=cwd, env=env, input=stdin, text=True,
                                capture_output=True, timeout=20)
        if error is not None:
            assert result.returncode != 0 and error in result.stderr, (result.stdout, result.stderr)
            assert len(requests) == expected_requests, requests
            return
        assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout.splitlines()

    def saved_session(sid):
        with sqlite3.connect(state / "sessions.sqlite3") as db:
            return json.loads(db.execute("SELECT data FROM sessions WHERE id=?", (sid,)).fetchone()[0])

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
                report = archive.with_suffix(".html")
                run(["--export-session", str(b), str(report)] + flags, "")
                html = report.read_bytes()
                assert html.startswith(b"<!doctype html>"), html[:80]
                run(["--export-session", str(b), str(report)] + flags, "", error="creating export")
                assert report.read_bytes() == html and not requests
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
    request = requests.pop()[1]
    assert "Archived Pi system prompt" not in json.dumps(request)
    messages = request["messages"][1:]
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
    # Imported model metadata survives, but a native reply currently exports
    # without responder identity. Keep this portability limit visible.
    assert records[-1]["message"]["provider"] == "panda"
    assert records[-1]["message"]["model"] == "unknown"
    saved = exported.read_bytes()
    run(["--export-session", str(sid), str(exported)], "", error="creating export")
    assert exported.read_bytes() == saved

    def pi_session(archive, commands):
        env = dict(PATH=os.environ["PATH"], HOME=str(root), PI_CODING_AGENT_DIR=str(root / "pi-agent"))
        cmd = [os.environ["PANDA_TEST_PI"], "--offline", "-ne", "-ns", "-np", "-nc", "--no-themes",
               "--mode", "rpc", "--session", str(archive)]
        pi = subprocess.run(cmd, cwd=repo, env=env, capture_output=True, text=True, timeout=20,
                            input="".join(json.dumps({"type": c}) + "\n" for c in commands))
        assert pi.returncode == 0, pi.stderr
        replies = {e["command"]: e for e in map(json.loads, pi.stdout.splitlines()) if e["type"] == "response"}
        assert all(replies[c]["success"] for c in commands), replies
        return replies

    if os.environ.get("PANDA_TEST_PI"):
        replies = pi_session(exported, ["get_entries", "get_tree", "get_messages"])
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

        session = saved_session(sid)
        raw = session["history"]
        assert len(raw) == 6 and raw[:3] == [
            {"User": old}, {"Assistant": {"text": "reviewed", "calls": []}}, {"User": recent}]
        call, = raw[3]["Assistant"]["calls"]
        result = raw[4]["Result"]
        assert call["name"] == "shell" and result["id"] == call["id"] and result["isError"]
        assert result["text"].count(compiler_error) == 1 and raw[5] == raw[1]
        assert session["checkpoint"] == {"through": 2, "summary": "Saved old exchange", "files": {"readFiles": [], "modifiedFiles": []}}
        initial, second, third = [req["messages"] for _, req in requests]
        assert initial[-1]["content"] == recent
        summary, tool_exchange = (second, third) if automatic else (third, second)
        assert len(summary) == 2 and summary[0]["content"].startswith("Summarize")
        prefix, transcript = summary[1]["content"].split("Transcript data:\n")
        assert prefix == "Prior summary:\n\n" and list(map(json.loads, transcript.splitlines())) == raw[:2]
        requests.clear()

        # The entire remaining exchange fits. A second compact makes no request.
        run(["--continue"], ":compact\n:q\n")
        assert not requests and saved_session(sid) == session
        run(["--print", "--continue", follow_up], "")
        (_, request), = requests
        summary_message = {"role": "user", "content": "Earlier conversation summary:\nSaved old exchange"}
        tool_call, tool_result = tool_exchange[-2:]
        assert tool_call["tool_calls"][0]["id"] == call["id"]
        assert tool_result == {"role": "tool", "content": result["text"], "tool_call_id": call["id"]}
        assert request["messages"][1:] == [
            summary_message, {"role": "user", "content": recent}, tool_call, tool_result,
            {"role": "assistant", "content": "reviewed"}, {"role": "user", "content": follow_up}]
        assert saved_session(sid)["history"] == raw + [{"User": follow_up}, raw[5]]
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

    path.unlink()
    (state / "catalog-openrouter-output.json").write_text(json.dumps({"fixture": 128}))
    model = ["--provider", "openrouter", "--model", "fixture"]
    sid = new_session("Older exchange", model)
    prompt = "Check the marker"
    next_calls.append(("shell", {"command": "echo once >> calls; echo tool-marker"}))
    length_stops.append(("unfinished draft", 12))
    result = done(run(["--mode", "json", "--continue", *model, prompt], ""))
    assert result["text"] == "reviewed" and result["session_id"] == sid
    initial, stopped, summary, retry = [r for _, r in requests]
    assert [m["content"] for m in initial["messages"] if m["role"] == "user"].count(prompt) == 1
    assert stopped["max_tokens"] == retry["max_tokens"] == 128
    assert summary["messages"][0]["content"].startswith("Summarize")
    assert "Older exchange" in summary["messages"][1]["content"]
    assert all(marker not in str(summary) for marker in (prompt, "tool-marker", "unfinished draft", "cut-off"))
    projected = retry["messages"][1:]
    assert [m["role"] for m in projected] == ["user", "user", "assistant", "tool"]
    assert [m["content"] for m in projected[:2]] == ["Earlier conversation summary:\nSaved old exchange", prompt]
    assert projected[-2:] == stopped["messages"][-2:] and "tool-marker" in projected[-1]["content"]
    assert all(marker not in str(retry) for marker in ("unfinished draft", "cut-off"))
    assert (repo / "calls").read_text() == "once\n"
    partial, = [m["Truncated"] for m in saved_session(sid)["history"] if "Truncated" in m]
    assert partial["text"] == "unfinished draft" and partial["output_tokens"] == 12
    assert partial["tool_fragments"][0]["id"] == "cut-off"
    requests.clear()
    run(["--print", "--continue", *model, "follow up"], "")
    (_, reopened), = requests
    assert reopened["messages"][1:] == projected + [
        {"role": "assistant", "content": "reviewed"}, {"role": "user", "content": "follow up"}]
    requests.clear()

    archive = root / "truncated.jsonl"
    run(["--export-session", str(sid), str(archive)], "")
    imported = run(["--import-session", str(archive)], "")
    restored = next(json.loads(line)["session_id"] for line in imported if line.startswith("{"))
    assert {"Truncated": partial} in saved_session(restored)["history"]
    run(["--print", "--session", str(restored), *model, "after import"], "")
    (_, after_import), = requests
    assert after_import["messages"][1:] == reopened["messages"][1:] + [
        {"role": "assistant", "content": "reviewed"}, {"role": "user", "content": "after import"}]
    requests.clear()

    for counts in ([128], [None], [5, 7]):
        length_stops.extend((f"truncated attempt {i}", n) for i, n in enumerate(counts))
        run(["--print", *model, "stop test"], "", error="length", expected_requests=len(counts))
        assert not length_stops
        requests.clear()
    assert not (repo / "cut-off-tool-ran").exists()

    length_stops.append(("unknown allowance", 1))
    run(["--print", "--provider", "openrouter", "--model", "uncatalogued", "unknown cap"], "",
        error="length", expected_requests=1)
    assert "max_tokens" not in requests.pop()[1]

    # The unit-test CLI hosts the loopback provider. Its sandbox must launch the real file worker.
    build = subprocess.check_output(["cargo", "build", "--locked", "--bin", "panda", "--message-format=json"],
                                    cwd=Path(__file__).parent, text=True)
    os.environ["PANDA_TEST_WORKER"] = next(m["executable"] for m in map(json.loads, build.splitlines())
                                          if m.get("executable") and m["target"]["name"] == "panda")

    ranking_repo = root / "ranking"
    ranking_repo.mkdir()
    for name, count in [("a.rs", 34), ("b.rs", 17)]:
        (ranking_repo / name).write_text("TODO\n" * count)

    (ranking_repo / "version.txt").write_text("1.2.3")

    for interface in (["--print"], ["--mode", "json"]):
        flags = ["--codemode-only", "--tools", "search"]
        seed = 'store("cursor", tools.search({query:"TODO"}).next_offset)'
        next_calls.append(("codemode", {"code": seed}))
        sid = done(run(["--mode", "json", *flags, "Save page one"], "", cwd=ranking_repo))["session_id"]
        requests.clear()
        resume = [*interface, *flags, "--session", str(sid)]
        script = """
            const page = tools.search({query:"TODO", offset:load("cursor")});
            text("last page");
            for (const hit of page.items) console.warn([hit.path, hit.line, hit.text]);
            store("cursor", page.next_offset);
            throw Error(["page", "failed"].join("-"));
        """
        next_calls.append(("codemode", {"code": script}))
        output = run([*resume, "Try page two"], "", cwd=ranking_repo)
        (_, before), (_, after) = requests
        _, result = after["messages"][len(before["messages"]):]
        assert result["role"] == "tool"
        content = result["content"]
        hit = '["b.rs",17,"TODO"]'
        assert f"last page\n<console_output>\n{hit}\n</console_output>\nScript error:" in content, content
        assert content.count(hit) == content.count("page-failed") == 1, content
        assert content.index(hit) < content.index("page-failed"), content
        if interface == ["--mode", "json"]:
            events = [json.loads(line) for line in output if line.startswith("{")]
            calls = [e["event"]["data"]["call"] for e in events
                     if e["type"] == "event" and e["event"]["type"] == "nested_call"]
            assert [(c["name"], c["args"]) for c in calls] == [("search", {"query":"TODO", "offset":50})]
        requests.clear()
        next_calls.append(("codemode", {"code": 'return load("cursor")'}))
        run([*resume, "Inspect saved cursor"], "", cwd=ranking_repo)
        assert requests[-1][1]["messages"][-1]["content"] == "50"
        requests.clear()

    def exercise_tools(interface, flags, declared, call, prompt):
        next_calls.append(call)
        output = run([*interface, "--no-session", *flags, prompt], "", cwd=ranking_repo)
        first, followup = [body for _, body in requests]
        requests.clear()
        for body in (first, followup):
            assert {t["function"]["name"] for t in body.get("tools", [])} == declared, body.get("tools")
        results = [m for m in followup["messages"] if m["role"] == "tool"]
        assert len(results) == 1, results
        result = results[0]
        events = [json.loads(line) for line in output if line.startswith("{")]
        calls = [e["event"]["data"]["call"] for e in events
                 if e["type"] == "event" and e["event"]["type"] == "nested_call"]
        nested = [(c["name"], c["args"]) for c in calls]
        return result["content"], nested

    rank = """
        const [hit] = await searchTools("find matching lines", {limit: 1});
        const description = await describeTool(hit.name);
        if (!description.includes("offset?: number")) throw Error(description);
        const counts = new Map();
        let offset = 0;
        do {
            const page = tools[hit.name]({query: "TODO", offset});
            console.log("searched", offset);
            for (const {path} of page.items) counts.set(path, (counts.get(path) ?? 0) + 1);
            offset = page.next_offset;
        } while (offset !== null);
        return [ALL_TOOLS.map(tool => tool.name).sort(), hit.name, [...counts].sort((a,b) => b[1]-a[1])];
    """
    read_version = """
        let blocked = false;
        try { tools.search({query: "TODO"}); } catch (err) { blocked = err instanceof TypeError; }
        text([ALL_TOOLS.map(tool => tool.name), (await searchTools("search")).map(tool => tool.name),
              await describeTool("search") === undefined, Object.keys(tools), blocked,
              tools.read({path: "version.txt"})]);
    """
    selected = {"codemode", "read", "search"}
    for interface in (["--mode", "json"], ["--print"]):
        read_once = ("codemode", {"code": 'return tools.read({path: "version.txt"});'})
        version, nested = exercise_tools(interface, ["--codemode-only", "--tools", "read"],
                                         {"codemode"}, read_once, "Read version.txt")
        assert version == "1.2.3", version
        if interface == ["--mode", "json"]:
            assert nested == [("read", {"path": "version.txt"})], nested
        for flag in ("--codemode", "--codemode-only"):
            only = flag == "--codemode-only"
            flags = [flag, "--tools", "codemode,read,search"]
            declared = {"codemode"} if only else selected
            found, nested = exercise_tools(interface, flags, declared, ("codemode", {"code": rank}), "Rank TODOs")
            ranking, console = found.split("\n<console_output>\n")
            assert json.loads(ranking) == [["read", "search"], "search", [["a.rs", 34], ["b.rs", 17]]], found
            assert console == "searched 0\nsearched 50\n</console_output>", found
            if interface == ["--mode", "json"]:
                assert nested == [("search", {"query": "TODO", "offset": n}) for n in (0, 50)], nested

            restricted = [flag, "--tools", "codemode,read"]
            hidden, nested = exercise_tools(interface, restricted, {"codemode"} if only else {"codemode", "read"},
                                            ("codemode", {"code": read_version}), "Read version.txt")
            assert json.loads(hidden) == [["read"], [], True, ["read"], True, "1.2.3"], hidden
            if interface == ["--mode", "json"]:
                assert nested == [("read", {"path": "version.txt"})], nested

            if only:
                # Search is allowed inside the worker. The provider tries calling it directly.
                denied, nested = exercise_tools(interface, flags, declared, ("search", {"query": "TODO"}), "Search directly")
                assert "tool search unavailable for this run" in denied and not nested, denied
            else:
                version, nested = exercise_tools(interface, flags, declared, ("read", {"path": "version.txt"}), "Read version.txt")
                assert version == "1.2.3" and not nested, version
                denied, _ = exercise_tools(interface, restricted, {"codemode", "read"}, ("search", {"query": "TODO"}), "Search directly")
                assert "tool search unavailable for this run" in denied, denied

    catalog = ("codemode", {"code": "text(ALL_TOOLS.map(tool => tool.name).sort())"})
    available, _ = exercise_tools(["--print"], ["--codemode-only"], {"codemode"}, catalog, "List helpers")
    assert json.loads(available) == ["ls", "read", "repo_map", "search"], available
    available, _ = exercise_tools(["--print"], ["--codemode-only", "--tools", "read"], {"codemode"}, catalog, "List helpers")
    assert json.loads(available) == ["read"], available
    denied, _ = exercise_tools(["--print"], ["--codemode-only", "--no-tools"], set(), catalog, "List helpers")
    assert "tool codemode unavailable for this run" in denied, denied

    # This model always summarizes as "Saved old exchange", without filenames.
    for file in ("a.rs", "b.rs", "d.rs"):
        (repo / file).write_text("before")
    sid = new_session("Track the workspace", model)
    selected = ["--session", str(sid)]
    next_calls.append(("codemode", {"code": 'tools.read({path:"a.rs"}); tools.read({path:"a.rs"}); throw Error("later failure")'}))
    next_calls.extend(("read", {"path": file}) for file in ("b.rs", "missing.rs", "b.rs"))
    run(["--print", "--codemode", *selected, *model, "Inspect inputs"], "")
    run(["--continue"], ":compact\n:q\n")
    assert saved_session(sid)["checkpoint"]["files"] == {"readFiles": ["a.rs", "b.rs"], "modifiedFiles": []}
    requests.clear()
    next_calls.extend([
        ("edit", {"path": "b.rs", "old": "before", "new": "after"}),
        ("write", {"path": "c.rs", "text": "new"}),
        ("edit", {"path": "a.rs", "old": "absent", "new": "bad"}),
    ])
    length_stops.append(("incomplete edit", 12, "edit", {"path": "d.rs", "old": "before", "new": "bad"}))
    run(["--print", *selected, *model, "Update outputs"], "")
    assert [(repo / f).read_text() for f in ("a.rs", "b.rs", "c.rs", "d.rs")] == ["before", "after", "new", "before"]
    run(["--continue"], ":compact\n:q\n")
    requests.clear()

    summary = "Saved old exchange\n\n<read-files>\na.rs\n</read-files>\n\n<modified-files>\nb.rs\nc.rs\n</modified-files>"
    def check_files(session_id):
        run(["--print", "--session", str(session_id), *model, "Continue"], "")
        (_, payload), = requests
        messages = payload["messages"][1:]
        assert messages[0]["content"] == "Earlier conversation summary:\n" + summary, messages
        assert all(json.dumps(messages).count(f) == 1 for f in ("a.rs", "b.rs", "c.rs"))
        assert all(f not in json.dumps(messages) for f in ("d.rs", "missing.rs"))
        requests.clear()

    check_files(sid)
    archive = root / "file-activity.jsonl"
    run(["--export-session", str(sid), str(archive)], "")
    checkpoints = [e for e in map(json.loads, archive.read_text().splitlines()) if e["type"] == "compaction"]
    assert checkpoints[-1]["summary"] == summary
    assert checkpoints[-1]["details"] == {"readFiles": ["a.rs"], "modifiedFiles": ["b.rs", "c.rs"]}
    if os.environ.get("PANDA_TEST_PI"):
        replies = pi_session(archive, ["get_messages"])
        pi_context = json.dumps(replies["get_messages"]["data"]["messages"])
        assert all(pi_context.count(f) == 1 for f in ("a.rs", "b.rs", "c.rs"))
        assert "d.rs" not in pi_context and "missing.rs" not in pi_context
    imported = run(["--import-session", str(archive)], "")
    restored = next(json.loads(line)["session_id"] for line in imported if line.startswith("{"))
    check_files(restored)
    run(["--continue"], ":compact\n:q\n")
    requests.clear()
    check_files(restored)

    # The system-prompt wrapper must forward branch state on every tool round.
    # Use a computed value so its appearance on the wire is distinguishable from the script.
    flags = ["--codemode", "--system-prompt", "Check stored values.", *model]
    scripts = [
        'store("__proto__", {value:["private","cursor"].join("-")}); store("optional", null); text("saved")',
        'const x = load("__proto__"); x.value = "changed"; text([load("__proto__").value.length, load("optional") === null]); store("optional", undefined)',
        'text(load("optional") === undefined)',
    ]
    next_calls.extend(("codemode", {"code": script}) for script in scripts)
    sid = done(run(["--mode", "json", *flags, "Remember the cursor"], ""))["session_id"]
    results = [m["content"] for m in requests[-1][1]["messages"] if m["role"] == "tool"]
    assert results == ["saved", "[14,true]", "true"], results
    assert "private-cursor" not in json.dumps(requests)
    requests.clear()
    archive = root / "codemode-store.jsonl"
    run(["--export-session", str(sid), str(archive)], "")
    if os.environ.get("PANDA_TEST_PI"):
        replies = pi_session(archive, ["get_entries", "get_messages"])
        deltas = [e["data"] for e in replies["get_entries"]["data"]["entries"] if e.get("customType") == "codemode-store"]
        assert deltas == [{"set":{"__proto__":{"value":"private-cursor"},"optional":None},"delete":[]},
                          {"set":{},"delete":["optional"]}], deltas
        assert "private-cursor" not in json.dumps(replies["get_messages"])
    imported = run(["--import-session", str(archive)], "")
    restored = next(json.loads(line)["session_id"] for line in imported if line.startswith("{"))
    run(["--continue"], ":compact\n:q\n")
    assert "private-cursor" not in json.dumps(requests)
    requests.clear()
    next_calls.append(("codemode", {"code": 'text([load("__proto__").value, load("optional") === undefined])'}))
    run(["--print", "--session", str(restored), *flags, "Inspect the restored value"], "")
    assert requests[-1][1]["messages"][-1]["content"] == '["private-cursor",true]'
