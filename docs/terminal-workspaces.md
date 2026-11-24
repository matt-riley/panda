# Saved workspaces and terminal preferences

`panda --all-sessions` lists saved sessions with their workspace paths. Choose a session and open its original directory or fork into the directory where you launched Panda. If the original directory disappeared, **Continue here** moves the saved session to the current directory. **Fork into current workspace** gives the copy its own ID. Escape or Cancel exits without changing either workspace.

In scripts, use `--resume-any ID` to open the original directory and `--fork-here ID` to fork. Both resolve tools, attachments, project instructions and prompt file paths in the chosen directory. Opening another directory clears the launch directory's `--trust-project` approval. Ordinary `--continue` and `--session ID` stay scoped to the launch directory.

## Checkpoint transcripts

After compaction, `:context` shows the checkpoint's private transcript pointer. Use `:transcript` to display the current transcript or `panda --recover-transcript ID POINTER` in the session's workspace to write its chronological JSONL to stdout. Append `--session-dir DIR` for a custom store. The private SQLite database retains the newest 50 transcripts per session. The active branch and checkpoint are saved together. Ephemeral runs keep their history in memory.

## Terminal preferences

Place theme files in `$PANDA_HOME/themes` (default `~/.panda/themes`). For example, save the following as `plum.json`.

```json
{"accent":"#c9a1e7","muted":"#a69faf","selection":"#342844"}
```

Switch immediately with `:theme plum`, inspect the current choice with `:theme`, and restore the built-in palette with `:theme default`. The selection persists in `theme.json`. Set `NO_COLOR` for monochrome output.

In `$PANDA_HOME/keybindings.json`, map picker actions to keys. For example:

```json
{"up":"F2","down":"F3","select":"Ctrl-G"}
```

The actions are `up`, `down`, `select`, `close`, `pageUp` and `pageDown`. Available keys include their standard navigation keys, F1 through F12, and unreserved Ctrl-letter combinations. Restart Panda after editing bindings. Use `:keys` or `:help` to see the active assignments. Duplicate and reserved keys report the conflicting action and file.

## Terminal scrollback

Start with `panda --scrollback` or `panda --scrollback --continue`. The transcript stays in the terminal after exit. Use the terminal’s scroll controls to revisit earlier output. The Vim prompt and command pickers remain available between turns. During a turn, Ctrl-C cancels the request. Enter follow-ups at the next prompt.

Panda previews PNG attachments and tool results directly in Kitty (`TERM=xterm-kitty`) and iTerm2 (`TERM_PROGRAM=iTerm.app`). Each turn or session replay can display up to four previews sharing 512 KiB of base64 data. The image dimensions must fit within 4096 × 4096 pixels, and each preview occupies at most 40 columns by 12 rows. Other image formats, larger images, multiplexers, and unrecognized terminals get a text placeholder. The original attachment stays in the session.
