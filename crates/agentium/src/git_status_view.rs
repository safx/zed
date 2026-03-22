use std::ops::Range;

use git::status::StageStatus;
use git_ui::git_status_icon;
use gpui::{prelude::*, *};
use project::Project;
use project::git_store::{GitStoreEvent, Repository, RepositoryEvent, StatusEntry};
use ui::{ActiveTheme, Checkbox, ToggleState, prelude::*};
use workspace::notifications::NotifyResultExt as _;
use workspace::{Item, Workspace};

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

pub(crate) struct GitStatusView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<GitStatusListEntry>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitStatusView {
    pub(crate) fn new(
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

        let Some(repo) = self.active_repository(cx) else {
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

    fn active_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.project.read(cx).active_repository(cx)
    }

    fn toggle_staged(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(GitStatusListEntry::Entry(status_entry)) = self.entries.get(ix) else {
            return;
        };
        let Some(repo) = self.active_repository(cx) else {
            return;
        };

        let stage_status = repo.read(cx)
            .status_for_path(&status_entry.repo_path)
            .map(|e| e.status.staging())
            .unwrap_or_else(|| status_entry.status.staging());

        let repo_path = status_entry.repo_path.clone();
        let should_stage = !stage_status.is_fully_staged();

        repo.update(cx, |repo, cx| {
            if should_stage {
                repo.stage_entries(vec![repo_path], cx)
            } else {
                repo.unstage_entries(vec![repo_path], cx)
            }
        })
        .detach_and_log_err(cx);
    }

    fn open_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(GitStatusListEntry::Entry(status_entry)) = self.entries.get(ix) else {
            return;
        };
        if status_entry.status.is_deleted() {
            return;
        }
        let Some(repo) = self.active_repository(cx) else {
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
                                        GitStatusListEntry::Entry(status_entry) => {
                                            let toggle_state =
                                                match status_entry.status.staging() {
                                                    StageStatus::Staged => ToggleState::Selected,
                                                    StageStatus::Unstaged => {
                                                        ToggleState::Unselected
                                                    }
                                                    StageStatus::PartiallyStaged => {
                                                        ToggleState::Indeterminate
                                                    }
                                                };

                                            div()
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
                                                .child(
                                                    Checkbox::new(
                                                        ("staged", ix),
                                                        toggle_state,
                                                    )
                                                    .on_click(cx.listener(
                                                        move |this, _, _window, cx| {
                                                            cx.stop_propagation();
                                                            this.toggle_staged(ix, cx);
                                                        },
                                                    )),
                                                )
                                                .child(git_status_icon(status_entry.status))
                                                .child(SharedString::from(
                                                    status_entry
                                                        .repo_path
                                                        .display(
                                                            util::paths::PathStyle::Posix,
                                                        )
                                                        .to_string(),
                                                ))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.open_entry(ix, window, cx);
                                                    },
                                                ))
                                                .into_any_element()
                                        }
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
