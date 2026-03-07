# Agentium crate rules

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

A Cargo feature flag approach (gating `tree-sitter/wasm` behind a `language` crate feature) would be cleaner but requires changes across 5 files (workspace Cargo.toml, language, languages, agentium Cargo.toml) due to feature unification in the dependency chain.

## Workspace entity is not in the element tree

Agentium's rendering hierarchy is `AgentiumApp` → `AgentiumWorkspace` → `PaneGroup` → `Pane`. The `Workspace` entity exists but is **not rendered** — it is used only as a data store (project, languages, etc.).

Many Zed crates register action handlers on `Workspace` via `workspace.register_action(...)` inside `cx.observe_new`. These handlers are unreachable in agentium because GPUI dispatches actions by bubbling up through the element tree, which does not include `Workspace`.

When integrating a Zed crate that registers workspace actions (e.g. `markdown_preview::init`), you must also register equivalent action handlers on `AgentiumWorkspace`'s rendered element via `.on_action(cx.listener(...))` in its `render` method. Use `self.workspace.upgrade()` and `workspace_entity.update(cx, ...)` to access `Workspace` state (project, languages, weak handle) needed by the crate's public API.

Additionally, many default keybindings in `assets/keymaps/` are scoped to `"context": "Workspace"`. GPUI matches this predicate against `KeyContext` values set on elements in the tree via `.key_context()`. Since `Workspace` is not rendered, its `KeyContext` with `"Workspace"` is never in the dispatch path. The `AgentiumWorkspace` render method must set `.key_context("Workspace")` on a wrapper div that is an ancestor of focused elements, so that keybindings scoped to `"Workspace"` context (e.g. `cmd-p` for `file_finder::Toggle`) can match.

## Claude Code hook IPC uses `Rc<RefCell<HashSet<u32>>>` for cross-closure state

The `ready_shell_pids` set is shared between `AgentiumApp` (which writes it when Claude Code sessions become ready or are cleared) and per-pane indicator closures (which read it to decide whether to show a dot on a terminal tab). Because all access is on the single foreground thread, `Rc<RefCell<...>>` is used instead of `Arc<Mutex<...>>`. The `AgentiumApp` owns the canonical `claude_sessions: HashMap<String, ClaudeSession>` and syncs the derived `HashSet<u32>` cache via `sync_ready_shell_pids()` after every mutation.

## IPC protocol distinguishes messages by first byte

The Unix datagram socket at `agentium.sock` carries two message formats: raw UTF-8 paths (for `agentium workspace new`) and JSON objects (for `agentium claude hook`). The receiver distinguishes them by checking whether the first byte is `{`. This works because UNIX paths start with `/`, never `{`.
