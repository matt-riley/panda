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


def active_history(state):
    with sqlite3.connect(state / "sessions.sqlite3") as db:
        rows = db.execute("select id,data from sessions").fetchall()
    assert len(rows) == 1 and rows[0][0] == 1
    return json.loads(rows[0][1])["history"]


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
    initial_state = Path(tmp) / "initial"
    with panda(repo, initial_state, args=["/review startup"], startup="First answer") as (send, expect):
        assert [m.get("User") for m in active_history(initial_state) if "User" in m] == ["/review startup"]
        send("Follow up\r")
        expect("Second answer")
        expect("deepseek-flash")
    history = active_history(initial_state)
    assert [m.get("User") for m in history if "User" in m] == ["/review startup", "Follow up"]
    assert [m["Assistant"]["text"] for m in history if "Assistant" in m] == ["First answer", "Second answer"]
    requests = [json.loads(line) for line in (initial_state / "requests.jsonl").read_text().splitlines()]
    assert requests == [history[:1], history[:3]]

    def saved_sessions():
        with sqlite3.connect(initial_state / "sessions.sqlite3") as db:
            return {sid: json.loads(data) for sid, data in db.execute("SELECT id,data FROM sessions")}

    def logged_requests():
        return [json.loads(line) for line in (initial_state / "requests.jsonl").read_text().splitlines()]

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
