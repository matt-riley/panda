"""Exercise Panda's native terminal entry point with a disposable PTY."""
import fcntl
import json
import os
import pty
import select
import sqlite3
import subprocess
import signal
import struct
import sys
import tempfile
import termios
import time
from pathlib import Path

with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    repo = root / 'repo'
    repo.mkdir()
    env=dict(os.environ, TERM="xterm-256color", PANDA_HOME=str(root / "state"))
    subprocess.run([sys.argv[1]],cwd=repo,env=env,input=b"/quit\n",check=True,stdout=subprocess.DEVNULL)
    with sqlite3.connect(root / "state" / "sessions.sqlite3") as db:
        saved=json.loads(db.execute("select data from sessions where id=1").fetchone()[0])
        saved["name"]="Quartz branch"
        saved["history"]=[
            {"User":"\n".join(f"older message {i}" for i in range(30))},
            {"Assistant":{"text":"","calls":[{"id":"one","name":"edit","args":{"path":"style.css","old":"serif","new":"mono"}}]}},
            {"Result":{"id":"one","isError":False,"text":"saved\n  1  before\n− 2  font: serif;\n+ 2  font: mono;\n  3  after"}}
        ]
        saved["history"].insert(0, {"User": "The saffron fixture belongs to the typography test."})
        saved["history"].append({"User":"Summarize the change."})
        saved["history"].append({"Assistant":{"text":"## Summary\n\nUse **bold** and `inline`.","calls":[]}})
        db.execute("update sessions set data=? where id=1",(json.dumps(saved),))
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.environ.update(TERM='xterm-256color', PANDA_HOME=str(root / 'state'))
        os.environ.pop('NO_COLOR', None)
        os.execv(sys.argv[1], [sys.argv[1]])

    pending = bytearray()
    def expect(marker):
        end = time.monotonic() + 6
        while marker not in pending:
            if time.monotonic() > end:
                raise AssertionError((marker, bytes(pending[-1000:])))
            if select.select([fd], [], [], 0.1)[0]:
                chunk = os.read(fd, 65536)
                pending.extend(chunk)
                if b'\x1b[6n' in chunk:
                    os.write(fd, b'\x1b[1;1R')
        del pending[:pending.index(marker) + len(marker)]

    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 28, 90, 0, 0))
        expect(b'\x1b[?1049h')
        expect('─'.encode())
        expect(b'insert')
        expect(b'deepseek-flash')
        os.write(fd,b':resume\r')
        expect(b'Resume session')
        os.write(fd,b'quartz')
        expect(b'inline')
        os.write(fd,b'\x1b')
        expect(b'session 2')
        os.write(fd,b':resume\r')
        expect(b'Resume session')
        os.write(fd,b'saffron')
        expect(b'typography')
        os.write(fd,b'\r')
        expect('− 2  font: serif;'.encode())
        expect(b'\x1b[32;48;5;22m')
        expect(b'+ 2  font: mono;')
        expect(b'bold')
        assert b'** and `inline`' not in pending
        expect(b'deepseek-flash')
        os.write(fd,b'\x1b[5~')
        expect(b'older message 0')
        os.write(fd,b'\x1b[6~')
        expect(b'+ 2  font: mono;')
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 18, 60, 0, 0))
        expect(('─'*59).encode())
        os.write(fd, b'\x1b')
        expect(b'normal')
        os.write(fd, b'i')
        expect(b'insert')
        os.write(fd, b'/login\r')
        expect(b'providers')
        os.write(fd, b'openai')
        expect(b'OpenAI')
        os.write(fd, b'\r')
        expect(b'Sign')
        expect(b'device')
        os.write(fd, b'\x1b')
        expect(b'insert')
        (root / 'state/openrouter.key').write_text('fixture')
        (root / 'state/catalog-openrouter.json').write_text(json.dumps([['fixture/alpha',8192], ['fixture/beta',8192], ['openai/gpt-fixture',8192]]))
        os.write(fd, b'\x1b')
        expect(b'normal')
        os.write(fd, b':mo')
        expect(b':model  :models')
        os.write(fd, b'\t\r')
        expect(b'models')
        os.write(fd, b'fixture')
        os.write(fd, b'\x1b[B\r')
        expect(b'openrouter/fixture/beta')
        os.write(fd, b'i')
        expect(b'insert')
        (root / 'state/codex-efforts.json').write_text(json.dumps({'gpt-fixture':['low','ultra']}))
        os.write(fd, b':model openai gpt-fixture\r')
        expect(b'gpt-fixture')
        os.write(fd, b'/effort\r')
        expect(b'Reasoning')
        os.write(fd, b'ultra\r')
        expect(b'Thinking effort ultra')
        os.write(fd, b':model deepseek checked\r')
        expect(b'deepseek/checked')
        assert json.loads((root / 'state/model.json').read_text()) == ['deepseek', 'checked']
        os.write(fd, b'\x1b[200~first\nsecond\nthird\nfourth\nfifth\x1b[201~')
        expect(b'fifth')
        expect(b'deepseek/checked')
        os.write(fd, b'\x03')
        expect(b'insert')
        os.write(fd, b'draft\x1b')
        expect(b'normal')
        os.write(fd, b':q\r')
        expect(b'\x1b[?1049l')
    finally:
        os.close(fd)
        try:
            os.kill(pid, signal.SIGHUP)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
