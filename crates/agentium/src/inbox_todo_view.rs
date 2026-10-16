use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context as _;
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder as _,
};
use ui::{
    ActiveTheme, Button, Clickable as _, Disableable as _, LabelSize,
    Toggleable as _, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::Pane;
use workspace::item::{Item, ItemEvent};

use crate::arena::Arena;
use crate::{
    InboxAsker, InboxTab, InboxTodoItem, InboxTodoKind, InboxTodoList, dropped_todo_titles,
    inbox_todo_color, todo_list_index, with_from_line,
};

gpui::actions!(
    agentium_inbox_todo,
    [
        /// Send the chosen answers to the tab that wrote the to-do list.
        SendAnswers
    ]
);

pub(crate) const KEY_CONTEXT: &str = "InboxTodo";

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

/// The body of the answer, numbered as `todo list` numbers the items, or
/// `None` when nothing is chosen. Each line repeats the title because the
/// agent may have replaced its list (and so its numbering) since.
pub(crate) fn todo_answer_message(
    items: &[InboxTodoItem],
    choices: &HashMap<String, TodoChoice>,
    notes: &HashMap<String, String>,
) -> Option<String> {
    let mut lines = vec!["To do".to_string()];
    for (index, item) in items.iter().enumerate() {
        let Some(choice) = choices.get(&item.title) else {
            continue;
        };
        let mut line = format!("{}: {} {}", choice.label(), index + 1, item.title);
        if let Some(note) = notes
            .get(&item.title)
            .map(|note| note.trim())
            .filter(|note| !note.is_empty())
        {
            line.push_str(" — Note: ");
            line.push_str(note);
        }
        lines.push(line);
    }
    (lines.len() > 1).then(|| lines.join("\n"))
}

/// Choices that still apply after the agent replaced its items: same title,
/// and a kind the choice is valid for.
pub(crate) fn carry_todo_choices(
    choices: &HashMap<String, TodoChoice>,
    items: &[InboxTodoItem],
) -> HashMap<String, TodoChoice> {
    choices
        .iter()
        .filter(|(title, choice)| {
            items
                .iter()
                .any(|item| &item.title == *title && choice.allowed_for(item.kind))
        })
        .map(|(title, choice)| (title.clone(), *choice))
        .collect()
}

pub(crate) fn todo_items_after_send(
    items: &[InboxTodoItem],
    choices: &HashMap<String, TodoChoice>,
) -> Vec<InboxTodoItem> {
    items
        .iter()
        .filter(|item| !choices.contains_key(&item.title))
        .cloned()
        .collect()
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

#[derive(Debug, PartialEq)]
pub(crate) enum DetailPane {
    Existing(Entity<Pane>),
    SplitLeftOf(Entity<Pane>),
}

/// A pane for the detail tab that keeps the owner tab visible beside it.
pub(crate) fn detail_pane_for(
    owner_pane: &Entity<Pane>,
    panes: &[Entity<Pane>],
    last_active: Option<&Entity<Pane>>,
) -> DetailPane {
    last_active
        .filter(|pane| *pane != owner_pane && panes.contains(pane))
        .or_else(|| panes.iter().find(|pane| *pane != owner_pane))
        .map_or_else(
            || DetailPane::SplitLeftOf(owner_pane.clone()),
            |pane| DetailPane::Existing(pane.clone()),
        )
}

/// Puts the detail tab in the owner's arena, in a pane other than the owner's,
/// with nothing zoomed, and brings both to the front: the point of the tab
/// is to answer while looking at the agent that asked. An existing detail tab
/// is moved rather than recreated when it is elsewhere (another arena, or the
/// owner's own pane after a drag).
pub(crate) fn show_detail_beside_owner(
    detail: &Entity<InboxTodoView>,
    owner_arena: &Entity<Arena>,
    owner_pane: &Entity<Pane>,
    owner_item: &dyn workspace::ItemHandle,
    arenas: &[Entity<Arena>],
    window: &mut Window,
    cx: &mut App,
) {
    owner_arena.update(cx, |arena, cx| {
        if let Some(zoomed) = arena
            .zoomed_pane
            .take()
            .and_then(|view| view.upgrade())
            .and_then(|view| view.downcast::<Pane>().ok())
        {
            zoomed.update(cx, |pane, cx| pane.set_zoomed(false, cx));
        }
        cx.notify();
    });

    let detail_id = detail.entity_id();
    let current = find_item_location(arenas, detail_id, cx);
    let in_place = current
        .as_ref()
        .is_some_and(|(arena, pane, _)| arena == owner_arena && pane != owner_pane);
    let detail_pane = if in_place {
        current.map(|(_, pane, _)| pane)
    } else {
        if let Some((_, pane, _)) = current {
            pane.update(cx, |pane, cx| {
                pane.remove_item(detail_id, false, true, window, cx)
            });
        }
        let (panes, active_pane) = {
            let arena = owner_arena.read(cx);
            (
                arena.center.panes().into_iter().cloned().collect::<Vec<_>>(),
                arena.active_pane.clone(),
            )
        };
        match detail_pane_for(owner_pane, &panes, Some(&active_pane)) {
            DetailPane::Existing(pane) => {
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(detail.clone()), true, true, None, window, cx);
                });
                Some(pane)
            }
            DetailPane::SplitLeftOf(pane) => Some(owner_arena.update(cx, |arena, cx| {
                arena.split_pane_with_item(
                    &pane,
                    Box::new(detail.clone()),
                    workspace::SplitDirection::Left,
                    window,
                    cx,
                )
            })),
        }
    };

    owner_pane.update(cx, |pane, cx| {
        if let Some(index) = pane.index_for_item(owner_item) {
            pane.activate_item(index, false, false, window, cx);
        }
    });
    if let Some(detail_pane) = detail_pane {
        detail_pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(detail) {
                pane.activate_item(index, true, true, window, cx);
            }
        });
    }
}

/// What a list records about its owner tab at `todo set`.
pub(crate) struct TodoOwner {
    pub(crate) title: String,
    pub(crate) arena_name: String,
    pub(crate) arena_path: Option<PathBuf>,
    pub(crate) branch: Option<String>,
}

/// How to bring back the tab of a list that is not bound to an open tab.
#[derive(Debug, PartialEq)]
pub(crate) enum RestorePlan {
    ResumeClaude { arena_path: PathBuf, session_id: String },
    OpenArena { arena_path: PathBuf },
    Fallback(String),
}

/// Lists written before `arena_path` existed only know the arena's display
/// name, which several worktrees can share; guessing would resume the agent
/// in the wrong directory, so the name must match exactly one open arena.
pub(crate) fn todo_restore_plan(
    list: &InboxTodoList,
    open_arenas: &[(String, PathBuf)],
) -> RestorePlan {
    let Some(asker) = &list.asker else {
        return RestorePlan::Fallback(
            "This tab cannot be restored; open it again to answer.".to_string(),
        );
    };
    let arena_path = match &list.arena_path {
        Some(path) => path.clone(),
        None => {
            let mut matches = open_arenas
                .iter()
                .filter(|(name, _)| *name == list.arena_name);
            match (matches.next(), matches.next()) {
                (Some((_, path)), None) => path.clone(),
                _ => {
                    return RestorePlan::Fallback(format!(
                        "Cannot tell which arena '{}' is; open the tab manually.",
                        list.arena_name
                    ));
                }
            }
        }
    };
    match asker {
        InboxAsker::ClaudeSession { session_id } => RestorePlan::ResumeClaude {
            arena_path,
            session_id: session_id.clone(),
        },
        InboxAsker::CodexAcp { .. } => RestorePlan::OpenArena { arena_path },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingRestore {
    /// `claude --resume` is being started.
    Launching,
    /// Started in this tab; waiting for its session to reconnect.
    Launched { item_id: EntityId },
    /// The tab exists again (ACP, restored with its arena); binds at the next sync.
    Restored,
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
                *entry = PendingRestore::Launched { item_id };
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
) -> Result<PendingRestore, String> {
    match (arena_found, asker_tab_found) {
        (false, _) => Err("Could not open the arena of this tab.".to_string()),
        (true, false) => Err("This Codex ACP tab is no longer saved in its arena.".to_string()),
        (true, true) => Ok(PendingRestore::Restored),
    }
}

pub(crate) struct RestoreObservation {
    pub(crate) list_exists: bool,
    pub(crate) list_bound: bool,
    pub(crate) launched_tab_open: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum RestoreOutcome {
    Open,
    Failed(String),
}

/// Settles pending restorations at a sync: a bound list opens its detail
/// tab, a vanished list is forgotten, and a launched tab that closed before
/// its session reconnected fails so that the next click retries.
pub(crate) fn advance_pending_restores(
    pending: &mut HashMap<u64, PendingRestore>,
    observe: impl Fn(u64, &PendingRestore) -> RestoreObservation,
) -> Vec<(u64, RestoreOutcome)> {
    let mut outcomes = Vec::new();
    pending.retain(|todo_id, restore| {
        let observation = observe(*todo_id, restore);
        if !observation.list_exists {
            return false;
        }
        if observation.list_bound {
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

/// Every tab's to-do list. An entity of its own so detail tabs can observe
/// it, and so tests can use it without `AgentiumApp` (whose construction
/// reads and writes the user's data_dir).
pub(crate) struct InboxTodos {
    lists: Vec<InboxTodoList>,
    path: Option<PathBuf>,
    write_task: Option<Task<()>>,
}

impl InboxTodos {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        let lists = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok().map(|content| (path, content)))
            .and_then(|(path, content)| {
                serde_json::from_slice(&content)
                    .with_context(|| format!("Failed to parse {}", path.display()))
                    .log_err()
            })
            .unwrap_or_default();
        Self {
            lists,
            path,
            write_task: None,
        }
    }

    pub(crate) fn lists(&self) -> &[InboxTodoList] {
        &self.lists
    }

    pub(crate) fn list(&self, todo_id: u64) -> Option<&InboxTodoList> {
        self.lists.iter().find(|list| list.id == todo_id)
    }

    /// Replaces the items of the list owned by `item_id` (or by `asker`),
    /// creating it if needed, and returns the titles that disappeared.
    pub(crate) fn set_items(
        &mut self,
        item_id: EntityId,
        tab: InboxTab,
        asker: Option<InboxAsker>,
        owner: TodoOwner,
        items: Vec<InboxTodoItem>,
        next_id: &mut u64,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let dropped = match todo_list_index(&self.lists, item_id, asker.as_ref()) {
            Some(index) => {
                let list = &mut self.lists[index];
                let dropped = dropped_todo_titles(&list.items, &items);
                list.choices = carry_todo_choices(&list.choices, &items);
                list.items = items;
                list.title = owner.title;
                list.arena_name = owner.arena_name;
                list.arena_path = owner.arena_path;
                list.branch = owner.branch;
                list.updated_at = chrono::Utc::now();
                list.asker = asker;
                list.tab = Some(tab);
                dropped
            }
            None => {
                let id = *next_id;
                *next_id += 1;
                self.lists.push(InboxTodoList {
                    id,
                    title: owner.title,
                    arena_name: owner.arena_name,
                    arena_path: owner.arena_path,
                    branch: owner.branch,
                    updated_at: chrono::Utc::now(),
                    asker,
                    items,
                    tab: Some(tab),
                    choices: HashMap::new(),
                });
                Vec::new()
            }
        };
        self.persist(cx);
        cx.notify();
        dropped
    }

    /// Removes the list owned by `item_id` (or by `asker`); returns how many
    /// items it had.
    pub(crate) fn clear_for(
        &mut self,
        item_id: EntityId,
        asker: Option<&InboxAsker>,
        cx: &mut Context<Self>,
    ) -> usize {
        let Some(index) = todo_list_index(&self.lists, item_id, asker) else {
            return 0;
        };
        let list = self.lists.remove(index);
        self.persist(cx);
        cx.notify();
        list.items.len()
    }

    pub(crate) fn dismiss(&mut self, todo_id: u64, cx: &mut Context<Self>) {
        let before = self.lists.len();
        self.lists.retain(|list| list.id != todo_id);
        if self.lists.len() != before {
            self.persist(cx);
            cx.notify();
        }
    }

    /// `None` clears the choice. A choice the item's kind does not allow is
    /// ignored.
    pub(crate) fn set_choice(
        &mut self,
        todo_id: u64,
        title: &str,
        choice: Option<TodoChoice>,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.lists.iter_mut().find(|list| list.id == todo_id) else {
            return;
        };
        let Some(item) = list.items.iter().find(|item| item.title == title) else {
            return;
        };
        match choice {
            Some(choice) if choice.allowed_for(item.kind) => {
                list.choices.insert(title.to_string(), choice);
            }
            Some(_) => return,
            None => {
                list.choices.remove(title);
            }
        }
        cx.notify();
    }

    pub(crate) fn clear_choices(&mut self, todo_id: u64, cx: &mut Context<Self>) {
        if let Some(list) = self.lists.iter_mut().find(|list| list.id == todo_id)
            && !list.choices.is_empty()
        {
            list.choices.clear();
            cx.notify();
        }
    }

    /// Applies the owner tabs the app found at a sync. Runs on every app
    /// render, so it notifies only when a list's owner actually changed;
    /// notifying unconditionally would re-render the app forever.
    pub(crate) fn rebind(
        &mut self,
        bindings: Vec<(u64, Option<InboxTab>)>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for (todo_id, tab) in bindings {
            let Some(list) = self.lists.iter_mut().find(|list| list.id == todo_id) else {
                continue;
            };
            let current = list.tab.as_ref().map(|tab| tab.item.id());
            if current != tab.as_ref().map(|tab| tab.item.id()) {
                list.tab = tab;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Delivers the chosen answers to `owner` in one message, then removes
    /// exactly the answered items. Nothing changes when there is no owner or
    /// delivery fails, so the user can retry.
    pub(crate) fn send(
        &mut self,
        todo_id: u64,
        notes: &HashMap<String, String>,
        owner: Option<&dyn workspace::ItemHandle>,
        deliver: impl FnOnce(&dyn workspace::ItemHandle, &str, &mut App) -> anyhow::Result<()>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let list = self
            .list(todo_id)
            .context("this to-do list no longer exists")?;
        let body = todo_answer_message(&list.items, &list.choices, notes)
            .context("choose Done, Go or Skip for at least one item")?;
        let owner = owner.context("the tab that wrote this list is not open")?;
        deliver(owner, &with_from_line("Agentium Inbox", &body), cx)?;
        if let Some(index) = self.lists.iter().position(|list| list.id == todo_id) {
            let list = &mut self.lists[index];
            list.items = todo_items_after_send(&list.items, &list.choices);
            list.choices.clear();
            if list.items.is_empty() {
                self.lists.remove(index);
            }
        }
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    /// Lists of Codex terminal tabs have no asker to find them again after a
    /// restart, so only lists with one are written, as for questions.
    fn persist(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let previous = self.write_task.take();
        let snapshot = serde_json::to_vec_pretty(
            &self
                .lists
                .iter()
                .filter(|list| list.asker.is_some())
                .collect::<Vec<_>>(),
        );
        self.write_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            snapshot
                .map_err(anyhow::Error::from)
                .and_then(|content| crate::write_file_atomically(&path, &content))
                .log_err();
        }));
    }
}

pub(crate) enum InboxTodoViewEvent {
    Send {
        todo_id: u64,
        notes: HashMap<String, String>,
    },
}

/// The detail tab of one to-do list: choose Done / Go / Skip per item and
/// send the answers to the tab that wrote the list.
pub(crate) struct InboxTodoView {
    todos: Entity<InboxTodos>,
    todo_id: u64,
    note_editors: HashMap<String, Entity<Editor>>,
    error: Option<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl InboxTodoView {
    pub(crate) fn new(
        todos: Entity<InboxTodos>,
        todo_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_in(&todos, window, |this, _, window, cx| {
            this.reconcile(window, cx);
        });
        let mut this = Self {
            todos,
            todo_id,
            note_editors: HashMap::new(),
            error: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![subscription],
        };
        this.reconcile(window, cx);
        this
    }

    /// The tab that wrote the list, as the app last found it in a pane.
    pub(crate) fn owner_item_id(&self, cx: &App) -> Option<EntityId> {
        self.todos
            .read(cx)
            .list(self.todo_id)?
            .tab
            .as_ref()?
            .item
            .upgrade()
            .map(|item| item.item_id())
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    /// Keeps one note editor per chosen item, so a replaced list keeps the
    /// notes of the items that survived.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen: Vec<String> = self
            .todos
            .read(cx)
            .list(self.todo_id)
            .map(|list| list.choices.keys().cloned().collect())
            .unwrap_or_default();
        self.note_editors.retain(|title, _| chosen.contains(title));
        for title in chosen {
            if self.note_editors.contains_key(&title) {
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
            self.note_editors.insert(title, editor);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn notes(&self, cx: &App) -> HashMap<String, String> {
        self.note_editors
            .iter()
            .map(|(title, editor)| (title.clone(), editor.read(cx).text(cx)))
            .collect()
    }

    pub(crate) fn preview(&self, cx: &App) -> Option<String> {
        let list = self.todos.read(cx).list(self.todo_id)?;
        todo_answer_message(&list.items, &list.choices, &self.notes(cx))
    }

    fn toggle(&mut self, title: String, choice: TodoChoice, cx: &mut Context<Self>) {
        let current = self
            .todos
            .read(cx)
            .list(self.todo_id)
            .and_then(|list| list.choices.get(&title).copied());
        let next = (current != Some(choice)).then_some(choice);
        self.todos
            .update(cx, |todos, cx| todos.set_choice(self.todo_id, &title, next, cx));
    }

    /// The button is disabled when this does not hold; the keyboard shortcut
    /// says why instead.
    fn send_problem(&self, cx: &App) -> Option<&'static str> {
        if self.owner_item_id(cx).is_none() {
            Some("The tab that wrote this list is not open.")
        } else if self.preview(cx).is_none() {
            Some("Choose Done, Go or Skip for at least one item.")
        } else {
            None
        }
    }

    fn send_answers(&mut self, _: &SendAnswers, _window: &mut Window, cx: &mut Context<Self>) {
        match self.send_problem(cx) {
            Some(problem) => self.set_error(Some(problem.to_string()), cx),
            None => self.send(cx),
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.emit(InboxTodoViewEvent::Send {
            todo_id: self.todo_id,
            notes: self.notes(cx),
        });
        cx.notify();
    }

    fn render_item(
        &self,
        number: usize,
        item: &InboxTodoItem,
        choice: Option<TodoChoice>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let todo_color = inbox_todo_color();
        let choices: &[TodoChoice] = match item.kind {
            InboxTodoKind::UserTask => &[TodoChoice::Done],
            InboxTodoKind::Proposal => &[TodoChoice::Go, TodoChoice::Skip],
        };
        let buttons = choices.iter().map(|option| {
            let title = item.title.clone();
            let option = *option;
            Button::new(
                SharedString::from(format!("todo-{number}-{}", option.label())),
                option.label(),
            )
            .label_size(LabelSize::Small)
            .toggle_state(choice == Some(option))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.toggle(title.clone(), option, cx);
            }))
        });
        v_flex()
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
                            .child(number.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(item.title.clone()),
                    )
                    .children(buttons),
            )
            .when_some(item.detail.clone(), |d, detail| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(colors.text_muted)
                        .child(detail),
                )
            })
            .when_some(
                choice.and_then(|_| self.note_editors.get(&item.title).cloned()),
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
}

impl EventEmitter<InboxTodoViewEvent> for InboxTodoView {}
impl EventEmitter<ItemEvent> for InboxTodoView {}

impl Focusable for InboxTodoView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InboxTodoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let todos = self.todos.read(cx);
        let Some(list) = todos.list(self.todo_id) else {
            return v_flex()
                .id("inbox-todo")
                .size_full()
                .p_4()
                .track_focus(&self.focus_handle)
                .bg(colors.editor_background)
                .child(
                    div()
                        .text_color(colors.text_muted)
                        .child("Nothing left on this to-do list."),
                )
                .into_any_element();
        };
        let title = list.title.clone();
        let location = match &list.branch {
            Some(branch) => format!("{} · {branch}", list.arena_name),
            None => list.arena_name.clone(),
        };
        let updated_at = list
            .updated_at
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        let items: Vec<(usize, InboxTodoItem, Option<TodoChoice>)> = list
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (index + 1, item.clone(), list.choices.get(&item.title).copied()))
            .collect();
        let owner_open = self.owner_item_id(cx).is_some();
        let preview = self.preview(cx);
        let can_send = self.send_problem(cx).is_none();
        let has_choices = !list.choices.is_empty();

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
        let user_tasks: Vec<_> = items
            .iter()
            .filter(|(_, item, _)| item.kind == InboxTodoKind::UserTask)
            .map(|(number, item, choice)| self.render_item(*number, item, *choice, cx).into_any_element())
            .collect();
        let proposals: Vec<_> = items
            .iter()
            .filter(|(_, item, _)| item.kind == InboxTodoKind::Proposal)
            .map(|(number, item, choice)| self.render_item(*number, item, *choice, cx).into_any_element())
            .collect();
        let error = self.error.clone().or_else(|| {
            (!owner_open).then(|| "The tab that wrote this list is not open.".to_string())
        });

        v_flex()
            .id("inbox-todo")
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::send_answers))
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(inbox_todo_color())
                            .bg(inbox_todo_color().opacity(0.15))
                            .child("To do"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child(format!("{location} · updated {updated_at}")),
                    ),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(!user_tasks.is_empty(), |d| {
                d.child(group("You do", "Done when finished"))
                    .children(user_tasks)
            })
            .when(!proposals.is_empty(), |d| {
                d.child(group("Pick", "Go to have the agent do it, Skip to drop it"))
                    .children(proposals)
            })
            .child(
                v_flex()
                    .mt_3()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child("Message"),
                    )
                    .child(
                        div()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border_variant)
                            .font_family("monospace")
                            .text_sm()
                            .text_color(colors.text_muted)
                            .whitespace_normal()
                            .child(preview.unwrap_or_else(|| "Nothing chosen yet.".to_string())),
                    )
                    .when_some(error, |d, error| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().status().error)
                                .child(error),
                        )
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
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        let todo_id = this.todo_id;
                                        this.todos
                                            .update(cx, |todos, cx| todos.clear_choices(todo_id, cx));
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.text_placeholder)
                                    .child("cmd-enter sends. Answered items leave the list; the rest stay."),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Item for InboxTodoView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.todos.read(cx).list(self.todo_id) {
            Some(list) if !list.title.is_empty() => {
                format!("To do · {}", crate::truncate_for_menu(&list.title)).into()
            }
            _ => "To do".into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that would pull gpui's `test` attribute macro into
    // scope and make `#[test]` expand recursively.
    use super::{
        DetailPane, InboxTodoView, InboxTodoViewEvent, InboxTodos, PendingRestore,
        RestoreObservation, RestoreOutcome, RestorePlan, SendAnswers, TodoChoice,
        acp_restore_outcome, advance_pending_restores, carry_todo_choices, detail_pane_for,
        find_item_location, record_restore_launch, show_detail_beside_owner,
        todo_answer_message, todo_items_after_send, todo_restore_plan,
    };
    use crate::{InboxAsker, InboxTodoList};
    use std::path::PathBuf;
    use crate::arena::Arena;
    use crate::{InboxTab, InboxTodoItem, InboxTodoKind};
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use workspace::ItemHandle as _;
    use workspace::item::test::TestItem;

    fn item(kind: InboxTodoKind, title: &str) -> InboxTodoItem {
        InboxTodoItem {
            kind,
            title: title.to_string(),
            detail: None,
        }
    }

    fn test_owner() -> super::TodoOwner {
        super::TodoOwner {
            title: "Fix the Inbox".to_string(),
            arena_name: "zed".to_string(),
            arena_path: None,
            branch: None,
        }
    }

    fn sample_items() -> Vec<InboxTodoItem> {
        vec![
            item(InboxTodoKind::UserTask, "Check the lane"),
            item(InboxTodoKind::Proposal, "Add tests"),
            item(InboxTodoKind::Proposal, "Open a PR"),
        ]
    }

    #[test]
    fn the_answer_numbers_items_like_todo_list_and_repeats_titles() {
        let items = sample_items();
        let choices = HashMap::from([
            ("Check the lane".to_string(), TodoChoice::Done),
            ("Open a PR".to_string(), TodoChoice::Skip),
        ]);
        let notes = HashMap::from([
            ("Open a PR".to_string(), "  not yet ".to_string()),
            ("Check the lane".to_string(), "   ".to_string()),
        ]);
        assert_eq!(
            todo_answer_message(&items, &choices, &notes).as_deref(),
            Some("To do\nDone: 1 Check the lane\nSkip: 3 Open a PR — Note: not yet")
        );
        assert_eq!(todo_answer_message(&items, &HashMap::new(), &notes), None);
    }

    #[test]
    fn choices_survive_a_replacement_only_where_they_still_apply() {
        let choices = HashMap::from([
            ("Check the lane".to_string(), TodoChoice::Done),
            ("Add tests".to_string(), TodoChoice::Go),
            ("Open a PR".to_string(), TodoChoice::Skip),
        ]);
        let replaced = vec![
            // Now a proposal: Done no longer fits.
            item(InboxTodoKind::Proposal, "Check the lane"),
            item(InboxTodoKind::Proposal, "Add tests"),
            item(InboxTodoKind::UserTask, "Link the skill"),
        ];
        assert_eq!(
            carry_todo_choices(&choices, &replaced),
            HashMap::from([("Add tests".to_string(), TodoChoice::Go)])
        );
        assert_eq!(
            todo_items_after_send(&sample_items(), &choices),
            Vec::<InboxTodoItem>::new()
        );
        let titles: Vec<String> = todo_items_after_send(
            &sample_items(),
            &HashMap::from([("Add tests".to_string(), TodoChoice::Go)]),
        )
        .into_iter()
        .map(|item| item.title)
        .collect();
        assert_eq!(titles, vec!["Check the lane", "Open a PR"]);
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

    fn owner_tab(arena: &Entity<Arena>, pane: &Entity<workspace::Pane>, owner: &Entity<TestItem>) -> InboxTab {
        InboxTab {
            arena: arena.downgrade(),
            pane: pane.downgrade(),
            item: owner.downgrade_item(),
        }
    }

    fn todos_with_list(
        tab: InboxTab,
        item_id: gpui::EntityId,
        cx: &mut VisualTestContext,
    ) -> (Entity<InboxTodos>, u64) {
        let todos = cx.new(|_| InboxTodos::new(None));
        let mut next_id = 1;
        todos.update(cx, |todos, cx| {
            todos.set_items(
                item_id,
                tab,
                None,
                test_owner(),
                sample_items(),
                &mut next_id,
                cx,
            );
        });
        (todos, 1)
    }

    fn notifications(
        todos: &Entity<InboxTodos>,
        cx: &mut VisualTestContext,
    ) -> (Rc<Cell<usize>>, gpui::Subscription) {
        let count = Rc::new(Cell::new(0));
        let subscription = cx.update({
            let count = count.clone();
            |_, cx| cx.observe(todos, move |_, _| count.set(count.get() + 1))
        });
        (count, subscription)
    }

    #[gpui::test]
    async fn the_owner_is_found_where_it_moved_and_lost_when_closed(cx: &mut TestAppContext) {
        let (arena, left, right, owner, cx) = arena_with_owner(cx).await;
        let arenas = vec![arena];
        let owner_id = owner.entity_id();
        let found = cx.update(|_, cx| find_item_location(&arenas, owner_id, cx).map(|(_, pane, _)| pane));
        assert_eq!(found.as_ref(), Some(&left));
        assert_eq!(
            detail_pane_for(&left, &[left.clone(), right.clone()], Some(&left)),
            DetailPane::Existing(right.clone())
        );
        assert_eq!(
            detail_pane_for(&left, std::slice::from_ref(&left), None),
            DetailPane::SplitLeftOf(left.clone())
        );

        // Moved: both panes stay open, the stored pane is now wrong.
        left.update_in(cx, |pane, window, cx| pane.remove_item(owner_id, false, false, window, cx));
        right.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(owner.clone()), true, true, None, window, cx);
        });
        let found = cx.update(|_, cx| find_item_location(&arenas, owner_id, cx).map(|(_, pane, _)| pane));
        assert_eq!(found.as_ref(), Some(&right));
        assert_eq!(
            detail_pane_for(&right, &[left.clone(), right.clone()], Some(&right)),
            DetailPane::Existing(left.clone())
        );

        // Closed while this test still holds the entity: not open any more.
        right.update_in(cx, |pane, window, cx| pane.remove_item(owner_id, false, false, window, cx));
        assert!(cx.update(|_, cx| find_item_location(&arenas, owner_id, cx)).is_none());
    }

    #[gpui::test]
    async fn rebinding_notifies_only_on_a_real_change(cx: &mut TestAppContext) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let (count, _subscription) = notifications(&todos, cx);

        todos.update(cx, |todos, cx| {
            todos.rebind(vec![(todo_id, Some(owner_tab(&arena, &left, &owner)))], cx)
        });
        cx.run_until_parked();
        assert_eq!(count.get(), 0);

        todos.update(cx, |todos, cx| todos.rebind(vec![(todo_id, None)], cx));
        todos.update(cx, |todos, cx| todos.rebind(vec![(todo_id, None)], cx));
        cx.run_until_parked();
        assert_eq!(count.get(), 1);

        todos.update(cx, |todos, cx| {
            todos.rebind(vec![(todo_id, Some(owner_tab(&arena, &left, &owner)))], cx)
        });
        cx.run_until_parked();
        assert_eq!(count.get(), 2);
    }

    #[gpui::test]
    async fn sending_removes_only_what_was_delivered(cx: &mut TestAppContext) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        todos.update(cx, |todos, cx| {
            todos.set_choice(todo_id, "Check the lane", Some(TodoChoice::Done), cx);
            todos.set_choice(todo_id, "Add tests", Some(TodoChoice::Go), cx);
            // Done does not fit a proposal: ignored.
            todos.set_choice(todo_id, "Open a PR", Some(TodoChoice::Done), cx);
        });
        let notes = HashMap::from([("Add tests".to_string(), "with the Failed lane".to_string())]);
        let owner_handle: Box<dyn workspace::ItemHandle> = Box::new(owner);

        // No owner open: delivery is never attempted.
        let delivered = Rc::new(Cell::new(false));
        let result = todos.update(cx, |todos, cx| {
            let delivered = delivered.clone();
            todos.send(todo_id, &notes, None, move |_, _, _| {
                delivered.set(true);
                Ok(())
            }, cx)
        });
        assert!(result.is_err() && !delivered.get());

        // Delivery rejected: nothing changes, so the user can retry.
        let result = todos.update(cx, |todos, cx| {
            todos.send(todo_id, &notes, Some(owner_handle.as_ref()), |_, _, _| {
                anyhow::bail!("tab is busy")
            }, cx)
        });
        assert!(result.is_err());
        todos.read_with(cx, |todos, _| {
            let list = todos.list(todo_id).expect("list");
            assert_eq!(list.items.len(), 3);
            assert_eq!(list.choices.len(), 2);
        });

        let message = Rc::new(std::cell::RefCell::new(String::new()));
        todos
            .update(cx, |todos, cx| {
                let message = message.clone();
                todos.send(todo_id, &notes, Some(owner_handle.as_ref()), move |_, text, _| {
                    *message.borrow_mut() = text.to_string();
                    Ok(())
                }, cx)
            })
            .expect("delivered");
        assert_eq!(
            message.borrow().as_str(),
            "[from: Agentium Inbox]\nTo do\nDone: 1 Check the lane\nGo: 2 Add tests — Note: with the Failed lane"
        );
        todos.read_with(cx, |todos, _| {
            let list = todos.list(todo_id).expect("list");
            let titles: Vec<&str> = list.items.iter().map(|item| item.title.as_str()).collect();
            assert_eq!(titles, vec!["Open a PR"]);
            assert!(list.choices.is_empty());
        });

        todos.update(cx, |todos, cx| todos.set_choice(todo_id, "Open a PR", Some(TodoChoice::Skip), cx));
        todos
            .update(cx, |todos, cx| {
                todos.send(todo_id, &HashMap::new(), Some(owner_handle.as_ref()), |_, _, _| Ok(()), cx)
            })
            .expect("delivered");
        assert!(todos.read_with(cx, |todos, _| todos.list(todo_id).is_none()));
    }

    #[gpui::test]
    async fn the_detail_tab_follows_the_list(cx: &mut TestAppContext) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let view = cx.new_window_entity(|window, cx| InboxTodoView::new(todos.clone(), todo_id, window, cx));
        assert_eq!(view.read_with(cx, |view, cx| view.owner_item_id(cx)), Some(owner.entity_id()));

        todos.update(cx, |todos, cx| {
            todos.set_choice(todo_id, "Add tests", Some(TodoChoice::Go), cx);
            todos.set_choice(todo_id, "Open a PR", Some(TodoChoice::Skip), cx);
        });
        cx.run_until_parked();
        let editor = view.read_with(cx, |view, _| view.note_editors.get("Add tests").cloned()).expect("note editor");
        assert!(view.read_with(cx, |view, _| view.note_editors.contains_key("Open a PR")));

        // Typing a note re-renders the preview.
        let rendered = Rc::new(Cell::new(0));
        let _observer = cx.update({
            let rendered = rendered.clone();
            |_, cx| cx.observe(&view, move |_, _| rendered.set(rendered.get() + 1))
        });
        editor.update_in(cx, |editor, window, cx| editor.set_text("with the Failed lane", window, cx));
        cx.run_until_parked();
        assert!(rendered.get() > 0);
        assert!(view
            .read_with(cx, |view, cx| view.preview(cx))
            .is_some_and(|preview| preview.contains("Go: 2 Add tests — Note: with the Failed lane")));

        // The agent replaces its list: the surviving item keeps its note.
        let mut next_id = 10;
        todos.update(cx, |todos, cx| {
            todos.set_items(
                owner.entity_id(),
                owner_tab(&arena, &left, &owner),
                None,
                test_owner(),
                vec![item(InboxTodoKind::Proposal, "Add tests")],
                &mut next_id,
                cx,
            );
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.note_editors.get("Add tests"), Some(&editor));
            assert!(!view.note_editors.contains_key("Open a PR"));
        });

        // Owner unbound, then back.
        todos.update(cx, |todos, cx| todos.rebind(vec![(todo_id, None)], cx));
        assert_eq!(view.read_with(cx, |view, cx| view.owner_item_id(cx)), None);
        todos.update(cx, |todos, cx| {
            todos.rebind(vec![(todo_id, Some(owner_tab(&arena, &left, &owner)))], cx)
        });
        assert_eq!(view.read_with(cx, |view, cx| view.owner_item_id(cx)), Some(owner.entity_id()));

        // Dismissed from the sidebar while open.
        todos.update(cx, |todos, cx| todos.dismiss(todo_id, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.preview(cx).is_none());
            assert!(view.owner_item_id(cx).is_none());
            assert!(view.note_editors.is_empty());
        });
    }

    #[gpui::test]
    async fn clearing_a_list_while_its_tab_is_open_empties_the_tab(cx: &mut TestAppContext) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let view = cx.new_window_entity(|window, cx| InboxTodoView::new(todos.clone(), todo_id, window, cx));
        todos.update(cx, |todos, cx| todos.set_choice(todo_id, "Add tests", Some(TodoChoice::Go), cx));
        cx.run_until_parked();
        let cleared = todos.update(cx, |todos, cx| todos.clear_for(owner.entity_id(), None, cx));
        assert_eq!(cleared, 3);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.preview(cx).is_none());
            assert!(view.note_editors.is_empty());
        });
    }

    fn pane_of(
        arenas: &[Entity<Arena>],
        item_id: gpui::EntityId,
        cx: &mut VisualTestContext,
    ) -> Option<(Entity<Arena>, Entity<workspace::Pane>)> {
        cx.update(|_, cx| find_item_location(arenas, item_id, cx).map(|(arena, pane, _)| (arena, pane)))
    }

    fn show(
        view: &Entity<InboxTodoView>,
        arena: &Entity<Arena>,
        owner: &Entity<TestItem>,
        arenas: &[Entity<Arena>],
        cx: &mut VisualTestContext,
    ) {
        let (_, owner_pane) = pane_of(arenas, owner.entity_id(), cx).expect("owner is open");
        cx.update(|window, cx| {
            show_detail_beside_owner(view, arena, &owner_pane, owner, arenas, window, cx)
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_detail_tab_moves_beside_an_owner_restored_in_another_arena(
        cx: &mut TestAppContext,
    ) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let project = arena.read_with(cx, |arena, _| arena.project.clone());
        let other_arena = cx.new_window_entity(|window, cx| {
            Arena::test_empty("Other".into(), project, "/other".into(), window, cx)
        });
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let view = cx.new_window_entity(|window, cx| InboxTodoView::new(todos, todo_id, window, cx));
        // Opened while the owner was not open: it sits in the other arena.
        let other_pane = other_arena.read_with(cx, |arena, _| arena.active_pane.clone());
        other_pane.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
        });
        let arenas = vec![arena.clone(), other_arena];

        show(&view, &arena, &owner, &arenas, cx);
        let (detail_arena, detail_pane) = pane_of(&arenas, view.entity_id(), cx).expect("detail open");
        let (_, owner_pane) = pane_of(&arenas, owner.entity_id(), cx).expect("owner open");
        assert_eq!(detail_arena, arena);
        assert_ne!(detail_pane, owner_pane);
        assert!(other_pane.read_with(cx, |pane, _| pane.index_for_item(&view).is_none()));
    }

    #[gpui::test]
    async fn the_detail_tab_leaves_the_owners_pane(cx: &mut TestAppContext) {
        let (arena, left, right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let view = cx.new_window_entity(|window, cx| InboxTodoView::new(todos, todo_id, window, cx));
        // The user dragged the owner next to the detail, hiding one of them.
        left.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
        });
        let arenas = vec![arena.clone()];

        show(&view, &arena, &owner, &arenas, cx);
        let (_, detail_pane) = pane_of(&arenas, view.entity_id(), cx).expect("detail open");
        assert_eq!(detail_pane, right);
        let front = |pane: &Entity<workspace::Pane>, cx: &mut VisualTestContext| {
            pane.read_with(cx, |pane, _| pane.active_item().map(|item| item.item_id()))
        };
        assert_eq!(front(&left, cx), Some(owner.entity_id()));
        assert_eq!(front(&right, cx), Some(view.entity_id()));
    }

    #[gpui::test]
    async fn showing_the_detail_tab_unzooms_the_arena(cx: &mut TestAppContext) {
        let (arena, left, right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
        let view = cx.new_window_entity(|window, cx| InboxTodoView::new(todos, todo_id, window, cx));
        right.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
        });
        let arenas = vec![arena.clone()];
        for zoomed in [&left, &right] {
            zoomed.update(cx, |pane, cx| pane.set_zoomed(true, cx));
            arena.update(cx, |arena, _| arena.zoomed_pane = Some(zoomed.downgrade().into()));

            show(&view, &arena, &owner, &arenas, cx);
            assert!(arena.read_with(cx, |arena, _| arena.zoomed_pane.is_none()));
            assert!(!zoomed.read_with(cx, |pane, _| pane.is_zoomed()));
        }
    }

    #[gpui::test]
    async fn choices_can_be_cleared_and_sent_from_the_keyboard(cx: &mut TestAppContext) {
        let (arena, left, _right, owner, cx) = arena_with_owner(cx).await;
        let (todos, todo_id) = todos_with_list(owner_tab(&arena, &left, &owner), owner.entity_id(), cx);
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
        let (view, cx) = cx.add_window_view(|window, cx| InboxTodoView::new(todos.clone(), todo_id, window, cx));
        let sent = Rc::new(Cell::new(0));
        let _subscription = cx.update({
            let sent = sent.clone();
            |_, cx| {
                cx.subscribe(&view, move |_, event: &InboxTodoViewEvent, _| {
                    let InboxTodoViewEvent::Send { .. } = event;
                    sent.set(sent.get() + 1);
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

        todos.update(cx, |todos, cx| {
            todos.set_choice(todo_id, "Add tests", Some(TodoChoice::Go), cx);
            todos.set_choice(todo_id, "Check the lane", Some(TodoChoice::Done), cx);
        });
        todos.update(cx, |todos, cx| todos.clear_choices(todo_id, cx));
        assert!(todos.read_with(cx, |todos, _| todos.list(todo_id).is_some_and(|list| list.choices.is_empty())));

        todos.update(cx, |todos, cx| todos.set_choice(todo_id, "Add tests", Some(TodoChoice::Go), cx));
        cx.run_until_parked();
        // From inside a note editor too, which binds cmd-enter itself.
        let editor = view.read_with(cx, |view, _| view.note_editors.get("Add tests").cloned()).expect("note editor");
        cx.update(|window, cx| {
            let handle = editor.read(cx).focus_handle(cx);
            handle.focus(window, cx);
        });
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(sent.get(), 1);
    }

    fn unbound_list(asker: Option<InboxAsker>, arena_path: Option<&str>) -> InboxTodoList {
        InboxTodoList {
            id: 4,
            title: "Fix the Inbox".to_string(),
            arena_name: "zed".to_string(),
            updated_at: chrono::Utc::now(),
            asker,
            items: sample_items(),
            branch: None,
            arena_path: arena_path.map(PathBuf::from),
            tab: None,
            choices: HashMap::new(),
        }
    }

    #[test]
    fn a_closed_owner_is_restored_only_where_it_unambiguously_lived() {
        let claude = || {
            Some(InboxAsker::ClaudeSession {
                session_id: "c8832ec2".to_string(),
            })
        };
        let open = |names: &[(&str, &str)]| {
            names
                .iter()
                .map(|(name, path)| (name.to_string(), PathBuf::from(path)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            todo_restore_plan(&unbound_list(claude(), Some("/src/zed")), &[]),
            RestorePlan::ResumeClaude {
                arena_path: PathBuf::from("/src/zed"),
                session_id: "c8832ec2".to_string()
            }
        );
        assert_eq!(
            todo_restore_plan(
                &unbound_list(
                    Some(InboxAsker::CodexAcp {
                        saved_id: uuid::Uuid::nil()
                    }),
                    Some("/src/zed")
                ),
                &[]
            ),
            RestorePlan::OpenArena {
                arena_path: PathBuf::from("/src/zed")
            }
        );
        assert!(matches!(
            todo_restore_plan(&unbound_list(None, Some("/src/zed")), &[]),
            RestorePlan::Fallback(_)
        ));

        // Lists written before arena_path: the name must be unique.
        let legacy = unbound_list(claude(), None);
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
        let mut pending = HashMap::from([(1, PendingRestore::Launching)]);

        // The launch fails: forgotten, with a reason for the row.
        let error = record_restore_launch(&mut pending, 1, Err(anyhow::anyhow!("no claude")));
        assert!(error.is_some_and(|error| error.contains("no claude")));
        assert!(pending.is_empty());

        // Retry: nothing stale blocks a new attempt.
        pending.insert(1, PendingRestore::Launching);
        assert_eq!(record_restore_launch(&mut pending, 1, Ok(tab)), None);
        assert_eq!(pending.get(&1), Some(&PendingRestore::Launched { item_id: tab }));

        let observe = |exists: bool, bound: bool, tab_open: bool| {
            move |_: u64, _: &PendingRestore| RestoreObservation {
                list_exists: exists,
                list_bound: bound,
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

        pending.insert(1, PendingRestore::Launched { item_id: tab });
        pending.insert(2, PendingRestore::Restored);
        pending.insert(3, PendingRestore::Launching);
        let outcomes = advance_pending_restores(&mut pending, |todo_id, _| RestoreObservation {
            list_exists: todo_id != 3,
            list_bound: todo_id == 1,
            launched_tab_open: true,
        });
        assert_eq!(outcomes, vec![(1, RestoreOutcome::Open)]);
        assert_eq!(pending, HashMap::from([(2, PendingRestore::Restored)]));
    }

    #[test]
    fn an_acp_owner_comes_back_only_if_its_arena_still_saves_it() {
        assert_eq!(acp_restore_outcome(true, true), Ok(PendingRestore::Restored));
        assert!(acp_restore_outcome(true, false).is_err());
        assert!(acp_restore_outcome(false, false).is_err());
    }
}

