"""Check structured edits and ordered images across worker, store and provider boundaries."""
import base64
import json
from pathlib import Path


def check(repo, run, next_calls, requests, saved_session):
    def invoke(name, args):
        requests.clear()
        next_calls.append((name, args))
        output = run(["--mode", "json", "--codemode", "Exercise tool"], "", cwd=repo)
        events = [json.loads(line) for line in output if line.startswith("{")]
        session = saved_session(events[-1]["session_id"])
        result, = [message["Result"] for message in session["history"] if "Result" in message]
        (_, initial), (_, followup) = requests
        return result, followup["messages"][len(initial["messages"]):], events

    original = "header\nα old\ncontext\nlast old\n"
    path = repo / "patch"
    path.write_text(original)
    args = {"path":"patch","edits":[{"old":"last old","new":"last new"},{"old":"α old","new":"β new\ninserted"}]}
    result, messages, events = invoke("edit", args)
    details = result["details"]
    assert details["firstChangedLine"] == 2 and details["type"] == "edit"
    assert messages[-1]["content"] == result["text"] and "− 2  α old" in result["text"]
    replay = original.encode()
    a,b = details["patch"]
    assert a["endByte"] <= b["startByte"]
    for change in reversed(details["patch"]):
        replay = replay[:change["startByte"]] + change["replacement"].encode() + replay[change["endByte"]:]
    assert replay == path.read_bytes()
    assert any(e.get("event",{}).get("data",{}) == {"id":result["id"],"details":details} for e in events)
    result, _, _ = invoke("edit", {"path":"patch","edits":[{"old":"β new","new":"oops"},{"old":"absent","new":"oops"}]})
    assert result["isError"] and "details" not in result and replay == path.read_bytes()

    pixels = Path(__file__).parent / "images"
    types = [(mime, (pixels / filename).read_bytes()) for mime, filename in
             [("png","pixel.png"),("jpeg","pixel.jpg"),("webp","pixel.webp")]]
    types.append(("gif",base64.b64decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")))
    for i,(mime,selected) in enumerate(types):
        unused = types[(i+1) % len(types)][1]
        (repo/"unused").write_bytes(unused)
        (repo/"selected").write_bytes(selected)
        result, messages, _ = invoke("codemode", {"code": """
            tools.read({path:"unused"});
            const block = tools.read({path:"selected"});
            if (block.type !== "image" || !block.data || !block.note) throw Error("missing image block");
            text("before"); text("next"); image(block); text("after");
        """})
        assert not result["isError"], result
        details = result["details"]
        assert details["success"] and details["elapsedMs"] >= 0
        assert [p["type"] for p in details["items"]] == ["text","text","image","text"]
        label = details["images"][0]["label"]
        assert label in messages[-2]["content"] and (repo/label).read_bytes() == selected
        parts = messages[-1]["content"]
        assert [p["type"] for p in parts] == ["text","text","image_url","text"]
        assert [p["text"] for p in parts if p["type"]=="text"] == ["before","next","after"]
        encoded = base64.b64encode(selected).decode()
        assert parts[2]["image_url"]["url"] == "data:image/"+mime+";base64,"+encoded
        assert json.dumps(messages).count(encoded)==1
        assert base64.b64encode(unused).decode() not in json.dumps(messages)
        assert result["images"] == [parts[2]["image_url"]["url"]]
        saved_label=label
        display="\n".join(run(["--continue"], ":q\n", cwd=repo))
        labels=["Codemode completed", "==> text 1/3 <==","before","==> text 2/3 <==","next","Image · "+label,"==> text 3/3 <==","after"]
        pos=display.index(labels[0])
        for label in labels[1:]:
            pos=display.index(label,pos+1)
        reread, _, _ = invoke("read", {"path":saved_label})
        assert reread["images"] == result["images"]
    result, _, _ = invoke("codemode", {"code": 'image("https://example.invalid/picture.png")'})
    assert result["isError"] and not result["details"]["success"] and "local image" in result["text"]
    requests.clear()
