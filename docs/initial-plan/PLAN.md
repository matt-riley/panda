# Panda — initial plan for a modular Rust coding harness

## First release

Open a repository on macOS, ask how the code works, develop a plan, switch to coding mode, edit, run tests, and resume the conversation after closing the terminal. Support DeepSeek and OpenRouter API keys and ChatGPT Codex subscription login.

Run the same coding session in a second Rust host with a scripted provider, an in-memory workspace and an in-memory session store to verify reuse. [Pi coding-agent SDK](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/sdk.ts) · [Pi Durable host example](https://github.com/earendil-works/pi-durable-cloudflare)

## Architecture

- **panda-core** owns the turn loop, messages, streamed events, cancellation, tool dispatch and session transitions. It defines provider, tool and store interfaces.
- **panda-coding** assembles sessions, tool schemas, repository instructions, context and mode policy. The host supplies workspace operations and skill sources.
- **panda-providers** normalizes streaming, tool calls, usage, authentication and errors. Adapters share compatible wire protocols.
- **panda-local** supplies native sandboxed workspace operations, skill discovery, credential storage and SQLite persistence.
- **panda-cli** composes the local host, renders events and handles login, model selection, input, cancellation and resume.

Both hosts open or resume a session, submit input, receive typed events and cancel via the same entry point. Persist completed messages, tool IDs, results and turn status. Show interrupted calls as having an unknown outcome and let the user decide how to continue.

## Implementation milestones

1. Prove native macOS isolation on a disposable repository and establish supported subscription access. Verify a streamed tool round trip with Codex and record account or registration dependencies. [Codex sandbox](https://github.com/openai/codex/blob/main/codex-rs/sandboxing/src/seatbelt.rs) · [Pi providers](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md)
2. Exercise a scripted read and final response through both hosts. Add streaming DeepSeek, OpenRouter and Codex adapters. Keep credentials outside tool processes.
3. Ship the terminal loop with login, model selection, resume, streaming, tool visibility and cancellation. Support read, exact-match edit, write and shell through the sandbox. Display loaded repository instructions and persist sessions outside the working tree.
4. Integrate repository mapping and paginated text search, dispatch-enforced read-only plan mode with clarification, and on-demand discovery and loading of Markdown skills. Only explicit user action restores coding mode. Defer symbol indexing until real tasks justify it. [Personal Pi extensions](https://github.com/matt-riley/pi-extensions)
5. Use Panda for discovery, planning, editing, testing and session resume on a real change. Record the first missing capability and use it to prioritize guardrails, compaction, jobs and subagents.

## Release checks and defaults

- Build the second host independently and run the same scripted session suite for both hosts. Exclude terminal, SQLite, native sandbox and concrete provider dependencies from the second host.
- Sandbox workspace tools and descendants, deny network by default, allow coding-mode workspace and private scratch writes, verify symlink containment and credential isolation, and fail closed at startup.
- Test partial provider streams, fragmented tool arguments, cancellation and authentication errors offline. Record a separate live tool round trip for each provider.
- Verify direct writes are rejected in plan mode, search reaches the next page, selected skills load from disk and resume never repeats an interrupted shell command.
- For each slice, write a failing test, then make it pass. Run reproducible generated-input checks in the ordinary Rust suite and coverage-guided fuzzing against the stream parser.
- Keep `docs/initial-plan/PLAN.md` canonical. Mirror its headings, decisions, milestones, acceptance checks and sources to the private Notion page **Panda — Initial Plan** and compare the fetched content.
