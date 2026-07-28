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

The canonical state lives in `AgentiumApp::claude_sessions: HashMap<String, ClaudeSession>`, where each session has a `ClaudeSessionState` enum (`Idle`, `Running`, `WaitingPermission`, `Completed`). After every mutation, `sync_session_derived_state()` rebuilds the derived PID sets:

- `running_shell_pids` — PIDs of sessions in `Running` state (green dot)
- `permission_shell_pids` — PIDs of sessions in `WaitingPermission` state (orange badge)
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

PR and CI status polling uses the `gh` CLI. If unavailable, all PR/CI features are disabled (`gh_available = false`).

An arena's branch can have **multiple PRs** (same head, different bases like develop/master). `fetch_pr_list` runs `gh pr list --head <branch> --state all` and falls back to `gh pr view` (detached HEAD, cross-fork PRs). `pr_info` holds `Vec<PrInfo>` per arena — never an empty Vec, the key is removed instead, so `contains_key` stays meaningful. CI state is keyed by `(EntityId, pr_number)`.

Data flows in a chain: PR info must exist before CI polling can run. When a fetch inserts **newly-appeared PR numbers** (incoming minus existing), it immediately triggers `fetch_ci_for_arena` for each. Without this chain, CI data would be delayed until the next CI polling cycle (up to 60 seconds).

Both PR and CI polling tasks are dropped on window deactivation and restarted on reactivation. A 5-second timeout guard on any `gh` subprocess permanently disables the polling **loop** for the session (`pr_polling_timed_out` / `ci_polling_timed_out`), but one-shot fetches triggered by explicit events (HeadChanged, arena creation/switch, Claude session completion) are not gated by this flag and still work even after a timeout. Because of this guard, per-PR subprocess work inside one fetch pass (e.g. review fetches) must run **concurrently** (`futures::future::join_all`), never sequentially per PR.

CI polling interval is adaptive based on HEAD commit age: <=1 hour → 60s, <=1 day → 180s, >1 day → 300s. The interval is computed on-the-fly via `compute_ci_poll_interval()` using `repo.head_commit.commit_timestamp`, not cached in a HashMap.

## External CLI probes must wait for the login shell environment

Finder launches start with a minimal PATH (`/usr/bin:/bin:...`), so tools installed via brew/mise/npm (`gh`, `bee`) are not found until `util::load_login_shell_environment()` completes. That load is spawned in `main.rs` and its completion is signaled to `AgentiumApp::new` through a `futures::channel::oneshot` receiver; the combined gh/bee availability probe awaits it before running `--version`. Probing earlier permanently disables the integration for the session with no user-visible error (this actually happened: `bee` lives in mise shims and `bee_available` stayed false). Any new external-CLI availability check must hook the same signal.

## board.json has exactly one writer at a time

The task board (`board::Board`, persisted at `data_dir()/board.json`) follows a strict single-writer rule:

- While an app instance is listening on the IPC socket, **only the app writes** — the CLI resolves selectors/paths locally and sends fully-resolved `TaskCommand`s as `{"type":"task_command",...}` datagrams (`dispatch_task_commands` in `main.rs`).
- The CLI writes the file directly **only** when `connect` fails with `NotFound`/`ConnectionRefused` (no listener). A send failure after a successful connect is an error, never a fallback to direct write — the app has no reload mechanism, so a direct write while it runs would be silently clobbered by the app's next persisted mutation.

macOS caps unix datagrams at 2048 bytes (`net.local.dgram.maxdgram`). Task-command envelopes are kept ≤1900 bytes; an oversized `New` is split into `New` + `AddIssue`×N + `AddArena`×N before sending. `Board::apply` is idempotent (duplicate `New` ids ignored, issues/worktrees deduped) so datagram duplication and CLI retries are safe.

## Sidebar inline editors share one `menu::Confirm`/`Cancel` dispatcher

GPUI actions stop at the first matching handler during bubble dispatch. The sidebar has three inline editors (arena rename, task rename, issue input) and exactly **one** Confirm/Cancel handler pair, on the sidebar column div, which branches on the active editing state. Do not add a second `on_action::<menu::Confirm>` for a new editor — extend the dispatcher, add mutual exclusion in the `start_*` method (each cancels the others), and subscribe to `EditorEvent::Blurred` as **cancel** (matching the existing rename behavior).

## No filesystem calls in sidebar render — cache derived board state

The sidebar re-renders every 2 seconds (`_busy_badge_refresh_task`), so `render` must not call `canonicalize`/`Path::exists`. Path-derived board state (which task worktree maps to a live arena / closed / missing, which arenas are unassigned) lives in `BoardCache`, recomputed by `rebuild_board_cache()` on board mutation (`board_changed`), arena add/remove, startup, and window reactivation. Arena `working_directory` is stored non-canonicalized, so any arena↔worktree comparison must canonicalize both sides (fallback to the raw path on error), following the `save_pr_session_mapping` precedent.

## PR fetch triggers on Claude session completion

`mark_claude_session_ready` (Stop hook handler) triggers `fetch_pr_for_arena` for the arena that owns the completed session's terminal. It uses `find_arena_entity_id_for_pids` to map the session's `ancestor_pids` to the correct arena by scanning each arena's pane items for a matching terminal PID. This ensures PRs created by Claude Code (e.g. via `gh pr create`) appear in the sidebar immediately rather than waiting for the next 60-second polling cycle.

## Caffeinate-based session state cleanup (macOS only)

Claude Code has no hook for turn cancellation (Ctrl+C). When a user cancels mid-turn, no Stop hook fires, leaving the session stuck in `Running`/`WaitingPermission` with a green/orange badge that never clears.

On macOS, Claude Code spawns `caffeinate -i -t 300` as a direct child process only while busy (executing a turn). It is killed with SIGKILL when Claude returns to idle. The absence of a caffeinate child process reliably indicates idle state.

A background monitor task (`_caffeinate_monitor_task`) polls every 3 seconds and checks each `Running`/`WaitingPermission` session:
- `ancestor_pids[0]` is the Claude Code node process PID (the hook process's parent)
- `libc::kill(pid, 0)` checks if the Claude process is alive — if dead, the session entry is **removed** from `claude_sessions` (not merely transitioned to `Idle`), so its wrapper PID is released from `pid_to_session_id` and the terminal becomes eligible for the busy-non-Claude badge again. This is the safety net for hard kills (SIGKILL, terminal close, crash) where the `SessionEnd` hook does not fire.
- `pgrep -P <pid> caffeinate` checks for a caffeinate child — if absent for >5 seconds (`caffeinate_absent_since` HashMap), transition to `Idle` (the session is **kept** because the next turn may revive it).

The 5-second grace period avoids false positives at turn boundaries, where caffeinate is briefly absent between the `UserPromptSubmit` hook and the moment Claude spawns a new caffeinate for the next turn (typically <1 second).

Clean Claude exits (`/exit`, `Ctrl+D`, idle `Ctrl+C`, `/logout`, `/clear`) flow through the `SessionEnd` hook → `handle_claude_session_end`, which also removes the session entry. The caffeinate-based path is the fallback for cases where `SessionEnd` cannot run.

## `fallback_pid` is not the shell's live PID

`ProcessIdGetter::fallback_pid` is captured from `pty.child().id()` at PTY spawn time. In practice this is a **wrapper process** PID, not the interactive shell's PID. Empirically on macOS/zsh, `tcgetpgrp(pty_fd)` returns a different PID (the actual shell) even when the shell is idle at a prompt.

Implication: **do not use `terminal.pid() != getter.fallback_pid()` to detect "something is running"**. This was tried for a busy-terminal indicator and always evaluated `true` — every zsh terminal looked busy. Use process-name comparison instead (see next rule). The `fallback_pid` is still useful as a stable identifier for the terminal (Claude session hooks are keyed to it), just not for foreground-state detection.

## `Terminal::foreground_process_name()` is a stale cache

`foreground_process_name()` reads `PtyProcessInfo.current`, a `RwLock<Option<ProcessInfo>>` that is only written by `emit_title_changed_if_changed()`. That function is only invoked from terminal event handlers — which do not fire for silent commands like `sleep 30` or `wait`. Verified empirically via eprintln: `foreground_process_name()` stays at `"zsh"` for the full 30 seconds of a `sleep 30`.

For features that need a live foreground process name, own a `sysinfo::System` directly and call `refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always))` with the target PIDs. Batching by known PIDs keeps the cost proportional to the number of terminals, not the number of processes on the machine. See `count_busy_non_claude_terminals_in_arena` in `agentium.rs` for the pattern.

## "Is this a Claude terminal?" uses `pid_to_session_id`, not state-specific sets

For checks of the form "does this shell host a Claude Code session?", use `session_state.pid_to_session_id` — it contains every session regardless of state (Idle / Running / WaitingPermission / Completed). The state-specific sets (`running_shell_pids`, `permission_shell_pids`, `ready_shell_pids`) each miss at least one legitimate Claude state; using them to "exclude Claude terminals" would wrongly count idle Claude terminals as non-Claude.

Entries are removed from `pid_to_session_id` (via `claude_sessions.remove()` + `sync_session_derived_state()`) only when Claude actually exits — either through the `SessionEnd` hook handler or the caffeinate monitor's `kill(pid, 0)` failure. So a terminal that *was* a Claude session but has since been quit becomes eligible for the busy-non-Claude badge again.

## `HeadChanged` fires on commits, not just branch switches

`GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _)` fires on **every** HEAD movement — `git commit`, `git rebase`, `git reset`, etc., not only branch switches. If a feature needs "branch switched" semantics (e.g. invalidating a flag), compare the current branch name against a stored previous name and act only on change. See the `last_branch_names` tracking used by the PR-session dirty flag in `AgentiumApp::new`.

## Sidebar needs periodic `cx.notify()` for non-event-driven state

AgentiumApp's sidebar re-renders only when `cx.notify()` fires on the AgentiumApp entity. There is no GPUI event for changes in `tcgetpgrp`, `sysinfo`, or similar polled OS state — so any sidebar indicator derived from such state needs a background task calling `cx.notify()` on a timer. See `_busy_badge_refresh_task` (2s cadence) and `_rate_limits_refresh_task` (30s cadence) for the pattern. Emit-title-changed → UpdateTab chains from the terminal crate will NOT fire for silent commands; don't rely on them.

## TERM_PROGRAM is derived from the binary name

`terminal::insert_zed_terminal_env` uses `std::env::current_exe()` to determine the value of `TERM_PROGRAM`. When the running binary is `agentium` (including inside `Agentium.app/Contents/MacOS/agentium`), terminals get `TERM_PROGRAM=agentium`. When running as `zed`, they get `TERM_PROGRAM=zed`. This is important for tools like Claude Code that inspect `TERM_PROGRAM` to detect the host terminal.
