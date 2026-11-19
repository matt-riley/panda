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

For DeepSeek or OpenRouter, set `DEEPSEEK_API_KEY` or `OPENROUTER_API_KEY` in the launching shell. Panda defaults to DeepSeek. Select a provider and model with `:model openrouter openai/gpt-4.1-mini`. To retain a configured key for future sessions, use `:login deepseek` or `:login openrouter`.

For ChatGPT Codex, enter `:login`, choose **OpenAI Codex**, then choose **Open browser** or **Use device code**. After signing in, enter `:model` to search the models from configured providers. Type to filter, move with ↑ or ↓, press Enter to select, or Esc to cancel. GPT selections use your Codex subscription. The direct command `:login openai` also works. The Rust binary handles authorization, token refresh, streaming, and tool calls. Existing Panda subscription credentials are read from the same private SQLite database.

## Use a session

The interactive prompt starts in Vim Insert mode. Press `Esc` for Normal mode, use motions such as `w`, `b`, `0` and `$`, and edit with `cw`, `dw` or `u`. Press `i` to insert again. Enter sends the prompt. Alt-Enter inserts a newline in terminals that report the shortcut. Multiline paste stays in the editor until you submit it.

The interactive UI fills the terminal. Assistant replies render Markdown as they stream, including headings, lists, emphasis, inline code and fenced code. Reopening a session renders its saved Markdown with the same styles and wrapping. Page Up and Page Down scroll the transcript while you edit. Successful file edits show a coloured, numbered before-and-after excerpt with context. The prompt sits between two rules, with the workspace and Git branch, session mode, latest reported input/output token counts, and selected model below it. The lower-left footer shows the Vim mode. Press Esc, type a command such as `:q`, and press Enter. In either the idle or steering prompt, typing `:mo` shows `:model` and `:models` above the command line. Tab completes the first match and cycles forward, Shift-Tab cycles backward, and Enter runs the displayed command. Esc cancels the command and restores the draft. Existing slash commands still work. The steering prompt shares the idle editor’s Vim motions, operators, and undo history. Set `NO_COLOR=1` for monochrome. Use `:tools` for full tool results.

- `:plan` enables directory listing, file discovery by glob, paginated literal and regex search, file reads, skill lookup and conversational clarification. Type `:code` to enable edits, writes and shell commands.
- `:resume` opens a searchable picker for this workspace. Type part of a name, prompt, or reply to see matching sessions and a conversation preview. Use ↑/↓ to preview another result, Enter to resume, or Esc to return. In piped input, bare `:resume` lists sessions. `:resume ID` reopens one and displays its transcript and mode. Ctrl-C interrupts an active turn. `:quit` closes Panda.
- After an interrupted turn, inspect the workspace and use `:ack` to close outstanding calls with an unknown outcome. Panda never reruns the old call during resume.
- Panda loads `$PANDA_HOME/AGENTS.md` and instructions from the Git root down to the launch directory. Its `instructions` tool reads the applicable nested files for a workspace path. Panda discovers Markdown skills in workspace and user skill directories. Set `PANDA_SKILL_ROOTS` to add colon-separated directories.
- Workspace tools inherit a private scratch home, restricted filesystem access, and a network-denying macOS profile. System commands and Xcode build tools are available. Set `PANDA_TOOLCHAIN_DIRS` to colon-separated, trusted toolchain installations to grant read access and add their `bin` directories to the tool PATH.
- `PANDA_HOME` defaults to `~/.panda`. This private directory holds the SQLite session database, model selection, and any stored credentials. Install Panda outside repositories you let it edit.

## Prompt, tools and sessions

| To do this | In Panda |
|---|---|
| Attach a workspace file | Type `@src/main.rs`, or `@"path with spaces"`. Press Tab in Insert mode for completions. For large files, ask Panda to read a range of lines. |
| Send an image | Attach a PNG, JPEG or WebP by path, or use Ctrl-V in Insert mode or `:paste` for a macOS clipboard image. `:detach` clears the pending clipboard image. Select a model that accepts images. |
| Edit a longer prompt | Press Ctrl-G in Insert mode. Panda opens the draft in `$VISUAL`, `$EDITOR` or `vi`. Save and close the editor to return. |
| Recover from a temporary provider failure | Panda retries the current model request up to three times before any response is shown. The countdown appears above the prompt. Ctrl-C cancels it. Completed tools are never replayed by the retry. |
| Redirect an active turn | Type a correction and press Enter. Panda delivers it after the current batch of tools finishes. Alt-Enter queues a follow-up after the answer. Alt-Up brings queued text back into the draft. |
| Copy a reply | Use `:copy` or Ctrl-X in Insert or Normal mode to copy the last completed reply on the active branch as original Markdown. During a turn the command runs when Panda returns to the prompt. |
| Inspect output | Shell output appears as it arrives. During a turn the activity timer sits above the steering prompt. Ctrl-O at the prompt expands tool results, `:tool ID` toggles one and Ctrl-T shows or hides reasoning supplied by the model. |
| Run a shell command yourself | In code mode, `!git status` adds the command and result to the conversation. `!!git status` displays it locally. Both use the workspace sandbox. |
| Explore another approach | Open `:tree`, type to filter, and select an earlier prompt to edit it in Insert mode. Submit it to start a sibling branch in the same session. Enter switches immediately. Press `s` on a selected entry to summarize the branch you are leaving and carry its findings into the destination. Use `/` to filter names containing any letter, then ↑/↓ to select or Ctrl-S to summarize the match. Ctrl-C cancels generation. Select a completed answer to continue from it. Esc closes the picker. Both paths survive quitting and resuming. Navigation changes conversation history only. Workspace files keep their current contents. `:fork 2 alternative` starts a session at the second completed answer. `:clone NAME` copies the current conversation. |
| Return to a session | `:resume ID` or `:resume NAME` opens a session in the current workspace. `panda --continue` opens the most recently created session for that workspace. |
| Manage context | `:context` shows projected bytes, estimated input tokens and the input budget. Before each model request, including after a tool batch, Panda uses the latest reported token usage plus an estimate for new messages. It reserves a quarter of the context window for the reply, capped at 16,384 tokens. An unknown window defaults to 65,536 tokens. `:context BYTES` sets an additional byte threshold. `:compact` summarizes earlier conversation for the model while preserving the transcript on disk. |
| Choose a model | `:models openrouter deepseek` filters the provider catalogue. Use `:model NUMBER` to select a result. Use `:effort` to choose a reasoning level, or enter it directly with `:effort high`. `:thinking` is an alias. Codex choices come from the selected model’s catalogue entry. |
| Use a Markdown skill | Search with `:skills QUERY` and invoke one with `:skill:NAME task`. Panda shows the selected path. |

## Codemode

Start `panda --codemode` to let the model combine read-only workspace tools in JavaScript. For example, ask “Rank the five files with the most TODO lines.” The script can walk every search page, count matches, and send the model just the ranking. Codemode works in both plan and code mode, and the flag also works with print, JSON and RPC.

Each invocation starts a fresh QuickJS worker under macOS Seatbelt. The script can call the same read-only tools as plan mode. For example, `tools.repo_map({glob: "**/*.rs"})` lists Rust files and `tools.search({query: "^pub fn ", regex: true, glob: "src/**/*.rs"})` finds matching lines under `src/`. Search defaults to case-sensitive literal text. For example, `tools.search({query: "todo", ignoreCase: true, context: 2})` finds mixed-case TODOs and returns up to two lines on each side. `ignoreCase` also works with regular expressions. Globs match workspace-relative paths: `*` stays within one path component and `**/` matches zero or more directories.

`tools.ls()` lists the workspace root. Pass `{path: "src"}` to list the immediate entries in a directory. Each item contains its `name` and `type` (file, directory, symlink, or other). Listings include dotfiles and ignored names, report symlinks without following them, and return up to 50 entries in lexical order. Pass `next_offset` back as `offset` with the same path for the next page. An empty directory returns an empty page. A missing path produces an error.

Search and map honor workspace and nested `.gitignore` rules before returning up to 50 matching items per page. A later `!` rule can re-include a file if its parent directories are visible. Pass `next_offset` back as `offset` with the same arguments until it is `null`. Search items contain `path`, one-based `line`, and `text`. A positive `context` adds `before` and `after` arrays of text in file order, clipped at file boundaries. Each line is limited to 500 characters. Only matching lines count toward the offset. A context-heavy page can end early to fit the 1 MiB tool-result limit. Map items are paths. `tools.read({path, offset, limit})` returns text and accepts an optional line offset and limit. Use `text(value)` to select what goes into the saved tool result and the next model request. Nested calls appear live in the terminal and as `nested_call` events in JSON and RPC.

A script has 10 seconds, a 32 MiB JavaScript heap, up to 64 tool calls and 16 KiB of selected output. Ctrl-C cancels the worker and its active workspace read.

## Scripting

```sh
panda --print "Where are sessions stored?"
panda --mode json "Read Cargo.toml and identify the workspace members"
git diff | panda --print --provider deepseek --model deepseek-flash --thinking high --no-session "Review this change"
panda --continue --mode rpc
```

Print mode writes the final answer. JSON mode emits one object per line, ending with a `done` object containing the answer and session ID. Both accept a prompt from standard input. If you also supply a prompt argument, Panda prepends the piped text verbatim, separates it from the prompt with a blank line, and resolves attachments from the argument.

For print and JSON runs, put options before the prompt. Use `--model MODEL` for an exact model ID on the configured provider, or add `--provider PROVIDER` to select a different provider. `--thinking LEVEL` accepts the same levels as `:thinking`, including `default`. These choices apply to this invocation. Add `--no-session` to keep the conversation in memory. In this mode the JSON session ID is `null`, and `--continue` is incompatible. Add `--codemode` to the same command to enable Codemode.

Select tools for a print or JSON run with `--tools read,search,repo_map,ls`. The list takes exact tool names and replaces the default selection. Use `--tools codemode,read` to enable Codemode with only `tools.read()` available inside scripts. Use `--no-tools` for a prompt-only run, such as `git diff HEAD | panda --print --no-tools "Review this diff"`. An explicit selection takes precedence over `--codemode`. The session’s plan or code mode still controls which selected tools can run.

Use `--system-prompt TEXT|PATH` to replace Panda’s built-in base prompt for a print or JSON run. Workspace instructions and guidance for enabled tools still follow the base. Each `--append-system-prompt TEXT|PATH` adds a final instruction in command-line order. Both flags accept literal text or the contents of an existing UTF-8 file. Relative paths resolve from the current directory.

```sh
git diff HEAD | panda --print --no-tools --system-prompt "You are a code reviewer." --append-system-prompt ./review-rubric.md --append-system-prompt "List findings by severity." "Review this diff."
```

RPC accepts one JSON object per input line and echoes its `id` on replies and turn events.

```json
{"id":1,"type":"prompt","text":"Read Cargo.toml and describe the workspace"}
```

During an RPC turn, send `steer` or `follow_up` with a `text` field, or send `cancel`. The terminal reply for that turn includes any undelivered input. Once the turn ends, send `session` to inspect the saved transcript, or `ack` after inspecting an interrupted tool call.

[Pi behavior and Rust test mapping](docs/pi-feature-map.md)

## Test and fuzz

```sh
cargo test --locked --workspace
cargo test --locked -p panda-cli --test isolation -- --ignored
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p panda-memory
```

The isolation tests require native macOS with permission to launch Seatbelt. The HTTP error and cancellation fixture uses loopback. The in-memory binary exercises the same turn loop independently of SQLite, the sandbox, providers and terminal.

The Markdown tests compare streamed prefixes with one-shot rendering across randomized Unicode, terminal widths and chunk boundaries. A SQLite round-trip test compares the live and resumed terminal frames.

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
