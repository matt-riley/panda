"""Acceptance checks using the normal CLI, real file worker and a loopback provider."""
import json
import sqlite3


def check(root, state, run, requests, next_calls):
    a, b = root / "workspace-a", root / "workspace-b"
    for directory, name in [(a, "alpha"), (b, "beta")]:
        directory.mkdir()
        (directory / "same").write_text(name)
        (directory / "AGENTS.md").write_text(name + " workspace instructions")
    events = run(["--mode", "json", "Seed workspace A"], "", cwd=a)
    session = str(next(json.loads(row)["session_id"] for row in events if '"type":"done"' in row))
    for flag, name in [("--resume-any", "alpha"), ("--fork-here", "beta")]:
        next_calls.append(("read", {"path": "same"}))
        events = run(["--mode", "json", flag, session, "Read the file"], "", cwd=b)
        messages = requests[-1][1]["messages"]
        assert name + " workspace instructions" in messages[0]["content"], messages
        assert messages[-1]["role"] == "tool" and name in messages[-1]["content"], messages
        result = str(next(json.loads(row)["session_id"] for row in events if '"type":"done"' in row))
        assert (result == session) == (flag == "--resume-any")
    with sqlite3.connect(state / "sessions.sqlite3") as conn:
        owner, parent = conn.execute("SELECT repo,json_extract(data,'$.parent') FROM sessions WHERE id=?", (result,)).fetchone()
    assert owner == str(b.resolve()) and str(parent) == session
    a.rename(root / "gone-a")
    run(["--resume-any", session, "--print", "read"], "", cwd=b,
        error="original workspace unavailable", expected_requests=len(requests))
