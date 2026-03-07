# Investigation: Agentium Terminal Pane Border Color Customization

## Context

Can Agentium change the border color of a specific terminal pane (e.g., to visually identify which terminal is running a Claude Code session)?

## Conclusion: Yes — Using the Existing `LeaderDecoration` Mechanism

Zed already has a **per-pane border coloring system** that can be leveraged.

---

## Existing Mechanism: `LeaderDecoration` + `PaneLeaderDecorator`

### 1. `LeaderDecoration` (`crates/workspace/src/pane_group.rs:306-309`)

```rust
pub struct LeaderDecoration {
    border: Option<Hsla>,           // Per-pane border color
    status_box: Option<AnyElement>, // Optional status indicator element
}
```

When `border` is `Some(color)`, the pane gets a colored border overlay rendered on top of it.

### 2. `PaneLeaderDecorator` trait (`pane_group.rs:311-315`)

```rust
pub trait PaneLeaderDecorator {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration;
    fn active_pane(&self) -> &Entity<Pane>;
    fn workspace(&self) -> &WeakEntity<Workspace>;
}
```

- `decorate()` is called for **each pane** during rendering
- It can return different `LeaderDecoration` values per pane
- Used by the collaboration feature to show leader borders with participant colors

### 3. How the border is rendered (`pane_group.rs:527-536`)

```rust
.when_some(decoration.border, |this, color| {
    this.child(
        div()
            .absolute()
            .size_full()
            .left_0()
            .top_0()
            .border_2()
            .border_color(color),
    )
})
```

An absolutely-positioned overlay div with `border_2()` is placed on top of the pane content.

---

## Current Agentium Implementation

In `crates/agentium/src/agentium.rs:473`:

```rust
let decorator = ActivePaneDecorator::new(&self.active_pane, &self.workspace);
self.center.render(workspace.zoomed_item(), &decorator, window, cx)
```

`ActivePaneDecorator` always returns `LeaderDecoration::default()` (no border). This is the extension point.

---

## Proposed Change

Create a custom `PaneLeaderDecorator` implementation for Agentium:

1. **Implement `AgentiumPaneDecorator`** in `crates/agentium/src/agentium.rs`
2. In `decorate()`, check if the pane's active item is a `TerminalView` running a Claude Code session
3. If so, return `LeaderDecoration { border: Some(highlight_color), status_box: None }`
4. Replace `ActivePaneDecorator` usage with the new decorator

### Detection of Claude Code sessions

To determine which terminal is running Claude Code:
- Access the pane's active item → downcast to `TerminalView` → get `Terminal` → check `pid()`
- Walk the process tree from the terminal's shell PID to find child processes matching `claude`
- Alternatively, check for a Claude Code-specific environment variable set on the terminal

---

## All Border/Frame Mechanisms in Zed

| Mechanism | Level | Purpose | Dynamic? |
|---|---|---|---|
| **`LeaderDecoration`** | Per-pane | Collaboration leader borders | **Yes — per pane, per render** |
| **Dock border** (`dock.rs:854`) | Entire dock | Panel boundary lines | Theme only |
| **`active_pane_modifiers`** (`pane_group.rs:1375`) | Active pane | Focus indicator overlay | Settings only |
| **`pane_group_border`** | Pane dividers | Split lines between panes | Theme only |

**`LeaderDecoration` is the only mechanism that supports per-pane, dynamic border color changes.**

---

## Theme Colors (for reference)

Defined in `crates/theme/src/styles/colors.rs`:

| Color | Used? | Purpose |
|---|---|---|
| `border` | Yes | General borders including dock edges |
| `border_variant` | Yes | Deemphasized borders |
| `border_focused` | Yes | Focused element borders |
| `border_selected` | Yes | Selected element borders, active pane overlay |
| `panel_background` | Yes | Panel surface background |
| `panel_focused_border` | **No** | Defined but unused — intended for focused panels |
| `pane_focused_border` | **No** | Defined but unused — intended for focused panes |
| `pane_group_border` | Yes | Divider color between panes |

Note: `panel_focused_border` and `pane_focused_border` are defined in themes but never referenced in rendering code, suggesting they were planned for future use.

---

## Key Files

- `crates/workspace/src/pane_group.rs` — `LeaderDecoration`, `PaneLeaderDecorator`, `ActivePaneDecorator`, pane rendering
- `crates/agentium/src/agentium.rs:473` — Where the decorator is used in Agentium
- `crates/workspace/src/dock.rs:849-864` — Dock-level border rendering
- `crates/theme/src/styles/colors.rs` — Theme color definitions
