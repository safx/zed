# Agentium crate rules

## Arena vs Workspace terminology

Agentium uses two distinct concepts that must not be confused:

- **Arena** (`Arena` struct in `arena.rs`): An Agentium-specific concept representing an isolated work area for an AI agent. Each arena has its own name, pane layout, working directory, and active pane. Multiple arenas are listed in the left sidebar and can be switched between. This is the unit the user creates via "+ New Arena".

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

`AgentiumApp::render()` also sets `.key_context("Agentium")` on its root div. This context is always in the dispatch path regardless of focus location (sidebar or arena). Agentium-specific keybindings that must work from anywhere (e.g. `cmd-1`...`cmd-9` for arena switching) are bound to the `"Agentium"` context in `main.rs`. GPUI uses depth-based precedence — deeper contexts win — so bindings that need to override default `"Workspace"`-scoped bindings inside arenas must also be registered at the `"Workspace"` context level.

## Claude Code hook IPC uses `Rc<RefCell<HashSet<u32>>>` for cross-closure state

`SharedSessionState` holds several `Rc<RefCell<...>>` sets shared between `AgentiumApp` (which writes them) and per-pane indicator closures (which read them to decide dot colors). Because all access is on the single foreground thread, `Rc<RefCell<...>>` is used instead of `Arc<Mutex<...>>`.

The canonical state lives in `AgentiumApp::claude_sessions: HashMap<String, ClaudeSession>`, where each session has a `ClaudeSessionState` enum (`Idle`, `Running`, `Completed`). After every mutation, `sync_session_derived_state()` rebuilds the derived PID sets:

- `running_shell_pids` — PIDs of sessions in `Running` state (green dot)
- `ready_shell_pids` — PIDs of sessions in `Completed` state (blue dot + border)
- `acknowledged_task_pids` — PIDs of non-Claude task terminals the user has acknowledged via key input (suppresses dot)
- `pid_to_session_id` — all sessions, for Fork Session lookups

Clearing Claude session state (Completed → Idle) routes through `AgentiumApp` via `ArenaEvent::TerminalKeyInput` because the canonical `claude_sessions` map lives there. Non-Claude task acknowledgment is handled locally in `Arena::handle_key_input` by inserting into `acknowledged_task_pids` directly — no upstream state needs syncing.

## Statusline command must pass through stdin before parsing

`agentium claude statusline` is a Claude Code statusLine command. The protocol requires that stdin is echoed to stdout verbatim — Claude Code reads stdout to verify the command is working. The implementation must: (1) read all stdin, (2) write it to stdout and flush (stdout is a pipe, so explicit flush is required before process exit), (3) only then parse JSON and send rate limit data via IPC. If parsing fails, the stdout pass-through must still have succeeded.

## IPC protocol distinguishes messages by first byte

The Unix datagram socket at `agentium.sock` carries two message formats: raw UTF-8 paths (for `agentium arena new`) and JSON objects (for `agentium claude hook`). The receiver distinguishes them by checking whether the first byte is `{`. This works because UNIX paths start with `/`, never `{`.

## LSP requires `language_server_download_dir`

`LanguageRegistry::set_language_server_download_dir` must be called before `languages::init`. Without it, LSP servers that aren't already on PATH cannot be downloaded or cached. Note that Node.js-based LSP servers (TypeScript, Pyright) also require a real `NodeRuntime`, which Agentium currently does not provide (`NodeRuntime::unavailable()`).

## Cross-file navigation uses `pane_for_open()` not `active_pane()`

Zed's `editor.rs` navigation code (`navigate_to_hover_links`, `find_all_references`, `open_locations_in_multibuffer`) was changed to use `workspace.pane_for_open()` instead of `workspace.active_pane()`. This is necessary because `Workspace::active_pane` points to its own internal pane (not rendered in Agentium), while `pane_for_open()` checks `last_active_center_pane` first, which Agentium sets via `Workspace::set_last_active_center_pane` on `pane::Event::Focus`.

## macOS app bundle uses `cargo bundle` (Zed fork)

`Agentium.app` is built via `script/bundle-agentium`, which uses the Zed fork of `cargo-bundle` (`cargo-bundle v0.6.1-zed` from `zed-industries/cargo-bundle`, branch `zed-deploy`). The bundle metadata is in `[package.metadata.bundle]` in `Cargo.toml` — not channel-suffixed like Zed's `bundle-dev`/`bundle-stable`, since Agentium has no release channels.

Resources at `crates/agentium/resources/info/` contain plist fragments that `cargo bundle` merges into `Info.plist`:
- `Permissions.plist` — NS*UsageDescription keys (camera, mic, location, etc.)
- `SupportedPlatforms.plist` — `CFBundleSupportedPlatforms: [MacOSX]`

Agentium does **not** set `ZED_BUNDLE=true` at build time. This env var controls whether the `git` crate looks for a bundled git binary at `Contents/MacOS/git`. Since Agentium does not bundle git, the system git is used instead.

## Modal layer is rendered by AgentiumApp, not Arena

The `ModalLayer` entity (from the shared `Workspace`) is rendered as a child of `AgentiumApp::render()`, not inside `Arena::render()`. This is because `AgentiumApp` can have zero arenas (initial state, all arenas closed), and modals must still work. If the modal layer were only rendered inside Arena, `workspace.toggle_modal()` would add the modal to the entity but it would have nowhere to render when no arenas exist.

When adding new modal-triggering features, do not assume an active arena exists. The modal layer is always available via `self.workspace_entity.read(cx).modal_layer()`.

Note: The recent projects picker ("+ New Arena") uses a `PopoverMenu` anchored to the button, not `ModalLayer`. But other features may still use the modal layer.

## Workspace `database_id` is None — DB writes must be explicit

Agentium creates its `Workspace` entity with `Workspace::new(None, ...)`, so `database_id` is always `None`. Zed's `serialize_workspace_internal()` checks `self.database_id()` and returns immediately if `None`, meaning **no automatic persistence occurs** — not for worktrees, panes, dock state, or recent projects.

Any feature that needs data in `WorkspaceDb` (e.g. recent projects list) must write to the DB explicitly using `WorkspaceDb::global(cx)` methods like `save_local_workspace_paths()`. Do not rely on Zed's event-driven serialization (`WorktreeAdded` → `serialize_workspace()`) — it is a no-op in Agentium.

## GitHub PR and CI polling requires `gh` CLI and chains data dependencies

PR and CI status polling uses the `gh` CLI. At startup, `gh --version` is checked; if unavailable, all PR/CI features are disabled (`gh_available = false`).

Data flows in a chain: PR info must exist before CI polling can run. When `fetch_pr_for_arena` or the PR polling loop inserts a **new** PR entry (`!had_pr`), it immediately triggers `fetch_ci_for_arena` for that arena. Without this chain, CI data would be delayed until the next CI polling cycle (up to 60 seconds).

Both PR and CI polling tasks are dropped on window deactivation and restarted on reactivation. A 5-second timeout guard on any `gh` subprocess permanently disables the respective polling loop for the session (`pr_polling_timed_out` / `ci_polling_timed_out`).

CI polling interval is adaptive based on HEAD commit age: <=1 hour → 60s, <=1 day → 180s, >1 day → 300s. The interval is computed on-the-fly via `compute_ci_poll_interval()` using `repo.head_commit.commit_timestamp`, not cached in a HashMap.

## TERM_PROGRAM is derived from the binary name

`terminal::insert_zed_terminal_env` uses `std::env::current_exe()` to determine the value of `TERM_PROGRAM`. When the running binary is `agentium` (including inside `Agentium.app/Contents/MacOS/agentium`), terminals get `TERM_PROGRAM=agentium`. When running as `zed`, they get `TERM_PROGRAM=zed`. This is important for tools like Claude Code that inspect `TERM_PROGRAM` to detect the host terminal.
