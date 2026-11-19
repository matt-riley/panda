// Exercise the shipped CLI and a local browser without a server or npm dependencies.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const dir = mkdtempSync(join(tmpdir(), "panda-html-"));
const workspace = join(dir, "workspace");
mkdirSync(workspace);
const cli = (...args) => execFileSync(process.argv[2], args, {
    cwd: workspace, env: { ...process.env, PANDA_HOME: join(dir, "state") }, encoding: "utf8"
});

async function offlineBrowser(file, verify) {
    const chrome = spawn(process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", [
        "--headless", "--disable-background-networking", "--no-first-run", "--remote-debugging-pipe",
        "--user-data-dir=" + join(dir, "profile")
    ], { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    // Chrome's DevTools pipe carries NUL-delimited JSON requests and replies.
    let nextId = 0, buffer = "", attached;
    const pending = new Map(), errors = [], requests = [];
    chrome.stdio[4].setEncoding("utf8").on("data", chunk => {
        const packets = (buffer + chunk).split("\0");
        buffer = packets.pop();
        for (const packet of packets) {
            const msg = JSON.parse(packet);
            if (msg.id) {
                const { resolve, reject } = pending.get(msg.id);
                pending.delete(msg.id);
                if (msg.error) reject(Error(msg.error.message));
                else resolve(msg.result);
            } else if (msg.method === "Runtime.exceptionThrown") errors.push(msg.params.exceptionDetails.text);
            else if (msg.method === "Network.requestWillBeSent") requests.push(msg.params.request.url);
            else if (msg.method === "Page.loadEventFired") loaded();
        }
    });
    let loaded;
    function send(method, params = {}) {
        const id = ++nextId;
        return new Promise((resolve, reject) => {
            pending.set(id, {resolve, reject});
            chrome.stdio[3].write(JSON.stringify({ id, method, params, sessionId: attached }) + "\0");
        });
    }
    const closed = once(chrome, "exit");
    const timeout = setTimeout(() => chrome.kill("SIGKILL"), 30_000);
    try {
        await Promise.race([closed.then(() => { throw Error("Chrome exited before verification"); }), (async () => {
            const {targetId} = await send("Target.createTarget", {url:"about:blank"});
            attached = (await send("Target.attachToTarget", {targetId, flatten:true})).sessionId;
            await send("Runtime.enable");
            await send("Network.enable");
            await send("Network.emulateNetworkConditions", {offline:true, latency:0, downloadThroughput:0, uploadThroughput:0});
            const onLoad = new Promise(resolve => { loaded = resolve; });
            await send("Page.enable");
            const url = pathToFileURL(file).href;
            await send("Page.navigate", {url});
            await onLoad;
            const evaluate = async expression => {
                const reply = await send("Runtime.evaluate", {expression, returnByValue:true});
                assert.ok(!reply.exceptionDetails, JSON.stringify(reply.exceptionDetails));
                return reply.result.value;
            };
            assert.equal(await evaluate("location.href"), url);
            assert.equal(await evaluate("navigator.onLine"), false);
            await verify(evaluate);
            assert.deepEqual(errors, []);
            assert.deepEqual(requests.filter(u => /^https?:/.test(u)), []);
        })()]);
    } finally {
        clearTimeout(timeout);
        chrome.kill();
        await closed;
    }
}

try {
    const fixture = readFileSync(new URL("./fixtures/pi-v3.jsonl", import.meta.url), "utf8").trim().split("\n").map(JSON.parse);
    const get = id => fixture.find(entry => entry.id === id);
    const literal = 'tool printed </script> <script>window.transcriptRan=true</script> & 海';
    get("result").message.content[0].text = literal;
    get("cp").summary = "Saved work.\n\n<read-files>\na.rs\n</read-files>\n\n<modified-files>\nb.rs\nc.rs\n</modified-files>";
    get("cp").details = {readFiles:["a.rs"], modifiedFiles:["b.rs","c.rs"]};
    // Put the sibling last in storage, then select the other leaf via a label record.
    const sibling = [get("sibling"), get("b-answer")];
    const records = fixture.filter(entry => !sibling.includes(entry)).concat(sibling, {
        type:"label", id:"bookmark", parentId:"a-answer", targetId:"sibling",
        timestamp:fixture[0].timestamp, label:"Alternative approach"
    });
    const archive = join(dir, "input.jsonl"), html = join(dir, "review.html");
    writeFileSync(archive, records.map(r => JSON.stringify(r)).join("\n") + "\n");
    const {session_id} = JSON.parse(cli("--import-session", archive));
    cli("--export-session", String(session_id), html);
    await offlineBrowser(html, async evaluate => {
        const transcript = () => evaluate("document.getElementById('transcript').innerText");
        assert.match(await transcript(), /active answer/);
        assert.doesNotMatch(await transcript(), /sibling answer/);
        assert.ok((await transcript()).includes(literal));
        assert.equal(await evaluate("Boolean(window.transcriptRan)"), false);
        for (const file of ["a.rs","b.rs","c.rs"]) assert.equal((await transcript()).split(file).length, 2);
        const options = await evaluate("Array.from(document.querySelector('select').options, o => o.text)");
        assert.equal(options.length, 2);
        assert.equal(options[1], "Alternative approach");
        await evaluate("document.querySelector('select').selectedIndex = 1; document.querySelector('select').dispatchEvent(new Event('change'))");
        assert.match(await transcript(), /sibling answer/);
        assert.doesNotMatch(await transcript(), /active answer|a\.rs|b\.rs|c\.rs/);
    });
} finally {
    rmSync(dir, {recursive:true, force:true});
}
