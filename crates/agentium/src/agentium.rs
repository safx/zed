use std::cell::RefCell;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use git_ui::git_status_icon;
use gpui::{prelude::*, *};
use project::Project;
use project::git_store::{GitStoreEvent, RepositoryEvent, StatusEntry};
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use markdown_preview::{OpenPreview, OpenPreviewToTheSide};
use search::ProjectSearchView;
use search::project_search::{ProjectSearch, ProjectSearchBar};
use task::Shell;
use terminal_view::TerminalView;
use ui::{ActiveTheme, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::notifications::NotifyResultExt as _;
use workspace::pane::render_item_indicator;
use workspace::{
    Item, pane, move_active_item, move_item, ActivateNextPane, ActivatePane,
    ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight, ActivatePaneUp, ActivatePreviousPane,
    AppState, DraggedTab, LeaderDecoration, ModalLayer, MoveItemToPane,
    MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft, MovePaneRight, MovePaneUp, NewTerminal,
    Pane, PaneGroup, PaneLeaderDecorator, Save, SaveAs, SaveIntent, SaveWithoutFormat,
    SplitDirection, SplitDown, SplitLeft, SplitMode, SplitRight, SplitUp, SwapPaneDown,
    SwapPaneLeft, SwapPaneRight, SwapPaneUp, ToggleFileFinder, ToggleZoom, Workspace,
};

pub enum PaneContentType {
    Terminal,
    Diff,
    BranchDiff,
    GitStatus,
    ProjectSearch,
}

actions!(agentium, [NewDiffView, NewBranchDiff, NewProjectSearch, NewGitStatus]);

struct ClaudeSession {
    ancestor_pids: Vec<u32>,
    is_ready: bool,
    user_prompt: String,
    status_message: String,
}

struct ReadyTerminalInfo {
    pane: Entity<Pane>,
    terminal_view: Entity<TerminalView>,
    user_prompt: String,
    status_message: String,
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
    claude_sessions: HashMap<String, ClaudeSession>,
    ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    should_move_window: bool,
    _git_subscription: gpui::Subscription,
    _arena_subscriptions: HashMap<EntityId, gpui::Subscription>,
}

struct Arena {
    id: usize,
    name: String,
    working_directory: Option<PathBuf>,
    active_pane: Entity<Pane>,
    center: PaneGroup,
    zoomed_pane: Option<AnyWeakView>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    modal_layer: Entity<ModalLayer>,
    ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
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
        let git_subscription = cx.subscribe(&git_store, |_this, _, _event: &GitStoreEvent, cx| {
            cx.notify();
        });

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        cx.subscribe_in(&rename_editor, window, |this: &mut Self, _, event: &EditorEvent, _window, cx| {
            if let EditorEvent::Blurred = event {
                this.finish_rename_arena(false, cx);
            }
        }).detach();

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
            ready_shell_pids: Rc::new(RefCell::new(HashSet::new())),
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
                project.find_or_create_worktree(&path, true, cx)
            })
            .detach_and_log_err(cx);
        self.add_arena_inner(Some(path), window, cx);
    }

    fn add_arena(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let working_directory = self
            .project
            .read(cx)
            .worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).abs_path().to_path_buf());
        self.add_arena_inner(working_directory, window, cx);
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
        let modal_layer = self.workspace_entity.read(cx).modal_layer().clone();
        let ready_shell_pids = self.ready_shell_pids.clone();

        let arena_entity = cx.new(|cx| {
            Arena::new(arena_id, name, workspace_weak, project, modal_layer, working_directory, ready_shell_pids, window, cx)
        });

        let subscription = cx.subscribe(
            &arena_entity,
            |this, _ws, event: &ArenaEvent, cx| match event {
                ArenaEvent::TerminalActivated { shell_pid } => {
                    this.clear_ready_for_shell_pid(*shell_pid, cx);
                }
            },
        );
        self._arena_subscriptions
            .insert(arena_entity.entity_id(), subscription);

        self.arenas.push(arena_entity.clone());
        self.active_arena_index = Some(self.arenas.len() - 1);
        let focus = arena_entity.focus_handle(cx);
        focus.focus(window, cx);
        cx.notify();
    }

    fn switch_arena(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.arenas.len() {
            self.active_arena_index = Some(index);
            let focus = self.arenas[index].focus_handle(cx);
            focus.focus(window, cx);
            cx.notify();
        }
    }

    fn active_arena(&self) -> Option<&Entity<Arena>> {
        self.active_arena_index
            .and_then(|i| self.arenas.get(i))
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

        let effective_path = match effective_path {
            Some(path) => path,
            None => return format!("Arena {}", fallback_id + 1),
        };

        // a. Remote repository name from matching git repo
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

        // b. Worktree root_name
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

        // c. Git repository work directory path's last component
        if let Some(repo) = matching_repo {
            let repo_path = &repo.read(cx).work_directory_abs_path;
            if let Some(name) = repo_path.file_name() {
                return name.to_string_lossy().to_string();
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
        self._arena_subscriptions
            .remove(&workspace.entity_id());
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

        if let Some(ws) = self.active_arena() {
            let focus = ws.focus_handle(cx);
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn sync_ready_shell_pids(&self) {
        let pids: HashSet<u32> = self
            .claude_sessions
            .values()
            .filter(|s| s.is_ready)
            .flat_map(|s| s.ancestor_pids.iter().copied())
            .collect();
        *self.ready_shell_pids.borrow_mut() = pids;
    }

    pub fn register_claude_session(
        &mut self,
        session_id: String,
        ancestor_pids: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        self.claude_sessions.insert(
            session_id,
            ClaudeSession {
                ancestor_pids,
                is_ready: false,
                user_prompt: String::new(),
                status_message: String::new(),
            },
        );
        self.sync_ready_shell_pids();
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
            session.is_ready = true;
            session.ancestor_pids = ancestor_pids;
            session.status_message = status_message;
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    is_ready: true,
                    user_prompt: String::new(),
                    status_message,
                },
            );
        }
        self.sync_ready_shell_pids();
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
            session.is_ready = false;
            session.status_message.clear();
        } else {
            self.claude_sessions.insert(
                session_id.to_string(),
                ClaudeSession {
                    ancestor_pids,
                    is_ready: false,
                    user_prompt: prompt,
                    status_message: String::new(),
                },
            );
        }
        self.sync_ready_shell_pids();
        self.notify_all_panes(cx);
        cx.notify();
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

    fn clear_ready_for_shell_pid(&mut self, shell_pid: u32, cx: &mut Context<Self>) {
        let mut changed = false;
        for session in self.claude_sessions.values_mut() {
            if session.is_ready && session.ancestor_pids.contains(&shell_pid) {
                session.is_ready = false;
                changed = true;
            }
        }
        if changed {
            self.sync_ready_shell_pids();
            self.notify_all_panes(cx);
            cx.notify();
        }
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
        let ready_pids = self.ready_shell_pids.borrow();
        if ready_pids.is_empty() {
            return 0;
        }
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
                            .is_some_and(|g| ready_pids.contains(&g.fallback_pid().as_u32()))
                    })
                    .count()
            })
            .sum()
    }

    fn collect_ready_terminal_infos(
        &self,
        arena_entity: &Entity<Arena>,
        cx: &App,
    ) -> Vec<ReadyTerminalInfo> {
        let ready_pids = self.ready_shell_pids.borrow();
        if ready_pids.is_empty() {
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
                    Some(p) if ready_pids.contains(&p) => p,
                    _ => continue,
                };
                let (user_prompt, status_message) = self
                    .claude_sessions
                    .values()
                    .find(|s| s.is_ready && s.ancestor_pids.contains(&pid))
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
                    info.user_prompt.clone()
                };
                let status_label = info.status_message.clone();

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
        window.focus(&target_pane.focus_handle(cx), cx);
        let item_index = target_pane
            .read(cx)
            .index_for_item(terminal_view);
        if let Some(index) = item_index {
            target_pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx);
            });
        }
    }
}

enum ArenaEvent {
    TerminalActivated { shell_pid: u32 },
}

impl EventEmitter<ArenaEvent> for Arena {}

fn repo_name_from_url(url: &str) -> Option<&str> {
    let path = url
        .strip_suffix(".git")
        .unwrap_or(url)
        .trim_end_matches('/');
    path.rsplit('/').next()
        .or_else(|| path.rsplit(':').next())
        .filter(|name| !name.is_empty())
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
                            .py_2()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.text)
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
                            })
                            .child("Agentium"),
                    )
                    .child(
                        div()
                            .id("workspace-list")
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
                                        git_store.repositories().values().find_map(|repo| {
                                            let repo = repo.read(cx);
                                            let repo_path = &repo.work_directory_abs_path;
                                            if working_dir.starts_with(repo_path.as_ref()) {
                                                let branch_name = repo.branch.as_ref().map(|b| b.name().to_string());
                                                let summary = repo.status_summary();
                                                Some((branch_name, summary))
                                            } else {
                                                None
                                            }
                                        })
                                    });

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
                                            let ready_count = self.count_ready_terminals_in_arena(arena_entity, cx);
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .justify_between()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .when(is_renaming, |d| d.child(self.rename_editor.clone()))
                                                .when(!is_renaming, |d| d.child(arena.name.clone()))
                                                .when(ready_count > 0, |d| {
                                                    let arena_entity_for_badge = arena_entity.clone();
                                                    d.child(
                                                        div()
                                                            .id(("arena-badge", arena.id))
                                                            .px_1p5()
                                                            .rounded_full()
                                                            .bg(colors.text_accent)
                                                            .text_color(colors.surface_background)
                                                            .text_xs()
                                                            .line_height(relative(1.4))
                                                            .cursor_pointer()
                                                            .child(format!("{ready_count}"))
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
                                        })
                                        .when_some(display_path, |d, path| {
                                            d.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(colors.text_muted)
                                                    .child(path),
                                            )
                                        })
                                        .when_some(git_info, |d, (branch, summary)| {
                                            use std::fmt::Write;
                                            let mut label = String::new();
                                            if let Some(branch_name) = branch {
                                                write!(&mut label, "\u{2387} {branch_name}").ok();
                                            }
                                            let added = summary.index.added + summary.worktree.added + summary.untracked;
                                            let modified = summary.index.modified + summary.worktree.modified;
                                            let deleted = summary.index.deleted + summary.worktree.deleted;
                                            if added > 0 {
                                                if !label.is_empty() { label.push_str("  "); }
                                                write!(&mut label, "+{added}").ok();
                                            }
                                            if modified > 0 {
                                                if !label.is_empty() { label.push_str("  "); }
                                                write!(&mut label, "~{modified}").ok();
                                            }
                                            if deleted > 0 {
                                                if !label.is_empty() { label.push_str("  "); }
                                                write!(&mut label, "-{deleted}").ok();
                                            }
                                            if label.is_empty() {
                                                d
                                            } else {
                                                d.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(colors.text_muted)
                                                        .child(label),
                                                )
                                            }
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
                    .child(
                        div().p_2().child(
                            div()
                                .id("add-workspace")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_sm()
                                .text_color(colors.text)
                                .cursor_pointer()
                                .hover(|d| d.bg(colors.element_hover))
                                .child("+ New Arena")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_arena(window, cx)
                                })),
                        ),
                    )
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
    }
}

// --- Arena (formerly AgentiumWorkspace) ---

impl Arena {
    fn new(
        id: usize,
        name: String,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        modal_layer: Entity<ModalLayer>,
        working_directory: Option<PathBuf>,
        ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = new_agentium_pane(workspace.clone(), project.clone(), ready_shell_pids.clone(), window, cx);
        let center = PaneGroup::new(pane.clone());

        let terminal_task = project.update(cx, |project, cx| {
            project.create_terminal_shell(working_directory.clone(), cx)
        });
        let workspace_weak = workspace.clone();
        let project_weak = project.downgrade();
        let active_pane = pane.clone();
        let pids_for_terminal = ready_shell_pids.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace_weak,
                        None,
                        project_weak,
                        window,
                        cx,
                    );
                    view.set_prompt_waiting_pids(pids_for_terminal);
                    view
                }));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, true, None, window, cx);
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);

        Self {
            id,
            name,
            working_directory,
            active_pane: pane,
            center,
            zoomed_pane: None,
            workspace,
            project,
            modal_layer,
            ready_shell_pids,
        }
    }

    fn add_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal_task = self
            .project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx));
        let workspace_weak = self.workspace.clone();
        let project_weak = self.project.downgrade();
        let active_pane = self.active_pane.clone();
        let pids = self.ready_shell_pids.clone();

        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace_weak,
                        None,
                        project_weak,
                        window,
                        cx,
                    );
                    view.set_prompt_waiting_pids(pids);
                    view
                }));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, true, None, window, cx);
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn save_active_item(
        &self,
        save_intent: SaveIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self.project.clone();
        let pane = self.active_pane.downgrade();
        let item = self.active_pane.read(cx).active_item();
        cx.spawn_in(window, async move |_this, cx| {
            if let Some(item) = item {
                Pane::save_item(project, &pane, item.as_ref(), save_intent, cx)
                    .await
                    .map(|_| ())?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn add_diff_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project_diff = cx.new(|cx| {
            git_ui::project_diff::ProjectDiff::new(
                self.project.clone(),
                workspace,
                window,
                cx,
            )
        });
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(project_diff), true, true, None, window, cx);
        });
    }

    fn add_branch_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = self.project.clone();
        let active_pane = self.active_pane.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let project_diff = cx
                .update(|window, cx| {
                    git_ui::project_diff::ProjectDiff::new_with_default_branch(
                        project, workspace, window, cx,
                    )
                })?
                .await?;
            cx.update(|window, cx| {
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(project_diff), true, true, None, window, cx);
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn add_project_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project_search = cx.new(|cx| ProjectSearch::new(self.project.clone(), cx));
        let view = cx.new(|cx| {
            ProjectSearchView::new(self.workspace.clone(), project_search, window, cx, None)
        });
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, true, None, window, cx);
        });
    }

    fn add_git_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| {
            GitStatusView::new(self.project.clone(), self.workspace.clone(), cx)
        });
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, true, None, window, cx);
        });
    }

    fn split_active_pane(
        &mut self,
        direction: SplitDirection,
        content_type: PaneContentType,
        keep_focus: bool,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match content_type {
            PaneContentType::Terminal => {
                self.split_with_terminal(direction, keep_focus, command, window, cx);
            }
            PaneContentType::Diff => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                let item = cx.new(|cx| {
                    git_ui::project_diff::ProjectDiff::new(
                        self.project.clone(),
                        workspace,
                        window,
                        cx,
                    )
                });
                self.split_with_item(Box::new(item), direction, keep_focus, window, cx);
            }
            PaneContentType::BranchDiff => {
                self.split_with_branch_diff(direction, keep_focus, window, cx);
            }
            PaneContentType::GitStatus => {
                let item = cx.new(|cx| {
                    GitStatusView::new(self.project.clone(), self.workspace.clone(), cx)
                });
                self.split_with_item(Box::new(item), direction, keep_focus, window, cx);
            }
            PaneContentType::ProjectSearch => {
                let project_search = cx.new(|cx| ProjectSearch::new(self.project.clone(), cx));
                let item = cx.new(|cx| {
                    ProjectSearchView::new(
                        self.workspace.clone(),
                        project_search,
                        window,
                        cx,
                        None,
                    )
                });
                self.split_with_item(Box::new(item), direction, keep_focus, window, cx);
            }
        }
    }

    fn split_with_item(
        &mut self,
        item: Box<dyn workspace::ItemHandle>,
        direction: SplitDirection,
        keep_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_pane = new_agentium_pane(
            self.workspace.clone(),
            self.project.clone(),
            self.ready_shell_pids.clone(),
            window,
            cx,
        );
        new_pane.update(cx, |pane, cx| {
            pane.add_item(item, true, true, None, window, cx);
        });
        self.center
            .split(&self.active_pane, &new_pane, direction, cx)
            .log_err();
        if !keep_focus {
            window.focus(&new_pane.focus_handle(cx), cx);
        }
    }

    fn split_with_terminal(
        &mut self,
        direction: SplitDirection,
        keep_focus: bool,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_task = if command.is_empty() {
            self.project.update(cx, |project, cx| {
                project.create_terminal_shell(self.working_directory.clone(), cx)
            })
        } else {
            let program = command[0].clone();
            let args = command[1..].to_vec();
            let shell = Shell::WithArguments {
                program,
                args,
                title_override: None,
            };
            self.project.update(cx, |project, cx| {
                project.create_terminal_with_shell(self.working_directory.clone(), shell, cx)
            })
        };

        let workspace_weak = self.workspace.clone();
        let project = self.project.clone();
        let ready_shell_pids = self.ready_shell_pids.clone();
        let active_pane = self.active_pane.clone();

        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal_task.await?;

            this.update_in(cx, move |this, window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace_weak.clone(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                }));
                let new_pane = new_agentium_pane(
                    workspace_weak,
                    project,
                    ready_shell_pids,
                    window,
                    cx,
                );
                new_pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, true, None, window, cx);
                });
                this.center
                    .split(&active_pane, &new_pane, direction, cx)
                    .log_err();
                if !keep_focus {
                    window.focus(&new_pane.focus_handle(cx), cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn split_with_branch_diff(
        &mut self,
        direction: SplitDirection,
        keep_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = self.project.clone();
        let workspace_weak = self.workspace.clone();
        let ready_shell_pids = self.ready_shell_pids.clone();
        let active_pane = self.active_pane.clone();

        cx.spawn_in(window, async move |this, cx| {
            let project_diff = cx
                .update(|window, cx| {
                    git_ui::project_diff::ProjectDiff::new_with_default_branch(
                        project.clone(),
                        workspace,
                        window,
                        cx,
                    )
                })?
                .await?;

            this.update_in(cx, |this, window, cx| {
                let new_pane = new_agentium_pane(
                    workspace_weak,
                    project,
                    ready_shell_pids,
                    window,
                    cx,
                );
                new_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(project_diff), true, true, None, window, cx);
                });
                this.center
                    .split(&active_pane, &new_pane, direction, cx)
                    .log_err();
                if !keep_focus {
                    window.focus(&new_pane.focus_handle(cx), cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn new_pane_with_terminal(
        &mut self,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let workspace_weak = self.workspace.clone();
        let project = self.project.clone();
        let ready_shell_pids = self.ready_shell_pids.clone();
        let working_directory = if clone {
            self.active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .and_then(|view| view.read(cx).terminal().read(cx).working_directory())
        } else {
            None
        };

        let terminal_task = self.project.update(cx, |project, cx| {
            project.create_terminal_shell(working_directory, cx)
        });

        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal_task.await.log_err()?;

            this.update_in(cx, move |_this, window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace_weak.clone(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    );
                    view.set_prompt_waiting_pids(ready_shell_pids.clone());
                    view
                }));
                let pane = new_agentium_pane(workspace_weak, project, ready_shell_pids, window, cx);
                pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, true, None, window, cx);
                });
                Some(pane)
            })
            .ok()
            .flatten()
        })
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::Remove { focus_on_pane } => {
                let pane_count = self.center.panes().len();
                self.center.remove(pane, cx).log_err();
                if pane_count > 1 {
                    if let Some(focus_pane) =
                        focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                    {
                        focus_pane.focus_handle(cx).focus(window, cx);
                    }
                }
            }
            pane::Event::ZoomIn => {
                if *pane == self.active_pane {
                    pane.update(cx, |pane, cx| pane.set_zoomed(true, cx));
                    if pane.read(cx).has_focus(window, cx) {
                        self.zoomed_pane = Some(pane.downgrade().into());
                    }
                    cx.notify();
                }
            }
            pane::Event::ZoomOut => {
                pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                self.zoomed_pane = None;
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    });
                }
            }
            &pane::Event::Split { direction, mode } => match mode {
                SplitMode::ClonePane | SplitMode::EmptyPane => {
                    let clone = matches!(mode, SplitMode::ClonePane);
                    let new_pane_task = self.new_pane_with_terminal(clone, window, cx);
                    let pane = pane.clone();
                    cx.spawn_in(window, async move |this, cx| {
                        let Some(new_pane) = new_pane_task.await else {
                            return;
                        };
                        this.update_in(cx, |this, window, cx| {
                            this.center
                                .split(&pane, &new_pane, direction, cx)
                                .log_err();
                            window.focus(&new_pane.focus_handle(cx), cx);
                        })
                        .ok();
                    })
                    .detach();
                }
                SplitMode::MovePane => {
                    let Some(item) =
                        pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                    else {
                        return;
                    };
                    let new_pane = new_agentium_pane(
                        self.workspace.clone(),
                        self.project.clone(),
                        self.ready_shell_pids.clone(),
                        window,
                        cx,
                    );
                    new_pane.update(cx, |pane, cx| {
                        pane.add_item(item, true, true, None, window, cx);
                    });
                    self.center.split(pane, &new_pane, direction, cx).log_err();
                    window.focus(&new_pane.focus_handle(cx), cx);
                }
            },
            pane::Event::ActivateItem { .. } => {
                if let Some(item) = pane.read(cx).active_item() {
                    if let Some(terminal_view) = item.downcast::<TerminalView>() {
                        let terminal = terminal_view.read(cx).terminal().read(cx);
                        if let Some(pid_getter) = terminal.pid_getter() {
                            cx.emit(ArenaEvent::TerminalActivated {
                                shell_pid: pid_getter.fallback_pid().as_u32(),
                            });
                        }
                    }
                }
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, _cx| {
                        workspace.set_last_active_center_pane(pane);
                    });
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
        {
            window.focus(&pane.focus_handle(cx), cx);
        }
    }

    fn swap_pane_in_direction(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if let Some(to) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
            .cloned()
        {
            self.center.swap(&self.active_pane, &to, cx);
            cx.notify();
        }
    }

    fn open_markdown_preview(
        &mut self,
        side: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return;
        };
        let active_pane = self.active_pane.clone();
        let Some(editor) = active_pane
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<editor::Editor>(cx))
        else {
            return;
        };
        if !MarkdownPreviewView::is_markdown_file(&editor, cx) {
            return;
        }

        let target_pane = if side {
            let right_pane = self
                .center
                .find_pane_in_direction(&self.active_pane, SplitDirection::Right, cx)
                .cloned();
            match right_pane {
                Some(pane) => pane,
                None => {
                    let new_pane = new_agentium_pane(
                        self.workspace.clone(),
                        self.project.clone(),
                        self.ready_shell_pids.clone(),
                        window,
                        cx,
                    );
                    self.center
                        .split(&active_pane, &new_pane, SplitDirection::Right, cx)
                        .log_err();
                    new_pane
                }
            }
        } else {
            active_pane.clone()
        };

        let existing_preview_idx =
            MarkdownPreviewView::find_existing_independent_preview_item_idx(
                target_pane.read(cx),
                &editor,
                cx,
            );

        if let Some(existing_idx) = existing_preview_idx {
            target_pane.update(cx, |pane, cx| {
                pane.activate_item(existing_idx, true, true, window, cx);
            });
            return;
        }

        workspace_entity.update(cx, |workspace, cx| {
            let language_registry = workspace.project().read(cx).languages().clone();
            let workspace_handle = workspace.weak_handle();
            let view = MarkdownPreviewView::new(
                MarkdownPreviewMode::Default,
                editor.clone(),
                workspace_handle,
                language_registry,
                window,
                cx,
            );
            target_pane.update(cx, |pane, cx| {
                let focus = !side;
                pane.add_item(Box::new(view), focus, focus, None, window, cx);
            });
        });

        if side {
            editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn move_pane_to_border(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if self
            .center
            .move_to_border(&self.active_pane, direction, cx)
            .log_err()
            .is_some_and(|moved| moved)
        {
            cx.notify();
        }
    }
}

impl Focusable for Arena {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }
}

impl Render for Arena {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_actions_div = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane.clone())
            })
            .unwrap_or_else(div);
        let ready_pids = self.ready_shell_pids.borrow().clone();
        self.workspace
            .update(cx, |_workspace, cx| {
                let decorator = AgentiumPaneDecorator {
                    active_pane: &self.active_pane,
                    workspace: &self.workspace,
                    ready_shell_pids: &ready_pids,
                };
                search_actions_div
                    .size_full()
                    .child(self.center.render(
                        self.zoomed_pane.as_ref(),
                        &decorator,
                        window,
                        cx,
                    ))
            })
            .ok()
            .map(|pane_group| {
                let mut context = KeyContext::new_with_defaults();
                context.add("Workspace");

                div()
                    .key_context(context)
                    .relative()
                    .size_full()
                    .on_action(
                        cx.listener(|this, action: &ToggleFileFinder, window, cx| {
                            let Some(workspace) = this.workspace.upgrade() else {
                                return;
                            };
                            workspace.update(cx, |workspace, cx| {
                                if workspace
                                    .active_modal::<file_finder::FileFinder>(cx)
                                    .is_some()
                                {
                                    workspace.hide_modal(window, cx);
                                } else {
                                    file_finder::FileFinder::open(
                                        workspace,
                                        action.separate_history,
                                        window,
                                        cx,
                                    )
                                    .detach();
                                }
                            });
                        }),
                    )
                    .on_action(
                        cx.listener(|this, action: &Save, window, cx| {
                            this.save_active_item(
                                action.save_intent.unwrap_or(SaveIntent::Save),
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &SaveWithoutFormat, window, cx| {
                            this.save_active_item(
                                SaveIntent::SaveWithoutFormat,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &SaveAs, window, cx| {
                            this.save_active_item(SaveIntent::SaveAs, window, cx);
                        }),
                    )
                    .child(pane_group)
                    .children(self.zoomed_pane.as_ref().and_then(|view| {
                        let zoomed_view = view.upgrade()?;
                        let colors = cx.theme().colors();
                        Some(
                            div()
                                .occlude()
                                .absolute()
                                .overflow_hidden()
                                .bg(colors.background)
                                .child(zoomed_view)
                                .inset_0()
                                .shadow_lg(),
                        )
                    }))
                    .on_action(
                        cx.listener(|this, _: &OpenPreview, window, cx| {
                            this.open_markdown_preview(false, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &OpenPreviewToTheSide, window, cx| {
                            this.open_markdown_preview(true, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &NewTerminal, window, cx| {
                            this.add_terminal(window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &NewDiffView, window, cx| {
                            this.add_diff_view(window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &NewBranchDiff, window, cx| {
                            this.add_branch_diff(window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &NewProjectSearch, window, cx| {
                            this.add_project_search(window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &NewGitStatus, window, cx| {
                            this.add_git_status(window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivatePaneLeft, window, cx| {
                            this.activate_pane_in_direction(SplitDirection::Left, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivatePaneRight, window, cx| {
                            this.activate_pane_in_direction(SplitDirection::Right, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivatePaneUp, window, cx| {
                            this.activate_pane_in_direction(SplitDirection::Up, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivatePaneDown, window, cx| {
                            this.activate_pane_in_direction(SplitDirection::Down, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivateNextPane, window, cx| {
                            let panes = this.center.panes();
                            if let Some(ix) =
                                panes.iter().position(|pane| **pane == this.active_pane)
                            {
                                let next_ix = (ix + 1) % panes.len();
                                window.focus(&panes[next_ix].focus_handle(cx), cx);
                            }
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &ActivatePreviousPane, window, cx| {
                            let panes = this.center.panes();
                            if let Some(ix) =
                                panes.iter().position(|pane| **pane == this.active_pane)
                            {
                                let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                                window.focus(&panes[prev_ix].focus_handle(cx), cx);
                            }
                        }),
                    )
                    .on_action(
                        cx.listener(|this, action: &ActivatePane, window, cx| {
                            let panes = this.center.panes();
                            if let Some(&pane) = panes.get(action.0) {
                                window.focus(&pane.read(cx).focus_handle(cx), cx);
                            }
                        }),
                    )
                    .on_action(cx.listener(|this, _: &SwapPaneLeft, _, cx| {
                        this.swap_pane_in_direction(SplitDirection::Left, cx);
                    }))
                    .on_action(cx.listener(|this, _: &SwapPaneRight, _, cx| {
                        this.swap_pane_in_direction(SplitDirection::Right, cx);
                    }))
                    .on_action(cx.listener(|this, _: &SwapPaneUp, _, cx| {
                        this.swap_pane_in_direction(SplitDirection::Up, cx);
                    }))
                    .on_action(cx.listener(|this, _: &SwapPaneDown, _, cx| {
                        this.swap_pane_in_direction(SplitDirection::Down, cx);
                    }))
                    .on_action(cx.listener(|this, _: &MovePaneLeft, _, cx| {
                        this.move_pane_to_border(SplitDirection::Left, cx);
                    }))
                    .on_action(cx.listener(|this, _: &MovePaneRight, _, cx| {
                        this.move_pane_to_border(SplitDirection::Right, cx);
                    }))
                    .on_action(cx.listener(|this, _: &MovePaneUp, _, cx| {
                        this.move_pane_to_border(SplitDirection::Up, cx);
                    }))
                    .on_action(cx.listener(|this, _: &MovePaneDown, _, cx| {
                        this.move_pane_to_border(SplitDirection::Down, cx);
                    }))
                    .on_action(
                        cx.listener(|this, action: &MoveItemToPane, window, cx| {
                            let Some(&target_pane) =
                                this.center.panes().get(action.destination)
                            else {
                                return;
                            };
                            move_active_item(
                                &this.active_pane,
                                target_pane,
                                action.focus,
                                true,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_action(cx.listener(
                        |this, action: &MoveItemToPaneInDirection, window, cx| {
                            if let Some(destination) = this
                                .center
                                .find_pane_in_direction(
                                    &this.active_pane,
                                    action.direction,
                                    cx,
                                )
                                .cloned()
                            {
                                move_active_item(
                                    &this.active_pane,
                                    &destination,
                                    action.focus,
                                    true,
                                    window,
                                    cx,
                                );
                            }
                        },
                    ))
                    .child(self.modal_layer.clone())
            })
            .unwrap_or_else(|| div().size_full())
    }
}

struct AgentiumPaneDecorator<'a> {
    active_pane: &'a Entity<Pane>,
    workspace: &'a WeakEntity<Workspace>,
    ready_shell_pids: &'a HashSet<u32>,
}

impl PaneLeaderDecorator for AgentiumPaneDecorator<'_> {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration {
        if pane != self.active_pane {
            return LeaderDecoration::default();
        }
        let is_ready = pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<TerminalView>())
            .and_then(|tv| {
                let terminal = tv.read(cx).terminal().read(cx);
                terminal
                    .pid_getter()
                    .map(|g| g.fallback_pid().as_u32())
            })
            .is_some_and(|pid| self.ready_shell_pids.contains(&pid));

        if is_ready {
            LeaderDecoration {
                border: Some(cx.theme().colors().border_focused),
                status_box: None,
            }
        } else {
            LeaderDecoration::default()
        }
    }

    fn active_pane(&self) -> &Entity<Pane> {
        self.active_pane
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        self.workspace
    }
}

fn new_agentium_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    window: &mut Window,
    cx: &mut Context<Arena>,
) -> Entity<Pane> {
    let arena = cx.entity().downgrade();

    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            NewTerminal::default().boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_, _| true);
        pane.set_zoom_out_on_close(false);

        let split_predicate_workspace = arena.clone();
        pane.set_can_split(Some(Arc::new(
            move |pane, dragged_item, _window, cx| {
                if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                    let is_current_pane = tab.pane == cx.entity();
                    let Some(can_drag_away) = split_predicate_workspace
                        .read_with(cx, |arena, _| {
                            let panes = arena.center.panes();
                            !panes.contains(&&tab.pane)
                                || panes.len() > 1
                                || (!is_current_pane || pane.items_len() > 1)
                        })
                        .ok()
                    else {
                        return false;
                    };
                    if can_drag_away {
                        let item = if is_current_pane {
                            pane.item_for_index(tab.ix)
                        } else {
                            tab.pane.read(cx).item_for_index(tab.ix)
                        };
                        if item.is_some() {
                            return true;
                        }
                    }
                }
                false
            },
        )));

        let toolbar = pane.toolbar().clone();
        if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
            let languages = Some(project.read(cx).languages().clone());
            (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
        }

        let project_search_bar = cx.new(|_| ProjectSearchBar::new());
        toolbar.update(cx, |toolbar, cx| {
            toolbar.add_item(project_search_bar, window, cx);
        });

        let drop_project = project.downgrade();
        let drop_arena = arena.clone();
        let drop_workspace = workspace.clone();
        let drop_ready_shell_pids = ready_shell_pids.clone();
        pane.set_custom_drop_handle(cx, move |pane, dropped_item, window, cx| {
            if !drop_project.upgrade().is_some() {
                return ControlFlow::Break(());
            }
            if let Some(tab) = dropped_item.downcast_ref::<DraggedTab>() {
                let this_pane = cx.entity();
                let item = if tab.pane == this_pane {
                    pane.item_for_index(tab.ix)
                } else {
                    tab.pane.read(cx).item_for_index(tab.ix)
                };
                if let Some(item) = item {
                    {
                        let source = tab.pane.clone();
                        let item_id_to_move = item.item_id();

                        let Some(split_direction) = pane.drag_split_direction() else {
                            return ControlFlow::Continue(());
                        };

                        let workspace_handle = drop_workspace.clone();
                        let arena = drop_arena.clone();
                        let project = drop_project.clone();
                        let ready_pids = drop_ready_shell_pids.clone();

                        cx.spawn_in(window, async move |_, cx| {
                            cx.update(|window, cx| {
                                let Some(project) = project.upgrade() else {
                                    return;
                                };
                                let Ok(new_pane) =
                                    arena.update(cx, |workspace, cx| {
                                        let new_pane = new_agentium_pane(
                                            workspace_handle,
                                            project,
                                            ready_pids,
                                            window,
                                            cx,
                                        );
                                        workspace.center.split(
                                            &this_pane,
                                            &new_pane,
                                            split_direction,
                                            cx,
                                        )?;
                                        anyhow::Ok(new_pane)
                                    })
                                else {
                                    return;
                                };
                                let Some(new_pane) = new_pane.log_err() else {
                                    return;
                                };
                                move_item(
                                    &source,
                                    &new_pane,
                                    item_id_to_move,
                                    new_pane.read(cx).active_item_index(),
                                    true,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                        })
                        .detach();
                    }
                }
            }
            ControlFlow::Break(())
        });

        pane
    });

    pane.update(cx, |pane, cx| {
        pane.set_render_tab_bar_buttons(cx, |pane, _window, cx| {
            let focus_handle = pane.focus_handle(cx);
            let right_children = h_flex()
                .gap(DynamicSpacing::Base02.rems(cx))
                .child(
                    PopoverMenu::new("agentium-tab-bar-popover-menu")
                        .trigger_with_tooltip(
                            IconButton::new("plus", IconName::Plus)
                                .icon_size(IconSize::Small),
                            Tooltip::text("New…"),
                        )
                        .anchor(Corner::TopRight)
                        .with_handle(pane.new_item_context_menu_handle.clone())
                        .menu(move |_window, cx| {
                            let focus_handle = focus_handle.clone();
                            Some(ContextMenu::build(_window, cx, |menu, _, _| {
                                menu.context(focus_handle.clone())
                                    .action(
                                        "New Terminal",
                                        NewTerminal::default().boxed_clone(),
                                    )
                                    .action(
                                        "New Diff View",
                                        NewDiffView.boxed_clone(),
                                    )
                                    .action(
                                        "New Branch Diff",
                                        NewBranchDiff.boxed_clone(),
                                    )
                                    .action(
                                        "New Project Search",
                                        NewProjectSearch.boxed_clone(),
                                    )
                                    .action(
                                        "New Git Status",
                                        NewGitStatus.boxed_clone(),
                                    )
                            }))
                        }),
                )
                .child(
                    PopoverMenu::new("agentium-pane-tab-bar-split")
                        .trigger_with_tooltip(
                            IconButton::new("agentium-pane-split", IconName::Split)
                                .icon_size(IconSize::Small),
                            Tooltip::text("Split Pane"),
                        )
                        .anchor(Corner::TopRight)
                        .with_handle(pane.split_item_context_menu_handle.clone())
                        .menu(|window, cx| {
                            ContextMenu::build(window, cx, |menu, _, _| {
                                menu.action("Split Right", SplitRight::default().boxed_clone())
                                    .action("Split Left", SplitLeft::default().boxed_clone())
                                    .action("Split Up", SplitUp::default().boxed_clone())
                                    .action("Split Down", SplitDown::default().boxed_clone())
                            })
                            .into()
                        }),
                )
                .child({
                    let zoomed = pane.is_zoomed();
                    IconButton::new("toggle_zoom", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .toggle_state(zoomed)
                        .selected_icon(IconName::Minimize)
                        .on_click(cx.listener(|pane, _, window, cx| {
                            pane.toggle_zoom(&ToggleZoom, window, cx);
                        }))
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action(
                                if zoomed { "Zoom Out" } else { "Zoom In" },
                                &ToggleZoom,
                                cx,
                            )
                        })
                })
                .into_any_element()
                .into();
            (None, right_children)
        });

        pane.set_tab_bar_drag_area(true);

        let ready_pids = ready_shell_pids;
        pane.set_render_item_indicator(move |item, cx| {
            if let Some(terminal_view) = item.downcast::<TerminalView>() {
                let terminal = terminal_view.read(cx).terminal().read(cx);
                if let Some(pid_getter) = terminal.pid_getter() {
                    let pid = pid_getter.fallback_pid().as_u32();
                    if ready_pids.borrow().contains(&pid) {
                        return Some(Indicator::dot().color(Color::Accent));
                    }
                }
            }
            render_item_indicator(item, cx)
        });
    });

    cx.subscribe_in(&pane, window, Arena::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

// --- GitStatusView ---

enum GitStatusSection {
    Conflicts,
    Tracked,
    Untracked,
}

impl GitStatusSection {
    fn title(&self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Tracked => "Tracked",
            Self::Untracked => "Untracked",
        }
    }
}

enum GitStatusListEntry {
    Header(GitStatusSection),
    Entry(StatusEntry),
}

struct GitStatusView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<GitStatusListEntry>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitStatusView {
    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, _)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_)
            | GitStoreEvent::ActiveRepositoryChanged(_) => {
                this.update_entries(cx);
            }
            _ => {}
        }));

        let mut this = Self {
            project,
            workspace,
            entries: Vec::new(),
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.update_entries(cx);
        this
    }

    fn update_entries(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();

        let repo = self
            .project
            .read(cx)
            .active_repository(cx)
            .or_else(|| {
                self.project
                    .read(cx)
                    .repositories(cx)
                    .values()
                    .next()
                    .cloned()
            });
        let Some(repo) = repo else {
            cx.notify();
            return;
        };
        let repo = repo.read(cx);

        let mut conflict_entries = Vec::new();
        let mut tracked_entries = Vec::new();
        let mut untracked_entries = Vec::new();

        for entry in repo.cached_status() {
            if repo.had_conflict_on_last_merge_head_change(&entry.repo_path) {
                conflict_entries.push(entry);
            } else if entry.status.is_created() {
                untracked_entries.push(entry);
            } else {
                tracked_entries.push(entry);
            }
        }

        for (section, entries) in [
            (GitStatusSection::Conflicts, conflict_entries),
            (GitStatusSection::Tracked, tracked_entries),
            (GitStatusSection::Untracked, untracked_entries),
        ] {
            if entries.is_empty() {
                continue;
            }
            self.entries.push(GitStatusListEntry::Header(section));
            for entry in entries {
                self.entries.push(GitStatusListEntry::Entry(entry));
            }
        }

        cx.notify();
    }

    fn open_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(GitStatusListEntry::Entry(status_entry)) = self.entries.get(ix) else {
            return;
        };
        if status_entry.status.is_deleted() {
            return;
        }
        let repo = self
            .project
            .read(cx)
            .active_repository(cx)
            .or_else(|| {
                self.project
                    .read(cx)
                    .repositories(cx)
                    .values()
                    .next()
                    .cloned()
            });
        let Some(repo) = repo else {
            return;
        };
        let Some(project_path) =
            repo.read(cx)
                .repo_path_to_project_path(&status_entry.repo_path, cx)
        else {
            return;
        };
        let Some(open_task) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .ok()
        else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, mut cx| {
            open_task
                .await
                .notify_workspace_async_err(workspace, &mut cx);
        })
        .detach();
    }
}

impl EventEmitter<()> for GitStatusView {}

impl Focusable for GitStatusView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitStatusView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let entry_count = self.entries.len();

        if entry_count == 0 {
            return div()
                .track_focus(&self.focus_handle)
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child("No changes")
                .into_any_element();
        }

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(
                uniform_list(
                    "git-status-entries",
                    entry_count,
                    cx.processor(
                        |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                            let colors = cx.theme().colors();
                            range
                                .map(|ix| {
                                    let entry = &this.entries[ix];
                                    match entry {
                                        GitStatusListEntry::Header(section) => div()
                                            .id(("header", ix))
                                            .px_2()
                                            .py_1()
                                            .text_sm()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(colors.text_muted)
                                            .child(section.title())
                                            .into_any_element(),
                                        GitStatusListEntry::Entry(status_entry) => div()
                                            .id(("entry", ix))
                                            .px_2()
                                            .py_0p5()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_2()
                                            .text_sm()
                                            .text_color(colors.text)
                                            .cursor_pointer()
                                            .hover(|style| {
                                                style.bg(colors.element_hover)
                                            })
                                            .child(git_status_icon(status_entry.status))
                                            .child(SharedString::from(
                                                status_entry
                                                    .repo_path
                                                    .display(util::paths::PathStyle::Posix)
                                                    .to_string(),
                                            ))
                                            .on_click(cx.listener(
                                                move |this, _, window, cx| {
                                                    this.open_entry(ix, window, cx);
                                                },
                                            ))
                                            .into_any_element(),
                                    }
                                })
                                .collect()
                        },
                    ),
                )
                .flex_1()
                .track_scroll(&self.scroll_handle),
            )
            .into_any_element()
    }
}

impl Item for GitStatusView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Git Status".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }
}
