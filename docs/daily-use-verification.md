# Using Panda for a real change

On 20 November 2026 the Homebrew installation on this Mac reported 0.1.14. The following runs used the current tap’s Apple Silicon binary in a temporary directory. It reported 0.1.21 and its archive matched the formula’s SHA-256, `5304421f78f64edf5cc245a266d875459f7c99ff9bfe9a7b918a0c4577355a6c`. [Release](https://github.com/matt-riley/panda/releases/tag/v0.1.21)

## Change, restart and recovery

In a disposable checkout, Panda used OpenAI Codex (`gpt-6-luna`) in plan mode to read the feature map and search for the guided-summary regression. After an explicit `:code`, it edited the summary row and ran a Python assertion over the documentation and terminal regression. That assertion passed but missed a contradictory paragraph. A subsequent review found it. The corrected [feature map](pi-feature-map.md) records both the implementation and its tests.

Forking at the first completed answer left the edit in the parent history. The fork received a distinct marker, compacted ten messages and closed. Starting a fresh process with `--continue` restored the named fork and its marker. In another disposable repository, a shell call appended one line and slept. Terminating Panda during the sleep, restarting and acknowledging the unknown outcome left exactly one line.

## Pi round-trip

Pi 1.5.2 recorded a new, non-sensitive session about search pagination. It read a local note, answered on two branches, labeled the shared answer and compacted the selected branch. The short example used a four-token retention setting so Pi would compact it. A sanitized copy removed provider response identifiers and replaced signature data, system text and summary prose. The original recording stayed local and its digest was identical before and after the round-trip.

Panda imported the 13 entries, continued the selected branch with the live provider and exported JSONL and HTML. All 13 input records, including parent IDs, the bookmark, the checkpoint and its retained-entry reference, survived intact. The continued answer used the verified offset of 50. Pi reopened the export offline on the new head, with the summary present and the abandoned answer absent. Offline Chrome opened on that same answer and offered two branches.

Pi’s recorded system message remained archival when Panda assembled its own provider prompt. The exported header records the importing workspace. The imported call and result retained their matching ID. Panda preserved Pi’s original provider and model metadata, but the newly authored assistant reply exported as provider `panda`, model `unknown`. The [synthetic round-trip regression](../crates/cli/tests/oneshot.py) checks that distinction without shipping the live transcript.

## Finding and using a shared skill

Skill discovery was the first point where we needed to consult the Pi setup. A search for `code review` in Panda returned no lines, giving no indication whether the command had run or where to look next. That was the first actionable blocker. The portability run completed, with responder attribution a lower-priority limitation. A failing terminal regression reproduced the empty result. Panda now reports a miss and points to `PANDA_SKILL_ROOTS`.

For the personal `/skill-search` workflow, point Panda at the bundled Markdown skills when launching it. For example:

```sh
PANDA_SKILL_ROOTS="$HOME/.pi/agent/extensions/pi-extensions/.agents/skills" panda
```

On the real documentation edit, `:skills code review` found `implementation-review`, and `:skill:implementation-review Review docs/pi-feature-map.md for contradictions` invoked it in plan mode. It found the paragraph that contradicted the new summary row. This lets the same review skill work in both clients. For the usage check, `:session` showed aggregate input and output tokens on the reopened fork. The broader `/workflow` sequences investigation, implementation, verification and review. This run needed only skill lookup and invocation. [Pi workflow definitions](https://github.com/matt-riley/pi-extensions#prompt-templates-and-workflow-skills)

## Extension compatibility decision

Keep native commands and Markdown skills as the supported surface for this workflow. Its required operations are metadata search, explicit skill selection and a normal prompt under the current mode. Those operations all ran in the shipped CLI. The run identified no package requiring TypeScript execution, Node imports, lifecycle hooks or custom renderers. Defer a Rust plugin contract and a Pi-compatible loader until a named daily workflow demonstrates a missing operation. Any later executable project resource needs explicit trust and must pass the same mode and workspace checks as native tools.

## Durable local jobs decision

Keep the CLI process as the owner of a local turn for now. The concrete candidate was a long test run started from a coding turn. For this change, the local check completed before exit and the hosted suite already had its own lifetime. The interrupted-shell experiment demonstrated why an in-flight call cannot safely be reissued after a restart.

SQLite already preserves the transcript and outstanding call IDs. RPC exposes session inspection, cancellation and acknowledgement while the host is running. The in-memory host can run the core with a scripted provider. None of these is a detached executor that owns a child across CLI exits. There is no demonstrated need to add one for this workflow.

If a local-only test must survive client exit, qualify that single job first. Have a scripted provider start a delayed test that appends a unique marker. Disconnect the client, restart it, reconnect by job ID and verify terminal status and the saved result. Repeat with cancellation during the delay and check that the child stops. Terminate the executor after the append but before recording success and require an unknown outcome, explicit acknowledgement and exactly one marker. That test would justify revisiting ownership without assuming a remote worker fleet.
