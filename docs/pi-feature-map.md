# Pi feature map

Each row names the upstream behavior and the Panda regression that checks it. The tree cases use [Pi’s navigation tests at 9428424](https://github.com/earendil-works/pi/blob/942842439e1f1d65f68a61857ddb7590b80c5a92/packages/coding-agent/test/agent-session-tree-navigation.test.ts). Panda exercises the equivalent behavior with Rust assertions, a scripted provider and SQLite.

| Pi behavior | Panda coverage |
|---|---|
| Selecting a user message puts its text in the editor and moves the active leaf to its parent, including the root | [Rust storage test](../crates/local/src/store/tree.rs) checks root and nested prompts. [Terminal test](../crates/cli/tests/tree.py) edits the restored text and submits it. |
| Selecting an assistant answer sets it as the leaf with no editor text | The Rust storage test checks the selected history, usage and compaction boundary. Selecting the same answer twice preserves the session revision. |
| Moving between branches keeps both alternatives accessible | [Rust fixture](../crates/cli/src/tree_tests.rs) checks the exact history received by the provider. The terminal test quits, reopens the same session, selects each answer and checks the saved active path. |
| Picker navigation and filtering | [Panda property test](../crates/cli/src/picker.rs) checks generated key sequences against stable entry IDs, including identical answer text on sibling branches. |
| Guided branch summaries | Shift-S in the tree opens an instruction editor. The [terminal regression](../crates/cli/tests/tree.py) checks the guidance in the summary request, cancellation, errors and placement after reopening. The [storage test](../crates/local/src/store/tree.rs) checks source-only input and unchanged navigation. Shipped in [#54](https://github.com/matt-riley/panda/pull/54). |
| Copy last reply | Panda’s `:copy` and Ctrl-X copy the original Markdown. The terminal test checks a private macOS pasteboard after reopening, switching branches and adding a branch summary. |
| Transient request recovery | The native shell regression checks one invocation and the saved result after SQLite reopen. Property tests cover retry limits and partial streams. |
| Context-window compaction | The core turn suite checks provider-reported usage after tools, summary projections and estimates after a checkpoint. |
| Recent-context retention | The loopback [one-shot regression](../crates/cli/tests/oneshot.py) checks that the next request retains a whole tool exchange under the configured token budget for both manual and automatic compaction. Shipped in [#56](https://github.com/matt-riley/panda/pull/56). |
| Early output-limit recovery | [Stream decoder tests](../crates/providers/src/lib.rs) exercise fragmented length stops. The [one-shot regression](../crates/cli/tests/oneshot.py) checks the next request and that an incomplete tool call never runs. Shipped in [#57](https://github.com/matt-riley/panda/pull/57). |
| File activity through compaction | A [property test](../crates/core/src/checkpoint_files.rs) checks successful operations across checkpoint boundaries. The one-shot regression covers direct and nested reads, modifications and reopening. Shipped in [#58](https://github.com/matt-riley/panda/pull/58). |
| Portable sessions and offline HTML | Pi JSONL import and export preserve the tree and active head. An [offline browser test](../crates/cli/tests/html_export.mjs) checks the initial branch, labels, literal tool output and switching branches. Shipped in [#48](https://github.com/matt-riley/panda/pull/48) and [#59](https://github.com/matt-riley/panda/pull/59). |
| Codemode console | Print and JSON loopback tests in the one-shot suite check ordered offsets 0 and 50 and cursor rollback on failure. Present on main after [#67](https://github.com/matt-riley/panda/pull/67). |

**Release coverage, 20 November 2026.** The machine’s installed Homebrew keg reported 0.1.14. The five behaviors from #54 through #59 above are in the 0.1.21 formula binary and on main. Console output was on main only at the audited revision (`18fb82e`). The linked regressions exercise fixtures. The separate [daily-use record](daily-use-verification.md) describes the live provider, Pi and Chrome runs. [Release 0.1.21](https://github.com/matt-riley/panda/releases/tag/v0.1.21) · [Audited main](https://github.com/matt-riley/panda/commit/18fb82e25df15bfd8e150883aea22a580cd1af85)

Panda’s picker offers user messages, completed assistant answers and saved branch summaries as navigation targets. Tool calls, results and reasoning stay in the selected path.

Run the storage cases with `cargo test -p panda-local store::tree`. On macOS, run the end-to-end case with `cargo test -p panda-cli edit_quit_reopen -- --ignored`. To exercise more key sequences, run `PROPTEST_CASES=16384 cargo test -p panda-cli tree_picker_key_sequences`.
