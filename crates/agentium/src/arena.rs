use std::cell::RefCell;
use std::cmp;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{prelude::*, *};
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use markdown_preview::{OpenPreview, OpenPreviewToTheSide};
use project::{Project, ProjectPath, WorktreeId};
use search::ProjectSearchView;
use search::project_search::{ProjectSearch, ProjectSearchBar};
use task::Shell;
use terminal::Terminal;
use terminal_view::TerminalView;
use ui::{ActiveTheme, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::pane::render_item_indicator;
use workspace::{
    pane, move_active_item, move_item, ActivateNextPane, ActivatePane,
    ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight, ActivatePaneUp, ActivatePreviousPane,
    DraggedTab, LeaderDecoration, ModalLayer, MoveItemToPane,
    MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft, MovePaneRight, MovePaneUp, NewTerminal,
    Pane, PaneGroup, PaneLeaderDecorator, Save, SaveAs, SaveIntent, SaveWithoutFormat,
    SplitDirection, SplitDown, SplitLeft, SplitMode, SplitRight, SplitUp, SwapPaneDown,
    SwapPaneLeft, SwapPaneRight, SwapPaneUp, ToggleFileFinder, ToggleZoom, Workspace,
};

use crate::{
    NewBranchDiff, NewClaudeCode, NewDiffView, NewGitStatus, NewProjectSearch, PaneContentType,
    git_status_view::GitStatusView,
};

pub(crate) enum ArenaEvent {
    TerminalActivated { shell_pid: u32 },
}

pub(crate) struct Arena {
    pub(crate) id: usize,
    pub(crate) name: String,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) active_pane: Entity<Pane>,
    pub(crate) center: PaneGroup,
    pub(crate) zoomed_pane: Option<AnyWeakView>,
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) project: Entity<Project>,
    modal_layer: Entity<ModalLayer>,
    pub(crate) session_state: crate::SharedSessionState,
}

impl Arena {
    pub(crate) fn new(
        id: usize,
        name: String,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        modal_layer: Entity<ModalLayer>,
        working_directory: Option<PathBuf>,
        session_state: crate::SharedSessionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = new_agentium_pane(workspace.clone(), project.clone(), session_state.clone(), window, cx);
        let center = PaneGroup::new(pane.clone());

        let terminal_task = project.update(cx, |project, cx| {
            project.create_terminal_shell(working_directory.clone(), cx)
        });
        let workspace_weak = workspace.clone();
        let project_weak = project.downgrade();
        let active_pane = pane.clone();
        let pids_for_terminal = session_state.ready_shell_pids.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                add_terminal_view_to_pane(
                    terminal,
                    workspace_weak,
                    project_weak,
                    pids_for_terminal,
                    &active_pane,
                    window,
                    cx,
                );
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
            session_state,
        }
    }

    pub(crate) fn worktree_id(&self, cx: &App) -> Option<WorktreeId> {
        let working_directory = self.working_directory.as_ref()?;
        self.project.read(cx).visible_worktrees(cx).find_map(|wt| {
            let wt = wt.read(cx);
            if wt.abs_path().as_ref() == working_directory.as_path() {
                Some(wt.id())
            } else {
                None
            }
        })
    }

    pub(crate) fn activate_context(&self, cx: &mut App) {
        let worktree_id = self.worktree_id(cx);

        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |ws, cx| {
                ws.set_active_worktree_override(worktree_id, cx);
            });
        }

        if let Some(worktree_id) = worktree_id {
            let project_path = ProjectPath {
                worktree_id,
                path: Arc::from(util::rel_path::RelPath::empty()),
            };
            let git_store = self.project.read(cx).git_store().clone();
            git_store.update(cx, |git_store, cx| {
                git_store.set_active_repo_for_path(&project_path, cx);
            });
        }
    }

    fn add_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal_task = self
            .project
            .update(cx, |project, cx| {
                project.create_terminal_shell(self.working_directory.clone(), cx)
            });
        let workspace_weak = self.workspace.clone();
        let project_weak = self.project.downgrade();
        let active_pane = self.active_pane.clone();
        let pids = self.session_state.ready_shell_pids.clone();

        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                add_terminal_view_to_pane(
                    terminal,
                    workspace_weak,
                    project_weak,
                    pids,
                    &active_pane,
                    window,
                    cx,
                );
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn add_claude_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shell = Shell::WithArguments {
            program: "claude".to_string(),
            args: vec![],
            title_override: Some("Claude Code".to_string()),
        };
        let terminal_task = self
            .project
            .update(cx, |project, cx| {
                project.create_terminal_with_shell(self.working_directory.clone(), shell, cx)
            });
        let workspace_weak = self.workspace.clone();
        let project_weak = self.project.downgrade();
        let active_pane = self.active_pane.clone();
        let pids = self.session_state.ready_shell_pids.clone();

        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                add_terminal_view_to_pane(
                    terminal,
                    workspace_weak,
                    project_weak,
                    pids,
                    &active_pane,
                    window,
                    cx,
                );
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
        self.activate_context(cx);
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
        self.activate_context(cx);
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
        self.activate_context(cx);
        let project_search = cx.new(|cx| ProjectSearch::new(self.project.clone(), cx));
        let needs_filter = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let filter = if needs_filter {
            self.working_directory
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| format!("{}/**", n.to_string_lossy()))
        } else {
            None
        };
        let workspace_weak = self.workspace.clone();
        let view = cx.new(|cx| {
            let mut view =
                ProjectSearchView::new(workspace_weak, project_search, window, cx, None);
            if let Some(ref filter) = filter {
                view.set_include_filter(filter, window, cx);
            }
            view
        });
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, true, None, window, cx);
        });
    }

    fn add_git_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_context(cx);
        let view = cx.new(|cx| {
            GitStatusView::new(self.project.clone(), self.workspace.clone(), cx)
        });
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, true, None, window, cx);
        });
    }

    pub(crate) fn add_tab(
        &mut self,
        content_type: PaneContentType,
        command: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match content_type {
            PaneContentType::Terminal => {
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
                        project.create_terminal_with_shell(
                            self.working_directory.clone(),
                            shell,
                            cx,
                        )
                    })
                };
                let workspace_weak = self.workspace.clone();
                let project_weak = self.project.downgrade();
                let active_pane = self.active_pane.clone();
                let pids = self.session_state.ready_shell_pids.clone();
                cx.spawn_in(window, async move |_this, cx| {
                    let terminal = terminal_task.await?;
                    cx.update(|window, cx| {
                        add_terminal_view_to_pane(
                            terminal,
                            workspace_weak,
                            project_weak,
                            pids,
                            &active_pane,
                            window,
                            cx,
                        );
                    })?;
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            PaneContentType::Diff => {
                self.add_diff_view(window, cx);
            }
            PaneContentType::BranchDiff => {
                self.add_branch_diff(window, cx);
            }
            PaneContentType::GitStatus => {
                self.add_git_status(window, cx);
            }
            PaneContentType::ProjectSearch => {
                self.add_project_search(window, cx);
            }
        }
    }

    pub(crate) fn split_active_pane(
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
                self.activate_context(cx);
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
                self.activate_context(cx);
                self.split_with_branch_diff(direction, keep_focus, window, cx);
            }
            PaneContentType::GitStatus => {
                self.activate_context(cx);
                let item = cx.new(|cx| {
                    GitStatusView::new(self.project.clone(), self.workspace.clone(), cx)
                });
                self.split_with_item(Box::new(item), direction, keep_focus, window, cx);
            }
            PaneContentType::ProjectSearch => {
                self.activate_context(cx);
                let project_search = cx.new(|cx| ProjectSearch::new(self.project.clone(), cx));
                let needs_filter =
                    self.project.read(cx).visible_worktrees(cx).count() > 1;
                let filter = if needs_filter {
                    self.working_directory
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| format!("{}/**", n.to_string_lossy()))
                } else {
                    None
                };
                let workspace_weak = self.workspace.clone();
                let item = cx.new(|cx| {
                    let mut view = ProjectSearchView::new(
                        workspace_weak,
                        project_search,
                        window,
                        cx,
                        None,
                    );
                    if let Some(ref filter) = filter {
                        view.set_include_filter(filter, window, cx);
                    }
                    view
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
            self.session_state.clone(),
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
        let session_state = self.session_state.clone();
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
                    session_state,
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
        let session_state = self.session_state.clone();
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
                    session_state,
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
        let session_state = self.session_state.clone();
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
                let pane = new_agentium_pane(workspace_weak.clone(), project.clone(), session_state.clone(), window, cx);
                add_terminal_view_to_pane(
                    terminal,
                    workspace_weak,
                    project.downgrade(),
                    session_state.ready_shell_pids.clone(),
                    &pane,
                    window,
                    cx,
                );
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
                        self.session_state.clone(),
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
                        self.session_state.clone(),
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

impl EventEmitter<ArenaEvent> for Arena {}

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
        let ready_pids = self.session_state.ready_shell_pids.borrow().clone();
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
                            let worktree_id = this.worktree_id(cx);
                            let Some(workspace) = this.workspace.upgrade() else {
                                return;
                            };
                            workspace.update(cx, |workspace, cx| {
                                if workspace
                                    .active_modal::<file_finder::FileFinder>(cx)
                                    .is_some()
                                {
                                    workspace.hide_modal(window, cx);
                                } else if let Some(worktree_id) = worktree_id {
                                    file_finder::FileFinder::open_scoped(
                                        workspace,
                                        action.separate_history,
                                        worktree_id,
                                        window,
                                        cx,
                                    )
                                    .detach();
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
                        cx.listener(|this, _: &NewClaudeCode, window, cx| {
                            this.add_claude_code(window, cx);
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
                    .on_action(
                        cx.listener(|this, action: &crate::ForkClaudeSession, window, cx| {
                            let shell = Shell::WithArguments {
                                program: "claude".to_string(),
                                args: vec![
                                    "--resume".to_string(),
                                    action.session_id.clone(),
                                    "--fork-session".to_string(),
                                ],
                                title_override: None,
                            };
                            let terminal_task = this.project.update(cx, |project, cx| {
                                project.create_terminal_with_shell(
                                    this.working_directory.clone(),
                                    shell,
                                    cx,
                                )
                            });
                            let workspace_weak = this.workspace.clone();
                            let project_weak = this.project.downgrade();
                            let ready_shell_pids = this.session_state.ready_shell_pids.clone();
                            let pane = this.active_pane.clone();
                            cx.spawn_in(window, async move |_this, cx| {
                                let terminal = terminal_task.await?;
                                cx.update(|window, cx| {
                                    add_terminal_view_to_pane(
                                        terminal,
                                        workspace_weak,
                                        project_weak,
                                        ready_shell_pids,
                                        &pane,
                                        window,
                                        cx,
                                    );
                                })?;
                                anyhow::Ok(())
                            })
                            .detach_and_log_err(cx);
                        }),
                    )
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

fn add_terminal_view_to_pane(
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    ready_shell_pids: Rc<RefCell<HashSet<u32>>>,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut App,
) {
    let terminal_view = Box::new(cx.new(|cx| {
        let mut view = TerminalView::new(terminal, workspace, None, project, window, cx);
        view.set_prompt_waiting_pids(ready_shell_pids);
        view
    }));
    pane.update(cx, |pane, cx| {
        pane.add_item(terminal_view, true, true, None, window, cx);
    });
}

pub(crate) fn new_agentium_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    session_state: crate::SharedSessionState,
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
        pane.set_show_external_drop_overlay(false);

        pane.set_tab_context_menu_extension({
            let pid_to_session_id = session_state.pid_to_session_id.clone();
            move |item, _window, cx| {
                let Some(entity) =
                    item.act_as_type(std::any::TypeId::of::<TerminalView>(), cx)
                else {
                    return Vec::new();
                };
                let Ok(terminal_view) = entity.downcast::<TerminalView>() else {
                    return Vec::new();
                };
                let terminal = terminal_view.read(cx).terminal().read(cx);
                let Some(shell_pid) = terminal
                    .pid_getter()
                    .map(|g| g.fallback_pid().as_u32())
                else {
                    return Vec::new();
                };
                let session_id_map = pid_to_session_id.borrow();
                let Some(session_id) = session_id_map.get(&shell_pid) else {
                    return Vec::new();
                };
                let session_id = session_id.clone();
                vec![(
                    "Fork Session".into(),
                    Box::new(crate::ForkClaudeSession {
                        session_id,
                    }) as Box<dyn Action>,
                )]
            }
        });

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
        let drop_session_state = session_state.clone();
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
                        let session_state = drop_session_state.clone();

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
                                            session_state,
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
                return ControlFlow::Break(());
            } else if let Some(paths) = dropped_item.downcast_ref::<ExternalPaths>() {
                add_paths_to_terminal(pane, paths.paths(), window, cx);
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
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
                            Some(ContextMenu::build(_window, cx, |menu: ui::ContextMenu, _, _| {
                                menu.context(focus_handle.clone())
                                    .action(
                                        "New Claude Code",
                                        NewClaudeCode.boxed_clone(),
                                    )
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
                            ContextMenu::build(window, cx, |menu: ui::ContextMenu, _, _| {
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

        let ready_pids = session_state.ready_shell_pids.clone();
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

fn add_paths_to_terminal(
    pane: &mut Pane,
    paths: &[PathBuf],
    window: &mut Window,
    cx: &mut Context<Pane>,
) {
    if let Some(terminal_view) = pane
        .active_item()
        .and_then(|item| item.downcast::<TerminalView>())
    {
        window.focus(&terminal_view.focus_handle(cx), cx);
        let mut new_text = String::new();
        for path in paths {
            new_text.push(' ');
            new_text.push_str(&format!("{path:?}"));
        }
        new_text.push(' ');
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.terminal().update(cx, |terminal, _| {
                terminal.paste(&new_text);
            });
        });
    }
}
