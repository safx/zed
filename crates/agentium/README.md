# Agentium

A terminal application for parallel work with AI coding agents, powered by [Zed](https://zed.dev/) and built on [GPUI](../gpui/).

## Features

- **Multiple arenas** — create isolated arenas with independent pane layouts for each agent
- **Terminal** — integrated terminal with shell support
- **LSP** — language server support for Go to Definition, Find All References, etc.
- **Diff view** — view uncommitted changes (powered by `git_ui::ProjectDiff`)
- **Project search** — full-text search across the project
- **Git status** — read-only view of changed files grouped by Conflicts/Tracked/Untracked, with click-to-open
- **Markdown preview** — preview markdown files side-by-side
- **Pane splitting** — split panes in any direction, drag and drop tabs between panes
- **Claude Code integration** — receive notifications when Claude Code finishes a task via hook-based IPC

## Claude Code Hook Setup

Add the following to your Claude Code `settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "matcher": "startup", "hooks": [{ "type": "command", "command": "agentium claude hook session-start" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "agentium claude hook stop" }] }],
    "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agentium claude hook notification" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "agentium claude hook user-prompt-submit" }] }]
  }
}
```

When Claude Code completes a response, the corresponding terminal tab shows a blue dot indicator and the arena sidebar shows a badge count. Switching to the tab clears the notification.

## Building

```
cargo run -p agentium
```

Requires Metal Toolchain on macOS:

```
xcodebuild -downloadComponent MetalToolchain
```


## completion

#### zsh
```
if command -v agentium >/dev/null 2>&1; then eval "$(command agentium completions zsh)"; fi
```
