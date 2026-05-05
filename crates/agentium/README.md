# Agentium

A terminal application for parallel work with AI coding agents, powered by [Zed](https://zed.dev/) and built on [GPUI](../gpui/).

## Features

- **Multiple arenas** — create isolated arenas with independent pane layouts for each agent
- **Terminal** — integrated terminal with shell support
- **LSP** — language server support for Go to Definition, Find All References, etc.
- **Diff view** — view uncommitted changes (powered by `git_ui::ProjectDiff`)
- **Project search** — full-text search across the project
- **Git status** — view changed files grouped by Conflicts/Tracked/Untracked, with staging checkboxes, per-file diff stats (+N/-N lines), and click-to-open
- **Markdown preview** — preview markdown files side-by-side
- **File browser** — navigate project files with expand/collapse, open files for editing
- **Git graph** — visualize git commit history
- **Pane splitting** — split panes in any direction, drag and drop tabs between panes
- **GitHub PR tracking** — display PR status (draft/open/merged/closed/conflicted) with colored icons per arena, clickable to open in browser. Requires `gh` CLI.
- **CI status** — poll GitHub Actions check status for PRs with adaptive intervals based on commit age (60s/180s/300s), show pass/fail/pending icons with rich tooltip showing individual check results
- **Claude Code integration** — receive notifications when Claude Code finishes a task via hook-based IPC, fork sessions from tab context menu, display rate limit usage in sidebar
- **Claude Code session ↔ PR tracking** — persist a many-to-many mapping between Claude Code session IDs and GitHub PR numbers per project (`~/Library/Application Support/Agentium/pr.json`), queryable via CLI
- **Running-command badge** — sidebar pill (theme-inverted white/black) shows the count of terminals in each arena currently running a non-Claude command (e.g. `cargo build`, `sleep 30`)

## Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Cmd+1`...`Cmd+9` | Switch to arena 1-9 |
| `Ctrl+[` / `Ctrl+]` | Previous / next pane |
| `Cmd+[` / `Cmd+]` | Previous / next tab |
| `Cmd+P` | File finder |
| `Cmd+W` | Close active tab |

## Claude Code Hook Setup

Add the following to your Claude Code `settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "matcher": "startup", "hooks": [{ "type": "command", "command": "agentium claude hook session-start" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "agentium claude hook session-end" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "agentium claude hook stop" }] }],
    "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agentium claude hook notification" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "agentium claude hook user-prompt-submit" }] }],
    "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "agentium claude hook permission-request" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "agentium claude hook post-tool-use" }] }],
    "PostToolUseFailure": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "agentium claude hook post-tool-use-failure" }] }]
  },
  "statusLine": "agentium claude statusline"
}
```

Terminal tabs show dot indicators based on execution state:

- **Claude terminals**: green dot while a prompt is running, blue dot + blue pane border when completed
- **Non-Claude task terminals**: green dot while running, blue dot on success, red dot on failure

Pressing any key while focused on a terminal clears its dot (and border). Selecting a terminal from the arena badge menu also clears it.

The arena sidebar shows pill-shaped badges:
- Orange pill — Claude sessions awaiting a permission decision (clickable: opens a menu to jump to the specific terminal)
- Green pill — running Claude sessions
- Blue pill — completed Claude sessions (clickable: opens a menu to jump to the specific terminal)
- White/black pill (theme-inverted) — terminals running a non-Claude foreground command, refreshed every 2 seconds

The `statusLine` setting enables rate limit display in the sidebar. Claude Code periodically sends session data (including rate limit usage) via stdin to the configured command. Agentium passes it through to stdout (required by the protocol) and extracts rate limit info for display. A "!" indicator appears if no update has been received for over 1 hour.

## CLI

### `agentium arena new <path>`

Open a new arena for the given directory.

### `agentium pane split`

Split the active pane.

```
agentium pane split [--horizontal|--vertical] [--before] [--type <TYPE>] [--keep-focus] [-- <COMMAND>...]
```

- `--horizontal` — split horizontally (new pane to the right, or left with `--before`)
- `--vertical` — split vertically (new pane below, or above with `--before`). This is the default.
- `--before` — place the new pane before the active one
- `--type` — content type: `terminal` (default), `diff`, `branch-diff`, `git-status`, `project-search`, `git-graph`
- `--keep-focus` — keep focus on the current pane instead of switching to the new one

### `agentium tab new`

Add a new tab to the active pane.

```
agentium tab new [--type <TYPE>] [-- <COMMAND>...]
```

- `--type` — content type: `terminal` (default), `diff`, `branch-diff`, `git-status`, `project-search`, `git-graph`

### `agentium claude hook <event>`

Claude Code hook integration. Events: `session-start`, `session-end`, `stop`, `notification`, `user-prompt-submit`, `permission-request`, `post-tool-use`, `post-tool-use-failure`.

### `agentium claude statusline`

Claude Code statusline pass-through. Reads JSON from stdin, writes it back to stdout unchanged, and sends rate limit data to the running Agentium instance via IPC.

### `agentium claude sessions`

List Claude Code sessions linked to GitHub PRs for the current project. The project is detected via `git rev-parse --show-toplevel` (canonicalized).

```
agentium claude sessions [--pr <NUMBER>] [--all-worktrees|-a]
```

- No flags: list all `<pr>\t<session_id>` rows for the current project, sorted by PR number
- `--pr <NUMBER>` — filter to sessions linked to that PR only
- `--all-worktrees` / `-a` — walk all worktrees of this repo (`git worktree list --porcelain`) and print a `worktree <path>` header before each group

The mapping is populated automatically: when a Claude Code session submits a user prompt (`user-prompt-submit` hook), the arena is marked dirty; when a PR is subsequently fetched for that arena, all sessions in the arena are linked to that PR. Branch switches reset the dirty flag so stale associations aren't written after checkout. Data is stored at `~/Library/Application Support/Agentium/pr.json`.

### `agentium claude grep`

Search user and assistant messages across Claude Code session transcripts under `~/.claude/projects/` belonging to the current repository.

```
agentium claude grep [--only-current-worktree|-c] [--ignore-case|-i] <PATTERN>
```

- `<PATTERN>` — Rust regex (the [`regex` crate](https://docs.rs/regex/) syntax).
- `-c` / `--only-current-worktree` — search only the current worktree's project directory (default: every worktree returned by `git worktree list --porcelain`).
- `-i` / `--ignore-case` — case-insensitive matching.

Only message content authored by the user or assistant is searched. Reasoning blocks (`thinking`), tool calls (`tool_use`, `server_tool_use`), and tool results (`tool_result`) are excluded, as are `progress`, `file-history-snapshot`, and other non-conversation entries. When a message spans multiple lines, only the first matching line is printed.

Output is ripgrep-like when stdout is a TTY (file headers in red, `<line>:<role>:<timestamp>:` prefix in yellow, matches highlighted with a yellow background), and machine-readable when piped:

```
<file>:<line>:<role>:<timestamp>:<content>
```

Exit codes: `0` on match, `1` on no match, `2` on invalid regex.

## Building

```
cargo run -p agentium
```

Requires Metal Toolchain on macOS:

```
xcodebuild -downloadComponent MetalToolchain
```

## macOS App Bundle

To build `Agentium.app`:

```
./script/bundle-agentium
```

Options:
- `-d` — debug build
- `-o` — open the app after building

The bundle is output to `target/<triple>/release/bundle/osx/Agentium.app`.

Replace the placeholder icons at `crates/agentium/resources/app-icon{,@2x}.png` with real Agentium icons (512x512 and 1024x1024).

## Completion

#### zsh
```
if command -v agentium >/dev/null 2>&1; then eval "$(command agentium completions zsh)"; fi
```
