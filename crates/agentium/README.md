# Agentium

A terminal application for parallel work with AI coding agents, powered by [Zed](https://zed.dev/) and built on [GPUI](../gpui/).

## Features

- **Multiple workspaces** — create isolated workspaces with independent pane layouts
- **Terminal** — integrated terminal with shell support
- **Diff view** — view uncommitted changes (powered by `git_ui::ProjectDiff`)
- **Project search** — full-text search across the project
- **Git status** — read-only view of changed files grouped by Conflicts/Tracked/Untracked, with click-to-open
- **Markdown preview** — preview markdown files side-by-side
- **Pane splitting** — split panes in any direction, drag and drop tabs between panes

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