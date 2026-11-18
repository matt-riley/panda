# Panda

Panda is a Rust coding harness with a macOS terminal host. It streams model replies, runs workspace tools under Seatbelt, and saves conversations locally. The core and coding crates can also run inside another host. The [initial plan](docs/initial-plan/PLAN.md) describes the boundaries and release checks.

## Run on macOS

Install the Intel or Apple Silicon release with `brew install matt-riley/tools/panda`. Check the installed version with `panda --version`.

From this checkout, install the terminal binary with a Rust toolchain that supports edition 2024 and standard-library file locks.

```sh
cargo install --locked --path crates/cli
cd /path/to/repository
panda
```

For DeepSeek or OpenRouter, set `DEEPSEEK_API_KEY` or `OPENROUTER_API_KEY` in the launching shell. Panda defaults to DeepSeek. Select a provider and model with `/model openrouter openai/gpt-4.1-mini`. To retain a configured key for future sessions, use `/login deepseek` or `/login openrouter`.

For ChatGPT Codex, enter `/login`, choose **OpenAI Codex**, then choose **Open browser** or **Use device code**. After signing in, enter `/model` to search the models from configured providers. Type to filter, move with ↑ or ↓, press Enter to select, or Esc to cancel. GPT selections use your Codex subscription. The direct command `/login openai` also works. The Rust binary handles authorization, token refresh, streaming, and tool calls. Existing Panda subscription credentials are read from the same private SQLite database.

## Use a session

The interactive prompt starts in Vim Insert mode. Press `Esc` for Normal mode, use motions such as `w`, `b`, `0` and `$`, and edit with `cw`, `dw` or `u`. Press `i` to insert again. Enter sends the prompt. Alt-Enter inserts a newline in terminals that report the shortcut. Multiline paste stays in the editor until you submit it.

The interactive UI fills the terminal. Page Up and Page Down scroll the transcript while you edit. Successful file edits show a coloured, numbered before-and-after excerpt with context. The prompt sits between two rules, with the workspace and Git branch, session mode, latest reported input/output token counts, and selected model below it. The input indicator shows the Vim mode. Set `NO_COLOR=1` for monochrome. Use `/tools` for full tool results.

- `/plan` enables repository mapping, paginated literal search, file reads, skill lookup and conversational clarification. Type `/code` to enable edits, writes and shell commands.
- `/resume` lists this workspace’s sessions. `/resume ID` reopens one and displays its transcript and mode. Ctrl-C interrupts an active turn. `/quit` closes Panda.
- After an interrupted turn, inspect the workspace and use `/ack` to close outstanding calls with an unknown outcome. Panda never reruns the old call during resume.
- Panda loads `$PANDA_HOME/AGENTS.md` and instructions from the Git root down to the launch directory. Its `instructions` tool reads the applicable nested files for a workspace path. Panda discovers Markdown skills in workspace and user skill directories. Set `PANDA_SKILL_ROOTS` to add colon-separated directories.
- Workspace tools inherit a private scratch home, restricted filesystem access, and a network-denying macOS profile. System commands and Xcode build tools are available. Set `PANDA_TOOLCHAIN_DIRS` to colon-separated, trusted toolchain installations to grant read access and add their `bin` directories to the tool PATH.
- `PANDA_HOME` defaults to `~/.panda`. This private directory holds the SQLite session database, model selection, and any stored credentials. Install Panda outside repositories you let it edit.

## Prompt, tools and sessions

| To do this | In Panda |
|---|---|
| Attach a workspace file | Type `@src/main.rs`, or `@"path with spaces"`. Press Tab in Insert mode for completions. For large files, ask Panda to read a range of lines. |
| Send an image | Attach a PNG, JPEG or WebP by path, or use Ctrl-V in Insert mode or `/paste` for a macOS clipboard image. `/detach` clears the pending clipboard image. Select a model that accepts images. |
| Edit a longer prompt | Press Ctrl-G in Insert mode. Panda opens the draft in `$VISUAL`, `$EDITOR` or `vi`. Save and close the editor to return. |
| Redirect an active turn | Type a correction and press Enter. Panda delivers it after the current batch of tools finishes. Alt-Enter queues a follow-up after the answer. Alt-Up brings queued text back into the draft. |
| Inspect output | Shell output appears as it arrives. During a turn the activity timer sits above the steering prompt. Ctrl-O at the prompt expands tool results, `/tool ID` toggles one and Ctrl-T shows or hides reasoning supplied by the model. |
| Run a shell command yourself | In code mode, `!git status` adds the command and result to the conversation. `!!git status` displays it locally. Both use the workspace sandbox. |
| Explore another approach | Use `/name NAME` and `/tree` to identify the session and completed turn. `/fork 2 alternative` starts a session at the second completed answer. `/clone NAME` copies the current conversation. |
| Return to a session | `/resume ID` or `/resume NAME` opens a session in the current workspace. `panda --continue` opens the most recently created session for that workspace. |
| Manage context | `/context` shows the estimated size in bytes. `/context BYTES` sets the automatic compaction threshold. `/compact` summarizes earlier conversation for the model while preserving the transcript on disk. |
| Choose a model | `/models openrouter deepseek` filters the provider catalogue. Use `/model NUMBER` to select a result. Use `/effort` to choose a reasoning level, or enter it directly with `/effort high`. `/thinking` is an alias. Codex choices come from the selected model’s catalogue entry. |
| Use a Markdown skill | Search with `/skills QUERY` and invoke one with `/skill:NAME task`. Panda shows the selected path. |

## Scripting

```sh
panda --print "Where are sessions stored?"
panda --mode json "Read Cargo.toml and identify the workspace members"
panda --continue --mode rpc
```

Print mode writes the final answer. JSON mode emits one object per line, ending with a `done` object containing the answer and session ID. Both modes accept a prompt from standard input. RPC accepts one JSON object per input line and echoes its `id` on replies and turn events.

```json
{"id":1,"type":"prompt","text":"Read Cargo.toml and describe the workspace"}
```

During an RPC turn, send `steer` or `follow_up` with a `text` field, or send `cancel`. The terminal reply for that turn includes any undelivered input. Once the turn ends, send `session` to inspect the saved transcript, or `ack` after inspecting an interrupted tool call.

## Test and fuzz

```sh
cargo test --locked --workspace
cargo test --locked -p panda-cli --test isolation -- --ignored
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p panda-memory
```

The isolation tests require native macOS with permission to launch Seatbelt. The HTTP error and cancellation fixture uses loopback. The in-memory binary exercises the same turn loop independently of SQLite, the sandbox, providers and terminal.

The regular Cargo suite uses proptest to generate Unicode arguments and arbitrary chunk boundaries. It also tests each chunk width for a fixed fragmented tool call. The coverage-guided target compares whole and chunked decoding on mutated bytes, starting with valid text and tool-call streams.

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
cargo +nightly fuzz run provider_stream -- -max_total_time=60
cargo +nightly fuzz run subscription_stream -- -max_total_time=60
```

## Local verification on 18 November 2026

DeepSeek, OpenRouter and ChatGPT Codex each completed a live tool round trip. Codex also completed a fresh browser login and returned the verified contents of a random test file.

Panda used DeepSeek to inspect a failing regression in its own subscription transcript conversion, proposed a plan, edited the implementation, and ran the two Node tests successfully in its sandbox. A new process reopened the session. The fixture exposed a practical requirement for access to an installed language toolchain, supplied through `PANDA_TOOLCHAIN_DIRS`. The run completed in Panda.


## Releasing

Conventional commits on main feed Release Please. Merge its release PR to publish the version in `version.txt`. This file versions the shipped binary independently of the internal Cargo workspace packages. The release workflow checks out the tag, packages the Intel and Apple Silicon binaries, uploads SHA-256 checksums, then dispatches the tap publisher at that tag. The publisher uses the tap App's `APP_ID` variable and `PRIVATE_KEY` Actions secret. To retry tap publication, dispatch the Homebrew workflow at the published tag.
