> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

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
- **PR tracking** — display every PR of the arena's branch (e.g. one to `develop` and one to `master`) with status icons (draft/open/merged/closed/conflicted) and base branch names, clickable to open in browser. GitHub repositories require the `gh` CLI. Backlog Git repositories (detected from the origin remote) require the [`bee`](https://nulab.github.io/bee/) CLI; their PRs are discovered through Backlog issues — the issue key in the branch name (e.g. `PROJ-123/fix-thing`) or issues registered on a task containing the arena — and show open/merged/closed states (Backlog has no draft, review, or CI data)
- **CI status** — poll GitHub Actions check status per PR (GitHub PRs only) with adaptive intervals based on commit age (60s/180s/300s), show pass/fail/pending icons with rich tooltip showing individual check results
- **Task board** — a Tasks sidebar tab with a priority-ordered task list; each task bundles issues (GitHub and Backlog) and arenas (worktrees), so multi-repository work is grouped under one task. Persisted to `~/Library/Application Support/Agentium/board.json`; closed worktrees reopen as arenas with one click. Issue titles/states are fetched via the `gh` and [`bee`](https://nulab.github.io/bee/) CLIs (both optional)
- **Claude Code integration** — receive notifications when Claude Code finishes a task via hook-based IPC, fork sessions from tab context menu, display rate limit usage in sidebar
- **Claude Code session ↔ PR tracking** — persist a many-to-many mapping between Claude Code session IDs and PR numbers per project (`~/Library/Application Support/Agentium/pr.json`), queryable via CLI. PR numbers are GitHub or Backlog depending on the project's origin remote
- **Codex state** — Codex terminals get the same running/permission/completed indicators as Claude Code, read from the terminal title Codex sets (braille spinner while working, `Action Required` while awaiting an approval); no hooks or Codex config needed
- **Running-command badge** — sidebar pill (theme-inverted white/black) shows the count of terminals in each arena currently running a command other than a coding agent (e.g. `cargo build`, `sleep 30`)

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
- **Codex terminals**: same dots, derived from the terminal title. Busy is reported once the spinner has run for 2 seconds so that the startup spin (MCP servers loading) does not count as a turn
- **Non-Claude task terminals**: green dot while running, blue dot on success, red dot on failure

Pressing any key while focused on a terminal clears its dot (and border). Selecting a terminal from the arena badge menu also clears it.

The arena sidebar shows pill-shaped badges:
- Orange pill — Claude or Codex sessions awaiting a permission decision (clickable: opens a menu to jump to the specific terminal)
- Green pill — running Claude or Codex sessions
- Blue pill — completed Claude or Codex sessions (clickable: opens a menu to jump to the specific terminal)
- White/black pill (theme-inverted) — terminals running a foreground command other than `claude` or `codex`, refreshed every 2 seconds

Terminals that previously hosted a Claude Code session are excluded from the white/black pill until that session ends. Codex terminals are excluded by process name and, once their title has shown a spinner, until Codex clears the title on exit. The `SessionEnd` hook is the canonical way to release the terminal so it can be counted as non-Claude busy when running other commands afterwards; on macOS, a caffeinate-based monitor is also used as a fallback for hard exits (SIGKILL, terminal close).

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
- `--title` — tab title override (terminal with a command only)

### `agentium tab send-message`

Paste text into the terminal tab whose title matches and optionally press Enter. Fails unless exactly one tab in the target arena matches.

```
agentium tab send-message --title <TITLE> [--arena <PATH>] [--submit] [--no-from] <MESSAGE>
```

- `--arena` — arena worktree to search; defaults to the arena the command runs in (process ancestry, then current directory)
- `--submit` — send Enter after pasting
- `--no-from` — do not prepend the sender line. By default a message sent from inside an Agentium terminal tab starts with `[from: <that tab's title>]` on its own line (the same title `agentium tab self` prints), unless the message already begins with `[from:` or is empty

### `agentium tab self` / `agentium tab list`

```
agentium tab self [--json]
agentium tab list [--arena <PATH>] [--json]
```

`self` prints the title of the terminal tab the command runs in. `list` prints title, kind, and Claude hook state (`permission`, `running`, `ready`, `idle`) for each tab of the arena. Both use the CLI socket `~/.local/share/agentium/agentium-cli.sock`; the Claude Code sandbox needs it in `sandbox.network.allowUnixSockets`.

### `agentium task`

Manage the task board. `new` / `add-issue` / `add-arena` / `add-pr` / `done` mutate the board: when an Agentium instance is running, they are handed to it as datagrams over `agentium.sock` (changes appear in the Tasks tab immediately); otherwise `board.json` is modified directly. `list` / `info` are read-only: when an Agentium instance is running, they are answered over the CLI socket (`~/.local/share/agentium/agentium-cli.sock`; the Claude Code sandbox needs it in `sandbox.network.allowUnixSockets`) from its in-memory board and PR cache, avoiding a race with the app's own background writes to `board.json`; without a running instance, they read `board.json` (and, for `info`, `pr_cache.json`) directly.

```
agentium task new <TITLE> [--issue <ISSUE>]... [--arena <PATH>]...
agentium task list [--json]
agentium task info [--task <TASK>] [--update] [--json]
agentium task add-issue <ISSUE> [--task <TASK>]
agentium task add-arena [<PATH>] [--task <TASK>]
agentium task add-pr <PR> [--task <TASK>]
agentium task done <TASK>
```

- `<ISSUE>` accepts a GitHub issue URL, `owner/repo#123`, a Backlog issue URL (`https://<space>/view/PROJ-123`), or a Backlog issue key (`PROJ-123`)
- `<PR>` accepts a GitHub PR URL, `owner/repo#123`, a Backlog PR URL (`https://<space>/git/<project>/<repo>/pullRequests/<n>`), or a bare number resolved against the current repo's origin. The PR's repository must be the origin of one of the task's arenas; otherwise `add-pr` errors and lists the task's origins
- `<TASK>` accepts the 1-based index shown by `task list`, a task UUID prefix, or a unique title substring
- When `--task` is omitted, the task containing the current directory's worktree (or a subdirectory of it) is used (errors with candidates if ambiguous)
- `task add-arena` defaults to the current directory; paths are canonicalized; it refuses a path that is already inside a worktree linked to the task
- `task list --json` includes archived tasks and task ids; the plain listing hides archived tasks
- `task info` prints the task's issues and its arenas with their PR state; PRs linked via `add-pr` are marked `(linked)`, and linked PRs whose repository matches none of the task's arenas are listed under `unlinked:`. Branch/commit/origin are always read locally via `git`. Arena PR data comes from the running app's PR cache (or, with no app running, `pr_cache.json` on disk), so values for arenas that were not recently active can be stale
- `task info --update` re-fetches PRs and issue metadata via `gh`/`bee`. This requires a running Agentium: the app performs the fetch and applies the result through the same path as PR polling, so `board.json` / `pr_cache.json` are updated and the Tasks sidebar reflects the refreshed values immediately. Without a running instance, `--update` errors instead of fetching. A fetch failure for one worktree or issue (provider unavailable, `gh`/`bee` error) is printed to stderr as a warning and leaves that worktree's/issue's cached value untouched
- `task list --json` prints the array of tasks (the app's reply's `tasks` field, or, with no app running, `board.json`'s tasks). `task info --json` prints the whole reply object (`ok`, `index`, `task`, `prs`, `warnings`) as JSON, whether or not an app is running

Issue metadata (title, state, URL) is fetched by the running app via `gh issue view` for GitHub and `bee issue view` for Backlog. Backlog integration requires the [`bee`](https://nulab.github.io/bee/) CLI, authenticated via `bee auth login`; without it, issues are shown by key only.

PRs linked with `add-pr` appear inside the row of every arena of the task whose origin is that repository, exactly like branch-discovered PRs (status icon, CI, reviews, tooltip). Right-click a PR pill for "Open in Browser"; a linked PR also offers "Remove from Task". Backlog PRs in arena rows carry a `b` badge.

### `agentium claude hook <event>`

Claude Code hook integration. Events: `session-start`, `session-end`, `stop`, `notification`, `user-prompt-submit`, `permission-request`, `post-tool-use`, `post-tool-use-failure`.

### `agentium claude statusline`

Claude Code statusline pass-through. Reads JSON from stdin, writes it back to stdout unchanged, and sends rate limit data to the running Agentium instance via IPC.

### `agentium claude sessions`

List Claude Code sessions linked to PRs for the current project (GitHub or Backlog Git, per the project's origin remote). The project is detected via `git rev-parse --show-toplevel` (canonicalized).

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
