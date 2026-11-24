"""Drive persisted trust, package activation and both MCP transports through the real CLI."""
import json
import os
import signal
import subprocess
import sys
import tarfile
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.dont_write_bytecode = True
import mcp_auth

binary = str(Path(sys.argv[1]).resolve())
chat_calls = []
wire = []
mcp_messages = []
waiting = threading.Event()
cancelled = threading.Event()
drop_read = threading.Event()


class Server(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, data, content="application/json", status=200, session=False):
        self.send_response(status)
        self.send_header("Content-Type", content)
        self.send_header("Content-Length", str(len(data)))
        if session:
            self.send_header("Mcp-Session-Id", "fixture-session")
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_DELETE(self):
        assert self.headers["Mcp-Session-Id"] == "fixture-session"
        mcp_messages.append(("DELETE", {}))
        self.reply(b"", status=204)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/model/chat/completions":
            wire.append(body)
            if chat_calls:
                name, args = chat_calls.pop(0)
                delta = {"tool_calls": [{"index": 0, "id": "fixture", "type": "function",
                          "function": {"name": name, "arguments": json.dumps(args)}}]}
                reason = "tool_calls"
            else:
                delta, reason = {"content": "done"}, "stop"
            chunk = {"choices": [{"delta": delta, "finish_reason": reason}]}
            self.reply(("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode(), "text/event-stream")
            return
        assert self.path == "/mcp"
        method = body["method"]
        mcp_messages.append((method, body))
        if method == "initialize":
            assert body["params"]["protocolVersion"] == "2025-06-18"
            result = {"protocolVersion":"2025-06-18","capabilities":{"resources":{}},
                      "serverInfo":{"name":"fixture","version":"1"}}
            if getattr(self.server, "mutation_fixture", False):
                result["capabilities"]["tools"] = {}
            self.reply(json.dumps({"jsonrpc":"2.0","id":body["id"],"result":result}).encode(), session=True)
            return
        assert self.headers["Mcp-Session-Id"] == "fixture-session"
        assert self.headers["MCP-Protocol-Version"] == "2025-06-18"
        assert "text/event-stream" in self.headers["Accept"]
        if method.startswith("notifications/"):
            if method == "notifications/cancelled":
                cancelled.set()
            self.reply(b"", status=202)
            return
        if method == "tools/list":
            payload = {"result":{"tools":[{"name":"mutate","inputSchema":{"type":"object"}}]}}
        elif method == "tools/call":
            self.connection.close()
            return
        elif method == "resources/list":
            payload = {"result":{"resources":[{"uri":"fixture://one","name":"one"}]}}
        elif method == "resources/templates/list":
            payload = {"result":{"resourceTemplates":[{"uriTemplate":"fixture://{key}","name":"key"}]}}
        elif method == "resources/read":
            uri=body["params"]["uri"]
            if uri.endswith("lost") or (uri.endswith("reconnect") and drop_read.is_set()):
                drop_read.clear()
                self.connection.close()
                return
            if uri.endswith("slow"):
                waiting.set()
                assert cancelled.wait(8), "MCP client did not notify server of cancellation"
            if uri.endswith("absent"):
                payload={"error":{"code":-32002,"message": self.headers.get("Authorization") or "missing fixture resource"}}
            else:
                payload={"result":{"contents":[{"uri":uri,"mimeType":"text/plain","text":"evidence"}]}}
        else:
            raise AssertionError(method)
        payload.update(jsonrpc="2.0", id=body["id"])
        self.reply((": keep-alive\r\n\r\nevent: message\r\ndata: " + json.dumps(payload) + "\r\n\r\n").encode(), "text/event-stream")


def exercise(root, http):
    port = http.server_port
    repo, state = root / "repo", root / "state"
    repo.mkdir()
    state.mkdir(mode=0o700)
    (repo / ".git").mkdir()
    (repo / ".panda").mkdir()
    nested = repo / "sub"
    nested.mkdir()
    link = root / "alias"
    link.symlink_to(repo, target_is_directory=True)
    env = dict(os.environ, PANDA_HOME=str(state), HOME=str(root), npm_config_offline="true")
    base = f"http://127.0.0.1:{port}"
    models = {"providers":{"fixture":{"baseUrl":base+"/model","models":[
            {"id":"demo","contextWindow":64000,"maxTokens":2048}]}}}
    (state/"models.json").write_text(json.dumps(models))
    (state/"model.json").write_text('["fixture","demo"]')

    def run(*args, stdin="", cwd=nested, error=None):
        res=subprocess.run([binary,*map(str,args)],cwd=cwd,env=env,input=stdin,text=True,
                           capture_output=True,timeout=30)
        if error:
            assert res.returncode and error in res.stderr,(args,res.stdout,res.stderr)
        else:
            assert res.returncode==0,(args,res.stdout,res.stderr)
        return res.stdout

    # The stdio sentinel is part of the workspace, and runs in Panda's read-only sandbox.
    (repo/"fixture.pl").write_text(r'''
use strict;
use JSON::PP;
$|=1;
open(my $marker, ">", "$ENV{HOME}/server-marker") or die "scratch unavailable";
close $marker;
while (<STDIN>) {
    my $q=decode_json($_);
    next unless exists $q->{id};
    my $method=$q->{method};
    my $r=$method eq "initialize" ? {protocolVersion=>"2025-06-18",capabilities=>{tools=>{}},serverInfo=>{name=>"sentinel",version=>"1"}} :
        $method eq "tools/list" ? {tools=>[{name=>"double",inputSchema=>{type=>"object",properties=>{n=>{type=>"integer"}},required=>["n"]}}]} :
        {content=>[{type=>"text",text=>"sentinel"}],structuredContent=>{value=>2*$q->{params}{arguments}{n},pid=>$$,home=>$ENV{HOME},tmp=>$ENV{TMPDIR}}};
    print encode_json({jsonrpc=>"2.0",id=>$q->{id},result=>$r}),"\n";
}
''')
    stdio = {"command":"/usr/bin/perl","args":["fixture.pl"],"readOnlyTools":["double"]}
    project_config = repo/".panda/mcp.json"
    project_config.write_text(json.dumps({"mcpServers":{"stdio":stdio,"peer":stdio}}))
    http_config = json.dumps({"mcpServers":{"http":{"url":base+"/mcp"}}})

    def schemas():
        run("--print","--no-session","inventory")
        return {t["function"]["name"] for t in wire[-1]["tools"]}

    assert "undecided" in run("--trust")
    assert "mcp_stdio_double" not in schemas() and not mcp_messages
    run("--trust","deny",cwd=link)
    assert "denied" in run("--trust") and "mcp_stdio_double" not in schemas() and not mcp_messages
    run("--trust","allow",cwd=link)
    assert "allowed" in run("--trust")
    assert {"mcp_stdio_double","mcp_peer_double"} <= schemas()
    # Simulate a new config revision in an already trusted checkout.
    project_config.write_text(http_config)
    run("--print","--no-session","inventory",error="project MCP configuration supports only local stdio")
    assert not mcp_messages
    for name in ("HOME", "TMPDIR"):
        project_config.write_text(json.dumps({"mcpServers":{"stdio":dict(stdio, env={name:str(repo)})}}))
        run("--print","--no-session","inventory",error="Panda sets HOME and TMPDIR")
    project_config.write_text(json.dumps({"mcpServers":{"stdio":stdio,"peer":stdio}}))
    personal_config = state/"mcp.json"
    personal_config.write_text(http_config)
    assert {"mcp_stdio_double","mcp_http_resources_list","mcp_http_resources_templates","mcp_http_resources_read"} <= schemas()
    assert mcp_messages[-1][0]=="DELETE"

    def call(name,args,allowed, extra=()):
        chat_calls.append((name,args))
        run("--print","--no-session",*extra,"--tools",allowed,"invoke fixture")
        message=wire[-1]["messages"][-1]
        assert message["role"]=="tool",message
        return message["content"]

    drop_read.set()
    mcp_messages.clear()
    recovered = call("mcp_http_resources_read", {"uri":"fixture://reconnect"}, "mcp_http_resources_read")
    assert "evidence" in recovered, recovered
    assert [m for m, _ in mcp_messages].count("initialize") == 2
    assert [m for m, _ in mcp_messages].count("resources/read") == 2

    mcp_messages.clear()
    read = "mcp_http_resources_read"
    lost = call(read, {"uri":"fixture://lost"}, read)
    assert "tool error" in lost
    assert [m for m, _ in mcp_messages].count("initialize") == 2
    assert [m for m, _ in mcp_messages].count("resources/read") == 2

    http.mutation_fixture = True
    mcp_messages.clear()
    mutation = "mcp_http_mutate"
    assert "tool error" in call(mutation, {}, mutation)
    assert [m for m, _ in mcp_messages].count("initialize") == 1
    assert [m for m, _ in mcp_messages].count("tools/call") == 1
    http.mutation_fixture = False

    chat_calls.extend([("mcp_stdio_double",{"n":1}),("mcp_peer_double",{"n":1}),
                       ("shell",{"command":'test ! -e "$HOME/server-marker" && echo "$HOME" && echo "$TMPDIR"'})])
    tmpdir = env.get("TMPDIR")
    env["TMPDIR"] = str(repo)
    run("--print","--no-session","--tools","mcp_stdio_double,mcp_peer_double,shell","compare scratch directories")
    if tmpdir is None:
        del env["TMPDIR"]
    else:
        env["TMPDIR"] = tmpdir
    results = [m["content"] for m in wire[-1]["messages"] if m["role"] == "tool"]
    first, peer = [json.loads(content)["structuredContent"] for content in results[:2]]
    shell_home, shell_tmp = results[2].splitlines()
    assert first["home"] == first["tmp"] and peer["home"] == peer["tmp"] and shell_home == shell_tmp
    homes = {first["home"], peer["home"], shell_home}
    assert len(homes) == 3
    assert all(not Path(home).is_relative_to(repo) and not Path(home).exists() for home in homes)

    result=json.loads(call("mcp_stdio_double",{"n":21},"mcp_stdio_double"))
    assert result["structuredContent"]["value"]==42 and result["content"][0]["text"]=="sentinel"
    try:
        os.kill(result["structuredContent"]["pid"],0)
    except ProcessLookupError:
        pass
    else:
        raise AssertionError("stdio child survived session shutdown")
    script="text(tools.mcp_stdio_double({n:4}).structuredContent.value)"
    assert call("codemode",{"code":script},"codemode,mcp_stdio_double")=="8"
    assert "unavailable" in call("mcp_stdio_double",{"n":1},"read")
    denied=call("codemode",{"code":script},"codemode,read")
    assert "error" in denied.lower() or "not a function" in denied,denied
    for name, key in [("resources_list","resources"),("resources_templates","resourceTemplates")]:
        assert key in json.loads(call("mcp_http_"+name,{},"mcp_http_"+name))
    read="mcp_http_resources_read"
    assert json.loads(call(read,{"uri":"fixture://one"},read))["contents"][0]["text"]=="evidence"
    assert "missing fixture resource" in call(read,{"uri":"fixture://absent"},read)
    chat_calls.append((read,{"uri":"fixture://slow"}))
    with subprocess.Popen([binary,"--print","--no-session","--tools",read,"wait"],
                          cwd=repo,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as child:
        assert waiting.wait(10)
        child.send_signal(signal.SIGINT)
        out,err=child.communicate(timeout=8)
        assert child.returncode and b"cancel" in err.lower() and cancelled.is_set(),(out,err)
    assert mcp_messages[-1][0]=="DELETE"
    mcp_auth.exercise(binary, repo, state, env, http, run, call, schemas, drop_read, mcp_messages, chat_calls, wire)
    personal_config.unlink()
    run("--trust","revoke")
    count=len(mcp_messages)
    assert "mcp_stdio_double" not in schemas() and len(mcp_messages)==count
    run("--trust","allow")
    old=root/"old-repo"
    repo.rename(old)
    repo.mkdir()
    (repo/".git").mkdir()
    assert "undecided" in run("--trust",cwd=repo)
    (repo/".git").rmdir()
    repo.rmdir()
    old.rename(repo)
    assert "allowed" in run("--trust")
    run("--trust","deny")

    # Exercise the same prevent-execution guarantee for local, npm and Git Markdown packages.
    package=root/"bundle"
    (package/"pieces/note").mkdir(parents=True)
    (package/"pieces/note/SKILL.md").write_text("---\nname: note\ndescription: Package skill\n---\nBody of skill")
    prompt=package/"pieces/review.md"
    prompt.write_text("Package review $1")
    marker=root/"unwanted-start"
    manifest={"name":"roadmap-fixture","version":"1.0.0",
              "scripts":{"postinstall":f"touch {marker}"},
              "pi":{"extensions":["extension.js"],"prompts":["pieces/*.md"],"skills":["pieces/note"]}}
    (package/"package.json").write_text(json.dumps(manifest))
    (package/"extension.js").write_text("throw Error('unexpected activation')")
    op=lambda *args:run("--package",*args)
    op("install",package,"--project","--only","prompts")
    assert "project roadmap-fixture" in op("list")
    assert "/review" not in run(stdin=":prompts\n:q\n")
    run("--trust","allow")
    run("--print","--no-session","/review A")
    assert wire[-1]["messages"][-1]["content"]=="Package review A"
    assert "No matching skills" in run(stdin=":skills note\n:q\n")
    op("activate","roadmap-fixture","--project","--only","skills/note")
    assert "Package skill" in run(stdin=":skills note\n:q\n")
    assert "/review" not in run(stdin=":prompts\n:q\n")
    prompt.write_text("Updated $1")
    op("update","roadmap-fixture","--project","--only","prompts")
    run("--print","--no-session","/review B")
    assert wire[-1]["messages"][-1]["content"]=="Updated B"
    op("remove","roadmap-fixture","--project")
    assert not op("list").strip() and "/review" not in run(stdin=":prompts\n:q\n")
    subprocess.run(["git","init","-q",package],check=True)
    subprocess.run(["git","-C",package,"add","."],check=True)
    subprocess.run(["git","-C",package,"-c","user.name=Fixture","-c","user.email=fixture@example.test",
                    "-c","commit.gpgsign=false","commit","-qm","fixture"],check=True)
    archive=root/"fixture.tgz"
    with tarfile.open(archive,"w:gz") as tar:
        for p in package.iterdir():
            if p.name!=".git":
                tar.add(p,arcname="package/"+p.name)
    for source in ("git:"+str(package),"npm:"+str(archive)):
        op("install",source,"--only","prompts/review")
        assert "user roadmap-fixture" in op("list")
        assert "/review" in run(stdin=":prompts\n:q\n")
        op("remove","roadmap-fixture")
    assert not marker.exists()


class TestServer(mcp_auth.OAuthFixture, Server):
    pass


with tempfile.TemporaryDirectory() as directory, ThreadingHTTPServer(("127.0.0.1",0),TestServer) as http:
    thread=threading.Thread(target=http.serve_forever,daemon=True)
    thread.start()
    try:
        exercise(Path(directory).resolve(),http)
    finally:
        http.shutdown()
