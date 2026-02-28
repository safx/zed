use std::sync::Arc;

use gpui::{prelude::*, *};
use project::Project;
use terminal_view::TerminalView;
use ui::ActiveTheme;
use workspace::{AppState, Workspace};

pub struct AgentiumApp {
    workspaces: Vec<AgentiumWorkspace>,
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
    terminal_view: Entity<TerminalView>,
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

        let terminal_task = self
            .project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx));

        let workspace_weak = self.workspace_entity.downgrade();
        let project_weak = self.project.downgrade();

        let task: Task<anyhow::Result<()>> = cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal_task.await?;
            this.update_in(cx, |this, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace_weak,
                        None,
                        project_weak,
                        window,
                        cx,
                    )
                });
                let focus = terminal_view.focus_handle(cx);
                this.workspaces.push(AgentiumWorkspace {
                    id: workspace_id,
                    name: format!("Workspace {}", workspace_id + 1),
                    terminal_view,
                });
                this.active_workspace_index = Some(this.workspaces.len() - 1);
                focus.focus(window, cx);
                cx.notify();
            })?;
            Ok(())
        });
        task.detach_and_log_err(cx);
    }

    fn switch_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.workspaces.len() {
            self.active_workspace_index = Some(index);
            let focus = self.workspaces[index].terminal_view.focus_handle(cx);
            focus.focus(window, cx);
            cx.notify();
        }
    }

    fn active_workspace(&self) -> Option<&AgentiumWorkspace> {
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
                                |(i, workspace)| {
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
                        Some(workspace) => d.child(workspace.terminal_view.clone()),
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
