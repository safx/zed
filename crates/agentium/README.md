# Agentium

A terminal application for parallel work with AI coding agents, powered by [Zed](https://zed.dev/) and built on [GPUI](../gpui/).

## Features

- **Multiple arenas** — create isolated arenas with independent pane layouts for each agent
- **Terminal** — integrated terminal with shell support
- **LSP** — language server support for Go to Definition, Find All References, etc.
- **Diff view** — view uncommitted changes (powered by `git_ui::ProjectDiff`)
- **Project search** — full-text search across the project
- **Git status** — view changed files grouped by Conflicts/Tracked/Untracked, with staging checkboxes and click-to-open
- **Markdown preview** — preview markdown files side-by-side
- **Pane splitting** — split panes in any direction, drag and drop tabs between panes
- **Claude Code integration** — receive notifications when Claude Code finishes a task via hook-based IPC, fork sessions from tab context menu, display rate limit usage in sidebar

## Claude Code Hook Setup

Add the following to your Claude Code `settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "matcher": "startup", "hooks": [{ "type": "command", "command": "agentium claude hook session-start" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "agentium claude hook stop" }] }],
    "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agentium claude hook notification" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "agentium claude hook user-prompt-submit" }] }]
  },
  "statusLine": "agentium claude statusline"
}
```

When Claude Code completes a response, the corresponding terminal tab shows a blue dot indicator and the arena sidebar shows a badge count. Switching to the tab clears the notification.

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
- `--type` — content type: `terminal` (default), `diff`, `branch-diff`, `git-status`, `project-search`
- `--keep-focus` — keep focus on the current pane instead of switching to the new one

### `agentium tab new`

Add a new tab to the active pane.

```
agentium tab new [--type <TYPE>] [-- <COMMAND>...]
```

- `--type` — content type: `terminal` (default), `diff`, `branch-diff`, `git-status`, `project-search`

### `agentium claude hook <event>`

Claude Code hook integration. Events: `session-start`, `stop`, `notification`, `user-prompt-submit`.

### `agentium claude statusline`

Claude Code statusline pass-through. Reads JSON from stdin, writes it back to stdout unchanged, and sends rate limit data to the running Agentium instance via IPC.

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
