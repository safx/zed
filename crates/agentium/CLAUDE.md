# Agentium crate rules

## Arena vs Workspace terminology

Agentium uses two distinct concepts that must not be confused:

- **Arena** (`Arena` struct in `agentium.rs`): An Agentium-specific concept representing an isolated work area for an AI agent. Each arena has its own name, pane layout, working directory, and active pane. Multiple arenas are listed in the left sidebar and can be switched between. This is the unit the user creates via "+ New Arena".

- **Workspace** (`workspace::Workspace` from the `workspace` crate): A Zed infrastructure entity that provides project, language registry, modal layer, and other shared services. It is **not rendered** in Agentium's element tree — it exists only as a data store. There is one `Workspace` entity shared across all arenas.

The rendering hierarchy is `AgentiumApp` -> `Arena` -> `PaneGroup` -> `Pane`. The `Workspace` entity is accessed via `WeakEntity<Workspace>` stored in each `Arena`.

When you see `workspace` in agentium code, check the type: `Entity<Workspace>` / `WeakEntity<Workspace>` refers to Zed's workspace infrastructure; `Entity<Arena>` refers to Agentium's arena concept.

## Binary entry point must handle `--printenv`

The `util::shell_env` module launches the current executable with `--printenv` to capture shell environment variables when creating terminals. Without handling this flag in `main()`, a full GUI app launches instead of printing env vars and exiting. This causes a phantom second window on every terminal creation (including pane splits).

```rust
fn main() {
    if std::env::args().any(|arg| arg == "--printenv") {
        util::shell_env::print_env();
        return;
    }
    // ... rest of app
}
```

Any new binary in the Zed repo that creates terminals needs this same pattern (see `crates/zed/src/main.rs`).

## WASM parsers must be disabled before language initialization

The workspace-level `tree-sitter` dependency has `features = ["wasm"]`, which links wasmtime/cranelift into the binary. When tree-sitter creates a parser via `with_parser()`, it initializes a `WasmStore` that triggers cranelift compilation of tree-sitter's WASM stdlib. This causes a stack overflow (`EXC_BAD_ACCESS code=2`) on macOS background threads (512KB stack).

Agentium only uses native grammars (registered via `languages::init` with `load-grammars` feature), so WASM support is unnecessary. Call `language::disable_wasm_parsers()` before `languages::init()`:

```rust
language::disable_wasm_parsers();
languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);
```

## Workspace entity is not in the element tree

Many Zed crates register action handlers on `Workspace` via `workspace.register_action(...)` inside `cx.observe_new`. These handlers are unreachable in agentium because GPUI dispatches actions by bubbling up through the element tree, which does not include `Workspace`.

When integrating a Zed crate that registers workspace actions (e.g. `markdown_preview::init`), you must also register equivalent action handlers on `Arena`'s rendered element via `.on_action(cx.listener(...))` in its `render` method. Use `self.workspace.upgrade()` and `workspace_entity.update(cx, ...)` to access `Workspace` state (project, languages, weak handle) needed by the crate's public API.

Additionally, many default keybindings in `assets/keymaps/` are scoped to `"context": "Workspace"`. GPUI matches this predicate against `KeyContext` values set on elements in the tree via `.key_context()`. Since `Workspace` is not rendered, its `KeyContext` with `"Workspace"` is never in the dispatch path. The `Arena` render method must set `.key_context("Workspace")` on a wrapper div that is an ancestor of focused elements, so that keybindings scoped to `"Workspace"` context (e.g. `cmd-p` for `file_finder::Toggle`) can match.

## Claude Code hook IPC uses `Rc<RefCell<HashSet<u32>>>` for cross-closure state

The `ready_shell_pids` set is shared between `AgentiumApp` (which writes it when Claude Code sessions become ready or are cleared) and per-pane indicator closures (which read it to decide whether to show a dot on a terminal tab). Because all access is on the single foreground thread, `Rc<RefCell<...>>` is used instead of `Arc<Mutex<...>>`. The `AgentiumApp` owns the canonical `claude_sessions: HashMap<String, ClaudeSession>` and syncs the derived `HashSet<u32>` cache via `sync_ready_shell_pids()` after every mutation.

## IPC protocol distinguishes messages by first byte

The Unix datagram socket at `agentium.sock` carries two message formats: raw UTF-8 paths (for `agentium arena new`) and JSON objects (for `agentium claude hook`). The receiver distinguishes them by checking whether the first byte is `{`. This works because UNIX paths start with `/`, never `{`.

## LSP requires `language_server_download_dir`

`LanguageRegistry::set_language_server_download_dir` must be called before `languages::init`. Without it, LSP servers that aren't already on PATH cannot be downloaded or cached. Note that Node.js-based LSP servers (TypeScript, Pyright) also require a real `NodeRuntime`, which Agentium currently does not provide (`NodeRuntime::unavailable()`).

## Cross-file navigation uses `pane_for_open()` not `active_pane()`

Zed's `editor.rs` navigation code (`navigate_to_hover_links`, `find_all_references`, `open_locations_in_multibuffer`) was changed to use `workspace.pane_for_open()` instead of `workspace.active_pane()`. This is necessary because `Workspace::active_pane` points to its own internal pane (not rendered in Agentium), while `pane_for_open()` checks `last_active_center_pane` first, which Agentium sets via `Workspace::set_last_active_center_pane` on `pane::Event::Focus`.
