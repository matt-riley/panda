"""Paired synthetic checkpoints through Panda's real terminal, SQLite, Pi JSONL and HTTP clients."""
import argparse
import copy
import fcntl
import pty
import select
import struct
import termios
import json
import os
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

RULES = ("Never publish the audit logs.", "Always preserve the original timestamps.")
ERROR = "E017: audit manifest mismatch"
SIBLING = "private-moon-sibling"
FACT = "Audit target is Linux."
CONTRARY = "Audit target is Windows."
UNKNOWN = "Audit owner is Morgan."
KEPT = "retained exact result: 猫 / 17"
GUIDANCE = "Keep the exact audit error."
NOTE = "Continue from the manifest review."
TIMESTAMP = "2026-01-02T00:00:00.000Z"


def archive(path, name, blocks):
    template = {r["id"]: r for r in map(json.loads, (Path(__file__).parent / "fixtures/pi-v3.jsonl").read_text().splitlines())}
    header = dict(template["pi-fixture"], id=f"fixture-{name}")
    rows = [header]
    parent = None

    def append(key, content, fields=None):
        nonlocal parent
        row = copy.deepcopy(template[key])
        row.update(id=f"m{len(rows)}", parentId=parent)
        message = row["message"]
        message["content"] = content
        if message["role"] == "assistant":
            message.update(provider="fixture", model="audit")
        message.update(fields or {})
        rows.append(row)
        parent = row["id"]

    def answer(text):
        append("a-answer", [{"type": "text", "text": text}])

    def read(i, filename, offset, content):
        ident = f"read-{i}"
        append("call", [{"type": "toolCall", "id": ident, "name": "read",
                         "arguments": {"path": filename, "offset": offset}}])
        append("result", [{"type": "text", "text": content}], {"toolCallId": ident})

    append("root", RULES[0] + "\n" + RULES[1])
    root = parent
    append("sibling", SIBLING)
    answer("sibling only")
    parent = root
    answer(FACT)
    paths = []
    for i in range(blocks):
        path_name = f"src/audit_{i}.rs"
        paths.append(path_name)
        lines = ["fn audit_item() { /* fixture 123456 */ }" for _ in range(max(12, 440 // blocks))]
        read(i, path_name, 17, "\n".join(lines))
    answer(ERROR)
    if name == "rejected":
        for i in range(6):
            answer("Rejection volume " + ("Q. " * 1100))
    append("keep", "Continue with the retained exchange.")
    read("tail", "kept.rs", 31, KEPT)
    answer("Retained exchange complete.")
    path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))
    return paths


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def reset(self, fixture):
        self.fixture = fixture
        self.log = []
        self.stall = False
        self.stall_after=0
        self.retry = fixture == "tool-heavy"
        self.overflow=False
        self.fail_summary=False
        self.arrived = threading.Event()
        self.release = threading.Event()


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        judge = self.path.endswith("/systemone")
        summary = not judge and body["messages"][0]["content"].startswith("Summarize")
        entry = {"kind": "judge" if judge else "summary" if summary else "turn", "body": body,
                 "retry": False, "cost": None}
        server = self.server
        server.log.append(entry)
        if server.stall and (judge or summary) and sum(r["kind"]!="turn" for r in server.log)>server.stall_after:
            server.arrived.set()
            server.release.wait(8)
            return
        if not judge and not summary and server.overflow:
            server.overflow=False
            self.send_response(400)
            self.send_header("Content-Type","application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"context_length_exceeded","message":"context window exceeded"}}')
            return
        if summary and server.fail_summary:
            self.send_error(401)
            return
        if judge and server.fixture == "unavailable":
            self.send_error(503)
            return
        if summary and server.retry:
            server.retry = False
            entry["retry"] = True
            self.send_response(503)
            self.send_header("Retry-After", "0")
            self.end_headers()
            return
        if judge:
            state = body["state"]
            segment = state.get("segment", {})
            text = segment.get("text", "") if isinstance(segment, dict) else ""
            answers = {}
            for key, spec in body["questions"].items():
                if spec["type"] == "choice":
                    disposition = "keep_verbatim" if text.startswith("Rejection volume") else "point" if segment.get("role") == "tool" else "drop"
                    answers[key] = {"type": "choice", "choice": disposition if key == "disposition" else next(iter(spec["criteria"])), "confidence": 1.0}
                else:
                    yes = key.startswith("s") or (key == "user_constraint" and RULES[0] in text) or (key == "decision" and text == FACT) or (key == "unresolved" and ERROR in text) or (key == "recoverable" and segment.get("role") == "tool")
                    answers[key] = {"type": "noul", "noul": 0.99 if yes else 0.01}
            entry["cost"] = 0.000001
            wire = json.dumps({"answers": answers, "usage": {"input_tokens": 18, "output_tokens": 2, "cost": entry["cost"]}})
            mime = "application/json"
        else:
            messages = body["messages"]
            if summary:
                # A fixed extractive stand-in for the provider. Calibration claims deliberately
                # exercise the independent contradiction and uncheckable counters.
                source = messages[-1]["content"]
                text = "\n".join(t for t in (RULES[0], ERROR, CONTRARY, UNKNOWN)
                                 if t in source or (t in (CONTRARY, UNKNOWN) and FACT in source))
                if NOTE in source:
                    text += "\nModel note: " + NOTE
                text = text.strip() or "Earlier work on the audit."
            else:
                text = "Loopback turn completed."
                if messages[-1].get("content")=="Second pass output.":
                    text="Synthetic second pass: "+("audit row 12345. "*(2000 if server.fixture=="cancelled" else 650))
            delta = {"content": text}
            last = messages[-1]
            if not summary and last["role"] == "user":
                requested = {"REQUEST SELF": ("self_compact", {"note": NOTE}), "REQUEST INFO": ("view_context", {})}.get(last["content"]) if isinstance(last["content"], str) else None
                if requested:
                    tool, args = requested
                    delta = {"tool_calls": [{"index": 0, "id": "control-call", "type": "function",
                                             "function": {"name": tool, "arguments": json.dumps(args)}}]}
            entry["cost"] = 0.00001
            packet = {"model": "audit", "usage": {"prompt_tokens": 50, "completion_tokens": 10, "cost": entry["cost"]},
                      "choices": [{"delta": delta, "finish_reason": "tool_calls" if "tool_calls" in delta else "stop"}]}
            wire = "data: " + json.dumps(packet) + "\n\ndata: [DONE]\n\n"
            mime = "text/event-stream"
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(wire.encode())))
        self.end_headers()
        try:
            self.wfile.write(wire.encode())
        except (BrokenPipeError, ConnectionResetError):
            pass


class Case:
    def __init__(self, binary, state, server, source, key):
        self.binary, self.state, self.server = binary, state, server
        state.mkdir(mode=0o700)
        self.repo=state.parent/"repo"
        self.repo.mkdir(exist_ok=True)
        port = server.server_port
        self.env = {k: v for k, v in os.environ.items() if k not in {"TYPESAFE_API_KEY","LORE_TYPESAFE_API_KEY","PANDA_NATIVE_COMPACTION"}}
        self.env.update(PANDA_HOME=str(state), HOME=str(state.parent), LOCAL_KEY="mock-chat", TYPESAFE_BASE_URL=f"http://127.0.0.1:{port}/v1")
        if key:
            self.env["TYPESAFE_API_KEY"] = "mock-jev"
        model = {"id": "audit", "name": "Audit fixture", "contextWindow": 65536, "maxTokens": 1024, "input": ["text"]}
        (state/"models.json").write_text(json.dumps({"providers":{"fixture":{"baseUrl":f"http://127.0.0.1:{port}/chat/v1","apiKey":"LOCAL_KEY","models":[model]}}}))
        (state/"model.json").write_text(json.dumps(["fixture","audit"]))
        self.sid = json.loads(self.run("--import-session", source))["session_id"]
        # Resolve model settings before recording revisions.
        self.terminal(":context info")

    def run(self, *args, stdin=""):
        proc = subprocess.run([self.binary, *map(str,args)], cwd=self.repo, env=self.env,
                              input=stdin, capture_output=True, text=True, timeout=30)
        assert proc.returncode == 0, (args, proc.stderr[-1500:])
        return proc.stdout + proc.stderr

    def terminal(self, *commands):
        return self.run("--continue", stdin=f":resume {self.sid}\n"+"\n".join(commands)+"\n:q\n")

    def ask(self, text, *options):
        return self.run("--print","--session",self.sid,*options,text)

    def snapshot(self):
        with sqlite3.connect(self.state/"sessions.sqlite3") as db:
            row = db.execute("SELECT data FROM sessions WHERE id=?",(self.sid,)).fetchone()
            n = db.execute("SELECT count(*) FROM sqlite_master WHERE name='recovery_transcripts'").fetchone()[0]
            count = db.execute("SELECT count(*) FROM recovery_transcripts WHERE session=?",(self.sid,)).fetchone()[0] if n else 0
        return json.loads(row[0]),count

    def cancel(self, after):
        server = self.server
        server.stall = True
        server.stall_after=after
        # Resume prints the entire synthetic transcript before reading the command.
        # A file also drains large transcripts while the HTTP request is being held.
        with tempfile.TemporaryFile(mode="w+") as output:
            proc = subprocess.Popen([self.binary,"--continue"],cwd=self.repo,env=self.env,stdin=subprocess.PIPE,stdout=output,stderr=subprocess.STDOUT,text=True)
            try:
                proc.stdin.write(f":resume {self.sid}\n:compact\n:q\n")
                proc.stdin.flush()
                assert server.arrived.wait(8), (after, [r["kind"] for r in server.log])
                proc.send_signal(signal.SIGINT)
                proc.communicate(timeout=4)
                output.seek(0)
                assert "cancel" in output.read().lower()
            finally:
                server.release.set()
                server.stall = False
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()



def compare(binary, base, server, name, native):
    server.reset(name)
    source = base/f"{name}.jsonl"
    paths = archive(source, name, 24 if name=="tool-heavy" else 3)
    case = Case(binary,base/(name+"-"+str(native)),server,source,native and name!="keyless")
    if name=="cancelled":
        case.terminal(":compact")
        case.ask("Second pass output.")
        case.ask("Keep the latest exchange after cancellation.")
    before,n0 = case.snapshot()
    server.log.clear()
    start = time.monotonic()
    if name == "cancelled":
        case.cancel(1 if native else 0)
    else:
        case.terminal(":compact "+GUIDANCE)
        if name == "repeated":
            case.ask("Second pass output.")
            case.ask("Continue the second exchange.")
            case.terminal(":compact")
    elapsed = round(1000*(time.monotonic()-start))
    data,n1 = case.snapshot()
    cp = data.get("checkpoint")
    committed = name!="cancelled"
    assert n1-n0==(2 if name=="repeated" else int(committed)), (name,native,data.get("native_schedule"),n0,n1)
    assert cp is not None
    if not committed:
        assert n0==1 and cp==before["checkpoint"] and data["history"]==before["history"]
    summary = cp["summary"] if committed else ""
    assert SIBLING not in summary
    native_path = native and name not in ("keyless","unavailable","rejected","cancelled")
    if native_path:
        assert all(rule in summary for rule in RULES) and ERROR in summary, (name,summary[:1000])
    if name in ("keyless","unavailable","rejected") and committed:
        assert "Provider summary" in data["native_schedule"]["last_outcome"]
    if name=="rejected" and native:
        assert "60% cutoff" in data["native_schedule"]["last_outcome"]
    if name=="keyless":
        assert all(r["kind"]!="judge" for r in server.log)
    if committed:
        assert cp["recovery"] and data["revision"]>before["revision"]
        assert set(paths)<=set(cp["files"]["readFiles"])
    records = [r for r in server.log if r["kind"]!="turn"]
    if not committed:
        assert records and records[-1]["cost"] is None
        if native:
            assert len(records)>=2 and records[0]["cost"] is not None
    metric = (name,"default" if native else "provider",sum(rule in summary for rule in RULES),
              int(CONTRARY in summary),int(UNKNOWN in summary),int(ERROR in summary),
              sum((p+", lines 17–") in summary for p in paths),len(paths),len(summary.encode()),
              len(records),sum(r["retry"] for r in records),elapsed,sum(r["cost"] for r in records if r["cost"] is not None) if any(r["cost"] is not None for r in records) else None)
    if name=="representative" and native:
        exported=base/"roundtrip.jsonl"
        case.run("--export-session",case.sid,exported)
        restored=json.loads(case.run("--import-session",exported))["session_id"]
        case.run("--print","--session",restored,"unique follow-up")
        history=server.log[-1]["body"]["messages"]
        assert sum(m.get("content")=="unique follow-up" for m in history)==1
        assert sum(m.get("content")==KEPT for m in history)==1
        assert sum(c["id"]=="read-tail" for m in history for c in m.get("tool_calls",[]))==1
        assert sum(isinstance(m.get("content"),str) and summary in m["content"] for m in history)==1
        assert SIBLING not in json.dumps(history)
    return metric


def terminal_preview(case):
    """Send Esc while a real judge request is in flight; prior settings and snapshots survive."""
    server=case.server
    previous=case.snapshot()
    server.log.clear()
    server.stall=True
    server.arrived.clear()
    server.release.clear()
    pid,fd=pty.fork()
    if pid==0:
        os.chdir(case.repo)
        env=dict(case.env,TERM="xterm-256color")
        os.execve(case.binary,[case.binary,"--continue"],env)
    data=bytearray()
    def wait(marker,timeout=8):
        deadline=time.monotonic()+timeout
        while marker not in data:
            assert time.monotonic()<deadline,(marker,bytes(data[-1200:]))
            if select.select([fd],[],[],0.1)[0]:
                chunk=os.read(fd,65536)
                data.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd,b"\x1b[1;1R")
        del data[:data.index(marker)+len(marker)]
    try:
        fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack("HHHH",55,230,0,0))
        wait(b"session")
        os.write(fd,b":compact info\r")
        wait(b"visibleTools")
        assert not server.log
        os.write(fd,b":compact preview\r")
        deadline=time.monotonic()+6
        while not server.arrived.is_set():
            assert time.monotonic()<deadline,"preview never reached the judge"
            if select.select([fd],[],[],0.05)[0]:
                chunk=os.read(fd,65536)
                if b"\x1b[6n" in chunk:
                    os.write(fd,b"\x1b[1;1R")
        os.write(fd,b"\x1b")
        wait(b"cancelled")
        os.write(fd,b":q\r")
        wait(b"\x1b[?1049l")
    finally:
        server.release.set()
        server.stall=False
        os.close(fd)
        if not os.waitpid(pid,os.WNOHANG)[0]:
            try: os.kill(pid,signal.SIGTERM)
            except ProcessLookupError: pass
            os.waitpid(pid,0)
    assert previous==case.snapshot()


def controls_and_recovery(binary,base,server):
    source=base/"controls.jsonl"
    archive(source,"controls",3)
    def fresh(label,key=True):
        server.reset(label)
        return Case(binary,base/label,server,source,key)
    case=fresh("controls")
    server.log.clear()
    before=case.snapshot()
    output=case.terminal(":compact info")
    info=json.loads(next(line.split("Context info: ",1)[1] for line in output.splitlines() if "Context info: " in line))
    assert info["softPercent"]==50 and info["hardPercent"]==70 and info["cache"]["state"] in ("cold","unknown")
    assert info["promptSections"] and not server.log
    assert "Compaction preview" in case.terminal(":compact preview "+GUIDANCE)
    assert before==case.snapshot() and any(r["kind"]=="judge" for r in server.log)
    terminal_preview(case)
    (case.state/"settings.json").write_text(json.dumps({"compaction":{"softAt":49,"hardAt":85,"cacheAt":84,"cacheForMinutes":5}}))
    server.log.clear()
    case.ask("REQUEST INFO","--tools","view_context","--compact-soft-at","42","--compact-at","99","--compact-cache-at","85","--compact-cache-for","0")
    result=json.loads(server.log[-1]["body"]["messages"][-1]["content"])
    assert (result["softPercent"],result["hardPercent"],result["cacheReadPercent"],result["cacheMinutes"])==(42,90,85,0)
    assert [row["name"] for row in result["visibleTools"]]==["view_context"]
    assert result["visibleToolCount"]==1 and result["visibleToolTokens"]==result["visibleTools"][0]["estimatedTokens"]
    assert result["lastUsage"] is not None and all(r["kind"]=="turn" for r in server.log)
    case.ask("REQUEST SELF","--no-tools")
    assert case.snapshot()[1]==0 and not server.log[-1]["body"].get("tools")
    case.ask("REQUEST SELF","--tools","self_compact")
    saved,count=case.snapshot()
    assert count==1 and NOTE in saved["checkpoint"]["summary"]
    assert not saved["native_schedule"]["agent_note"]
    case.ask("No additional checkpoint is pending.")
    assert case.snapshot()[1]==count
    # Old checkpoint survives a failed provider fallback.
    case.env.pop("TYPESAFE_API_KEY")
    case.ask("Second pass output.")
    case.ask("Keep this latest prompt.")
    server.fail_summary=True
    case.terminal(":compact")
    failed,n=case.snapshot()
    assert n==count and failed["checkpoint"]==saved["checkpoint"]
    for key in (True,False):
        label="hard-verified" if key else "hard-fallback"
        auto=fresh(label,key)
        auto.terminal(":context 4096")
        server.log.clear()
        auto.ask("Automatic pressure follow-up.")
        saved,count=auto.snapshot()
        assert count==1 and saved["context_budget"]==4096
        assert ("Verified handoff" if key else "Provider summary") in saved["native_schedule"]["last_outcome"]
        assert sum(r["kind"]=="summary" for r in server.log)==(0 if key else 1)
        overflow=fresh("overflow-"+str(key),key)
        server.overflow=True
        overflow.ask("Recover from the injected overflow.")
        saved,count=overflow.snapshot()
        assert count==1 and ("Verified handoff" if key else "Provider summary") in saved["native_schedule"]["last_outcome"]
        assert sum(r["kind"]=="turn" for r in server.log)==2
    for key in (True, False):
        rpc_case = fresh(f"pi-rpc-{key}", key)
        if not key:
            # Another application's credential cannot authorize external judgments.
            rpc_case.env["LORE_TYPESAFE_API_KEY"] = "other-application-fixture"
        def rpc_command(command, **fields):
            payload = dict(type=command, id="fixture", **fields)
            output = rpc_case.run("--mode", "rpc", "--rpc-version", "pi-v1", "--continue",
                                  stdin=json.dumps(payload)+"\n")
            response = next(json.loads(line) for line in output.splitlines()
                            if line.startswith("{") and json.loads(line).get("type")=="response")
            assert response["success"], response
            return response.get("data")
        rpc_case.terminal(":context 4096")
        rpc_command("set_auto_compaction", enabled=False)
        rpc_case.ask("Keep automatic checkpoints disabled.")
        assert rpc_case.snapshot()[1] == 0
        assert all(r["kind"]=="turn" for r in server.log)
        result = rpc_command("compact", customInstructions=GUIDANCE)
        state, count = rpc_case.snapshot()
        assert count == 1 and state["checkpoint"]["recovery"]
        assert result["summary"] == state["checkpoint"]["summary"]
        assert result["tokensBefore"] > 0 and result["estimatedTokensAfter"] > 0
        assert ("Verified handoff" if key else "Provider summary") in state["native_schedule"]["last_outcome"]
        if key:
            assert GUIDANCE in result["summary"] and result["firstKeptEntryId"]
        else:
            assert all(r["kind"] != "judge" for r in server.log)
    fallback=fresh("agent-fallback",False)
    fallback.ask("REQUEST SELF","--tools","self_compact")
    summaries=[r for r in server.log if r["kind"]=="summary"]
    assert len(summaries)==1
    assert NOTE in summaries[0]["body"]["messages"][-1]["content"]
    assert NOTE not in summaries[0]["body"]["messages"][0]["content"]
    assert fallback.snapshot()[1]==1


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report",type=Path)
    args=parser.parse_args()
    binary=str(args.binary.resolve())
    with Server(("127.0.0.1",0),Endpoint) as server, tempfile.TemporaryDirectory() as tmp:
        thread=threading.Thread(target=server.serve_forever,daemon=True)
        thread.start()
        base=Path(tmp).resolve()
        rows=[compare(binary,base,server,name,native) for name in
              ("representative","tool-heavy","repeated","keyless","unavailable","rejected","cancelled") for native in (False,True)]
        controls_and_recovery(binary,base,server)
        server.shutdown()
    intro="""# Synthetic compaction comparison

Both strategies in each pair use the same generated Pi branch. The default uses an HTTP Jev fixture. The provider baseline exercises Panda’s existing summarizer with Jev unconfigured. All requests go to loopback. The keyless pair has no Jev credential on either path. These observations describe the scripted services.

The fixtures port the upstream cases for real read ranges, repeated handoffs, oversized handoff rejection, unavailable judging and mid-run cancellation. The adjacent provider regressions also check masking before tool scoring truncation. [Pinned safe-compact tests](https://github.com/matt-riley/pi-extensions/blob/2b31ab39412dd30971019107d23e6ac4c9056744/packages/safe-compact/test/safe-compact.test.mjs) · [Rust scoring transport tests](../crates/providers/tests/jev.rs)\n\nThe independent oracle checks two standing instructions verbatim, one exact error, and each file path with its actual read range. It counts a contradictory operating-system claim and an unsupported ownership claim independently. The provider fixture deliberately emits both calibration claims when the source contains the checked fact. File lists alone do not count as recoverable line pointers. The cancellation fixture establishes a prior checkpoint, then cancels a second compaction. The native strategy completes one priced judgment before its next request is cancelled. The baseline cancels its single provider request. It verifies the original checkpoint and transcript survive. A cancelled checkpoint conveys no new claims. Bytes measure the stored UTF-8 summary. Requests include failed calls and exclude setup and ordinary turn calls. Retry counts identify scheduled repeat requests. Latency measures local elapsed time across the fixture’s compaction workflow. Cost uses synthetic USD reported by the loopback services, with no external charges. A dash means the client received no priced response.

| Fixture | Strategy | Instructions / 2 | Contradicted | Uncheckable | Exact error / 1 | Pointers | Bytes | Requests | Retries | Latency ms | Reported USD |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
"""
    text=intro+"".join(f"| {name} | {strategy} | {rules} | {contra} | {unknown} | {error} | {pointers}/{paths} | {size} | {requests} | {retries} | {latency} | {format(cost,'.6f') if cost is not None else '—'} |\n" for name,strategy,rules,contra,unknown,error,pointers,paths,size,requests,retries,latency,cost in rows)
    text+="\nReproduce with `cargo build --locked -p panda-cli --bin panda` followed by `python3 crates/cli/tests/compaction_comparison.py target/debug/panda`. Pass `--report docs/compaction-comparison.md` to refresh this report. Wall-clock measurements vary by host.\n"
    if args.report:
        args.report.write_text(text)
    else:
        print(text)


if __name__=="__main__":
    main()
