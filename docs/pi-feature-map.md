# Pi feature map

Each row names the upstream behavior and the Panda regression that checks it. The tree cases use [Pi’s navigation tests at 9428424](https://github.com/earendil-works/pi/blob/942842439e1f1d65f68a61857ddb7590b80c5a92/packages/coding-agent/test/agent-session-tree-navigation.test.ts). Panda exercises the equivalent behavior with Rust assertions, a scripted provider and SQLite.

| Pi behavior | Panda coverage |
|---|---|
| Selecting a user message puts its text in the editor and moves the active leaf to its parent, including the root | [Rust storage test](../crates/local/src/store/tree.rs) checks root and nested prompts. [Terminal test](../crates/cli/tests/tree.py) edits the restored text and submits it. |
| Selecting an assistant answer sets it as the leaf with no editor text | The Rust storage test checks the selected history, usage and compaction boundary. Selecting the same answer twice preserves the session revision. |
| Moving between branches keeps both alternatives accessible | [Rust fixture](../crates/cli/src/tree_tests.rs) checks the exact history received by the provider. The terminal test quits, reopens the same session, selects each answer and checks the saved active path. |
| Picker navigation and filtering | [Panda property test](../crates/cli/src/picker.rs) checks generated key sequences against stable entry IDs, including identical answer text on sibling branches. |
| Optional summary on branch navigation | The Rust storage test `pi_summary_for_nested_prompt_is_attached_to_its_parent` checks source-only input, placement, provider failure and same-position navigation. The terminal test checks placement after an answer, Ctrl-C during generation, reopening and the next provider request. |

Pi’s custom instructions for branch summarization remain open. Panda’s picker offers user messages, completed assistant answers and saved branch summaries as navigation targets. Tool calls, results and reasoning stay in the selected path.

Run the storage cases with `cargo test -p panda-local store::tree`. On macOS, run the end-to-end case with `cargo test -p panda-cli edit_quit_reopen -- --ignored`. To exercise more key sequences, run `PROPTEST_CASES=16384 cargo test -p panda-cli tree_picker_key_sequences`.
