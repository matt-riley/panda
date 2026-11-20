"""Native provider contracts at the CLI boundary, using only loopback credentials."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BIN = str(Path(sys.argv[1]).resolve())
MODELS = {"a": "claude-fixture", "r": "responses-fixture", "g": "gemini-3-fixture"}
APIS = {"a": "anthropic-messages", "r": "openai-responses", "g": "google-generative-ai"}
REQUESTS = []
BLOCKED = threading.Event()
RELEASE = threading.Event()
ARGS = {"path": "note.txt"}
SIGN = {"a": "signed-anthropic", "r": "signed-openai", "g": "signed-gemini"}


def packets(family, tool):
    if family == "a":
        def packet(kind, **data):
            return {"type": kind, **data}
        yield packet("message_start", message={"model": MODELS[family], "usage": {
            "input_tokens": 10, "cache_read_input_tokens": 2, "output_tokens": 0}})
        yield packet("content_block_start", index=0, content_block={"type": "thinking", "thinking": ""})
        yield packet("content_block_delta", index=0, delta={"type": "thinking_delta", "thinking": "ponder"})
        yield packet("content_block_delta", index=0, delta={"type": "signature_delta", "signature": SIGN[family]})
        yield packet("content_block_stop", index=0)
        if tool:
            yield packet("content_block_start", index=1, content_block={"type": "tool_use", "id": "call-a", "name": "read", "input": {}})
            for bit in ['{"path":', '"note.txt"}']:
                yield packet("content_block_delta", index=1, delta={"type": "input_json_delta", "partial_json": bit})
        else:
            yield packet("content_block_start", index=1, content_block={"type": "text", "text": ""})
            yield packet("content_block_delta", index=1, delta={"type": "text_delta", "text": "finished"})
        yield packet("content_block_stop", index=1)
        yield packet("message_delta", delta={"stop_reason": "tool_use" if tool else "end_turn"}, usage={"output_tokens": 3})
        yield packet("message_stop")
    elif family == "r":
        yield {"type": "response.reasoning_summary_text.delta", "delta": "ponder"}
        yield {"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs",
               "encrypted_content": SIGN[family], "summary": []}}
        if tool:
            item = {"type": "function_call", "id": "fc-1", "call_id": "call-r", "name": "read", "arguments": ""}
            yield {"type": "response.output_item.added", "item": item, "output_index": 1}
            for bit in ['{"path":', '"note.txt"}']:
                yield {"type": "response.function_call_arguments.delta", "item_id": "fc-1", "delta": bit}
            yield {"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc-1", "status": "completed"}}
        else:
            yield {"type": "response.output_text.delta", "delta": "finished"}
        yield {"type": "response.completed", "response": {"model": MODELS[family], "status": "completed",
               "usage": {"input_tokens": 12, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 2}}}}
    else:
        def candidate(parts, **more):
            return {"modelVersion": MODELS[family],
                    "candidates": [{"content": {"role": "model", "parts": parts}, **more}]}
        yield candidate([{"text": "ponder", "thought": True, "thoughtSignature": "signed-thought"}])
        part = {"functionCall": {"name": "read", "args": ARGS}, "thoughtSignature": SIGN[family]} if tool else {"text": "finished"}
        finished = candidate([part], finishReason="STOP")
        finished["usageMetadata"] = {"promptTokenCount": 12, "candidatesTokenCount": 2, "thoughtsTokenCount": 1,
                                     "cachedContentTokenCount": 2}
        yield finished


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        family = self.path.split("/")[1]
        REQUESTS.append((family, self.path, dict(self.headers), body))
        text = json.dumps(body)
        if "AUTH_FAIL" in text:
            self.send_response(401)
            self.end_headers()
            return
        if "BLOCK_RESPONSE" in text:
            BLOCKED.set()
            RELEASE.wait(8)
            self.send_response(200)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        tool = "READ_FIXTURE" in text and "peacock-731" not in text
        for packet in packets(family, tool):
            line = ("data: " + json.dumps(packet) + "\r\n\r\n").encode()
            for start in range(0, len(line), 5):
                self.wfile.write(line[start:start + 5])


def exercise(server, root):
    repo = root / "workspace"
    repo.mkdir()
    (repo / "note.txt").write_text("peacock-731")
    state = root / "state"
    state.mkdir(mode=0o700)
    env = {k: v for k, v in os.environ.items()
           if k not in {"OPENAI_API_KEY", "GEMINI_API_KEY", "ANTHROPIC_API_KEY", "PANDA_NATIVE_COMPACTION"}}
    env.update(PANDA_HOME=str(state), HOME=str(root))
    providers = {}
    for key, model in MODELS.items():
        env[f"FIXTURE_{key.upper()}_KEY"] = f"only-{key}"
        providers[key] = {"api": APIS[key], "apiKey": f"FIXTURE_{key.upper()}_KEY",
                          "baseUrl": f"http://127.0.0.1:{server.server_port}/{key}/v1",
                          "models": [{"id": model, "contextWindow": 32000, "maxTokens": 4096, "input": ["text", "image"]}]}
    routes = {"auto": {"default": {"provider": "a", "model": MODELS["a"]},
                       "efforts": {"high": {"provider": "r", "model": MODELS["r"], "thinking": "low"}}}}
    (state / "models.json").write_text(json.dumps({"providers": providers, "virtualModels": routes}))

    def run(*args, stdin="", err=None):
        result = subprocess.run([BIN, *map(str, args)], input=stdin, text=True, cwd=repo, env=env,
                                capture_output=True, timeout=15)
        if err:
            assert result.returncode and err in result.stderr, (result.stdout, result.stderr)
        else:
            assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout

    def session(sid):
        with sqlite3.connect(state / "sessions.sqlite3") as db:
            return json.loads(db.execute("select data from sessions where id=?", (sid,)).fetchone()[0])

    for i, (f, next_f) in enumerate([("a", "r"), ("r", "g"), ("g", "a")], start=1):
        (state / "model.json").write_text(json.dumps([f, MODELS[f]]))
        REQUESTS.clear()
        assert run("--print", "--thinking", "low", "--tools", "read", "READ_FIXTURE").strip() == "finished"
        assert len(REQUESTS) == 2
        _, path, hdrs, first = REQUESTS[0]
        hdr = {k.lower(): v for k,v in hdrs.items()}
        assert path == {"a": "/a/v1/messages", "r": "/r/v1/responses",
                        "g": "/g/v1/models/gemini-3-fixture:streamGenerateContent?alt=sse"}[f], path
        expected = {"a": ("x-api-key", "only-a"), "r": ("authorization", "Bearer only-r"),
                    "g": ("x-goog-api-key", "only-g")}[f]
        assert hdr[expected[0]] == expected[1]
        assert len({"x-api-key", "x-goog-api-key", "authorization"} & hdr.keys()) == 1
        second = REQUESTS[1][3]
        assert SIGN[f] in json.dumps(second) and "peacock-731" in json.dumps(second)
        def verify_history(family, body):
            if family == "a":
                uses = [p for m in body["messages"] for p in m["content"] if p.get("type") == "tool_use"]
                results = [p for m in body["messages"] for p in m["content"] if p.get("type") == "tool_result"]
                assert uses[0]["input"] == ARGS and uses[0]["id"] == results[0]["tool_use_id"]
            elif family == "r":
                call = next(p for p in body["input"] if p.get("type") == "function_call")
                result = next(p for p in body["input"] if p.get("type") == "function_call_output")
                assert json.loads(call["arguments"]) == ARGS and result["call_id"] == call["call_id"]
            else:
                parts = [p for m in body["contents"] for p in m["parts"]]
                call = next(p["functionCall"] for p in parts if "functionCall" in p)
                result = next(p["functionResponse"] for p in parts if "functionResponse" in p)
                assert call["args"] == ARGS and call["id"] == result["id"] and result["name"] == "read"
        verify_history(f, second)
        if f == "a":
            assert hdr["anthropic-version"] == "2023-06-01" and first["thinking"]["budget_tokens"] == 1024
            assert first["tools"][0]["input_schema"]["type"] == "object"
        elif f == "r":
            assert first["reasoning"]["effort"] == "low" and first["tools"][0]["type"] == "function"
        else:
            assert "signed-thought" in json.dumps(second)
            assert first["generationConfig"]["thinkingConfig"]["thinkingLevel"] == "low"
            assert first["tools"][0]["functionDeclarations"][0]["name"] == "read"
        readings = session(i)["usage"]
        assert [(u["input"], u["output"], u["cache_read"]) for u in readings] == [(12, 3, 2)] * 2, readings
        run("--print", "--session", i, "--provider", next_f, "--model", MODELS[next_f], "continue in a different family")
        follow = REQUESTS[-1][3]
        assert SIGN[f] not in json.dumps(follow)
        verify_history(next_f, follow)
        run("--print", "--no-session", "--provider", f, "--model", MODELS[f], "AUTH_FAIL", err="401")

    records = run(stdin=":model router auto\nfirst answer\n:thinking high\nsecond answer\n:q\n")
    assert "Responder · a/claude-fixture" in records and "Responder · r/responses-fixture" in records, records
    assert [r[0] for r in REQUESTS[-2:]] == ["a", "r"]
    assert REQUESTS[-1][3]["reasoning"]["effort"] == "low"
    assert session(4)["model"] == {"provider": "router", "model": "auto"}
    path = root / "routed.jsonl"
    run("--export-session", 4, path)
    answers = [row["message"] for row in map(json.loads, path.read_text().splitlines())
               if row.get("message", {}).get("role") == "assistant"]
    assert [(m["provider"],m["model"]) for m in answers] == [("a", MODELS["a"]), ("r", MODELS["r"])]
    run("--print", "--session", 4, "resumed")
    assert REQUESTS[-1][0] == "r"

    for f in MODELS:
        (state / "model.json").write_text(json.dumps([f, MODELS[f]]))
        BLOCKED.clear()
        RELEASE.clear()
        proc = subprocess.Popen([BIN, "--mode", "rpc"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, env=env, cwd=repo)
        try:
            proc.stdin.write('{"type":"prompt","id":1,"text":"BLOCK_RESPONSE"}\n')
            proc.stdin.flush()
            assert BLOCKED.wait(5), f
            out, errors = proc.communicate('{"type":"cancel","id":2}\n', timeout=3)
            assert proc.returncode == 0, errors
            rows = list(map(json.loads, out.splitlines()))
            assert any(r["id"] == 2 and r["type"] == "ok" for r in rows), rows
            assert any(r["id"] == 1 and "cancelled" in r.get("error","") for r in rows), rows
        finally:
            RELEASE.set()
            if proc.poll() is None:
                proc.kill()
                proc.wait()


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), Endpoint) as server, tempfile.TemporaryDirectory() as tmp:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            exercise(server, Path(tmp))
        finally:
            RELEASE.set()
            server.shutdown()
