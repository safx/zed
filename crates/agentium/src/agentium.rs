mod arena;
mod git_status_view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{prelude::*, *};
use project::Project;
use project::git_store::GitStoreEvent;
use terminal_view::TerminalView;
use ui::{ActiveTheme, ContextMenu, prelude::*};
use workspace::{
    AppState, Pane, SplitDirection, Workspace, ZoomOut,
};

use arena::{Arena, ArenaEvent};

pub enum PaneContentType {
    Terminal,
    Diff,
    BranchDiff,
    GitStatus,
    ProjectSearch,
}

actions!(agentium, [NewClaudeCode, NewDiffView, NewBranchDiff, NewProjectSearch, NewGitStatus]);

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = agentium)]
pub(crate) struct ForkClaudeSession {
    pub session_id: String,
}

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

#[derive(Clone)]
pub(crate) struct SharedSessionState {
    pub ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pub pid_to_session_id: Rc<RefCell<HashMap<u32, String>>>,
}

impl SharedSessionState {
    fn new() -> Self {
        Self {
            ready_shell_pids: Rc::new(RefCell::new(HashSet::new())),
            pid_to_session_id: Rc::new(RefCell::new(HashMap::new())),
        }
    }
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
            session_state: SharedSessionState::new(),
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
        let session_state = self.session_state.clone();

        let arena_entity = cx.new(|cx| {
            Arena::new(arena_id, name, workspace_weak, project, modal_layer, working_directory, session_state, window, cx)
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
        arena_entity.update(cx, |arena, cx| {
            arena.activate_context(cx);
        });
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
        let pids: HashSet<u32> = self
            .claude_sessions
            .values()
            .filter(|s| s.is_ready)
            .flat_map(|s| s.ancestor_pids.iter().copied())
            .collect();
        *self.session_state.ready_shell_pids.borrow_mut() = pids;

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
        self.claude_sessions.insert(
            session_id,
            ClaudeSession {
                ancestor_pids,
                is_ready: false,
                user_prompt: String::new(),
                status_message: String::new(),
            },
        );
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
        self.sync_session_derived_state();
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
            self.sync_session_derived_state();
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
        let ready_pids = self.session_state.ready_shell_pids.borrow();
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
        let ready_pids = self.session_state.ready_shell_pids.borrow();
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
                                                    let summary = repo.status_summary();
                                                    Some((repo_path.clone(), branch_name, summary))
                                                } else {
                                                    None
                                                }
                                            })
                                            .max_by_key(|(repo_path, _, _)| repo_path.clone())
                                            .map(|(_, branch_name, summary)| (branch_name, summary))
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
