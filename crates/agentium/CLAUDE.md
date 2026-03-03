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
