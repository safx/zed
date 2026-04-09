mod arena;
mod file_browser_view;
mod git_status_view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use editor::{Editor, EditorEvent};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{prelude::*, *};
use picker::{
    Picker, PickerDelegate,
    highlighted_match_with_paths::{HighlightedMatch, HighlightedMatchWithPaths},
};
use project::Project;
use project::git_store::{GitStoreEvent, RepositoryEvent};
use terminal_view::TerminalView;
use ui::{
    ActiveTheme, ButtonStyle, ContextMenu, ListItem, ListItemSpacing, PopoverMenu, Tooltip,
    prelude::*,
};
use ui_input::ErasedEditor;
use util::{ResultExt as _, paths::PathExt};
use workspace::{
    AppState, Pane, PathList, SerializedWorkspaceLocation, SplitDirection, Workspace,
    WorkspaceDb, WorkspaceId, ZoomOut,
};

use arena::{Arena, ArenaEvent};

pub enum PaneContentType {
    Terminal,
    Diff,
    BranchDiff,
    GitStatus,
    ProjectSearch,
    FileBrowser,
    GitGraph,
}

actions!(agentium, [NewClaudeCode, NewDiffView, NewBranchDiff, NewProjectSearch, NewGitStatus, NewFileBrowser, NewGitGraph]);

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = agentium)]
pub(crate) struct ForkClaudeSession {
    pub session_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = agentium)]
pub struct ActivateArena {
    pub index: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum ClaudeSessionState {
    Idle,
    Running,
    WaitingPermission,
    Completed,
}

struct ClaudeSession {
    ancestor_pids: Vec<u32>,
    state: ClaudeSessionState,
    user_prompt: String,
    status_message: String,
}

struct RateLimitEntry {
    used_pct: f32,
    resets_at: i64,
}

struct RateLimits {
    five_hour: RateLimitEntry,
    seven_day: RateLimitEntry,
    received_at: Instant,
}

struct ReadyTerminalInfo {
    pane: Entity<Pane>,
    terminal_view: Entity<TerminalView>,
    user_prompt: String,
    status_message: String,
}

#[derive(Clone)]
pub(crate) struct SharedSessionState {
    pub ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub running_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub permission_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub acknowledged_task_pids: Rc<RefCell<HashSet<u32>>>,
    pub pid_to_session_id: Rc<RefCell<HashMap<u32, String>>>,
}

impl SharedSessionState {
    fn new() -> Self {
        Self {
            ready_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            running_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            permission_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            acknowledged_task_pids: Rc::new(RefCell::new(HashSet::new())),
            pid_to_session_id: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum PrStatus {
    Draft,
    Open,
    Merged,
    Closed,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq)]
enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Debug, PartialEq)]
enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Clone, Debug)]
struct ReviewEntry {
    user: SharedString,
    state: ReviewState,
    commit_oid: SharedString,
    avatar_url: SharedString,
    submitted_at: SharedString,
}

#[derive(Clone, Debug)]
struct PrInfo {
    number: u32,
    title: SharedString,
    status: PrStatus,
    html_url: SharedString,
    review_decision: Option<ReviewDecision>,
    review_count: usize,
    head_sha: SharedString,
    reviews: Vec<ReviewEntry>,
}

#[derive(Clone, Debug, PartialEq)]
enum CiStatus {
    AllPassed,
    Failed,
    PendingWithFailure,
    PendingClean,
}

#[derive(Clone, Debug)]
struct CiCheckEntry {
    name: SharedString,
    bucket: SharedString,
}

#[derive(Clone, Debug)]
struct CiInfo {
    status: CiStatus,
    checks: Vec<CiCheckEntry>,
}

pub struct AgentiumApp {
    arenas: Vec<Entity<Arena>>,
    active_arena_index: Option<usize>,
    workspace_entity: Entity<Workspace>,
    project: Entity<Project>,
    #[allow(dead_code)]
    app_state: Arc<AppState>,
    next_arena_id: usize,
    focus_handle: FocusHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    badge_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    rename_editor: Entity<Editor>,
    renaming_arena: Option<Entity<Arena>>,
    pub(crate) claude_sessions: HashMap<String, ClaudeSession>,
    session_state: SharedSessionState,
    should_move_window: bool,
    rate_limits: Option<RateLimits>,
    _rate_limits_refresh_task: Option<Task<()>>,
    gh_available: bool,
    pr_info: HashMap<EntityId, PrInfo>,
    pr_last_checked: HashMap<EntityId, Instant>,
    pr_polling_timed_out: bool,
    _pr_poll_task: Option<Task<()>>,
    ci_status: HashMap<EntityId, CiInfo>,
    ci_last_checked: HashMap<EntityId, Instant>,
    ci_polling_timed_out: bool,
    _ci_poll_task: Option<Task<()>>,
    _window_activation_subscription: gpui::Subscription,
    _git_subscription: gpui::Subscription,
    _arena_subscriptions: HashMap<EntityId, gpui::Subscription>,
}

impl AgentiumApp {
    pub fn new(
        workspace_entity: Entity<Workspace>,
        project: Entity<Project>,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let git_subscription = cx.subscribe(&git_store, |this, _, event: &GitStoreEvent, cx| {
            if let GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _) = event {
                if let Some(arena) = this.active_arena().cloned() {
                    let entity_id = arena.entity_id();
                    this.pr_info.remove(&entity_id);
                    this.ci_status.remove(&entity_id);
                    if this.gh_available && !this.pr_polling_timed_out {
                        this.fetch_pr_for_arena(entity_id, cx);
                    }
                }
            }
            cx.notify();
        });

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        cx.subscribe_in(&rename_editor, window, |this: &mut Self, _, event: &EditorEvent, _window, cx| {
            if let EditorEvent::Blurred = event {
                this.finish_rename_arena(false, cx);
            }
        }).detach();

        let window_activation_subscription =
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    if this.gh_available
                        && this._pr_poll_task.is_none()
                        && !this.pr_polling_timed_out
                    {
                        this.start_pr_polling(cx);
                    }
                    if this.gh_available
                        && this._ci_poll_task.is_none()
                        && !this.ci_polling_timed_out
                    {
                        this.start_ci_polling(cx);
                    }
                    // Re-render so rate limit rows can detect expired reset times
                    // and hide progress bars immediately on app reactivation.
                    if this.rate_limits.is_some() {
                        cx.notify();
                    }
                } else {
                    this._pr_poll_task = None;
                    this._ci_poll_task = None;
                }
            });

        cx.spawn(async move |this, cx| {
            let available = cx
                .background_executor()
                .spawn(async {
                    smol::process::Command::new("gh")
                        .args(&["--version"])
                        .output()
                        .await
                        .map(|output| output.status.success())
                        .unwrap_or(false)
                })
                .await;
            this.update(cx, |this, cx| {
                this.gh_available = available;
                if available {
                    this.start_pr_polling(cx);
                    this.start_ci_polling(cx);
                }
            })
            .ok();
        })
        .detach();

        Self {
            arenas: Vec::new(),
            active_arena_index: None,
            workspace_entity,
            project,
            app_state,
            next_arena_id: 0,
            focus_handle: cx.focus_handle(),
            context_menu: None,
            badge_menu: None,
            rename_editor,
            renaming_arena: None,
            claude_sessions: HashMap::new(),
            should_move_window: false,
            rate_limits: None,
            _rate_limits_refresh_task: None,
            session_state: SharedSessionState::new(),
            gh_available: false,
            pr_info: HashMap::new(),
            pr_last_checked: HashMap::new(),
            pr_polling_timed_out: false,
            _pr_poll_task: None,
            ci_status: HashMap::new(),
            ci_last_checked: HashMap::new(),
            ci_polling_timed_out: false,
            _ci_poll_task: None,
            _window_activation_subscription: window_activation_subscription,
            _git_subscription: git_subscription,
            _arena_subscriptions: HashMap::new(),
        }
    }

    pub fn add_arena_with_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project
            .update(cx, |project, cx| {
                project.create_worktree(&path, true, cx)
            })
            .detach_and_log_err(cx);

        let db = WorkspaceDb::global(cx);
        let paths = vec![path.clone()];
        cx.background_spawn(async move {
            db.save_local_workspace_paths(&paths).await;
        })
        .detach();

        self.add_arena_inner(Some(path), window, cx);
    }




    fn add_arena_inner(
        &mut self,
        working_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let arena_id = self.next_arena_id;
        self.next_arena_id += 1;

        let name = self.resolve_arena_name(working_directory.as_deref(), arena_id, cx);

        let workspace_weak = self.workspace_entity.downgrade();
        let project = self.project.clone();
        let session_state = self.session_state.clone();

        let arena_entity = cx.new(|cx| {
            Arena::new(arena_id, name, workspace_weak, project, working_directory, session_state, window, cx)
        });

        let subscription = cx.subscribe(
            &arena_entity,
            |this, _ws, event: &ArenaEvent, cx| match event {
                // Claude session clearing routes through AgentiumApp because
                // claude_sessions state lives here. Non-Claude task acknowledgment
                // is handled locally in Arena::handle_key_input via the shared
                // acknowledged_task_pids set.
                ArenaEvent::TerminalKeyInput { shell_pid } => {
                    this.clear_session_for_shell_pid(*shell_pid, cx);
                }
            },
        );
        self._arena_subscriptions
            .insert(arena_entity.entity_id(), subscription);

        self.arenas.push(arena_entity.clone());
        self.active_arena_index = Some(self.arenas.len() - 1);
        arena_entity.update(cx, |arena, cx| {
            arena.activate_context(cx);
        });
        if self.gh_available && !self.pr_polling_timed_out {
            self.fetch_pr_for_arena(arena_entity.entity_id(), cx);
        }
        let focus = arena_entity.focus_handle(cx);
        focus.focus(window, cx);
        cx.notify();
    }

    fn switch_arena(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.arenas.len() {
            self.active_arena_index = Some(index);
            self.arenas[index].update(cx, |arena, cx| {
                arena.activate_context(cx);
            });
            if self.gh_available {
                let entity_id = self.arenas[index].entity_id();
                if !self.pr_polling_timed_out {
                    let pr_stale = self
                        .pr_last_checked
                        .get(&entity_id)
                        .map_or(true, |t| t.elapsed() > Duration::from_secs(60));
                    if pr_stale {
                        self.fetch_pr_for_arena(entity_id, cx);
                    }
                }
                if !self.ci_polling_timed_out {
                    if let Some(pr) = self.pr_info.get(&entity_id) {
                        let pr_number = pr.number;
                        let ci_stale = self
                            .ci_last_checked
                            .get(&entity_id)
                            .map_or(true, |t| t.elapsed() > Duration::from_secs(60));
                        if ci_stale {
                            self.fetch_ci_for_arena(entity_id, pr_number, cx);
                        }
                    }
                }
            }
            let focus = self.arenas[index].focus_handle(cx);
            focus.focus(window, cx);
            cx.notify();
        }
    }

    fn active_arena(&self) -> Option<&Entity<Arena>> {
        self.active_arena_index
            .and_then(|i| self.arenas.get(i))
    }

    fn start_pr_polling(&mut self, cx: &mut Context<Self>) {
        self._pr_poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let fetch_info = this
                    .update(cx, |this, cx| {
                        this.active_arena().and_then(|arena| {
                            let arena_ref = arena.read(cx);
                            let working_dir = arena_ref.working_directory.clone()?;
                            Some((arena.entity_id(), working_dir))
                        })
                    })
                    .ok()
                    .flatten();

                if let Some((entity_id, working_dir)) = fetch_info {
                    let start = Instant::now();
                    let result = cx
                        .background_executor()
                        .spawn(async move { fetch_pr_info(&working_dir).await })
                        .await;
                    let elapsed = start.elapsed();

                    let should_stop = this
                        .update(cx, |this, cx| {
                            if elapsed > Duration::from_secs(5) {
                                this.pr_polling_timed_out = true;
                                this._pr_poll_task = None;
                                return true;
                            }
                            this.pr_last_checked.insert(entity_id, Instant::now());
                            match result {
                                Ok(Some(info)) => {
                                    let pr_number = info.number;
                                    let had_pr = this.pr_info.contains_key(&entity_id);
                                    this.pr_info.insert(entity_id, info);
                                    if !had_pr && !this.ci_polling_timed_out {
                                        this.fetch_ci_for_arena(entity_id, pr_number, cx);
                                    }
                                }
                                Ok(None) | Err(_) => {
                                    this.pr_info.remove(&entity_id);
                                }
                            }
                            cx.notify();
                            false
                        })
                        .unwrap_or(true);

                    if should_stop {
                        break;
                    }
                }

                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;

                if this.update(cx, |_, _| {}).is_err() {
                    break;
                }
            }
        }));
    }

    fn fetch_pr_for_arena(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        let working_dir = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)
            .and_then(|a| a.read(cx).working_directory.clone());
        let Some(working_dir) = working_dir else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch_pr_info(&working_dir).await })
                .await;
            this.update(cx, |this, cx| {
                this.pr_last_checked.insert(entity_id, Instant::now());
                match result {
                    Ok(Some(info)) => {
                        let pr_number = info.number;
                        let had_pr = this.pr_info.contains_key(&entity_id);
                        this.pr_info.insert(entity_id, info);
                        if !had_pr && !this.ci_polling_timed_out {
                            this.fetch_ci_for_arena(entity_id, pr_number, cx);
                        }
                    }
                    Ok(None) | Err(_) => {
                        this.pr_info.remove(&entity_id);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_ci_polling(&mut self, cx: &mut Context<Self>) {
        self._ci_poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let targets = this
                    .update(cx, |this, cx| {
                        let mut targets = Vec::new();
                        for arena in &this.arenas {
                            let entity_id = arena.entity_id();
                            let Some(pr) = this.pr_info.get(&entity_id) else {
                                continue;
                            };
                            if matches!(pr.status, PrStatus::Merged) {
                                continue;
                            }
                            let pr_number = pr.number;

                            let interval = this.compute_ci_poll_interval(entity_id, cx);
                            let last = this.ci_last_checked.get(&entity_id).copied();
                            let due = last.map_or(true, |t| t.elapsed() >= interval);

                            if due {
                                if let Some(working_dir) =
                                    arena.read(cx).working_directory.clone()
                                {
                                    targets.push((entity_id, working_dir, pr_number));
                                }
                            }
                        }
                        targets
                    })
                    .unwrap_or_default();

                let mut should_stop = false;
                for (entity_id, working_dir, pr_number) in targets {
                    let start = Instant::now();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            fetch_ci_status(&working_dir, pr_number).await
                        })
                        .await;
                    let elapsed = start.elapsed();

                    should_stop = this
                        .update(cx, |this, cx| {
                            if elapsed > Duration::from_secs(5) {
                                this.ci_polling_timed_out = true;
                                this._ci_poll_task = None;
                                return true;
                            }
                            this.ci_last_checked.insert(entity_id, Instant::now());
                            match result {
                                Ok(Some(status)) => {
                                    this.ci_status.insert(entity_id, status);
                                }
                                Ok(None) | Err(_) => {
                                    this.ci_status.remove(&entity_id);
                                }
                            }
                            cx.notify();
                            false
                        })
                        .unwrap_or(true);

                    if should_stop {
                        break;
                    }
                }

                if should_stop {
                    break;
                }

                let sleep_duration = this
                    .update(cx, |this, cx| {
                        let mut min_remaining = Duration::from_secs(60);
                        for arena in &this.arenas {
                            let entity_id = arena.entity_id();
                            let dominated_by_merged = this.pr_info.get(&entity_id)
                                .map_or(true, |pr| matches!(pr.status, PrStatus::Merged));
                            if !dominated_by_merged {
                                let interval =
                                    this.compute_ci_poll_interval(entity_id, cx);
                                let elapsed = this
                                    .ci_last_checked
                                    .get(&entity_id)
                                    .map_or(Duration::ZERO, |t| t.elapsed());
                                let remaining = interval.saturating_sub(elapsed);
                                min_remaining = min_remaining.min(remaining);
                            }
                        }
                        min_remaining.max(Duration::from_secs(10))
                    })
                    .unwrap_or(Duration::from_secs(60));

                cx.background_executor().timer(sleep_duration).await;

                if this.update(cx, |_, _| {}).is_err() {
                    break;
                }
            }
        }));
    }

    fn fetch_ci_for_arena(
        &mut self,
        entity_id: EntityId,
        pr_number: u32,
        cx: &mut Context<Self>,
    ) {
        let working_dir = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)
            .and_then(|a| a.read(cx).working_directory.clone());
        let Some(working_dir) = working_dir else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch_ci_status(&working_dir, pr_number).await })
                .await;
            this.update(cx, |this, cx| {
                this.ci_last_checked.insert(entity_id, Instant::now());
                match result {
                    Ok(Some(status)) => {
                        this.ci_status.insert(entity_id, status);
                    }
                    Ok(None) | Err(_) => {
                        this.ci_status.remove(&entity_id);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn compute_ci_poll_interval(&self, entity_id: EntityId, cx: &App) -> Duration {
        let timestamp = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)
            .and_then(|arena| {
                let working_dir = arena.read(cx).working_directory.as_ref()?;
                let git_store = self.project.read(cx).git_store().read(cx);
                git_store
                    .repositories()
                    .values()
                    .filter_map(|repo| {
                        let repo = repo.read(cx);
                        if working_dir.starts_with(repo.work_directory_abs_path.as_ref()) {
                            repo.head_commit.as_ref().map(|c| {
                                (repo.work_directory_abs_path.clone(), c.commit_timestamp)
                            })
                        } else {
                            None
                        }
                    })
                    .max_by_key(|(path, _)| path.clone())
                    .map(|(_, ts)| ts)
            });

        let Some(timestamp) = timestamp else {
            return Duration::from_secs(60);
        };

        let now = chrono::Utc::now().timestamp();
        let age_secs = now - timestamp;
        if age_secs <= 3600 {
            Duration::from_secs(60)
        } else if age_secs <= 86400 {
            Duration::from_secs(180)
        } else {
            Duration::from_secs(300)
        }
    }

    fn resolve_arena_name(
        &self,
        working_directory: Option<&std::path::Path>,
        fallback_id: usize,
        cx: &App,
    ) -> String {
        let effective_path = working_directory
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                self.project
                    .read(cx)
                    .worktrees(cx)
                    .next()
                    .map(|wt| wt.read(cx).abs_path().to_path_buf())
            });

        let Some(effective_path) = effective_path else {
            return format!("Arena {}", fallback_id + 1);
        };

        let git_store = self.project.read(cx).git_store().read(cx);
        let matching_repo = git_store.repositories().values().find(|repo| {
            let repo_path = &repo.read(cx).work_directory_abs_path;
            effective_path.starts_with(repo_path.as_ref())
        });

        if let Some(repo) = matching_repo {
            let repo = repo.read(cx);

            if let Some(name) = repo.remote_origin_url.as_deref().and_then(repo_name_from_url) {
                return name.to_string();
            }
        }

        let worktree_name = self.project.read(cx).worktrees(cx).find_map(|wt| {
            let wt = wt.read(cx);
            let wt_path = wt.abs_path();
            if effective_path.starts_with(wt_path.as_ref())
            {
                let name = wt.root_name_str();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
            None
        });

        if let Some(name) = worktree_name {
            return name;
        }

        if let Some(repo) = matching_repo {
            let repo_path = &repo.read(cx).work_directory_abs_path;
            if let Some(name) = repo_path.file_name() {
                return name.to_string_lossy().to_string();
            }
        }

        if let Some(name) = effective_path.file_name() {
            let name = name.to_string_lossy();
            if !name.is_empty() {
                return name.to_string();
            }
        }

        format!("Arena {}", fallback_id + 1)
    }

    fn deploy_arena_context_menu(
        &mut self,
        arena_entity: Entity<Arena>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let arena_for_rename = arena_entity.clone();
        let arena_for_close = arena_entity;
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            menu.entry(
                "Rename…",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.start_rename_arena(arena_for_rename.clone(), window, cx);
                }),
            )
            .separator()
            .entry(
                "Close",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.remove_arena(&arena_for_close, window, cx);
                }),
            )
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn start_rename_arena(
        &mut self,
        workspace: Entity<Arena>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = workspace.read(cx).name.clone();
        self.renaming_arena = Some(workspace);
        self.rename_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&Default::default(), window, cx);
        });
        window.focus(&self.rename_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn finish_rename_arena(&mut self, accept: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.renaming_arena.take() else { return };
        if accept {
            let new_name = self.rename_editor.read(cx).text(cx);
            if !new_name.trim().is_empty() {
                workspace.update(cx, |ws, _| ws.name = new_name);
            }
        }
        cx.notify();
    }

    fn remove_arena(
        &mut self,
        workspace: &Entity<Arena>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.arenas.iter().position(|ws| ws == workspace) else { return };
        let entity_id = workspace.entity_id();
        self._arena_subscriptions.remove(&entity_id);
        self.pr_info.remove(&entity_id);
        self.pr_last_checked.remove(&entity_id);
        self.ci_status.remove(&entity_id);
        self.ci_last_checked.remove(&entity_id);
        self.arenas.remove(index);

        if self.arenas.is_empty() {
            self.active_arena_index = None;
        } else if let Some(active) = self.active_arena_index {
            if active == index {
                self.active_arena_index = Some(active.min(self.arenas.len() - 1));
            } else if active > index {
                self.active_arena_index = Some(active - 1);
            }
        }

        if self.renaming_arena.as_ref() == Some(workspace) {
            self.renaming_arena = None;
        }

        if let Some(ws) = self.active_arena().cloned() {
            ws.update(cx, |arena, cx| {
                arena.activate_context(cx);
            });
            let focus = ws.focus_handle(cx);
            focus.focus(window, cx);
        } else {
            self.workspace_entity.update(cx, |ws, cx| {
                ws.clear_active_worktree_override(cx);
            });
        }
        cx.notify();
    }

    fn sync_session_derived_state(&self) {
        let mut ready_pids = HashSet::new();
        let mut running_pids = HashSet::new();
        let mut permission_pids = HashSet::new();
        for session in self.claude_sessions.values() {
            match session.state {
                ClaudeSessionState::Completed => {
                    ready_pids.extend(session.ancestor_pids.iter().copied());
                }
                ClaudeSessionState::Running => {
                    running_pids.extend(session.ancestor_pids.iter().copied());
                }
                ClaudeSessionState::WaitingPermission => {
                    permission_pids.extend(session.ancestor_pids.iter().copied());
                }
                ClaudeSessionState::Idle => {}
            }
        }
        *self.session_state.ready_shell_pids.borrow_mut() = ready_pids;
        *self.session_state.running_shell_pids.borrow_mut() = running_pids;
        *self.session_state.permission_shell_pids.borrow_mut() = permission_pids;

        // Includes all sessions (not just ready ones) so that Fork Session
        // is available while Claude is still running.
        let mut pid_map = self.session_state.pid_to_session_id.borrow_mut();
        pid_map.clear();
        for (session_id, session) in &self.claude_sessions {
            for &pid in &session.ancestor_pids {
                pid_map.insert(pid, session_id.clone());
            }
        }
    }

    pub fn register_claude_session(
        &mut self,
        session_id: String,
        ancestor_pids: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        // Use or_insert to avoid overwriting a session that was already
        // created by a UserPromptSubmit arriving before this SessionStart
        // (the two hooks run as separate IPC processes with no ordering guarantee).
        let session = self
            .claude_sessions
            .entry(session_id)
            .or_insert_with(|| ClaudeSession {
                ancestor_pids: Vec::new(),
                state: ClaudeSessionState::Idle,
                user_prompt: String::new(),
                status_message: String::new(),
            });
        session.ancestor_pids = ancestor_pids;
        self.sync_session_derived_state();
        cx.notify();
    }

    pub fn mark_claude_session_ready(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let status_message = title.unwrap_or_else(|| "Ready".to_string());
        if let Some(session) = self.claude_sessions.get_mut(session_id) {
            session.state = ClaudeSessionState::Completed;
            session.ancestor_pids = ancestor_pids;
            session.status_message = status_message;
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    state: ClaudeSessionState::Completed,
                    user_prompt: String::new(),
                    status_message,
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        cx.notify();
    }

    pub fn handle_claude_notification(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.claude_sessions.get_mut(session_id) {
            session.ancestor_pids = ancestor_pids;
            session.status_message = title;
            // Don't overwrite Running → Completed for notifications;
            // only upgrade Idle → Completed so the blue badge appears.
            if session.state == ClaudeSessionState::Idle {
                session.state = ClaudeSessionState::Completed;
            }
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    state: ClaudeSessionState::Completed,
                    user_prompt: String::new(),
                    status_message: title,
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        cx.notify();
    }

    pub fn set_claude_session_prompt(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.claude_sessions.get_mut(session_id) {
            session.user_prompt = prompt;
            session.ancestor_pids = ancestor_pids;
            session.state = ClaudeSessionState::Running;
            session.status_message.clear();
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    state: ClaudeSessionState::Running,
                    user_prompt: prompt,
                    status_message: String::new(),
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        cx.notify();
    }

    pub fn handle_claude_permission_request(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.claude_sessions.get_mut(session_id) {
            if session.state == ClaudeSessionState::Running
                || session.state == ClaudeSessionState::Idle
            {
                session.ancestor_pids = ancestor_pids;
                session.state = ClaudeSessionState::WaitingPermission;
            }
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    state: ClaudeSessionState::WaitingPermission,
                    user_prompt: String::new(),
                    status_message: String::new(),
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        cx.notify();
    }

    pub fn handle_claude_post_tool_use(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.claude_sessions.get_mut(session_id) {
            if session.state == ClaudeSessionState::WaitingPermission {
                session.ancestor_pids = ancestor_pids;
                session.state = ClaudeSessionState::Running;
                self.sync_session_derived_state();
                self.notify_all_panes(cx);
                cx.notify();
            }
        }
    }

    pub fn handle_pane_split(
        &mut self,
        direction: SplitDirection,
        content_type: PaneContentType,
        keep_focus: bool,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(arena) = self.active_arena().cloned() else {
            return;
        };
        arena.update(cx, |arena, cx| {
            arena.split_active_pane(direction, content_type, keep_focus, command, window, cx);
        });
    }

    pub fn handle_tab_new(
        &mut self,
        content_type: PaneContentType,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(arena) = self.active_arena().cloned() else {
            return;
        };
        arena.update(cx, |arena, cx| {
            arena.add_tab(content_type, command, window, cx);
        });
    }

    fn clear_session_for_shell_pid(&mut self, shell_pid: u32, cx: &mut Context<Self>) {
        let mut changed = false;
        for session in self.claude_sessions.values_mut() {
            if session.state == ClaudeSessionState::Completed
                && session.ancestor_pids.contains(&shell_pid)
            {
                session.state = ClaudeSessionState::Idle;
                changed = true;
            }
        }
        if changed {
            self.sync_session_derived_state();
            self.notify_all_panes(cx);
            cx.notify();
        }
    }

    pub fn update_rate_limits(
        &mut self,
        five_hour_used_pct: f32,
        five_hour_resets_at: i64,
        seven_day_used_pct: f32,
        seven_day_resets_at: i64,
        cx: &mut Context<Self>,
    ) {
        self.rate_limits = Some(RateLimits {
            five_hour: RateLimitEntry {
                used_pct: five_hour_used_pct,
                resets_at: five_hour_resets_at,
            },
            seven_day: RateLimitEntry {
                used_pct: seven_day_used_pct,
                resets_at: seven_day_resets_at,
            },
            received_at: Instant::now(),
        });

        self._rate_limits_refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));

        cx.notify();
    }

    fn notify_all_panes(&self, cx: &mut Context<Self>) {
        let panes: Vec<_> = self
            .arenas
            .iter()
            .flat_map(|ws| ws.read(cx).center.panes().into_iter().cloned())
            .collect();
        for pane in panes {
            pane.update(cx, |_: &mut Pane, cx| cx.notify());
        }
    }

    fn count_ready_terminals_in_arena(
        &self,
        workspace: &Entity<Arena>,
        cx: &App,
    ) -> usize {
        let ready_pids = self.session_state.ready_shell_pids.borrow();
        if ready_pids.is_empty() {
            return 0;
        }
        let running_pids = self.session_state.running_shell_pids.borrow();
        let permission_pids = self.session_state.permission_shell_pids.borrow();
        let ws = workspace.read(cx);
        ws.center
            .panes()
            .iter()
            .map(|pane| {
                pane.read(cx)
                    .items_of_type::<TerminalView>()
                    .filter(|tv| {
                        tv.read(cx)
                            .terminal()
                            .read(cx)
                            .pid_getter()
                            .is_some_and(|g| {
                                let pid = g.fallback_pid().as_u32();
                                ready_pids.contains(&pid)
                                    && !running_pids.contains(&pid)
                                    && !permission_pids.contains(&pid)
                            })
                    })
                    .count()
            })
            .sum()
    }

    fn count_running_claudes_in_arena(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> usize {
        let running_pids = self.session_state.running_shell_pids.borrow();
        if running_pids.is_empty() {
            return 0;
        }
        let permission_pids = self.session_state.permission_shell_pids.borrow();
        let arena = arena_entity.read(cx);
        arena
            .center
            .panes()
            .iter()
            .map(|pane| {
                pane.read(cx)
                    .items_of_type::<TerminalView>()
                    .filter(|tv| {
                        tv.read(cx)
                            .terminal()
                            .read(cx)
                            .pid_getter()
                            .is_some_and(|g| {
                                let pid = g.fallback_pid().as_u32();
                                running_pids.contains(&pid)
                                    && !permission_pids.contains(&pid)
                            })
                    })
                    .count()
            })
            .sum()
    }

    fn count_waiting_claudes_in_arena(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> usize {
        let permission_pids = self.session_state.permission_shell_pids.borrow();
        if permission_pids.is_empty() {
            return 0;
        }
        let arena = arena_entity.read(cx);
        arena
            .center
            .panes()
            .iter()
            .map(|pane| {
                pane.read(cx)
                    .items_of_type::<TerminalView>()
                    .filter(|tv| {
                        tv.read(cx)
                            .terminal()
                            .read(cx)
                            .pid_getter()
                            .is_some_and(|g| {
                                permission_pids.contains(&g.fallback_pid().as_u32())
                            })
                    })
                    .count()
            })
            .sum()
    }

    fn collect_terminal_infos_for_state(
        &self,
        arena_entity: &Entity<Arena>,
        state: ClaudeSessionState,
        pids: &HashSet<u32>,
        cx: &App,
    ) -> Vec<ReadyTerminalInfo> {
        if pids.is_empty() {
            return vec![];
        }
        let arena = arena_entity.read(cx);
        let mut infos = Vec::new();
        for pane in arena.center.panes() {
            for tv in pane.read(cx).items_of_type::<TerminalView>() {
                let pid = tv
                    .read(cx)
                    .terminal()
                    .read(cx)
                    .pid_getter()
                    .map(|g| g.fallback_pid().as_u32());
                let pid = match pid {
                    Some(p) if pids.contains(&p) => p,
                    _ => continue,
                };
                let (user_prompt, status_message) = self
                    .claude_sessions
                    .values()
                    .find(|s| {
                        s.state == state
                            && s.ancestor_pids.contains(&pid)
                    })
                    .map(|s| (s.user_prompt.clone(), s.status_message.clone()))
                    .unwrap_or_default();
                infos.push(ReadyTerminalInfo {
                    pane: pane.clone(),
                    terminal_view: tv,
                    user_prompt,
                    status_message,
                });
            }
        }
        infos
    }

    fn collect_ready_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<ReadyTerminalInfo> {
        let pids = self.session_state.ready_shell_pids.borrow().clone();
        self.collect_terminal_infos_for_state(
            arena_entity,
            ClaudeSessionState::Completed,
            &pids,
            cx,
        )
    }

    fn collect_permission_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<ReadyTerminalInfo> {
        let pids = self.session_state.permission_shell_pids.borrow().clone();
        self.collect_terminal_infos_for_state(
            arena_entity,
            ClaudeSessionState::WaitingPermission,
            &pids,
            cx,
        )
    }

    fn deploy_badge_menu(
        &mut self,
        arena_index: usize,
        terminal_infos: Vec<ReadyTerminalInfo>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.context_menu.take();
        let context_menu = ContextMenu::build(window, cx, |mut menu, window, _cx| {
            for info in terminal_infos {
                let pane = info.pane.clone();
                let terminal_view = info.terminal_view.clone();
                let prompt_label = if info.user_prompt.is_empty() {
                    "(no prompt)".to_string()
                } else {
                    truncate_for_menu(&info.user_prompt)
                };
                let status_label = truncate_for_menu(&info.status_message);

                menu = menu.custom_entry(
                    move |_window, cx| {
                        let colors = cx.theme().colors();
                        v_flex()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .truncate()
                                    .child(prompt_label.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .truncate()
                                    .child(status_label.clone()),
                            )
                            .into_any_element()
                    },
                    window.handler_for(&this, move |this, window, cx| {
                        this.navigate_to_terminal(arena_index, &pane, &terminal_view, window, cx);
                    }),
                );
            }
            menu
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.badge_menu.take();
                cx.notify();
            });
        self.badge_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn navigate_to_terminal(
        &mut self,
        arena_index: usize,
        target_pane: &Entity<Pane>,
        terminal_view: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_arena(arena_index, window, cx);

        if let Some(arena) = self.arenas.get(arena_index) {
            if let Some(zoomed_view) = arena
                .read(cx)
                .zoomed_pane
                .as_ref()
                .and_then(|z| z.upgrade())
            {
                if zoomed_view.entity_id() != target_pane.entity_id() {
                    if let Ok(zoomed_pane) = zoomed_view.downcast::<Pane>() {
                        zoomed_pane.update(cx, |pane, cx| {
                            pane.zoom_out(&ZoomOut, window, cx);
                        });
                    }
                }
            }
        }

        window.focus(&target_pane.focus_handle(cx), cx);
        let item_index = target_pane
            .read(cx)
            .index_for_item(terminal_view);
        if let Some(index) = item_index {
            target_pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx);
            });
        }

        let shell_pid = terminal_view
            .read(cx)
            .terminal()
            .read(cx)
            .pid_getter()
            .map(|g| g.fallback_pid().as_u32());
        if let Some(pid) = shell_pid {
            self.clear_session_for_shell_pid(pid, cx);
        }
    }
}

fn truncate_for_menu(s: &str) -> String {
    for (i, ch) in s.char_indices() {
        if i >= 100 || ch == '\n' || ch == '\u{3002}' {
            let end = if ch == '\n' { i } else { i + ch.len_utf8() };
            return format!("{} \u{2026}", &s[..end]);
        }
    }
    s.to_string()
}

fn render_arena_pill(
    id: impl Into<ElementId>,
    count: usize,
    bg_color: Hsla,
    text_color: Hsla,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_1p5()
        .rounded_full()
        .bg(bg_color)
        .text_color(text_color)
        .text_xs()
        .line_height(relative(1.4))
        .child(format!("{count}"))
}

fn format_resets_in(resets_at: i64) -> String {
    if resets_at <= 0 {
        return "Unknown".to_string();
    }
    let now = chrono::Local::now().timestamp();
    let remaining = resets_at - now;
    if remaining <= 0 {
        return "Session reset".to_string();
    }
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    if hours > 0 {
        format!("Resets in {}h {}m", hours, minutes)
    } else {
        format!("Resets in {}m", minutes)
    }
}

fn format_resets_at_day(resets_at: i64) -> String {
    use chrono::{Local, TimeZone};
    if resets_at <= 0 {
        return "Unknown".to_string();
    }
    let now = chrono::Local::now().timestamp();
    if resets_at <= now {
        return "Weekly reset".to_string();
    }
    let Some(dt) = Local.timestamp_opt(resets_at, 0).single() else {
        return "Unknown".to_string();
    };
    dt.format("Resets %a %-H:%M").to_string()
}

fn render_rate_limit_row(
    label: &str,
    used_pct: f32,
    resets_at: i64,
    reset_text: &str,
    cx: &App,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let now = chrono::Local::now().timestamp();
    let expired = resets_at > 0 && resets_at <= now;
    v_flex()
        .id(SharedString::from(format!("rate-limit-row-{label}")))
        .px_1()
        .gap_0p5()
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(reset_text.to_string()),
                ),
        )
        .when(!expired, |row| {
            row.child(
                ui::ProgressBar::new(
                    SharedString::from(format!("rate-limit-{label}")),
                    used_pct,
                    100.0,
                    cx,
                )
                .bg_color(colors.border),
            )
            .tooltip(Tooltip::text(format!("{:.0}%", used_pct)))
        })
}

fn repo_name_from_url(url: &str) -> Option<&str> {
    let path = url
        .strip_suffix(".git")
        .unwrap_or(url)
        .trim_end_matches('/');
    path.rsplit('/').next()
        .or_else(|| path.rsplit(':').next())
        .filter(|name| !name.is_empty())
}

fn render_pr_tooltip(
    pr: Option<&PrInfo>,
    ci: Option<&CiInfo>,
    branch: Option<&str>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let status_colors = cx.theme().status();

    let mut content = v_flex().gap_1().max_w_96();

    if let Some(pr) = pr {
        let (status_label, pr_icon, pill_bg): (SharedString, IconName, Hsla) = match pr.status {
            PrStatus::Draft => ("Draft".into(), IconName::GitPullRequest, colors.text_muted),
            PrStatus::Open => ("Open".into(), IconName::GitPullRequest, status_colors.success),
            PrStatus::Merged => (
                "Merged".into(),
                IconName::GitGraph,
                hsla(286.0 / 360.0, 0.51, 0.64, 1.0),
            ),
            PrStatus::Closed => (
                "Closed".into(),
                IconName::GitPullRequestClosed,
                status_colors.error,
            ),
            PrStatus::Conflicted => (
                "Conflicted".into(),
                IconName::GitMergeConflict,
                status_colors.warning,
            ),
        };

        let pill_bg_dark = Hsla { l: (pill_bg.l * 0.65).min(0.4), ..pill_bg };
        let pill_text = gpui::white();
        let status_pill = h_flex()
            .flex_shrink_0()
            .gap_1()
            .items_center()
            .px_1p5()
            .py_0p5()
            .rounded_full()
            .bg(pill_bg_dark)
            .text_color(pill_text)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(Icon::new(pr_icon).size(IconSize::XSmall).color(Color::Custom(pill_text)))
            .child(status_label);

        content = content
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(format!("{} #{}", pr.title, pr.number)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .overflow_hidden()
                    .child(status_pill)
                    .when_some(branch, |d, branch| {
                        d.child(
                            div()
                                .min_w_0()
                                .flex_shrink()
                                .truncate()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child(branch.to_string()),
                        )
                    }),
            );
    }

    if let Some(ci) = ci {
        if !ci.checks.is_empty() {
            // Sort: fail first, then pending, then pass, then skipping, then alphabetical.
            let mut sorted_checks: Vec<&CiCheckEntry> = ci.checks.iter().collect();
            sorted_checks.sort_by(|a, b| {
                ci_bucket_order(a.bucket.as_ref())
                    .cmp(&ci_bucket_order(b.bucket.as_ref()))
                    .then_with(|| a.name.as_ref().cmp(b.name.as_ref()))
            });

            let mut checks_list = v_flex().gap_0p5().pt_1();
            for check in sorted_checks {
                let (icon, color) = match check.bucket.as_ref() {
                    "pass" => (IconName::Check, status_colors.success),
                    "fail" => (IconName::Circle, status_colors.error),
                    "pending" => (IconName::Circle, status_colors.warning),
                    "skipping" => (IconName::Slash, colors.text_muted),
                    _ => (IconName::Circle, colors.text_muted),
                };
                let is_notable = matches!(check.bucket.as_ref(), "pass" | "fail");
                let text_color = if is_notable { colors.text } else { colors.text_muted };
                checks_list = checks_list.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(Color::Custom(color)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(text_color)
                                .child(check.name.clone()),
                        ),
                );
            }
            content = content.child(checks_list);
        }
    }

    content.into_any_element()
}

fn render_review_tooltip(
    reviews: &[ReviewEntry],
    head_sha: &str,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let status_colors = cx.theme().status();

    let mut content = v_flex().gap_0p5().max_w_96();

    // Sort: current-commit reviews first, then by state priority, then alphabetical.
    let mut sorted: Vec<&ReviewEntry> = reviews.iter().collect();
    sorted.sort_by(|a, b| {
        let a_current = a.commit_oid.as_ref() == head_sha;
        let b_current = b.commit_oid.as_ref() == head_sha;
        b_current
            .cmp(&a_current)
            .then_with(|| review_state_order(&a.state).cmp(&review_state_order(&b.state)))
            .then_with(|| a.user.as_ref().cmp(b.user.as_ref()))
    });

    for review in sorted {
        let is_current_commit = review.commit_oid.as_ref() == head_sha;

        let name_color = if is_current_commit {
            colors.text
        } else {
            colors.text_muted
        };

        let avatar_url = review.avatar_url.to_string();
        let relative_time = format_relative_time(&review.submitted_at);

        let mut row = h_flex().gap_1().items_center();

        if is_current_commit {
            let (icon, icon_color) = match review.state {
                ReviewState::Approved => (IconName::Check, status_colors.success),
                ReviewState::ChangesRequested => (IconName::XCircleFilled, status_colors.error),
                ReviewState::Commented => (IconName::Circle, status_colors.info),
                ReviewState::Dismissed => (IconName::Circle, colors.text_muted),
                ReviewState::Pending => (IconName::Circle, status_colors.warning),
            };
            row = row.child(
                Icon::new(icon)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(icon_color)),
            );
        } else {
            row = row.child(div().size(IconSize::XSmall.rems()));
        }

        row = row
            .child(
                img(avatar_url)
                    .size(px(16.))
                    .rounded_full()
                    .flex_shrink_0()
                    .with_fallback(|| {
                        Icon::new(IconName::Person)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element()
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(name_color)
                    .child(review.user.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(relative_time),
            );

        content = content.child(row);
    }

    content.into_any_element()
}

fn review_state_order(state: &ReviewState) -> u8 {
    match state {
        ReviewState::Pending => 0,
        ReviewState::Commented => 1,
        ReviewState::Approved => 2,
        ReviewState::Dismissed => 3,
        ReviewState::ChangesRequested => 4,
    }
}

fn ci_bucket_order(bucket: &str) -> u8 {
    match bucket {
        "pending" => 0,
        "pass" => 1,
        "skipping" => 2,
        "fail" => 3,
        _ => 4,
    }
}

fn format_relative_time(iso: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(parsed);

    let total_seconds = duration.num_seconds();
    if total_seconds < 0 {
        return iso.to_string();
    }

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();

    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days < 30 {
        format!("{}d ago", days)
    } else if days < 365 {
        format!("{}mo ago", days / 30)
    } else {
        format!("{}y ago", days / 365)
    }
}

fn remote_url_to_browser_url(url: &str) -> Option<String> {
    let url = url.strip_suffix(".git").unwrap_or(url).trim_end_matches('/');
    if url.starts_with("https://") || url.starts_with("http://") {
        Some(url.to_string())
    } else if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:owner/repo → https://github.com/owner/repo
        let browser_url = rest.replacen(':', "/", 1);
        Some(format!("https://{browser_url}"))
    } else {
        None
    }
}

async fn fetch_pr_info(working_dir: &std::path::Path) -> anyhow::Result<Option<PrInfo>> {
    let output = smol::process::Command::new("gh")
        .current_dir(working_dir)
        .args(&[
            "pr",
            "view",
            "--json",
            "number,title,state,mergeable,isDraft,url,reviewDecision,headRefOid",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("no pull requests found") {
            log::warn!("gh pr view failed: {stderr}");
        }
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct GhPrView {
        number: u32,
        title: String,
        state: String,
        mergeable: String,
        #[serde(rename = "isDraft")]
        is_draft: bool,
        url: String,
        #[serde(rename = "reviewDecision")]
        review_decision: String,
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
    }

    let pr: GhPrView = serde_json::from_slice(&output.stdout)?;

    let status = if pr.mergeable == "CONFLICTING" {
        PrStatus::Conflicted
    } else if pr.is_draft {
        PrStatus::Draft
    } else {
        match pr.state.as_str() {
            "MERGED" => PrStatus::Merged,
            "CLOSED" => PrStatus::Closed,
            _ => PrStatus::Open,
        }
    };

    let review_decision = match pr.review_decision.as_str() {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };

    // Fetch individual reviews via REST API to get avatar URLs.
    // Parse owner/repo from the PR URL (e.g. "https://github.com/owner/repo/pull/123").
    let reviews = fetch_reviews(working_dir, &pr.url, &pr.head_ref_oid).await;

    let (reviews, review_count) = match reviews {
        Ok(entries) => {
            let count = entries
                .iter()
                .filter(|entry| entry.commit_oid.as_ref() == pr.head_ref_oid)
                .count();
            (entries, count)
        }
        Err(err) => {
            log::warn!("failed to fetch reviews: {err}");
            (Vec::new(), 0)
        }
    };

    Ok(Some(PrInfo {
        number: pr.number,
        title: pr.title.into(),
        status,
        html_url: pr.url.into(),
        review_decision,
        review_count,
        head_sha: pr.head_ref_oid.into(),
        reviews,
    }))
}

/// Fetch per-reviewer data via the REST API (`gh api repos/{owner}/{repo}/pulls/{number}/reviews`).
/// Deduplicates by author, keeping only the latest review per user.
async fn fetch_reviews(
    working_dir: &std::path::Path,
    pr_url: &str,
    _head_ref_oid: &str,
) -> anyhow::Result<Vec<ReviewEntry>> {
    // Parse "https://github.com/{owner}/{repo}/pull/{number}" into an API path.
    let url_path = pr_url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| anyhow::anyhow!("unexpected PR URL format: {pr_url}"))?;
    let parts: Vec<&str> = url_path.splitn(4, '/').collect();
    if parts.len() < 4 {
        anyhow::bail!("unexpected PR URL format: {pr_url}");
    }
    let (owner, repo, number) = (parts[0], parts[1], parts[3]);
    let api_path = format!("repos/{owner}/{repo}/pulls/{number}/reviews");

    let output = smol::process::Command::new("gh")
        .current_dir(working_dir)
        .args(&["api", &api_path])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh api reviews failed: {stderr}");
    }

    #[derive(serde::Deserialize)]
    struct RestReview {
        user: RestUser,
        state: String,
        commit_id: String,
        submitted_at: String,
    }

    #[derive(serde::Deserialize)]
    struct RestUser {
        login: String,
        avatar_url: String,
    }

    let all_reviews: Vec<RestReview> = serde_json::from_slice(&output.stdout)?;

    // Deduplicate per author, keeping the latest (reviews are chronologically ordered).
    let mut latest_by_author: HashMap<String, RestReview> = HashMap::new();
    for review in all_reviews {
        latest_by_author.insert(review.user.login.clone(), review);
    }

    Ok(latest_by_author
        .into_values()
        .map(|review| {
            let state = match review.state.as_str() {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                _ => ReviewState::Pending,
            };
            ReviewEntry {
                user: SharedString::from(review.user.login),
                state,
                commit_oid: SharedString::from(review.commit_id),
                avatar_url: SharedString::from(review.user.avatar_url),
                submitted_at: SharedString::from(review.submitted_at),
            }
        })
        .collect())
}

async fn fetch_ci_status(
    working_dir: &std::path::Path,
    pr_number: u32,
) -> anyhow::Result<Option<CiInfo>> {
    let output = smol::process::Command::new("gh")
        .current_dir(working_dir)
        .args(&[
            "pr",
            "checks",
            &pr_number.to_string(),
            "--json",
            "name,bucket",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            log::warn!("gh pr checks failed: {stderr}");
        }
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct GhCheckEntry {
        name: String,
        bucket: String,
    }

    let checks: Vec<GhCheckEntry> = serde_json::from_slice(&output.stdout)?;
    if checks.is_empty() {
        return Ok(None);
    }

    let has_pending = checks.iter().any(|c| c.bucket == "pending");
    let has_fail = checks.iter().any(|c| c.bucket == "fail");
    let all_passed = checks
        .iter()
        .all(|c| c.bucket == "pass" || c.bucket == "skipping");

    let status = if has_pending {
        if has_fail {
            CiStatus::PendingWithFailure
        } else {
            CiStatus::PendingClean
        }
    } else if all_passed {
        CiStatus::AllPassed
    } else {
        CiStatus::Failed
    };

    let entries = checks
        .into_iter()
        .map(|c| CiCheckEntry {
            name: c.name.into(),
            bucket: c.bucket.into(),
        })
        .collect();

    Ok(Some(CiInfo {
        status,
        checks: entries,
    }))
}

impl Focusable for AgentiumApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentiumApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active_index = self.active_arena_index;

        div()
            .key_context("Agentium")
            .on_action(cx.listener(|this, action: &ActivateArena, window, cx| {
                this.switch_arena(action.index, window, cx);
            }))
            .flex()
            .flex_row()
            .size_full()
            .bg(colors.background)
            .text_color(colors.text)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(240.0))
                    .h_full()
                    .bg(colors.panel_background)
                    .border_r_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .id("agentium-title")
                            .pl(px(78.0))
                            .pr_2()
                            .h(px(36.0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, _| {
                                    this.should_move_window = true;
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, _| {
                                    this.should_move_window = false;
                                }),
                            )
                            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                                this.should_move_window = false;
                            }))
                            .on_mouse_move(cx.listener(|this, _, window, _| {
                                if this.should_move_window {
                                    this.should_move_window = false;
                                    window.start_window_move();
                                }
                            }))
                            .on_click(|event, window, _| {
                                if event.click_count() == 2 {
                                    window.titlebar_double_click();
                                }
                            }),
                    )
                    .child(
                        div()
                            .id("arena-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                                this.finish_rename_arena(true, cx);
                            }))
                            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| {
                                this.finish_rename_arena(false, cx);
                            }))
                            .children(self.arenas.iter().enumerate().map(
                                |(i, arena_entity)| {
                                    let arena = arena_entity.read(cx);
                                    let is_active = Some(i) == active_index;
                                    let status_colors = cx.theme().status();

                                    let effective_path = arena.working_directory.clone().or_else(|| {
                                        self.project
                                            .read(cx)
                                            .worktrees(cx)
                                            .next()
                                            .map(|wt| wt.read(cx).abs_path().to_path_buf())
                                    });

                                    let display_path = effective_path.as_ref().and_then(|path| {
                                        path.file_name().map(|name| name.to_string_lossy().to_string())
                                    });

                                    let git_info = effective_path.as_ref().and_then(|working_dir| {
                                        let git_store = self.project.read(cx).git_store().read(cx);
                                        git_store.repositories().values()
                                            .filter_map(|repo| {
                                                let repo = repo.read(cx);
                                                let repo_path = &repo.work_directory_abs_path;
                                                if working_dir.starts_with(repo_path.as_ref()) {
                                                    let branch_name = repo.branch.as_ref().map(|b| b.name().to_string());
                                                    let browser_url = repo.remote_origin_url.as_deref()
                                                        .and_then(remote_url_to_browser_url);
                                                    let (lines_added, lines_deleted) = repo.cached_status()
                                                        .fold((0u32, 0u32), |(added, deleted), entry| {
                                                            if let Some(stat) = &entry.diff_stat {
                                                                (added + stat.added, deleted + stat.deleted)
                                                            } else {
                                                                (added, deleted)
                                                            }
                                                        });
                                                    Some((repo_path.clone(), branch_name, browser_url, lines_added, lines_deleted))
                                                } else {
                                                    None
                                                }
                                            })
                                            .max_by_key(|(repo_path, _, _, _, _)| repo_path.clone())
                                            .map(|(_, branch_name, browser_url, lines_added, lines_deleted)| (branch_name, browser_url, lines_added, lines_deleted))
                                    });

                                    let pr_info = self.pr_info.get(&arena_entity.entity_id());
                                    let is_merged = pr_info
                                        .map_or(false, |pr| matches!(pr.status, PrStatus::Merged));

                                    let ci_icon_element = if is_merged {
                                        None
                                    } else {
                                        self.ci_status.get(&arena_entity.entity_id()).map(|ci| {
                                            let (icon, color) = match &ci.status {
                                                CiStatus::AllPassed => (IconName::Check, status_colors.success),
                                                CiStatus::Failed => (IconName::XCircleFilled, status_colors.error),
                                                CiStatus::PendingWithFailure => (IconName::Circle, status_colors.error),
                                                CiStatus::PendingClean => (IconName::Circle, status_colors.warning),
                                            };
                                            Icon::new(icon).size(IconSize::Small).color(Color::Custom(color))
                                        })
                                    };

                                    let tooltip_pr = pr_info.cloned();
                                    let tooltip_ci = self.ci_status.get(&arena_entity.entity_id()).cloned();
                                    let tooltip_branch = git_info.as_ref()
                                        .and_then(|(branch, _, _, _)| branch.clone());

                                    let (pr_element, review_element) = if let Some(pr) = pr_info {
                                        let pr_color = match pr.status {
                                            PrStatus::Draft => colors.text_muted,
                                            PrStatus::Open => status_colors.success,
                                            // No semantic purple in StatusColors; matches GitHub's merge color
                                            PrStatus::Merged => hsla(286.0 / 360.0, 0.51, 0.64, 1.0),
                                            PrStatus::Closed => status_colors.error,
                                            PrStatus::Conflicted => status_colors.warning,
                                        };
                                        let pr_icon = match pr.status {
                                            PrStatus::Draft | PrStatus::Open => IconName::GitPullRequest,
                                            PrStatus::Merged => IconName::GitGraph,
                                            PrStatus::Closed => IconName::GitPullRequestClosed,
                                            PrStatus::Conflicted => IconName::GitMergeConflict,
                                        };
                                        let url = pr.html_url.clone();
                                        let pr_el = h_flex()
                                            .id(("arena-pr", arena.id))
                                            .gap_1()
                                            .items_center()
                                            .px_1()
                                            .rounded_sm()
                                            .when(is_active, |d| {
                                                d.cursor_pointer()
                                                    .hover(|d| d.bg(colors.element_hover))
                                            })
                                            .child(
                                                Icon::new(pr_icon)
                                                    .size(IconSize::Small)
                                                    .color(Color::Custom(pr_color)),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(colors.text_muted)
                                                    .child(format!("#{}", pr.number)),
                                            )
                                            .when_some(ci_icon_element, |d, icon| d.child(icon))
                                            .tooltip(Tooltip::element({
                                                let tooltip_pr = tooltip_pr.clone();
                                                let tooltip_ci = tooltip_ci.clone();
                                                let tooltip_branch = tooltip_branch.clone();
                                                move |_window, cx| {
                                                    render_pr_tooltip(
                                                        tooltip_pr.as_ref(),
                                                        tooltip_ci.as_ref(),
                                                        tooltip_branch.as_deref(),
                                                        cx,
                                                    )
                                                }
                                            }))
                                            .when(is_active, |d| {
                                                d.on_click(cx.listener({
                                                    let url = url.clone();
                                                    move |_this, _event: &ClickEvent, _window, cx| {
                                                        cx.stop_propagation();
                                                        cx.open_url(&url);
                                                    }
                                                }))
                                            });

                                        let review_el = if is_merged {
                                            None
                                        } else {
                                            match &pr.review_decision {
                                                Some(ReviewDecision::Approved) => {
                                                    Some((IconName::Check, status_colors.success, None))
                                                }
                                                Some(ReviewDecision::ChangesRequested) => {
                                                    Some((IconName::Circle, status_colors.error, None))
                                                }
                                                Some(ReviewDecision::ReviewRequired) => {
                                                    Some((IconName::Circle, status_colors.warning, None))
                                                }
                                                None if pr.review_count > 0 => {
                                                    Some((IconName::Eye, colors.text_muted, Some(pr.review_count.to_string())))
                                                }
                                                None => None,
                                            }
                                            .map(|(review_icon, review_color, review_label)| {
                                                let tooltip_reviews = pr.reviews.clone();
                                                let tooltip_head_sha = pr.head_sha.clone();
                                                h_flex()
                                                    .id(("arena-review", arena.id))
                                                    .gap_0p5()
                                                    .items_center()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .when(is_active, |d| {
                                                        d.cursor_pointer()
                                                            .hover(|d| d.bg(colors.element_hover))
                                                    })
                                                    .child(
                                                        Icon::new(review_icon)
                                                            .size(IconSize::Small)
                                                            .color(Color::Custom(review_color)),
                                                    )
                                                    .when_some(review_label, |d, label| {
                                                        d.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(colors.text_muted)
                                                                .child(label),
                                                        )
                                                    })
                                                    .tooltip(Tooltip::element(move |_window, cx| {
                                                        render_review_tooltip(
                                                            &tooltip_reviews,
                                                            &tooltip_head_sha,
                                                            cx,
                                                        )
                                                    }))
                                                    .when(is_active, |d| {
                                                        d.on_click(cx.listener(
                                                            move |_this, _event: &ClickEvent, _window, cx| {
                                                                cx.stop_propagation();
                                                                cx.open_url(&url);
                                                            },
                                                        ))
                                                    })
                                            })
                                        };

                                        (Some(pr_el), review_el)
                                    } else {
                                        (None, None)
                                    };

                                    div()
                                        .id(("arena", arena.id))
                                        .px_2()
                                        .py_1()
                                        .mx_1()
                                        .my_px()
                                        .rounded_md()
                                        .text_color(colors.text)
                                        .cursor_pointer()
                                        .when(is_active, |d| d.bg(colors.element_selected))
                                        .when(!is_active, |d: Stateful<Div>| {
                                            d.hover(|d| d.bg(colors.element_hover))
                                        })
                                        .child({
                                            let is_renaming = self.renaming_arena.as_ref() == Some(arena_entity);
                                            let permission_count = self.count_waiting_claudes_in_arena(arena_entity, cx);
                                            let running_count = self.count_running_claudes_in_arena(arena_entity, cx);
                                            let ready_count = self.count_ready_terminals_in_arena(arena_entity, cx);
                                            let has_pills = permission_count > 0 || running_count > 0 || ready_count > 0;
                                            let project_url = git_info.as_ref()
                                                .and_then(|(_, browser_url, _, _)| browser_url.clone());
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .justify_between()
                                                .text_size(rems(0.9375))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .when(is_renaming, |d| d.child(self.rename_editor.clone()))
                                                .when(!is_renaming, |d| {
                                                    if is_active && project_url.is_some() {
                                                        let url = project_url.clone().unwrap();
                                                        d.child(
                                                            div()
                                                                .id(("arena-name", arena.id))
                                                                .cursor_pointer()
                                                                .rounded_sm()
                                                                .hover(|d| d.bg(colors.element_hover))
                                                                .child(arena.name.clone())
                                                                .on_click(cx.listener(
                                                                    move |_this, _event: &ClickEvent, _window, cx| {
                                                                        cx.stop_propagation();
                                                                        cx.open_url(&url);
                                                                    },
                                                                ))
                                                        )
                                                    } else {
                                                        d.child(arena.name.clone())
                                                    }
                                                })
                                                .when(has_pills, |d| {
                                                    let arena_entity_for_badge = arena_entity.clone();
                                                    d.child(
                                                        h_flex()
                                                            .gap_0p5()
                                                            .when(permission_count > 0, |d| {
                                                                let arena_entity_for_permission = arena_entity.clone();
                                                                d.child(
                                                                    render_arena_pill(
                                                                        ("arena-permission-badge", arena.id),
                                                                        permission_count,
                                                                        status_colors.warning,
                                                                        colors.surface_background,
                                                                    )
                                                                    .cursor_pointer()
                                                                    .on_click(cx.listener(
                                                                        move |this, event: &ClickEvent, window, cx| {
                                                                            cx.stop_propagation();
                                                                            this.switch_arena(i, window, cx);
                                                                            let infos = this.collect_permission_terminal_infos(
                                                                                &arena_entity_for_permission, cx,
                                                                            );
                                                                            this.deploy_badge_menu(
                                                                                i, infos, event.position(), window, cx,
                                                                            );
                                                                        },
                                                                    ))
                                                                )
                                                            })
                                                            .when(running_count > 0, |d| {
                                                                d.child(render_arena_pill(
                                                                    ("arena-running-badge", arena.id),
                                                                    running_count,
                                                                    status_colors.success,
                                                                    colors.surface_background,
                                                                ))
                                                            })
                                                            .when(ready_count > 0, |d| {
                                                                d.child(
                                                                    render_arena_pill(
                                                                        ("arena-ready-badge", arena.id),
                                                                        ready_count,
                                                                        colors.text_accent,
                                                                        colors.surface_background,
                                                                    )
                                                                    .cursor_pointer()
                                                                    .on_click(cx.listener(
                                                                        move |this, event: &ClickEvent, window, cx| {
                                                                            cx.stop_propagation();
                                                                            this.switch_arena(i, window, cx);
                                                                            let infos = this.collect_ready_terminal_infos(
                                                                                &arena_entity_for_badge, cx,
                                                                            );
                                                                            this.deploy_badge_menu(
                                                                                i, infos, event.position(), window, cx,
                                                                            );
                                                                        },
                                                                    ))
                                                                )
                                                            })
                                                    )
                                                })
                                        })
                                        // Row 2: branch name (left) + diff stats (right)
                                        .child({
                                            let branch_label: SharedString = git_info.as_ref()
                                                .and_then(|(branch, _, _, _)| branch.clone())
                                                .unwrap_or_default()
                                                .into();
                                            let diff_stats = git_info.as_ref()
                                                .map(|(_, _, added, deleted)| (*added, *deleted));
                                            h_flex()
                                                .items_center()
                                                .gap_1()
                                                .text_xs()
                                                .min_h(px(16.0))
                                                .when(!branch_label.is_empty(), |d| {
                                                    d.child(
                                                        div()
                                                            .min_w_0()
                                                            .flex_shrink()
                                                            .text_color(colors.text_muted)
                                                            .truncate()
                                                            .child(branch_label),
                                                    )
                                                })
                                                .child(div().flex_grow())
                                                .when_some(diff_stats, |d, (added, deleted)| {
                                                    d.child(
                                                        h_flex()
                                                            .flex_shrink_0()
                                                            .gap_1()
                                                            .when(added > 0, |d| {
                                                                d.child(
                                                                    div()
                                                                        .text_color(status_colors.created)
                                                                        .child(format!("+{added}")),
                                                                )
                                                            })
                                                            .when(deleted > 0, |d| {
                                                                d.child(
                                                                    div()
                                                                        .text_color(status_colors.deleted)
                                                                        .child(format!("-{deleted}")),
                                                                )
                                                            }),
                                                    )
                                                })
                                        })
                                        // Row 3: directory name (if different from arena name) + PR element (right)
                                        .child({
                                            let show_dir = display_path.as_ref()
                                                .map_or(false, |path| path != &arena.name);
                                            h_flex()
                                                .items_center()
                                                .gap_1()
                                                .text_xs()
                                                .min_h(px(16.0))
                                                .when(show_dir, |d| {
                                                    let path = display_path.as_ref().cloned().unwrap_or_default();
                                                    d.child(
                                                        Icon::new(IconName::Folder)
                                                            .size(IconSize::XSmall)
                                                            .color(Color::Muted),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_color(colors.text_muted)
                                                            .child(path),
                                                    )
                                                })
                                                .when(pr_element.is_some() || review_element.is_some(), |d| {
                                                    d.child(div().flex_grow())
                                                })
                                                .when_some(pr_element, |d, el| d.child(el))
                                                .when_some(review_element, |d, el| d.child(el))
                                        })
                                        .on_click(cx.listener(
                                            move |this, _, window, cx| {
                                                this.switch_arena(i, window, cx)
                                            },
                                        ))
                                        .on_mouse_down(MouseButton::Right, {
                                            let arena_entity = arena_entity.clone();
                                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                                this.deploy_arena_context_menu(
                                                    arena_entity.clone(),
                                                    event.position,
                                                    window,
                                                    cx,
                                                );
                                            })
                                        })
                                },
                            )),
                    )
                    .child(div().p_2().child({
                        let agentium_weak = cx.entity().downgrade();
                        let fs = self.app_state.fs.clone();
                        PopoverMenu::new("new-arena-menu")
                            .menu(move |window, cx| {
                                Some(cx.new(|cx| {
                                    AgentiumRecentProjects::new(
                                        agentium_weak.clone(),
                                        fs.clone(),
                                        window,
                                        cx,
                                    )
                                }))
                            })
                            .trigger(
                                Button::new("add-workspace", "+ New Arena")
                                    .full_width()
                                    .style(ButtonStyle::Subtle),
                            )
                            .anchor(Corner::BottomLeft)
                    }))
                    .when_some(self.rate_limits.as_ref(), |sidebar, rate_limits| {
                        let is_stale =
                            rate_limits.received_at.elapsed() > Duration::from_secs(3600);
                        sidebar.child(
                            v_flex()
                                .px_2()
                                .pb_2()
                                .gap_1()
                                .child(div().h_px().mx_1().mb_1().bg(colors.border))
                                .child(
                                    h_flex()
                                        .px_1()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(colors.text_muted)
                                                .child("Rate limits"),
                                        )
                                        .when(is_stale, |row| {
                                            row.child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(cx.theme().status().warning)
                                                    .child("!"),
                                            )
                                        }),
                                )
                                .child(render_rate_limit_row(
                                    "session",
                                    rate_limits.five_hour.used_pct,
                                    rate_limits.five_hour.resets_at,
                                    &format_resets_in(rate_limits.five_hour.resets_at),
                                    cx,
                                ))
                                .child(render_rate_limit_row(
                                    "week",
                                    rate_limits.seven_day.used_pct,
                                    rate_limits.seven_day.resets_at,
                                    &format_resets_at_day(rate_limits.seven_day.resets_at),
                                    cx,
                                )),
                        )
                    })
                    .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                        deferred(
                            anchored()
                                .position(*position)
                                .anchor(Corner::TopLeft)
                                .child(menu.clone()),
                        )
                        .with_priority(1)
                    }))
                    .children(self.badge_menu.as_ref().map(|(menu, position, _)| {
                        deferred(
                            anchored()
                                .position(*position)
                                .anchor(Corner::TopLeft)
                                .child(menu.clone()),
                        )
                        .with_priority(1)
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .bg(colors.background)
                    .map(|d| match self.active_arena() {
                        Some(arena) => d.child(arena.clone()),
                        None => d.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size_full()
                                .text_color(colors.text_muted)
                                .child("Press + to add an arena"),
                        ),
                    }),
            )
            .child(self.workspace_entity.read(cx).modal_layer().clone())
    }
}

struct AgentiumRecentProjects {
    picker: Entity<Picker<AgentiumRecentProjectsDelegate>>,
    _subscription: Subscription,
}

impl AgentiumRecentProjects {
    fn new(
        agentium_app: WeakEntity<AgentiumApp>,
        fs: Arc<dyn fs::Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = AgentiumRecentProjectsDelegate {
            agentium_app,
            fs: fs.clone(),
            workspaces: Vec::new(),
            filtered_workspaces: Vec::new(),
            selected_index: 0,
            focus_handle: cx.focus_handle(),
        };

        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .show_scrollbar(true)
        });

        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle;
        });

        let _subscription =
            cx.subscribe(&picker, |_this: &mut Self, _, _, cx| cx.emit(DismissEvent));

        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            let workspaces = db
                .recent_workspaces_on_disk(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, move |this, window, cx| {
                this.picker.update(cx, move |picker, cx| {
                    picker.delegate.set_workspaces(workspaces);
                    picker.update_matches(picker.query(cx), window, cx)
                })
            })
            .ok();
        })
        .detach();

        picker.focus_handle(cx).focus(window, cx);

        Self {
            picker,
            _subscription,
        }
    }
}

impl EventEmitter<DismissEvent> for AgentiumRecentProjects {}

impl Focusable for AgentiumRecentProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for AgentiumRecentProjects {
    fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AgentiumRecentProjects")
            .w(px(235.0))
            .child(self.picker.clone())
    }
}

struct AgentiumRecentProjectsDelegate {
    agentium_app: WeakEntity<AgentiumApp>,
    fs: Arc<dyn fs::Fs>,
    workspaces: Vec<(
        WorkspaceId,
        SerializedWorkspaceLocation,
        PathList,
        DateTime<Utc>,
    )>,
    filtered_workspaces: Vec<StringMatch>,
    selected_index: usize,
    focus_handle: FocusHandle,
}

impl AgentiumRecentProjectsDelegate {
    fn set_workspaces(&mut self, workspaces: Vec<workspace::RecentWorkspace>) {
        self.workspaces = workspaces
            .into_iter()
            .map(|w| (w.workspace_id, w.location, w.paths, w.timestamp))
            .collect();
    }

    fn delete_recent_project(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(hit) = self.filtered_workspaces.get(ix) else {
            return;
        };
        let Some((workspace_id, _, _, _)) = self.workspaces.get(hit.candidate_id) else {
            return;
        };
        let workspace_id = *workspace_id;
        let fs = self.fs.clone();
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            db.delete_workspace_by_id(workspace_id).await.log_err();
            let workspaces = db
                .recent_workspaces_on_disk(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, move |picker, window, cx| {
                picker.delegate.set_workspaces(workspaces);
                picker
                    .delegate
                    .set_selected_index(ix.saturating_sub(1), window, cx);
                picker.update_matches(picker.query(cx), window, cx);
            })
            .ok();
        })
        .detach();
    }

}

impl EventEmitter<DismissEvent> for AgentiumRecentProjectsDelegate {}

impl PickerDelegate for AgentiumRecentProjectsDelegate {
    type ListItem = AnyElement;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn render_editor(
        &self,
        editor: &Arc<dyn ErasedEditor>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Div {
        h_flex()
            .flex_none()
            .h_9()
            .px_2p5()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(editor.render(window, cx))
    }

    fn match_count(&self) -> usize {
        self.filtered_workspaces.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim_start();
        let smart_case = query.chars().any(|c| c.is_uppercase());
        let is_empty_query = query.is_empty();

        let candidates: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, (_, location, _, _))| {
                matches!(location, SerializedWorkspaceLocation::Local)
            })
            .map(|(id, (_, _, paths, _))| {
                let combined_string = paths
                    .ordered_paths()
                    .map(|path| path.compact().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("");
                StringMatchCandidate::new(id, &combined_string)
            })
            .collect();

        if is_empty_query {
            self.filtered_workspaces = candidates
                .into_iter()
                .map(|candidate| StringMatch {
                    candidate_id: candidate.id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: candidate.string,
                })
                .collect();
        } else {
            let mut matches = smol::block_on(fuzzy::match_strings(
                &candidates,
                query,
                smart_case,
                true,
                100,
                &Default::default(),
                cx.background_executor().clone(),
            ));
            matches.sort_unstable_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.candidate_id.cmp(&b.candidate_id))
            });
            self.filtered_workspaces = matches;
        }

        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(hit) = self.filtered_workspaces.get(self.selected_index) else {
            return;
        };
        let Some((_, location, paths, _)) = self.workspaces.get(hit.candidate_id) else {
            return;
        };
        if !matches!(location, SerializedWorkspaceLocation::Local) {
            return;
        }
        let Some(path) = paths.paths().first().cloned() else {
            return;
        };
        let window_handle = window.window_handle();
        cx.defer(move |cx| {
            if let Some(handle) = window_handle.downcast::<AgentiumApp>() {
                handle
                    .update(cx, |app, window, cx| {
                        app.add_arena_with_path(path, window, cx);
                    })
                    .ok();
            }
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.workspaces.is_empty() {
            Some("Recently opened projects will show up here".into())
        } else {
            Some("No matches".into())
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.filtered_workspaces.get(ix)?;
        let (_, _, paths, _) = self.workspaces.get(hit.candidate_id)?;

        let ordered_paths: Vec<_> = paths
            .ordered_paths()
            .map(|p| p.compact().to_string_lossy().to_string())
            .collect();
        let tooltip_path: SharedString = ordered_paths.join("\n").into();

        let dir_name = paths
            .ordered_paths()
            .next()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| ordered_paths.first().cloned().unwrap_or_default());

        let match_label = HighlightedMatch {
            text: dir_name,
            highlight_positions: Vec::new(),
            color: Color::Default,
        };
        let highlighted = HighlightedMatchWithPaths {
            prefix: None,
            match_label,
            paths: Vec::new(),
        };

        let delete_button = IconButton::new(("delete", ix), IconName::Close)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Delete from Recent Projects"))
            .on_click(cx.listener(move |picker, _, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
                picker.delegate.delete_recent_project(ix, window, cx);
            }))
            .into_any_element();

        Some(
            ListItem::new(ix)
                .toggle_state(selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    h_flex()
                        .gap_3()
                        .flex_grow()
                        .child(highlighted.render(window, cx)),
                )
                .tooltip(Tooltip::text(tooltip_path))
                .map(|el| {
                    if self.selected_index == ix {
                        el.end_slot(delete_button)
                    } else {
                        el.end_slot(delete_button)
                    }
                })
                .into_any_element(),
        )
    }

    fn render_footer(
        &self,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            div()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    ListItem::new("open_local_project")
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(Label::new("Open Local Project…").size(LabelSize::Small))
                        .on_click(cx.listener(|picker, _, window, cx| {
                            let paths_receiver =
                                cx.prompt_for_paths(gpui::PathPromptOptions {
                                    files: false,
                                    directories: true,
                                    multiple: false,
                                    prompt: None,
                                });
                            let agentium_app = picker.delegate.agentium_app.clone();
                            cx.spawn_in(window, async move |_, cx| {
                                if let Ok(Ok(Some(paths))) = paths_receiver.await {
                                    if let Some(path) = paths.into_iter().next() {
                                        agentium_app
                                            .update_in(cx, |app, window, cx| {
                                                app.add_arena_with_path(path, window, cx);
                                            })
                                            .ok();
                                    }
                                }
                                anyhow::Ok(())
                            })
                            .detach_and_log_err(cx);
                            cx.emit(DismissEvent);
                        })),
                )
                .into_any(),
        )
    }
}
