use std::cmp;
use std::ops::ControlFlow;
use std::sync::Arc;

use gpui::{prelude::*, *};
use project::Project;
use terminal_view::TerminalView;
use ui::{ActiveTheme, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    pane, move_active_item, move_item, ActivePaneDecorator, ActivateNextPane, ActivatePane,
    ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight, ActivatePaneUp, ActivatePreviousPane,
    AppState, DraggedTab, MoveItemToPane, MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft,
    MovePaneRight, MovePaneUp, NewTerminal, Pane, PaneGroup, SplitDirection, SplitDown,
    SplitLeft, SplitMode, SplitRight, SplitUp, SwapPaneDown, SwapPaneLeft, SwapPaneRight,
    SwapPaneUp, ToggleZoom, Workspace,
};

actions!(agentium, [NewDiffView]);

pub struct AgentiumApp {
    workspaces: Vec<Entity<AgentiumWorkspace>>,
    active_workspace_index: Option<usize>,
    workspace_entity: Entity<Workspace>,
    project: Entity<Project>,
    #[allow(dead_code)]
    app_state: Arc<AppState>,
    next_workspace_id: usize,
    focus_handle: FocusHandle,
}

struct AgentiumWorkspace {
    id: usize,
    name: String,
    active_pane: Entity<Pane>,
    center: PaneGroup,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
}

impl AgentiumApp {
    pub fn new(
        workspace_entity: Entity<Workspace>,
        project: Entity<Project>,
        app_state: Arc<AppState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspaces: Vec::new(),
            active_workspace_index: None,
            workspace_entity,
            project,
            app_state,
            next_workspace_id: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace_id = self.next_workspace_id;
        self.next_workspace_id += 1;
        let name = format!("Workspace {}", workspace_id + 1);
        let workspace_weak = self.workspace_entity.downgrade();
        let project = self.project.clone();

        let workspace_entity = cx.new(|cx| {
            AgentiumWorkspace::new(workspace_id, name, workspace_weak, project, window, cx)
        });

        self.workspaces.push(workspace_entity.clone());
        self.active_workspace_index = Some(self.workspaces.len() - 1);
        let focus = workspace_entity.focus_handle(cx);
        focus.focus(window, cx);
        cx.notify();
    }

    fn switch_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.workspaces.len() {
            self.active_workspace_index = Some(index);
            let focus = self.workspaces[index].focus_handle(cx);
            focus.focus(window, cx);
            cx.notify();
        }
    }

    fn active_workspace(&self) -> Option<&Entity<AgentiumWorkspace>> {
        self.active_workspace_index
            .and_then(|i| self.workspaces.get(i))
    }
}

impl Focusable for AgentiumApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentiumApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active_index = self.active_workspace_index;

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(colors.background)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(220.0))
                    .h_full()
                    .bg(colors.panel_background)
                    .border_r_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .p_2()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.text)
                            .child("Workspaces"),
                    )
                    .child(
                        div()
                            .id("workspace-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .children(self.workspaces.iter().enumerate().map(
                                |(i, workspace_entity)| {
                                    let workspace = workspace_entity.read(cx);
                                    let is_active = Some(i) == active_index;
                                    div()
                                        .id(("workspace", workspace.id))
                                        .px_2()
                                        .py_1()
                                        .mx_1()
                                        .my_px()
                                        .rounded_md()
                                        .text_sm()
                                        .text_color(colors.text)
                                        .cursor_pointer()
                                        .when(is_active, |d| d.bg(colors.element_selected))
                                        .when(!is_active, |d: Stateful<Div>| {
                                            d.hover(|d| d.bg(colors.element_hover))
                                        })
                                        .child(workspace.name.clone())
                                        .on_click(cx.listener(
                                            move |this, _, window, cx| {
                                                this.switch_workspace(i, window, cx)
                                            },
                                        ))
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
                                .child("+ New Workspace")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_workspace(window, cx)
                                })),
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .bg(colors.background)
                    .map(|d| match self.active_workspace() {
                        Some(workspace) => d.child(workspace.clone()),
                        None => d.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size_full()
                                .text_color(colors.text_muted)
                                .child("Press + to add a workspace"),
                        ),
                    }),
            )
    }
}

// --- AgentiumWorkspace ---

impl AgentiumWorkspace {
    fn new(
        id: usize,
        name: String,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = new_agentium_pane(workspace.clone(), project.clone(), window, cx);
        let center = PaneGroup::new(pane.clone());

        let terminal_task = project.update(cx, |project, cx| {
            project.create_terminal_shell(None, cx)
        });
        let workspace_weak = workspace.clone();
        let project_weak = project.downgrade();
        let active_pane = pane.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace_weak,
                        None,
                        project_weak,
                        window,
                        cx,
                    )
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
            active_pane: pane,
            center,
            workspace,
            project,
        }
    }

    fn add_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal_task = self
            .project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx));
        let workspace_weak = self.workspace.clone();
        let project_weak = self.project.downgrade();
        let active_pane = self.active_pane.clone();

        cx.spawn_in(window, async move |_this, cx| {
            let terminal = terminal_task.await?;
            cx.update(|window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace_weak,
                        None,
                        project_weak,
                        window,
                        cx,
                    )
                }));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, true, None, window, cx);
                });
            })?;
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

    fn new_pane_with_terminal(
        &mut self,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let workspace_weak = self.workspace.clone();
        let project = self.project.clone();
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
                    TerminalView::new(
                        terminal,
                        workspace_weak.clone(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                }));
                let pane = new_agentium_pane(workspace_weak, project, window, cx);
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
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(true, cx));
                }
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                }
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

impl Focusable for AgentiumWorkspace {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }
}

impl Render for AgentiumWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_actions_div = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane.clone())
            })
            .unwrap_or_else(div);
        self.workspace
            .update(cx, |workspace, cx| {
                let decorator = ActivePaneDecorator::new(&self.active_pane, &self.workspace);
                search_actions_div
                    .size_full()
                    .child(self.center.render(
                        workspace.zoomed_item(),
                        &decorator,
                        window,
                        cx,
                    ))
            })
            .ok()
            .map(|pane_group| {
                pane_group
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
            })
            .unwrap_or_else(|| div().size_full())
    }
}

fn new_agentium_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut Context<AgentiumWorkspace>,
) -> Entity<Pane> {
    let agentium_workspace = cx.entity().downgrade();

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

        let split_predicate_workspace = agentium_workspace.clone();
        pane.set_can_split(Some(Arc::new(
            move |pane, dragged_item, _window, cx| {
                if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                    let is_current_pane = tab.pane == cx.entity();
                    let Some(can_drag_away) = split_predicate_workspace
                        .read_with(cx, |agentium_workspace, _| {
                            let panes = agentium_workspace.center.panes();
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
                        if let Some(item) = item {
                            return item.downcast::<TerminalView>().is_some();
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

        let drop_project = project.downgrade();
        let drop_agentium_workspace = agentium_workspace.clone();
        let drop_workspace = workspace.clone();
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
                    if item.downcast::<TerminalView>().is_some() {
                        let source = tab.pane.clone();
                        let item_id_to_move = item.item_id();

                        let Some(split_direction) = pane.drag_split_direction() else {
                            return ControlFlow::Continue(());
                        };

                        let workspace_handle = drop_workspace.clone();
                        let agentium_workspace = drop_agentium_workspace.clone();
                        let project = drop_project.clone();

                        cx.spawn_in(window, async move |_, cx| {
                            cx.update(|window, cx| {
                                let Some(project) = project.upgrade() else {
                                    return;
                                };
                                let Ok(new_pane) =
                                    agentium_workspace.update(cx, |workspace, cx| {
                                        let new_pane = new_agentium_pane(
                                            workspace_handle,
                                            project,
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
    });

    cx.subscribe_in(&pane, window, AgentiumWorkspace::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}
