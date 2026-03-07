# LSP Integration Investigation for Agentium

Date: 2026-03-07

## Goal

Enable LSP-powered features (Go to Definition, Go to Declaration, Go to Type Definition, Go to Implementation, Find All References, Rename Symbol, Format Buffer, Show Code Actions) in Agentium's file editor panel via the right-click context menu.

## Current Status

- The context menu **does appear** on right-click (confirmed by user).
- Clicking "Go to Definition" etc. **does nothing** (no jump occurs).

---

## Architecture: Full Action Dispatch Chain

```
Right-click on editor
  -> deploy_context_menu()                [crates/editor/src/mouse_context_menu.rs:158]
     (only shown if editor.project.is_some() && editor.mode().is_full())
  -> Builds ContextMenu with actions like Box::new(GoToDefinition)
  -> User clicks menu item
  -> GPUI dispatches GoToDefinition action, bubbles up element tree

EditorElement::register_actions()         [crates/editor/src/element.rs:225]
  (registered during prepaint, NOT on Workspace element)
  -> window.on_action(TypeId::of::<GoToDefinition>(), ...)
  -> calls editor.go_to_definition()

Editor::go_to_definition()                [crates/editor/src/editor.rs:17879]
  -> go_to_definition_of_kind(GotoDefinitionKind::Symbol, false, ...)

Editor::go_to_definition_of_kind()        [crates/editor/src/editor.rs:17969]
  -> self.semantics_provider              (Option<Rc<dyn SemanticsProvider>>)
  -> provider.definitions(&buffer, head, kind, cx)
  -> awaits result
  -> navigate_to_hover_links()

SemanticsProvider impl for Entity<Project> [crates/editor/src/editor.rs:27097]
  -> project.update(cx, |project, cx| match kind {
       GotoDefinitionKind::Symbol         => project.definitions(buffer, position, cx),
       GotoDefinitionKind::Declaration    => project.declarations(buffer, position, cx),
       GotoDefinitionKind::Type           => project.type_definitions(buffer, position, cx),
       GotoDefinitionKind::Implementation => project.implementations(buffer, position, cx),
     })

Project::definitions()                    [crates/project/src/project.rs:3983]
  -> self.lsp_store.update(cx, |lsp_store, cx| lsp_store.definitions(buffer, position, cx))

LspStore::definitions()                   [crates/project/src/lsp_store.rs:5671]
  -> [local] request_multiple_lsp_locally(buffer, GetDefinitions{position}, cx)
  -> [remote] upstream_client.request_lsp(...)

LspStore::request_lsp()                   [crates/project/src/lsp_store.rs:4930]
  -> finds language server for the buffer that has the right capability
  -> language_server.request::<lsp::request::GotoDefinition>(params, timeout)
  -> JSON-RPC "textDocument/definition" -> LSP server process
```

For `find_all_references`, the path differs: it uses `self.workspace()?.read(cx).project()` instead of `self.semantics_provider`, then calls `project.references(...)`.

---

## Key Files

| File | Purpose |
|------|---------|
| `crates/editor/src/mouse_context_menu.rs` | Context menu construction (`deploy_context_menu` at line 158) |
| `crates/editor/src/element.rs` | Action handler registration (`register_actions` at line 225) |
| `crates/editor/src/editor.rs` | `go_to_definition_of_kind` (line 17969), `navigate_to_hover_links` (line 18097), `SemanticsProvider` impl (line 27097) |
| `crates/editor/src/actions.rs` | Action definitions: `GoToDefinition` (line 554), `GoToDeclaration` (line 549), `GoToTypeDefinition` (line 576), `GoToImplementation` (line 562), `FindAllReferences` (line 886) |
| `crates/editor/src/items.rs` | `added_to_workspace` (line 989) — sets `editor.workspace` field |
| `crates/project/src/project.rs` | `Project::definitions` etc. (lines 3983–4071) — delegates to LspStore |
| `crates/project/src/lsp_store.rs` | `LspStore::new_local` (line 4150), `on_buffer_added` (line 4433), `initialize_buffer` (line 2466), `start_language_server` (line 400) |
| `crates/project/src/lsp_command.rs` | `GetDefinitions` (line 189) implements `LspCommand` trait — maps to `lsp::request::GotoDefinition` |
| `crates/language/src/language.rs` | `get_language_server_command` (line 714) — binary resolution: check_if_user_installed -> cached -> download |
| `crates/language/src/language_registry.rs` | `set_language_server_download_dir` (line 644) |
| `crates/languages/src/rust.rs` | `RustLspAdapter::check_if_user_installed` (line 635) — searches PATH for `rust-analyzer` |
| `crates/workspace/src/workspace.rs` | `Workspace::active_pane()` (line 4864), `pane_for_open()` (line 4868), `set_last_active_center_pane()` (line 4593) |
| `crates/workspace/src/item.rs` | `added_to_pane` (line 719) — calls `added_to_workspace` which sets editor.workspace |

---

## Identified Problems

### Problem 1: LSP Server Not Starting (Most Likely Root Cause)

#### What Agentium does

```rust
// main.rs
let languages = Arc::new(language::LanguageRegistry::new(
    cx.background_executor().clone(),
));
// ... NO call to languages.set_language_server_download_dir(...)
language::disable_wasm_parsers();
languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);
```

#### What Zed does (for comparison)

```rust
// crates/zed/src/main.rs:474
languages.set_language_server_download_dir(paths::languages_dir().clone());
```

#### LSP Binary Resolution Flow

The binary resolution in `get_language_server_command` (language.rs:714) follows this order:

1. **`check_if_user_installed`** — Searches PATH for the binary (e.g. `rust-analyzer`). If found, uses it directly. **This works without download_dir.**
2. **Check cached binary** — If step 1 fails, checks `cached_binary` field.
3. **Check `allow_binary_download`** — If disabled, returns error.
4. **`language_server_download_dir`** — If `None`, returns `Err("no language server download dir defined")`.
5. **Try cached server binary from disk** — Checks download dir for previously downloaded binary.
6. **Download** — Fetches new binary via `try_fetch_server_binary`.

Without `set_language_server_download_dir`, steps 4-6 all fail. The LSP server will ONLY start if the binary is already on PATH (step 1).

#### Additionally: NodeRuntime is unavailable

```rust
// main.rs
let node_runtime = node_runtime::NodeRuntime::unavailable();
```

This means Node.js-based LSP servers (TypeScript/tsserver, Pyright, etc.) cannot start at all, even if `language_server_download_dir` is set. Only LSP servers that are standalone binaries work (e.g. `rust-analyzer`, `clangd`).

#### Additionally: `language_extension::init` not called

Extensions can provide additional LSP adapters. Without `language_extension::init`, only built-in adapters from `languages::init` are available. This affects languages whose LSP support comes from extensions rather than built-in adapters.

### Problem 2: Navigation to Different Files Broken (Workspace active_pane Mismatch)

When Go to Definition resolves to a **different file**, the navigation code in `navigate_to_hover_links` (editor.rs:18275) does:

```rust
let pane = workspace.read(cx).active_pane().clone();
// ... opens target buffer in this pane
```

But `Workspace::active_pane()` (workspace.rs:4864) returns `&self.active_pane`, which is the pane created during `Workspace::new()` — **NOT** the panes managed by `AgentiumWorkspace`.

Agentium does call `workspace.set_last_active_center_pane(pane)` in the `pane::Event::Focus` handler (agentium.rs:816), but this sets `last_active_center_pane` which is a different field from `active_pane`.

The `navigate_to_hover_links` code uses `active_pane()`, not `pane_for_open()` (which does check `last_active_center_pane`).

**For same-file jumps**: This is NOT a problem — the code takes a different branch at line 18261-18264 that doesn't use `workspace` at all:

```rust
if !split && Some(&target_buffer) == editor.buffer.read(cx).as_singleton().as_ref() {
    editor.go_to_singleton_buffer_range(range, window, cx);
    // ... no workspace needed
}
```

### Problem 3: `find_all_references` Uses Different Path

`find_all_references` (editor.rs:18501) accesses the project via:

```rust
let workspace = self.workspace()?;  // editor.workspace field
let project = workspace.read(cx).project().clone();
project.references(...)
```

This should work because `editor.workspace` IS set via `added_to_workspace` (items.rs:995) which is called through `added_to_pane` (items.rs:719), and Agentium does call `item.added_to_pane(workspace, pane, window, cx)` in `handle_pane_event` for `pane::Event::AddItem` (agentium.rs:767-772).

However, the result display for multiple references uses `workspace.open_project_item` which has the same active_pane issue as Problem 2.

---

## How Editor Gets Its `project` and `workspace` Fields

### `project` field

Set during `Editor::new_internal` (editor.rs:2399):

```rust
semantics_provider: project.clone().map(|p| Rc::new(p) as _),
```

When files are opened via `workspace.open_path()`, the Workspace creates the Editor with its project. This is working correctly in Agentium.

### `workspace` field

Set in `Item::added_to_workspace` (items.rs:989-995):

```rust
fn added_to_workspace(&mut self, workspace: &mut Workspace, ...) {
    self.workspace = Some((workspace.weak_handle(), workspace.database_id()));
}
```

Called via the chain: `Pane::Event::AddItem` -> `item.added_to_pane(workspace, pane, window, cx)` -> `added_to_workspace`.

Agentium handles this in `handle_pane_event` (agentium.rs:767-772):

```rust
pane::Event::AddItem { item } => {
    if let Some(workspace) = self.workspace.upgrade() {
        workspace.update(cx, |workspace, cx| {
            item.added_to_pane(workspace, pane.clone(), window, cx)
        });
    }
}
```

This is correct — the editor gets a valid `WeakEntity<Workspace>`.

---

## LSP Server Lifecycle in Detail

### Buffer opened -> LSP starts

```
File opened via workspace.open_path()
  -> BufferStore creates Buffer
  -> BufferStoreEvent::BufferAdded emitted
  -> LspStore::on_buffer_store_event [lsp_store.rs:4307]
  -> LspStore::on_buffer_added [lsp_store.rs:4433]
     -> detect_language_for_buffer
     -> initialize_buffer [lsp_store.rs:2466]
        -> lsp_tree.get(path, language_name, manifest, delegate, cx)
           -> Returns existing server_id if server already running for this language
           -> OR triggers server start via ensure_server [lsp_store.rs:373-397]
              -> languages.lsp_adapters(language_name) — finds registered adapters
              -> start_language_server [lsp_store.rs:400]
                 -> adapter.get_language_server_command(delegate, toolchain, binary_options, cx)
                    -> check_if_user_installed -> cached -> download
                 -> lsp::LanguageServer::new(server_id, binary, ..., cx)
                 -> server.initialize(initialization_params).await
                 -> registers capabilities
```

### Adapter registration (built-in)

`languages::init` (called by Agentium) registers built-in LSP adapters:

- Rust: `RustLspAdapter` — looks for `rust-analyzer` on PATH
- Python: `PyrightLspAdapter` — requires Node.js (won't work in Agentium)
- TypeScript: multiple adapters — require Node.js (won't work)
- C/C++: `CLspAdapter` — looks for `clangd` on PATH
- Go: `GoLspAdapter` — looks for `gopls` on PATH
- And many more...

---

## Zed vs Agentium Initialization Comparison

| Feature | Zed (main.rs) | Agentium (main.rs) | Impact |
|---------|---------------|---------------------|--------|
| `languages::init` (built-in adapters) | Yes (line 505) | Yes (line 169) | Built-in LSP adapters registered |
| `set_language_server_download_dir` | Yes (line 474) | **Missing** | LSP binary download/cache broken |
| `language_extension::init` | Yes (line 509) | **Missing** | Extension-based LSP unavailable |
| `extension_host::init` | Yes | **Missing** | No extension management |
| `NodeRuntime` | Fully configured | `unavailable()` | Node-based LSPs broken |
| `Project::local()` | Yes | Yes | LspStore created correctly |
| `Project::init()` | Yes | Yes | Proto handlers registered |
| `editor::init()` | Yes | Yes | Editor actions registered |
| `workspace::init()` | Yes | Yes | Workspace infrastructure ready |

---

## Required Fixes

### Fix 1: Enable LSP Binary Resolution (Critical)

Add `language_server_download_dir` to `main.rs` after `LanguageRegistry::new`:

```rust
use util::paths;  // may need to add `paths` or `util` dependency

let mut languages = language::LanguageRegistry::new(cx.background_executor().clone());
languages.set_language_server_download_dir(paths::languages_dir().clone());
let languages = Arc::new(languages);
```

Check if `util::paths::languages_dir()` is available. If not, you may need to either:
- Add `paths` as a dependency in Cargo.toml
- Or define a custom path like: `dirs::data_dir().unwrap().join("agentium").join("languages")`

Alternatively, if `rust-analyzer` is already on PATH, the LSP should start without this change. **Test this first by running `which rust-analyzer` in your terminal.**

### Fix 2: Sync Workspace active_pane (Required for Cross-File Navigation)

The core issue: `Workspace::active_pane` is a private field set by `Workspace::set_active_pane()` which is also private. Agentium cannot directly sync it.

Options:

**Option A: Make `Workspace::set_active_pane` accessible**

Add a public method to `Workspace` or make the existing one public. Then call it from `AgentiumWorkspace::handle_pane_event` on `pane::Event::Focus`.

Risk: `set_active_pane` may have side effects (notify, focus change, etc.) that conflict with Agentium's own focus management.

**Option B: Change `navigate_to_hover_links` to use `pane_for_open()`**

In `crates/editor/src/editor.rs:18275`, change:

```rust
// Before
let pane = workspace.read(cx).active_pane().clone();
// After
let pane = workspace.read(cx).pane_for_open();
```

`pane_for_open()` (workspace.rs:4868) checks `last_active_center_pane` first, which IS set by Agentium. This is a smaller, safer change.

Also apply the same change to the multi-location case at line 18188 and other `active_pane()` usages in navigation code.

**Option C: Override navigation behavior in Agentium**

Use `editor.custom_context_menu` or wrap the actions in `AgentiumWorkspace` to intercept navigation and handle pane routing explicitly. This is more complex but avoids modifying shared crates.

### Fix 3: NodeRuntime for Node-based LSPs (Optional)

Only needed if you want TypeScript, Python (Pyright), or other Node.js-based language servers:

```rust
// Replace:
let node_runtime = node_runtime::NodeRuntime::unavailable();
// With a real NodeRuntime initialization (see crates/zed/src/main.rs:502)
```

This also requires a real HTTP client instead of `BlockedHttpClient`.

### Fix 4: Extension LSP Support (Optional)

Call `language_extension::init(...)` to enable extension-based LSP adapters. Requires extension infrastructure setup.

---

## Testing Plan

### Step 1: Verify LSP Server Status

Check if `rust-analyzer` is on PATH:
```bash
which rust-analyzer
```

### Step 2: Test Same-File Go to Definition

Open a Rust file. Define a function, then use it below. Right-click on the usage and select "Go to Definition". This should jump within the same file WITHOUT needing workspace active_pane sync.

If this doesn't work -> LSP server is not starting (Problem 1).
If this works -> LSP is running, Problem 2 is next.

### Step 3: Test Cross-File Go to Definition

Open a Rust file that uses a type from another file. Right-click and "Go to Definition". This requires workspace active_pane fix (Problem 2).

### Step 4: Check Logs

Look for LSP-related log messages:
- `"found user-installed language server"` — LSP binary found on PATH
- `"failed to run rust-analyzer"` — binary found but broken
- `"no language server download dir defined"` — download dir not set
- `"starting language server"` — server is launching

---

## Summary

| Problem | Root Cause | Fix Difficulty | Priority |
|---------|-----------|----------------|----------|
| LSP not starting | `language_server_download_dir` not set + `NodeRuntime::unavailable()` | Easy (1 line for download_dir) | **Critical** |
| Same-file jump broken | LSP not starting (no definitions returned) | Solved by Fix 1 | **Critical** |
| Cross-file jump broken | `Workspace::active_pane` not synced with Agentium's panes | Medium (need to change editor or workspace crate) | **High** |
| Node-based LSPs broken | `NodeRuntime::unavailable()` | Medium | Low (only if needed) |
| Extension LSPs unavailable | `language_extension::init` not called | Medium-High | Low (only if needed) |
