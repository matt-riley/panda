"""A line-oriented Pi v1 client speaking to the real CLI host."""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading


def check(binary, root, server, calls, requests):
    work = root / "pi-client-workspace"
    work.mkdir()
    state = root / "pi-client-state"
    state.mkdir(mode=0o700)
    (state / "model.json").write_text('["deepseek","one"]')
    (state / "catalog-deepseek.json").write_text('[["one",8000],["two",12000]]')
    args = ["--mode", "rpc", "--rpc-version", "pi-v1"]
    env = dict(os.environ, HOME=str(root), PANDA_HOME=str(state), DEEPSEEK_API_KEY="fixture",
               PANDA_ONESHOT_ARGS=json.dumps(args), PANDA_ONESHOT_SERVER=server)
    # Prevent ambient accounts from adding network lookups to this fixture.
    for key in ("OPENROUTER_API_KEY", "OPENAI_API_KEY"):
        env.pop(key, None)
    proc = subprocess.Popen([binary,"--exact","automation::tests::oneshot_fixture","--nocapture"],
                            env=env,cwd=work,stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
    records = queue.Queue()
    history = []

    def reader():
        for line in proc.stdout:
            if line.startswith("{"):
                records.put(json.loads(line))
        records.put(None)
    threading.Thread(target=reader,daemon=True).start()

    def until(match):
        while True:
            record = records.get(timeout=12)
            assert record is not None, (proc.poll(), history[-10:])
            history.append(record)
            if match(record):
                return record

    def send(kind, **args):
        ident = f"request-{len(history)}-{kind}"
        proc.stdin.write(json.dumps(dict(type=kind,id=ident,**args))+"\n")
        proc.stdin.flush()
        r = until(lambda e:e["type"]=="response" and e.get("id")==ident)
        assert r["success"] and r["command"]==kind,r
        return r.get("data")

    def settled():
        return until(lambda e:e["type"]=="agent_settled")

    try:
        start = send("get_state")
        assert not start["isStreaming"] and start["model"]["id"]=="one"
        assert send("set_model",provider="deepseek",modelId="two")["id"]=="two"
        send("set_steering_mode",mode="one-at-a-time")
        send("set_follow_up_mode",mode="all")
        assert send("get_state")["steeringMode"]=="one-at-a-time"
        # A shell marker gives a deterministic window for steering, inspection and abort.
        calls.append(("shell",{"command":"printf READY; sleep 5"}))
        assert send("prompt",message="hold") == {"disposition":"started"}
        until(lambda e:e["type"]=="tool_execution_update" and "READY" in str(e))
        assert send("get_state")["isStreaming"]
        assert send("steer",message="first") == {"disposition":"queued"}
        assert send("follow_up",message="second") == {"disposition":"queued"}
        assert send("clear_queue")=={"steering":["first"],"followUp":["second"]}
        send("abort")
        assert settled()["aborted"]
        assert sum(e["type"]=="agent_settled" for e in history)==1
        send("prompt",message="after abort")
        assert not settled()["aborted"]
        assert sum(e["type"]=="agent_settled" for e in history)==2
        assert requests[-1][1]["model"]=="two"
        updates = [r for r in history if r["type"]=="message_update"]
        assert updates and all("usage" in r for r in updates)
        assert any(r["assistantMessageEvent"]["type"]=="text_end" for r in updates)
        calls.append(("shell",{"command":"printf WAIT; sleep 5"}))
        send("prompt",message="hold again")
        until(lambda e:e["type"]=="tool_execution_update" and "WAIT" in str(e))
        send("follow_up",message="keep this follow-up")
        send("abort")
        assert not settled()["aborted"]
        assert sum(e["type"]=="agent_settled" for e in history)==3
        msgs = send("get_messages")["messages"]
        assert any(m["role"]=="assistant" and m["content"][0].get("text")=="reviewed" for m in msgs if m.get("content"))
        assert send("bash",command="printf test")["output"]=="test"
        failure = send("bash",command="printf oops; exit 7",excludeFromContext=True)
        assert failure["exitCode"]==7 and failure["output"]=="oops"
        assert send("compact",customInstructions="keep notes")["summary"]=="Saved old exchange"
        stats = send("get_session_stats")
        assert stats["toolCalls"] == 2 and stats["userMessages"] >= 4, stats
        tree = send("get_tree")
        entries = send("get_entries")
        assert tree["tree"] and entries["leafId"]==tree["leafId"]
        assert send("get_entries",since=entries["entries"][-1]["id"])["entries"]==[]
        fork = send("get_fork_messages")["messages"][0]
        assert send("fork",entryId=fork["entryId"])["text"]==fork["text"]
        assert send("get_state")["sessionId"]!=start["sessionId"]
        send("switch_session",sessionPath=start["sessionFile"])
        assert send("get_state")["sessionId"]==start["sessionId"]
        send("new_session",parentSession=start["sessionFile"])
        assert send("get_state")["model"]["id"]=="two"
        assert send("get_messages")["messages"]==[]
    finally:
        proc.stdin.close()
        if proc.wait(timeout=15)!=0:
            raise AssertionError(history[-12:])
