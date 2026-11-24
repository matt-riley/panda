"""OAuth issuer and protected MCP resource for the CLI lifecycle test."""
import base64
import hashlib
import fcntl
import os
import pty
import select
import struct
import termios
import time
import json
import selectors
import subprocess
from urllib import error, parse, request
from threading import Event


class OAuthFixture:
    code = "fixture-code"
    token = "private-fixture-access"

    @property
    def base(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def do_GET(self):
        if self.path == "/metadata":
            value = {"resource": self.base + "/private/mcp", "authorization_servers": [self.base]}
        elif self.path == "/.well-known/oauth-authorization-server":
            value = {"issuer": self.base, "authorization_endpoint": self.base + "/authorize",
                     "token_endpoint": self.base + "/token", "code_challenge_methods_supported": ["S256"]}
        else:
            return self.send_error(404)
        self.reply(json.dumps(value).encode())

    def authorized(self):
        return self.headers.get("Authorization") == "Bearer " + self.token

    def do_POST(self):
        if self.path == "/token":
            params = parse.parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
            challenge = hashlib.sha256(params["code_verifier"][0].encode()).digest()
            assert base64.urlsafe_b64encode(challenge).decode().rstrip("=") == self.server.challenge
            assert params["client_id"] == ["fixture-client"] and params["code"] == [self.code]
            assert params["grant_type"] == ["authorization_code"]
            assert params["resource"] == [self.base + "/private/mcp"]
            assert params["redirect_uri"] == [self.server.callback]
            self.reply(json.dumps({"access_token": self.token, "token_type": "Bearer"}).encode())
            return
        if self.path == "/private/mcp":
            if not self.authorized() or getattr(self.server, "reject_access", False):
                self.send_response(401)
                self.send_header("WWW-Authenticate", f'Bearer resource_metadata="{self.base}/metadata"')
                self.end_headers()
                return
            if hasattr(self.server, "authorized_request"):
                self.server.authorized_request.set()
            self.path = "/mcp"
        return super().do_POST()

    def do_DELETE(self):
        if self.path == "/private/mcp":
            assert self.authorized()
        return super().do_DELETE()


def terminal_login_prompt(binary, repo, env, base):
    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
        with subprocess.Popen([binary], cwd=repo, env=dict(env, TERM="xterm-256color"),
                              stdin=slave, stdout=slave, stderr=slave) as child:
            try:
                pending = bytearray()
                def expect(marker):
                    deadline = time.monotonic() + 8
                    while marker not in pending:
                        assert time.monotonic() < deadline, "terminal did not display " + marker.decode()
                        if select.select([master], [], [], 0.1)[0]:
                            chunk = os.read(master, 65536)
                            pending.extend(chunk)
                            if b"\x1b[6n" in chunk:
                                os.write(master, b"\x1b[1;1R")
                    pending.clear()
                expect(b"insert")
                os.write(master, b":mcp login protected\r")
                expect(("Open " + base + "/authorize?").encode())
            finally:
                child.kill()
    finally:
        os.close(master)
        os.close(slave)


def exercise(binary, repo, state, env, http, run, call, schemas, drop, messages, queue, wire):
    base = f"http://127.0.0.1:{http.server_port}"
    resource = base + "/private/mcp"
    config = {"url": resource, "oauth": {"clientId": "fixture-client"}}
    (state / "mcp.json").write_text(json.dumps({"mcpServers": {"protected": config}}))
    read = "mcp_protected_resources_read"
    with subprocess.Popen([binary, "--mcp", "login", "protected"], cwd=repo, env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as child:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                assert selector.select(10), "login did not offer an authorization URL"
                line = child.stdout.readline().strip()
            assert line.startswith("Open " + base + "/authorize?"), line
            query = parse.parse_qs(parse.urlsplit(line[5:]).query)
            assert query["client_id"] == ["fixture-client"] and query["resource"] == [resource]
            assert query["code_challenge_method"] == ["S256"] and query["response_type"] == ["code"]
            http.challenge, = query["code_challenge"]
            http.callback, = query["redirect_uri"]
            callback = http.callback + "?" + parse.urlencode({"state": query["state"][0], "code": OAuthFixture.code})
            try:
                request.urlopen(http.callback + "?state=wrong&code=declined", timeout=5)
            except error.HTTPError as rejected:
                assert rejected.code == 400
            else:
                raise AssertionError("login accepted a mismatched callback")
            with request.urlopen(callback, timeout=5) as response:
                assert response.status == 200
            out, _ = child.communicate(timeout=10)
            assert child.returncode == 0 and OAuthFixture.token not in out, out
        finally:
            if child.poll() is None:
                child.kill()
    assert read in schemas()
    http.reject_access = True
    try:
        assert read not in schemas()
        run("--mcp", "diagnostics", "protected", error="MCP login required")
    finally:
        http.reject_access = False
    drop.set()
    messages.clear()
    result = call(read, {"uri":"fixture://reconnect"}, read)
    assert json.loads(result)["contents"][0]["text"] == "evidence"
    assert [m for m, _ in messages].count("initialize") == 2
    assert [m for m, _ in messages].count("resources/read") == 2
    failed = call(read, {"uri":"fixture://absent"}, read)
    assert "error" in failed and OAuthFixture.token not in failed, "credential appeared in an MCP error"
    listing = run("--mcp", "list", cwd=repo.parent)
    assert "protected" in listing and "HTTP" in listing and "signed in" in listing, listing
    assert OAuthFixture.token not in listing
    messages.clear()
    diagnostic = run("--mcp", "reconnect", "protected")
    assert "connected" in diagnostic and OAuthFixture.token not in diagnostic
    assert [m for m, _ in messages] == ["initialize", "notifications/initialized", "DELETE"]

    diagnosis = run("--mcp", "diagnostics", "protected")
    assert "3 tools" in diagnosis and "signed in" in diagnosis and OAuthFixture.token not in diagnosis

    config.update(exposure="deferred", toolExposure={"resources_templates":"hidden", "resources_list":"direct"})
    (state / "mcp.json").write_text(json.dumps({"mcpServers":{"protected":config}}))
    names = schemas()
    assert "tool_search" in names and read not in names and "mcp_protected_resources_templates" not in names
    assert "mcp_protected_resources_list" in names
    messages.clear()
    queue.extend([(read, {"uri":"fixture://one"}), ("tool_search", {"query":read}), (read, {"uri":"fixture://one"})])
    run("--print", "--no-session", "discover a protected read")
    results = [m["content"] for m in wire[-1]["messages"] if m["role"] == "tool"]
    assert "unavailable" in results[0] and read in results[1] and "evidence" in results[2]
    assert [m for m, _ in messages].count("resources/read") == 1

    run("--mcp", "exposure", "protected", "resources_read", "hidden")
    assert read not in schemas()
    queue.extend([("tool_search", {"query":read}), (read, {"uri":"fixture://one"})])
    run("--print", "--no-session", "look up the hidden read")
    results = [m["content"] for m in wire[-1]["messages"] if m["role"] == "tool"]
    assert read not in results[0] and "unavailable" in results[1]
    queue.append((read, {"uri":"fixture://one"}))
    run(stdin=":mcp exposure protected resources_read direct\nuse the newly visible read\n:q\n")
    assert "evidence" in wire[-1]["messages"][-1]["content"]
    run("--mcp", "exposure", "protected", "resources_read", "reset")
    assert read not in schemas()
    run("--mcp", "exposure", "protected", "direct")
    assert read in schemas()
    run("--mcp", "exposure", "protected", "reset")

    # Keep another client's declarations alive while the terminal logs out.
    http.authorized_request = Event()
    with subprocess.Popen([binary, "--mode", "rpc"], cwd=repo, env=env, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as other:
        try:
            assert http.authorized_request.wait(10)
            output = run(stdin=":mcp status protected\n:mcp reconnect protected\n:mcp logout protected\n:mcp status protected\n:q\n")
            assert "signed in" in output and "connected" in output and "signed out" in output, output
            assert OAuthFixture.token not in output
            messages.clear()
            queue.append(("mcp_protected_resources_list", {}))
            other.communicate(json.dumps({"type":"prompt", "id":5, "text":"list resources"}) + "\n", timeout=15)
            assert other.returncode == 0
            assert "login required" in wire[-1]["messages"][-1]["content"]
            assert "resources/list" not in [m for m, _ in messages]
        finally:
            if other.poll() is None:
                other.kill()
    run("--mcp", "logout", "protected")
    messages.clear()
    assert read not in schemas() and not messages
    terminal_login_prompt(binary, repo, env, base)
