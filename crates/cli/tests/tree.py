"""Drive the normal editor in two processes with an offline provider."""
import base64
import fcntl
import json
import os
import pty
import select
import signal
import sqlite3
import struct
import subprocess
import uuid
import sys
import tempfile
import termios
import time
from contextlib import contextmanager
from pathlib import Path


@contextmanager
def panda(repo, state, released=None, slow=False, startup=None, args=None, one_shot=False):
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.environ.update(TERM="xterm-256color", PANDA_HOME=str(state), PANDA_TREE_FIXTURE="1")
        if args is not None:
            os.environ["PANDA_TREE_ARGS"] = json.dumps(args)
            os.environ["PANDA_TEST_REQUEST_LOG"] = str(state / "requests.jsonl")
        if slow:
            os.environ["PANDA_SLOW_SUMMARY"] = "1"
        if released:
            os.execv(released, [released, "--continue"])
        os.execv(sys.argv[1], [sys.argv[1], "--exact", "tree_tests::fixture", "--nocapture"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 90, 0, 0))
    pending = bytearray()

    def expect(text):
        marker = text.encode()
        deadline = time.monotonic() + 8
        while marker not in pending:
            assert time.monotonic() < deadline, (text, bytes(pending[-1200:]))
            if select.select([fd], [], [], 0.1)[0]:
                chunk = os.read(fd, 65536)
                assert chunk, (text, pending)
                pending.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
        del pending[:pending.index(marker) + len(marker)]

    try:
        if one_shot:
            yield lambda s: os.write(fd, s.encode()), expect
            expect("test result: ok.")
            return
        if startup:
            expect(startup)
        expect("deepseek-flash")
        yield lambda s: os.write(fd, s.encode()), expect
        os.write(fd, b":q\r")
        expect("\x1b[?1049l")
    finally:
        os.close(fd)
        try:
            os.kill(pid, signal.SIGHUP)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)


def saved_session(state):
    with sqlite3.connect(state / "sessions.sqlite3") as db:
        rows = db.execute("select id,data from sessions").fetchall()
    assert len(rows) == 1 and rows[0][0] == 1
    return json.loads(rows[0][1])


def active_history(state):
    return saved_session(state)["history"]


def request_log(state):
    return [json.loads(line) for line in (state / "requests.jsonl").read_text().splitlines()]


def test_guided_branch_summary(repo, state):
    guidance = "Carry over the  unresolved test."
    summary = {"BranchSummary": "Checkpoint answer"}

    def open_destination(send, expect):
        send(":tree\r")
        expect("Session tree")
        send("/revised answerS")
        expect("Branch summary instructions")

    def stored_tree():
        with sqlite3.connect(state / "sessions.sqlite3") as db:
            head = db.execute("select tip from tree_heads where session=1").fetchone()
            nodes = db.execute("select id,parent,message from tree_nodes order by id").fetchall()
        return head, nodes

    with panda(repo, state, args=["Shared decision"], startup="First answer") as (send, expect):
        send("Follow up differently\r")
        expect("Revised answer")
        expect("deepseek-flash")
        branch_b = active_history(state)
        send(":tree\r")
        expect("Session tree")
        send("/follow up differently\r")
        expect("deepseek-flash")
        send(" on branch A\r")
        expect("Second answer")
        expect("deepseek-flash")
        branch_a = active_history(state)
        assert branch_a[:2] == branch_b[:2] and branch_a[2:] != branch_b[2:]
        initial_requests = request_log(state)

        open_destination(send, expect)
        original_tree = stored_tree()
        send("discard this\x1b")
        expect("Session tree")
        assert request_log(state) == initial_requests
        send("S")
        expect("Branch summary instructions")
        send("Fail this summary\r")
        expect("summary unavailable")
        expect("deepseek-flash")
        assert active_history(state) == branch_a and stored_tree() == original_tree

        open_destination(send, expect)
        send(guidance + "\r")
        expect("Checkpoint answer")
        expect("deepseek-flash")
        assert active_history(state) == branch_b + [summary]

    with panda(repo, state, args=["--continue"], startup="Checkpoint answer") as (send, expect):
        send("What survived?\r")
        expect("Second answer")
        expect("deepseek-flash")

    failed, guided, follow_up = request_log(state)[len(initial_requests):]
    assert failed["system"].count("Fail this summary") == 1
    assert guided["system"].count(guidance) == 1
    payload, = guided["history"]
    prefix = "Prior summary:\n\nTranscript data:\n"
    assert payload["User"].startswith(prefix)
    assert [json.loads(line) for line in payload["User"][len(prefix):].splitlines()] == branch_a[2:]
    assert follow_up["history"] == branch_b + [summary, {"User": "What survived?"}]
    assert guidance not in follow_up["system"]


def test_manual_compaction(repo, state):
    # A numeric first word also catches accidentally treating guidance as a new byte limit.
    instructions = "8192  is the ticket number. Preserve its decision."
    with panda(repo, state, args=["Discuss the ticket"], startup="First answer") as (send, expect):
        send(":context 16384\r")
        expect("byte limit 16384")
        expect("deepseek-flash")
        raw = active_history(state)
        send(":compact " + instructions + "\r")
        expect("2 messages")
        expect("deepseek-flash")
    saved = saved_session(state)
    assert saved["context_budget"] == 16384 and saved["history"] == raw
    assert saved["checkpoint"] == {"summary": "Checkpoint answer", "through": 2, "files": {"readFiles": [], "modifiedFiles": []}}
    first, guided = request_log(state)
    assert guided["system"].count(instructions) == 1
    assert instructions not in str(guided["history"]) and instructions not in str(first)

    with panda(repo, state, args=["--continue"]) as (send, expect):
        send("What was decided?\r")
        expect("Second answer")
        expect("deepseek-flash")
        before_failure = saved_session(state)
        send(":compact Fail this summary\r")
        expect("summary unavailable")
        expect("deepseek-flash")
        assert saved_session(state) == before_failure
        send(":compact\r")
        expect("4 messages")
        expect("deepseek-flash")

    _, _, follow_up, failed, bare = request_log(state)
    assert follow_up["history"] == [
        {"User": "Earlier conversation summary:\nCheckpoint answer"}, {"User": "What was decided?"}]
    assert instructions not in str(follow_up)
    assert failed["system"].count("Fail this summary") == 1
    assert bare["system"] in guided["system"] and bare["system"].startswith("Summarize")
    assert instructions not in str(bare) and "Fail this summary" not in str(bare)
    final = saved_session(state)
    assert final["history"] == before_failure["history"]
    assert final["context_budget"] == 16384 and final["checkpoint"]["through"] == 4


def read_copy():
    script = """ObjC.import('AppKit'); function run(args){
        const b=$.NSPasteboard.pasteboardWithName(args[0]);
        const d=b.dataForType('public.utf8-plain-text');
        return d.isNil() ? '' : ObjC.unwrap(d.base64EncodedStringWithOptions(0));
    }"""
    return base64.b64decode(subprocess.check_output(
        ["/usr/bin/osascript","-l","JavaScript","-e",script,os.environ["PANDA_TEST_PASTEBOARD"]]
    )).decode()


os.environ["PANDA_TEST_PASTEBOARD"] = "panda-copy-" + str(uuid.uuid4())
with tempfile.TemporaryDirectory() as tmp:
    repo, state = Path(tmp) / "repo", Path(tmp) / "state"
    repo.mkdir()
    test_guided_branch_summary(repo, Path(tmp) / "guided")
    test_manual_compaction(repo, Path(tmp) / "compact")
    # Two identical prompts have different ancestors. Mark the second, then open
    # the exported tree in a fresh store so local node IDs cannot be relied on.
    source = Path(tmp) / "branches.jsonl"
    edited = Path(tmp) / "bookmarks.jsonl"
    marked = Path(tmp) / "marked"
    imported = Path(tmp) / "imported"
    stamp = "2026-01-02T00:00:00.000Z"
    rows = [dict(type="session", version=3, id="branches", cwd=str(repo), timestamp=stamp)]
    for prefix in ("left", "right"):
        for suffix, parent, text in [("root", None, prefix), ("prompt", prefix+"root", "Duplicate prompt")]:
            rows.append(dict(type="message", id=prefix+suffix, parentId=parent, timestamp=stamp,
                             message=dict(role="user", content=text, timestamp=0)))
    rows.append(dict(type="custom", id="lefttip", parentId="leftprompt", timestamp=stamp,
                     customType="fixture", data={}))
    source.write_text("".join(json.dumps(r)+"\n" for r in rows))

    def transfer(store, args):
        with panda(repo, store, args=args, one_shot=True):
            pass

    transfer(marked, ["--import-session", str(source)])
    with panda(repo, marked, args=["--continue"]) as (send, expect):
        send(":tree\r")
        expect("Session tree")
        send("/Duplicate\x1b[BL")
        expect("Edit label")
        send("working\r")
        expect("[working]")
        send("L")
        expect("Edit label")
        send("\x15chosen\r")
        expect("[chosen]")
        send("\x0c")
        expect("Labeled")
        send("L")
        expect("Edit label")
        send("\x15discarded\x03")
        expect("[chosen]")
        send("\x1b")
        expect("deepseek-flash")
    assert not (marked / "requests.jsonl").exists()
    transfer(marked, ["--export-session", "1", str(edited)])
    records = [json.loads(line) for line in edited.read_text().splitlines()]
    assert [(e["targetId"], e["label"]) for e in records if e["type"] == "label"] == [("rightprompt", "chosen")]
    assert records[-1]["parentId"] == "lefttip"

    def verify_in_pi(path, expected):
        if not os.environ.get("PANDA_TEST_PI"):
            return
        cmd = [os.environ["PANDA_TEST_PI"], "--offline", "-ne", "-ns", "-np", "-nc", "--no-themes",
               "--mode", "rpc", "--session", str(path)]
        env = dict(PATH=os.environ["PATH"], HOME=tmp, PI_CODING_AGENT_DIR=str(Path(tmp) / "pi-agent"))
        proc = subprocess.run(cmd, cwd=repo, env=env, input='{"type":"get_tree"}\n',
                              capture_output=True, text=True, timeout=20, check=True)
        reply = next(r for r in map(json.loads, proc.stdout.splitlines()) if r["type"] == "response")
        assert reply["success"], reply
        nodes = list(reply["data"]["tree"])
        found = {}
        while nodes:
            node = nodes.pop()
            if node.get("label"):
                found[node["entry"]["id"]] = node["label"]
            nodes.extend(node["children"])
        assert found == expected, found

    verify_in_pi(edited, {"rightprompt": "chosen"})
    transfer(imported, ["--import-session", str(edited)])
    with panda(repo, imported, args=["--continue"]) as (send, expect):
        send(":tree\r")
        expect("Session tree")
        send("\x0c")
        expect("Labeled")
        send("\x06")
        expect("session 2 (Code)")
        expect("Duplicate prompt")
        assert not (imported / "requests.jsonl").exists()
        send(" revised\r")
        expect("Second answer")
        expect("deepseek-flash")
        send(":resume 1\r")
        expect("session 1 (Code)")
        expect("deepseek-flash")
        send(":tree\r")
        expect("Session tree")
        send("\x0cL")
        expect("Edit label")
        send("\x15\r")
        expect("matches")
        send("\x1b")
        expect("deepseek-flash")
    with panda(repo, imported, args=["--continue"]) as (send, expect):
        send(":tree\r")
        expect("Session tree")
        send("\x0c")
        expect("matches")
        send("\x1b")
        expect("deepseek-flash")
    cleared = Path(tmp) / "cleared.jsonl"
    transfer(imported, ["--export-session", "1", str(cleared)])
    clears = [r for r in map(json.loads, cleared.read_text().splitlines()) if r["type"] == "label"]
    assert clears[-1]["targetId"] == "rightprompt" and "label" not in clears[-1]
    verify_in_pi(cleared, {})
    assert [r["history"] for r in request_log(imported)] == [
        [{"User": "right"}, {"User": "Duplicate prompt revised"}]]

    initial_state = Path(tmp) / "initial"
    with panda(repo, initial_state, args=["/review startup"], startup="First answer") as (send, expect):
        assert [m.get("User") for m in active_history(initial_state) if "User" in m] == ["/review startup"]
        send("Follow up\r")
        expect("Second answer")
        expect("deepseek-flash")
    history = active_history(initial_state)
    assert [m.get("User") for m in history if "User" in m] == ["/review startup", "Follow up"]
    assert [m["Assistant"]["text"] for m in history if "Assistant" in m] == ["First answer", "Second answer"]
    requests = [r["history"] for r in request_log(initial_state)]
    assert requests == [history[:1], history[:3]]

    def saved_sessions():
        with sqlite3.connect(initial_state / "sessions.sqlite3") as db:
            return {sid: json.loads(data) for sid, data in db.execute("SELECT id,data FROM sessions")}

    def logged_requests():
        return [r["history"] for r in request_log(initial_state)]

    parent = saved_sessions()[1]
    with panda(repo, initial_state, args=["--continue"]) as (send, expect):
        send(":tree\r")
        expect("Session tree")
        send("/Follow up")
        expect("fork")
        send("\x06")
        expect("session 2 (Code)")
        expect("Follow up")
        before_submit = saved_sessions()
        assert before_submit[1] == parent
        assert before_submit[2]["parent"] == 1 and before_submit[2]["history"] == history[:2]
        assert logged_requests() == requests
        send(" differently\r")
        expect("Revised answer")
        expect("deepseek-flash")

    revised = history[:2] + [{"User": "Follow up differently"}]
    with panda(repo, initial_state, args=["--continue"], startup="session 2 (Code)") as (send, expect):
        send(":resume 1\r")
        expect("session 1 (Code)")
        expect("you> Follow up")
        expect("deepseek-flash")
        send(":resume 2\r")
        expect("session 2 (Code)")
        expect("you> Follow up differently")
        expect("deepseek-flash")
    both = saved_sessions()
    assert logged_requests() == requests + [revised]
    assert both[1] == parent
    assert both[2]["history"] == revised + [{"Assistant": {"text": "Revised answer", "calls": []}}]
    for flags in (["--print"], ["--mode", "json"], ["--no-session"]):
        with panda(repo, Path(tmp) / "once", args=flags + ["/review startup"], one_shot=True) as (_, expect):
            expect('"type":"done"' if flags == ["--mode", "json"] else "First answer\r\n")
    with panda(repo, state) as (send, expect):
        send(":copy\r")
        expect("No assistant reply")
        expect("deepseek-flash")
        send("Choose stone\r")
        expect("Granite")
        expect("deepseek-flash")
        send(":tree\r")
        expect("Session tree")
        send("Choose stone\r")
        expect("Choose stone")
        expect("deepseek-flash")
        send(" instead\r")
        expect("Quartz")
        expect("deepseek-flash")
    assert active_history(state)[0] == {"User": "Choose stone instead"}
    with panda(repo, state, sys.argv[2] if len(sys.argv) > 2 else None) as (send, expect):
        for answer in ("Granite", "Quartz"):
            send(":tree\r")
            expect("Session tree")
            send(answer + "\r")
            expect("session 1 (Code)")
            expect(answer)
            expect("deepseek-flash")
            assert active_history(state)[-1]["Assistant"]["text"] == answer + " **🐼**\n\n    sample\n"
            if len(sys.argv) == 2:
                send(":copy\r" if answer == "Granite" else "\x18")
                expect("Copied last reply")
                expect("deepseek-flash")
                assert read_copy() == answer + " **🐼**\n\n    sample\n"

    source = active_history(state)
    for slow in (True, False):
        with panda(repo, state, slow=slow) as (send, expect):
            send(":tree\r")
            expect("Session tree")
            send("Granite\x1b[B")
            send("s")
            expect("Summarizing")
            if slow:
                send("\x03")
                expect("cancelled")
                expect("deepseek-flash")
                assert active_history(state) == source
            else:
                expect("Quartz finding")
                expect("deepseek-flash")
    with panda(repo, state, sys.argv[2] if len(sys.argv) > 2 else None, startup="Quartz finding") as (send, expect):
        saved = active_history(state)
        assert saved[-1] == {"BranchSummary":"Quartz finding"}
        assert saved[-2]["Assistant"]["text"] == "Granite **🐼**\n\n    sample\n"

    with panda(repo, state) as (send, expect):
        send(":copy\r")
        expect("Copied last reply")
        assert read_copy() == "Granite **🐼**\n\n    sample\n"
        send("What survived?\r")
        expect("Context verified")
        expect("deepseek-flash")
