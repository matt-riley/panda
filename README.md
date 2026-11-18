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

For ChatGPT Codex, use `/login openai-codex` and choose browser or device-code sign-in. After login, select an available model with `/model PROVIDER MODEL`. The Rust binary handles authorization, token refresh, streaming, and tool calls. Existing Panda subscription credentials are read from the same private SQLite database.

## Use a session

- `/plan` enables repository mapping, paginated literal search, file reads, skill lookup and conversational clarification. Type `/code` to enable edits, writes and shell commands.
- `/resume` lists this workspace’s sessions. `/resume ID` reopens one and displays its transcript and mode. Ctrl-C interrupts an active turn. `/quit` closes Panda.
- After an interrupted turn, inspect the workspace and use `/ack` to close outstanding calls with an unknown outcome. Panda never reruns the old call during resume.
- Start in the repository root to load its `AGENTS.md`. Panda discovers Markdown skills in workspace and user skill directories. Set `PANDA_SKILL_ROOTS` to add colon-separated directories.
- Workspace tools inherit a private scratch home, restricted filesystem access, and a network-denying macOS profile. System commands and Xcode build tools are available. Set `PANDA_TOOLCHAIN_DIRS` to colon-separated, trusted toolchain installations to grant read access and add their `bin` directories to the tool PATH.
- `PANDA_HOME` defaults to `~/.panda`. This private directory holds the SQLite session database, model selection, and any stored credentials. Install Panda outside repositories you let it edit.

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
