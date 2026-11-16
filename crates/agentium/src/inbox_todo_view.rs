use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    Refineable as _, Render, SharedString, StatefulInteractiveElement, StyleRefinement, Styled,
    Subscription, TextStyleRefinement, UnderlineStyle, Window, div, prelude::FluentBuilder as _,
    px, rems,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use ui::{
    ActiveTheme, Button, ButtonCommon as _, Clickable as _, Disableable as _, Icon, IconName,
    IconSize, LabelSize, Toggleable as _, h_flex, v_flex,
};
use util::ResultExt as _;
use uuid::Uuid;
use workspace::Pane;

use crate::arena::Arena;
use crate::{InboxAsker, inbox_todo_color, with_from_line};

gpui::actions!(
    agentium_inbox_todo,
    [
        /// Send the chosen answers to the assigned agents.
        SendAnswers
    ]
);

pub(crate) const KEY_CONTEXT: &str = "InboxTodo";

pub const MAX_TODO_TITLE_CHARS: usize = 200;
pub const MAX_TODO_TEXT_CHARS: usize = 2000;
/// Open items (not Done) one task, or one task-less arena, may hold.
pub const MAX_OPEN_TODOS_PER_SCOPE: usize = 30;
/// Done items older than this are dropped when the store loads.
const DONE_RETENTION_DAYS: i64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxTodoKind {
    /// The user does something, then answers "Done".
    UserTask,
    /// The user decides whether an agent does it ("Go" / "Skip").
    Proposal,
}

/// Who created, requested or closed an item. A human has no session
/// identity: the CLI resolves the caller's tab, and a plain shell has no
/// agent in it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "who", rename_all = "snake_case")]
pub enum TodoCreator {
    Session { asker: InboxAsker },
    Human,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    Open,
    Assigned,
    Requested,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoOutcome {
    Completed,
    Skipped,
    Dismissed,
}

/// A session working on an item, with where its tab lived so the tab can be
/// brought back after it closed. The item's own arena is where it was
/// created, which can differ.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Assignee {
    pub asker: InboxAsker,
    pub arena_path: Option<PathBuf>,
    pub arena_name: String,
}

/// Set when the item was requested: by the user's Go, or by the agent that
/// created it for itself.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TodoRequest {
    /// The user's condition; it overrides `detail` but never rewrites it.
    pub note: Option<String>,
    pub at: DateTime<Utc>,
    pub by: TodoCreator,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TodoResolution {
    pub outcome: TodoOutcome,
    pub note: Option<String>,
    pub at: DateTime<Utc>,
    pub by: TodoCreator,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: u64,
    /// `None` for an arena that belongs to no task.
    pub task_id: Option<Uuid>,
    /// The arena the item was created in; the default when a session is
    /// attached from the overview.
    pub arena_path: Option<PathBuf>,
    /// Kept from lists written before `arena_path` existed; such items are
    /// matched to a scope by name.
    pub arena_name: String,
    pub branch: Option<String>,
    pub kind: InboxTodoKind,
    pub title: String,
    pub detail: Option<String>,
    pub created_by: TodoCreator,
    pub assigned: Vec<Assignee>,
    pub state: TodoState,
    pub request: Option<TodoRequest>,
    /// `Some` exactly when `state` is `Done`.
    pub resolution: Option<TodoResolution>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TodoItem {
    pub fn is_open(&self) -> bool {
        self.state != TodoState::Done
    }

    pub fn is_assigned_to(&self, asker: &InboxAsker) -> bool {
        self.assigned.iter().any(|assignee| &assignee.asker == asker)
    }

    /// Only an assigned session closes or edits an item. The user does both
    /// from the overview as `Human`. This is a guard against accidents, not a
    /// boundary: any session may assign itself first.
    fn may_write(&self, by: &TodoCreator) -> bool {
        match by {
            TodoCreator::Human => true,
            TodoCreator::Session { asker } => self.is_assigned_to(asker),
        }
    }
}

/// What `add` needs besides the generated id and timestamps.
pub struct TodoDraft {
    pub task_id: Option<Uuid>,
    pub arena_path: Option<PathBuf>,
    pub arena_name: String,
    pub branch: Option<String>,
    pub kind: InboxTodoKind,
    pub title: String,
    pub detail: Option<String>,
    pub created_by: TodoCreator,
    /// The creating session when it keeps the item; `None` leaves it Open.
    pub assignee: Option<Assignee>,
}

/// Which items an overview shows. `task_id` is authoritative: an item owned
/// by a task appears only there, even when another task shares its worktree
/// or the arena later moved. Only task-less items fall back to the arena's
/// path, or to its name for items saved before paths were recorded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TodoScope {
    pub task_id: Option<Uuid>,
    pub arena_paths: Vec<PathBuf>,
    pub arena_names: Vec<String>,
}

impl TodoScope {
    pub fn contains(&self, item: &TodoItem) -> bool {
        match (self.task_id, item.task_id) {
            (Some(scope_task), Some(item_task)) => scope_task == item_task,
            (None, Some(_)) => false,
            (_, None) => match &item.arena_path {
                Some(path) => self.arena_paths.iter().any(|candidate| candidate == path),
                None => self.arena_names.contains(&item.arena_name),
            },
        }
    }
}

pub fn validate_todo_title(title: &str) -> anyhow::Result<String> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("the title is empty");
    }
    if title.chars().count() > MAX_TODO_TITLE_CHARS {
        anyhow::bail!("the title is longer than {MAX_TODO_TITLE_CHARS} characters");
    }
    Ok(title.to_string())
}

/// Trims and drops empty text; detail and notes share one limit.
pub fn validate_todo_text(label: &str, text: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > MAX_TODO_TEXT_CHARS {
        anyhow::bail!("the {label} is longer than {MAX_TODO_TEXT_CHARS} characters");
    }
    Ok(Some(text.to_string()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TodoChoice {
    Done,
    Go,
    Skip,
}

impl TodoChoice {
    fn allowed_for(self, kind: InboxTodoKind) -> bool {
        matches!(
            (self, kind),
            (Self::Done, InboxTodoKind::UserTask) | (Self::Go | Self::Skip, InboxTodoKind::Proposal)
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Go => "Go",
            Self::Skip => "Skip",
        }
    }
}

/// The message one agent receives for the items the user sent it with Go.
/// Items are named by id and title: the agent lists them with `todo list`
/// and closes them with `todo done <id>`.
pub(crate) fn todo_request_message(items: &[(&TodoItem, Option<&str>)]) -> String {
    let mut lines = vec!["To do".to_string()];
    for (item, note) in items {
        let mut line = format!("Go: #{} {}", item.id, item.title);
        if let Some(detail) = &item.detail {
            line.push_str(" :: ");
            line.push_str(detail);
        }
        if let Some(note) = note {
            line.push_str(" — Note: ");
            line.push_str(note);
        }
        lines.push(line);
    }
    lines.push("Run `agentium todo done <id>` for each item once it is finished.".to_string());
    lines.join("\n")
}

/// Where a tab is now. Tabs move between panes and a closed tab's entity can
/// outlive it, so only membership in a current pane counts.
pub(crate) fn find_item_location(
    arenas: &[Entity<Arena>],
    item_id: EntityId,
    cx: &App,
) -> Option<(Entity<Arena>, Entity<Pane>, Box<dyn workspace::ItemHandle>)> {
    for arena in arenas {
        for pane in arena.read(cx).center.panes() {
            if let Some(item) = pane
                .read(cx)
                .items()
                .find(|item| item.item_id() == item_id)
            {
                return Some((arena.clone(), pane.clone(), item.boxed_clone()));
            }
        }
    }
    None
}

/// How to bring back the tab of an assignee that is not open.
#[derive(Debug, PartialEq)]
pub(crate) enum RestorePlan {
    ResumeClaude { arena_path: PathBuf, session_id: String },
    OpenArena { arena_path: PathBuf },
    Fallback(String),
}

/// Assignees recorded without a path only know the arena's display name,
/// which several worktrees can share; guessing would resume the agent in the
/// wrong directory, so the name must match exactly one open arena.
pub(crate) fn todo_restore_plan(
    assignee: &Assignee,
    open_arenas: &[(String, PathBuf)],
) -> RestorePlan {
    let arena_path = match &assignee.arena_path {
        Some(path) => path.clone(),
        None => {
            let mut matches = open_arenas
                .iter()
                .filter(|(name, _)| *name == assignee.arena_name);
            match (matches.next(), matches.next()) {
                (Some((_, path)), None) => path.clone(),
                _ => {
                    return RestorePlan::Fallback(format!(
                        "Cannot tell which arena '{}' is; open the tab manually.",
                        assignee.arena_name
                    ));
                }
            }
        }
    };
    match &assignee.asker {
        InboxAsker::ClaudeSession { session_id } => RestorePlan::ResumeClaude {
            arena_path,
            session_id: session_id.clone(),
        },
        InboxAsker::CodexAcp { .. } => RestorePlan::OpenArena { arena_path },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingRestore {
    /// `claude --resume` is being started.
    Launching { asker: InboxAsker },
    /// Started in this tab; waiting for its session to reconnect.
    Launched { asker: InboxAsker, item_id: EntityId },
    /// The tab exists again (ACP, restored with its arena); binds at the next sync.
    Restored { asker: InboxAsker },
}

impl PendingRestore {
    pub(crate) fn asker(&self) -> &InboxAsker {
        match self {
            Self::Launching { asker } | Self::Launched { asker, .. } | Self::Restored { asker } => {
                asker
            }
        }
    }
}

/// Applies the outcome of starting `claude --resume`; returns the error to
/// show on the row when it failed.
pub(crate) fn record_restore_launch(
    pending: &mut HashMap<u64, PendingRestore>,
    todo_id: u64,
    result: anyhow::Result<EntityId>,
) -> Option<String> {
    match result {
        Ok(item_id) => {
            if let Some(entry) = pending.get_mut(&todo_id) {
                let asker = entry.asker().clone();
                *entry = PendingRestore::Launched { asker, item_id };
            }
            None
        }
        Err(error) => {
            pending.remove(&todo_id);
            Some(format!("Could not restore the tab: {error:#}"))
        }
    }
}

/// Decides an ACP restoration right after its arena was ensured: ACP tabs
/// come back synchronously with their arena, so a missing one will not appear.
pub(crate) fn acp_restore_outcome(
    arena_found: bool,
    asker_tab_found: bool,
    asker: InboxAsker,
) -> Result<PendingRestore, String> {
    match (arena_found, asker_tab_found) {
        (false, _) => Err("Could not open the arena of this tab.".to_string()),
        (true, false) => Err("This Codex ACP tab is no longer saved in its arena.".to_string()),
        (true, true) => Ok(PendingRestore::Restored { asker }),
    }
}

pub(crate) struct RestoreObservation {
    pub(crate) item_exists: bool,
    pub(crate) assignee_open: bool,
    pub(crate) launched_tab_open: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum RestoreOutcome {
    Open,
    Failed(String),
}

/// Settles pending restorations at a sync: a reconnected assignee is done, a
/// vanished item is forgotten, and a launched tab that closed before its
/// session reconnected fails so that the next click retries.
pub(crate) fn advance_pending_restores(
    pending: &mut HashMap<u64, PendingRestore>,
    observe: impl Fn(u64, &PendingRestore) -> RestoreObservation,
) -> Vec<(u64, RestoreOutcome)> {
    let mut outcomes = Vec::new();
    pending.retain(|todo_id, restore| {
        let observation = observe(*todo_id, restore);
        if !observation.item_exists {
            return false;
        }
        if observation.assignee_open {
            outcomes.push((*todo_id, RestoreOutcome::Open));
            return false;
        }
        if matches!(restore, PendingRestore::Launched { .. }) && !observation.launched_tab_open {
            outcomes.push((
                *todo_id,
                RestoreOutcome::Failed(
                    "The restored tab closed before its session reconnected.".to_string(),
                ),
            ));
            return false;
        }
        true
    });
    outcomes.sort_by_key(|(todo_id, _)| *todo_id);
    outcomes
}

/// The persisted form: items of every task, plus a version so the per-tab
/// lists written by earlier versions (a bare array) are recognized.
#[derive(serde::Serialize, serde::Deserialize)]
struct TodoFile {
    version: u32,
    items: Vec<TodoItem>,
}

const TODO_FILE_VERSION: u32 = 2;

/// One per-tab list as versions before `TodoFile` wrote it.
#[derive(serde::Deserialize)]
struct LegacyTodoList {
    id: u64,
    #[serde(default)]
    arena_name: String,
    #[serde(default = "Utc::now")]
    updated_at: DateTime<Utc>,
    #[serde(default)]
    asker: Option<InboxAsker>,
    #[serde(default)]
    items: Vec<LegacyTodoItem>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    arena_path: Option<PathBuf>,
}

#[derive(serde::Deserialize)]
struct LegacyTodoItem {
    kind: InboxTodoKind,
    title: String,
    #[serde(default)]
    detail: Option<String>,
}

/// Turns per-tab lists into items. The list's tab becomes the assignee of
/// every item, which stays Requested as the agent had already taken it on;
/// a list without an asker (a plain terminal) becomes unassigned Open items.
fn migrate_legacy_lists(
    lists: Vec<LegacyTodoList>,
    resolve_task: &dyn Fn(&Path) -> Option<Uuid>,
    next_id: &mut u64,
) -> Vec<TodoItem> {
    let mut items = Vec::new();
    for list in lists {
        let task_id = list.arena_path.as_deref().and_then(resolve_task);
        let (created_by, assignee, state, request) = match &list.asker {
            Some(asker) => (
                TodoCreator::Session {
                    asker: asker.clone(),
                },
                Some(Assignee {
                    asker: asker.clone(),
                    arena_path: list.arena_path.clone(),
                    arena_name: list.arena_name.clone(),
                }),
                TodoState::Requested,
                Some(TodoRequest {
                    note: None,
                    at: list.updated_at,
                    by: TodoCreator::Session {
                        asker: asker.clone(),
                    },
                }),
            ),
            None => (TodoCreator::Human, None, TodoState::Open, None),
        };
        for item in list.items {
            let id = *next_id;
            *next_id += 1;
            items.push(TodoItem {
                id,
                task_id,
                arena_path: list.arena_path.clone(),
                arena_name: list.arena_name.clone(),
                branch: list.branch.clone(),
                kind: item.kind,
                title: item.title,
                detail: item.detail,
                created_by: created_by.clone(),
                assigned: assignee.clone().into_iter().collect(),
                state,
                request: request.clone(),
                resolution: None,
                created_at: list.updated_at,
                updated_at: list.updated_at,
            });
        }
        // Ids of the old lists shared the Inbox counter; keep moving past them.
        *next_id = (*next_id).max(list.id + 1);
    }
    items
}

/// Parses either file format. Legacy ids are reassigned from `next_id`; the
/// flag says so, and the caller writes the result back before anything
/// else can hand those ids out again.
fn parse_todo_file(
    content: &[u8],
    resolve_task: &dyn Fn(&Path) -> Option<Uuid>,
    next_id: &mut u64,
) -> anyhow::Result<(Vec<TodoItem>, bool)> {
    let value: serde_json::Value = serde_json::from_slice(content)?;
    if value.is_array() {
        let lists: Vec<LegacyTodoList> = serde_json::from_value(value)?;
        return Ok((migrate_legacy_lists(lists, resolve_task, next_id), true));
    }
    let file: TodoFile = serde_json::from_value(value)?;
    anyhow::ensure!(
        file.version <= TODO_FILE_VERSION,
        "todo.json version {} is newer than this Agentium",
        file.version
    );
    Ok((file.items, false))
}

/// Keeps every open item and the Done items closed within the retention.
fn prune_done(items: Vec<TodoItem>, now: DateTime<Utc>) -> Vec<TodoItem> {
    items
        .into_iter()
        .filter(|item| match &item.resolution {
            Some(resolution) => now - resolution.at < chrono::Duration::days(DONE_RETENTION_DAYS),
            None => true,
        })
        .collect()
}

/// What `send` did, for the view to report.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SendReport {
    pub(crate) closed: usize,
    pub(crate) requested: usize,
}

/// Every task's To do items. An entity of its own so the overview can
/// observe it, and so tests can use it without `AgentiumApp` (whose
/// construction reads and writes the user's data_dir).
pub(crate) struct InboxTodos {
    items: Vec<TodoItem>,
    /// The user's draft answers; cheap to redo, so not persisted.
    choices: HashMap<u64, TodoChoice>,
    /// Sessions that already received Go for an item whose other sessions
    /// could not be reached, with the note they received, so a retry with
    /// the same note does not ask them twice and a retry with an edited
    /// note starts over. Cleared once the item is Requested or the choice
    /// changes.
    delivered: HashMap<u64, (Option<String>, Vec<InboxAsker>)>,
    /// The file held legacy lists; their new ids are final only once
    /// `persist_migration` wrote them.
    migrated: bool,
    path: Option<PathBuf>,
    writer: crate::SerialWriter,
}

impl InboxTodos {
    /// `next_id` is the shared Inbox counter: migrated legacy items take ids
    /// from it and advance it.
    pub(crate) fn load(
        path: Option<PathBuf>,
        resolve_task: &dyn Fn(&Path) -> Option<Uuid>,
        next_id: &mut u64,
    ) -> Self {
        let (items, migrated) = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok().map(|content| (path, content)))
            .and_then(|(path, content)| {
                parse_todo_file(&content, resolve_task, next_id)
                    .with_context(|| format!("Failed to parse {}", path.display()))
                    .log_err()
            })
            .unwrap_or_default();
        Self {
            items: prune_done(items, Utc::now()),
            choices: HashMap::new(),
            delivered: HashMap::new(),
            migrated,
            path,
            writer: crate::SerialWriter::default(),
        }
    }

    /// Writes a migrated file back at once, so the ids the migration handed
    /// out survive a restart that writes nothing else.
    pub(crate) fn persist_migration(&mut self, cx: &mut Context<Self>) {
        if self.migrated {
            self.migrated = false;
            self.persist(cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        let mut next_id = 1;
        Self::load(path, &|_| None, &mut next_id)
    }

    pub(crate) fn items(&self) -> &[TodoItem] {
        &self.items
    }

    pub(crate) fn item(&self, todo_id: u64) -> Option<&TodoItem> {
        self.items.iter().find(|item| item.id == todo_id)
    }

    pub(crate) fn max_id(&self) -> Option<u64> {
        self.items.iter().map(|item| item.id).max()
    }

    pub(crate) fn choice(&self, todo_id: u64) -> Option<TodoChoice> {
        self.choices.get(&todo_id).copied()
    }

    fn item_mut(&mut self, todo_id: u64) -> anyhow::Result<&mut TodoItem> {
        self.items
            .iter_mut()
            .find(|item| item.id == todo_id)
            .with_context(|| format!("no To do item #{todo_id}"))
    }

    fn open_items_in_same_scope(&self, draft: &TodoDraft) -> usize {
        self.items
            .iter()
            .filter(|item| item.is_open())
            .filter(|item| match draft.task_id {
                Some(task_id) => item.task_id == Some(task_id),
                None => item.task_id.is_none() && item.arena_path == draft.arena_path,
            })
            .count()
    }

    pub(crate) fn add(
        &mut self,
        draft: TodoDraft,
        next_id: &mut u64,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<u64> {
        let title = validate_todo_title(&draft.title)?;
        let detail = validate_todo_text("detail", draft.detail.as_deref())?;
        if self.open_items_in_same_scope(&draft) >= MAX_OPEN_TODOS_PER_SCOPE {
            anyhow::bail!(
                "this task already has {MAX_OPEN_TODOS_PER_SCOPE} open To do items; close some first"
            );
        }
        let now = Utc::now();
        let id = *next_id;
        *next_id += 1;
        let (state, request) = match (&draft.assignee, &draft.created_by) {
            (Some(_), TodoCreator::Session { .. }) => (
                TodoState::Requested,
                Some(TodoRequest {
                    note: None,
                    at: now,
                    by: draft.created_by.clone(),
                }),
            ),
            (Some(_), TodoCreator::Human) => (TodoState::Assigned, None),
            (None, _) => (TodoState::Open, None),
        };
        self.items.push(TodoItem {
            id,
            task_id: draft.task_id,
            arena_path: draft.arena_path,
            arena_name: draft.arena_name,
            branch: draft.branch,
            kind: draft.kind,
            title,
            detail,
            created_by: draft.created_by,
            assigned: draft.assignee.into_iter().collect(),
            state,
            request,
            resolution: None,
            created_at: now,
            updated_at: now,
        });
        self.persist(cx);
        cx.notify();
        Ok(id)
    }

    /// Adds a session to the item. Anyone may do this (see `may_write`).
    pub(crate) fn assign(
        &mut self,
        todo_id: u64,
        assignee: Assignee,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let item = self.item_mut(todo_id)?;
        anyhow::ensure!(item.is_open(), "To do item #{todo_id} is already done");
        if item.is_assigned_to(&assignee.asker) {
            return Ok(());
        }
        item.assigned.push(assignee);
        if item.state == TodoState::Open {
            item.state = TodoState::Assigned;
        }
        item.updated_at = Utc::now();
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    /// A session picks the item up for itself: assigned, and requested as if
    /// it had created the item. A request the user already made (with its
    /// condition) stays as it is.
    pub(crate) fn take(
        &mut self,
        todo_id: u64,
        assignee: Assignee,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let asker = assignee.asker.clone();
        self.assign(todo_id, assignee, cx)?;
        let item = self.item_mut(todo_id)?;
        if item.state != TodoState::Requested {
            item.state = TodoState::Requested;
            if item.request.is_none() {
                item.request = Some(TodoRequest {
                    note: None,
                    at: Utc::now(),
                    by: TodoCreator::Session { asker },
                });
            }
            item.updated_at = Utc::now();
            self.persist(cx);
            cx.notify();
        }
        Ok(())
    }

    /// Records the arena the user chose when starting a session for an item
    /// that had none, so the next session defaults to it.
    pub(crate) fn set_arena(
        &mut self,
        todo_id: u64,
        arena_path: PathBuf,
        arena_name: String,
        cx: &mut Context<Self>,
    ) {
        let Ok(item) = self.item_mut(todo_id) else {
            return;
        };
        if item.arena_path.as_ref() == Some(&arena_path) {
            return;
        }
        item.arena_path = Some(arena_path);
        item.arena_name = arena_name;
        item.updated_at = Utc::now();
        self.persist(cx);
        cx.notify();
    }

    pub(crate) fn unassign(
        &mut self,
        todo_id: u64,
        asker: &InboxAsker,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let item = self.item_mut(todo_id)?;
        anyhow::ensure!(item.is_open(), "To do item #{todo_id} is already done");
        let before = item.assigned.len();
        item.assigned.retain(|assignee| &assignee.asker != asker);
        if item.assigned.len() == before {
            anyhow::bail!("that session is not assigned to To do item #{todo_id}");
        }
        if item.assigned.is_empty() {
            // A request with nobody to carry it out waits for a new assignee.
            item.state = TodoState::Open;
        }
        item.updated_at = Utc::now();
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    /// Closes the item. A session must be assigned to it; the user may close
    /// anything. `request` stays as it was, so a Done item records whether it
    /// was ever requested.
    pub(crate) fn done(
        &mut self,
        todo_id: u64,
        by: TodoCreator,
        outcome: TodoOutcome,
        note: Option<&str>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let note = validate_todo_text("note", note)?;
        let item = self.item_mut(todo_id)?;
        anyhow::ensure!(item.is_open(), "To do item #{todo_id} is already done");
        anyhow::ensure!(
            item.may_write(&by),
            "only an assigned session can close To do item #{todo_id}; run `agentium todo take {todo_id}` first"
        );
        let now = Utc::now();
        item.state = TodoState::Done;
        item.resolution = Some(TodoResolution {
            outcome,
            note,
            at: now,
            by,
        });
        item.updated_at = now;
        self.choices.remove(&todo_id);
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn update(
        &mut self,
        todo_id: u64,
        by: &TodoCreator,
        title: Option<&str>,
        detail: Option<&str>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let title = title.map(validate_todo_title).transpose()?;
        let detail = detail
            .map(|detail| validate_todo_text("detail", Some(detail)))
            .transpose()?;
        let item = self.item_mut(todo_id)?;
        anyhow::ensure!(item.is_open(), "To do item #{todo_id} is already done");
        anyhow::ensure!(
            item.may_write(by),
            "only an assigned session can edit To do item #{todo_id}; run `agentium todo take {todo_id}` first"
        );
        if let Some(title) = title {
            item.title = title;
        }
        if let Some(detail) = detail {
            item.detail = detail;
        }
        item.updated_at = Utc::now();
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    /// `None` clears the choice. A choice the item's kind does not allow is
    /// ignored.
    pub(crate) fn set_choice(
        &mut self,
        todo_id: u64,
        choice: Option<TodoChoice>,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.item(todo_id) else {
            return;
        };
        match choice {
            Some(choice) if choice.allowed_for(item.kind) => {
                if choice != TodoChoice::Go {
                    self.delivered.remove(&todo_id);
                }
                self.choices.insert(todo_id, choice);
            }
            Some(_) => return,
            None => {
                self.choices.remove(&todo_id);
                self.delivered.remove(&todo_id);
            }
        }
        cx.notify();
    }

    pub(crate) fn clear_choices(&mut self, todo_ids: &[u64], cx: &mut Context<Self>) {
        let before = self.choices.len();
        self.choices.retain(|todo_id, _| !todo_ids.contains(todo_id));
        self.delivered.retain(|todo_id, _| !todo_ids.contains(todo_id));
        if self.choices.len() != before {
            cx.notify();
        }
    }

    /// Applies the user's choices for `todo_ids`: Done and Skip close the
    /// item; Go sends one message per assigned session (an item with several
    /// assignees is named in each) and marks the item Requested once every
    /// one of its sessions received it. A Go item one of whose sessions
    /// cannot be reached stays as it was, so the user can retry; the others
    /// still go through.
    pub(crate) fn send(
        &mut self,
        todo_ids: &[u64],
        notes: &HashMap<u64, String>,
        mut deliver: impl FnMut(&InboxAsker, &str, &mut App) -> anyhow::Result<()>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<SendReport> {
        let chosen: Vec<(u64, TodoChoice)> = todo_ids
            .iter()
            .filter_map(|todo_id| self.choices.get(todo_id).map(|choice| (*todo_id, *choice)))
            .collect();
        anyhow::ensure!(!chosen.is_empty(), "choose Done, Go or Skip for at least one item");
        let mut report = SendReport::default();
        let mut errors = Vec::new();
        // Notes are checked before anything is sent or closed, so one bad
        // note never leaves a message delivered and its item unchanged.
        let mut valid_notes: HashMap<u64, Option<String>> = HashMap::new();
        let chosen: Vec<(u64, TodoChoice)> = chosen
            .into_iter()
            .filter(|(todo_id, _)| {
                match validate_todo_text("note", notes.get(todo_id).map(String::as_str)) {
                    Ok(note) => {
                        valid_notes.insert(*todo_id, note);
                        true
                    }
                    Err(error) => {
                        errors.push(format!("#{todo_id}: {error:#}"));
                        false
                    }
                }
            })
            .collect();
        let note_for = |todo_id: u64| valid_notes.get(&todo_id).cloned().flatten();
        let mut go_by_asker: Vec<(InboxAsker, Vec<u64>)> = Vec::new();
        for (todo_id, choice) in &chosen {
            match choice {
                TodoChoice::Done | TodoChoice::Skip => {
                    let outcome = if *choice == TodoChoice::Done {
                        TodoOutcome::Completed
                    } else {
                        TodoOutcome::Skipped
                    };
                    let note = note_for(*todo_id);
                    match self.done(*todo_id, TodoCreator::Human, outcome, note.as_deref(), cx) {
                        Ok(()) => report.closed += 1,
                        Err(error) => errors.push(format!("#{todo_id}: {error:#}")),
                    }
                }
                TodoChoice::Go => {
                    let Some(item) = self.item(*todo_id) else {
                        errors.push(format!("#{todo_id}: no such item"));
                        continue;
                    };
                    if item.assigned.is_empty() {
                        errors.push(format!(
                            "#{todo_id}: assign a session before sending Go"
                        ));
                        continue;
                    }
                    let askers: Vec<InboxAsker> =
                        item.assigned.iter().map(|assignee| assignee.asker.clone()).collect();
                    // Receipts hold only for the note they were sent with: an
                    // edited condition must reach every session again.
                    let note = note_for(*todo_id);
                    let already = match self.delivered.get(todo_id) {
                        Some((sent_note, askers)) if *sent_note == note => askers.clone(),
                        _ => {
                            self.delivered.remove(todo_id);
                            Vec::new()
                        }
                    };
                    for asker in askers {
                        // A retry after a partial failure skips the sessions
                        // that already have the request.
                        if already.contains(&asker) {
                            continue;
                        }
                        match go_by_asker.iter_mut().find(|(candidate, _)| *candidate == asker) {
                            Some((_, ids)) => ids.push(*todo_id),
                            None => go_by_asker.push((asker, vec![*todo_id])),
                        }
                    }
                }
            }
        }
        for (asker, ids) in &go_by_asker {
            let entries: Vec<(&TodoItem, Option<&str>)> = ids
                .iter()
                .filter_map(|todo_id| self.item(*todo_id))
                .map(|item| (item, valid_notes.get(&item.id).and_then(|note| note.as_deref())))
                .collect();
            let body = with_from_line("Task Overview", &todo_request_message(&entries));
            match deliver(asker, &body, cx) {
                Ok(()) => {
                    for todo_id in ids {
                        let note = note_for(*todo_id);
                        self.delivered
                            .entry(*todo_id)
                            .or_insert_with(|| (note, Vec::new()))
                            .1
                            .push(asker.clone());
                    }
                }
                Err(error) => {
                    errors.push(format!(
                        "{}: {error:#}",
                        ids.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        let go_ids: Vec<u64> = chosen
            .iter()
            .filter(|(_, choice)| *choice == TodoChoice::Go)
            .map(|(todo_id, _)| *todo_id)
            .collect();
        let now = Utc::now();
        for todo_id in go_ids {
            let Some(item) = self.item(todo_id) else {
                continue;
            };
            // Requested once every current assignee holds this item's
            // request; failures of other items in the same message do not
            // hold it back.
            let received: &[InboxAsker] = self
                .delivered
                .get(&todo_id)
                .map(|(_, askers)| askers.as_slice())
                .unwrap_or_default();
            let unreached = item.assigned.is_empty()
                || item
                    .assigned
                    .iter()
                    .any(|assignee| !received.contains(&assignee.asker));
            if unreached {
                continue;
            }
            let note = note_for(todo_id);
            if let Ok(item) = self.item_mut(todo_id) {
                item.state = TodoState::Requested;
                item.request = Some(TodoRequest {
                    note,
                    at: now,
                    by: TodoCreator::Human,
                });
                item.updated_at = now;
            }
            self.choices.remove(&todo_id);
            self.delivered.remove(&todo_id);
            report.requested += 1;
        }
        if report.closed + report.requested > 0 {
            self.persist(cx);
            cx.notify();
        }
        if errors.is_empty() {
            Ok(report)
        } else {
            Err(anyhow::anyhow!(errors.join("\n")))
        }
    }

    pub(crate) fn persist(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let snapshot = serde_json::to_vec_pretty(&TodoFile {
            version: TODO_FILE_VERSION,
            items: self.items.clone(),
        })
        .map_err(anyhow::Error::from);
        self.writer.write(path, snapshot, cx);
    }

    pub(crate) fn writer(&self) -> &crate::SerialWriter {
        &self.writer
    }
}

pub(crate) enum InboxTodoViewEvent {
    Send {
        todo_ids: Vec<u64>,
        notes: HashMap<u64, String>,
    },
    /// The user wants to pick a session for the item; the app shows the menu.
    Assign {
        todo_id: u64,
        position: Point<Pixels>,
    },
    /// Open the assignee's tab, or bring it back when it is closed.
    OpenAssignee {
        todo_id: u64,
        asker: InboxAsker,
    },
    Unassign {
        todo_id: u64,
        asker: InboxAsker,
    },
    /// Open the assignee's activity popover (permission, completion, failure).
    OpenActivity {
        asker: InboxAsker,
        position: Point<Pixels>,
    },
    /// Stop waiting for every session started from Assign… for the item.
    CancelPendingSession {
        todo_id: u64,
    },
    /// Hide the failure of a session start; other attempts keep waiting.
    DismissSessionError {
        todo_id: u64,
    },
}

/// What the overview knows about one assignee that the view cannot find out
/// itself: whether its tab is open and what state the agent is in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AssigneeRow {
    pub(crate) asker: InboxAsker,
    pub(crate) label: String,
    pub(crate) icon: Option<IconName>,
    /// `None` while the tab is closed.
    pub(crate) state_color: Option<Hsla>,
    pub(crate) state_label: &'static str,
    pub(crate) restoring: bool,
    pub(crate) error: Option<String>,
    /// The card's status button, kept on the row: "Running", "Awaiting
    /// answer", … Click opens the agent's activity.
    pub(crate) status: &'static str,
}

/// Claude Code tabs started for the item from Assign…: the wait for a
/// session, and the last failure, each shown while present.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PendingSession {
    pub(crate) waiting: Option<String>,
    pub(crate) error: Option<String>,
}

/// Which text of an item a [`Markdown`] in [`InboxTodoView::texts`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ItemText {
    Title,
    Detail,
}

/// The To do list of one task (or task-less arena) in Task Overview.
pub(crate) struct InboxTodoView {
    todos: Entity<InboxTodos>,
    scope: TodoScope,
    assignees: HashMap<u64, Vec<AssigneeRow>>,
    pending_sessions: HashMap<u64, PendingSession>,
    /// Shown items' titles and details, kept across renders because the
    /// user's text selection lives in the entity.
    texts: HashMap<(u64, ItemText), Entity<Markdown>>,
    note_editors: HashMap<u64, Entity<Editor>>,
    error: Option<String>,
    last_report: Option<SendReport>,
    show_done: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl InboxTodoView {
    pub(crate) fn new(
        todos: Entity<InboxTodos>,
        scope: TodoScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_in(&todos, window, |this, _, window, cx| {
            this.reconcile(window, cx);
        });
        let mut this = Self {
            todos,
            scope,
            assignees: HashMap::new(),
            pending_sessions: HashMap::new(),
            texts: HashMap::new(),
            note_editors: HashMap::new(),
            error: None,
            last_report: None,
            show_done: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![subscription],
        };
        this.reconcile(window, cx);
        this
    }

    /// Re-points the view at another task. Note editors are kept: choices
    /// live in the store across scopes, so their drafts must too.
    pub(crate) fn set_scope(&mut self, scope: TodoScope, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.error = None;
            self.last_report = None;
            self.reconcile(window, cx);
        }
    }

    /// Called by the overview on every render; notifies only on a change so
    /// the render loop settles.
    pub(crate) fn set_assignees(
        &mut self,
        assignees: HashMap<u64, Vec<AssigneeRow>>,
        pending_sessions: HashMap<u64, PendingSession>,
        cx: &mut Context<Self>,
    ) {
        if self.assignees != assignees || self.pending_sessions != pending_sessions {
            self.assignees = assignees;
            self.pending_sessions = pending_sessions;
            cx.notify();
        }
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    pub(crate) fn set_report(&mut self, report: SendReport, cx: &mut Context<Self>) {
        self.last_report = Some(report);
        cx.notify();
    }

    /// Open items in scope: Pick (proposals) first, then You do, newest first
    /// within each.
    pub(crate) fn open_items(&self, cx: &App) -> Vec<TodoItem> {
        let mut items: Vec<TodoItem> = self
            .todos
            .read(cx)
            .items()
            .iter()
            .filter(|item| item.is_open() && self.scope.contains(item))
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            kind_rank(a.kind)
                .cmp(&kind_rank(b.kind))
                .then(b.updated_at.cmp(&a.updated_at))
        });
        items
    }

    fn done_items(&self, cx: &App) -> Vec<TodoItem> {
        let mut items: Vec<TodoItem> = self
            .todos
            .read(cx)
            .items()
            .iter()
            .filter(|item| !item.is_open() && self.scope.contains(item))
            .cloned()
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        items
    }

    /// Keeps one note editor per chosen item. Editors of chosen items outside
    /// the current scope are kept too: the choice is still in the store, and
    /// the draft must be there when the user comes back.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (chosen_anywhere, chosen_here): (Vec<u64>, Vec<u64>) = {
            let todos = self.todos.read(cx);
            let chosen_anywhere: Vec<u64> = todos
                .items()
                .iter()
                .filter(|item| item.is_open() && todos.choice(item.id).is_some())
                .map(|item| item.id)
                .collect();
            let chosen_here = todos
                .items()
                .iter()
                .filter(|item| item.is_open() && self.scope.contains(item))
                .filter(|item| todos.choice(item.id).is_some())
                .map(|item| item.id)
                .collect();
            (chosen_anywhere, chosen_here)
        };
        self.note_editors
            .retain(|todo_id, _| chosen_anywhere.contains(todo_id));
        for todo_id in chosen_here {
            if self.note_editors.contains_key(&todo_id) {
                continue;
            }
            let editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 4, window, cx);
                editor.set_placeholder_text("Condition or note (optional)", window, cx);
                editor
            });
            self._subscriptions
                .push(cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        cx.notify();
                    }
                }));
            self.note_editors.insert(todo_id, editor);
        }
        self.sync_texts(cx);
        cx.notify();
    }

    /// Keeps one [`Markdown`] per shown title and detail. Done items get
    /// theirs only while the Done list is unfolded.
    fn sync_texts(&mut self, cx: &mut Context<Self>) {
        let mut wanted: HashMap<(u64, ItemText), SharedString> = HashMap::new();
        let mut add = |item: &TodoItem, title: String| {
            wanted.insert((item.id, ItemText::Title), title.into());
            if let Some(detail) = &item.detail {
                wanted.insert((item.id, ItemText::Detail), detail.clone().into());
            }
        };
        for item in self.open_items(cx) {
            add(&item, item.title.clone());
        }
        if self.show_done {
            for item in self.done_items(cx) {
                add(&item, done_title(&item));
            }
        }
        self.texts.retain(|key, _| wanted.contains_key(key));
        for (key, source) in wanted {
            match self.texts.get(&key) {
                Some(markdown) => markdown.update(cx, |markdown, cx| markdown.reset(source, cx)),
                None => {
                    let markdown = cx.new(|cx| Markdown::new_text(source, cx));
                    self.texts.insert(key, markdown);
                }
            }
        }
    }

    fn render_text(&self, todo_id: u64, part: ItemText, style: &MarkdownStyle) -> Option<AnyElement> {
        let markdown = self.texts.get(&(todo_id, part))?;
        Some(
            div()
                .min_w_0()
                .debug_selector(|| format!("todo-text-{todo_id}-{part:?}"))
                .child(MarkdownElement::new(markdown.clone(), style.clone()))
                .into_any_element(),
        )
    }

    fn notes(&self, cx: &App) -> HashMap<u64, String> {
        self.note_editors
            .iter()
            .map(|(todo_id, editor)| (*todo_id, editor.read(cx).text(cx)))
            .collect()
    }

    fn toggle(&mut self, todo_id: u64, choice: TodoChoice, cx: &mut Context<Self>) {
        let current = self.todos.read(cx).choice(todo_id);
        let next = (current != Some(choice)).then_some(choice);
        self.todos
            .update(cx, |todos, cx| todos.set_choice(todo_id, next, cx));
    }

    fn chosen_ids(&self, cx: &App) -> Vec<u64> {
        let todos = self.todos.read(cx);
        self.open_items(cx)
            .iter()
            .filter(|item| todos.choice(item.id).is_some())
            .map(|item| item.id)
            .collect()
    }

    /// The button is disabled when this does not hold; the keyboard shortcut
    /// says why instead.
    fn send_problem(&self, cx: &App) -> Option<String> {
        let todos = self.todos.read(cx);
        let chosen = self.chosen_ids(cx);
        if chosen.is_empty() {
            return Some("Choose Done, Go or Skip for at least one item.".to_string());
        }
        for todo_id in chosen {
            if todos.choice(todo_id) == Some(TodoChoice::Go)
                && todos.item(todo_id).is_some_and(|item| item.assigned.is_empty())
            {
                return Some(format!("#{todo_id} needs a session before Go: use Assign."));
            }
        }
        None
    }

    fn send_answers(&mut self, _: &SendAnswers, _window: &mut Window, cx: &mut Context<Self>) {
        match self.send_problem(cx) {
            Some(problem) => self.set_error(Some(problem), cx),
            None => self.send(cx),
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.last_report = None;
        cx.emit(InboxTodoViewEvent::Send {
            todo_ids: self.chosen_ids(cx),
            notes: self.notes(cx),
        });
        cx.notify();
    }

    fn render_assignees(&self, item: &TodoItem, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let todo_id = item.id;
        let rows = self.assignees.get(&todo_id).cloned().unwrap_or_default();
        v_flex().min_w_0().gap_1().child(
        h_flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .text_xs()
            .when(rows.is_empty(), |row| {
                row.child(div().text_color(colors.text_muted).child("Unassigned"))
            })
            .children(rows.into_iter().enumerate().map(|(index, assignee)| {
                let asker = assignee.asker.clone();
                let unassign_asker = assignee.asker.clone();
                let activity_asker = assignee.asker.clone();
                let dot_color = assignee.state_color.unwrap_or(colors.text_muted);
                let tooltip = match (&assignee.error, assignee.restoring, assignee.state_color) {
                    (Some(error), _, _) => error.clone(),
                    (None, true, _) => "Restoring the tab…".to_string(),
                    (None, false, None) => "Tab closed · click to restore".to_string(),
                    (None, false, Some(_)) => format!("{} · click to open", assignee.state_label),
                };
                h_flex()
                    .id(SharedString::from(format!("todo-assignee-{todo_id}-{index}")))
                    .gap_1()
                    .items_center()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(colors.element_hover))
                    .tooltip(ui::Tooltip::text(tooltip))
                    .child(div().size(gpui::px(8.0)).rounded_full().bg(dot_color))
                    .when_some(assignee.icon, |row, icon| {
                        row.child(Icon::new(icon).size(IconSize::XSmall))
                    })
                    .child(
                        div()
                            .when(assignee.state_color.is_none(), |d| d.text_color(colors.text_muted))
                            .child(assignee.label),
                    )
                    .when(assignee.state_color.is_some(), |row| {
                        row.child(
                            Button::new(
                                SharedString::from(format!("todo-activity-{todo_id}-{index}")),
                                assignee.status,
                            )
                            .label_size(LabelSize::Small)
                            .tooltip(ui::Tooltip::text("Show the agent's activity"))
                            .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(InboxTodoViewEvent::OpenActivity {
                                    asker: activity_asker.clone(),
                                    position: event.position(),
                                });
                            })),
                        )
                    })
                    .child(
                        ui::IconButton::new(
                            SharedString::from(format!("todo-unassign-{todo_id}-{index}")),
                            IconName::Close,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(ui::Tooltip::text("Unassign"))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(InboxTodoViewEvent::Unassign {
                                todo_id,
                                asker: unassign_asker.clone(),
                            });
                        })),
                    )
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(InboxTodoViewEvent::OpenAssignee {
                            todo_id,
                            asker: asker.clone(),
                        });
                    }))
            }))
            .child(
                Button::new(
                    SharedString::from(format!("todo-assign-{todo_id}")),
                    "Assign…",
                )
                .label_size(LabelSize::Small)
                .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(InboxTodoViewEvent::Assign {
                        todo_id,
                        position: event.position(),
                    });
                })),
            ))
            .when_some(self.pending_sessions.get(&todo_id).cloned(), |column, pending| {
                let status_colors = cx.theme().status().clone();
                column
                    .when_some(pending.waiting, |column, waiting| {
                        column.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .child(div().text_color(colors.text_muted).child(waiting))
                                .child(
                                    Button::new(
                                        SharedString::from(format!("todo-pending-cancel-{todo_id}")),
                                        "Cancel",
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(InboxTodoViewEvent::CancelPendingSession { todo_id });
                                    })),
                                ),
                        )
                    })
                    .when_some(pending.error, |column, error| {
                        column.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .child(div().text_color(status_colors.error).child(error))
                                .child(
                                    Button::new(
                                        SharedString::from(format!("todo-pending-dismiss-{todo_id}")),
                                        "Dismiss",
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(InboxTodoViewEvent::DismissSessionError { todo_id });
                                    })),
                                ),
                        )
                    })
            })
    }

    fn render_item(
        &self,
        number: usize,
        item: &TodoItem,
        choice: Option<TodoChoice>,
        styles: &TextStyles,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let todo_color = inbox_todo_color();
        let todo_id = item.id;
        let choices: &[TodoChoice] = match item.kind {
            InboxTodoKind::UserTask => &[TodoChoice::Done],
            InboxTodoKind::Proposal => &[TodoChoice::Go, TodoChoice::Skip],
        };
        let buttons = choices.iter().map(|option| {
            let option = *option;
            Button::new(
                SharedString::from(format!("todo-{todo_id}-{}", option.label())),
                option.label(),
            )
            .label_size(LabelSize::Small)
            .toggle_state(choice == Some(option))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.toggle(todo_id, option, cx);
            }))
        });
        let state_label = match item.state {
            TodoState::Open => None,
            TodoState::Assigned => Some("Assigned"),
            TodoState::Requested => Some("Requested"),
            TodoState::Done => Some("Done"),
        };
        v_flex()
            .flex_shrink_0()
            .min_w_0()
            .p_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(if choice.is_some() {
                todo_color
            } else {
                colors.border_variant
            })
            .bg(colors.element_background)
            .when(choice == Some(TodoChoice::Skip), |d| d.opacity(0.6))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child(format!("{number}.")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(self.render_text(todo_id, ItemText::Title, &styles.open_title)),
                    )
                    .when_some(state_label, |row, label| {
                        row.child(div().text_xs().text_color(colors.text_muted).child(label))
                    })
                    .children(buttons),
            )
            .children(self.render_text(todo_id, ItemText::Detail, &styles.open_detail))
            .when_some(
                item.request.as_ref().and_then(|request| request.note.clone()),
                |d, note| {
                    d.child(
                        div()
                            .text_sm()
                            .text_color(colors.text_accent)
                            .child(format!("Condition: {note}")),
                    )
                },
            )
            .child(self.render_assignees(item, cx))
            .when_some(
                choice.and_then(|_| self.note_editors.get(&todo_id).cloned()),
                |d, editor| {
                    d.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .child(editor),
                    )
                },
            )
    }

    fn render_done(&self, styles: &TextStyles, cx: &mut Context<Self>) -> impl IntoElement {
        let done = self.done_items(cx);
        let colors = cx.theme().colors().clone();
        v_flex()
            .min_w_0()
            .flex_shrink_0()
            .gap_2()
            .when(!done.is_empty(), |column| {
                column
                    .child(
                        Button::new(
                            "done-todos",
                            format!(
                                "{} Done · {}",
                                if self.show_done { "▾" } else { "▸" },
                                done.len()
                            ),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_done = !this.show_done;
                            this.sync_texts(cx);
                            cx.notify();
                        })),
                    )
                    .when(self.show_done, |column| {
                        column.children(done.into_iter().map(|item| {
                            let (state, note) = match &item.resolution {
                                Some(resolution) => (
                                    match resolution.outcome {
                                        TodoOutcome::Completed => "Done",
                                        TodoOutcome::Skipped => "Skipped",
                                        TodoOutcome::Dismissed => "Dismissed",
                                    },
                                    resolution.note.clone(),
                                ),
                                None => ("Done", None),
                            };
                            v_flex()
                                .min_w_0()
                                .flex_shrink_0()
                                .gap_1()
                                .p_2()
                                .border_1()
                                .border_color(colors.border_variant)
                                .rounded_md()
                                .child(ui::Label::new(state))
                                .children(self.render_text(item.id, ItemText::Title, &styles.done_title))
                                .children(self.render_text(item.id, ItemText::Detail, &styles.done_detail))
                                .when_some(
                                    item.request.as_ref().and_then(|request| request.note.clone()),
                                    |row, condition| {
                                        row.child(div().text_sm().child(format!("Condition: {condition}")))
                                    },
                                )
                                .when_some(note, |row, note| {
                                    row.child(div().text_sm().child(format!("Note: {note}")))
                                })
                        }))
                    })
            })
    }
}

fn kind_rank(kind: InboxTodoKind) -> u8 {
    match kind {
        InboxTodoKind::Proposal => 0,
        InboxTodoKind::UserTask => 1,
    }
}

fn done_title(item: &TodoItem) -> String {
    format!("#{} {}", item.id, item.title)
}

/// A [`MarkdownElement`] does not inherit the surrounding div's text style,
/// so each kind of item text carries its own.
struct TextStyles {
    open_title: MarkdownStyle,
    open_detail: MarkdownStyle,
    done_title: MarkdownStyle,
    done_detail: MarkdownStyle,
}

impl TextStyles {
    fn new(window: &Window, cx: &App) -> Self {
        let colors = cx.theme().colors();
        let style = |refinement: TextStyleRefinement| {
            let mut text_style = window.text_style();
            text_style.refine(&refinement);
            // Text runs carry no size: lines are sized by the container.
            let mut container_style = StyleRefinement::default();
            container_style.text.font_size = refinement.font_size;
            MarkdownStyle {
                base_text_style: text_style,
                container_style,
                selection_background_color: colors.element_selection_background,
                link: TextStyleRefinement {
                    color: Some(colors.text_accent),
                    underline: Some(UnderlineStyle {
                        color: Some(colors.text_accent.opacity(0.5)),
                        thickness: px(1.),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }
        };
        let small = || TextStyleRefinement {
            font_size: Some(rems(0.875).into()),
            ..Default::default()
        };
        Self {
            open_title: style(TextStyleRefinement {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            }),
            open_detail: style(TextStyleRefinement {
                color: Some(colors.text_muted),
                ..small()
            }),
            done_title: style(TextStyleRefinement {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..small()
            }),
            done_detail: style(small()),
        }
    }
}

impl EventEmitter<InboxTodoViewEvent> for InboxTodoView {}

impl Focusable for InboxTodoView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InboxTodoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let styles = TextStyles::new(window, cx);
        let items = self.open_items(cx);
        let choices: Vec<Option<TodoChoice>> = {
            let todos = self.todos.read(cx);
            items.iter().map(|item| todos.choice(item.id)).collect()
        };
        let can_send = self.send_problem(cx).is_none();
        let has_choices = choices.iter().any(|choice| choice.is_some());
        let chosen_ids = self.chosen_ids(cx);

        let group = |label: &'static str, hint: &'static str| {
            h_flex()
                .gap_2()
                .mt_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_muted)
                        .child(label),
                )
                .child(div().text_xs().text_color(colors.text_placeholder).child(hint))
        };
        let mut proposals = Vec::new();
        let mut user_tasks = Vec::new();
        for (index, (item, choice)) in items.iter().zip(choices).enumerate() {
            let element = self.render_item(index + 1, item, choice, &styles, cx).into_any_element();
            match item.kind {
                InboxTodoKind::Proposal => proposals.push(element),
                InboxTodoKind::UserTask => user_tasks.push(element),
            }
        }
        let report = self.last_report.as_ref().map(|report| {
            format!(
                "Sent: {} requested, {} closed.",
                report.requested, report.closed
            )
        });

        v_flex()
            .id("inbox-todo")
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::send_answers))
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .gap_2()
            .track_focus(&self.focus_handle)
            .when(items.is_empty(), |column| {
                column.child(div().text_color(colors.text_muted).child("No pending To do items."))
            })
            .when(!proposals.is_empty(), |d| {
                d.child(group("Pick", "Go to have the agent do it, Skip to drop it"))
                    .children(proposals)
            })
            .when(!user_tasks.is_empty(), |d| {
                d.child(group("You do", "Done when finished")).children(user_tasks)
            })
            .when(!items.is_empty(), |column| {
                column.child(
                    v_flex()
                        .mt_2()
                        .gap_1()
                        .when_some(self.error.clone(), |d, error| {
                            d.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().status().error)
                                    .child(error),
                            )
                        })
                        .when_some(report, |d, report| {
                            d.child(div().text_sm().text_color(colors.text_muted).child(report))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("inbox-todo-send", "Send")
                                        .disabled(!can_send)
                                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                            this.send(cx);
                                        })),
                                )
                                .child(
                                    Button::new("inbox-todo-clear", "Clear choices")
                                        .disabled(!has_choices)
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                            let ids = chosen_ids.clone();
                                            this.todos
                                                .update(cx, |todos, cx| todos.clear_choices(&ids, cx));
                                        })),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(colors.text_placeholder)
                                        .child("cmd-enter sends. Go asks the assigned agent; it closes the item with `todo done` when finished."),
                                ),
                        ),
                )
            })
            .child(self.render_done(&styles, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that would pull gpui's `test` attribute macro into
    // scope and make `#[test]` expand recursively.
    use super::{
        Assignee, InboxTodoKind, InboxTodoView, InboxTodoViewEvent, InboxTodos, PendingRestore,
        RestoreObservation, RestoreOutcome, RestorePlan, SendAnswers, TodoChoice, TodoCreator,
        TodoDraft, TodoItem, TodoOutcome, TodoScope, TodoState, acp_restore_outcome,
        advance_pending_restores, find_item_location, parse_todo_file, prune_done,
        record_restore_launch, todo_request_message, todo_restore_plan,
    };
    use crate::InboxAsker;
    use crate::arena::Arena;
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use uuid::Uuid;
    use workspace::item::test::TestItem;

    fn claude(session: &str) -> InboxAsker {
        InboxAsker::ClaudeSession {
            session_id: session.to_string(),
        }
    }

    fn assignee(session: &str, path: Option<&str>) -> Assignee {
        Assignee {
            asker: claude(session),
            arena_path: path.map(PathBuf::from),
            arena_name: "zed".to_string(),
        }
    }

    fn draft(kind: InboxTodoKind, title: &str, task_id: Option<Uuid>) -> TodoDraft {
        TodoDraft {
            task_id,
            arena_path: Some(PathBuf::from("/src/zed")),
            arena_name: "zed".to_string(),
            branch: None,
            kind,
            title: title.to_string(),
            detail: None,
            created_by: TodoCreator::Human,
            assignee: None,
        }
    }

    fn session_draft(kind: InboxTodoKind, title: &str, session: &str, unassigned: bool) -> TodoDraft {
        TodoDraft {
            created_by: TodoCreator::Session {
                asker: claude(session),
            },
            assignee: (!unassigned).then(|| assignee(session, Some("/src/zed"))),
            ..draft(kind, title, None)
        }
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            release_channel::init(semver::Version::new(0, 1, 0), cx);
        });
    }

    fn store(cx: &mut TestAppContext) -> Entity<InboxTodos> {
        cx.new(|_| InboxTodos::new(None))
    }

    #[test]
    fn task_id_is_authoritative_for_scope_matching() {
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();
        let mut item = TodoItem {
            id: 1,
            task_id: Some(task_a),
            arena_path: Some(PathBuf::from("/shared")),
            arena_name: "shared".to_string(),
            branch: None,
            kind: InboxTodoKind::Proposal,
            title: "x".to_string(),
            detail: None,
            created_by: TodoCreator::Human,
            assigned: Vec::new(),
            state: TodoState::Open,
            request: None,
            resolution: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let scope_a = TodoScope {
            task_id: Some(task_a),
            arena_paths: vec![],
            arena_names: vec![],
        };
        // Task B shares the worktree: the item still belongs to A only.
        let scope_b = TodoScope {
            task_id: Some(task_b),
            arena_paths: vec![PathBuf::from("/shared")],
            arena_names: vec!["shared".to_string()],
        };
        let arena_scope = TodoScope {
            task_id: None,
            arena_paths: vec![PathBuf::from("/shared")],
            arena_names: vec!["shared".to_string()],
        };
        assert!(scope_a.contains(&item));
        assert!(!scope_b.contains(&item));
        assert!(!arena_scope.contains(&item));

        // Task-less items fall back to the path, then the name, and so show
        // under every scope that includes their arena.
        item.task_id = None;
        assert!(arena_scope.contains(&item));
        assert!(scope_b.contains(&item));
        assert!(!scope_a.contains(&item));
        item.arena_path = None;
        assert!(arena_scope.contains(&item));
        item.arena_name = "other".to_string();
        assert!(!arena_scope.contains(&item));
    }

    #[gpui::test]
    fn adding_sets_the_state_from_creator_and_assignee(cx: &mut TestAppContext) {
        let todos = store(cx);
        let mut next_id = 7;
        let (own, unassigned, human) = todos.update(cx, |todos, cx| {
            (
                todos.add(session_draft(InboxTodoKind::Proposal, "Add tests", "s1", false), &mut next_id, cx).unwrap(),
                todos.add(session_draft(InboxTodoKind::UserTask, "Check CI", "s1", true), &mut next_id, cx).unwrap(),
                todos.add(draft(InboxTodoKind::Proposal, "Human idea", None), &mut next_id, cx).unwrap(),
            )
        });
        assert_eq!((own, unassigned, human, next_id), (7, 8, 9, 10));
        todos.read_with(cx, |todos, _| {
            let own = todos.item(own).unwrap();
            assert_eq!(own.state, TodoState::Requested);
            assert!(own.request.as_ref().is_some_and(|request| request.note.is_none()));
            assert!(own.is_assigned_to(&claude("s1")));
            assert_eq!(todos.item(unassigned).unwrap().state, TodoState::Open);
            assert_eq!(todos.item(human).unwrap().state, TodoState::Open);
        });
        let rejected = todos.update(cx, |todos, cx| {
            todos.add(draft(InboxTodoKind::Proposal, "   ", None), &mut next_id, cx)
        });
        assert!(rejected.is_err());
        let long = "x".repeat(super::MAX_TODO_TITLE_CHARS + 1);
        let rejected = todos.update(cx, |todos, cx| {
            todos.add(draft(InboxTodoKind::Proposal, &long, None), &mut next_id, cx)
        });
        assert!(rejected.is_err());
    }

    #[gpui::test]
    fn only_assignees_close_or_edit_but_anyone_assigns(cx: &mut TestAppContext) {
        let todos = store(cx);
        let mut next_id = 1;
        let id = todos.update(cx, |todos, cx| {
            todos.add(draft(InboxTodoKind::Proposal, "Add tests", None), &mut next_id, cx).unwrap()
        });
        let stranger = TodoCreator::Session { asker: claude("s2") };
        let result = todos.update(cx, |todos, cx| {
            todos.done(id, stranger.clone(), TodoOutcome::Completed, None, cx)
        });
        assert!(result.is_err());
        let result = todos.update(cx, |todos, cx| todos.update(id, &stranger, Some("New"), None, cx));
        assert!(result.is_err());

        // A stranger may still assign (itself or another session), then close.
        todos.update(cx, |todos, cx| todos.assign(id, assignee("s2", None), cx)).unwrap();
        todos.read_with(cx, |todos, _| {
            assert_eq!(todos.item(id).unwrap().state, TodoState::Assigned);
        });
        todos.update(cx, |todos, cx| todos.update(id, &stranger, Some("New title"), Some(" more "), cx)).unwrap();
        todos.update(cx, |todos, cx| {
            todos.done(id, stranger.clone(), TodoOutcome::Completed, Some("pushed"), cx)
        }).unwrap();
        todos.read_with(cx, |todos, _| {
            let item = todos.item(id).unwrap();
            assert_eq!(item.state, TodoState::Done);
            assert_eq!(item.title, "New title");
            assert_eq!(item.detail.as_deref(), Some("more"));
            assert!(item.request.is_none());
            let resolution = item.resolution.as_ref().unwrap();
            assert_eq!(resolution.outcome, TodoOutcome::Completed);
            assert_eq!(resolution.note.as_deref(), Some("pushed"));
        });
        // Done items reject every further write.
        assert!(todos.update(cx, |todos, cx| todos.assign(id, assignee("s3", None), cx)).is_err());
    }

    #[gpui::test]
    fn take_requests_and_unassign_reopens(cx: &mut TestAppContext) {
        let todos = store(cx);
        let mut next_id = 1;
        let id = todos.update(cx, |todos, cx| {
            todos.add(draft(InboxTodoKind::UserTask, "Verify", None), &mut next_id, cx).unwrap()
        });
        todos.update(cx, |todos, cx| todos.take(id, assignee("s1", Some("/src/zed")), cx)).unwrap();
        todos.read_with(cx, |todos, _| {
            let item = todos.item(id).unwrap();
            assert_eq!(item.state, TodoState::Requested);
            assert_eq!(
                item.request.as_ref().map(|request| request.by.clone()),
                Some(TodoCreator::Session { asker: claude("s1") })
            );
        });
        assert!(todos.update(cx, |todos, cx| todos.unassign(id, &claude("nobody"), cx)).is_err());
        todos.update(cx, |todos, cx| todos.unassign(id, &claude("s1"), cx)).unwrap();
        todos.read_with(cx, |todos, _| {
            let item = todos.item(id).unwrap();
            assert!(item.assigned.is_empty());
            assert_eq!(item.state, TodoState::Open);
        });

        // A user's Go condition outlives unassign + take by another session.
        let requested = todos.update(cx, |todos, cx| {
            let id = todos.add(draft(InboxTodoKind::Proposal, "Conditioned", None), &mut next_id, cx).unwrap();
            todos.assign(id, assignee("s1", None), cx).unwrap();
            todos.set_choice(id, Some(TodoChoice::Go), cx);
            todos.send(&[id], &HashMap::from([(id, "only on main".to_string())]), |_, _, _| Ok(()), cx).unwrap();
            todos.unassign(id, &claude("s1"), cx).unwrap();
            todos.take(id, assignee("s2", None), cx).unwrap();
            id
        });
        todos.read_with(cx, |todos, _| {
            let item = todos.item(requested).unwrap();
            assert_eq!(item.state, TodoState::Requested);
            let request = item.request.as_ref().unwrap();
            assert_eq!(request.note.as_deref(), Some("only on main"));
            assert_eq!(request.by, TodoCreator::Human);
        });
    }

    #[gpui::test]
    fn sending_closes_done_and_skip_and_requests_go_per_session(cx: &mut TestAppContext) {
        let todos = store(cx);
        let mut next_id = 1;
        let (done_id, skip_id, go_a, go_b, go_unassigned) = todos.update(cx, |todos, cx| {
            let done_id = todos.add(draft(InboxTodoKind::UserTask, "Check the lane", None), &mut next_id, cx).unwrap();
            let skip_id = todos.add(draft(InboxTodoKind::Proposal, "Open a PR", None), &mut next_id, cx).unwrap();
            let go_a = todos.add(draft(InboxTodoKind::Proposal, "Add tests", None), &mut next_id, cx).unwrap();
            let go_b = todos.add(draft(InboxTodoKind::Proposal, "Fix flake", None), &mut next_id, cx).unwrap();
            let go_unassigned = todos.add(draft(InboxTodoKind::Proposal, "Nobody", None), &mut next_id, cx).unwrap();
            todos.assign(go_a, assignee("s1", None), cx).unwrap();
            todos.assign(go_b, assignee("s1", None), cx).unwrap();
            todos.set_choice(done_id, Some(TodoChoice::Done), cx);
            todos.set_choice(skip_id, Some(TodoChoice::Skip), cx);
            todos.set_choice(go_a, Some(TodoChoice::Go), cx);
            todos.set_choice(go_b, Some(TodoChoice::Go), cx);
            todos.set_choice(go_unassigned, Some(TodoChoice::Go), cx);
            // Done does not fit a proposal: ignored.
            todos.set_choice(skip_id, Some(TodoChoice::Done), cx);
            (done_id, skip_id, go_a, go_b, go_unassigned)
        });
        let notes = HashMap::from([
            (go_a, "with the Failed lane".to_string()),
            (skip_id, "not now".to_string()),
        ]);
        let ids = [done_id, skip_id, go_a, go_b, go_unassigned];

        // Delivery rejected: the Go items stay with their choices; the rest
        // still closed. The unassigned Go is reported.
        let result = todos.update(cx, |todos, cx| {
            todos.send(&ids, &notes, |_, _, _| anyhow::bail!("tab is busy"), cx)
        });
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("tab is busy") && error.contains("assign a session"));
        todos.read_with(cx, |todos, _| {
            assert_eq!(todos.item(done_id).unwrap().state, TodoState::Done);
            let skipped = todos.item(skip_id).unwrap();
            assert_eq!(skipped.resolution.as_ref().unwrap().outcome, TodoOutcome::Skipped);
            assert_eq!(skipped.resolution.as_ref().unwrap().note.as_deref(), Some("not now"));
            assert!(skipped.request.is_none());
            assert_eq!(todos.item(go_a).unwrap().state, TodoState::Assigned);
            assert_eq!(todos.choice(go_a), Some(TodoChoice::Go));
        });

        let delivered = Rc::new(RefCell::new(Vec::new()));
        let result = todos.update(cx, |todos, cx| {
            let delivered = delivered.clone();
            todos.send(&[go_a, go_b], &notes, move |asker, text, _| {
                delivered.borrow_mut().push((asker.clone(), text.to_string()));
                Ok(())
            }, cx)
        });
        let report = result.expect("delivered");
        assert_eq!((report.requested, report.closed), (2, 0));
        let delivered = delivered.borrow();
        assert_eq!(delivered.len(), 1, "one message per session");
        assert_eq!(delivered[0].0, claude("s1"));
        assert_eq!(
            delivered[0].1,
            format!(
                "[from: Task Overview]\nTo do\nGo: #{go_a} Add tests — Note: with the Failed lane\nGo: #{go_b} Fix flake\nRun `agentium todo done <id>` for each item once it is finished."
            )
        );
        todos.read_with(cx, |todos, _| {
            let item = todos.item(go_a).unwrap();
            assert_eq!(item.state, TodoState::Requested);
            assert_eq!(item.request.as_ref().unwrap().note.as_deref(), Some("with the Failed lane"));
            assert_eq!(item.request.as_ref().unwrap().by, TodoCreator::Human);
            assert!(todos.choice(go_a).is_none());
        });
        // Two assignees: both get a message naming the item, and the item is
        // Requested only once both were reached.
        let shared = todos.update(cx, |todos, cx| {
            let shared = todos.add(draft(InboxTodoKind::Proposal, "Shared", None), &mut next_id, cx).unwrap();
            todos.assign(shared, assignee("s1", None), cx).unwrap();
            todos.assign(shared, assignee("s2", None), cx).unwrap();
            todos.set_choice(shared, Some(TodoChoice::Go), cx);
            shared
        });
        let shared_notes = HashMap::from([(shared, "carefully".to_string())]);
        let result = todos.update(cx, |todos, cx| {
            todos.send(&[shared], &shared_notes, |asker, _, _| {
                if *asker == claude("s2") { anyhow::bail!("s2 is busy") } else { Ok(()) }
            }, cx)
        });
        assert!(result.is_err());
        todos.read_with(cx, |todos, _| {
            assert_eq!(todos.item(shared).unwrap().state, TodoState::Assigned);
            assert_eq!(todos.choice(shared), Some(TodoChoice::Go));
        });
        // The retry reaches only the session that missed the request.
        let reached = Rc::new(RefCell::new(Vec::new()));
        let report = todos.update(cx, |todos, cx| {
            let reached = reached.clone();
            todos.send(&[shared], &shared_notes, move |asker, _, _| {
                reached.borrow_mut().push(asker.clone());
                Ok(())
            }, cx)
        }).expect("both reached");
        assert_eq!(report.requested, 1);
        assert_eq!(*reached.borrow(), vec![claude("s2")]);
        todos.read_with(cx, |todos, _| {
            let item = todos.item(shared).unwrap();
            assert_eq!(item.state, TodoState::Requested);
            assert_eq!(item.request.as_ref().unwrap().note.as_deref(), Some("carefully"));
        });

        // Mixed retry: X (A done, B pending) sent together with Y (A busy).
        // B receives X, A rejects Y; X is Requested, Y is not.
        let (x, y) = todos.update(cx, |todos, cx| {
            let x = todos.add(draft(InboxTodoKind::Proposal, "X", None), &mut next_id, cx).unwrap();
            let y = todos.add(draft(InboxTodoKind::Proposal, "Y", None), &mut next_id, cx).unwrap();
            todos.assign(x, assignee("s1", None), cx).unwrap();
            todos.assign(x, assignee("s2", None), cx).unwrap();
            todos.assign(y, assignee("s1", None), cx).unwrap();
            todos.set_choice(x, Some(TodoChoice::Go), cx);
            todos.set_choice(y, Some(TodoChoice::Go), cx);
            todos.send(&[x], &HashMap::new(), |asker, _, _| {
                if *asker == claude("s2") { anyhow::bail!("s2 is busy") } else { Ok(()) }
            }, cx).unwrap_err();
            (x, y)
        });
        let result = todos.update(cx, |todos, cx| {
            todos.send(&[x, y], &HashMap::new(), |asker, _, _| {
                if *asker == claude("s1") { anyhow::bail!("s1 is busy") } else { Ok(()) }
            }, cx)
        });
        assert!(result.is_err());
        todos.read_with(cx, |todos, _| {
            assert_eq!(todos.item(x).unwrap().state, TodoState::Requested);
            assert_eq!(todos.item(y).unwrap().state, TodoState::Assigned);
            assert_eq!(todos.choice(y), Some(TodoChoice::Go));
        });

        // Editing the note after a partial delivery starts over: every
        // session gets the new condition, and that is what is recorded.
        let edited = todos.update(cx, |todos, cx| {
            let id = todos.add(draft(InboxTodoKind::Proposal, "Edited", None), &mut next_id, cx).unwrap();
            todos.assign(id, assignee("s1", None), cx).unwrap();
            todos.assign(id, assignee("s2", None), cx).unwrap();
            todos.set_choice(id, Some(TodoChoice::Go), cx);
            todos.send(&[id], &HashMap::from([(id, "N1".to_string())]), |asker, _, _| {
                if *asker == claude("s2") { anyhow::bail!("s2 is busy") } else { Ok(()) }
            }, cx).unwrap_err();
            id
        });
        let reached = Rc::new(RefCell::new(Vec::new()));
        todos.update(cx, |todos, cx| {
            let reached = reached.clone();
            todos.send(&[edited], &HashMap::from([(edited, "N2".to_string())]), move |asker, text, _| {
                reached.borrow_mut().push((asker.clone(), text.contains("Note: N2")));
                Ok(())
            }, cx)
        }).expect("resent to both");
        assert_eq!(reached.borrow().len(), 2);
        assert!(reached.borrow().iter().all(|(_, has_new_note)| *has_new_note));
        todos.read_with(cx, |todos, _| {
            assert_eq!(todos.item(edited).unwrap().request.as_ref().unwrap().note.as_deref(), Some("N2"));
        });

        // An overlong Go note is rejected before anything is sent.
        let long_id = todos.update(cx, |todos, cx| {
            let id = todos.add(draft(InboxTodoKind::Proposal, "Long", None), &mut next_id, cx).unwrap();
            todos.assign(id, assignee("s1", None), cx).unwrap();
            todos.set_choice(id, Some(TodoChoice::Go), cx);
            id
        });
        let long_note = HashMap::from([(long_id, "x".repeat(super::MAX_TODO_TEXT_CHARS + 1))]);
        let sent = Rc::new(Cell::new(false));
        let result = todos.update(cx, |todos, cx| {
            let sent = sent.clone();
            todos.send(&[long_id], &long_note, move |_, _, _| { sent.set(true); Ok(()) }, cx)
        });
        assert!(result.is_err() && !sent.get());
        assert!(todos.read_with(cx, |todos, _| todos.item(long_id).unwrap().state == TodoState::Assigned));

        // The condition survives reassignment and closing.
        todos.update(cx, |todos, cx| {
            todos.unassign(go_a, &claude("s1"), cx).unwrap();
            todos.assign(go_a, assignee("s2", None), cx).unwrap();
            todos.done(go_a, TodoCreator::Session { asker: claude("s2") }, TodoOutcome::Completed, None, cx).unwrap();
        });
        todos.read_with(cx, |todos, _| {
            let item = todos.item(go_a).unwrap();
            assert_eq!(item.request.as_ref().unwrap().note.as_deref(), Some("with the Failed lane"));
            assert!(item.resolution.is_some());
        });
    }

    #[test]
    fn legacy_lists_become_items_with_the_tab_as_assignee() {
        let task = Uuid::new_v4();
        let content = serde_json::json!([
            {
                "id": 5,
                "title": "Fix the Inbox",
                "arena_name": "zed",
                "updated_at": "2026-10-01T00:00:00Z",
                "asker": { "kind": "claude_session", "session_id": "c8832ec2" },
                "items": [
                    { "kind": "user_task", "title": "Check the lane", "detail": "red" },
                    { "kind": "proposal", "title": "Add tests" }
                ],
                "branch": "feature",
                "arena_path": "/src/zed"
            },
            {
                "id": 2,
                "title": "shell",
                "arena_name": "scala",
                "updated_at": "2026-10-02T00:00:00Z",
                "asker": null,
                "items": [{ "kind": "proposal", "title": "Legacy" }]
            }
        ]);
        let resolve = |path: &Path| (path == Path::new("/src/zed")).then_some(task);
        let mut next_id = 10;
        let (items, migrated) = parse_todo_file(content.to_string().as_bytes(), &resolve, &mut next_id)
            .expect("migrated");
        assert!(migrated);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, 10);
        assert_eq!(items[0].task_id, Some(task));
        assert_eq!(items[0].branch.as_deref(), Some("feature"));
        assert_eq!(items[0].state, TodoState::Requested);
        assert_eq!(items[0].assigned, vec![assignee("c8832ec2", Some("/src/zed"))]);
        assert_eq!(items[0].created_by, TodoCreator::Session { asker: claude("c8832ec2") });
        assert_eq!(items[0].detail.as_deref(), Some("red"));
        assert_eq!(items[2].task_id, None);
        assert_eq!(items[2].arena_path, None);
        assert_eq!(items[2].arena_name, "scala");
        assert_eq!(items[2].state, TodoState::Open);
        assert_eq!(items[2].created_by, TodoCreator::Human);
        assert!(items[2].assigned.is_empty());
        // Past the migrated items and past the old list ids.
        assert_eq!(next_id, 13);

        let file = serde_json::json!({ "version": 2, "items": items });
        let (reread, migrated) = parse_todo_file(file.to_string().as_bytes(), &resolve, &mut next_id).expect("v2");
        assert_eq!(reread, items);
        assert!(!migrated);
        assert!(parse_todo_file(br#"{ "version": 3, "items": [] }"#, &resolve, &mut next_id).is_err());
    }

    #[test]
    fn done_items_are_dropped_after_the_retention() {
        let now = chrono::Utc::now();
        let mut items = Vec::new();
        for (id, days_ago) in [(1, 0), (2, 29), (3, 31)] {
            let mut item = TodoItem {
                id,
                task_id: None,
                arena_path: None,
                arena_name: "zed".to_string(),
                branch: None,
                kind: InboxTodoKind::Proposal,
                title: "x".to_string(),
                detail: None,
                created_by: TodoCreator::Human,
                assigned: Vec::new(),
                state: TodoState::Done,
                request: None,
                resolution: Some(super::TodoResolution {
                    outcome: TodoOutcome::Completed,
                    note: None,
                    at: now - chrono::Duration::days(days_ago),
                    by: TodoCreator::Human,
                }),
                created_at: now,
                updated_at: now,
            };
            if id == 3 {
                // An open item is never pruned, however old.
                items.push(TodoItem { id: 4, state: TodoState::Open, resolution: None, ..item.clone() });
            }
            item.id = id;
            items.push(item);
        }
        let kept: Vec<u64> = prune_done(items, now).into_iter().map(|item| item.id).collect();
        assert_eq!(kept, vec![1, 2, 4]);
    }

    #[test]
    fn the_request_message_names_items_by_id() {
        let item = |id, title: &str, detail: Option<&str>| TodoItem {
            id,
            task_id: None,
            arena_path: None,
            arena_name: "zed".to_string(),
            branch: None,
            kind: InboxTodoKind::Proposal,
            title: title.to_string(),
            detail: detail.map(str::to_string),
            created_by: TodoCreator::Human,
            assigned: Vec::new(),
            state: TodoState::Assigned,
            request: None,
            resolution: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let a = item(3, "Add tests", Some("rebinding"));
        let b = item(8, "Open a PR", None);
        assert_eq!(
            todo_request_message(&[(&a, Some("quickly")), (&b, None)]),
            "To do\nGo: #3 Add tests :: rebinding — Note: quickly\nGo: #8 Open a PR\nRun `agentium todo done <id>` for each item once it is finished."
        );
    }

    /// An arena with two panes (`left`, `right`) and an owner tab in `left`.
    async fn arena_with_owner(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Arena>,
        Entity<workspace::Pane>,
        Entity<workspace::Pane>,
        Entity<TestItem>,
        &mut VisualTestContext,
    ) {
        init(cx);
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let cx = cx.add_empty_window();
        let arena = cx.new_window_entity(|window, cx| {
            Arena::test_empty("Test".into(), project, "/arena".into(), window, cx)
        });
        let left = arena.read_with(cx, |arena, _| arena.active_pane.clone());
        let owner = cx.new(TestItem::new);
        left.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(owner.clone()), true, true, None, window, cx);
        });
        let filler = cx.new(TestItem::new);
        let right = arena.update_in(cx, |arena, window, cx| {
            arena.split_pane_with_item(
                &left,
                Box::new(filler),
                workspace::SplitDirection::Right,
                window,
                cx,
            )
        });
        (arena, left, right, owner, cx)
    }

    #[gpui::test]
    async fn the_owner_is_found_where_it_moved_and_lost_when_closed(cx: &mut TestAppContext) {
        let (arena, left, right, owner, cx) = arena_with_owner(cx).await;
        let arenas = vec![arena];
        let owner_id = owner.entity_id();
        let found = cx.update(|_, cx| find_item_location(&arenas, owner_id, cx).map(|(_, pane, _)| pane));
        assert_eq!(found.as_ref(), Some(&left));

        // Moved: both panes stay open, the stored pane is now wrong.
        left.update_in(cx, |pane, window, cx| pane.remove_item(owner_id, false, false, window, cx));
        right.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(owner.clone()), true, true, None, window, cx);
        });
        let found = cx.update(|_, cx| find_item_location(&arenas, owner_id, cx).map(|(_, pane, _)| pane));
        assert_eq!(found.as_ref(), Some(&right));

        // Closed while this test still holds the entity: not open any more.
        right.update_in(cx, |pane, window, cx| pane.remove_item(owner_id, false, false, window, cx));
        assert!(cx.update(|_, cx| find_item_location(&arenas, owner_id, cx)).is_none());
    }

    fn scoped_view(cx: &mut TestAppContext) -> (Entity<InboxTodos>, u64, u64, Entity<InboxTodoView>, &mut VisualTestContext) {
        init(cx);
        let task = Uuid::new_v4();
        let todos = cx.new(|_| InboxTodos::new(None));
        let mut next_id = 1;
        let (go_id, done_id) = todos.update(cx, |todos, cx| {
            let go_id = todos.add(draft(InboxTodoKind::Proposal, "Add tests", Some(task)), &mut next_id, cx).unwrap();
            let done_id = todos.add(draft(InboxTodoKind::UserTask, "Check the lane", Some(task)), &mut next_id, cx).unwrap();
            // Another task's item never shows here.
            todos.add(draft(InboxTodoKind::Proposal, "Elsewhere", Some(Uuid::new_v4())), &mut next_id, cx).unwrap();
            todos.assign(go_id, assignee("s1", None), cx).unwrap();
            (go_id, done_id)
        });
        let scope = TodoScope {
            task_id: Some(task),
            arena_paths: vec![],
            arena_names: vec![],
        };
        let (view, cx) = cx.add_window_view(|window, cx| InboxTodoView::new(todos.clone(), scope, window, cx));
        (todos, go_id, done_id, view, cx)
    }

    #[gpui::test]
    async fn the_view_lists_pick_before_you_do_and_keeps_notes_per_item(cx: &mut TestAppContext) {
        let (todos, go_id, done_id, view, cx) = scoped_view(cx);
        let titles: Vec<String> = view.read_with(cx, |view, cx| view.open_items(cx).into_iter().map(|item| item.title).collect());
        assert_eq!(titles, vec!["Add tests", "Check the lane"]);

        todos.update(cx, |todos, cx| {
            todos.set_choice(go_id, Some(TodoChoice::Go), cx);
            todos.set_choice(done_id, Some(TodoChoice::Done), cx);
        });
        cx.run_until_parked();
        let editor = view.read_with(cx, |view, _| view.note_editors.get(&go_id).cloned()).expect("note editor");
        assert!(view.read_with(cx, |view, _| view.note_editors.contains_key(&done_id)));
        editor.update_in(cx, |editor, window, cx| editor.set_text("with the Failed lane", window, cx));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.notes(cx)).get(&go_id).map(String::as_str),
            Some("with the Failed lane")
        );

        // Clearing one choice drops its editor and keeps the other.
        todos.update(cx, |todos, cx| todos.clear_choices(&[done_id], cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.note_editors.get(&go_id), Some(&editor));
            assert!(!view.note_editors.contains_key(&done_id));
        });
    }

    #[gpui::test]
    async fn choices_can_be_sent_from_the_keyboard(cx: &mut TestAppContext) {
        let (todos, go_id, _done_id, view, cx) = scoped_view(cx);
        cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-enter", SendAnswers, Some(super::KEY_CONTEXT)),
                gpui::KeyBinding::new(
                    "cmd-enter",
                    SendAnswers,
                    Some(&format!("{} > Editor", super::KEY_CONTEXT)),
                ),
            ])
        });
        let sent = Rc::new(Cell::new(0));
        let _subscription = cx.update({
            let sent = sent.clone();
            |_, cx| {
                cx.subscribe(&view, move |_, event: &InboxTodoViewEvent, _| {
                    if let InboxTodoViewEvent::Send { .. } = event {
                        sent.set(sent.get() + 1);
                    }
                })
            }
        });

        // Nothing chosen: the shortcut explains instead of sending.
        cx.update(|window, cx| {
            let handle = view.read(cx).focus_handle.clone();
            handle.focus(window, cx);
        });
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(sent.get(), 0);
        assert!(view.read_with(cx, |view, _| view.error.is_some()));

        todos.update(cx, |todos, cx| todos.set_choice(go_id, Some(TodoChoice::Go), cx));
        cx.run_until_parked();
        // From inside a note editor too, which binds cmd-enter itself.
        let editor = view.read_with(cx, |view, _| view.note_editors.get(&go_id).cloned()).expect("note editor");
        cx.update(|window, cx| {
            let handle = editor.read(cx).focus_handle(cx);
            handle.focus(window, cx);
        });
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(sent.get(), 1);
    }

    #[gpui::test]
    async fn titles_and_details_can_be_selected_and_copied(cx: &mut TestAppContext) {
        init(cx);
        let task = Uuid::new_v4();
        let todos = cx.new(|_| InboxTodos::new(None));
        let mut next_id = 1;
        let todo_id = todos.update(cx, |todos, cx| {
            let draft = TodoDraft {
                detail: Some("Cover the Failed lane".to_string()),
                ..draft(InboxTodoKind::Proposal, "Add tests", Some(task))
            };
            todos.add(draft, &mut next_id, cx).unwrap()
        });
        assert_eq!(todo_id, 1, "the selectors below name item 1");
        let scope = TodoScope {
            task_id: Some(task),
            arena_paths: vec![],
            arena_names: vec![],
        };
        let (_view, cx) = cx.add_window_view(|window, cx| InboxTodoView::new(todos.clone(), scope, window, cx));
        cx.update(|_, cx| cx.bind_keys([gpui::KeyBinding::new("cmd-c", markdown::Copy, Some("Markdown"))]));
        cx.run_until_parked();

        let title = cx.debug_bounds("todo-text-1-Title").expect("title");
        let detail = cx.debug_bounds("todo-text-1-Detail").expect("detail");
        assert!(
            detail.size.height < title.size.height,
            "the detail keeps its smaller text: {detail:?} vs {title:?}"
        );

        for (selector, expected) in [
            ("todo-text-1-Title", "Add tests"),
            ("todo-text-1-Detail", "Cover the Failed lane"),
        ] {
            let bounds = cx.debug_bounds(selector).expect(selector);
            let start = gpui::point(bounds.left() + gpui::px(1.), bounds.center().y);
            let end = gpui::point(bounds.right() - gpui::px(1.), bounds.center().y);
            cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(end, gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_keystrokes("cmd-c");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
                Some(expected),
            );
        }
    }

    #[test]
    fn a_closed_assignee_is_restored_only_where_it_unambiguously_lived() {
        let open = |names: &[(&str, &str)]| {
            names
                .iter()
                .map(|(name, path)| (name.to_string(), PathBuf::from(path)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            todo_restore_plan(&assignee("c8832ec2", Some("/src/zed")), &[]),
            RestorePlan::ResumeClaude {
                arena_path: PathBuf::from("/src/zed"),
                session_id: "c8832ec2".to_string()
            }
        );
        assert_eq!(
            todo_restore_plan(
                &Assignee {
                    asker: InboxAsker::CodexAcp { saved_id: Uuid::nil() },
                    arena_path: Some(PathBuf::from("/src/zed")),
                    arena_name: "zed".to_string(),
                },
                &[]
            ),
            RestorePlan::OpenArena {
                arena_path: PathBuf::from("/src/zed")
            }
        );

        // Recorded before arena_path: the name must be unique.
        let legacy = assignee("c8832ec2", None);
        assert_eq!(
            todo_restore_plan(&legacy, &open(&[("zed", "/src/zed"), ("scala", "/src/scala")])),
            RestorePlan::ResumeClaude {
                arena_path: PathBuf::from("/src/zed"),
                session_id: "c8832ec2".to_string()
            }
        );
        assert!(matches!(
            todo_restore_plan(&legacy, &open(&[("zed", "/src/zed"), ("zed", "/work/zed")])),
            RestorePlan::Fallback(_)
        ));
        assert!(matches!(
            todo_restore_plan(&legacy, &open(&[("scala", "/src/scala")])),
            RestorePlan::Fallback(_)
        ));
    }

    #[test]
    fn restorations_settle_on_bind_failure_or_closure() {
        let tab = gpui::EntityId::from(9u64);
        let asker = claude("s1");
        let mut pending = HashMap::from([(1, PendingRestore::Launching { asker: asker.clone() })]);

        // The launch fails: forgotten, with a reason for the row.
        let error = record_restore_launch(&mut pending, 1, Err(anyhow::anyhow!("no claude")));
        assert!(error.is_some_and(|error| error.contains("no claude")));
        assert!(pending.is_empty());

        // Retry: nothing stale blocks a new attempt.
        pending.insert(1, PendingRestore::Launching { asker: asker.clone() });
        assert_eq!(record_restore_launch(&mut pending, 1, Ok(tab)), None);
        assert_eq!(
            pending.get(&1),
            Some(&PendingRestore::Launched { asker: asker.clone(), item_id: tab })
        );

        let observe = |exists: bool, bound: bool, tab_open: bool| {
            move |_: u64, _: &PendingRestore| RestoreObservation {
                item_exists: exists,
                assignee_open: bound,
                launched_tab_open: tab_open,
            }
        };
        // Still reconnecting: kept.
        assert!(advance_pending_restores(&mut pending, observe(true, false, true)).is_empty());
        assert_eq!(pending.len(), 1);
        // The tab was closed (or claude exited) before the session reconnected.
        let outcomes = advance_pending_restores(&mut pending, observe(true, false, false));
        assert!(matches!(outcomes.as_slice(), [(1, RestoreOutcome::Failed(_))]));
        assert!(pending.is_empty());

        pending.insert(1, PendingRestore::Launched { asker: asker.clone(), item_id: tab });
        pending.insert(2, PendingRestore::Restored { asker: asker.clone() });
        pending.insert(3, PendingRestore::Launching { asker: asker.clone() });
        let outcomes = advance_pending_restores(&mut pending, |todo_id, _| RestoreObservation {
            item_exists: todo_id != 3,
            assignee_open: todo_id == 1,
            launched_tab_open: true,
        });
        assert_eq!(outcomes, vec![(1, RestoreOutcome::Open)]);
        assert_eq!(pending, HashMap::from([(2, PendingRestore::Restored { asker })]));
    }

    #[test]
    fn an_acp_owner_comes_back_only_if_its_arena_still_saves_it() {
        let asker = InboxAsker::CodexAcp { saved_id: Uuid::nil() };
        assert_eq!(
            acp_restore_outcome(true, true, asker.clone()),
            Ok(PendingRestore::Restored { asker: asker.clone() })
        );
        assert!(acp_restore_outcome(true, false, asker.clone()).is_err());
        assert!(acp_restore_outcome(false, false, asker).is_err());
    }
}
