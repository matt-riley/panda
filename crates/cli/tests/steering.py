"""Send steering and a follow-up during an actual Seatbelt shell command."""
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time
from pathlib import Path

with tempfile.TemporaryDirectory() as tmp:
    repo = Path(tmp) / "repo"
    repo.mkdir()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.environ.update(TERM="xterm-256color", PANDA_HOME=tmp + "/state", PANDA_STEERING_FIXTURE="1")
        os.execv(sys.argv[1], [sys.argv[1], "--exact", "active::tests::fixture", "--nocapture"])
    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 90, 0, 0))
        deadline = time.monotonic() + 10
        output = bytearray()
        sent = False
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.05)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
                if not sent and b"RUNNINGFIXTURE" in output:
                    os.write(fd, b"wrong\x1b")
                    time.sleep(0.08)
                    os.write(fd, b"0cwcorrection\rnext\x1b\r")
                    sent = True
        if time.monotonic() >= deadline:
            os.kill(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
        assert sent and os.waitstatus_to_exitcode(status) == 0, bytes(output[-1600:])
    finally:
        os.close(fd)
