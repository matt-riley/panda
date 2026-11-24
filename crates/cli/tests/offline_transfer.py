"""Exercise an installed Panda CLI without a model, tool worker, or session scratch."""
import json
import os
import platform
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
fixture = Path(__file__).parent / "fixtures" / "pi-v3.jsonl"

with tempfile.TemporaryDirectory() as tmp:
    home = Path(tmp)
    repo = home / "project"
    repo.mkdir()
    env = dict(os.environ, HOME=str(home), PANDA_HOME=str(home / "state"),
               TMPDIR=str(home / "no-worker-scratch"))
    env.pop("PANDA_TOOLCHAIN_DIRS", None)

    def run(*args, check=True):
        return subprocess.run([binary, *args], cwd=repo, env=env, capture_output=True,
                              text=True, timeout=15, check=check)

    for override in ([], ["--session-dir", str(home / "session store")]):
        imported = run("--import-session", fixture, *override, check=False)
        assert imported.returncode == 0, imported.stderr
        sid = str(json.loads(imported.stdout)["session_id"])
        out = repo / ("custom.jsonl" if override else "default.jsonl")
        run("--export-session", sid, out, *override)
        records = [json.loads(line) for line in out.read_text().splitlines()]
        messages = [row["message"]["content"] for row in records if row.get("type") == "message"]
        assert "kept prompt" in messages and "sibling only" in messages
        original = out.read_bytes()
        assert run("--export-session", sid, out, *override, check=False).returncode != 0
        assert out.read_bytes() == original

    unsafe_store = repo / "sessions"
    unsafe_store.mkdir(mode=0o700)
    denied = run("--import-session", fixture, "--session-dir", unsafe_store, check=False)
    assert denied.returncode != 0 and "workspace overlaps" in denied.stderr
    assert not list(unsafe_store.iterdir())

if sys.platform == "linux":
    distro = platform.freedesktop_os_release()["PRETTY_NAME"]
    print(f"Installed CLI session transfer passed on {distro} ({platform.machine()})")
