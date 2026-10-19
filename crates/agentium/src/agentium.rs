mod arena;
mod codex_acp_persistence;
mod codex_acp_resume;
pub mod codex_acp_view;
pub mod board;
pub mod file_browser_view;
mod git_status_view;
mod inbox_todo_view;
pub mod questionnaire_view;
mod review_view;
mod svg_view;

pub use questionnaire_view::QuestionnaireView;
pub use inbox_todo_view::SendAnswers as SendInboxTodoAnswers;
pub use review_view::ReviewView;
pub use svg_view::SvgView;

#[derive(Clone)]
pub struct AgentiumWorkspaceHandle(pub Entity<Workspace>);
impl gpui::Global for AgentiumWorkspaceHandle {}

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sysinfo::{ProcessRefreshKind, System, UpdateKind};
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
use uuid::Uuid;
use workspace::{
    AppState, Pane, PathList, SerializedWorkspaceLocation, SplitDirection, Workspace,
    WorkspaceDb, WorkspaceId, ZoomOut,
};

use arena::{Arena, ArenaEvent, TitleState};

/// How a CLI command names the arena it targets.
#[derive(Debug)]
pub enum ArenaSelector {
    /// Explicit `--arena <path>`: the arena whose worktree contains this path.
    Path(PathBuf),
    /// The arena the command was run from: a terminal whose shell is among the
    /// caller's ancestor processes, else the arena whose worktree contains the cwd.
    Caller {
        ancestor_pids: Vec<u32>,
        cwd: Option<PathBuf>,
    },
}

#[derive(Debug, serde::Serialize)]
pub struct TabInfo {
    pub title: String,
    pub kind: &'static str,
    /// Claude hook state of a terminal tab: permission, running, ready, or idle.
    pub state: Option<&'static str>,
    pub arena: Option<PathBuf>,
}

/// Reply payload for the `task_info` CLI request.
#[derive(serde::Serialize)]
pub struct TaskInfoReply {
    /// 1-based position in `task list` output; `None` for an archived task.
    pub index: Option<usize>,
    pub task: board::BoardTask,
    /// Keyed by the same canonical path strings as `pr_cache`, one entry per
    /// worktree of `task`.
    pub prs: HashMap<String, Vec<PrInfo>>,
    /// Non-fatal issues from an `--update` fetch (unavailable providers,
    /// per-worktree or per-issue fetch failures). Empty otherwise.
    pub warnings: Vec<String>,
}

pub enum PaneContentType {
    CodexAcp,
    Terminal,
    Diff,
    BranchDiff,
    GitStatus,
    ProjectSearch,
    FileBrowser,
    GitGraph,
}

actions!(agentium, [NewClaudeCode, NewCodexAcp, ResumeCodexConversation, NewDiffView, NewBranchDiff, NewProjectSearch, NewGitStatus, NewFileBrowser, NewGitGraph, OpenNewItemMenu, OpenSplitMenu]);

pub fn init_acp_host(cx: &mut App) {
    cx.set_global(agent_ui::AgentUiHost {
        is_conversation_visible: |conversation_id, window, cx| {
            let Some(application) = window.root::<AgentiumApp>().flatten() else {
                return false;
            };
            let application = application.read(cx);
            application.active_arena().is_some_and(|arena| {
                arena.read(cx).center.panes().iter().any(|pane| {
                    pane.read(cx)
                        .active_item()
                        .and_then(|item| item.downcast::<codex_acp_view::CodexAcpView>())
                        .is_some_and(|view| {
                            view.read(cx).conversation_id() == conversation_id
                                && view.focus_handle(cx).contains_focused(window, cx)
                        })
                })
            })
        },
        activate_conversation: |conversation_id, window, cx| {
            let Some(application) = window.root::<AgentiumApp>().flatten() else {
                return;
            };
            application.update(cx, |application, cx| {
                let target =
                    application
                        .arenas
                        .iter()
                        .enumerate()
                        .find_map(|(arena_index, arena)| {
                            arena.read(cx).center.panes().iter().find_map(|pane| {
                                pane.read(cx)
                                    .items_of_type::<codex_acp_view::CodexAcpView>()
                                    .find(|view| view.read(cx).conversation_id() == conversation_id)
                                    .map(|view| (arena_index, (*pane).clone(), view))
                            })
                        });
                if let Some((arena_index, pane, view)) = target {
                    application.switch_arena(arena_index, window, cx);
                    let index = pane.read(cx).index_for_item(&view);
                    pane.update(cx, |pane, cx| {
                        if let Some(index) = index {
                            pane.activate_item(index, true, true, window, cx);
                        }
                        pane.zoom_out(&ZoomOut, window, cx);
                    });
                    window.focus(&view.focus_handle(cx), cx);
                }
            });
        },
        spawn_terminal_login: |conversation_id, mut login, project, window, cx| {
            let target = window
                .root::<AgentiumApp>()
                .flatten()
                .and_then(|application| {
                    application.read(cx).arenas.iter().find_map(|arena| {
                        arena
                            .read(cx)
                            .center
                            .panes()
                            .iter()
                            .any(|pane| {
                                pane.read(cx)
                                    .items_of_type::<codex_acp_view::CodexAcpView>()
                                    .any(|view| view.read(cx).conversation_id() == conversation_id)
                            })
                            .then(|| arena.clone())
                    })
                });
            let Some(arena) = target else {
                return Task::ready(Err(anyhow::anyhow!(
                    "The Codex (ACP) tab is no longer open"
                )));
            };
            login.cwd = arena.read(cx).working_directory.clone();
            let terminal =
                project.update(cx, |project, cx| project.create_terminal_task(login, cx));
            let workspace = arena.read(cx).workspace.clone();
            let arena = arena.downgrade();
            window.spawn(cx, async move |cx| {
                let terminal = terminal.await?;
                arena.update_in(cx, |arena, window, cx| {
                    let focus_login =
                        window
                            .root::<AgentiumApp>()
                            .flatten()
                            .is_some_and(|application| {
                                application
                                    .read(cx)
                                    .active_arena()
                                    .is_some_and(|active| active.entity_id() == cx.entity_id())
                            });
                    let view = cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            workspace,
                            None,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    });
                    arena.active_pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(view), true, focus_login, None, window, cx);
                    });
                })?;
                Ok(terminal.downgrade())
            })
        },
    });
}

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

#[derive(Clone, Copy, PartialEq, Debug)]
enum ClaudeSessionState {
    Idle,
    Running,
    WaitingPermission,
    Completed,
}

struct ClaudeSession {
    ancestor_pids: Vec<u32>,
    state: ClaudeSessionState,
    // Terminal title spinner (◐/◑) overrides `state` to Running.
    title_busy: bool,
    user_prompt: String,
    status_message: String,
}

impl ClaudeSession {
    fn effective_state(&self) -> ClaudeSessionState {
        if self.title_busy {
            ClaudeSessionState::Running
        } else {
            self.state
        }
    }
}

/// Prepends the `[from: <tab title>]` line agents use to tell messages apart.
/// An empty message (Enter-only send) stays empty, and a message that already
/// carries the line keeps the one its author wrote.
/// Always prepended, even when `text` already starts with `[from:`, so the
/// first line is the only one a receiver can trust.
fn with_from_line(from: &str, text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!("[from: {from}]\n{text}")
    }
}

/// The sender shown in the `[from: ...]` line: a tab verified from the
/// kernel-reported peer ancestry bare, otherwise the claimed `--from` (or
/// `unknown`) marked `(unverified)`. Names lose `]` and control characters so
/// that neither a claimed name nor a self-set tab title can close the bracket
/// early or start a forged line.
pub fn sender_label(verified: Option<&str>, claimed: Option<&str>) -> String {
    let sanitize = |name: &str| -> String {
        name.chars()
            .filter(|character| *character != ']' && !character.is_control())
            .collect()
    };
    match verified {
        Some(verified) => sanitize(verified),
        None => format!("{} (unverified)", sanitize(claimed.unwrap_or("unknown"))),
    }
}

/// Unlike terminal's `last_n_non_empty_lines`, keeps blank lines inside the
/// output and only drops the empty screen rows below the cursor.
fn tail_lines(content: &str, count: usize) -> String {
    let mut lines: Vec<&str> = content.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let start = if count == 0 {
        0
    } else {
        lines.len().saturating_sub(count)
    };
    let mut text = lines[start..].join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Next state of a hook-less agent (Codex) tracked from its terminal title.
/// `None` means "not tracked": a plain title never starts tracking, so a shell
/// that sets its own title after the agent exited is left alone.
fn next_title_driven_state(
    previous: Option<ClaudeSessionState>,
    title: TitleState,
) -> Option<ClaudeSessionState> {
    match (previous, title) {
        (_, TitleState::Busy) => Some(ClaudeSessionState::Running),
        (_, TitleState::ActionRequired) => Some(ClaudeSessionState::WaitingPermission),
        (
            Some(ClaudeSessionState::Running | ClaudeSessionState::WaitingPermission),
            TitleState::Idle,
        ) => Some(ClaudeSessionState::Completed),
        (previous, TitleState::Idle) => previous,
        (_, TitleState::Empty) => None,
    }
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

#[derive(Clone)]
struct BadgeEntryInfo {
    pane: Entity<Pane>,
    item: Box<dyn workspace::ItemHandle>,
    primary: String,
    secondary: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct InboxOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxTodoKind {
    /// The user does something, then answers "Done".
    UserTask,
    /// The user decides whether the agent does it ("Go" / "Skip").
    Proposal,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InboxTodoItem {
    pub kind: InboxTodoKind,
    pub title: String,
    pub detail: Option<String>,
}

pub const MAX_TODO_ITEMS: usize = 8;

/// Shared by the CLI and the app. Titles must be unique because answers
/// refer to items by title once the agent has replaced its list.
pub fn validate_todo_items(items: &[InboxTodoItem]) -> anyhow::Result<()> {
    if items.is_empty() {
        anyhow::bail!("give at least one --user-task or --proposal; use `todo clear` to empty the list");
    }
    if items.len() > MAX_TODO_ITEMS {
        anyhow::bail!(
            "{} items is more than {MAX_TODO_ITEMS}; drop the least important ones",
            items.len()
        );
    }
    let mut titles = HashSet::new();
    for item in items {
        if item.title.trim().is_empty() {
            anyhow::bail!("an item has an empty title");
        }
        if !titles.insert(item.title.as_str()) {
            anyhow::bail!("the title {:?} is used twice", item.title);
        }
    }
    Ok(())
}

/// Decodes a `todo_set` request strictly: one malformed entry fails the whole
/// request, because the list is replaced wholesale and a silently skipped
/// item would be lost.
pub fn parse_todo_set_request(request: &serde_json::Value) -> anyhow::Result<Vec<InboxTodoItem>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        title: String,
        detail: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Request {
        user_tasks: Vec<Entry>,
        proposals: Vec<Entry>,
    }
    let request: Request = serde_json::from_value(request.clone())
        .map_err(|error| anyhow::anyhow!("malformed todo_set request: {error}"))?;
    let entries = |entries: Vec<Entry>, kind| {
        entries.into_iter().map(move |entry| InboxTodoItem {
            kind,
            title: entry.title.trim().to_string(),
            detail: entry
                .detail
                .map(|detail| detail.trim().to_string())
                .filter(|detail| !detail.is_empty()),
        })
    };
    let items: Vec<InboxTodoItem> = entries(request.user_tasks, InboxTodoKind::UserTask)
        .chain(entries(request.proposals, InboxTodoKind::Proposal))
        .collect();
    validate_todo_items(&items)?;
    Ok(items)
}

/// `None` when the prompt is a message another tab or the Inbox pasted in
/// (`[from: ...]`) and there is no earlier title to keep.
fn headline_from_prompt(prompt: &str, previous: Option<String>) -> Option<String> {
    if prompt.trim_start().starts_with("[from: ") {
        previous
    } else {
        Some(prompt.to_string())
    }
}

/// Titles of `old` items that `new` no longer has, in their old order.
fn dropped_todo_titles(old: &[InboxTodoItem], new: &[InboxTodoItem]) -> Vec<String> {
    old.iter()
        .filter(|old| !new.iter().any(|new| new.title == old.title))
        .map(|old| old.title.clone())
        .collect()
}

#[derive(serde::Serialize)]
pub struct TodoSetReply {
    pub count: usize,
    pub dropped: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct TodoListReply {
    pub own: Vec<InboxTodoItem>,
    /// Other tabs of the caller's arena, by tab title.
    pub others: Vec<(String, Vec<InboxTodoItem>)>,
}

/// Requests and proposals an agent handed to the user at the end of a turn.
/// One list per tab; the agent replaces it wholesale with `todo set`.
#[derive(serde::Serialize, serde::Deserialize)]
struct InboxTodoList {
    id: u64,
    /// The tab title at the last `todo set`.
    title: String,
    arena_name: String,
    updated_at: DateTime<Utc>,
    asker: Option<InboxAsker>,
    items: Vec<InboxTodoItem>,
    /// The owner arena's branch at the last `todo set`.
    #[serde(default)]
    branch: Option<String>,
    /// The owner arena's directory, to restore the tab after a restart.
    #[serde(default)]
    arena_path: Option<PathBuf>,
    #[serde(skip)]
    tab: Option<InboxTab>,
    /// The user's draft answers, by item title. Not persisted: cheap to redo.
    #[serde(skip)]
    choices: HashMap<String, inbox_todo_view::TodoChoice>,
}

/// The list owned by the tab `item_id`, or by the same asker in a tab
/// reopened after a restart.
fn todo_list_index(
    lists: &[InboxTodoList],
    item_id: EntityId,
    asker: Option<&InboxAsker>,
) -> Option<usize> {
    lists
        .iter()
        .position(|list| list.tab.as_ref().is_some_and(|tab| tab.item.id() == item_id))
        .or_else(|| {
            let asker = asker?;
            lists.iter().position(|list| list.asker.as_ref() == Some(asker))
        })
}

/// The id to hand out next: every persisted kind of Inbox record shares one
/// counter, so ids stay unique across a restart.
fn next_inbox_id(
    questions: &[InboxQuestion],
    completions: &[InboxCompletion],
    todos: &[InboxTodoList],
) -> u64 {
    questions
        .iter()
        .map(|question| question.id)
        .chain(completions.iter().map(|completion| completion.id))
        .chain(todos.iter().map(|todo| todo.id))
        .max()
        .map_or(1, |id| id + 1)
}

/// What survives an Agentium restart to find the asking tab again: tab
/// entities and shell PIDs do not, but `claude --resume` keeps the session id
/// and restored ACP tabs keep their saved id.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InboxAsker {
    ClaudeSession { session_id: String },
    CodexAcp { saved_id: Uuid },
}

struct InboxTab {
    arena: WeakEntity<Arena>,
    pane: WeakEntity<Pane>,
    item: Box<dyn workspace::WeakItemHandle>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InboxQuestion {
    id: u64,
    question: String,
    context: Option<String>,
    options: Vec<InboxOption>,
    recommended: Option<usize>,
    /// The agent kept working on the recommended option instead of ending
    /// its turn (`inbox ask --assume`).
    #[serde(default)]
    assumed: bool,
    arena_name: String,
    asker: Option<InboxAsker>,
    #[serde(skip)]
    tab: Option<InboxTab>,
    #[serde(skip)]
    error: Option<String>,
}

impl InboxTab {
    fn live(&self) -> Option<(Entity<Arena>, Entity<Pane>, Box<dyn workspace::ItemHandle>)> {
        Some((self.arena.upgrade()?, self.pane.upgrade()?, self.item.upgrade()?))
    }
}

impl InboxQuestion {
    fn live_tab(&self) -> Option<(Entity<Arena>, Entity<Pane>, Box<dyn workspace::ItemHandle>)> {
        self.tab.as_ref()?.live()
    }
}

/// A turn that finished, kept until the user dismisses it: the live Completed
/// state it is recorded from clears as soon as the tab is touched.
#[derive(serde::Serialize, serde::Deserialize)]
struct InboxCompletion {
    id: u64,
    title: String,
    detail: String,
    arena_name: String,
    completed_at: DateTime<Utc>,
    /// Set when the user first looks at the tab. A confirmed record no longer
    /// counts toward the Inbox badge and expires after
    /// `INBOX_CONFIRMED_COMPLETION_TTL` or on restart.
    #[serde(default)]
    confirmed_at: Option<DateTime<Utc>>,
    asker: Option<InboxAsker>,
    #[serde(skip)]
    tab: Option<InboxTab>,
    /// Set for a Failed-lane record: why it failed.
    #[serde(default)]
    failure: Option<String>,
    /// Set for a PR CI failure, which has no tab.
    #[serde(default)]
    pull_request: Option<InboxPullRequest>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct InboxPullRequest {
    arena_path: PathBuf,
    number: u32,
}

/// Identifies one finished turn of a tab. ACP tabs count their turns because
/// an observer may only see the last state of a batch of notifications;
/// terminal tabs report each state change synchronously and use turn 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FinishKey {
    failed: bool,
    turn: u64,
}

/// A tab that is Completed or Failed at one Inbox sync.
struct FinishedTab {
    item_id: EntityId,
    key: FinishKey,
    tab: Option<InboxTab>,
    asker: Option<InboxAsker>,
    arena_name: String,
    title: String,
    detail: String,
    failure: Option<String>,
    notify: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedTabState {
    Gone,
    Busy,
    Viewed,
    Idle,
}

/// Records each tab whose finish key changed since the previous sync, replacing
/// that tab's existing record (same item, or same asker). Tabs in
/// `asking_items` are not recorded as completed: the asking agent ends its turn
/// right after `inbox ask`, and that is not a finished task. They are still
/// remembered as seen, so answering the question does not record the stale
/// completion. Returns each new record id with whether to pop up a notification.
fn record_finished_tabs(
    records: &mut Vec<InboxCompletion>,
    seen: &mut HashMap<EntityId, FinishKey>,
    finished: Vec<FinishedTab>,
    asking_items: &HashSet<EntityId>,
    next_id: &mut u64,
    now: DateTime<Utc>,
) -> Vec<(u64, bool)> {
    let previous = std::mem::take(seen);
    let mut recorded = Vec::new();
    for finished in finished {
        seen.insert(finished.item_id, finished.key);
        if previous.get(&finished.item_id) == Some(&finished.key)
            || (!finished.key.failed && asking_items.contains(&finished.item_id))
        {
            continue;
        }
        records.retain(|record| {
            let same_tab = record
                .tab
                .as_ref()
                .is_some_and(|tab| tab.item.id() == finished.item_id);
            let same_asker = finished.asker.is_some() && record.asker == finished.asker;
            !same_tab && !same_asker
        });
        let id = *next_id;
        *next_id += 1;
        records.insert(
            0,
            InboxCompletion {
                id,
                title: finished.title,
                detail: finished.detail,
                arena_name: finished.arena_name,
                completed_at: now,
                confirmed_at: None,
                asker: finished.asker,
                tab: finished.tab,
                failure: finished.failure,
                pull_request: None,
            },
        );
        recorded.push((id, finished.notify));
    }
    recorded
}

/// Drops records whose tab is working again or that stayed confirmed past
/// `INBOX_CONFIRMED_COMPLETION_TTL`, and confirms the ones the user is looking
/// at. Returns whether anything changed.
fn age_inbox_records(
    records: &mut Vec<InboxCompletion>,
    now: DateTime<Utc>,
    state_of: impl Fn(&InboxCompletion) -> RecordedTabState,
) -> bool {
    let before = records.len();
    records.retain(|record| {
        let expired = record
            .confirmed_at
            .is_some_and(|confirmed_at| now - confirmed_at > INBOX_CONFIRMED_COMPLETION_TTL);
        !expired && state_of(record) != RecordedTabState::Busy
    });
    let mut changed = records.len() != before;
    for record in records.iter_mut() {
        if record.confirmed_at.is_none() && state_of(record) == RecordedTabState::Viewed {
            record.confirmed_at = Some(now);
            changed = true;
        }
    }
    changed
}

fn ci_is_failing(status: &CiStatus) -> bool {
    matches!(status, CiStatus::Failed | CiStatus::PendingWithFailure)
}

/// Updates the PR's Failed-lane record for a new CI result. A clean result
/// removes it whatever came before, so a failure persisted across a restart
/// clears too. A failure is recorded only on a change from a known
/// non-failing status: the first status seen (e.g. right after startup) is
/// not news, and a failure the user dismissed stays dismissed until CI
/// recovers. Returns whether records changed and the new record id.
fn apply_ci_status(
    records: &mut Vec<InboxCompletion>,
    pull_request: &InboxPullRequest,
    previous: Option<&CiStatus>,
    next: Option<&CiInfo>,
    title: String,
    arena_name: String,
    next_id: &mut u64,
    now: DateTime<Utc>,
) -> (bool, Option<u64>) {
    let Some(next) = next else {
        return (false, None);
    };
    let before = records.len();
    if !ci_is_failing(&next.status) {
        records.retain(|record| record.pull_request.as_ref() != Some(pull_request));
        return (records.len() != before, None);
    }
    if !previous.is_some_and(|previous| !ci_is_failing(previous)) {
        return (false, None);
    }
    records.retain(|record| record.pull_request.as_ref() != Some(pull_request));
    let failing_checks = next
        .checks
        .iter()
        .filter(|check| check.bucket == "fail")
        .map(|check| check.name.to_string())
        .collect::<Vec<_>>();
    let failure = if failing_checks.is_empty() {
        "CI failed".to_string()
    } else {
        format!("Failing: {}", failing_checks.join(", "))
    };
    let id = *next_id;
    *next_id += 1;
    records.insert(
        0,
        InboxCompletion {
            id,
            title,
            detail: format!("PR #{}", pull_request.number),
            arena_name,
            completed_at: now,
            confirmed_at: None,
            asker: None,
            tab: None,
            failure: Some(failure),
            pull_request: Some(pull_request.clone()),
        },
    );
    (true, Some(id))
}

/// Tags each CI request so a response is applied only if no newer request for
/// the same PR started since, and not at all after the arena closed or switched
/// branches. Tokens come from one counter that never resets, so a request
/// started after `forget_arena` cannot reuse an older request's token.
#[derive(Default)]
struct CiFetchGenerations {
    next_token: u64,
    latest: HashMap<(EntityId, u32), u64>,
}

impl CiFetchGenerations {
    fn start(&mut self, key: (EntityId, u32)) -> u64 {
        self.next_token += 1;
        self.latest.insert(key, self.next_token);
        self.next_token
    }

    fn is_current(&self, key: (EntityId, u32), token: u64) -> bool {
        self.latest.get(&key) == Some(&token)
    }

    fn forget_arena(&mut self, entity_id: EntityId) {
        self.latest.retain(|(id, _), _| *id != entity_id);
    }

    /// Accepts a response if its request is still the latest for the PR, and
    /// only then counts the PR as checked: a dropped response must not delay
    /// the fetch that replaces it.
    fn accept(
        &self,
        key: (EntityId, u32),
        token: u64,
        last_checked: &mut HashMap<(EntityId, u32), Instant>,
        now: Instant,
    ) -> bool {
        let current = self.is_current(key, token);
        if current {
            last_checked.insert(key, now);
        }
        current
    }
}

impl InboxCompletion {
    fn live_tab(&self) -> Option<(Entity<Arena>, Entity<Pane>, Box<dyn workspace::ItemHandle>)> {
        self.tab.as_ref()?.live()
    }
}

struct InboxEntry {
    arena_index: usize,
    arena_name: String,
    info: BadgeEntryInfo,
    is_active: bool,
    permission: Option<InboxPermissionRow>,
}

/// A Claude PermissionRequest hook held open until the user answers here.
struct InboxPermission {
    id: u64,
    session_id: String,
    tool_name: String,
    summary: String,
    suggestions: serde_json::Value,
    reply: futures::channel::oneshot::Sender<serde_json::Value>,
}

struct InboxPermissionRow {
    id: u64,
    tool_name: String,
    summary: String,
    can_always_allow: bool,
}

#[derive(Clone, Copy)]
enum InboxPermissionAnswer {
    Allow,
    AlwaysAllow,
    Deny,
}

struct InboxCompletionRow {
    id: u64,
    is_active: bool,
    title: String,
    detail: String,
    arena_name: String,
    completed_at: DateTime<Utc>,
    note: Option<&'static str>,
    failure: Option<String>,
    confirmed: bool,
    /// Items in the same tab's to-do list.
    todo_count: usize,
}

struct InboxTodoRow {
    id: u64,
    is_active: bool,
    title: String,
    arena_name: String,
    item_count: usize,
    updated_at: DateTime<Utc>,
    note: Option<String>,
}

struct InboxQuestionRow {
    id: u64,
    is_active: bool,
    arena_name: String,
    resume_hint: Option<String>,
    question: String,
    context: Option<String>,
    options: Vec<(String, Option<String>)>,
    recommended: Option<usize>,
    assumed: bool,
    error: Option<String>,
}

#[derive(Default)]
struct InboxEntries {
    questions: Vec<InboxQuestionRow>,
    permission: Vec<InboxEntry>,
    failed: Vec<InboxCompletionRow>,
    /// Not counted in the badge: the agent is not waiting on them.
    todos: Vec<InboxTodoRow>,
    completed: Vec<InboxCompletionRow>,
    /// Shown for orientation only: running tabs need nothing from the user,
    /// so they stay out of the badge.
    in_progress: Vec<InboxEntry>,
}

#[derive(Clone, Copy)]
pub(crate) struct TerminalPidInfo {
    pub pid: u32,
    pub is_task_terminal: bool,
}

#[derive(Clone)]
pub(crate) struct SharedSessionState {
    pub ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub running_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub permission_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub acknowledged_task_pids: Rc<RefCell<HashSet<u32>>>,
    pub pid_to_session_id: Rc<RefCell<HashMap<u32, String>>>,
    pub terminal_pid_cache: Rc<RefCell<HashMap<EntityId, TerminalPidInfo>>>,
}

impl SharedSessionState {
    fn new() -> Self {
        Self {
            ready_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            running_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            permission_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            acknowledged_task_pids: Rc::new(RefCell::new(HashSet::new())),
            pid_to_session_id: Rc::new(RefCell::new(HashMap::new())),
            terminal_pid_cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct PrSessionData {
    pub pr: BTreeSet<u32>,
}

pub use board::{PrProvider, PrStatus};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReviewEntry {
    pub user: SharedString,
    pub state: ReviewState,
    pub commit_oid: SharedString,
    pub avatar_url: SharedString,
    pub submitted_at: SharedString,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrInfo {
    pub number: u32,
    pub title: SharedString,
    pub status: PrStatus,
    pub html_url: SharedString,
    pub review_decision: Option<ReviewDecision>,
    pub review_count: usize,
    pub head_sha: SharedString,
    pub reviews: Vec<ReviewEntry>,
    pub base_ref: SharedString,
    pub provider: PrProvider,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CiStatus {
    AllPassed,
    Failed,
    PendingWithFailure,
    PendingClean,
}

#[derive(Clone, Debug)]
pub struct CiCheckEntry {
    pub name: SharedString,
    pub bucket: SharedString,
}

#[derive(Clone, Debug)]
pub struct CiInfo {
    pub status: CiStatus,
    pub checks: Vec<CiCheckEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Repositories,
    Tasks,
}

#[derive(Clone)]
enum WorktreeRow {
    Live { arena_index: usize },
    Closed { path: PathBuf },
    Missing { path: PathBuf },
}

/// Derived board state that requires filesystem calls (canonicalize/exists),
/// recomputed on board or arena changes — never during render, which runs
/// every 2 seconds via `_busy_badge_refresh_task`.
#[derive(Default)]
struct BoardCache {
    task_worktrees: HashMap<Uuid, Vec<WorktreeRow>>,
    unassigned_arenas: Vec<usize>,
}

pub struct AgentiumApp {
    codex_tabs: Entity<codex_acp_persistence::CodexAcpPersistence>,
    _codex_tabs_subscription: Subscription,
    _codex_quit_subscription: Subscription,
    arena_error: Option<String>,
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
    // Hook-less agents (Codex) tracked from their terminal title, keyed by the
    // terminal's wrapper PID. Kept apart from `claude_sessions` so Fork Session,
    // pr.json and the caffeinate monitor never see them.
    title_driven_states: HashMap<u32, ClaudeSessionState>,
    session_state: SharedSessionState,
    should_move_window: bool,
    rate_limits: Option<RateLimits>,
    _rate_limits_refresh_task: Option<Task<()>>,
    _busy_badge_refresh_task: Task<()>,
    // Reused across `collect_busy_terminal_infos` calls to avoid reconstructing
    // the System on every sidebar render. `RefCell` is safe only so long as the
    // `borrow_mut()` inside that method is taken after the arena walk completes
    // — do not hold the borrow across the arena loop.
    sysinfo_system: RefCell<System>,
    gh_available: bool,
    // Invariant: the Vec is never empty — an arena with no PRs has no entry.
    // This keeps `contains_key` and `Option` call sites meaningful.
    pr_info: HashMap<EntityId, Vec<PrInfo>>,
    pr_last_checked: HashMap<EntityId, Instant>,
    pr_polling_timed_out: bool,
    _pr_poll_task: Option<Task<()>>,
    ci_status: HashMap<(EntityId, u32), CiInfo>,
    ci_last_checked: HashMap<(EntityId, u32), Instant>,
    ci_polling_timed_out: bool,
    _ci_poll_task: Option<Task<()>>,
    _window_activation_subscription: gpui::Subscription,
    _git_subscription: gpui::Subscription,
    _arena_subscriptions: HashMap<EntityId, gpui::Subscription>,
    pr_dirty: HashSet<EntityId>,
    last_branch_names: HashMap<EntityId, Option<String>>,
    pr_session_db: HashMap<String, HashMap<String, PrSessionData>>,
    _pr_session_db_write_task: Option<Task<()>>,
    _inbox_write_task: Option<Task<()>>,
    // Snapshot of the most recent arena-derived PR fetch per canonical worktree
    // path, persisted so `agentium task info` can show PRs without an app
    // instance running. Loaded once at startup; see `apply_pr_fetch_result`.
    pr_cache: HashMap<String, Vec<PrInfo>>,
    _pr_cache_write_task: Option<Task<()>>,
    board: board::Board,
    _board_write_task: Option<Task<()>>,
    bee_available: bool,
    // Backlog issue key -> numeric issue id, needed by `bee pr list --issue`.
    // Ids are immutable, so entries are never invalidated.
    backlog_issue_ids: HashMap<String, u64>,
    // Keys that Backlog reported as nonexistent (deleted issues, false-positive
    // branch segments like FIX-123); skipped on later polls to avoid re-resolving
    // every cycle. Transient failures are NOT recorded here and retry naturally.
    failed_backlog_issue_keys: HashSet<String>,
    _issue_fetch_task: Option<Task<()>>,
    sidebar_tab: SidebarTab,
    agent_activity: Option<(EntityId, Point<Pixels>)>,
    inbox_questions: Vec<InboxQuestion>,
    next_inbox_question_id: u64,
    inbox_completions: Vec<InboxCompletion>,
    inbox_todos: Entity<inbox_todo_view::InboxTodos>,
    inbox_todo_views: HashMap<u64, WeakEntity<inbox_todo_view::InboxTodoView>>,
    /// Lists whose closed owner tab is being brought back; the detail tab
    /// opens once the owner binds.
    pending_todo_restores: HashMap<u64, inbox_todo_view::PendingRestore>,
    /// Why restoring a list's owner tab failed, shown on its row until the
    /// next click retries.
    todo_restore_errors: HashMap<u64, String>,
    _inbox_todos_subscription: Subscription,
    inbox_note_editors: HashMap<u64, Entity<Editor>>,
    inbox_permissions: Vec<InboxPermission>,
    completion_notifications: Vec<WindowHandle<agent_ui::AgentNotification>>,
    _completion_notification_subscriptions: Vec<Subscription>,
    /// Tabs seen Completed or Failed on the previous sync, so a finished turn
    /// is recorded once, and not again after a dismiss.
    inbox_finished_items: HashMap<EntityId, FinishKey>,
    ci_fetch_generations: CiFetchGenerations,
    _inbox_completions_write_task: Option<Task<()>>,
    renaming_task: Option<Uuid>,
    task_rename_editor: Entity<Editor>,
    adding_issue_to_task: Option<Uuid>,
    issue_input_editor: Entity<Editor>,
    issue_input_error: Option<SharedString>,
    board_cache: BoardCache,
    #[cfg(target_os = "macos")]
    caffeinate_absent_since: HashMap<String, Instant>,
    #[cfg(target_os = "macos")]
    _caffeinate_monitor_task: Task<()>,
}

impl AgentiumApp {
    pub fn new(
        workspace_entity: Entity<Workspace>,
        project: Entity<Project>,
        app_state: Arc<AppState>,
        env_loaded: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let codex_tabs = cx.new(|_| {
            codex_acp_persistence::CodexAcpPersistence::new(
                paths::data_dir().join("codex_acp_tabs.json"),
            )
        });
        let codex_tabs_subscription = cx.observe(&codex_tabs, |_, _, cx| cx.notify());
        let codex_quit_subscription = cx.on_app_quit(|this, cx| {
            for arena in this.arenas.clone() {
                this.sync_codex_tabs(&arena, cx);
            }
            let pending = this.codex_tabs.update(cx, |store, _| store.finish());
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
            }
        });
        let git_store = project.read(cx).git_store().clone();
        let git_subscription = cx.subscribe(&git_store, |this, _, event: &GitStoreEvent, cx| {
            if let GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _) = event {
                if let Some(arena) = this.active_arena().cloned() {
                    let entity_id = arena.entity_id();

                    let current_branch = this.branch_name_for_arena(entity_id, cx);
                    // Only reset the dirty flag when the branch name actually
                    // changed (a real branch switch), not on commits/rebases
                    // that merely advance HEAD on the same branch.
                    if let Some(previous) = this.last_branch_names.get(&entity_id) {
                        if *previous != current_branch {
                            this.pr_dirty.remove(&entity_id);
                        }
                    }
                    this.last_branch_names.insert(entity_id, current_branch);

                    this.pr_info.remove(&entity_id);
                    this.ci_status.retain(|(id, _), _| *id != entity_id);
                    this.ci_fetch_generations.forget_arena(entity_id);
                    if this.gh_available || this.bee_available {
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

        let task_rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        cx.subscribe_in(&task_rename_editor, window, |this: &mut Self, _, event: &EditorEvent, _window, cx| {
            if let EditorEvent::Blurred = event {
                this.finish_rename_task(false, cx);
            }
        }).detach();

        let issue_input_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("PROJ-123 / owner/repo#12 / URL", window, cx);
            editor
        });
        cx.subscribe_in(&issue_input_editor, window, |this: &mut Self, _, event: &EditorEvent, _window, cx| {
            if let EditorEvent::Blurred = event {
                this.finish_add_issue(false, cx);
            }
        }).detach();

        let window_activation_subscription =
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    if (this.gh_available || this.bee_available)
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
                    // Closed/missing worktree rows can go stale if directories
                    // are deleted or recreated outside Agentium.
                    this.rebuild_board_cache(cx);
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
            // Wait for the login shell environment before probing: a Finder
            // launch starts with a minimal PATH, and probing too early would
            // permanently disable gh/bee integration for the session.
            env_loaded.await.ok();
            let (gh_available, bee_available) = cx
                .background_executor()
                .spawn(async {
                    futures::join!(
                        version_check_succeeds("gh"),
                        version_check_succeeds("bee")
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                this.gh_available = gh_available;
                this.bee_available = bee_available;
                if gh_available || bee_available {
                    this.start_pr_polling(cx);
                    this.fetch_issue_metadata(true, cx);
                }
                if gh_available {
                    this.start_ci_polling(cx);
                }
            })
            .ok();
        })
        .detach();

        #[cfg(target_os = "macos")]
        let caffeinate_monitor_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(3))
                    .await;

                let sessions_to_check: Vec<(String, u32)> = this
                    .update(cx, |this, _cx| {
                        this.claude_sessions
                            .iter()
                            .filter_map(|(session_id, session)| {
                                if !matches!(
                                    session.state,
                                    ClaudeSessionState::Running
                                        | ClaudeSessionState::WaitingPermission
                                ) {
                                    return None;
                                }
                                session
                                    .ancestor_pids
                                    .first()
                                    .map(|&pid| (session_id.clone(), pid))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if sessions_to_check.is_empty() {
                    if this.update(cx, |_, _| {}).is_err() {
                        break;
                    }
                    continue;
                }

                let mut results: Vec<(String, bool, bool)> = Vec::new();
                for (session_id, claude_pid) in sessions_to_check {
                    let (alive, has_caffeinate) = cx
                        .background_executor()
                        .spawn(async move {
                            let alive =
                                unsafe { libc::kill(claude_pid as i32, 0) == 0 };
                            if !alive {
                                return (false, false);
                            }
                            let has_caff = smol::process::Command::new("pgrep")
                                .args(&[
                                    "-P",
                                    &claude_pid.to_string(),
                                    "caffeinate",
                                ])
                                .output()
                                .await
                                .map_or(false, |o| o.status.success());
                            (alive, has_caff)
                        })
                        .await;
                    results.push((session_id, alive, has_caffeinate));
                }

                let should_stop = this
                    .update(cx, |this, cx| {
                        let mut changed = false;
                        for (session_id, alive, has_caffeinate) in &results {
                            let is_active = this
                                .claude_sessions
                                .get(session_id)
                                .map_or(false, |s| {
                                    matches!(
                                        s.state,
                                        ClaudeSessionState::Running
                                            | ClaudeSessionState::WaitingPermission
                                    )
                                });
                            if !is_active {
                                this.caffeinate_absent_since.remove(session_id);
                                continue;
                            }

                            if !alive {
                                // The Claude process is gone — drop the session entirely
                                // so that pid_to_session_id releases the wrapper PID and
                                // the terminal can show as a non-Claude busy terminal
                                // again. SessionEnd is the canonical cleanup path; this
                                // is the safety net for hard kills (SIGKILL, terminal
                                // close, crash) where SessionEnd doesn't fire.
                                if this.claude_sessions.remove(session_id).is_some() {
                                    changed = true;
                                }
                                this.caffeinate_absent_since.remove(session_id);
                            } else if *has_caffeinate {
                                this.caffeinate_absent_since.remove(session_id);
                            } else {
                                let absent_since = this
                                    .caffeinate_absent_since
                                    .entry(session_id.clone())
                                    .or_insert_with(Instant::now);
                                if absent_since.elapsed() > Duration::from_secs(5)
                                {
                                    if let Some(session) =
                                        this.claude_sessions.get_mut(session_id)
                                    {
                                        session.state = ClaudeSessionState::Idle;
                                        changed = true;
                                    }
                                    this.caffeinate_absent_since
                                        .remove(session_id);
                                }
                            }
                        }

                        if changed {
                            this.sync_session_derived_state();
                            this.notify_all_panes(cx);
                            cx.notify();
                        }
                    })
                    .is_err();

                if should_stop {
                    break;
                }
            }
        });

        // Periodically re-render so the "busy non-Claude" sidebar badge updates.
        // No GPUI event fires when `tcgetpgrp` state changes (e.g. user runs
        // `sleep 30` at a shell prompt), so we poll at a modest cadence.
        let busy_badge_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });

        let inbox_questions: Vec<InboxQuestion> = load_inbox_file(INBOX_QUESTIONS_FILE);
        let mut inbox_completions: Vec<InboxCompletion> = load_inbox_file(INBOX_COMPLETIONS_FILE);
        inbox_completions.retain(|completion| completion.confirmed_at.is_none());
        let inbox_todos = cx.new(|_| {
            inbox_todo_view::InboxTodos::new(Some(paths::data_dir().join(INBOX_TODOS_FILE)))
        });
        // Arenas re-render too: their pane decorators show which tab owns the
        // to-do list in front.
        let inbox_todos_subscription = cx.observe(&inbox_todos, |this, _, cx| {
            for arena in &this.arenas {
                arena.update(cx, |_, cx| cx.notify());
            }
            cx.notify();
        });
        let next_inbox_question_id = next_inbox_id(
            &inbox_questions,
            &inbox_completions,
            inbox_todos.read(cx).lists(),
        );
        let mut this = Self {
            codex_tabs,
            _codex_tabs_subscription: codex_tabs_subscription,
            _codex_quit_subscription: codex_quit_subscription,
            arena_error: None,
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
            title_driven_states: HashMap::new(),
            should_move_window: false,
            rate_limits: None,
            _rate_limits_refresh_task: None,
            _busy_badge_refresh_task: busy_badge_refresh_task,
            // The RefreshKind here is irrelevant — no processes are loaded at
            // construction time. The actual refresh spec is passed to
            // `refresh_processes_specifics` per call.
            sysinfo_system: RefCell::new(System::new()),
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
            pr_dirty: HashSet::new(),
            last_branch_names: HashMap::new(),
            pr_session_db: load_pr_session_db(),
            _pr_session_db_write_task: None,
            _inbox_write_task: None,
            _inbox_completions_write_task: None,
            pr_cache: load_pr_cache(),
            _pr_cache_write_task: None,
            board: board::load_board(),
            _board_write_task: None,
            bee_available: false,
            backlog_issue_ids: HashMap::new(),
            failed_backlog_issue_keys: HashSet::new(),
            _issue_fetch_task: None,
            sidebar_tab: SidebarTab::Repositories,
            agent_activity: None,
            next_inbox_question_id,
            inbox_questions,
            inbox_completions,
            inbox_todos,
            inbox_todo_views: HashMap::new(),
            pending_todo_restores: HashMap::new(),
            todo_restore_errors: HashMap::new(),
            _inbox_todos_subscription: inbox_todos_subscription,
            inbox_note_editors: HashMap::new(),
            inbox_permissions: Vec::new(),
            completion_notifications: Vec::new(),
            _completion_notification_subscriptions: Vec::new(),
            inbox_finished_items: HashMap::new(),
            ci_fetch_generations: CiFetchGenerations::default(),
            renaming_task: None,
            task_rename_editor,
            adding_issue_to_task: None,
            issue_input_editor,
            issue_input_error: None,
            board_cache: BoardCache::default(),
            #[cfg(target_os = "macos")]
            caffeinate_absent_since: HashMap::new(),
            #[cfg(target_os = "macos")]
            _caffeinate_monitor_task: caffeinate_monitor_task,
        };
        this.rebuild_board_cache(cx);
        this
    }

    pub fn add_arena_with_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) => {
                self.arena_error = Some(format!("Could not open {}: {error}", path.display()));
                cx.notify();
                return;
            }
        };
        if let Some(index) = self
            .arenas
            .iter()
            .position(|arena| arena.read(cx).working_directory.as_ref() == Some(&path))
        {
            self.switch_arena(index, window, cx);
            return;
        }
        self.arena_error = None;
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
        let saved_tabs = working_directory
            .as_ref()
            .map(|path| self.codex_tabs.read(cx).tabs(path))
            .unwrap_or_default();

        let arena_entity = cx.new(|cx| {
            Arena::new(arena_id, name, workspace_weak, project, working_directory, session_state, window, cx)
        });

        let subscription =
            cx.subscribe(
                &arena_entity,
                |this, arena, event: &ArenaEvent, cx| match event {
                    ArenaEvent::CodexTabsChanged => {
                        this.sync_codex_tabs(&arena, cx);
                        // Records ACP completions and failures while nothing
                        // renders (e.g. Agentium in the background).
                        this.sync_inbox_completions(cx);
                    }
                    ArenaEvent::ActiveItemChanged => cx.notify(),
                    // Claude session clearing routes through AgentiumApp because
                    // claude_sessions state lives here. Non-Claude task acknowledgment
                    // is handled locally in Arena::handle_key_input via the shared
                    // acknowledged_task_pids set.
                    ArenaEvent::TerminalKeyInput { shell_pid } => {
                        this.clear_session_for_shell_pid(*shell_pid, cx);
                    }
                    ArenaEvent::TerminalTitleState { shell_pid, state } => {
                        this.handle_terminal_title_state(*shell_pid, *state, cx);
                    }
                },
            );
        self._arena_subscriptions
            .insert(arena_entity.entity_id(), subscription);

        self.arenas.push(arena_entity.clone());
        arena_entity.update(cx, |arena, cx| {
            for saved in saved_tabs {
                arena.add_codex_acp(None, Some(saved), window, cx).log_err();
            }
        });
        self.active_arena_index = Some(self.arenas.len() - 1);
        arena_entity.update(cx, |arena, cx| {
            arena.activate_context(cx);
        });
        if self.gh_available || self.bee_available {
            self.fetch_pr_for_arena(arena_entity.entity_id(), cx);
        }
        self.rebuild_board_cache(cx);
        let focus = arena_entity.focus_handle(cx);
        focus.focus(window, cx);
        cx.notify();
    }

    fn sync_codex_tabs(&mut self, arena: &Entity<Arena>, cx: &mut Context<Self>) {
        if !self.arenas.contains(arena) {
            return;
        }
        let arena = arena.read(cx);
        if let Some(directory) = arena.working_directory.clone() {
            let tabs = arena.codex_tabs(cx);
            self.codex_tabs
                .update(cx, |store, cx| store.replace(directory, tabs, cx));
        }
    }

    fn resume_codex_conversation(
        &mut self,
        origin: &Entity<Arena>,
        directory: &Path,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let origin_index = self
            .arenas
            .iter()
            .position(|arena| arena == origin)
            .ok_or_else(|| anyhow::anyhow!("The originating Arena is no longer open"))?;
        let existing = codex_acp_resume::resume_target(
            &self.arenas,
            origin,
            directory,
            session_id,
            self.codex_tabs.read(cx),
            cx,
        )?;
        self.switch_arena(origin_index, window, cx);
        if let Some(codex_acp_resume::OpenConversation { pane, view }) = existing {
            let index = pane
                .read(cx)
                .index_for_item(&view)
                .ok_or_else(|| anyhow::anyhow!("The conversation tab is no longer open"))?;
            pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx);
                pane.zoom_out(&ZoomOut, window, cx);
            });
            window.focus(&view.focus_handle(cx), cx);
        } else {
            origin.update(cx, |arena, cx| {
                arena.resume_codex_tab(session_id.to_owned(), window, cx)
            })?;
            self.sync_codex_tabs(origin, cx);
        }
        Ok(())
    }

    fn switch_arena(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.arenas.len() {
            self.agent_activity = None;
            self.active_arena_index = Some(index);
            self.arenas[index].update(cx, |arena, cx| {
                arena.activate_context(cx);
            });
            if self.gh_available || self.bee_available {
                let entity_id = self.arenas[index].entity_id();
                let pr_stale = self
                    .pr_last_checked
                    .get(&entity_id)
                    .map_or(true, |t| t.elapsed() > Duration::from_secs(60));
                if pr_stale {
                    self.fetch_pr_for_arena(entity_id, cx);
                }
                let ci_stale_numbers: Vec<u32> = self
                    .pr_info
                    .get(&entity_id)
                    .map(|prs| {
                        prs.iter()
                            .filter(|pr| {
                                pr.provider == PrProvider::GitHub
                                    && self
                                        .ci_last_checked
                                        .get(&(entity_id, pr.number))
                                        .map_or(true, |t| t.elapsed() > Duration::from_secs(60))
                            })
                            .map(|pr| pr.number)
                            .collect()
                    })
                    .unwrap_or_default();
                for pr_number in ci_stale_numbers {
                    self.fetch_ci_for_arena(entity_id, pr_number, cx);
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
                            let entity_id = arena.entity_id();
                            Some((entity_id, working_dir, this.pr_fetch_context(entity_id, cx)))
                        })
                    })
                    .ok()
                    .flatten();

                if let Some((entity_id, working_dir, context)) = fetch_info {
                    let start = Instant::now();
                    let result = cx
                        .background_executor()
                        .spawn(async move { fetch_pr_list(&working_dir, context).await })
                        .await;
                    let elapsed = start.elapsed();

                    let should_stop = this
                        .update(cx, |this, cx| {
                            if elapsed > Duration::from_secs(5) {
                                this.pr_polling_timed_out = true;
                                this._pr_poll_task = None;
                                return true;
                            }
                            this.apply_pr_fetch_result(entity_id, result, cx);
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

    /// Snapshot the inputs `fetch_pr_list` needs so the background fetch never
    /// touches app state.
    fn pr_fetch_context(&self, entity_id: EntityId, cx: &App) -> PrFetchContext {
        let task_ids = self
            .arenas
            .iter()
            .position(|arena| arena.entity_id() == entity_id)
            .map(|index| self.tasks_containing_arena(index, cx))
            .unwrap_or_default();
        self.pr_fetch_context_for_tasks(&task_ids)
    }

    /// Same as `pr_fetch_context`, but for an explicit set of task ids rather
    /// than the tasks containing one arena. Used by `task_info`, which must
    /// aggregate over every task that shares a worktree (§3.6): fetching with
    /// only one such task's inputs would drop another task's manually linked
    /// PRs from the shared `pr_cache` / `pr_info` entry.
    fn pr_fetch_context_for_tasks(&self, task_ids: &[Uuid]) -> PrFetchContext {
        let tasks: Vec<&board::BoardTask> = task_ids
            .iter()
            .filter_map(|task_id| self.board.tasks.iter().find(|task| task.id == *task_id))
            .collect();
        let (task_issue_keys, task_pr_refs) = task_fetch_inputs(&tasks, self.bee_available);
        PrFetchContext {
            gh_available: self.gh_available,
            bee_available: self.bee_available,
            task_issue_keys,
            task_pr_refs,
            known_issue_ids: self.backlog_issue_ids.clone(),
            failed_issue_keys: self.failed_backlog_issue_keys.clone(),
        }
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
        let context = self.pr_fetch_context(entity_id, cx);

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch_pr_list(&working_dir, context).await })
                .await;
            this.update(cx, |this, cx| {
                this.apply_pr_fetch_result(entity_id, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_pr_fetch_result(
        &mut self,
        entity_id: EntityId,
        result: anyhow::Result<PrFetchOutcome>,
        cx: &mut Context<Self>,
    ) {
        if self.apply_pr_fetch_result_without_persist(entity_id, result, cx) {
            self.persist_pr_cache(cx);
        }
    }

    /// Applies a PR fetch result to `pr_info` (and its CI-chain / pr-session
    /// side effects) and mirrors it into `pr_cache`, without writing
    /// `pr_cache` to disk. Returns whether `pr_cache` changed, so callers that
    /// batch several fetches (`task_info --update`) can call `persist_pr_cache`
    /// once at the end instead of once per worktree. Canonicalizes the
    /// arena's `working_directory` on the caller's thread (fine for PR
    /// polling and `fetch_pr_for_arena`, which already run on the
    /// foreground); `task_info --update`'s apply stage must not do that (its
    /// `this.update` closure runs on the foreground) and calls
    /// `_at` directly with a path already canonicalized in the background.
    fn apply_pr_fetch_result_without_persist(
        &mut self,
        entity_id: EntityId,
        result: anyhow::Result<PrFetchOutcome>,
        cx: &mut Context<Self>,
    ) -> bool {
        let canonical_path = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)
            .and_then(|a| a.read(cx).working_directory.clone())
            .map(|working_dir| {
                std::fs::canonicalize(&working_dir)
                    .unwrap_or(working_dir)
                    .to_string_lossy()
                    .to_string()
            });
        self.apply_pr_fetch_result_without_persist_at(entity_id, canonical_path.as_deref(), result, cx)
    }

    /// Same as `apply_pr_fetch_result_without_persist`, but takes the
    /// worktree's canonical path (the `pr_cache` / pr.json key) as an
    /// argument instead of recomputing it via `canonicalize` on the caller's
    /// thread. `task_info --update` resolves the canonical path in its
    /// background fetch step and passes it here, so its `this.update` apply
    /// stage never touches the filesystem.
    fn apply_pr_fetch_result_without_persist_at(
        &mut self,
        entity_id: EntityId,
        canonical_path: Option<&str>,
        result: anyhow::Result<PrFetchOutcome>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pr_last_checked.insert(entity_id, Instant::now());
        match result {
            Ok(outcome) => {
                self.backlog_issue_ids.extend(outcome.resolved_backlog_ids);
                self.failed_backlog_issue_keys
                    .extend(outcome.failed_backlog_keys);
                if outcome.prs.is_empty() {
                    self.pr_info.remove(&entity_id);
                } else {
                    let existing: HashSet<u32> = self
                        .pr_info
                        .get(&entity_id)
                        .map(|prs| prs.iter().map(|pr| pr.number).collect())
                        .unwrap_or_default();
                    // Chain a CI fetch for PRs that just appeared so their status
                    // shows up immediately instead of on the next CI polling cycle.
                    // Backlog has no CI API, so only GitHub PRs are chained.
                    let new_numbers: Vec<u32> = outcome
                        .prs
                        .iter()
                        .filter(|pr| pr.provider == PrProvider::GitHub)
                        .map(|pr| pr.number)
                        .filter(|number| !existing.contains(number))
                        .collect();
                    self.pr_info.insert(entity_id, outcome.prs);
                    if !self.ci_polling_timed_out {
                        for number in new_numbers {
                            self.fetch_ci_for_arena(entity_id, number, cx);
                        }
                    }
                    // `canonical_path` is `None` only when no arena/working
                    // directory could be found for `entity_id`, in which case
                    // `save_pr_session_mapping`'s own lookup would also fail.
                    if self.pr_dirty.contains(&entity_id) {
                        if let Some(path) = canonical_path {
                            self.save_pr_session_mapping_at(entity_id, path, cx);
                        }
                    }
                }
            }
            Err(_) => {
                self.pr_info.remove(&entity_id);
            }
        }

        // Snapshot the fetch result to disk so `agentium task info` can show
        // arena PRs without an app instance running. Keyed the same way as
        // `save_pr_session_mapping` so both agree on one arena's identity.
        let changed = match canonical_path {
            Some(path) => {
                let new_entry = self.pr_info.get(&entity_id).cloned();
                self.update_pr_cache(path.to_string(), new_entry)
            }
            None => false,
        };

        cx.notify();
        changed
    }

    /// Inserts or removes `pr_cache`'s entry for `path`, without touching
    /// disk. Returns whether the entry actually changed, so callers can skip
    /// a redundant `persist_pr_cache`.
    fn update_pr_cache(&mut self, path: String, prs: Option<Vec<PrInfo>>) -> bool {
        let old_entry = match &prs {
            Some(prs) => self.pr_cache.insert(path, prs.clone()),
            None => self.pr_cache.remove(&path),
        };
        old_entry != prs
    }

    /// Writes `pr_cache` to disk in the background, chained after any prior
    /// pending write rather than replacing it, so two writes can never race
    /// on the same `pr_cache.json.tmp` (dropping the earlier `Task` would not
    /// have stopped it — it was already polling on a background thread).
    fn persist_pr_cache(&mut self, cx: &mut Context<Self>) {
        let previous = self._pr_cache_write_task.take();
        let snapshot = self.pr_cache.clone();
        self._pr_cache_write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            write_pr_cache(&snapshot).log_err();
        }));
    }

    /// Applies a PR fetch result for a worktree with no live arena (a
    /// `Closed` row in `task_info --update`, or a `Live` one whose arena
    /// closed while the fetch was in flight): absorbs the Backlog id caches
    /// and mirrors into `pr_cache` by `path`, but never touches `pr_info`
    /// (there is no `EntityId` to key it by). Does not persist to disk; the
    /// caller batches every worktree of one `task_info --update` and persists
    /// once at the end.
    fn apply_detached_pr_fetch(
        &mut self,
        path: String,
        outcome: PrFetchOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        self.backlog_issue_ids.extend(outcome.resolved_backlog_ids);
        self.failed_backlog_issue_keys
            .extend(outcome.failed_backlog_keys);
        let changed = self.update_pr_cache(path, (!outcome.prs.is_empty()).then_some(outcome.prs));
        cx.notify();
        changed
    }

    fn start_ci_polling(&mut self, cx: &mut Context<Self>) {
        self._ci_poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let targets = this
                    .update(cx, |this, cx| {
                        let mut targets = Vec::new();
                        for arena in &this.arenas {
                            let entity_id = arena.entity_id();
                            let Some(prs) = this.pr_info.get(&entity_id) else {
                                continue;
                            };
                            let interval = this.compute_ci_poll_interval(entity_id, cx);
                            for pr in prs {
                                // Backlog has no CI API.
                                if pr.provider != PrProvider::GitHub
                                    || matches!(pr.status, PrStatus::Merged)
                                {
                                    continue;
                                }
                                let pr_number = pr.number;

                                let last =
                                    this.ci_last_checked.get(&(entity_id, pr_number)).copied();
                                let due = last.map_or(true, |t| t.elapsed() >= interval);

                                if due {
                                    if let Some(working_dir) =
                                        arena.read(cx).working_directory.clone()
                                    {
                                        let token = this
                                            .ci_fetch_generations
                                            .start((entity_id, pr_number));
                                        targets.push((entity_id, working_dir, pr_number, token));
                                    }
                                }
                            }
                        }
                        targets
                    })
                    .unwrap_or_default();

                let mut should_stop = false;
                for (entity_id, working_dir, pr_number, token) in targets {
                    // A direct fetch, an arena close or a branch switch since
                    // this batch was queued supersedes the queued request.
                    let superseded = this
                        .read_with(cx, |this, _| {
                            !this
                                .ci_fetch_generations
                                .is_current((entity_id, pr_number), token)
                        })
                        .unwrap_or(true);
                    if superseded {
                        continue;
                    }
                    let start = Instant::now();
                    let result = cx
                        .background_executor()
                        .spawn({
                            let working_dir = working_dir.clone();
                            async move {
                                fetch_ci_status(Some(&working_dir), pr_number, None).await
                            }
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
                            this.set_ci_status(
                                entity_id,
                                working_dir,
                                pr_number,
                                token,
                                result.ok().flatten(),
                                cx,
                            );
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
                            let Some(prs) = this.pr_info.get(&entity_id) else {
                                continue;
                            };
                            let interval = this.compute_ci_poll_interval(entity_id, cx);
                            for pr in prs {
                                // Backlog has no CI API.
                                if pr.provider != PrProvider::GitHub
                                    || matches!(pr.status, PrStatus::Merged)
                                {
                                    continue;
                                }
                                let elapsed = this
                                    .ci_last_checked
                                    .get(&(entity_id, pr.number))
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
        let token = self.ci_fetch_generations.start((entity_id, pr_number));

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let working_dir = working_dir.clone();
                    async move { fetch_ci_status(Some(&working_dir), pr_number, None).await }
                })
                .await;
            this.update(cx, |this, cx| {
                this.set_ci_status(
                    entity_id,
                    working_dir,
                    pr_number,
                    token,
                    result.ok().flatten(),
                    cx,
                );
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
        project_url: Option<String>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let arena_index = self
            .arenas
            .iter()
            .position(|arena| arena.entity_id() == arena_entity.entity_id());
        let arena_path =
            arena_index.and_then(|index| self.arena_canonical_path(index, cx));
        let assigned_tasks =
            arena_index.map_or(Vec::new(), |index| self.tasks_containing_arena(index, cx));
        let task_choices: Vec<(Uuid, String)> = self
            .board
            .active_tasks()
            .filter(|task| !assigned_tasks.contains(&task.id))
            .map(|task| (task.id, task.title.clone()))
            .collect();
        let arena_for_rename = arena_entity.clone();
        let arena_for_close = arena_entity;
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            let menu = menu.entry(
                "Rename…",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.start_rename_arena(arena_for_rename.clone(), window, cx);
                }),
            );
            let menu = if let (Some(arena_index), Some(arena_path)) = (arena_index, arena_path) {
                let submenu_this = this.clone();
                let menu = menu.submenu("Assign to Task…", move |mut submenu, window, _cx| {
                    for (task_id, title) in &task_choices {
                        let task_id = *task_id;
                        submenu = submenu.entry(
                            title.clone(),
                            None,
                            window.handler_for(&submenu_this, move |this, _window, cx| {
                                this.assign_arena_to_task(arena_index, task_id, cx);
                            }),
                        );
                    }
                    if !task_choices.is_empty() {
                        submenu = submenu.separator();
                    }
                    let arena_path = arena_path.clone();
                    submenu.entry(
                        "New Task with Arena",
                        None,
                        window.handler_for(&submenu_this, move |this, window, cx| {
                            this.create_task(Some(arena_path.clone()), window, cx);
                        }),
                    )
                });
                if assigned_tasks.is_empty() {
                    menu
                } else {
                    menu.entry(
                        "Remove from Task",
                        None,
                        window.handler_for(&this, move |this, _window, cx| {
                            this.remove_arena_from_tasks(arena_index, cx);
                        }),
                    )
                }
            } else {
                menu
            };
            let menu = if let Some(url) = project_url {
                menu.entry(
                    "Open GitHub Repository",
                    None,
                    window.handler_for(&this, move |_this, _window, cx| {
                        cx.open_url(&url);
                    }),
                )
            } else {
                menu
            };
            menu.separator().entry(
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
        self.finish_rename_task(false, cx);
        self.finish_add_issue(false, cx);
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
        if let Some(directory) = workspace.read(cx).working_directory.clone() {
            self.codex_tabs
                .update(cx, |store, cx| store.replace(directory, Vec::new(), cx));
        }
        let entity_id = workspace.entity_id();
        self._arena_subscriptions.remove(&entity_id);
        self.pr_info.remove(&entity_id);
        self.pr_last_checked.remove(&entity_id);
        self.ci_status.retain(|(id, _), _| *id != entity_id);
        self.ci_last_checked.retain(|(id, _), _| *id != entity_id);
        self.ci_fetch_generations.forget_arena(entity_id);
        self.pr_dirty.remove(&entity_id);
        self.last_branch_names.remove(&entity_id);
        self.arenas.remove(index);
        self.rebuild_board_cache(cx);

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
        }
        cx.notify();
    }

    fn sync_session_derived_state(&self) {
        let mut ready_pids = HashSet::new();
        let mut running_pids = HashSet::new();
        let mut permission_pids = HashSet::new();
        for session in self.claude_sessions.values() {
            match session.effective_state() {
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
        for (&pid, &state) in &self.title_driven_states {
            match state {
                ClaudeSessionState::Completed => {
                    ready_pids.insert(pid);
                }
                ClaudeSessionState::Running => {
                    running_pids.insert(pid);
                }
                ClaudeSessionState::WaitingPermission => {
                    permission_pids.insert(pid);
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
                title_busy: false,
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
        self.drop_inbox_permissions(session_id);
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
                    title_busy: false,
                    user_prompt: String::new(),
                    status_message,
                },
            );
        }
        let pids: Vec<u32> = self.claude_sessions.get(session_id)
            .map(|s| s.ancestor_pids.clone())
            .unwrap_or_default();
        self.sync_session_derived_state();
        self.notify_all_panes(cx);

        if self.gh_available || self.bee_available {
            if let Some(entity_id) = self.find_arena_entity_id_for_pids(&pids, cx) {
                self.fetch_pr_for_arena(entity_id, cx);
            }
        }

        self.sync_inbox_completions(cx);
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
                    title_busy: false,
                    user_prompt: String::new(),
                    status_message: title,
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        self.sync_inbox_completions(cx);
        cx.notify();
    }

    pub fn set_claude_session_prompt(
        &mut self,
        session_id: &str,
        ancestor_pids: Vec<u32>,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        self.drop_inbox_permissions(session_id);
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
                    title_busy: false,
                    user_prompt: prompt,
                    status_message: String::new(),
                },
            );
        }
        self.sync_session_derived_state();
        self.notify_all_panes(cx);
        cx.notify();

        let pids: Vec<u32> = self
            .claude_sessions
            .get(session_id)
            .map(|s| s.ancestor_pids.clone())
            .unwrap_or_default();
        if let Some(entity_id) = self.find_arena_entity_id_for_pids(&pids, cx) {
            self.pr_dirty.insert(entity_id);
            if self.pr_info.contains_key(&entity_id) {
                self.save_pr_session_mapping(entity_id, cx);
            }
        }
    }

    /// Registers a PermissionRequest hook waiting on the CLI socket. The
    /// returned task resolves to the hook's reply: a `decision` once the user
    /// answers in the Inbox, or none if the request is dropped.
    pub fn inbox_permission_request(
        &mut self,
        session_id: &str,
        peer_ancestor_pids: &[u32],
        tool_name: &str,
        tool_input: &serde_json::Value,
        suggestions: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Task<serde_json::Value> {
        // The session's Claude process must be an ancestor of the hook, so a
        // process elsewhere cannot attach a fake prompt to a real session.
        if let Some(session) = self.claude_sessions.get(session_id)
            && !session
                .ancestor_pids
                .first()
                .is_some_and(|claude_pid| peer_ancestor_pids.contains(claude_pid))
        {
            return Task::ready(serde_json::json!({
                "ok": false,
                "error": "the requester is not the session's Claude process",
            }));
        }
        // The hook's datagram and this request race on separate sockets; set
        // the state here too so the request is never pruned as stale.
        self.handle_claude_permission_request(session_id, peer_ancestor_pids.to_vec(), cx);
        let (reply, receiver) = futures::channel::oneshot::channel();
        let id = self.next_inbox_question_id;
        self.next_inbox_question_id += 1;
        self.inbox_permissions.push(InboxPermission {
            id,
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            summary: permission_summary(tool_input),
            suggestions,
            reply,
        });
        cx.notify();
        cx.background_spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| serde_json::json!({ "ok": true }))
        })
    }

    /// Ends the hook's wait without a decision. Called once the session has
    /// moved on, which means the user answered in the terminal.
    fn drop_inbox_permissions(&mut self, session_id: &str) {
        self.inbox_permissions
            .retain(|permission| permission.session_id != session_id);
    }

    /// Drops requests whose session left WaitingPermission through a path
    /// with no hook (e.g. the caffeinate monitor after Ctrl-C).
    fn drop_stale_inbox_permissions(&mut self) {
        let sessions = &self.claude_sessions;
        self.inbox_permissions.retain(|permission| {
            sessions
                .get(&permission.session_id)
                .is_some_and(|session| session.state == ClaudeSessionState::WaitingPermission)
        });
    }

    fn answer_inbox_permission(
        &mut self,
        permission_id: u64,
        answer: InboxPermissionAnswer,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self
            .inbox_permissions
            .iter()
            .position(|permission| permission.id == permission_id)
        else {
            return;
        };
        let permission = self.inbox_permissions.remove(position);
        let decision = match answer {
            InboxPermissionAnswer::Allow => serde_json::json!({ "behavior": "allow" }),
            InboxPermissionAnswer::AlwaysAllow => serde_json::json!({
                "behavior": "allow",
                "updatedPermissions": permission.suggestions,
            }),
            InboxPermissionAnswer::Deny => serde_json::json!({
                "behavior": "deny",
                "message": "Denied from the Agentium Inbox",
            }),
        };
        if permission
            .reply
            .send(serde_json::json!({ "ok": true, "decision": decision }))
            .is_err()
        {
            log::warn!(
                "permission request {permission_id} for {} was already gone",
                permission.tool_name
            );
        }
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
                    title_busy: false,
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
        self.drop_inbox_permissions(session_id);
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

    pub fn handle_claude_session_end(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.drop_inbox_permissions(session_id);
        if self.claude_sessions.remove(session_id).is_some() {
            self.caffeinate_absent_since.remove(session_id);
            self.sync_session_derived_state();
            self.notify_all_panes(cx);
            cx.notify();
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

    /// Adds a tab to the arena `selector` names. A caller outside every arena
    /// (a shell in some other terminal app) falls back to the active arena,
    /// where `tab new` always went before it learned about arenas. A tab that
    /// lands in a hidden arena must not take focus: focusing an unrendered
    /// item blurs the visible arena and swallows keystrokes. Only terminals
    /// can be added that way; the other views focus themselves (some
    /// asynchronously) and switch the git store's active repository.
    pub fn handle_tab_new(
        &mut self,
        selector: &ArenaSelector,
        content_type: PaneContentType,
        title: Option<String>,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let arena = match selector {
            ArenaSelector::Path(_) => self.resolve_arena(selector, cx)?,
            ArenaSelector::Caller { .. } => self
                .resolve_arena(selector, cx)
                .ok()
                .or_else(|| self.active_arena())
                .ok_or_else(|| anyhow::anyhow!("no arena is open"))?,
        }
        .clone();
        let in_active_arena = self
            .active_arena()
            .is_some_and(|active| active.entity_id() == arena.entity_id());
        if !in_active_arena && !matches!(content_type, PaneContentType::Terminal) {
            anyhow::bail!("only terminal tabs can be added to an arena other than the active one");
        }
        arena.update(cx, |arena, cx| {
            arena.add_tab(content_type, title, command, in_active_arena, window, cx)
        })
    }

    fn resolve_arena(
        &self,
        selector: &ArenaSelector,
        cx: &App,
    ) -> anyhow::Result<&Entity<Arena>> {
        let arena = match selector {
            ArenaSelector::Path(path) => self.arena_containing_path(path, cx),
            ArenaSelector::Caller { ancestor_pids, cwd } => self
                .find_arena_entity_id_for_pids(ancestor_pids, cx)
                .and_then(|entity_id| {
                    self.arenas
                        .iter()
                        .find(|arena| arena.entity_id() == entity_id)
                })
                .or_else(|| {
                    cwd.as_deref()
                        .and_then(|cwd| self.arena_containing_path(cwd, cx))
                }),
        };
        arena.ok_or_else(|| anyhow::anyhow!("no arena for {selector:?}"))
    }

    fn terminal_shell_pid(&self, item: &dyn workspace::ItemHandle, cx: &App) -> Option<u32> {
        let terminal_view = item.act_as::<TerminalView>(cx)?;
        let getter = terminal_view.read(cx).terminal().read(cx).pid_getter()?;
        Some(getter.fallback_pid().as_u32())
    }

    fn tab_process_id(&self, item: &dyn workspace::ItemHandle, cx: &App) -> Option<u32> {
        if let Some(view) = item.act_as::<codex_acp_view::CodexAcpView>(cx) {
            view.read(cx).process_id(cx)
        } else {
            self.terminal_shell_pid(item, cx)
        }
    }

    fn tab_info(
        &self,
        item: &dyn workspace::ItemHandle,
        arena: &Entity<Arena>,
        cx: &App,
    ) -> TabInfo {
        if let Some(view) = item.act_as::<codex_acp_view::CodexAcpView>(cx) {
            return TabInfo {
                title: item.tab_content_text(0, cx).to_string(),
                kind: "codex-acp",
                state: Some(view.read(cx).state().cli_state()),
                arena: arena.read(cx).working_directory.clone(),
            };
        }
        let state = self.terminal_shell_pid(item, cx).map(|pid| {
            if self.session_state.permission_shell_pids.borrow().contains(&pid) {
                "permission"
            } else if self.session_state.running_shell_pids.borrow().contains(&pid) {
                "running"
            } else if self.session_state.ready_shell_pids.borrow().contains(&pid) {
                "ready"
            } else {
                "idle"
            }
        });
        let is_terminal = item.act_as::<TerminalView>(cx).is_some();
        TabInfo {
            title: item.tab_content_text(0, cx).to_string(),
            kind: if is_terminal { "terminal" } else { "other" },
            state,
            arena: arena.read(cx).working_directory.clone(),
        }
    }

    pub fn tab_self(&self, ancestor_pids: &[u32], cx: &App) -> anyhow::Result<TabInfo> {
        let (arena, _pane, item) = self.tab_for_ancestors(ancestor_pids, cx)?;
        Ok(self.tab_info(item.as_ref(), &arena, cx))
    }

    fn tab_for_ancestors(
        &self,
        ancestor_pids: &[u32],
        cx: &App,
    ) -> anyhow::Result<(Entity<Arena>, Entity<Pane>, Box<dyn workspace::ItemHandle>)> {
        for ancestor in ancestor_pids {
            let mut matches = Vec::new();
            for arena in &self.arenas {
                for pane in arena.read(cx).center.panes() {
                    for item in pane.read(cx).items() {
                        if self.tab_process_id(item.as_ref(), cx) == Some(*ancestor) {
                            matches.push((arena.clone(), pane.clone(), item.boxed_clone()));
                        }
                    }
                }
            }
            if matches.len() > 1 {
                anyhow::bail!("caller matches multiple Agentium tabs");
            }
            if let Some(found) = matches.pop() {
                return Ok(found);
            }
        }
        anyhow::bail!("caller is not running inside an Agentium terminal or ACP tab")
    }

    pub fn tab_list(&self, selector: &ArenaSelector, cx: &App) -> anyhow::Result<Vec<TabInfo>> {
        let arena = self.resolve_arena(selector, cx)?;
        let mut tabs = Vec::new();
        for pane in arena.read(cx).center.panes() {
            for item in pane.read(cx).items() {
                tabs.push(self.tab_info(item.as_ref(), arena, cx));
            }
        }
        Ok(tabs)
    }

    fn unique_terminal_view_by_title(
        &self,
        title: &str,
        selector: &ArenaSelector,
        cx: &App,
    ) -> anyhow::Result<Entity<TerminalView>> {
        self.unique_item_by_title(title, selector, cx)?
            .act_as::<TerminalView>(cx)
            .ok_or_else(|| {
                anyhow::anyhow!("tab {title:?} is not a terminal; tab log is not supported")
            })
    }

    fn unique_item_by_title(
        &self,
        title: &str,
        selector: &ArenaSelector,
        cx: &App,
    ) -> anyhow::Result<Box<dyn workspace::ItemHandle>> {
        let arena = self.resolve_arena(selector, cx)?;
        let mut matches = Vec::new();
        for pane in arena.read(cx).center.panes() {
            for item in pane.read(cx).items() {
                if &*item.tab_content_text(0, cx) == title {
                    matches.push(item.boxed_clone());
                }
            }
        }
        let item = match matches.as_slice() {
            [item] => item,
            [] => anyhow::bail!("no tab titled {title:?}"),
            _ => anyhow::bail!("{} tabs titled {title:?}", matches.len()),
        };
        Ok(item.boxed_clone())
    }

    /// The last `lines` lines (all when 0) of the terminal tab titled `title`.
    pub fn tab_log(
        &self,
        title: &str,
        selector: &ArenaSelector,
        lines: usize,
        cx: &App,
    ) -> anyhow::Result<String> {
        let terminal_view = self.unique_terminal_view_by_title(title, selector, cx)?;
        let content = terminal_view.read(cx).terminal().read(cx).get_content();
        Ok(tail_lines(&content, lines))
    }

    pub fn handle_tab_send_message(
        &mut self,
        title: &str,
        selector: &ArenaSelector,
        submit: bool,
        from: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let item = self.unique_item_by_title(title, selector, cx)?;
        self.send_message_to_item(item.as_ref(), submit, &with_from_line(from, text), window, cx)
    }

    fn send_message_to_item(
        &self,
        item: &dyn workspace::ItemHandle,
        submit: bool,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<()> {
        let title = item.tab_content_text(0, cx).to_string();
        if let Some(view) = item.act_as::<codex_acp_view::CodexAcpView>(cx) {
            return view.update(cx, |view, cx| view.send_message(text, submit, window, cx));
        }
        let terminal_view = item
            .act_as::<TerminalView>(cx)
            .ok_or_else(|| anyhow::anyhow!("tab {title:?} cannot receive messages"))?;
        let terminal = terminal_view.read(cx).terminal().clone();
        self.ensure_safe_message_target(&title, terminal.read(cx))?;
        terminal.update(cx, |terminal, _cx| {
            if !text.is_empty() {
                terminal.paste(&text);
            }
            if submit {
                terminal.input(&b"\r"[..]);
            }
        });
        Ok(())
    }

    /// The CLI socket is reachable from inside agent sandboxes, so pasting into
    /// a shell would run commands outside the sandbox, and pasting into a
    /// permission dialog would answer it without the user.
    fn ensure_safe_message_target(
        &self,
        title: &str,
        terminal: &terminal::Terminal,
    ) -> anyhow::Result<()> {
        let getter = terminal
            .pid_getter()
            .ok_or_else(|| anyhow::anyhow!("tab {title:?} has no process"))?;
        let shell_pid = getter.fallback_pid().as_u32();
        if self
            .session_state
            .permission_shell_pids
            .borrow()
            .contains(&shell_pid)
        {
            anyhow::bail!(
                "tab {title:?} is waiting for a permission answer; ask the user instead"
            );
        }
        let live_pid = terminal
            .pid()
            .ok_or_else(|| anyhow::anyhow!("tab {title:?} has no foreground process"))?;
        let mut system = self.sysinfo_system.borrow_mut();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[live_pid]),
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        let name = system.process(live_pid).and_then(argv0_name).ok_or_else(|| {
            anyhow::anyhow!("cannot identify the foreground process of tab {title:?}")
        })?;
        // An allowlist rather than a shell denylist: text pasted while any
        // other program (including this CLI, when sending to its own tab) is in
        // the foreground stays in the tty queue and the shell runs it once that
        // program exits.
        if !is_agent_process_name(name) {
            anyhow::bail!(
                "tab {title:?} is running {name}, not a coding agent; refusing to paste"
            );
        }
        Ok(())
    }

    /// Registers a question from the tab whose process tree contains
    /// `peer_ancestor_pids`. Only the kernel-verified peer ancestry is used:
    /// the answer is later pasted into that tab, so a self-reported pid list
    /// would let any process redirect it.
    pub fn inbox_ask(
        &mut self,
        peer_ancestor_pids: &[u32],
        question: String,
        context: Option<String>,
        options: Vec<InboxOption>,
        recommended: Option<usize>,
        assumed: bool,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<u64> {
        if question.trim().is_empty() {
            anyhow::bail!("the question is empty");
        }
        if assumed && recommended.is_none() {
            anyhow::bail!("--assume needs --recommended to name the assumed option");
        }
        if let Some(index) = recommended
            && index >= options.len()
        {
            anyhow::bail!("--recommended {} is out of range", index + 1);
        }
        let (arena, pane, item) = self.tab_for_ancestors(peer_ancestor_pids, cx)?;
        let id = self.next_inbox_question_id;
        self.next_inbox_question_id += 1;
        self.inbox_questions.push(InboxQuestion {
            id,
            question,
            context,
            options,
            recommended,
            assumed,
            arena_name: arena.read(cx).name.clone(),
            asker: self.inbox_asker(item.as_ref(), cx),
            tab: Some(InboxTab {
                arena: arena.downgrade(),
                pane: pane.downgrade(),
                item: item.downgrade_item(),
            }),
            error: None,
        });
        self.persist_inbox(cx);
        cx.notify();
        Ok(id)
    }

    /// Replaces the to-do list of the tab whose process tree contains
    /// `peer_ancestor_pids` (kernel-verified, as for `inbox_ask`). `items` is
    /// already validated by `parse_todo_set_request`.
    pub fn todo_set(
        &mut self,
        peer_ancestor_pids: &[u32],
        items: Vec<InboxTodoItem>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<TodoSetReply> {
        validate_todo_items(&items)?;
        let (arena, pane, item) = self.tab_for_ancestors(peer_ancestor_pids, cx)?;
        let asker = self.inbox_asker(item.as_ref(), cx);
        let tab = InboxTab {
            arena: arena.downgrade(),
            pane: pane.downgrade(),
            item: item.downgrade_item(),
        };
        let previous_title = todo_list_index(
            self.inbox_todos.read(cx).lists(),
            item.item_id(),
            asker.as_ref(),
        )
        .map(|index| self.inbox_todos.read(cx).lists()[index].title.clone());
        let owner = inbox_todo_view::TodoOwner {
            title: self.tab_headline(item.as_ref(), previous_title, cx),
            arena_name: arena.read(cx).name.clone(),
            arena_path: arena.read(cx).working_directory.clone(),
            branch: self.branch_name_for_arena(arena.entity_id(), cx),
        };
        let count = items.len();
        let mut next_id = self.next_inbox_question_id;
        let dropped = self.inbox_todos.update(cx, |todos, cx| {
            todos.set_items(
                item.item_id(),
                tab,
                asker,
                owner,
                items,
                &mut next_id,
                cx,
            )
        });
        self.next_inbox_question_id = next_id;
        Ok(TodoSetReply { count, dropped })
    }

    /// What the agent in the tab is working on: the prompt for a Claude
    /// terminal (the tab title is just "Claude"), the terminal title for other
    /// agents, the tab title otherwise.
    /// The latest prompt can be the Inbox's own answer to this list; the
    /// list keeps `previous` then rather than titling itself after it.
    fn tab_headline(
        &self,
        item: &dyn workspace::ItemHandle,
        previous: Option<String>,
        cx: &App,
    ) -> String {
        if let Some(view) = item.act_as::<TerminalView>(cx) {
            let terminal = view.read(cx).terminal().read(cx);
            if let Some(pid) = terminal.pid_getter().map(|getter| getter.fallback_pid().as_u32())
                && let Some(session) = self
                    .claude_sessions
                    .values()
                    .find(|session| session.ancestor_pids.contains(&pid))
                && !session.user_prompt.trim().is_empty()
            {
                return headline_from_prompt(&session.user_prompt, previous)
                    .unwrap_or_else(|| item.tab_content_text(0, cx).to_string());
            }
            if !terminal.breadcrumb_text.trim().is_empty() {
                return terminal.breadcrumb_text.clone();
            }
        }
        item.tab_content_text(0, cx).to_string()
    }

    /// The caller's list, and read-only the lists of the other tabs in its arena.
    pub fn todo_list(&self, peer_ancestor_pids: &[u32], cx: &App) -> anyhow::Result<TodoListReply> {
        let (arena, _pane, item) = self.tab_for_ancestors(peer_ancestor_pids, cx)?;
        let asker = self.inbox_asker(item.as_ref(), cx);
        let lists = self.inbox_todos.read(cx).lists();
        let own_index = todo_list_index(lists, item.item_id(), asker.as_ref());
        let own = own_index
            .map(|index| lists[index].items.clone())
            .unwrap_or_default();
        let others = lists
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != own_index)
            .filter_map(|(_, list)| {
                let item_id = list.tab.as_ref()?.item.id();
                let (list_arena, _, list_item) =
                    inbox_todo_view::find_item_location(&self.arenas, item_id, cx)?;
                (list_arena == arena).then(|| {
                    (
                        list_item.tab_content_text(0, cx).to_string(),
                        list.items.clone(),
                    )
                })
            })
            .collect();
        Ok(TodoListReply { own, others })
    }

    pub fn todo_clear(
        &mut self,
        peer_ancestor_pids: &[u32],
        cx: &mut Context<Self>,
    ) -> anyhow::Result<usize> {
        let (_arena, _pane, item) = self.tab_for_ancestors(peer_ancestor_pids, cx)?;
        let asker = self.inbox_asker(item.as_ref(), cx);
        Ok(self.inbox_todos.update(cx, |todos, cx| {
            todos.clear_for(item.item_id(), asker.as_ref(), cx)
        }))
    }

    fn dismiss_inbox_todo(&mut self, todo_id: u64, cx: &mut Context<Self>) {
        self.inbox_todos
            .update(cx, |todos, cx| todos.dismiss(todo_id, cx));
    }

    /// Opens the list's detail tab beside its owner tab, or brings back the
    /// one already open (moving it beside the owner if needed).
    fn open_inbox_todo(&mut self, todo_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = self.inbox_todos.read(cx).list(todo_id) else {
            return;
        };
        let owner_open = list.tab.as_ref().is_some_and(|tab| {
            inbox_todo_view::find_item_location(&self.arenas, tab.item.id(), cx).is_some()
        });
        if !owner_open {
            match self.pending_todo_restores.get(&todo_id).copied() {
                Some(inbox_todo_view::PendingRestore::Launched { item_id }) => {
                    // Show what the resumed session is doing (asking
                    // something, or failing) instead of starting another.
                    if let Some((arena, pane, item)) =
                        inbox_todo_view::find_item_location(&self.arenas, item_id, cx)
                        && let Some(index) =
                            self.arenas.iter().position(|candidate| *candidate == arena)
                    {
                        self.navigate_to_item(index, &pane, item.as_ref(), window, cx);
                    }
                    return;
                }
                Some(_) => return,
                None => {}
            }
            let open_arenas: Vec<(String, PathBuf)> = self
                .arenas
                .iter()
                .filter_map(|arena| {
                    let arena = arena.read(cx);
                    Some((arena.name.clone(), arena.working_directory.clone()?))
                })
                .collect();
            let plan = inbox_todo_view::todo_restore_plan(list, &open_arenas);
            let asker = list.asker.clone();
            self.todo_restore_errors.remove(&todo_id);
            match plan {
                inbox_todo_view::RestorePlan::Fallback(reason) => {
                    self.todo_restore_errors.insert(todo_id, reason);
                }
                plan => {
                    self.restore_todo_owner(todo_id, plan, asker, window, cx);
                    cx.notify();
                    return;
                }
            }
        }
        let view = match self
            .inbox_todo_views
            .get(&todo_id)
            .and_then(|view| view.upgrade())
        {
            Some(view) => view,
            None => {
                let todos = self.inbox_todos.clone();
                let view = cx.new(|cx| {
                    inbox_todo_view::InboxTodoView::new(todos, todo_id, window, cx)
                });
                cx.subscribe_in(&view, window, Self::handle_inbox_todo_view_event)
                    .detach();
                self.inbox_todo_views.insert(todo_id, view.downgrade());
                view
            }
        };
        let owner = self
            .inbox_todos
            .read(cx)
            .list(todo_id)
            .and_then(|list| list.tab.as_ref().map(|tab| tab.item.id()))
            .and_then(|item_id| inbox_todo_view::find_item_location(&self.arenas, item_id, cx));
        match owner {
            Some((owner_arena, owner_pane, owner_item)) => {
                if let Some(index) = self.arenas.iter().position(|arena| *arena == owner_arena) {
                    self.switch_arena(index, window, cx);
                }
                inbox_todo_view::show_detail_beside_owner(
                    &view,
                    &owner_arena,
                    &owner_pane,
                    owner_item.as_ref(),
                    &self.arenas,
                    window,
                    cx,
                );
            }
            // No owner to sit beside: wherever it already is, else the
            // active pane.
            None => match inbox_todo_view::find_item_location(&self.arenas, view.entity_id(), cx) {
                Some((arena, pane, item)) => {
                    if let Some(index) = self.arenas.iter().position(|candidate| *candidate == arena) {
                        self.navigate_to_item(index, &pane, item.as_ref(), window, cx);
                    }
                }
                None => {
                    let Some(arena) = self.active_arena().cloned() else {
                        return;
                    };
                    let pane = arena.read(cx).active_pane.clone();
                    pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(view), true, true, None, window, cx);
                    });
                }
            },
        }
        cx.notify();
    }

    /// Brings back the closed owner tab of a list: `claude --resume` for a
    /// Claude session, its arena (which restores saved ACP tabs) for Codex ACP.
    fn restore_todo_owner(
        &mut self,
        todo_id: u64,
        plan: inbox_todo_view::RestorePlan,
        asker: Option<InboxAsker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let arena_path = match &plan {
            inbox_todo_view::RestorePlan::ResumeClaude { arena_path, .. }
            | inbox_todo_view::RestorePlan::OpenArena { arena_path } => arena_path.clone(),
            inbox_todo_view::RestorePlan::Fallback(_) => return,
        };
        // `add_arena_with_path` reports failure only through `arena_error`, so
        // success is checked by finding the arena, never by the active one.
        self.add_arena_with_path(arena_path.clone(), window, cx);
        let canonical = std::fs::canonicalize(&arena_path).ok();
        let arena = self
            .arenas
            .iter()
            .find(|arena| {
                arena.read(cx).working_directory.as_ref().is_some_and(|path| {
                    *path == arena_path || Some(path) == canonical.as_ref()
                })
            })
            .cloned();
        match plan {
            inbox_todo_view::RestorePlan::ResumeClaude { session_id, .. } => {
                let Some(arena) = arena else {
                    let detail = self.arena_error.clone().unwrap_or_default();
                    self.todo_restore_errors.insert(
                        todo_id,
                        format!("Could not open {}. {detail}", arena_path.display())
                            .trim()
                            .to_string(),
                    );
                    return;
                };
                self.pending_todo_restores
                    .insert(todo_id, inbox_todo_view::PendingRestore::Launching);
                let launch = arena.update(cx, |arena, cx| {
                    arena.resume_claude_session(session_id, window, cx)
                });
                cx.spawn(async move |this, cx| {
                    let result = launch.await;
                    this.update(cx, |this, cx| {
                        if let Some(error) = inbox_todo_view::record_restore_launch(
                            &mut this.pending_todo_restores,
                            todo_id,
                            result,
                        ) {
                            this.todo_restore_errors.insert(todo_id, error);
                        }
                        cx.notify();
                    })
                })
                .detach_and_log_err(cx);
            }
            inbox_todo_view::RestorePlan::OpenArena { .. } => {
                let asker_tab = asker
                    .as_ref()
                    .is_some_and(|asker| self.tab_for_asker(asker, cx).is_some());
                match inbox_todo_view::acp_restore_outcome(arena.is_some(), asker_tab) {
                    Ok(pending) => {
                        self.pending_todo_restores.insert(todo_id, pending);
                    }
                    Err(error) => {
                        self.todo_restore_errors.insert(todo_id, error);
                    }
                }
            }
            inbox_todo_view::RestorePlan::Fallback(_) => {}
        }
    }

    fn handle_inbox_todo_view_event(
        &mut self,
        view: &Entity<inbox_todo_view::InboxTodoView>,
        event: &inbox_todo_view::InboxTodoViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inbox_todo_view::InboxTodoViewEvent::Send { todo_id, notes } = event;
        // Located again now: the binding is only refreshed on render, and the
        // tab may have been closed since.
        let owner = self
            .inbox_todos
            .read(cx)
            .list(*todo_id)
            .and_then(|list| list.tab.as_ref().map(|tab| tab.item.id()))
            .and_then(|item_id| inbox_todo_view::find_item_location(&self.arenas, item_id, cx))
            .map(|(_, _, item)| item);
        let todos = self.inbox_todos.clone();
        let result = todos.update(cx, |todos, cx| {
            todos.send(
                *todo_id,
                notes,
                owner.as_deref(),
                |item, message, cx| self.send_message_to_item(item, true, message, window, cx),
                cx,
            )
        });
        let error = result.err().map(|error| format!("{error:#}"));
        view.update(cx, |view, cx| view.set_error(error, cx));
    }

    fn inbox_asker(&self, item: &dyn workspace::ItemHandle, cx: &App) -> Option<InboxAsker> {
        if let Some(view) = item.act_as::<codex_acp_view::CodexAcpView>(cx) {
            return view
                .read(cx)
                .saved_tab()
                .map(|saved| InboxAsker::CodexAcp { saved_id: saved.id });
        }
        let shell_pid = self.terminal_shell_pid(item, cx)?;
        self.session_state
            .pid_to_session_id
            .borrow()
            .get(&shell_pid)
            .map(|session_id| InboxAsker::ClaudeSession {
                session_id: session_id.clone(),
            })
    }

    /// Re-attaches questions and completions whose tab is gone (closed, or
    /// not yet reopened after a restart) to the tab now hosting the same
    /// asker, and drops questions that have lost their tab and have no asker
    /// to wait for. Completions stay until dismissed.
    fn bind_inbox_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut found = Vec::new();
        for (index, question) in self.inbox_questions.iter().enumerate() {
            if question.live_tab().is_some() {
                continue;
            }
            if let Some(tab) = question
                .asker
                .as_ref()
                .and_then(|asker| self.tab_for_asker(asker, cx))
            {
                found.push((index, tab));
            }
        }
        for (index, tab) in found {
            if let Some(question) = self.inbox_questions.get_mut(index) {
                question.tab = Some(tab);
                question.error = None;
            }
        }
        self.inbox_questions
            .retain(|question| question.asker.is_some() || question.live_tab().is_some());

        let mut found = Vec::new();
        for (index, completion) in self.inbox_completions.iter().enumerate() {
            if completion.live_tab().is_some() {
                continue;
            }
            if let Some(tab) = completion
                .asker
                .as_ref()
                .and_then(|asker| self.tab_for_asker(asker, cx))
            {
                found.push((index, tab));
            }
        }
        for (index, tab) in found {
            if let Some(completion) = self.inbox_completions.get_mut(index) {
                completion.tab = Some(tab);
            }
        }

        // Ownership means being in a pane now: a closed tab's entity can
        // outlive it, and a moved tab keeps its item but not its pane.
        let bindings: Vec<(u64, Option<InboxTab>)> = self
            .inbox_todos
            .read(cx)
            .lists()
            .iter()
            .map(|list| {
                let current = list
                    .tab
                    .as_ref()
                    .and_then(|tab| {
                        inbox_todo_view::find_item_location(&self.arenas, tab.item.id(), cx)
                    })
                    .map(|(arena, pane, item)| InboxTab {
                        arena: arena.downgrade(),
                        pane: pane.downgrade(),
                        item: item.downgrade_item(),
                    });
                let tab = current.or_else(|| {
                    list.asker
                        .as_ref()
                        .and_then(|asker| self.tab_for_asker(asker, cx))
                });
                (list.id, tab)
            })
            .collect();
        self.inbox_todos
            .update(cx, |todos, cx| todos.rebind(bindings, cx));

        let todos = self.inbox_todos.read(cx);
        let outcomes = inbox_todo_view::advance_pending_restores(
            &mut self.pending_todo_restores,
            |todo_id, restore| {
                let list = todos.list(todo_id);
                inbox_todo_view::RestoreObservation {
                    list_exists: list.is_some(),
                    list_bound: list.is_some_and(|list| list.tab.is_some()),
                    launched_tab_open: match restore {
                        inbox_todo_view::PendingRestore::Launched { item_id } => {
                            inbox_todo_view::find_item_location(&self.arenas, *item_id, cx)
                                .is_some()
                        }
                        _ => true,
                    },
                }
            },
        );
        self.todo_restore_errors
            .retain(|todo_id, _| todos.list(*todo_id).is_some());
        for (todo_id, outcome) in outcomes {
            match outcome {
                inbox_todo_view::RestoreOutcome::Open => {
                    cx.defer_in(window, move |this, window, cx| {
                        this.open_inbox_todo(todo_id, window, cx);
                    });
                }
                inbox_todo_view::RestoreOutcome::Failed(error) => {
                    self.todo_restore_errors.insert(todo_id, error);
                }
            }
        }
    }

    fn tab_for_asker(&self, asker: &InboxAsker, cx: &App) -> Option<InboxTab> {
        for arena in &self.arenas {
            for pane in arena.read(cx).center.panes() {
                for item in pane.read(cx).items() {
                    if self.inbox_asker(item.as_ref(), cx).as_ref() == Some(asker) {
                        return Some(InboxTab {
                            arena: arena.downgrade(),
                            pane: pane.downgrade(),
                            item: item.downgrade_item(),
                        });
                    }
                }
            }
        }
        None
    }

    /// Records each tab that finished a turn (Completed, or a Codex ACP tab in
    /// Error) since the last sync, then ages the records; see
    /// `record_finished_tabs` and `age_inbox_records`.
    fn sync_inbox_completions(&mut self, cx: &mut Context<Self>) {
        let asking_items: HashSet<EntityId> = self
            .inbox_questions
            .iter()
            .filter(|question| !question.assumed)
            .filter_map(|question| question.tab.as_ref().map(|tab| tab.item.id()))
            .collect();
        let mut finished = Vec::new();
        for arena in &self.arenas {
            let arena_name = arena.read(cx).name.clone();
            let completed = self
                .collect_ready_terminal_infos(arena, cx)
                .into_iter()
                .map(|info| (info, None));
            let failed = codex_badge_entries(arena.read(cx), codex_acp_view::CodexAcpState::Error, cx)
                .into_iter()
                .map(|info| (info, Some("Stopped with an error".to_string())));
            for (info, failure) in completed.chain(failed) {
                let item_id = info.item.item_id();
                let acp_view = info.item.act_as::<codex_acp_view::CodexAcpView>(cx);
                let key = FinishKey {
                    failed: failure.is_some(),
                    turn: acp_view
                        .as_ref()
                        .map_or(0, |view| view.read(cx).finished_turns()),
                };
                // ACP tabs pop up their own notification from the conversation view.
                let notify = acp_view.is_none()
                    && !self.is_tab_in_view(&arena.downgrade(), &info.pane, item_id, cx);
                finished.push(FinishedTab {
                    item_id,
                    key,
                    tab: Some(InboxTab {
                        arena: arena.downgrade(),
                        pane: info.pane.downgrade(),
                        item: info.item.downgrade_item(),
                    }),
                    asker: self.inbox_asker(info.item.as_ref(), cx),
                    arena_name: arena_name.clone(),
                    title: info.primary,
                    detail: info.secondary,
                    failure,
                    notify,
                });
            }
        }
        let now = Utc::now();
        let recorded = record_finished_tabs(
            &mut self.inbox_completions,
            &mut self.inbox_finished_items,
            finished,
            &asking_items,
            &mut self.next_inbox_question_id,
            now,
        );
        let mut changed = !recorded.is_empty();
        for (id, notify) in recorded {
            if notify {
                self.notify_inbox_record(id, cx);
            }
        }

        let mut records = std::mem::take(&mut self.inbox_completions);
        changed |= age_inbox_records(&mut records, now, |record| {
            if record.pull_request.is_some() {
                return RecordedTabState::Idle;
            }
            let Some((arena, pane, item)) = record.live_tab() else {
                return RecordedTabState::Gone;
            };
            if matches!(
                self.tab_info(item.as_ref(), &arena, cx).state,
                Some("running" | "permission")
            ) {
                RecordedTabState::Busy
            } else if self.is_active_tab(&arena, &pane, item.item_id(), cx) {
                RecordedTabState::Viewed
            } else {
                RecordedTabState::Idle
            }
        });
        self.inbox_completions = records;
        if changed {
            self.persist_inbox_completions(cx);
        }
    }

    fn notify_inbox_record(&self, id: u64, cx: &mut Context<Self>) {
        let Some(record) = self.inbox_completions.iter().find(|record| record.id == id) else {
            return;
        };
        let title = record.title.clone();
        let caption = record.failure.clone().unwrap_or_else(|| record.detail.clone());
        let arena_name = record.arena_name.clone();
        // Deferred: the sync also runs inside `render`, where opening a window
        // is not safe.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.show_completion_notification(id, title, caption, arena_name, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    /// Applies one CI response. Stale responses (a newer request for the PR
    /// started, or the arena closed or switched branches since) are dropped.
    fn set_ci_status(
        &mut self,
        entity_id: EntityId,
        arena_path: PathBuf,
        pr_number: u32,
        token: u64,
        status: Option<CiInfo>,
        cx: &mut Context<Self>,
    ) {
        let key = (entity_id, pr_number);
        if !self
            .ci_fetch_generations
            .accept(key, token, &mut self.ci_last_checked, Instant::now())
        {
            return;
        }
        let Some(arena) = self
            .arenas
            .iter()
            .find(|arena| arena.entity_id() == entity_id)
        else {
            return;
        };
        let arena_name = arena.read(cx).name.clone();
        let title = self
            .pr_info
            .get(&entity_id)
            .and_then(|prs| prs.iter().find(|pr| pr.number == pr_number))
            .map_or_else(|| format!("PR #{pr_number}"), |pr| pr.title.to_string());
        let (changed, recorded) = apply_ci_status(
            &mut self.inbox_completions,
            &InboxPullRequest {
                arena_path,
                number: pr_number,
            },
            self.ci_status.get(&key).map(|info| &info.status),
            status.as_ref(),
            title,
            arena_name,
            &mut self.next_inbox_question_id,
            Utc::now(),
        );
        if changed {
            self.persist_inbox_completions(cx);
        }
        if let Some(id) = recorded {
            self.notify_inbox_record(id, cx);
        }
        match status {
            Some(status) => {
                self.ci_status.insert(key, status);
            }
            None => {
                self.ci_status.remove(&key);
            }
        }
        cx.notify();
    }

    /// Whether the user is looking at the tab: Agentium is the active window
    /// and the tab is the active one in it.
    fn is_tab_in_view(
        &self,
        arena: &WeakEntity<Arena>,
        pane: &Entity<Pane>,
        item_id: EntityId,
        cx: &App,
    ) -> bool {
        cx.active_window()
            .is_some_and(|window| window.downcast::<AgentiumApp>().is_some())
            && arena
                .upgrade()
                .is_some_and(|arena| self.is_active_tab(&arena, pane, item_id, cx))
    }

    /// The same pop-up Codex ACP tabs show, for terminal agents. Honors the
    /// agent panel's `notify_when_agent_waiting` setting so one switch
    /// controls both.
    fn show_completion_notification(
        &mut self,
        completion_id: u64,
        title: String,
        caption: String,
        arena_name: String,
        cx: &mut Context<Self>,
    ) {
        use agent_settings::AgentSettings;
        use settings::{NotifyWhenAgentWaiting, Settings as _};

        let screens = match AgentSettings::get_global(cx).notify_when_agent_waiting {
            NotifyWhenAgentWaiting::PrimaryScreen => cx.primary_display().into_iter().collect(),
            NotifyWhenAgentWaiting::AllScreens => cx.displays(),
            NotifyWhenAgentWaiting::Never => return,
        };
        // A newer completion replaces the previous pop-up instead of stacking.
        self.dismiss_completion_notifications(cx);
        let title = if title.trim().is_empty() {
            arena_name.clone()
        } else {
            truncate_for_menu(&title)
        };
        let caption = if caption.trim().is_empty() {
            "Finished".to_string()
        } else {
            truncate_for_menu(&caption)
        };
        for screen in screens {
            let options = agent_ui::AgentNotification::window_options(screen, cx);
            let Some(window) = cx
                .open_window(options, |_window, cx| {
                    cx.new(|_cx| {
                        agent_ui::AgentNotification::new(
                            title.clone(),
                            Some(caption.clone().into()),
                            IconName::ZedAssistant,
                            Some(arena_name.clone()),
                        )
                    })
                })
                .log_err()
            else {
                continue;
            };
            let Some(pop_up) = window.entity(cx).log_err() else {
                continue;
            };
            self._completion_notification_subscriptions.push(cx.subscribe(
                &pop_up,
                move |this, _, event: &agent_ui::AgentNotificationEvent, cx| match event {
                    agent_ui::AgentNotificationEvent::Accepted => {
                        this.dismiss_completion_notifications(cx);
                        let Some(app_window) = cx
                            .windows()
                            .into_iter()
                            .find_map(|window| window.downcast::<AgentiumApp>())
                        else {
                            return;
                        };
                        cx.activate(true);
                        // Deferred: this entity is being updated, and the
                        // window update below leases it again.
                        cx.defer(move |cx| {
                            app_window
                                .update(cx, |this, window, cx| {
                                    window.activate_window();
                                    this.open_inbox_completion(completion_id, window, cx);
                                })
                                .log_err();
                        });
                    }
                    agent_ui::AgentNotificationEvent::Dismissed => {
                        this.dismiss_completion_notifications(cx);
                    }
                },
            ));
            self.completion_notifications.push(window);
        }
    }

    fn dismiss_completion_notifications(&mut self, cx: &mut Context<Self>) {
        for window in self.completion_notifications.drain(..) {
            window
                .update(cx, |_, window, _| window.remove_window())
                .log_err();
        }
        self._completion_notification_subscriptions.clear();
    }

    fn dismiss_inbox_completion(&mut self, completion_id: u64, cx: &mut Context<Self>) {
        self.inbox_completions
            .retain(|completion| completion.id != completion_id);
        self.persist_inbox_completions(cx);
        cx.notify();
    }

    fn open_inbox_completion(
        &mut self,
        completion_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = self
            .inbox_completions
            .iter_mut()
            .find(|completion| completion.id == completion_id)
        else {
            return;
        };
        if let Some(pull_request) = completion.pull_request.clone() {
            // No tab to look at, so opening the row is the confirmation.
            if completion.confirmed_at.is_none() {
                completion.confirmed_at = Some(Utc::now());
                self.persist_inbox_completions(cx);
            }
            if let Some(index) = self.arenas.iter().position(|arena| {
                arena.read(cx).working_directory.as_ref() == Some(&pull_request.arena_path)
            }) {
                self.switch_arena(index, window, cx);
            }
            cx.notify();
            return;
        }
        let Some((arena, pane, item)) = completion.live_tab() else {
            return;
        };
        let Some(arena_index) = self
            .arenas
            .iter()
            .position(|candidate| candidate.entity_id() == arena.entity_id())
        else {
            return;
        };
        self.navigate_to_item(arena_index, &pane, item.as_ref(), window, cx);
    }

    fn persist_inbox_completions(&mut self, cx: &mut Context<Self>) {
        let previous = self._inbox_completions_write_task.take();
        let snapshot = serde_json::to_vec_pretty(&self.inbox_completions);
        self._inbox_completions_write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            snapshot
                .map_err(anyhow::Error::from)
                .and_then(|content| write_inbox_file(INBOX_COMPLETIONS_FILE, &content))
                .log_err();
        }));
    }

    fn dismiss_inbox_question(&mut self, question_id: u64, cx: &mut Context<Self>) {
        self.inbox_questions
            .retain(|question| question.id != question_id);
        self.persist_inbox(cx);
        cx.notify();
    }

    fn persist_inbox(&mut self, cx: &mut Context<Self>) {
        let previous = self._inbox_write_task.take();
        let snapshot = serde_json::to_vec_pretty(
            &self
                .inbox_questions
                .iter()
                .filter(|question| question.asker.is_some())
                .collect::<Vec<_>>(),
        );
        self._inbox_write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            snapshot
                .map_err(anyhow::Error::from)
                .and_then(|content| write_inbox_file(INBOX_QUESTIONS_FILE, &content))
                .log_err();
        }));
    }

    /// Sends the chosen option, with the note typed under the question as its
    /// condition. A question without options is answered by the note alone,
    /// so `option_index` is `None` only for those.
    fn answer_inbox_question(
        &mut self,
        question_id: u64,
        option_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self
            .inbox_questions
            .iter()
            .position(|question| question.id == question_id)
        else {
            return;
        };
        let note = self
            .inbox_note_editors
            .get(&question_id)
            .map(|editor| editor.read(cx).text(cx).trim().to_string())
            .unwrap_or_default();
        let question = &self.inbox_questions[position];
        let mut answer = match option_index {
            Some(index) => {
                let Some(option) = question.options.get(index) else {
                    return;
                };
                if note.is_empty() {
                    option.label.clone()
                } else {
                    format!("{}\nNote: {note}", option.label)
                }
            }
            None if question.options.is_empty() && !note.is_empty() => note.clone(),
            None => return,
        };
        let assumed_index = question.recommended.filter(|_| question.assumed);
        if let Some(assumed_index) = assumed_index {
            if option_index == Some(assumed_index) && note.is_empty() {
                // The agent is already working on this answer; telling it so
                // would only start a needless turn.
                self.inbox_questions.remove(position);
                self.inbox_note_editors.remove(&question_id);
                self.persist_inbox(cx);
                cx.notify();
                return;
            }
            let assumed_label = question
                .options
                .get(assumed_index)
                .map(|option| option.label.as_str())
                .unwrap_or_default();
            if option_index == Some(assumed_index) {
                answer.push_str("\nThis matches the option you assumed; apply the note.");
            } else {
                answer.push_str(&format!(
                    "\nYou assumed \"{assumed_label}\". Revise the work you did on that assumption."
                ));
            }
        }
        let text = with_from_line(
            "Agentium Inbox",
            &format!(
                "Answer to your `agentium inbox ask` question.\nQuestion: {}\nAnswer: {answer}",
                question.question
            ),
        );
        let result = match question.live_tab() {
            Some((_, _, item)) => self.send_message_to_item(item.as_ref(), true, &text, window, cx),
            None => Err(anyhow::anyhow!("the asking tab is not open")),
        };
        match result {
            Ok(()) => {
                self.inbox_questions.remove(position);
                self.inbox_note_editors.remove(&question_id);
                self.persist_inbox(cx);
            }
            Err(error) => {
                if let Some(question) = self.inbox_questions.get_mut(position) {
                    question.error = Some(format!("{error:#}"));
                }
            }
        }
        cx.notify();
    }

    fn sync_inbox_note_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question_ids: HashSet<u64> = self
            .inbox_questions
            .iter()
            .map(|question| question.id)
            .collect();
        self.inbox_note_editors
            .retain(|question_id, _| question_ids.contains(question_id));
        for question in &self.inbox_questions {
            let has_options = !question.options.is_empty();
            self.inbox_note_editors
                .entry(question.id)
                .or_insert_with(|| {
                    cx.new(|cx| {
                        let mut editor = Editor::auto_height(1, 6, window, cx);
                        editor.set_placeholder_text(
                            if has_options {
                                "Condition or note for the chosen option (optional)"
                            } else {
                                "Answer (Enter to send, Shift-Enter for a new line)"
                            },
                            window,
                            cx,
                        );
                        editor
                    })
                });
        }
    }

    fn focused_inbox_note(&self, window: &Window, cx: &App) -> Option<u64> {
        self.inbox_note_editors
            .iter()
            .find(|(_, editor)| editor.focus_handle(cx).is_focused(window))
            .map(|(question_id, _)| *question_id)
    }

    fn open_inbox_question(&mut self, question_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(question) = self
            .inbox_questions
            .iter()
            .find(|question| question.id == question_id)
        else {
            return;
        };
        let Some((arena, pane, item)) = question.live_tab() else {
            return;
        };
        let Some(arena_index) = self
            .arenas
            .iter()
            .position(|candidate| candidate.entity_id() == arena.entity_id())
        else {
            return;
        };
        self.navigate_to_item(arena_index, &pane, item.as_ref(), window, cx);
    }

    /// All tasks, active and archived (the CLI's `--json` and non-json list
    /// modes filter differently; that choice is left to the caller).
    pub fn task_list(&self) -> Vec<board::BoardTask> {
        self.board.tasks.clone()
    }

    /// The task selected by `selector` (or, if `None`, the task containing
    /// `cwd`): its issues and worktree PR state. With `update`, re-fetches PR
    /// and issue metadata via `gh`/`bee`, applying the result through the same
    /// path as PR polling before replying, instead of just reading the cache.
    pub fn task_info(
        &mut self,
        selector: Option<&str>,
        cwd: Option<&Path>,
        update: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<TaskInfoReply>> {
        let task_id = match self.board.resolve_target(selector, cwd) {
            Ok(task_id) => task_id,
            Err(error) => return Task::ready(Err(error)),
        };

        if !update {
            return Task::ready(self.build_task_info_reply(task_id));
        }

        let Some(task) = self.board.tasks.iter().find(|task| task.id == task_id) else {
            return Task::ready(Err(anyhow::anyhow!("resolved task id not found in board")));
        };
        let gh_available = self.gh_available;
        let bee_available = self.bee_available;

        let mut issue_refs: Vec<board::IssueRef> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for issue in &task.issues {
            let (provider_available, provider) = match &issue.reference {
                board::IssueRef::GitHub { .. } => (gh_available, "gh"),
                board::IssueRef::Backlog { .. } => (bee_available, "bee"),
            };
            if provider_available {
                issue_refs.push(issue.reference.clone());
            } else {
                warnings.push(format!(
                    "{provider} unavailable; cached issue {}",
                    issue.reference.short_label()
                ));
            }
        }

        if !gh_available && !bee_available {
            return match self.build_task_info_reply(task_id) {
                Ok(mut reply) => {
                    warnings.push("gh/bee not available; showing cached values".to_string());
                    reply.warnings.extend(warnings);
                    Task::ready(Ok(reply))
                }
                Err(error) => Task::ready(Err(error)),
            };
        }

        // Snapshot worktree rows from the already-computed board cache: no
        // canonicalize/exists on the foreground (§3.6). `rows` is built in
        // the same order as `task.worktrees` by `rebuild_board_cache`.
        let rows: Vec<WorktreeRow> = self
            .board_cache
            .task_worktrees
            .get(&task_id)
            .cloned()
            .unwrap_or_default();

        struct WorktreeFetch {
            path: PathBuf,
            entity_id: Option<EntityId>,
            context: PrFetchContext,
        }

        let mut worktree_fetches: Vec<WorktreeFetch> = Vec::new();
        for (worktree, row) in task.worktrees.iter().zip(rows.iter()) {
            match row {
                WorktreeRow::Missing { .. } => continue,
                WorktreeRow::Live { arena_index } => {
                    let Some(arena) = self.arenas.get(*arena_index) else {
                        continue;
                    };
                    let entity_id = arena.entity_id();
                    let shared_ids = tasks_sharing_arena(&self.board_cache.task_worktrees, *arena_index);
                    worktree_fetches.push(WorktreeFetch {
                        path: worktree.clone(),
                        entity_id: Some(entity_id),
                        context: self.pr_fetch_context_for_tasks(&shared_ids),
                    });
                }
                WorktreeRow::Closed { .. } => {
                    let shared_ids: Vec<Uuid> = self
                        .board
                        .active_tasks()
                        .filter(|task| task.worktrees.contains(worktree))
                        .map(|task| task.id)
                        .collect();
                    worktree_fetches.push(WorktreeFetch {
                        path: worktree.clone(),
                        entity_id: None,
                        context: self.pr_fetch_context_for_tasks(&shared_ids),
                    });
                }
            }
        }

        cx.spawn(async move |this, cx| {
            let (pr_results, issue_results) = cx
                .background_executor()
                .spawn(async move {
                    let pr_fetch =
                        futures::future::join_all(worktree_fetches.into_iter().map(|fetch| {
                            async move {
                                // Resolved here (background thread), not in the
                                // foreground apply stage below: `task_info` must
                                // not call `canonicalize` on the foreground.
                                let canonical_path = std::fs::canonicalize(&fetch.path)
                                    .unwrap_or_else(|_| fetch.path.clone())
                                    .to_string_lossy()
                                    .to_string();
                                let result = fetch_pr_list(&fetch.path, fetch.context).await;
                                (canonical_path, fetch.entity_id, result)
                            }
                        }));
                    let issue_fetch =
                        futures::future::join_all(issue_refs.into_iter().map(|reference| async {
                            let metadata = fetch_issue_metadata_for(&reference).await;
                            (reference, metadata)
                        }));
                    futures::join!(pr_fetch, issue_fetch)
                })
                .await;

            this.update(cx, |this, cx| {
                let mut pr_cache_changed = false;
                for (path, entity_id, result) in pr_results {
                    match result {
                        Err(error) => {
                            warnings.push(format!("failed to fetch PRs for {path}: {error:#}"));
                        }
                        Ok(outcome) if !outcome.failures.is_empty() => {
                            for failure in &outcome.failures {
                                warnings.push(format!("failed to fetch PRs for {path}: {failure}"));
                            }
                            this.backlog_issue_ids.extend(outcome.resolved_backlog_ids);
                            this.failed_backlog_issue_keys
                                .extend(outcome.failed_backlog_keys);
                        }
                        Ok(outcome) => {
                            // An arena that closed while its fetch was in flight
                            // has no live entity to key `pr_info` by anymore.
                            let live_entity_id = entity_id
                                .filter(|id| this.arenas.iter().any(|arena| arena.entity_id() == *id));
                            let changed = match live_entity_id {
                                Some(entity_id) => this.apply_pr_fetch_result_without_persist_at(
                                    entity_id,
                                    Some(&path),
                                    Ok(outcome),
                                    cx,
                                ),
                                None => this.apply_detached_pr_fetch(path, outcome, cx),
                            };
                            if changed {
                                pr_cache_changed = true;
                            }
                        }
                    }
                }
                if pr_cache_changed {
                    this.persist_pr_cache(cx);
                }

                warnings.extend(this.apply_issue_metadata(issue_results, cx));

                this.build_task_info_reply(task_id).map(|mut reply| {
                    reply.warnings.extend(warnings);
                    reply
                })
            })
            .unwrap_or_else(|error| Err(error))
        })
    }

    /// Builds a `task_info` reply straight from in-memory state (`board` and
    /// `pr_cache`): the non-`--update` path, and the last step of the
    /// `--update` path once fetches have been applied.
    fn build_task_info_reply(&self, task_id: Uuid) -> anyhow::Result<TaskInfoReply> {
        let task = self
            .board
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| anyhow::anyhow!("resolved task id not found in board"))?;
        // 1-based, matching `task list` output and the `--task <N>` selector.
        let index = self
            .board
            .active_tasks()
            .position(|task| task.id == task_id)
            .map(|index| index + 1);
        let prs = task
            .worktrees
            .iter()
            .map(|worktree| {
                let key = worktree.to_string_lossy().to_string();
                let prs = self.pr_cache.get(&key).cloned().unwrap_or_default();
                (key, prs)
            })
            .collect();
        Ok(TaskInfoReply {
            index,
            task: task.clone(),
            prs,
            warnings: Vec::new(),
        })
    }

    /// The arena whose worktree contains `path`; the deepest one wins when worktrees nest.
    fn arena_containing_path(&self, path: &Path, cx: &App) -> Option<&Entity<Arena>> {
        self.arenas
            .iter()
            .filter_map(|arena| {
                let directory = arena.read(cx).working_directory.as_deref()?;
                let directory = std::fs::canonicalize(directory).ok()?;
                path.starts_with(&directory)
                    .then_some((directory.components().count(), arena))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, arena)| arena)
    }

    fn find_arena_entity_id_for_pids(&self, pids: &[u32], cx: &App) -> Option<EntityId> {
        for pid in pids {
            let mut matches = Vec::new();
            for arena in &self.arenas {
                for pane in arena.read(cx).center.panes() {
                    for item in pane.read(cx).items() {
                        if self.tab_process_id(item.as_ref(), cx) == Some(*pid) {
                            matches.push(arena.entity_id());
                        }
                    }
                }
            }
            if matches.len() > 1 {
                return None;
            }
            if let Some(id) = matches.pop() {
                return Some(id);
            }
        }
        None
    }

    fn branch_name_for_arena(&self, entity_id: EntityId, cx: &App) -> Option<String> {
        let working_dir = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)?
            .read(cx)
            .working_directory
            .as_ref()?;
        let git_store = self.project.read(cx).git_store().read(cx);
        git_store
            .repositories()
            .values()
            .filter_map(|repo| {
                let repo = repo.read(cx);
                if working_dir.starts_with(repo.work_directory_abs_path.as_ref()) {
                    Some((
                        repo.work_directory_abs_path.clone(),
                        repo.branch.as_ref().map(|b| b.name().to_string()),
                    ))
                } else {
                    None
                }
            })
            .max_by_key(|(path, _)| path.clone())
            .and_then(|(_, name)| name)
    }

    fn session_ids_for_arena(&self, entity_id: EntityId, cx: &App) -> Vec<String> {
        let Some(arena_entity) = self.arenas.iter().find(|a| a.entity_id() == entity_id) else {
            return Vec::new();
        };

        let arena = arena_entity.read(cx);
        let mut terminal_entity_ids = Vec::new();
        for pane in arena.center.panes() {
            for tv in pane.read(cx).items_of_type::<TerminalView>() {
                terminal_entity_ids.push(tv.entity_id());
            }
        }

        let pid_cache = self.session_state.terminal_pid_cache.borrow();
        let sid_map = self.session_state.pid_to_session_id.borrow();
        let mut session_ids = HashSet::new();
        for tv_entity_id in &terminal_entity_ids {
            if let Some(pid_info) = pid_cache.get(tv_entity_id) {
                if let Some(session_id) = sid_map.get(&pid_info.pid) {
                    session_ids.insert(session_id.clone());
                }
            }
        }
        session_ids.into_iter().collect()
    }

    fn save_pr_session_mapping(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        let working_dir = self
            .arenas
            .iter()
            .find(|a| a.entity_id() == entity_id)
            .and_then(|a| a.read(cx).working_directory.clone());
        let Some(working_dir) = working_dir else {
            return;
        };
        let project_path = std::fs::canonicalize(&working_dir)
            .unwrap_or(working_dir)
            .to_string_lossy()
            .to_string();
        self.save_pr_session_mapping_at(entity_id, &project_path, cx);
    }

    /// Same as `save_pr_session_mapping`, but takes the arena's canonical
    /// working-directory path (the pr.json project key) as an argument
    /// instead of recomputing it via `canonicalize` on the caller's thread.
    /// See `apply_pr_fetch_result_without_persist_at`.
    fn save_pr_session_mapping_at(
        &mut self,
        entity_id: EntityId,
        project_path: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(prs) = self.pr_info.get(&entity_id) else {
            return;
        };
        let pr_numbers: Vec<u32> = prs.iter().map(|pr| pr.number).collect();

        let session_ids = self.session_ids_for_arena(entity_id, cx);
        if session_ids.is_empty() {
            return;
        }

        let project_entry = self.pr_session_db.entry(project_path.to_string()).or_default();
        for sid in &session_ids {
            let data = project_entry.entry(sid.clone()).or_default();
            data.pr.extend(pr_numbers.iter().copied());
        }

        self.persist_pr_session_db(cx);
    }

    /// Writes `pr_session_db` to disk in the background, chained after any
    /// prior pending write rather than replacing it — same reasoning as
    /// `persist_pr_cache` / `persist_board`: dropping a `Task` does not
    /// cancel the background write it already scheduled, so two writes could
    /// otherwise race on `pr.json.tmp` when `task_info --update` applies
    /// several `pr_dirty` arenas in one tick.
    fn persist_pr_session_db(&mut self, cx: &mut Context<Self>) {
        let previous = self._pr_session_db_write_task.take();
        let snapshot = self.pr_session_db.clone();
        self._pr_session_db_write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            write_pr_session_db(&snapshot).log_err();
        }));
    }

    pub fn handle_task_command(&mut self, command: board::TaskCommand, cx: &mut Context<Self>) {
        let linked_pr_task = match &command {
            board::TaskCommand::AddPr { task_id, .. } => Some(*task_id),
            _ => None,
        };
        if let Err(error) = self.board.apply(command) {
            log::warn!("failed to apply task command: {error:#}");
            return;
        }
        self.board_changed(cx);
        self.fetch_issue_metadata(false, cx);
        if let Some(task_id) = linked_pr_task {
            // Polling only covers the active arena, so a PR linked to an
            // inactive arena would otherwise not show until it is switched to.
            self.refetch_prs_for_task_arenas(task_id, cx);
        }
    }

    fn live_arena_entity_ids_for_task(&self, task_id: Uuid) -> Vec<EntityId> {
        self.board_cache
            .task_worktrees
            .get(&task_id)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| match row {
                        WorktreeRow::Live { arena_index } => self
                            .arenas
                            .get(*arena_index)
                            .map(|arena| arena.entity_id()),
                        WorktreeRow::Closed { .. } | WorktreeRow::Missing { .. } => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn refetch_prs_for_task_arenas(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        for entity_id in self.live_arena_entity_ids_for_task(task_id) {
            self.fetch_pr_for_arena(entity_id, cx);
        }
    }

    /// Which task, if any, manually linked the PR at `html_url` to the arena at
    /// `arena_index`. Render-path helper: reads the board cache only, never the
    /// filesystem.
    fn manual_pr_link_for_arena(
        &self,
        arena_index: usize,
        html_url: &str,
    ) -> Option<(Uuid, board::PrRef)> {
        self.board_cache
            .task_worktrees
            .iter()
            .find_map(|(task_id, rows)| {
                let arena_is_in_task = rows.iter().any(|row| {
                    matches!(row, WorktreeRow::Live { arena_index: index } if *index == arena_index)
                });
                if !arena_is_in_task {
                    return None;
                }
                let task = self.board.tasks.iter().find(|task| task.id == *task_id)?;
                task.prs
                    .iter()
                    .find(|reference| reference.html_url() == html_url)
                    .map(|reference| (*task_id, reference.clone()))
            })
    }

    fn board_changed(&mut self, cx: &mut Context<Self>) {
        self.rebuild_board_cache(cx);
        self.persist_board(cx);
        cx.notify();
    }

    /// Chained after any prior pending write, for the same reason as
    /// `persist_pr_cache`: dropping a `Task` does not cancel the background
    /// write it already scheduled, so replacing the field would let two
    /// writes race on `board.json.tmp`.
    fn persist_board(&mut self, cx: &mut Context<Self>) {
        let previous = self._board_write_task.take();
        let snapshot = self.board.clone();
        self._board_write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            board::write_board(&snapshot).log_err();
        }));
    }

    fn rebuild_board_cache(&mut self, cx: &App) {
        let canonical_arena_paths: Vec<(PathBuf, usize)> = self
            .arenas
            .iter()
            .enumerate()
            .filter_map(|(index, arena)| {
                let directory = arena.read(cx).working_directory.clone()?;
                let canonical = std::fs::canonicalize(&directory).unwrap_or(directory);
                Some((canonical, index))
            })
            .collect();

        let mut assigned: HashSet<usize> = HashSet::new();
        let mut task_worktrees: HashMap<Uuid, Vec<WorktreeRow>> = HashMap::new();
        for task in self.board.tasks.iter().filter(|task| !task.archived) {
            let mut rows = Vec::new();
            for worktree in &task.worktrees {
                match std::fs::canonicalize(worktree) {
                    Ok(canonical) => {
                        if let Some((_, arena_index)) = canonical_arena_paths
                            .iter()
                            .find(|(path, _)| *path == canonical)
                        {
                            assigned.insert(*arena_index);
                            rows.push(WorktreeRow::Live {
                                arena_index: *arena_index,
                            });
                        } else {
                            rows.push(WorktreeRow::Closed {
                                path: worktree.clone(),
                            });
                        }
                    }
                    Err(_) => rows.push(WorktreeRow::Missing {
                        path: worktree.clone(),
                    }),
                }
            }
            task_worktrees.insert(task.id, rows);
        }

        let unassigned_arenas = (0..self.arenas.len())
            .filter(|index| !assigned.contains(index))
            .collect();
        self.board_cache = BoardCache {
            task_worktrees,
            unassigned_arenas,
        };
    }

    fn arena_canonical_path(&self, arena_index: usize, cx: &App) -> Option<PathBuf> {
        let directory = self
            .arenas
            .get(arena_index)?
            .read(cx)
            .working_directory
            .clone()?;
        Some(std::fs::canonicalize(&directory).unwrap_or(directory))
    }

    fn create_task(&mut self, worktree: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        self.board
            .apply(board::TaskCommand::New {
                id,
                title: "New Task".to_string(),
                issues: Vec::new(),
                worktrees: worktree.into_iter().collect(),
            })
            .log_err();
        self.sidebar_tab = SidebarTab::Tasks;
        self.board_changed(cx);
        self.start_rename_task(id, window, cx);
    }

    fn start_add_issue(&mut self, task_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_rename_arena(false, cx);
        self.finish_rename_task(false, cx);
        if !self.board.tasks.iter().any(|task| task.id == task_id) {
            return;
        }
        self.adding_issue_to_task = Some(task_id);
        self.issue_input_error = None;
        // The input row renders under the task header, so the task must be
        // expanded for the editor to be visible and focusable.
        if let Ok(task) = self.board.task_mut(task_id)
            && task.collapsed
        {
            task.collapsed = false;
            self.persist_board(cx);
        }
        self.issue_input_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        window.focus(&self.issue_input_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn finish_add_issue(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(task_id) = self.adding_issue_to_task else {
            return;
        };
        if !commit {
            self.adding_issue_to_task = None;
            self.issue_input_error = None;
            cx.notify();
            return;
        }
        let input = self.issue_input_editor.read(cx).text(cx);
        let trimmed = input.trim();
        if trimmed.is_empty() {
            self.adding_issue_to_task = None;
            self.issue_input_error = None;
            cx.notify();
            return;
        }
        match board::parse_issue_ref(trimmed) {
            Ok(issue) => {
                self.adding_issue_to_task = None;
                self.issue_input_error = None;
                self.board
                    .apply(board::TaskCommand::AddIssue { task_id, issue })
                    .log_err();
                self.board_changed(cx);
                self.fetch_issue_metadata(false, cx);
            }
            // Keep the editor open so the input can be corrected.
            Err(error) => {
                self.issue_input_error = Some(SharedString::from(format!("{error:#}")));
                cx.notify();
            }
        }
    }

    fn start_rename_task(&mut self, task_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_rename_arena(false, cx);
        self.finish_add_issue(false, cx);
        let Some(task) = self.board.tasks.iter().find(|task| task.id == task_id) else {
            return;
        };
        let title = task.title.clone();
        self.renaming_task = Some(task_id);
        self.task_rename_editor.update(cx, |editor, cx| {
            editor.set_text(title, window, cx);
            editor.select_all(&Default::default(), window, cx);
        });
        window.focus(&self.task_rename_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn finish_rename_task(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(task_id) = self.renaming_task.take() else {
            return;
        };
        if commit {
            let new_title = self.task_rename_editor.read(cx).text(cx);
            let trimmed = new_title.trim();
            if !trimmed.is_empty() {
                if let Ok(task) = self.board.task_mut(task_id) {
                    task.title = trimmed.to_string();
                }
                self.persist_board(cx);
            }
        }
        cx.notify();
    }

    fn move_task(&mut self, task_id: Uuid, up: bool, cx: &mut Context<Self>) {
        // Move among visible (non-archived) neighbors; archived entries keep
        // their positions in the underlying Vec.
        let visible: Vec<usize> = self
            .board
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| !task.archived)
            .map(|(index, _)| index)
            .collect();
        let Some(position) = visible
            .iter()
            .position(|&index| self.board.tasks[index].id == task_id)
        else {
            return;
        };
        let neighbor = if up {
            position.checked_sub(1)
        } else {
            (position + 1 < visible.len()).then_some(position + 1)
        };
        let Some(neighbor) = neighbor else {
            return;
        };
        self.board.tasks.swap(visible[position], visible[neighbor]);
        self.board_changed(cx);
    }

    fn archive_task(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if let Ok(task) = self.board.task_mut(task_id) {
            task.archived = true;
        }
        self.board_changed(cx);
    }

    fn assign_arena_to_task(&mut self, arena_index: usize, task_id: Uuid, cx: &mut Context<Self>) {
        let Some(path) = self.arena_canonical_path(arena_index, cx) else {
            return;
        };
        self.board
            .apply(board::TaskCommand::AddArena { task_id, path })
            .log_err();
        self.board_changed(cx);
    }

    fn remove_arena_from_tasks(&mut self, arena_index: usize, cx: &mut Context<Self>) {
        let Some(path) = self.arena_canonical_path(arena_index, cx) else {
            return;
        };
        for task in &mut self.board.tasks {
            task.worktrees.retain(|worktree| {
                std::fs::canonicalize(worktree)
                    .unwrap_or_else(|_| worktree.clone())
                    != path
            });
        }
        self.board_changed(cx);
    }

    fn remove_issue_from_task(
        &mut self,
        task_id: Uuid,
        reference: &board::IssueRef,
        cx: &mut Context<Self>,
    ) {
        if let Ok(task) = self.board.task_mut(task_id) {
            task.issues.retain(|issue| issue.reference != *reference);
        }
        self.board_changed(cx);
    }

    fn remove_pr_from_task(
        &mut self,
        task_id: Uuid,
        reference: &board::PrRef,
        cx: &mut Context<Self>,
    ) {
        if let Ok(task) = self.board.task_mut(task_id) {
            task.prs.retain(|pr| pr != reference);
        }
        self.board_changed(cx);
        self.refetch_prs_for_task_arenas(task_id, cx);
    }

    fn remove_worktree_from_task(
        &mut self,
        task_id: Uuid,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        if let Ok(task) = self.board.task_mut(task_id) {
            task.worktrees.retain(|worktree| worktree != path);
        }
        self.board_changed(cx);
    }

    /// Tasks the given arena is currently assigned to (by canonical path).
    fn tasks_containing_arena(&self, arena_index: usize, cx: &App) -> Vec<Uuid> {
        let Some(path) = self.arena_canonical_path(arena_index, cx) else {
            return Vec::new();
        };
        self.board
            .active_tasks()
            .filter(|task| {
                task.worktrees.iter().any(|worktree| {
                    std::fs::canonicalize(worktree)
                        .unwrap_or_else(|_| worktree.clone())
                        == path
                })
            })
            .map(|task| task.id)
            .collect()
    }

    fn deploy_task_context_menu(
        &mut self,
        task_id: Uuid,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            menu.entry(
                "Rename…",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.start_rename_task(task_id, window, cx);
                }),
            )
            .entry(
                "Add Issue…",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.start_add_issue(task_id, window, cx);
                }),
            )
            .entry(
                "Move Up",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.move_task(task_id, true, cx);
                }),
            )
            .entry(
                "Move Down",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.move_task(task_id, false, cx);
                }),
            )
            .separator()
            .entry(
                "Archive",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.archive_task(task_id, cx);
                }),
            )
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn deploy_issue_context_menu(
        &mut self,
        task_id: Uuid,
        reference: board::IssueRef,
        url: Option<String>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            let menu = if let Some(url) = url {
                menu.entry(
                    "Open in Browser",
                    None,
                    window.handler_for(&this, move |_this, _window, cx| {
                        cx.open_url(&url);
                    }),
                )
            } else {
                menu
            };
            menu.entry(
                "Remove from Task",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.remove_issue_from_task(task_id, &reference, cx);
                }),
            )
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn deploy_pr_context_menu(
        &mut self,
        url: String,
        manual_link: Option<(Uuid, board::PrRef)>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            let menu = menu.entry(
                "Open in Browser",
                None,
                window.handler_for(&this, move |_this, _window, cx| {
                    cx.open_url(&url);
                }),
            );
            match manual_link {
                Some((task_id, reference)) => menu.entry(
                    "Remove from Task",
                    None,
                    window.handler_for(&this, move |this, _window, cx| {
                        this.remove_pr_from_task(task_id, &reference, cx);
                    }),
                ),
                None => menu,
            }
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn deploy_worktree_context_menu(
        &mut self,
        task_id: Uuid,
        path: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.badge_menu.take();
        let context_menu = ContextMenu::build(window, cx, |menu, window, _| {
            menu.entry(
                "Remove from Task",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.remove_worktree_from_task(task_id, &path, cx);
                }),
            )
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    /// Fetch title/state/url for board issues via `gh` / `bee`. With
    /// `refresh_all`, already-cached entries are refreshed too (used at
    /// startup so cached states don't go stale forever); otherwise only
    /// unfetched issues are looked up.
    fn fetch_issue_metadata(&mut self, refresh_all: bool, cx: &mut Context<Self>) {
        let mut pending: Vec<board::IssueRef> = Vec::new();
        for task in self.board.tasks.iter().filter(|task| !task.archived) {
            for issue in &task.issues {
                let provider_available = match issue.reference {
                    board::IssueRef::GitHub { .. } => self.gh_available,
                    board::IssueRef::Backlog { .. } => self.bee_available,
                };
                if !provider_available {
                    continue;
                }
                if !refresh_all && issue.title.is_some() && issue.url.is_some() {
                    continue;
                }
                if !pending.contains(&issue.reference) {
                    pending.push(issue.reference.clone());
                }
            }
        }
        if pending.is_empty() {
            return;
        }

        self._issue_fetch_task = Some(cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    futures::future::join_all(pending.into_iter().map(|reference| async {
                        let metadata = fetch_issue_metadata_for(&reference).await;
                        (reference, metadata)
                    }))
                    .await
                })
                .await;
            this.update(cx, |this, cx| {
                for failure in this.apply_issue_metadata(results, cx) {
                    log::warn!("{failure}");
                }
            })
            .ok();
        }));
    }

    /// Registers resolved Backlog ids and merges title/state/url into every
    /// board issue matching each fetched reference, persisting on change.
    /// Returns one message per issue whose fetch errored (no board mutation
    /// happens for those), so callers can surface them (a `log::warn!` here;
    /// `task_info --update`'s `warnings`).
    fn apply_issue_metadata(
        &mut self,
        results: Vec<(board::IssueRef, anyhow::Result<IssueMetadata>)>,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let mut changed = false;
        let mut failures = Vec::new();
        for (reference, metadata) in results {
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(error) => {
                    failures.push(format!(
                        "failed to fetch issue {}: {error:#}",
                        reference.short_label()
                    ));
                    continue;
                }
            };
            if let (board::IssueRef::Backlog { issue_key }, Some(id)) =
                (&reference, metadata.backlog_id)
            {
                self.backlog_issue_ids.insert(issue_key.clone(), id);
                self.failed_backlog_issue_keys.remove(issue_key);
            }
            if self.board.merge_issue_metadata(
                &reference,
                metadata.title.as_deref(),
                metadata.state.as_deref(),
                metadata.url.as_deref(),
            ) {
                changed = true;
            }
        }
        if changed {
            self.persist_board(cx);
            cx.notify();
        }
        failures
    }

    fn handle_terminal_title_state(
        &mut self,
        shell_pid: u32,
        state: TitleState,
        cx: &mut Context<Self>,
    ) {
        let session_id = self
            .session_state
            .pid_to_session_id
            .borrow()
            .get(&shell_pid)
            .cloned();
        let changed = match session_id {
            // A hook-registered Claude session owns the terminal; its title
            // spinner only overrides the hook state to Running.
            Some(session_id) => {
                let dropped_title_entry = self.title_driven_states.remove(&shell_pid).is_some();
                let busy = state == TitleState::Busy;
                let session_changed = match self.claude_sessions.get_mut(&session_id) {
                    Some(session) if session.title_busy != busy => {
                        session.title_busy = busy;
                        true
                    }
                    _ => false,
                };
                dropped_title_entry || session_changed
            }
            None => {
                let previous = self.title_driven_states.get(&shell_pid).copied();
                match next_title_driven_state(previous, state) {
                    Some(next) => self.title_driven_states.insert(shell_pid, next) != Some(next),
                    None => self.title_driven_states.remove(&shell_pid).is_some(),
                }
            }
        };
        if changed {
            self.sync_session_derived_state();
            self.notify_all_panes(cx);
            // Here rather than only in `render`: a window in the background
            // may not render, and that is when the notification matters.
            self.sync_inbox_completions(cx);
            cx.notify();
        }
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
        if let Some(state) = self.title_driven_states.get_mut(&shell_pid) {
            if *state == ClaudeSessionState::Completed {
                *state = ClaudeSessionState::Idle;
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


    fn collect_terminal_infos_for_state(
        &self,
        arena_entity: &Entity<Arena>,
        state: ClaudeSessionState,
        pids: &HashSet<u32>,
        cx: &App,
    ) -> Vec<BadgeEntryInfo> {
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
                let (primary, secondary) = self
                    .claude_sessions
                    .values()
                    .find(|s| {
                        s.effective_state() == state
                            && s.ancestor_pids.contains(&pid)
                    })
                    .map(|s| (s.user_prompt.clone(), s.status_message.clone()))
                    // Title-driven agents (Codex) have no hook data; their
                    // title carries the thread name and project instead.
                    .unwrap_or_else(|| {
                        let title = tv.read(cx).terminal().read(cx).breadcrumb_text.clone();
                        (title, String::new())
                    });
                infos.push(BadgeEntryInfo {
                    pane: pane.clone(),
                    item: Box::new(tv),
                    primary,
                    secondary,
                });
            }
        }
        let codex_state = match state {
            ClaudeSessionState::Completed => codex_acp_view::CodexAcpState::Completed,
            ClaudeSessionState::WaitingPermission => codex_acp_view::CodexAcpState::Permission,
            ClaudeSessionState::Running => codex_acp_view::CodexAcpState::Running,
            ClaudeSessionState::Idle => codex_acp_view::CodexAcpState::Idle,
        };
        infos.extend(codex_badge_entries(arena, codex_state, cx));
        infos
    }

    fn collect_ready_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<BadgeEntryInfo> {
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
    ) -> Vec<BadgeEntryInfo> {
        let pids = self.session_state.permission_shell_pids.borrow().clone();
        self.collect_terminal_infos_for_state(
            arena_entity,
            ClaudeSessionState::WaitingPermission,
            &pids,
            cx,
        )
    }

    fn collect_running_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<BadgeEntryInfo> {
        let running = self.session_state.running_shell_pids.borrow();
        let permission = self.session_state.permission_shell_pids.borrow();
        let pids: HashSet<u32> = running.difference(&permission).copied().collect();
        self.collect_terminal_infos_for_state(
            arena_entity,
            ClaudeSessionState::Running,
            &pids,
            cx,
        )
    }

    fn collect_busy_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<BadgeEntryInfo> {
        struct Candidate {
            live_pid: u32,
            pane: Entity<Pane>,
            terminal_view: Entity<TerminalView>,
        }
        let claude_pids = self.session_state.pid_to_session_id.borrow();
        let arena = arena_entity.read(cx);
        let mut candidates: Vec<Candidate> = Vec::new();
        for pane in arena.center.panes() {
            for tv in pane.read(cx).items_of_type::<TerminalView>() {
                let terminal = tv.read(cx).terminal().read(cx);
                let Some(getter) = terminal.pid_getter() else {
                    continue;
                };
                let shell_pid = getter.fallback_pid().as_u32();
                // A title-driven entry settles at Idle instead of being removed,
                // including for non-agent programs whose title once spun.
                if claude_pids.contains_key(&shell_pid)
                    || self
                        .title_driven_states
                        .get(&shell_pid)
                        .is_some_and(|state| *state != ClaudeSessionState::Idle)
                {
                    continue;
                }
                if let Some(live_pid) = terminal.pid() {
                    candidates.push(Candidate {
                        live_pid: live_pid.as_u32(),
                        pane: pane.clone(),
                        terminal_view: tv.clone(),
                    });
                }
            }
        }
        drop(claude_pids);
        if candidates.is_empty() {
            return Vec::new();
        }

        let pids: Vec<sysinfo::Pid> = candidates
            .iter()
            .map(|c| sysinfo::Pid::from_u32(c.live_pid))
            .collect();
        let mut system = self.sysinfo_system.borrow_mut();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );

        let mut infos = Vec::new();
        for c in candidates {
            let Some(process) = system.process(sysinfo::Pid::from_u32(c.live_pid)) else {
                continue;
            };
            let Some(name) = argv0_name(process) else {
                continue;
            };
            if is_agent_process_name(name) || is_shell_process_name(name) {
                continue;
            }
            infos.push(BadgeEntryInfo {
                pane: c.pane,
                item: Box::new(c.terminal_view),
                primary: name.to_owned(),
                secondary: format!("PID {}", c.live_pid),
            });
        }
        infos
    }

    fn deploy_badge_menu(
        &mut self,
        arena_index: usize,
        terminal_infos: Vec<BadgeEntryInfo>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.context_menu.take();
        let context_menu = ContextMenu::build(window, cx, |mut menu, window, _cx| {
            for info in terminal_infos {
                let pane = info.pane.clone();
                let item = info.item.clone();
                let primary_label = if info.primary.is_empty() {
                    "(no prompt)".to_string()
                } else {
                    truncate_for_menu(&info.primary)
                };
                let secondary_label = truncate_for_menu(&info.secondary);

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
                                    .child(primary_label.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .truncate()
                                    .child(secondary_label.clone()),
                            )
                            .into_any_element()
                    },
                    window.handler_for(&this, move |this, window, cx| {
                        this.navigate_to_item(arena_index, &pane, item.as_ref(), window, cx);
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

    fn navigate_to_item(
        &mut self,
        arena_index: usize,
        target_pane: &Entity<Pane>,
        item: &dyn workspace::ItemHandle,
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
            .index_for_item(item);
        if let Some(index) = item_index {
            target_pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx);
            });
        }

        if let Some(terminal_view) = item.act_as::<TerminalView>(cx) {
            let shell_pid = terminal_view.read(cx).terminal().read(cx).pid_getter()
                .map(|getter| getter.fallback_pid().as_u32());
            if let Some(pid) = shell_pid { self.clear_session_for_shell_pid(pid, cx); }
        }
        if let Some(view) = item.act_as::<codex_acp_view::CodexAcpView>(cx) {
            view.update(cx, |view, cx| view.acknowledge(cx));
        }
        item.item_focus_handle(cx).focus(window, cx);
    }
}

fn codex_badge_entries(
    arena: &Arena,
    state: codex_acp_view::CodexAcpState,
    cx: &App,
) -> Vec<BadgeEntryInfo> {
    let mut entries = Vec::new();
    for pane in arena.center.panes() {
        for view in pane
            .read(cx)
            .items_of_type::<codex_acp_view::CodexAcpView>()
        {
            if view.read(cx).state() == state {
                entries.push(BadgeEntryInfo {
                    pane: pane.clone(),
                    primary: workspace::Item::tab_content_text(view.read(cx), 0, cx).to_string(),
                    secondary: "Codex (ACP)".to_owned(),
                    item: Box::new(view),
                });
            }
        }
    }
    entries
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

const BADGE_TOOLTIP_MAX_ENTRIES: usize = 10;

fn render_badge_tooltip(entries: &[BadgeEntryInfo], cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let shown = entries.len().min(BADGE_TOOLTIP_MAX_ENTRIES);
    let hidden = entries.len() - shown;
    let mut column = v_flex().gap_1();
    for entry in &entries[..shown] {
        let primary = if entry.primary.is_empty() {
            "(no prompt)".to_string()
        } else {
            truncate_for_menu(&entry.primary)
        };
        let secondary = truncate_for_menu(&entry.secondary);
        column = column.child(
            v_flex()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .child(primary),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(secondary),
                ),
        );
    }
    if hidden > 0 {
        column = column.child(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(format!("+{hidden} more")),
        );
    }
    column.into_any_element()
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
    if resets_at - now < 24 * 3600 {
        return format_resets_in(resets_at);
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
    let status = cx.theme().status();
    let fg_color = if used_pct >= 90.0 {
        status.error
    } else if used_pct >= 70.0 {
        status.warning
    } else {
        status.info
    };
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
                .bg_color(colors.border)
                .fg_color(fg_color),
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
    pr: &PrInfo,
    ci: Option<&CiInfo>,
    branch: Option<&str>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let status_colors = cx.theme().status();

    let mut content = v_flex().gap_1().max_w_96();

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

    let flow_label = match branch {
        Some(branch) if !pr.base_ref.is_empty() => Some(format!("{branch} → {}", pr.base_ref)),
        Some(branch) => Some(branch.to_string()),
        None if !pr.base_ref.is_empty() => Some(format!("→ {}", pr.base_ref)),
        None => None,
    };

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
                .when_some(flow_label, |d, label| {
                    d.child(
                        div()
                            .min_w_0()
                            .flex_shrink_1()
                            .truncate()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child(label),
                    )
                }),
        );

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

pub fn remote_url_to_browser_url(url: &str) -> Option<String> {
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

/// Refresh with `ProcessRefreshKind::with_cmd(UpdateKind::Always)` first.
/// `process.name()` is captured once per PID and survives `exec`, so a shell
/// forked for `sbt` would keep reporting "zsh" after it becomes java; argv[0]
/// is re-read on every refresh.
fn argv0_name(process: &sysinfo::Process) -> Option<&str> {
    process
        .cmd()
        .first()
        .and_then(|arg0| Path::new(arg0).file_name())
        .and_then(|name| name.to_str())
}

fn arena_agent_icon(
    is_codex_acp: bool,
    has_claude_session: bool,
    process_name: Option<&str>,
) -> Option<IconName> {
    if is_codex_acp {
        Some(IconName::AiOpenAi)
    } else if has_claude_session || process_name == Some("claude") {
        Some(IconName::AiClaude)
    } else if process_name == Some("codex") {
        Some(IconName::AiOpenAi)
    } else {
        None
    }
}

fn is_agent_process_name(name: &str) -> bool {
    matches!(name, "claude" | "codex")
}

fn is_shell_process_name(name: &str) -> bool {
    // Login shells are sometimes reported with a leading '-' (e.g. "-zsh").
    let stripped = name.strip_prefix('-').unwrap_or(name);
    matches!(
        stripped,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "pwsh" | "xonsh" | "elvish"
    )
}

// Not the theme's `terminal_ansi_magenta`: several themes map it to a blue
// indistinguishable from the Completed lane's accent.
fn inbox_decision_color() -> Hsla {
    rgb(0xb48ef2).into()
}

/// Fixed for the same reason as `inbox_decision_color`, and kept apart from
/// the In progress lane's `status.success` green.
fn inbox_todo_color() -> Hsla {
    rgb(0x55c4b8).into()
}

/// The one field of a tool call that says what it will touch, falling back to
/// the whole input for tools without one.
fn permission_summary(tool_input: &serde_json::Value) -> String {
    ["command", "file_path", "notebook_path", "url", "path", "pattern"]
        .iter()
        .find_map(|key| tool_input[key].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| tool_input.to_string())
}

fn render_inbox_permission(
    permission: InboxPermissionRow,
    colors: &theme::ThemeColors,
    cx: &mut Context<AgentiumApp>,
) -> impl IntoElement {
    let permission_id = permission.id;
    let button = |label: &'static str, answer: InboxPermissionAnswer, cx: &mut Context<AgentiumApp>| {
        Button::new(
            SharedString::from(format!("inbox-permission-{permission_id}-{label}")),
            label,
        )
        .style(ButtonStyle::Subtle)
        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            this.answer_inbox_permission(permission_id, answer, cx);
        }))
    };
    v_flex()
        .gap_1()
        .pt_1()
        .child(
            h_flex()
                .gap_1()
                .text_xs()
                .child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border)
                        .child(permission.tool_name),
                )
                .child(
                    div()
                        .font_buffer(cx)
                        .truncate()
                        .child(truncate_for_menu(&permission.summary)),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .justify_end()
                .child(button("Allow", InboxPermissionAnswer::Allow, cx))
                .when(permission.can_always_allow, |d| {
                    d.child(button("Always allow", InboxPermissionAnswer::AlwaysAllow, cx))
                })
                .child(button("Deny", InboxPermissionAnswer::Deny, cx)),
        )
}

const INBOX_QUESTIONS_FILE: &str = "inbox.json";
const INBOX_COMPLETIONS_FILE: &str = "inbox_completions.json";
const INBOX_TODOS_FILE: &str = "todo.json";
const INBOX_CONFIRMED_COMPLETION_TTL: chrono::TimeDelta = chrono::TimeDelta::hours(1);

fn load_inbox_file<T: serde::de::DeserializeOwned>(file_name: &str) -> Vec<T> {
    let path = paths::data_dir().join(file_name);
    match std::fs::read(&path) {
        Ok(content) => serde_json::from_slice(&content)
            .map_err(|error| anyhow::anyhow!("Failed to parse {}: {error}", path.display()))
            .log_err()
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_inbox_file(file_name: &str, content: &[u8]) -> anyhow::Result<()> {
    write_file_atomically(&paths::data_dir().join(file_name), content)
}

fn write_file_atomically(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary_path = path.with_extension("json.tmp");
    std::fs::write(&temporary_path, content)?;
    std::fs::rename(&temporary_path, &path)?;
    Ok(())
}

fn load_pr_session_db() -> HashMap<String, HashMap<String, PrSessionData>> {
    let db_path = paths::data_dir().join("pr.json");
    match std::fs::read_to_string(&db_path) {
        Ok(content) => serde_json::from_str(&content)
            .map_err(|error| anyhow::anyhow!("Failed to parse {}: {error}", db_path.display()))
            .log_err()
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn write_pr_session_db(
    db: &HashMap<String, HashMap<String, PrSessionData>>,
) -> anyhow::Result<()> {
    let db_path = paths::data_dir().join("pr.json");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = db_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(db)?)?;
    std::fs::rename(&tmp_path, &db_path)?;
    Ok(())
}

/// Snapshot of the last-fetched PRs per worktree, keyed by canonical path, so
/// `agentium task info` can show arena PRs without a live app instance.
/// Written only by the app (single-writer, same rule as `board.json`).
pub fn load_pr_cache() -> HashMap<String, Vec<PrInfo>> {
    let path = paths::data_dir().join("pr_cache.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .map_err(|error| anyhow::anyhow!("Failed to parse {}: {error}", path.display()))
            .log_err()
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn write_pr_cache(cache: &HashMap<String, Vec<PrInfo>>) -> anyhow::Result<()> {
    let path = paths::data_dir().join("pr_cache.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(cache)?)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// The ids of every task with a `Live { arena_index }` row in
/// `task_worktrees`. Pure over the board cache so it can be unit tested
/// without an `AgentiumApp`.
fn tasks_sharing_arena(
    task_worktrees: &HashMap<Uuid, Vec<WorktreeRow>>,
    arena_index: usize,
) -> Vec<Uuid> {
    task_worktrees
        .iter()
        .filter(|(_, rows)| {
            rows.iter()
                .any(|row| matches!(row, WorktreeRow::Live { arena_index: index } if *index == arena_index))
        })
        .map(|(task_id, _)| *task_id)
        .collect()
}

/// Deduplicated Backlog issue keys and manually linked PR references across
/// `tasks`. Pure so it can be unit tested without an `AgentiumApp`; the order
/// of first occurrence is preserved, matching `contains`-guarded pushes.
fn task_fetch_inputs(
    tasks: &[&board::BoardTask],
    bee_available: bool,
) -> (Vec<String>, Vec<board::PrRef>) {
    let mut task_issue_keys: Vec<String> = Vec::new();
    let mut task_pr_refs: Vec<board::PrRef> = Vec::new();
    for task in tasks {
        if bee_available {
            for issue in &task.issues {
                if let board::IssueRef::Backlog { issue_key } = &issue.reference {
                    if !task_issue_keys.contains(issue_key) {
                        task_issue_keys.push(issue_key.clone());
                    }
                }
            }
        }
        for reference in &task.prs {
            if !task_pr_refs.contains(reference) {
                task_pr_refs.push(reference.clone());
            }
        }
    }
    (task_issue_keys, task_pr_refs)
}

/// Per-fetch inputs snapshotted on the main thread so the background fetch
/// never touches app state.
pub struct PrFetchContext {
    pub gh_available: bool,
    pub bee_available: bool,
    // Backlog issue keys registered on tasks containing this arena.
    pub task_issue_keys: Vec<String>,
    // PRs manually linked (`agentium task add-pr`) on tasks containing this
    // arena; only those of the arena's own repository are fetched.
    pub task_pr_refs: Vec<board::PrRef>,
    pub known_issue_ids: HashMap<String, u64>,
    pub failed_issue_keys: HashSet<String>,
}

#[derive(Default)]
pub struct PrFetchOutcome {
    pub prs: Vec<PrInfo>,
    // Issue key -> id pairs resolved during this fetch, for the app-level cache.
    pub resolved_backlog_ids: Vec<(String, u64)>,
    // Keys Backlog reported as nonexistent, for the negative cache.
    pub failed_backlog_keys: Vec<String>,
    // Non-empty means part of this fetch failed to reach its provider, so
    // callers (task_info) must not treat an empty `prs` as "no PRs" and must
    // not overwrite a cached value with it. Polling's `apply_pr_fetch_result`
    // deliberately does not consult this (see §6 in the plan doc).
    pub failures: Vec<String>,
}

pub struct BacklogRemote {
    pub web_host: String,
    pub project: String,
    pub repo: String,
}

/// Whether a manually linked PR belongs to the repository at `remote_url`
/// (a git origin URL). Shared by the arena PR fetch, which uses it to pick
/// the task's linked PRs that concern the arena, and by the CLI, which refuses
/// to link a PR from a repository none of the task's worktrees point at.
pub fn pr_ref_matches_remote(reference: &board::PrRef, remote_url: &str) -> bool {
    match reference {
        board::PrRef::GitHub { repo, .. } => github_owner_repo_from_remote(remote_url)
            .is_some_and(|owner_repo| owner_repo.eq_ignore_ascii_case(repo)),
        board::PrRef::Backlog {
            web_host,
            project,
            repo,
            ..
        } => parse_backlog_remote(remote_url).is_some_and(|remote| {
            remote.web_host == *web_host && remote.project == *project && remote.repo == *repo
        }),
    }
}

/// `owner/repo` of a github.com origin in `https://`, `git@host:` or
/// `ssh://git@host/` form; `None` for any other host or path shape.
pub fn github_owner_repo_from_remote(remote_url: &str) -> Option<String> {
    let url = remote_url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let host_and_path = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else {
        return None;
    };
    let (host, path) = host_and_path.split_once('/')?;
    // Drop a `user@` prefix and a `:port` suffix from the authority.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

pub fn parse_backlog_remote(url: &str) -> Option<BacklogRemote> {
    let url = url.trim();
    let (host, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/')?;
        (host, path.strip_prefix("git/")?)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
        (host, path)
    } else if let Some((user_host, path)) = url.split_once(':') {
        // scp-like syntax: nulab@nulab.git.backlog.jp:/PROJ/repo.git
        let host = user_host
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(user_host);
        (host, path.trim_start_matches('/'))
    } else {
        return None;
    };

    if !host.ends_with(".backlog.jp") && !host.ends_with(".backlog.com") {
        return None;
    }
    // SSH remotes use a dedicated git host (nulab.git.backlog.jp); web URLs
    // live on the space host (nulab.backlog.jp).
    let web_host = host.replace(".git.backlog.", ".backlog.");

    let mut segments = path.split('/');
    let project = segments.next()?;
    let repo = segments.next()?;
    if project.is_empty() || repo.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some(BacklogRemote {
        web_host,
        project: project.to_string(),
        repo: repo.to_string(),
    })
}

fn backlog_issue_keys_in_branch(branch: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for segment in branch.split('/') {
        if board::is_backlog_issue_key(segment) && !keys.iter().any(|key| key == segment) {
            keys.push(segment.to_string());
        }
    }
    keys
}

pub async fn git_remote_url(working_dir: &std::path::Path) -> Option<String> {
    let get_url = |remote: String| async move {
        let output = smol::process::Command::new("git")
            .current_dir(working_dir)
            .args(&["remote", "get-url", &remote])
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!url.is_empty()).then_some(url)
    };

    if let Some(url) = get_url("origin".to_string()).await {
        return Some(url);
    }
    let output = smol::process::Command::new("git")
        .current_dir(working_dir)
        .args(&["remote"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let first = stdout.lines().next()?.trim().to_string();
    if first.is_empty() {
        return None;
    }
    get_url(first).await
}

#[derive(serde::Deserialize)]
struct GhPr {
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
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
}

const JSON_FIELDS: &str =
    "number,title,state,mergeable,isDraft,url,reviewDecision,headRefOid,baseRefName";

fn pr_info_from_gh(pr: GhPr, reviews: Vec<ReviewEntry>) -> PrInfo {
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

    let review_count = reviews
        .iter()
        .filter(|entry| entry.commit_oid.as_ref() == pr.head_ref_oid)
        .count();

    PrInfo {
        number: pr.number,
        title: pr.title.into(),
        status,
        html_url: pr.url.into(),
        review_decision,
        review_count,
        head_sha: pr.head_ref_oid.into(),
        reviews,
        base_ref: pr.base_ref_name.into(),
        provider: PrProvider::GitHub,
    }
}

pub async fn fetch_pr_list(
    working_dir: &std::path::Path,
    context: PrFetchContext,
) -> anyhow::Result<PrFetchOutcome> {
    let branch_output = smol::process::Command::new("git")
        .current_dir(working_dir)
        .args(&["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await?;
    let branch = if branch_output.status.success() {
        let name = String::from_utf8_lossy(&branch_output.stdout)
            .trim()
            .to_string();
        // "HEAD" means detached; fall back to `gh pr view` resolution below.
        (name != "HEAD" && !name.is_empty()).then_some(name)
    } else {
        None
    };

    let remote_url = git_remote_url(working_dir).await;
    let backlog_remote = remote_url.as_deref().and_then(parse_backlog_remote);
    let provider_available = if backlog_remote.is_some() {
        context.bee_available
    } else {
        context.gh_available
    };
    if !provider_available {
        let provider = if backlog_remote.is_some() { "bee" } else { "gh" };
        return Ok(PrFetchOutcome {
            failures: vec![format!("{provider} not available")],
            ..PrFetchOutcome::default()
        });
    }

    // PRs linked with `agentium task add-pr` that belong to this repository.
    // Fetched alongside the branch-driven discovery rather than after it: the
    // PR polling loop disables itself when one pass exceeds 5 seconds.
    let manual_refs: Vec<board::PrRef> = match remote_url.as_deref() {
        Some(remote_url) => context
            .task_pr_refs
            .iter()
            .filter(|reference| pr_ref_matches_remote(reference, remote_url))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let manual_fetch = futures::future::join_all(manual_refs.iter().map(|reference| async move {
        match fetch_pr_by_ref(reference).await {
            Ok(pr) => Ok(pr),
            // Isolated per PR: an error here must not become an `Err` for the
            // whole fetch, which would wipe the arena's discovered PRs.
            Err(error) => {
                let message = format!(
                    "failed to fetch linked PR {}: {error:#}",
                    reference.short_label()
                );
                log::warn!("{message}");
                Err(message)
            }
        }
    }));
    let discovered_fetch = async {
        match backlog_remote {
            Some(remote) => fetch_backlog_pr_list(branch, remote, context).await,
            None => fetch_github_pr_list(working_dir, branch).await,
        }
    };
    let (discovered, manual_prs) = futures::join!(discovered_fetch, manual_fetch);

    let mut outcome = discovered?;
    for result in manual_prs {
        match result {
            Ok(pr) => {
                if !outcome.prs.iter().any(|existing| existing.number == pr.number) {
                    outcome.prs.push(pr);
                }
            }
            Err(message) => outcome.failures.push(message),
        }
    }
    outcome.prs.sort_by_key(|pr| pr.number);
    Ok(outcome)
}

async fn fetch_github_pr_list(
    working_dir: &std::path::Path,
    branch: Option<String>,
) -> anyhow::Result<PrFetchOutcome> {
    let mut failures: Vec<String> = Vec::new();
    let mut gh_prs: Vec<GhPr> = Vec::new();
    if let Some(branch) = branch {
        let output = smol::process::Command::new("gh")
            .current_dir(working_dir)
            .args(&[
                "pr",
                "list",
                "--head",
                branch.as_str(),
                "--state",
                "all",
                "--limit",
                "10",
                "--json",
                JSON_FIELDS,
            ])
            .output()
            .await?;
        if output.status.success() {
            gh_prs = serde_json::from_slice(&output.stdout)?;
        } else {
            let message = format!(
                "gh pr list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            log::warn!("{message}");
            failures.push(message);
        }
    }

    if gh_prs.is_empty() {
        // Detached HEAD, list failure, or no same-repo PR for this branch (e.g. a
        // cross-fork PR that only `gh pr view`'s own branch resolution can find).
        let output = smol::process::Command::new("gh")
            .current_dir(working_dir)
            .args(&["pr", "view", "--json", JSON_FIELDS])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("no pull requests found") {
                let message = format!("gh pr view failed: {stderr}");
                log::warn!("{message}");
                failures.push(message);
            }
            return Ok(PrFetchOutcome {
                failures,
                ..PrFetchOutcome::default()
            });
        }
        gh_prs = vec![serde_json::from_slice(&output.stdout)?];
    }

    gh_prs.sort_by_key(|pr| pr.number);

    // Fetch individual reviews via REST API to get avatar URLs, concurrently for
    // all PRs: the PR polling loop permanently disables itself when one pass
    // exceeds 5 seconds, so the cost must not grow linearly with the PR count.
    let reviews_per_pr = futures::future::join_all(
        gh_prs
            .iter()
            .map(|pr| fetch_reviews(Some(working_dir), &pr.url, &pr.head_ref_oid)),
    )
    .await;

    let prs = gh_prs
        .into_iter()
        .zip(reviews_per_pr)
        .map(|(pr, reviews)| {
            let reviews = match reviews {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!("failed to fetch reviews: {err}");
                    Vec::new()
                }
            };
            pr_info_from_gh(pr, reviews)
        })
        .collect();
    Ok(PrFetchOutcome {
        prs,
        failures,
        ..PrFetchOutcome::default()
    })
}

enum BacklogIssueIdError {
    NoSuchIssue(String),
    Other(anyhow::Error),
}

async fn resolve_backlog_issue_id(
    issue_key: &str,
    space_host: &str,
) -> Result<u64, BacklogIssueIdError> {
    // `-s` pins the space to the remote's host; bee's default space could
    // otherwise resolve a same-named key from a different space.
    let output = smol::process::Command::new("bee")
        .args(&["issue", "view", issue_key, "--json", "id", "-s", space_host])
        .output()
        .await
        .map_err(|error| BacklogIssueIdError::Other(error.into()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // Nonexistent issues fail with `{"errors":[{"message":"No such issue. ..."}]}`
        // on stderr; only those go to the negative cache so that transient
        // failures (network, auth) retry on the next poll.
        if stderr.contains("No such issue") || stderr.contains("No such project") {
            return Err(BacklogIssueIdError::NoSuchIssue(stderr));
        }
        return Err(BacklogIssueIdError::Other(anyhow::anyhow!(
            "bee issue view failed: {stderr}"
        )));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| BacklogIssueIdError::Other(error.into()))?;
    json["id"]
        .as_u64()
        .ok_or_else(|| BacklogIssueIdError::Other(anyhow::anyhow!("bee issue view returned no id")))
}

#[derive(serde::Deserialize)]
struct BeePrStatus {
    name: String,
}
#[derive(serde::Deserialize)]
struct BeePr {
    number: u32,
    summary: String,
    base: String,
    branch: String,
    status: BeePrStatus,
}

fn pr_info_from_bee(pr: BeePr, remote: &BacklogRemote) -> PrInfo {
    let status = match pr.status.name.as_str() {
        "Merged" => PrStatus::Merged,
        "Closed" => PrStatus::Closed,
        "Open" => PrStatus::Open,
        other => {
            log::warn!("unknown backlog PR status {other:?}, treating as open");
            PrStatus::Open
        }
    };
    PrInfo {
        number: pr.number,
        title: pr.summary.into(),
        status,
        html_url: format!(
            "https://{}/git/{}/{}/pullRequests/{}",
            remote.web_host, remote.project, remote.repo, pr.number
        )
        .into(),
        review_decision: None,
        review_count: 0,
        head_sha: "".into(),
        reviews: Vec::new(),
        base_ref: pr.base.into(),
        provider: PrProvider::Backlog,
    }
}

async fn fetch_backlog_pr_list(
    branch: Option<String>,
    remote: BacklogRemote,
    context: PrFetchContext,
) -> anyhow::Result<PrFetchOutcome> {
    // Detached HEAD: Backlog has no equivalent of `gh pr view`'s branch resolution.
    let Some(branch) = branch else {
        return Ok(PrFetchOutcome::default());
    };

    let mut issue_keys = backlog_issue_keys_in_branch(&branch);
    for key in &context.task_issue_keys {
        if !issue_keys.contains(key) {
            issue_keys.push(key.clone());
        }
    }

    let mut issue_ids: Vec<u64> = Vec::new();
    let mut unknown_keys: Vec<String> = Vec::new();
    for key in issue_keys {
        if let Some(id) = context.known_issue_ids.get(&key) {
            issue_ids.push(*id);
        } else if !context.failed_issue_keys.contains(&key) {
            unknown_keys.push(key);
        }
    }

    let resolutions = futures::future::join_all(
        unknown_keys
            .iter()
            .map(|key| resolve_backlog_issue_id(key, &remote.web_host)),
    )
    .await;
    let mut outcome = PrFetchOutcome::default();
    for (key, resolution) in unknown_keys.into_iter().zip(resolutions) {
        match resolution {
            Ok(id) => {
                issue_ids.push(id);
                outcome.resolved_backlog_ids.push((key, id));
            }
            Err(BacklogIssueIdError::NoSuchIssue(message)) => {
                log::warn!("backlog issue {key} does not exist, skipping: {message}");
                outcome.failed_backlog_keys.push(key);
            }
            Err(BacklogIssueIdError::Other(error)) => {
                let message = format!("failed to resolve backlog issue id for {key}: {error:#}");
                log::warn!("{message}");
                outcome.failures.push(message);
            }
        }
    }

    if issue_ids.is_empty() {
        return Ok(outcome);
    }

    // `bee pr list` returns PRs oldest-first and Backlog caps a page at 100
    // entries, so an unfiltered listing would miss recent PRs in large repos.
    // Filtering by issue id server-side keeps the result set complete; only an
    // issue with more than 100 linked PRs could lose its newest ones.
    let mut args: Vec<String> = vec![
        "pr".into(),
        "list".into(),
        "-p".into(),
        remote.project.clone(),
        "-R".into(),
        remote.repo.clone(),
        "-s".into(),
        remote.web_host.clone(),
        "--json".into(),
        "number,summary,base,branch,status".into(),
        "-L".into(),
        "100".into(),
    ];
    for id in &issue_ids {
        args.push("--issue".into());
        args.push(id.to_string());
    }

    let output = smol::process::Command::new("bee").args(&args).output().await?;
    if !output.status.success() {
        let message = format!(
            "bee pr list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        log::warn!("{message}");
        outcome.failures.push(message);
        return Ok(outcome);
    }

    let bee_prs: Vec<BeePr> = serde_json::from_slice(&output.stdout)?;
    outcome.prs = bee_prs
        .into_iter()
        .filter(|pr| pr.branch == branch)
        .map(|pr| pr_info_from_bee(pr, &remote))
        .collect();
    outcome.prs.sort_by_key(|pr| pr.number);
    Ok(outcome)
}

/// Fetch per-reviewer data via the REST API (`gh api repos/{owner}/{repo}/pulls/{number}/reviews`).
/// Deduplicates by author, keeping only the latest review per user.
pub async fn fetch_reviews(
    working_dir: Option<&std::path::Path>,
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

    let mut command = smol::process::Command::new("gh");
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    let output = command.args(&["api", &api_path]).output().await?;

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

pub async fn fetch_ci_status(
    working_dir: Option<&std::path::Path>,
    pr_number: u32,
    repo: Option<&str>,
) -> anyhow::Result<Option<CiInfo>> {
    let mut command = smol::process::Command::new("gh");
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    let mut args: Vec<String> = vec![
        "pr".into(),
        "checks".into(),
        pr_number.to_string(),
        "--json".into(),
        "name,bucket".into(),
    ];
    if let Some(repo) = repo {
        args.push("--repo".into());
        args.push(repo.to_string());
    }
    let output = command.args(&args).output().await?;

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

/// Fetch a single PR by reference, independent of any branch/worktree. Used
/// by manually-linked PRs (`agentium task add-pr`), where there is no local
/// checkout to derive the branch from.
///
/// Unlike `fetch_pr_list`, subprocess failures are propagated with `bail!`
/// rather than logged and swallowed: this is also called from the CLI, where
/// `log::warn!` output is otherwise invisible, so an expired `gh`/`bee` login
/// must not silently look like "no PR found".
pub async fn fetch_pr_by_ref(reference: &board::PrRef) -> anyhow::Result<PrInfo> {
    match reference {
        board::PrRef::GitHub { repo, number } => {
            let output = smol::process::Command::new("gh")
                .args(&[
                    "pr",
                    "view",
                    &number.to_string(),
                    "--repo",
                    repo,
                    "--json",
                    JSON_FIELDS,
                ])
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!(
                    "gh pr view failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let pr: GhPr = serde_json::from_slice(&output.stdout)?;
            let reviews = match fetch_reviews(None, &pr.url, &pr.head_ref_oid).await {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!("failed to fetch reviews: {err}");
                    Vec::new()
                }
            };
            Ok(pr_info_from_gh(pr, reviews))
        }
        board::PrRef::Backlog {
            web_host,
            project,
            repo,
            number,
        } => {
            let output = smol::process::Command::new("bee")
                .args(&[
                    "pr",
                    "view",
                    &number.to_string(),
                    "-p",
                    project,
                    "-R",
                    repo,
                    "-s",
                    web_host,
                    "--json",
                    "number,summary,base,branch,status",
                ])
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!(
                    "bee pr view failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let pr: BeePr = serde_json::from_slice(&output.stdout)?;
            let remote = BacklogRemote {
                web_host: web_host.clone(),
                project: project.clone(),
                repo: repo.clone(),
            };
            Ok(pr_info_from_bee(pr, &remote))
        }
    }
}

async fn version_check_succeeds(program: &str) -> bool {
    smol::process::Command::new(program)
        .args(&["--version"])
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub struct IssueMetadata {
    pub title: Option<String>,
    pub state: Option<String>,
    pub url: Option<String>,
    // Numeric Backlog issue id, needed by `bee pr list --issue`. None for GitHub.
    pub backlog_id: Option<u64>,
}

pub async fn fetch_issue_metadata_for(reference: &board::IssueRef) -> anyhow::Result<IssueMetadata> {
    match reference {
        board::IssueRef::GitHub { repo, number } => fetch_github_issue(repo, *number).await,
        board::IssueRef::Backlog { issue_key } => fetch_backlog_issue(issue_key).await,
    }
}

async fn fetch_github_issue(repo: &str, number: u64) -> anyhow::Result<IssueMetadata> {
    let output = smol::process::Command::new("gh")
        .args(&[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            "title,state,url",
        ])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "gh issue view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(serde::Deserialize)]
    struct GhIssue {
        title: String,
        state: String,
        url: String,
    }
    let issue: GhIssue = serde_json::from_slice(&output.stdout)?;
    Ok(IssueMetadata {
        title: Some(issue.title),
        state: Some(issue.state.to_lowercase()),
        url: Some(issue.url),
        backlog_id: None,
    })
}

async fn fetch_backlog_issue(issue_key: &str) -> anyhow::Result<IssueMetadata> {
    let output = smol::process::Command::new("bee")
        .args(&["issue", "view", issue_key, "--json"])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "bee issue view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // bee's --json field names are undocumented; extract defensively.
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let title = json["summary"].as_str().map(String::from);
    let state = json["status"]["name"].as_str().map(String::from);
    let backlog_id = json["id"].as_u64();

    // `-n` prints the issue URL instead of opening a browser.
    let url_output = smol::process::Command::new("bee")
        .args(&["issue", "view", issue_key, "-n"])
        .output()
        .await?;
    let url = if url_output.status.success() {
        let url = String::from_utf8_lossy(&url_output.stdout)
            .trim()
            .to_string();
        url.starts_with("http").then_some(url)
    } else {
        None
    };

    Ok(IssueMetadata {
        title,
        state,
        url,
        backlog_id,
    })
}

/// The small green "b" square used to mark a Backlog issue or PR row where
/// the provider isn't otherwise obvious (task rows can mix GitHub and
/// Backlog; arena rows don't need it since the provider is implied by the
/// arena's remote).
fn backlog_badge() -> AnyElement {
    h_flex()
        .flex_shrink_0()
        .w(px(14.0))
        .h(px(14.0))
        .rounded_sm()
        // Backlog brand green
        .bg(hsla(160.0 / 360.0, 0.55, 0.42, 1.0))
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(gpui::white())
        .child("b")
        .into_any_element()
}

impl Focusable for AgentiumApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AgentiumApp {
    fn render_arena_agents(
        &self,
        arena_index: usize,
        arena: &Entity<Arena>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors().clone();
        let status_colors = cx.theme().status().clone();
        let mut rows = Vec::new();
        for pane in arena.read(cx).center.panes() {
            for item in pane.read(cx).items() {
                let asker = self.inbox_asker(item.as_ref(), cx);
                let is_codex = item.act_as::<codex_acp_view::CodexAcpView>(cx).is_some();
                let process_name = item
                    .act_as::<TerminalView>(cx)
                    .and_then(|view| view.read(cx).terminal().read(cx).foreground_process_name());
                let has_claude_session = matches!(asker, Some(InboxAsker::ClaudeSession { .. }));
                let Some(icon) =
                    arena_agent_icon(is_codex, has_claude_session, process_name.as_deref())
                else {
                    continue;
                };
                let info = self.tab_info(item.as_ref(), arena, cx);
                let item_id = item.item_id();
                let todo = {
                    let todos = self.inbox_todos.read(cx).lists();
                    todo_list_index(todos, item_id, asker.as_ref())
                        .and_then(|index| todos.get(index))
                        .filter(|list| !list.items.is_empty())
                        .map(|list| (list.id, list.items.len()))
                };
                let has_question = self.inbox_questions.iter().any(|question| {
                    question
                        .tab
                        .as_ref()
                        .is_some_and(|tab| tab.item.id() == item_id)
                });
                let (status, status_color) = if has_question {
                    ("Decision", inbox_decision_color())
                } else {
                    match info.state {
                        Some("permission") => ("Permission", status_colors.warning),
                        Some("running") => ("Running", status_colors.success),
                        Some("ready") => ("Completed", colors.text_accent),
                        Some("error") => ("Failed", status_colors.error),
                        _ => ("Idle", colors.text_muted),
                    }
                };
                let selected = self.is_active_tab(arena, pane, item_id, cx);
                let target_pane = pane.clone();
                let target_item = item.clone();
                let title = if info.title.is_empty() {
                    if icon == IconName::AiClaude {
                        "Claude"
                    } else {
                        "Codex"
                    }
                    .to_owned()
                } else {
                    info.title
                };
                rows.push(
                    h_flex()
                        .id(("arena-agent", item_id))
                        .ml_3()
                        .px_1()
                        .py_1()
                        .gap_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.element_active))
                        .hover(|row| row.bg(colors.element_hover))
                        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .child(title.clone()),
                        )
                        .tooltip(Tooltip::text(title))
                        .when_some(todo, |row, (todo_id, count)| {
                            row.child(
                                render_arena_pill(
                                    ("agent-todo", item_id),
                                    count,
                                    inbox_todo_color(),
                                    colors.surface_background,
                                )
                                .flex_shrink_0()
                                .tooltip(Tooltip::text(format!("Open To do · {count} items")))
                                .on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.agent_activity = None;
                                        this.open_inbox_todo(todo_id, window, cx);
                                    },
                                )),
                            )
                        })
                        .child(
                            div()
                                .id(("agent-status", item_id))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(status_color)
                                .child(status)
                                .tooltip(Tooltip::text("Show agent activity"))
                                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.agent_activity = Some((item_id, event.position()));
                                    cx.notify();
                                })),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.agent_activity = None;
                            this.navigate_to_item(
                                arena_index,
                                &target_pane,
                                target_item.as_ref(),
                                window,
                                cx,
                            );
                        }))
                        .into_any_element(),
                );
            }
        }
        for list in self.inbox_todos.read(cx).lists() {
            let arena_state = arena.read(cx);
            let belongs_to_arena = match &list.arena_path {
                Some(path) => arena_state.working_directory.as_ref() == Some(path),
                None => list.arena_name == arena_state.name,
            };
            if !belongs_to_arena
                || list.items.is_empty()
                || list.tab.as_ref().is_some_and(|tab| {
                    inbox_todo_view::find_item_location(&self.arenas, tab.item.id(), cx).is_some()
                })
            {
                continue;
            }
            let todo_id = list.id;
            rows.push(
                h_flex()
                    .id(("closed-arena-agent", todo_id))
                    .ml_3()
                    .px_1()
                    .py_1()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .child(list.title.clone()),
                    )
                    .child(
                        render_arena_pill(
                            ("closed-agent-todo", todo_id),
                            list.items.len(),
                            inbox_todo_color(),
                            colors.surface_background,
                        )
                        .cursor_pointer()
                        .tooltip(Tooltip::text("Restore agent and open To do"))
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.open_inbox_todo(todo_id, window, cx);
                            },
                        )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child("Tab closed"),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    fn render_arena_row(&mut self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(arena_entity) = self.arenas.get(i).cloned() else {
            return div().into_any_element();
        };
        let agent_rows = self.render_arena_agents(i, &arena_entity, cx);
        let arena_entity = &arena_entity;
        let colors = cx.theme().colors();
        let active_index = self.active_arena_index;
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
                                            let head_sha = repo.head_commit.as_ref().map(|c| c.sha.clone());
                                            let head_tags = repo.head_tags.clone();
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
                                            Some((repo_path.clone(), branch_name, head_sha, head_tags, browser_url, lines_added, lines_deleted))
                                        } else {
                                            None
                                        }
                                    })
                                    .max_by_key(|(repo_path, _, _, _, _, _, _)| repo_path.clone())
                                    .map(|(_, branch_name, head_sha, head_tags, browser_url, lines_added, lines_deleted)| (branch_name, head_sha, head_tags, browser_url, lines_added, lines_deleted))
                            });

                            let entity_id = arena_entity.entity_id();
                            let prs = self.pr_info.get(&entity_id);
                            let multiple_prs = prs.map_or(false, |prs| prs.len() > 1);
                            let tooltip_branch = git_info.as_ref()
                                .and_then(|(branch, _, _, _, _, _)| branch.clone());

                            let mut pr_rows: Vec<(AnyElement, Option<AnyElement>)> = Vec::new();
                            for pr in prs.map(|prs| prs.as_slice()).unwrap_or_default() {
                                let ci_info = self.ci_status.get(&(entity_id, pr.number));
                                let (pr_el, review_el) = self.render_pr_elements(
                                    &format!("arena-pr-{}", arena.id),
                                    pr,
                                    ci_info,
                                    tooltip_branch.as_deref(),
                                    multiple_prs,
                                    is_active,
                                    self.manual_pr_link_for_arena(i, &pr.html_url),
                                    cx,
                                );
                                pr_rows.push((pr_el, review_el));
                            }
                            let mut pr_rows = pr_rows.into_iter();
                            let first_pr_row = pr_rows.next();
                            let extra_pr_rows: Vec<_> = pr_rows.collect();

                            let project_url = git_info.as_ref()
                                .and_then(|(_, _, _, browser_url, _, _)| browser_url.clone());

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
                                    let busy_infos = self.collect_busy_terminal_infos(arena_entity, cx);
                                    let has_pills = !busy_infos.is_empty();
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .text_size(rems(0.9375))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .when(is_renaming, |d| d.child(self.rename_editor.clone()))
                                        .when(!is_renaming, |d| {
                                            if is_active {
                                                let arena_for_rename = arena_entity.clone();
                                                d.child(
                                                    div()
                                                        .id(("arena-name", arena.id))
                                                        .cursor_pointer()
                                                        .rounded_sm()
                                                        .hover(|d| d.bg(colors.element_hover))
                                                        .child(arena.name.clone())
                                                        .on_click(cx.listener(
                                                            move |this, _event: &ClickEvent, window, cx| {
                                                                cx.stop_propagation();
                                                                this.start_rename_arena(
                                                                    arena_for_rename.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                )
                                            } else {
                                                d.child(arena.name.clone())
                                            }
                                        })
                                        .when(has_pills, |d| {
                                            d.child(
                                                h_flex()
                                                    .gap_0p5()
                                                    .when(!busy_infos.is_empty(), |d| {
                                                        let entries = busy_infos.clone();
                                                        let tooltip_entries = entries.clone();
                                                        d.child(
                                                            render_arena_pill(
                                                                ("arena-busy-badge", arena.id),
                                                                entries.len(),
                                                                colors.text,
                                                                colors.surface_background,
                                                            )
                                                            .cursor_pointer()
                                                            .tooltip(Tooltip::element(move |_window, cx| {
                                                                render_badge_tooltip(&tooltip_entries, cx)
                                                            }))
                                                            .on_click(cx.listener(
                                                                move |this, event: &ClickEvent, window, cx| {
                                                                    cx.stop_propagation();
                                                                    this.switch_arena(i, window, cx);
                                                                    this.deploy_badge_menu(
                                                                        i, entries.clone(), event.position(), window, cx,
                                                                    );
                                                                },
                                                            ))
                                                        )
                                                    })
                                            )
                                        })
                                })
                                // Row 2: branch/tag/sha (left) + diff stats (right)
                                .child({
                                    let (ref_icon, ref_label): (Option<IconName>, SharedString) =
                                        if let Some((Some(branch), _, _, _, _, _)) = git_info.as_ref() {
                                            (Some(IconName::GitBranch), branch.clone().into())
                                        } else if let Some((None, _, head_tags, _, _, _)) = git_info.as_ref() {
                                            if let Some(tag) = head_tags.first() {
                                                (Some(IconName::Tag), tag.clone().into())
                                            } else if let Some((_, Some(sha), _, _, _, _)) = git_info.as_ref() {
                                                (None, sha.chars().take(8).collect::<String>().into())
                                            } else {
                                                (None, SharedString::default())
                                            }
                                        } else {
                                            (None, SharedString::default())
                                        };
                                    let diff_stats = git_info.as_ref()
                                        .map(|(_, _, _, _, added, deleted)| (*added, *deleted));
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .text_xs()
                                        .min_h(px(16.0))
                                        .when(!ref_label.is_empty(), |d| {
                                            d.child(
                                                h_flex()
                                                    .min_w_0()
                                                    .flex_shrink_1()
                                                    .items_center()
                                                    .gap_0p5()
                                                    .text_color(colors.text_muted)
                                                    .when_some(ref_icon, |d, icon| {
                                                        d.child(
                                                            Icon::new(icon)
                                                                .size(IconSize::XSmall)
                                                                .color(Color::Muted),
                                                        )
                                                    })
                                                    .child(
                                                        div().min_w_0().truncate().child(ref_label),
                                                    ),
                                            )
                                        })
                                        .child(div().flex_grow_1())
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
                                        .when(first_pr_row.is_some(), |d| {
                                            d.child(div().flex_grow_1())
                                        })
                                        .when_some(first_pr_row, |d, (pr_el, review_el)| {
                                            d.child(pr_el).children(review_el)
                                        })
                                })
                                // Rows 4..N: one row per additional PR when the
                                // branch has several (e.g. develop + master bases).
                                .children(extra_pr_rows.into_iter().map(|(pr_el, review_el)| {
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .text_xs()
                                        .min_h(px(16.0))
                                        .child(div().flex_grow_1())
                                        .child(pr_el)
                                        .children(review_el)
                                }))
                                .children(agent_rows)
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
                                            project_url.clone(),
                                            event.position,
                                            window,
                                            cx,
                                        );
                                    })
                                })
            .into_any_element()
    }

    /// Build the PR pill and (optional) review pill for one `PrInfo`. Shared
    /// between arena rows (`show_base` for multi-PR branches, tooltips always
    /// on) and manually-linked task PR rows (no base column, tooltip only
    /// once real status is known — see `render_task_pr_row`).
    fn render_pr_elements(
        &self,
        id_prefix: &str,
        pr: &PrInfo,
        ci: Option<&CiInfo>,
        branch: Option<&str>,
        show_base: bool,
        clickable: bool,
        manual_link: Option<(Uuid, board::PrRef)>,
        cx: &Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let colors = cx.theme().colors();
        let status_colors = cx.theme().status();
        let is_pr_merged = matches!(pr.status, PrStatus::Merged);

        let ci_icon_element = if is_pr_merged {
            None
        } else {
            ci.map(|ci| {
                let (icon, color) = match &ci.status {
                    CiStatus::AllPassed => (IconName::Check, status_colors.success),
                    CiStatus::Failed => (IconName::XCircleFilled, status_colors.error),
                    CiStatus::PendingWithFailure => (IconName::Circle, status_colors.error),
                    CiStatus::PendingClean => (IconName::Circle, status_colors.warning),
                };
                Icon::new(icon).size(IconSize::Small).color(Color::Custom(color))
            })
        };

        let tooltip_pr = pr.clone();
        let tooltip_ci = ci.cloned();
        let tooltip_branch = branch.map(|branch| branch.to_string());

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
            .id(SharedString::from(format!("{id_prefix}-{}", pr.number)))
            .gap_1()
            .items_center()
            .px_1()
            .rounded_sm()
            .when(clickable, |d| {
                d.cursor_pointer().hover(|d| d.bg(colors.element_hover))
            })
            .when(pr.provider == PrProvider::Backlog, |d| d.child(backlog_badge()))
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
            .when(show_base, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .max_w_20()
                        .truncate()
                        .child(pr.base_ref.clone()),
                )
            })
            .tooltip(Tooltip::element({
                move |_window, cx| {
                    render_pr_tooltip(
                        &tooltip_pr,
                        tooltip_ci.as_ref(),
                        tooltip_branch.as_deref(),
                        cx,
                    )
                }
            }))
            .when(clickable, |d| {
                d.on_click(cx.listener({
                    let url = url.clone();
                    move |_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        cx.open_url(&url);
                    }
                }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let url = url.clone();
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_pr_context_menu(
                            url.to_string(),
                            manual_link.clone(),
                            event.position,
                            window,
                            cx,
                        );
                    }
                }),
            )
            .into_any_element();

        let review_el = if is_pr_merged {
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
                    .id(SharedString::from(format!("{id_prefix}-review-{}", pr.number)))
                    .gap_0p5()
                    .items_center()
                    .px_1()
                    .rounded_sm()
                    .when(clickable, |d| {
                        d.cursor_pointer().hover(|d| d.bg(colors.element_hover))
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
                        render_review_tooltip(&tooltip_reviews, &tooltip_head_sha, cx)
                    }))
                    .when(clickable, |d| {
                        d.on_click(cx.listener(
                            move |_this, _event: &ClickEvent, _window, cx| {
                                cx.stop_propagation();
                                cx.open_url(&url);
                            },
                        ))
                    })
                    .into_any_element()
            })
        };

        (pr_el, review_el)
    }

    /// Whether `item_id` is the tab the user is looking at: the active item of
    /// the active pane of the active arena.
    fn is_active_tab(
        &self,
        arena: &Entity<Arena>,
        pane: &Entity<Pane>,
        item_id: EntityId,
        cx: &App,
    ) -> bool {
        self.active_arena_index.and_then(|index| self.arenas.get(index)) == Some(arena)
            && arena.read(cx).active_pane == *pane
            && pane
                .read(cx)
                .active_item()
                .is_some_and(|item| item.item_id() == item_id)
    }

    fn pending_permission_row(
        &self,
        item: &dyn workspace::ItemHandle,
        cx: &App,
    ) -> Option<InboxPermissionRow> {
        let shell_pid = self.terminal_shell_pid(item, cx)?;
        let session_ids = self.session_state.pid_to_session_id.borrow();
        let session_id = session_ids.get(&shell_pid)?;
        self.inbox_permissions
            .iter()
            .find(|permission| &permission.session_id == session_id)
            .map(|permission| InboxPermissionRow {
                id: permission.id,
                tool_name: permission.tool_name.clone(),
                summary: permission.summary.clone(),
                can_always_allow: permission
                    .suggestions
                    .as_array()
                    .is_some_and(|suggestions| !suggestions.is_empty()),
            })
    }

    fn collect_inbox_entries(&self, cx: &App) -> InboxEntries {
        let mut entries = InboxEntries::default();
        for question in &self.inbox_questions {
            let resume_hint = match (question.live_tab(), &question.asker) {
                (Some(_), _) | (None, None) => None,
                (None, Some(InboxAsker::ClaudeSession { session_id })) => Some(format!(
                    "Tab not open. Resume it with `claude --resume {session_id}`."
                )),
                (None, Some(InboxAsker::CodexAcp { .. })) => {
                    Some("Tab not open. Open its arena to restore it.".to_string())
                }
            };
            entries.questions.push(InboxQuestionRow {
                id: question.id,
                is_active: question.live_tab().is_some_and(|(arena, pane, item)| {
                    self.is_active_tab(&arena, &pane, item.item_id(), cx)
                }),
                arena_name: question.arena_name.clone(),
                resume_hint,
                question: question.question.clone(),
                context: question.context.clone(),
                options: question
                    .options
                    .iter()
                    .map(|option| (option.label.clone(), option.description.clone()))
                    .collect(),
                recommended: question.recommended,
                assumed: question.assumed,
                error: question.error.clone(),
            });
        }
        let todo_lists = self.inbox_todos.read(cx).lists();
        for completion in &self.inbox_completions {
            let note = (completion.pull_request.is_none() && completion.live_tab().is_none())
                .then_some("tab not open");
            let lane = if completion.failure.is_some() {
                &mut entries.failed
            } else {
                &mut entries.completed
            };
            lane.push(InboxCompletionRow {
                id: completion.id,
                is_active: completion.live_tab().is_some_and(|(arena, pane, item)| {
                    self.is_active_tab(&arena, &pane, item.item_id(), cx)
                }),
                title: completion.title.clone(),
                detail: completion.detail.clone(),
                arena_name: completion.arena_name.clone(),
                completed_at: completion.completed_at,
                confirmed: completion.confirmed_at.is_some(),
                note,
                failure: completion.failure.clone(),
                todo_count: completion
                    .tab
                    .as_ref()
                    .map(|tab| tab.item.id())
                    .and_then(|item_id| {
                        todo_list_index(todo_lists, item_id, completion.asker.as_ref())
                    })
                    .or_else(|| {
                        let asker = completion.asker.as_ref()?;
                        todo_lists
                            .iter()
                            .position(|list| list.asker.as_ref() == Some(asker))
                    })
                    .map_or(0, |index| todo_lists[index].items.len()),
            });
        }
        for list in todo_lists {
            let detail_view = self
                .inbox_todo_views
                .get(&list.id)
                .and_then(|view| view.upgrade());
            entries.todos.push(InboxTodoRow {
                id: list.id,
                is_active: detail_view.is_some_and(|view| {
                    inbox_todo_view::find_item_location(&self.arenas, view.entity_id(), cx)
                        .is_some_and(|(arena, pane, item)| {
                            self.is_active_tab(&arena, &pane, item.item_id(), cx)
                        })
                }),
                title: list.title.clone(),
                arena_name: list.arena_name.clone(),
                item_count: list.items.len(),
                updated_at: list.updated_at,
                note: self.todo_restore_errors.get(&list.id).cloned().or_else(|| {
                    if self.pending_todo_restores.contains_key(&list.id) {
                        Some("restoring the tab…".to_string())
                    } else {
                        list.tab.is_none().then(|| "tab not open · click to restore".to_string())
                    }
                }),
            });
        }
        for (arena_index, arena_entity) in self.arenas.iter().enumerate() {
            let arena_name = arena_entity.read(cx).name.clone();
            let to_entry = |info: BadgeEntryInfo| InboxEntry {
                arena_index,
                arena_name: arena_name.clone(),
                is_active: self.is_active_tab(arena_entity, &info.pane, info.item.item_id(), cx),
                permission: self.pending_permission_row(info.item.as_ref(), cx),
                info,
            };
            entries.permission.extend(
                self.collect_permission_terminal_infos(arena_entity, cx)
                    .into_iter()
                    .map(to_entry),
            );
            entries.in_progress.extend(
                self.collect_running_terminal_infos(arena_entity, cx)
                    .into_iter()
                    .map(to_entry),
            );
        }
        entries
    }

    fn render_agent_activity_popover(
        &self,
        item_id: EntityId,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let mut entries = self.collect_inbox_entries(cx);
        entries.questions.retain(|row| {
            self.inbox_questions.iter().any(|question| {
                question.id == row.id
                    && question
                        .tab
                        .as_ref()
                        .is_some_and(|tab| tab.item.id() == item_id)
            })
        });
        entries
            .permission
            .retain(|entry| entry.info.item.item_id() == item_id);
        entries
            .in_progress
            .retain(|entry| entry.info.item.item_id() == item_id);
        entries.completed.retain(|row| {
            self.inbox_completions.iter().any(|completion| {
                completion.id == row.id
                    && completion
                        .tab
                        .as_ref()
                        .is_some_and(|tab| tab.item.id() == item_id)
            })
        });
        entries.failed.retain(|row| {
            self.inbox_completions.iter().any(|completion| {
                completion.id == row.id
                    && completion
                        .tab
                        .as_ref()
                        .is_some_and(|tab| tab.item.id() == item_id)
            })
        });
        entries.todos.clear();
        let empty = entries.questions.is_empty()
            && entries.permission.is_empty()
            && entries.in_progress.is_empty()
            && entries.completed.is_empty()
            && entries.failed.is_empty();
        let title = inbox_todo_view::find_item_location(&self.arenas, item_id, cx)
            .map(|(_, _, item)| item.tab_content_text(0, cx))
            .unwrap_or_else(|| "Agent activity".into());
        deferred(
            anchored().position(position).anchor(Anchor::TopLeft).child(
                v_flex()
                    .id("agent-activity")
                    .occlude()
                    .w(px(440.0))
                    .max_h(px(600.0))
                    .p_2()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .bg(colors.elevated_surface_background)
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.agent_activity = None;
                        cx.notify();
                    }))
                    .child(h_flex().justify_between().child(Label::new(title)).child(
                        Button::new("close-agent-activity", "Close").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.agent_activity = None;
                                cx.notify();
                            },
                        )),
                    ))
                    .when(empty, |element| {
                        element.child(Label::new("No pending activity").color(Color::Muted))
                    })
                    .child(self.render_agent_activity(entries, cx)),
            ),
        )
        .with_priority(1)
    }

    fn render_agent_activity(&self, entries: InboxEntries, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let status_colors = cx.theme().status().clone();

        let lane_header = |label: &'static str, count: usize, swatch: Hsla| {
            h_flex()
                .gap_1p5()
                .px_1()
                .pt_2()
                .pb_0p5()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(div().size_2().rounded_sm().bg(swatch))
                .child(label)
                .child(
                    div()
                        .text_color(colors.text_muted)
                        .child(count.to_string()),
                )
        };
        let render_rows = |lane: &'static str, lane_entries: Vec<InboxEntry>, cx: &mut Context<Self>| {
            let is_in_progress = lane == "in-progress";
            lane_entries
                .into_iter()
                .enumerate()
                .map(|(row_index, entry)| {
                    let InboxEntry {
                        arena_index,
                        arena_name,
                        info,
                        is_active,
                        permission,
                    } = entry;
                    let primary = if info.primary.is_empty() {
                        "(no prompt)".to_string()
                    } else {
                        truncate_for_menu(&info.primary)
                    };
                    let secondary = if info.secondary.is_empty() {
                        arena_name
                    } else {
                        format!("{arena_name} · {}", truncate_for_menu(&info.secondary))
                    };
                    v_flex()
                        .id(SharedString::from(format!("inbox-{lane}-{row_index}")))
                        .when(is_in_progress, |d| d.opacity(0.7))
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(if is_active {
                            colors.element_selected
                        } else {
                            colors.element_background
                        })
                        .cursor_pointer()
                        .hover(|d| d.bg(colors.element_hover))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(primary),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .truncate()
                                .child(secondary),
                        )
                        .when_some(permission, |d, permission| {
                            d.child(render_inbox_permission(permission, &colors, cx))
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.navigate_to_item(
                                arena_index,
                                &info.pane,
                                info.item.as_ref(),
                                window,
                                cx,
                            );
                        }))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        };

        let question_count = entries.questions.len();
        let question_rows = entries
            .questions
            .into_iter()
            .map(|row| self.render_inbox_question(row, cx))
            .collect::<Vec<_>>();
        let permission_count = entries.permission.len();
        let failed_count = entries.failed.len();
        let failed_rows = entries
            .failed
            .into_iter()
            .map(|row| self.render_inbox_completion(row, cx))
            .collect::<Vec<_>>();
        let todo_count = entries.todos.len();
        let todo_rows = entries
            .todos
            .into_iter()
            .map(|row| self.render_inbox_todo(row, cx))
            .collect::<Vec<_>>();
        let completed_count = entries.completed.len();
        let permission_rows = render_rows("permission", entries.permission, cx);
        let in_progress_count = entries.in_progress.len();
        let in_progress_rows = render_rows("in-progress", entries.in_progress, cx);
        let completed_rows = entries
            .completed
            .into_iter()
            .map(|row| self.render_inbox_completion(row, cx))
            .collect::<Vec<_>>();

        v_flex()
            .id("agent-activity-list")
            .flex_1()
            .overflow_y_scroll()
            .gap_1()
            .px_1()
            .when(!question_rows.is_empty(), |d| {
                d.child(lane_header("Needs decision", question_count, inbox_decision_color()))
                    .children(question_rows)
            })
            .when(!permission_rows.is_empty(), |d| {
                d.child(lane_header("Needs permission", permission_count, status_colors.warning))
                    .children(permission_rows)
            })
            .when(!failed_rows.is_empty(), |d| {
                d.child(lane_header("Failed", failed_count, status_colors.error))
                    .children(failed_rows)
            })
            .when(!todo_rows.is_empty(), |d| {
                d.child(lane_header("To do", todo_count, inbox_todo_color()))
                    .children(todo_rows)
            })
            .when(!completed_rows.is_empty(), |d| {
                d.child(lane_header("Completed", completed_count, colors.text_accent))
                    .children(completed_rows)
            })
            .when(!in_progress_rows.is_empty(), |d| {
                d.child(lane_header("In progress", in_progress_count, status_colors.success))
                    .children(in_progress_rows)
            })
            .into_any_element()
    }

    fn render_inbox_todo(&self, row: InboxTodoRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let todo_id = row.id;
        let title = if row.title.is_empty() {
            row.arena_name.clone()
        } else {
            truncate_for_menu(&row.title)
        };
        let detail = format!(
            "{} · {} {} · {}",
            row.arena_name,
            row.item_count,
            if row.item_count == 1 { "item" } else { "items" },
            row.updated_at.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
        );
        h_flex()
            .id(SharedString::from(format!("inbox-todo-{todo_id}")))
            .mx_1()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_md()
            .bg(if row.is_active {
                colors.element_selected
            } else {
                colors.element_background
            })
            .cursor_pointer()
            .hover(|d| d.bg(colors.element_hover))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .truncate()
                            .child(detail),
                    )
                    .when_some(row.note, |d, note| {
                        d.child(div().text_xs().text_color(colors.text_accent).child(note))
                    }),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("inbox-todo-{todo_id}-dismiss")),
                    IconName::Close,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Dismiss"))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    this.dismiss_inbox_todo(todo_id, cx);
                })),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_inbox_todo(todo_id, window, cx);
            }))
            .into_any_element()
    }

    fn render_inbox_completion(&self, row: InboxCompletionRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let error_color = cx.theme().status().error;
        let completion_id = row.id;
        let title = if row.title.is_empty() {
            "(no prompt)".to_string()
        } else {
            truncate_for_menu(&row.title)
        };
        let mut detail = format!(
            "{} · {}",
            row.completed_at.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
            row.arena_name
        );
        if !row.detail.is_empty() {
            detail.push_str(" · ");
            detail.push_str(&truncate_for_menu(&row.detail));
        }
        h_flex()
            .id(SharedString::from(format!("inbox-completion-{completion_id}")))
            .when(row.confirmed, |d| d.opacity(0.6))
            .mx_1()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_md()
            .bg(if row.is_active {
                colors.element_selected
            } else {
                colors.element_background
            })
            .cursor_pointer()
            .hover(|d| d.bg(colors.element_hover))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(title),
                            )
                            .when(row.todo_count > 0, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .px_1p5()
                                        .rounded_md()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(inbox_todo_color())
                                        .bg(inbox_todo_color().opacity(0.15))
                                        .child(format!("To do {}", row.todo_count)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .truncate()
                            .child(detail),
                    )
                    .when_some(row.failure, |d, failure| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(error_color)
                                .truncate()
                                .child(failure),
                        )
                    })
                    .when_some(row.note, |d, note| {
                        d.child(div().text_xs().text_color(colors.text_accent).child(note))
                    }),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("inbox-completion-{completion_id}-dismiss")),
                    IconName::Close,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Dismiss"))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    this.dismiss_inbox_completion(completion_id, cx);
                })),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_inbox_completion(completion_id, window, cx);
            }))
            .into_any_element()
    }

    fn render_inbox_question(&self, row: InboxQuestionRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let status_colors = cx.theme().status().clone();
        let question_id = row.id;
        let has_options = !row.options.is_empty();
        let option_buttons = row
            .options
            .into_iter()
            .enumerate()
            .map(|(option_index, (label, description))| {
                let is_recommended = row.recommended == Some(option_index);
                v_flex()
                    .id(SharedString::from(format!(
                        "inbox-question-{question_id}-option-{option_index}"
                    )))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_recommended {
                        inbox_decision_color()
                    } else {
                        colors.border
                    })
                    .cursor_pointer()
                    .hover(|d| d.bg(colors.element_hover))
                    .child(
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(colors.text_muted)
                                    .child(format!("{}.", option_index + 1)),
                            )
                            .child(label)
                            .when(is_recommended, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(inbox_decision_color())
                                        .child(if row.assumed {
                                            "assumed · agent continues"
                                        } else {
                                            "recommended"
                                        }),
                                )
                            }),
                    )
                    .when_some(description, |d, description| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child(description),
                        )
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.answer_inbox_question(question_id, Some(option_index), window, cx);
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        v_flex()
            .mx_1()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_md()
            .bg(if row.is_active {
                colors.element_selected
            } else {
                colors.element_background
            })
            .child(
                div()
                    .id(SharedString::from(format!("inbox-question-{question_id}")))
                    .cursor_pointer()
                    .hover(|d| d.text_color(colors.text_accent))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(row.question)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_inbox_question(question_id, window, cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(row.arena_name),
            )
            .when_some(row.context, |d, context| {
                d.child(div().text_xs().child(context))
            })
            .children(option_buttons)
            .when_some(self.inbox_note_editors.get(&question_id).cloned(), |d, editor| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .text_sm()
                        .child(editor),
                )
            })
            .when(!has_options, |d| {
                d.child(
                    h_flex().justify_end().child(
                        Button::new(
                            SharedString::from(format!("inbox-question-{question_id}-send")),
                            "Send",
                        )
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.answer_inbox_question(question_id, None, window, cx);
                        })),
                    ),
                )
            })
            .when_some(row.resume_hint, |d, hint| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(status_colors.warning)
                        .child(hint),
                )
            })
            .when_some(row.error, |d, error| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(status_colors.error)
                        .child(error),
                )
            })
            .child(
                h_flex().justify_end().child(
                    Button::new(
                        SharedString::from(format!("inbox-question-{question_id}-dismiss")),
                        "Dismiss",
                    )
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.dismiss_inbox_question(question_id, cx);
                    })),
                ),
            )
            .into_any_element()
    }

    fn render_sidebar_tab_button(
        &self,
        label: &'static str,
        tab: SidebarTab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let is_active = self.sidebar_tab == tab;
        h_flex()
            .id(label)
            .flex_1()
            .justify_center()
            .px_2()
            .py_0p5()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(is_active, |d| {
                d.bg(colors.element_selected)
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .when(!is_active, |d| {
                d.text_color(colors.text_muted)
                    .hover(|d| d.bg(colors.element_hover))
            })
            .gap_1()
            .child(label)
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.agent_activity = None;
                this.sidebar_tab = tab;
                cx.notify();
            }))
    }

    fn render_task_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();

        struct TaskData {
            id: Uuid,
            title: String,
            issues: Vec<board::IssueLink>,
            worktrees: Vec<WorktreeRow>,
            collapsed: bool,
        }
        let tasks: Vec<TaskData> = self
            .board
            .active_tasks()
            .map(|task| TaskData {
                id: task.id,
                title: task.title.clone(),
                issues: task.issues.clone(),
                worktrees: self
                    .board_cache
                    .task_worktrees
                    .get(&task.id)
                    .cloned()
                    .unwrap_or_default(),
                collapsed: task.collapsed,
            })
            .collect();
        let unassigned = self.board_cache.unassigned_arenas.clone();

        let mut list = div().id("task-list").flex_1().overflow_y_scroll();

        if tasks.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child("No tasks yet"),
            );
        }

        for task in tasks {
            let task_id = task.id;
            let is_renaming = self.renaming_task == Some(task_id);
            let contains_active_arena = task.worktrees.iter().any(|row| {
                matches!(row, WorktreeRow::Live { arena_index }
                    if Some(*arena_index) == self.active_arena_index)
            });
            let header = h_flex()
                .id(SharedString::from(format!("task-header-{task_id}")))
                .px_2()
                .py_1()
                .gap_1()
                .items_center()
                .cursor_pointer()
                .hover(|d| d.bg(colors.element_hover))
                .child(
                    Icon::new(if task.collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(if is_renaming {
                    div()
                        .flex_1()
                        .child(self.task_rename_editor.clone())
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(task.title.clone())
                        .into_any_element()
                })
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if this.renaming_task == Some(task_id) {
                        return;
                    }
                    if let Ok(task) = this.board.task_mut(task_id) {
                        task.collapsed = !task.collapsed;
                        this.persist_board(cx);
                    }
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        this.deploy_task_context_menu(task_id, event.position, window, cx);
                    }),
                );

            let mut section = div()
                .id(SharedString::from(format!("task-section-{task_id}")))
                .mb_1()
                // Width is reserved for every task so the accent line on the
                // task holding the active arena doesn't shift layout.
                .border_l_2()
                .border_color(if contains_active_arena {
                    colors.text_accent
                } else {
                    gpui::transparent_black()
                })
                .child(header);

            if !task.collapsed {
                if self.adding_issue_to_task == Some(task_id) {
                    section = section.child(
                        div()
                            .pl_5()
                            .pr_2()
                            .py_0p5()
                            .text_xs()
                            .child(self.issue_input_editor.clone()),
                    );
                    if let Some(error) = self.issue_input_error.clone() {
                        section = section.child(
                            div()
                                .pl_5()
                                .pr_2()
                                .text_xs()
                                .text_color(cx.theme().status().error)
                                .child(error),
                        );
                    }
                }
                for issue in &task.issues {
                    section = section.child(self.render_issue_row(task_id, issue, &colors, cx));
                }
                for row in &task.worktrees {
                    section = match row {
                        WorktreeRow::Live { arena_index } => section.child(
                            div().pl_3().child(self.render_arena_row(*arena_index, cx)),
                        ),
                        WorktreeRow::Closed { path } => section.child(
                            self.render_closed_worktree_row(
                                task_id,
                                path.clone(),
                                false,
                                &colors,
                                cx,
                            ),
                        ),
                        WorktreeRow::Missing { path } => section.child(
                            self.render_closed_worktree_row(
                                task_id,
                                path.clone(),
                                true,
                                &colors,
                                cx,
                            ),
                        ),
                    };
                }
            }
            list = list.child(section);
        }

        if !unassigned.is_empty() {
            let mut section = div().id("unassigned-section").mt_1().child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .pb_0p5()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child("Unassigned"),
                    )
                    .child(div().flex_1().h_px().bg(colors.border)),
            );
            for arena_index in unassigned {
                section = section.child(self.render_arena_row(arena_index, cx));
            }
            list = list.child(section);
        }

        list.into_any_element()
    }

    fn render_issue_row(
        &self,
        task_id: Uuid,
        issue: &board::IssueLink,
        colors: &theme::ThemeColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reference = issue.reference.clone();
        let label = reference.short_label();
        let url_for_click = issue.url.clone();
        let url_for_menu = issue.url.clone();

        let provider_badge: AnyElement = match &issue.reference {
            board::IssueRef::GitHub { .. } => Icon::new(IconName::Github)
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
            board::IssueRef::Backlog { .. } => backlog_badge(),
        };

        let tooltip_text = issue.title.as_ref().map(|title| match &issue.state {
            Some(state) => format!("{title} ({state})"),
            None => title.clone(),
        });

        h_flex()
            .id(SharedString::from(format!("task-issue-{task_id}-{label}")))
            .pl_5()
            .pr_2()
            .py_0p5()
            .gap_1()
            .items_center()
            .text_xs()
            .when(url_for_click.is_some(), |d| {
                d.cursor_pointer().hover(|d| d.bg(colors.element_hover))
            })
            .when_some(tooltip_text, |d, text| d.tooltip(Tooltip::text(text)))
            .child(provider_badge)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colors.text_muted)
                    .child(label),
            )
            .when_some(issue.title.clone(), |d, title| {
                d.child(div().min_w_0().flex_shrink_1().truncate().child(title))
            })
            .child(div().flex_grow_1())
            .when_some(issue.state.clone(), |d, state| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .text_color(colors.text_muted)
                        .child(state),
                )
            })
            .when_some(url_for_click, |d, url| {
                d.on_click(cx.listener(move |_this, _, _window, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_issue_context_menu(
                        task_id,
                        reference.clone(),
                        url_for_menu.clone(),
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
    }

    /// Render one manually-linked task PR (`agentium task add-pr`). Mirrors
    /// `render_issue_row`'s layout. Falls back to a provisional `PrInfo`
    /// built from the cached `PrLink` (title/status only) when `gh`/`bee`
    /// haven't resolved it yet, and suppresses the tooltip in that case so it
    /// doesn't show a fabricated "Draft" state.
    fn render_closed_worktree_row(
        &self,
        task_id: Uuid,
        path: PathBuf,
        missing: bool,
        colors: &theme::ThemeColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let element_id = SharedString::from(format!("task-worktree-{task_id}-{}", path.display()));
        let path_for_open = path.clone();
        let path_for_menu = path;

        h_flex()
            .id(element_id)
            .pl_5()
            .pr_2()
            .py_0p5()
            .gap_1()
            .items_center()
            .text_xs()
            .child(
                Icon::new(IconName::Folder)
                    .size(IconSize::XSmall)
                    .color(if missing { Color::Warning } else { Color::Muted }),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(colors.text_muted)
                    .child(name),
            )
            .when(missing, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .text_color(colors.text_muted)
                        .child("(missing)"),
                )
            })
            .when(!missing, |d| {
                d.cursor_pointer()
                    .hover(|d| d.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.add_arena_with_path(path_for_open.clone(), window, cx);
                    }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_worktree_context_menu(
                        task_id,
                        path_for_menu.clone(),
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
    }
}

impl Render for AgentiumApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Owned so that closures needing `&mut cx` (e.g. the per-arena row
        // renderer) can run while colors are still in scope.
        let colors = cx.theme().colors().clone();
        self.bind_inbox_tabs(window, cx);
        self.drop_stale_inbox_permissions();
        self.sync_inbox_note_editors(window, cx);
        self.sync_inbox_completions(cx);

        div()
            .key_context("Agentium")
            .on_action(cx.listener(|this, action: &ActivateArena, window, cx| {
                this.switch_arena(action.index, window, cx);
            }))
            .on_action(|_: &workspace::Open, _, _| {})
            .on_action(|_: &workspace::OpenFiles, _, _| {})
            .flex()
            .flex_row()
            .size_full()
            .bg(colors.background)
            .text_color(colors.text)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(320.0))
                    .h_full()
                    .bg(colors.panel_background)
                    .border_r_1()
                    .border_color(colors.border)
                    // Single dispatcher for both rename editors: arena rows are
                    // also rendered under the Tasks tab, so their editor can be
                    // focused outside #arena-list; actions stop at the first
                    // matching handler, so exactly one handler must exist here.
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        if let Some(question_id) = this.focused_inbox_note(window, cx) {
                            // Enter sends only an option-less question's
                            // answer; with options, a click picks which one.
                            this.answer_inbox_question(question_id, None, window, cx);
                        } else if this.adding_issue_to_task.is_some() {
                            this.finish_add_issue(true, cx);
                        } else if this.renaming_task.is_some() {
                            this.finish_rename_task(true, cx);
                        } else {
                            this.finish_rename_arena(true, cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        if this.focused_inbox_note(window, cx).is_some() {
                            // Keep the draft; Escape only leaves the note.
                        } else if this.adding_issue_to_task.is_some() {
                            this.finish_add_issue(false, cx);
                        } else if this.renaming_task.is_some() {
                            this.finish_rename_task(false, cx);
                        } else {
                            this.finish_rename_arena(false, cx);
                        }
                    }))
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
                        h_flex()
                            .px_2()
                            .pb_1()
                            .gap_1()
                            .child(self.render_sidebar_tab_button(
                                "Repositories",
                                SidebarTab::Repositories,
                                cx,
                            ))
                            .child(self.render_sidebar_tab_button("Tasks", SidebarTab::Tasks, cx)),
                    )
                    .child(match self.sidebar_tab {
                        SidebarTab::Repositories => div()
                            .id("arena-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .children(
                                (0..self.arenas.len()).map(|i| self.render_arena_row(i, cx)),
                            )
                            .into_any_element(),
                        SidebarTab::Tasks => self.render_task_list(cx),
                    })
                    .when(self.sidebar_tab == SidebarTab::Repositories, |d| d.child(
                        div()
                            .p_2()
                            .child({
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
                                    .anchor(Anchor::BottomLeft)
                            })
                    ))
                    .when(self.sidebar_tab == SidebarTab::Tasks, |d| d.child(
                        div()
                            .p_2()
                            .child(
                                Button::new("new-task", "+ New Task")
                                    .full_width()
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create_task(None, window, cx);
                                    })),
                            ),
                    ))
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
                                .anchor(Anchor::TopLeft)
                                .child(menu.clone()),
                        )
                        .with_priority(1)
                    }))
                    .when_some(self.agent_activity, |element, (item_id, position)| {
                        element.child(self.render_agent_activity_popover(item_id, position, cx))
                    })
                    .children(self.badge_menu.as_ref().map(|(menu, position, _)| {
                        deferred(
                            anchored()
                                .position(*position)
                                .anchor(Anchor::TopLeft)
                                .child(menu.clone()),
                        )
                        .with_priority(1)
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .h_full()
                    .bg(colors.background)
                    .when_some(
                        self.arena_error
                            .clone()
                            .or_else(|| self.codex_tabs.read(cx).error()),
                        |element, error| {
                            element.child(
                                div()
                                    .p_2()
                                    .text_sm()
                                    .text_color(cx.theme().status().warning)
                                    .child(error),
                            )
                        },
                    )
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

// The picker draws its own elevated background, but sizes its results list from
// its own shape rather than the parent. If this width and the wrapper's disagree,
// the list overflows the background and the excess renders with no surface behind
// it.
const RECENT_PROJECTS_WIDTH: f32 = 235.0;

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
                .initial_width(rems_from_px(RECENT_PROJECTS_WIDTH))
                .popover()
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
                .recent_project_workspaces(fs.as_ref())
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
            .w(px(RECENT_PROJECTS_WIDTH))
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
                .recent_project_workspaces(fs.as_ref())
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

    fn name() -> &'static str {
        "agentium recent projects"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn render_editor(
        &self,
        editor: &Arc<dyn ErasedEditor>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Div> {
        Some(
            h_flex()
                .flex_none()
                .h_9()
                .px_2p5()
                .justify_between()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(editor.render(window, cx)),
        )
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
            active: false,
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
                        .flex_grow_1()
                        // Disambiguated because gpui's blanket `View` impl also
                        // provides a `render` for this type via the glob import.
                        .child(RenderOnce::render(highlighted, window, cx)),
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

#[cfg(test)]
mod tests {
    // Not `use super::*`: that would pull gpui's `test` attribute macro into
    // scope and make `#[test]` expand recursively.
    use super::{
        CiCheckEntry, CiFetchGenerations, CiInfo, CiStatus, FinishKey, FinishedTab,
        InboxCompletion, InboxPullRequest,
        InboxTab, InboxTodoItem, InboxTodoKind, InboxTodoList, RecordedTabState,
        age_inbox_records, apply_ci_status, dropped_todo_titles, next_inbox_id,
        parse_todo_set_request, record_finished_tabs, todo_list_index,
        ClaudeSessionState, InboxAsker, InboxOption, InboxQuestion, WorktreeRow, permission_summary, backlog_issue_keys_in_branch, next_title_driven_state,
        parse_backlog_remote, pr_ref_matches_remote, sender_label, tail_lines, task_fetch_inputs,
        tasks_sharing_arena, with_from_line,
    };
    use crate::arena::TitleState;
    use crate::board::{BoardTask, IssueLink, IssueRef, PrRef};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn permission_summary_shows_what_the_tool_touches() {
        assert_eq!(
            permission_summary(&serde_json::json!({ "command": "rm -rf build", "description": "x" })),
            "rm -rf build"
        );
        assert_eq!(
            permission_summary(&serde_json::json!({ "file_path": "/a.rs", "content": "..." })),
            "/a.rs"
        );
        assert_eq!(
            permission_summary(&serde_json::json!({ "server": "github" })),
            r#"{"server":"github"}"#
        );
    }

    #[test]
    fn inbox_question_persists_its_asker_but_not_its_tab() {
        let question = InboxQuestion {
            id: 3,
            question: "JST or UTC?".to_string(),
            context: None,
            options: vec![InboxOption {
                label: "JST".to_string(),
                description: Some("matches invoices".to_string()),
            }],
            recommended: Some(0),
            assumed: true,
            arena_name: "backlog-scala".to_string(),
            asker: Some(InboxAsker::ClaudeSession {
                session_id: "c8832ec2".to_string(),
            }),
            tab: None,
            error: Some("the asking tab is not open".to_string()),
        };
        let json = serde_json::to_value(&question).expect("serialize");
        assert_eq!(
            json["asker"],
            serde_json::json!({ "kind": "claude_session", "session_id": "c8832ec2" })
        );
        assert!(json.get("error").is_none());

        assert_eq!(json["assumed"], true);

        let restored: InboxQuestion = serde_json::from_value(json.clone()).expect("deserialize");
        assert!(restored.tab.is_none() && restored.error.is_none());
        assert!(restored.assumed);

        // Files written before `--assume` existed lack the field.
        let mut older = json;
        older.as_object_mut().expect("object").remove("assumed");
        let restored: InboxQuestion = serde_json::from_value(older).expect("deserialize older");
        assert!(!restored.assumed);
        assert!(
            restored.asker
                == Some(InboxAsker::ClaudeSession {
                    session_id: "c8832ec2".to_string()
                })
        );
        assert_eq!(restored.options[0].label, "JST");
    }

    fn test_tab(cx: &mut gpui::TestAppContext) -> (gpui::EntityId, InboxTab) {
        use workspace::ItemHandle as _;
        use gpui::AppContext as _;
        let item = cx.update(|cx| cx.new(workspace::item::test::TestItem::new));
        let tab = InboxTab {
            arena: gpui::WeakEntity::new_invalid(),
            pane: gpui::WeakEntity::new_invalid(),
            item: item.downgrade_item(),
        };
        (item.entity_id(), tab)
    }

    fn finished(
        item_id: gpui::EntityId,
        tab: &InboxTab,
        failed: bool,
        turn: u64,
        asker: Option<InboxAsker>,
    ) -> FinishedTab {
        FinishedTab {
            item_id,
            key: FinishKey { failed, turn },
            tab: Some(InboxTab {
                arena: tab.arena.clone(),
                pane: tab.pane.clone(),
                item: tab.item.boxed_clone(),
            }),
            asker,
            arena_name: "zed".to_string(),
            title: "Fix the Inbox".to_string(),
            detail: "Codex (ACP)".to_string(),
            failure: failed.then(|| "Stopped with an error".to_string()),
            notify: false,
        }
    }

    fn ci(status: CiStatus, failing: &[&str]) -> CiInfo {
        CiInfo {
            status,
            checks: failing
                .iter()
                .map(|name| CiCheckEntry {
                    name: name.to_string().into(),
                    bucket: "fail".into(),
                })
                .chain(std::iter::once(CiCheckEntry {
                    name: "docs".into(),
                    bucket: "pass".into(),
                }))
                .collect(),
        }
    }

    #[gpui::test]
    fn finished_turns_are_recorded_once_and_replace_the_tabs_record(
        cx: &mut gpui::TestAppContext,
    ) {
        let (item, tab) = test_tab(cx);
        let mut records = Vec::new();
        let mut seen = HashMap::new();
        let mut next_id = 1;
        let now = chrono::Utc::now();
        let no_questions = std::collections::HashSet::new();
        let mut sync = |records: &mut Vec<InboxCompletion>, tabs: Vec<FinishedTab>| {
            record_finished_tabs(records, &mut seen, tabs, &no_questions, &mut next_id, now)
        };

        assert_eq!(sync(&mut records, vec![finished(item, &tab, false, 0, None)]).len(), 1);
        // Still Completed at the next sync: not news.
        assert!(sync(&mut records, vec![finished(item, &tab, false, 0, None)]).is_empty());
        assert_eq!(records.len(), 1);

        // The same tab failing replaces its completion.
        assert_eq!(sync(&mut records, vec![finished(item, &tab, true, 1, None)]).len(), 1);
        assert_eq!(records.len(), 1);
        assert!(records[0].failure.is_some());

        // Error -> Running -> Error seen only as its final state: the turn
        // counter still tells the second failure apart.
        let first_failure = records[0].id;
        assert_eq!(sync(&mut records, vec![finished(item, &tab, true, 2, None)]).len(), 1);
        assert_eq!(records.len(), 1);
        assert_ne!(records[0].id, first_failure);

        // Completing again replaces the failure.
        assert_eq!(sync(&mut records, vec![finished(item, &tab, false, 3, None)]).len(), 1);
        assert_eq!(records.len(), 1);
        assert!(records[0].failure.is_none());

        // Not finished at one sync, finished again at the next: a new turn.
        assert!(sync(&mut records, Vec::new()).is_empty());
        assert_eq!(sync(&mut records, vec![finished(item, &tab, false, 0, None)]).len(), 1);
        assert_eq!(records.len(), 1);
    }

    #[gpui::test]
    fn a_reopened_tab_replaces_the_record_of_the_same_asker(cx: &mut gpui::TestAppContext) {
        let (before_restart, old_tab) = test_tab(cx);
        let (after_restart, new_tab) = test_tab(cx);
        let asker = || {
            Some(InboxAsker::ClaudeSession {
                session_id: "c8832ec2".to_string(),
            })
        };
        let mut records = Vec::new();
        let mut seen = HashMap::new();
        let mut next_id = 1;
        let now = chrono::Utc::now();
        let no_questions = std::collections::HashSet::new();
        record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(before_restart, &old_tab, false, 0, asker())],
            &no_questions,
            &mut next_id,
            now,
        );
        record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(after_restart, &new_tab, false, 0, asker())],
            &no_questions,
            &mut next_id,
            now,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].tab.as_ref().map(|tab| tab.item.id()),
            Some(after_restart)
        );
    }

    #[gpui::test]
    fn an_asking_tab_is_not_completed_but_can_fail(cx: &mut gpui::TestAppContext) {
        let (item, tab) = test_tab(cx);
        let asking = std::collections::HashSet::from([item]);
        let mut records = Vec::new();
        let mut seen = HashMap::new();
        let mut next_id = 1;
        let now = chrono::Utc::now();
        let recorded = record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(item, &tab, false, 0, None)],
            &asking,
            &mut next_id,
            now,
        );
        assert!(recorded.is_empty());
        // Answering the question leaves the tab Completed: still not recorded.
        let recorded = record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(item, &tab, false, 0, None)],
            &std::collections::HashSet::new(),
            &mut next_id,
            now,
        );
        assert!(recorded.is_empty());
        let recorded = record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(item, &tab, true, 1, None)],
            &asking,
            &mut next_id,
            now,
        );
        assert_eq!(recorded.len(), 1);
    }

    #[gpui::test]
    fn records_age_by_their_tab_state(cx: &mut gpui::TestAppContext) {
        let (item, tab) = test_tab(cx);
        let mut records = Vec::new();
        let mut seen = HashMap::new();
        let mut next_id = 1;
        let now = chrono::Utc::now();
        record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(item, &tab, false, 0, None)],
            &std::collections::HashSet::new(),
            &mut next_id,
            now,
        );

        assert!(!age_inbox_records(&mut records, now, |_| RecordedTabState::Gone));
        assert!(!age_inbox_records(&mut records, now, |_| RecordedTabState::Idle));
        assert!(records[0].confirmed_at.is_none());

        assert!(age_inbox_records(&mut records, now, |_| RecordedTabState::Viewed));
        assert_eq!(records[0].confirmed_at, Some(now));

        let within_ttl = now + chrono::TimeDelta::minutes(59);
        assert!(!age_inbox_records(&mut records, within_ttl, |_| RecordedTabState::Gone));
        assert_eq!(records.len(), 1);
        let past_ttl = now + chrono::TimeDelta::minutes(61);
        assert!(age_inbox_records(&mut records, past_ttl, |_| RecordedTabState::Gone));
        assert!(records.is_empty());

        record_finished_tabs(
            &mut records,
            &mut seen,
            vec![finished(item, &tab, true, 1, None)],
            &std::collections::HashSet::new(),
            &mut next_id,
            now,
        );
        assert!(age_inbox_records(&mut records, now, |_| RecordedTabState::Busy));
        assert!(records.is_empty());
    }

    #[test]
    fn ci_failures_are_recorded_on_a_change_from_a_known_good_status() {
        let pull_request = InboxPullRequest {
            arena_path: PathBuf::from("/src/zed"),
            number: 812,
        };
        let mut records = Vec::new();
        let mut next_id = 1;
        let now = chrono::Utc::now();
        let mut apply = |records: &mut Vec<InboxCompletion>,
                         previous: Option<CiStatus>,
                         next: Option<CiInfo>| {
            apply_ci_status(
                records,
                &pull_request,
                previous.as_ref(),
                next.as_ref(),
                "Retry on 429".to_string(),
                "zed".to_string(),
                &mut next_id,
                now,
            )
        };

        // The first status seen (e.g. at startup) is not news.
        assert_eq!(
            apply(&mut records, None, Some(ci(CiStatus::Failed, &["lint"]))),
            (false, None)
        );
        assert!(records.is_empty());

        let (changed, recorded) = apply(
            &mut records,
            Some(CiStatus::PendingClean),
            Some(ci(CiStatus::PendingWithFailure, &["lint", "test"])),
        );
        assert!(changed && recorded.is_some());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].failure.as_deref(), Some("Failing: lint, test"));
        assert_eq!(records[0].detail, "PR #812");

        // Still failing: no duplicate.
        assert_eq!(
            apply(
                &mut records,
                Some(CiStatus::PendingWithFailure),
                Some(ci(CiStatus::Failed, &["lint"]))
            ),
            (false, None)
        );
        assert_eq!(records.len(), 1);

        // A dismissed failure stays dismissed while CI keeps failing...
        records.clear();
        assert_eq!(
            apply(&mut records, Some(CiStatus::Failed), Some(ci(CiStatus::Failed, &["lint"]))),
            (false, None)
        );
        assert!(records.is_empty());
        // ...and a recovery followed by a new failure is recorded again.
        assert_eq!(
            apply(&mut records, Some(CiStatus::Failed), Some(ci(CiStatus::AllPassed, &[]))),
            (false, None)
        );
        let (_, recorded) = apply(
            &mut records,
            Some(CiStatus::AllPassed),
            Some(ci(CiStatus::Failed, &["lint"])),
        );
        assert!(recorded.is_some());

        // A response with no status (gh error) leaves the record alone.
        assert_eq!(apply(&mut records, Some(CiStatus::Failed), None), (false, None));
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn a_clean_ci_result_clears_a_failure_persisted_across_a_restart() {
        let pull_request = InboxPullRequest {
            arena_path: PathBuf::from("/src/zed"),
            number: 812,
        };
        let mut records = vec![InboxCompletion {
            id: 4,
            title: "Retry on 429".to_string(),
            detail: "PR #812".to_string(),
            arena_name: "zed".to_string(),
            completed_at: chrono::Utc::now(),
            confirmed_at: None,
            asker: None,
            tab: None,
            failure: Some("Failing: lint".to_string()),
            pull_request: Some(pull_request.clone()),
        }];
        let mut next_id = 5;
        // After a restart `ci_status` is empty, so there is no previous status.
        let (changed, recorded) = apply_ci_status(
            &mut records,
            &pull_request,
            None,
            Some(&ci(CiStatus::PendingClean, &[])),
            "Retry on 429".to_string(),
            "zed".to_string(),
            &mut next_id,
            chrono::Utc::now(),
        );
        assert!(changed && recorded.is_none());
        assert!(records.is_empty());
    }

    #[test]
    fn stale_ci_responses_are_rejected() {
        let arena = gpui::EntityId::from(7u64);
        let key = (arena, 812);
        let mut generations = CiFetchGenerations::default();

        let older = generations.start(key);
        let newer = generations.start(key);
        assert!(!generations.is_current(key, older));
        assert!(generations.is_current(key, newer));

        // Closing the arena or switching its branch invalidates what is in
        // flight, including a request started again for the same key later.
        generations.forget_arena(arena);
        assert!(!generations.is_current(key, newer));
        let after_reset = generations.start(key);
        assert!(!generations.is_current(key, newer));
        assert!(!generations.is_current(key, older));
        assert!(generations.is_current(key, after_reset));
    }

    #[test]
    fn a_superseded_ci_request_leaves_the_pr_due_for_a_fetch() {
        let key = (gpui::EntityId::from(7u64), 812);
        let mut generations = CiFetchGenerations::default();
        let mut last_checked = HashMap::new();
        let now = std::time::Instant::now();

        // A poll batch queues the PR, then a direct fetch (arena switch)
        // starts a newer request before the queued one runs.
        let queued = generations.start(key);
        let direct = generations.start(key);
        assert!(!generations.is_current(key, queued));

        assert!(!generations.accept(key, queued, &mut last_checked, now));
        assert!(last_checked.is_empty());
        assert!(generations.accept(key, direct, &mut last_checked, now));
        assert_eq!(last_checked.get(&key), Some(&now));

        // After the arena closes, a late response does not mark it checked.
        let mut last_checked = HashMap::new();
        let in_flight = generations.start(key);
        generations.forget_arena(key.0);
        assert!(!generations.accept(key, in_flight, &mut last_checked, now));
        assert!(last_checked.is_empty());
    }

    #[test]
    fn a_todo_set_request_with_one_malformed_item_is_rejected_whole() {
        let request = serde_json::json!({
            "type": "todo_set",
            "user_tasks": [{ "title": "Check the Failed lane", "detail": null }],
            "proposals": [{ "title": "Add GPUI tests" }, { "detail": "no title" }],
        });
        assert!(parse_todo_set_request(&request).is_err());
        assert!(parse_todo_set_request(&serde_json::json!({ "user_tasks": [] })).is_err());
        assert!(
            parse_todo_set_request(&serde_json::json!({ "user_tasks": "a", "proposals": [] }))
                .is_err()
        );
        let items = parse_todo_set_request(&serde_json::json!({
            "proposals": [{ "title": " Add GPUI tests ", "detail": "  " }],
            "user_tasks": [{ "title": "Check the Failed lane", "detail": "red lane" }],
        }))
        .expect("valid request");
        assert_eq!(
            items,
            vec![
                InboxTodoItem {
                    kind: InboxTodoKind::UserTask,
                    title: "Check the Failed lane".to_string(),
                    detail: Some("red lane".to_string()),
                },
                InboxTodoItem {
                    kind: InboxTodoKind::Proposal,
                    title: "Add GPUI tests".to_string(),
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn a_list_is_not_titled_after_a_pasted_message() {
        assert_eq!(
            super::headline_from_prompt("Fix the flaky test", Some("Old".to_string())),
            Some("Fix the flaky test".to_string())
        );
        assert_eq!(
            super::headline_from_prompt("[from: Agentium Inbox]\nTo do\nGo: 1 x", Some("Old".to_string())),
            Some("Old".to_string())
        );
        assert_eq!(super::headline_from_prompt("[from: Codex]\nhello", None), None);
    }

    #[test]
    fn todo_set_reports_the_items_it_dropped() {
        let item = |kind, title: &str| InboxTodoItem {
            kind,
            title: title.to_string(),
            detail: None,
        };
        let old = vec![
            item(InboxTodoKind::UserTask, "Check the lane"),
            item(InboxTodoKind::Proposal, "Add tests"),
            item(InboxTodoKind::Proposal, "Open a PR"),
        ];
        // Only the new items, the mistake the output exists to catch.
        let new = vec![item(InboxTodoKind::Proposal, "Fix the flaky test")];
        assert_eq!(
            dropped_todo_titles(&old, &new),
            vec!["Check the lane", "Add tests", "Open a PR"]
        );
        // Reordered, or moved to the other kind: still there.
        let new = vec![
            item(InboxTodoKind::Proposal, "Open a PR"),
            item(InboxTodoKind::UserTask, "Add tests"),
            item(InboxTodoKind::UserTask, "Check the lane"),
        ];
        assert!(dropped_todo_titles(&old, &new).is_empty());
    }

    #[gpui::test]
    fn a_todo_list_is_found_by_its_tab_or_by_its_asker_after_a_restart(
        cx: &mut gpui::TestAppContext,
    ) {
        let (item, tab) = test_tab(cx);
        let (reopened, _) = test_tab(cx);
        let asker = InboxAsker::ClaudeSession {
            session_id: "c8832ec2".to_string(),
        };
        let list = |id, tab: Option<InboxTab>, asker: Option<InboxAsker>| InboxTodoList {
            id,
            title: "Fix the Inbox".to_string(),
            arena_name: "zed".to_string(),
            updated_at: chrono::Utc::now(),
            asker,
            items: Vec::new(),
            branch: None,
            arena_path: None,
            tab,
            choices: HashMap::new(),
        };
        let lists = vec![list(3, None, None), list(5, Some(tab), Some(asker.clone()))];
        assert_eq!(todo_list_index(&lists, item, None), Some(1));

        // After a restart the list is loaded without a tab.
        let json = serde_json::to_value(&lists[1]).expect("serialize");
        assert!(json.get("tab").is_none());
        let restored: InboxTodoList = serde_json::from_value(json).expect("deserialize");
        let lists = vec![restored];
        assert_eq!(todo_list_index(&lists, reopened, Some(&asker)), Some(0));
        assert_eq!(todo_list_index(&lists, reopened, None), None);

        // Ids continue after the to-do lists even with nothing else saved.
        assert_eq!(next_inbox_id(&[], &[], &lists), 6);
        assert_eq!(next_inbox_id(&[], &[], &[]), 1);
    }

    #[test]
    fn inbox_completions_written_before_the_failed_lane_still_load() {
        let older = serde_json::json!([{
            "id": 9,
            "title": "Fix the Inbox",
            "detail": "",
            "arena_name": "zed",
            "completed_at": "2026-10-14T09:12:00Z",
            "asker": null
        }]);
        let restored: Vec<InboxCompletion> = serde_json::from_value(older).expect("deserialize");
        assert!(restored[0].failure.is_none() && restored[0].pull_request.is_none());
    }

    fn task_with(title: &str, issues: Vec<IssueLink>, prs: Vec<PrRef>) -> BoardTask {
        BoardTask {
            id: Uuid::new_v4(),
            title: title.to_string(),
            issues,
            worktrees: Vec::new(),
            archived: false,
            collapsed: false,
            prs,
        }
    }

    fn backlog_issue(issue_key: &str) -> IssueLink {
        IssueLink {
            reference: IssueRef::Backlog {
                issue_key: issue_key.to_string(),
            },
            title: None,
            state: None,
            url: None,
        }
    }

    #[test]
    fn tail_lines_keeps_inner_blank_lines_and_drops_trailing_rows() {
        let content = "one  \n\ntwo\nthree\n   \n\n";
        assert_eq!(tail_lines(content, 2), "two\nthree\n");
        assert_eq!(tail_lines(content, 3), "\ntwo\nthree\n");
        assert_eq!(tail_lines(content, 0), "one\n\ntwo\nthree\n");
        assert_eq!(tail_lines(content, 100), "one\n\ntwo\nthree\n");
        assert_eq!(tail_lines("  \n\n", 5), "");
    }

    #[test]
    fn with_from_line_always_prepends_the_sender() {
        assert_eq!(with_from_line("Codex", "hi"), "[from: Codex]\nhi");
        assert_eq!(
            with_from_line("Codex (unverified)", "[from: Claude]\nhi"),
            "[from: Codex (unverified)]\n[from: Claude]\nhi"
        );
        assert_eq!(with_from_line("Codex", ""), "");
    }

    #[test]
    fn sender_label_marks_and_sanitizes_unverified_senders() {
        assert_eq!(sender_label(Some("Claude"), Some("Codex")), "Claude");
        assert_eq!(sender_label(None, Some("Codex")), "Codex (unverified)");
        assert_eq!(sender_label(None, None), "unknown (unverified)");
        assert_eq!(
            sender_label(None, Some("Claude]\n[from: Lead")),
            "Claude[from: Lead (unverified)"
        );
        assert_eq!(sender_label(Some("Lead]\u{1b}x"), None), "Leadx");
    }

    #[test]
    fn arena_agent_rows_include_idle_agents_without_title_tracking() {
        assert_eq!(
            super::arena_agent_icon(false, false, Some("codex")),
            Some(ui::IconName::AiOpenAi)
        );
        assert_eq!(
            super::arena_agent_icon(false, false, Some("claude")),
            Some(ui::IconName::AiClaude)
        );
        assert_eq!(
            super::arena_agent_icon(true, false, None),
            Some(ui::IconName::AiOpenAi)
        );
        assert_eq!(
            super::arena_agent_icon(false, true, Some("cargo")),
            Some(ui::IconName::AiClaude)
        );
    }

    #[test]
    fn arena_agent_rows_exclude_shells_and_other_running_processes() {
        for process in [None, Some("zsh"), Some("cargo"), Some("node")] {
            assert_eq!(super::arena_agent_icon(false, false, process), None);
        }
    }

    #[test]
    fn title_driven_state_follows_codex_title_transitions() {
        // A plain title alone (shell prompt, idle Codex) is not tracked.
        assert_eq!(next_title_driven_state(None, TitleState::Idle), None);
        let running = next_title_driven_state(None, TitleState::Busy);
        assert_eq!(running, Some(ClaudeSessionState::Running));
        let waiting = next_title_driven_state(running, TitleState::ActionRequired);
        assert_eq!(waiting, Some(ClaudeSessionState::WaitingPermission));
        let completed = next_title_driven_state(waiting, TitleState::Idle);
        assert_eq!(completed, Some(ClaudeSessionState::Completed));
        // Completed stays until key input clears it; a thread rename does not.
        assert_eq!(next_title_driven_state(completed, TitleState::Idle), completed);
        assert_eq!(
            next_title_driven_state(Some(ClaudeSessionState::Idle), TitleState::Idle),
            Some(ClaudeSessionState::Idle)
        );
        // Codex clears its title on exit.
        assert_eq!(next_title_driven_state(completed, TitleState::Empty), None);
    }

    #[test]
    fn pr_ref_matches_remote_compares_repository_identity() {
        let github = PrRef::GitHub {
            repo: "safx/zed".to_string(),
            number: 1,
        };
        assert!(pr_ref_matches_remote(&github, "https://github.com/safx/zed.git"));
        assert!(pr_ref_matches_remote(&github, "git@github.com:safx/zed.git"));
        assert!(pr_ref_matches_remote(&github, "ssh://git@github.com/safx/zed"));
        assert!(pr_ref_matches_remote(&github, "https://github.com/SafX/Zed"));
        assert!(!pr_ref_matches_remote(&github, "https://github.com/safx/other.git"));
        assert!(!pr_ref_matches_remote(&github, "https://gitlab.com/safx/zed.git"));

        let backlog = PrRef::Backlog {
            web_host: "nulab.backlog.jp".to_string(),
            project: "BLG_AI".to_string(),
            repo: "repo-name".to_string(),
            number: 2,
        };
        assert!(pr_ref_matches_remote(&backlog, "https://nulab.backlog.jp/git/BLG_AI/repo-name.git"));
        assert!(!pr_ref_matches_remote(&backlog, "https://nulab.backlog.jp/git/BLG_AI/other.git"));
        assert!(!pr_ref_matches_remote(&backlog, "https://github.com/safx/zed.git"));
    }

    #[test]
    fn parse_backlog_remote_accepts_known_url_forms() {
        let https = parse_backlog_remote("https://nulab.backlog.jp/git/BLG_AI/repo-name.git")
            .expect("https form");
        assert_eq!(https.web_host, "nulab.backlog.jp");
        assert_eq!(https.project, "BLG_AI");
        assert_eq!(https.repo, "repo-name");

        let scp = parse_backlog_remote("nulab@nulab.git.backlog.jp:/BLG_AI/repo-name.git")
            .expect("scp-like form");
        assert_eq!(scp.web_host, "nulab.backlog.jp");
        assert_eq!(scp.project, "BLG_AI");
        assert_eq!(scp.repo, "repo-name");

        let ssh = parse_backlog_remote("ssh://nulab@nulab.git.backlog.jp/BLG_AI/repo.git")
            .expect("ssh form");
        assert_eq!(ssh.web_host, "nulab.backlog.jp");
        assert_eq!(ssh.project, "BLG_AI");
        assert_eq!(ssh.repo, "repo");

        let without_git_suffix =
            parse_backlog_remote("https://example.backlog.com/git/PROJ/repo").expect("no .git");
        assert_eq!(without_git_suffix.web_host, "example.backlog.com");
        assert_eq!(without_git_suffix.repo, "repo");
    }

    #[test]
    fn parse_backlog_remote_rejects_non_backlog_urls() {
        assert!(parse_backlog_remote("https://github.com/safx/zed.git").is_none());
        assert!(parse_backlog_remote("git@github.com:safx/zed.git").is_none());
        // A Backlog host without the /git/ prefix is not a git remote.
        assert!(parse_backlog_remote("https://nulab.backlog.jp/view/PROJ-1").is_none());
        assert!(parse_backlog_remote("").is_none());
        assert!(parse_backlog_remote("https://nulab.backlog.jp/git/PROJ").is_none());
    }

    #[test]
    fn backlog_issue_keys_in_branch_extracts_key_segments() {
        assert_eq!(
            backlog_issue_keys_in_branch("BLG_AI-517/real-usage-queries"),
            vec!["BLG_AI-517".to_string()]
        );
        assert_eq!(
            backlog_issue_keys_in_branch("PROJ-1"),
            vec!["PROJ-1".to_string()]
        );
        assert!(backlog_issue_keys_in_branch("feature/add-thing").is_empty());
        assert!(backlog_issue_keys_in_branch("main").is_empty());
        // Lowercase project prefixes are not Backlog keys.
        assert!(backlog_issue_keys_in_branch("fix-123/things").is_empty());
        // Uppercase segments that look like keys are accepted (false positives
        // are handled by the negative cache at resolution time).
        assert_eq!(
            backlog_issue_keys_in_branch("FIX-123/things"),
            vec!["FIX-123".to_string()]
        );
    }

    #[test]
    fn task_fetch_inputs_omits_backlog_keys_when_bee_unavailable() {
        let task = task_with("t", vec![backlog_issue("PROJ-1")], Vec::new());
        let (keys, _prs) = task_fetch_inputs(&[&task], false);
        assert!(keys.is_empty());
        let (keys, _prs) = task_fetch_inputs(&[&task], true);
        assert_eq!(keys, vec!["PROJ-1".to_string()]);
    }

    #[test]
    fn task_fetch_inputs_dedupes_shared_keys_and_prs_across_tasks() {
        let pr = PrRef::GitHub {
            repo: "safx/zed".to_string(),
            number: 1,
        };
        let task_a = task_with("a", vec![backlog_issue("PROJ-1")], vec![pr.clone()]);
        let task_b = task_with("b", vec![backlog_issue("PROJ-1")], vec![pr.clone()]);
        let (keys, prs) = task_fetch_inputs(&[&task_a, &task_b], true);
        assert_eq!(keys, vec!["PROJ-1".to_string()]);
        assert_eq!(prs, vec![pr]);
    }

    #[test]
    fn task_fetch_inputs_includes_a_pr_only_the_second_shared_task_linked() {
        // Regression case for §3.6: worktree shared by task A and task B,
        // where only B manually linked a PR. Aggregating from A alone would
        // drop B's PR from the shared pr_cache/pr_info entry.
        let pr = PrRef::GitHub {
            repo: "safx/zed".to_string(),
            number: 7,
        };
        let task_a = task_with("a", Vec::new(), Vec::new());
        let task_b = task_with("b", Vec::new(), vec![pr.clone()]);
        let (_keys, prs) = task_fetch_inputs(&[&task_a, &task_b], false);
        assert_eq!(prs, vec![pr]);
    }

    #[test]
    fn tasks_sharing_arena_finds_only_tasks_live_on_that_arena_index() {
        let shared_task = Uuid::new_v4();
        let other_arena_task = Uuid::new_v4();
        let closed_task = Uuid::new_v4();
        let mut task_worktrees: HashMap<Uuid, Vec<WorktreeRow>> = HashMap::new();
        // Two tasks share arena 0 (mirrors §3.6's shared-worktree regression case).
        task_worktrees.insert(shared_task, vec![WorktreeRow::Live { arena_index: 0 }]);
        task_worktrees.insert(
            other_arena_task,
            vec![WorktreeRow::Live { arena_index: 1 }],
        );
        task_worktrees.insert(
            closed_task,
            vec![WorktreeRow::Closed {
                path: PathBuf::from("/tmp/example"),
            }],
        );
        // A second, non-shared task also lives on arena 0.
        let second_shared_task = Uuid::new_v4();
        task_worktrees.insert(second_shared_task, vec![WorktreeRow::Live { arena_index: 0 }]);

        let mut result = tasks_sharing_arena(&task_worktrees, 0);
        result.sort();
        let mut expected = vec![shared_task, second_shared_task];
        expected.sort();
        assert_eq!(result, expected);
        assert!(tasks_sharing_arena(&task_worktrees, 2).is_empty());
    }
}
