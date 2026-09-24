use std::ops::Range;

use git::repository::RepoPath;
use git::status::{FileStatus, StageStatus};
use git_ui::git_status_icon;
use gpui::{prelude::*, *};
use project::Project;
use project::git_store::{GitStoreEvent, Repository, RepositoryEvent, StatusEntry};
use ui::{ActiveTheme, Checkbox, ContextMenu, ContextMenuEntry, ToggleState, prelude::*};
use workspace::notifications::DetachAndPromptErr as _;
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

enum DiscardKind {
    Discard,
    Restore,
}

impl DiscardKind {
    fn label(&self) -> &'static str {
        match self {
            DiscardKind::Discard => "Discard Changes…",
            DiscardKind::Restore => "Restore File…",
        }
    }
}

enum IgnoreTarget {
    Gitignore,
    InfoExclude,
}

struct EntryMenuState {
    can_stage: bool,
    can_unstage: bool,
    discard: Option<DiscardKind>,
    can_trash: bool,
    can_ignore: bool,
}

fn entry_menu_state(status: FileStatus) -> EntryMenuState {
    let staging = status.staging();
    EntryMenuState {
        can_stage: !staging.is_fully_staged(),
        can_unstage: staging.has_staged(),
        discard: if status.is_created() {
            None
        } else if status.is_deleted() {
            Some(DiscardKind::Restore)
        } else {
            Some(DiscardKind::Discard)
        },
        can_trash: status.is_created(),
        can_ignore: status.is_untracked(),
    }
}

pub(crate) struct GitStatusView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<GitStatusListEntry>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
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
            context_menu: None,
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

        let stage_status = repo
            .read(cx)
            .status_for_path(&status_entry.repo_path)
            .map(|e| e.status.staging())
            .unwrap_or_else(|| status_entry.status.staging());

        let repo_path = status_entry.repo_path.clone();

        if stage_status.is_fully_staged() {
            self.unstage_path(repo_path, cx);
        } else {
            self.stage_path(repo_path, cx);
        }
    }

    fn stage_path(&mut self, repo_path: RepoPath, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        repo.update(cx, |repo, cx| repo.stage_entries(vec![repo_path], cx))
            .detach_and_log_err(cx);
    }

    fn unstage_path(&mut self, repo_path: RepoPath, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        repo.update(cx, |repo, cx| repo.unstage_entries(vec![repo_path], cx))
            .detach_and_log_err(cx);
    }

    fn discard_path(&mut self, repo_path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        let Some(status_entry) = repo.read(cx).status_for_path(&repo_path) else {
            return;
        };

        let path_style = util::paths::PathStyle::Unix;
        let display_path = repo_path.display(path_style);
        let filename = repo_path.file_name().unwrap_or(display_path.as_ref());

        let (message, confirm_label) = if status_entry.status.is_deleted() {
            (
                format!("Are you sure you want to restore {filename}?"),
                "Restore File",
            )
        } else {
            (
                format!("Are you sure you want to discard changes to {filename}?"),
                "Discard Changes",
            )
        };
        let has_staged = status_entry.status.staging().has_staged();

        let prompt = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &[confirm_label, "Cancel"],
            cx,
        );
        let repo_weak = repo.downgrade();
        let project = self.project.clone();

        cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }

            if has_staged {
                repo_weak
                    .update(cx, |repo, cx| {
                        repo.unstage_entries(vec![repo_path.clone()], cx)
                    })?
                    .await?;
            }

            repo_weak
                .update(cx, |repo, cx| {
                    repo.checkout_files("HEAD", vec![repo_path.clone()], cx)
                })?
                .await?;

            let project_path = repo_weak
                .read_with(cx, |repo, cx| repo.repo_path_to_project_path(&repo_path, cx))?;

            if let Some(project_path) = project_path {
                let buffer = project.read_with(cx, |project, cx| {
                    project.buffer_store().read(cx).get_by_path(&project_path)
                });
                if let Some(buffer) = buffer {
                    let reload_task = buffer
                        .update(cx, |buffer, cx| buffer.is_dirty().then(|| buffer.reload(cx)));
                    if let Some(reload_task) = reload_task {
                        reload_task.await?;
                    }
                }
            }

            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to discard changes", window, cx, |e, _, _| {
            Some(format!("{e}"))
        });
    }

    fn trash_path(&mut self, repo_path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        let Some(status_entry) = repo.read(cx).status_for_path(&repo_path) else {
            return;
        };
        let Some(project_path) = repo.read(cx).repo_path_to_project_path(&repo_path, cx) else {
            return;
        };

        let path_style = util::paths::PathStyle::Unix;
        let display_path = repo_path.display(path_style);
        let filename = repo_path.file_name().unwrap_or(display_path.as_ref());
        let has_staged = status_entry.status.staging().has_staged();

        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!("Trash {filename}?"),
            None,
            &["Trash", "Cancel"],
            cx,
        );
        let repo_weak = repo.downgrade();
        let project = self.project.clone();

        cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }

            if has_staged {
                repo_weak
                    .update(cx, |repo, cx| {
                        repo.unstage_entries(vec![repo_path.clone()], cx)
                    })?
                    .await?;
            }

            let task = project.update(cx, |project, cx| project.trash_file(project_path, cx));
            if let Some(task) = task {
                task.await?;
            }

            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to trash file", window, cx, |e, _, _| {
            Some(format!("{e}"))
        });
    }

    fn add_to_ignore(&mut self, repo_path: RepoPath, target: IgnoreTarget, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        let receiver = repo.update(cx, |repo, _cx| match target {
            IgnoreTarget::Gitignore => repo.add_path_to_gitignore(&repo_path, false),
            IgnoreTarget::InfoExclude => repo.add_path_to_git_info_exclude(&repo_path, false),
        });
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            receiver.await?.notify_workspace_async_err(workspace, cx);
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn copy_path(&mut self, repo_path: RepoPath, relative: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository(cx) else {
            return;
        };
        let text = if relative {
            repo_path.display(util::paths::PathStyle::Unix).to_string()
        } else {
            repo.read(cx)
                .repo_path_to_abs_path(&repo_path)
                .to_string_lossy()
                .to_string()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn deploy_entry_context_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(GitStatusListEntry::Entry(status_entry)) = self.entries.get(ix) else {
            return;
        };
        let repo_path = status_entry.repo_path.clone();
        let status = status_entry.status;
        let menu_state = entry_menu_state(status);
        let can_open = !status.is_deleted();
        let this = cx.weak_entity();

        let context_menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.item(
                ContextMenuEntry::new("Stage")
                    .disabled(!menu_state.can_stage)
                    .handler({
                        let this = this.clone();
                        let repo_path = repo_path.clone();
                        move |_window, cx| {
                            this.update(cx, |this, cx| this.stage_path(repo_path.clone(), cx))
                                .ok();
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Unstage")
                    .disabled(!menu_state.can_unstage)
                    .handler({
                        let this = this.clone();
                        let repo_path = repo_path.clone();
                        move |_window, cx| {
                            this.update(cx, |this, cx| this.unstage_path(repo_path.clone(), cx))
                                .ok();
                        }
                    }),
            )
            .when_some(menu_state.discard, |menu, discard_kind| {
                menu.entry(discard_kind.label(), None, {
                    let this = this.clone();
                    let repo_path = repo_path.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.discard_path(repo_path.clone(), window, cx)
                        })
                        .ok();
                    }
                })
            })
            .when(menu_state.can_trash, |menu| {
                menu.entry("Trash File…", None, {
                    let this = this.clone();
                    let repo_path = repo_path.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.trash_path(repo_path.clone(), window, cx)
                        })
                        .ok();
                    }
                })
            })
            .separator()
            .when(can_open, |menu| {
                menu.entry("Open File", None, {
                    let this = this.clone();
                    let repo_path = repo_path.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            // The status list may have been rebuilt while the menu was open.
                            let current_ix = this.entries.iter().position(|entry| {
                                matches!(
                                    entry,
                                    GitStatusListEntry::Entry(status_entry)
                                        if status_entry.repo_path == repo_path
                                )
                            });
                            if let Some(current_ix) = current_ix {
                                this.open_entry(current_ix, window, cx);
                            }
                        })
                        .ok();
                    }
                })
                .separator()
            })
            .entry("Copy Path", None, {
                let this = this.clone();
                let repo_path = repo_path.clone();
                move |_window, cx| {
                    this.update(cx, |this, cx| this.copy_path(repo_path.clone(), false, cx))
                        .ok();
                }
            })
            .entry("Copy Relative Path", None, {
                let this = this.clone();
                let repo_path = repo_path.clone();
                move |_window, cx| {
                    this.update(cx, |this, cx| this.copy_path(repo_path.clone(), true, cx))
                        .ok();
                }
            })
            .when(menu_state.can_ignore, |menu| {
                menu.separator()
                    .entry("Add to .gitignore", None, {
                        let this = this.clone();
                        let repo_path = repo_path.clone();
                        move |_window, cx| {
                            this.update(cx, |this, cx| {
                                this.add_to_ignore(repo_path.clone(), IgnoreTarget::Gitignore, cx)
                            })
                            .ok();
                        }
                    })
                    .entry("Add to .git/info/exclude", None, {
                        let this = this.clone();
                        let repo_path = repo_path.clone();
                        move |_window, cx| {
                            this.update(cx, |this, cx| {
                                this.add_to_ignore(
                                    repo_path.clone(),
                                    IgnoreTarget::InfoExclude,
                                    cx,
                                )
                            })
                            .ok();
                        }
                    })
            })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
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
                                                .child(
                                                    div()
                                                        .min_w_0()
                                                        .flex_shrink_1()
                                                        .truncate()
                                                        .child(SharedString::from(
                                                            status_entry
                                                                .repo_path
                                                                .display(
                                                                    util::paths::PathStyle::Unix,
                                                                )
                                                                .to_string(),
                                                        )),
                                                )
                                                .child(div().flex_grow_1())
                                                .when_some(
                                                    status_entry.diff_stat,
                                                    |d, stat| {
                                                        let status_colors =
                                                            cx.theme().status();
                                                        d.child(
                                                            h_flex()
                                                                .flex_shrink_0()
                                                                .gap_2()
                                                                .text_xs()
                                                                .when(stat.added > 0, |d| {
                                                                    d.child(
                                                                        div()
                                                                            .text_color(
                                                                                status_colors
                                                                                    .created,
                                                                            )
                                                                            .child(format!(
                                                                                "+{}",
                                                                                stat.added
                                                                            )),
                                                                    )
                                                                })
                                                                .when(stat.deleted > 0, |d| {
                                                                    d.child(
                                                                        div()
                                                                            .text_color(
                                                                                status_colors
                                                                                    .deleted,
                                                                            )
                                                                            .child(format!(
                                                                                "-{}",
                                                                                stat.deleted
                                                                            )),
                                                                    )
                                                                }),
                                                        )
                                                    },
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.open_entry(ix, window, cx);
                                                    },
                                                ))
                                                .on_mouse_down(
                                                    MouseButton::Right,
                                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                                        cx.stop_propagation();
                                                        this.deploy_entry_context_menu(
                                                            ix,
                                                            event.position,
                                                            window,
                                                            cx,
                                                        );
                                                    }),
                                                )
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
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
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

#[cfg(test)]
mod tests {
    // No `use super::*`: it would pull in `gpui::test` and shadow `#[test]`.
    use super::{DiscardKind, GitStatusView, entry_menu_state};

    use fs::{FakeFs, Fs as _};
    use git::repository::RepoPath;
    use git::status::{FileStatus, StageStatus, StatusCode, TrackedStatus};
    use gpui::{Entity, TestAppContext, VisualTestContext, WeakEntity};
    use project::{Project, ProjectPath};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use util::path;
    use util::rel_path::rel_path;

    fn init_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    async fn build_view(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<GitStatusView>, &mut VisualTestContext) {
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        cx.read(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .as_local()
                .unwrap()
                .scan_complete()
        })
        .await;
        cx.run_until_parked();

        let (view, cx) = cx.add_window_view(|_, cx| {
            GitStatusView::new(project.clone(), WeakEntity::new_invalid(), cx)
        });
        cx.run_until_parked();
        (project, view, cx)
    }

    // Step 1: entry_menu_state, a pure function.

    #[test]
    fn entry_menu_state_untracked() {
        let state = entry_menu_state(FileStatus::Untracked);
        assert!(state.can_stage);
        assert!(!state.can_unstage);
        assert!(state.discard.is_none());
        assert!(state.can_trash);
        assert!(state.can_ignore);
    }

    #[test]
    fn entry_menu_state_tracked_modified_unstaged() {
        let state = entry_menu_state(FileStatus::worktree(StatusCode::Modified));
        assert!(state.can_stage);
        assert!(!state.can_unstage);
        assert!(matches!(state.discard, Some(DiscardKind::Discard)));
        assert!(!state.can_trash);
        assert!(!state.can_ignore);
    }

    #[test]
    fn entry_menu_state_index_only_modified() {
        let state = entry_menu_state(FileStatus::index(StatusCode::Modified));
        assert!(!state.can_stage);
        assert!(state.can_unstage);
    }

    #[test]
    fn entry_menu_state_partially_staged() {
        let status = FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Modified,
            worktree_status: StatusCode::Modified,
        });
        let state = entry_menu_state(status);
        assert!(state.can_stage);
        assert!(state.can_unstage);
    }

    #[test]
    fn entry_menu_state_deleted() {
        let state = entry_menu_state(FileStatus::worktree(StatusCode::Deleted));
        assert!(matches!(state.discard, Some(DiscardKind::Restore)));
    }

    #[test]
    fn entry_menu_state_conflict() {
        use git::status::{UnmergedStatus, UnmergedStatusCode};

        let status = FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        });
        let state = entry_menu_state(status);
        assert!(matches!(state.discard, Some(DiscardKind::Discard)));
        assert!(state.can_stage);
    }

    #[test]
    fn entry_menu_state_added_in_index() {
        let state = entry_menu_state(FileStatus::index(StatusCode::Added));
        assert!(!state.can_ignore);
        assert!(state.can_trash);
    }

    // Step 2: stage / unstage.

    #[gpui::test]
    async fn stage_path_stages_modified_unstaged_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "content\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", FileStatus::worktree(StatusCode::Modified))],
        );

        let (_project, view, cx) = build_view(fs, cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        view.update(cx, |view, cx| view.stage_path(repo_path("a.txt"), cx));
        cx.run_until_parked();

        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status.staging(), StageStatus::Staged);
    }

    #[gpui::test]
    async fn unstage_path_unstages_staged_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "content\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", FileStatus::index(StatusCode::Added))],
        );

        let (_project, view, cx) = build_view(fs, cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        view.update(cx, |view, cx| view.unstage_path(repo_path("a.txt"), cx));
        cx.run_until_parked();

        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status.staging(), StageStatus::Unstaged);
    }

    #[gpui::test]
    async fn unstage_path_reverts_index_to_head_for_partially_staged_file(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "content\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "a.txt",
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Modified,
                    worktree_status: StatusCode::Modified,
                }),
            )],
        );

        let (_project, view, cx) = build_view(fs, cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        let status_before = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status_before.staging(), StageStatus::PartiallyStaged);

        view.update(cx, |view, cx| view.unstage_path(repo_path("a.txt"), cx));
        cx.run_until_parked();

        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status.staging(), StageStatus::Unstaged);
    }

    #[gpui::test]
    async fn toggle_staged_delegates_to_stage_and_unstage(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "content\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", FileStatus::worktree(StatusCode::Modified))],
        );

        let (_project, view, cx) = build_view(fs, cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        let ix = view
            .read_with(cx, |view, _| {
                view.entries.iter().position(|entry| {
                    matches!(
                        entry,
                        super::GitStatusListEntry::Entry(entry) if entry.repo_path == repo_path("a.txt")
                    )
                })
            })
            .expect("entry should be listed");

        view.update(cx, |view, cx| view.toggle_staged(ix, cx));
        cx.run_until_parked();

        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status.staging(), StageStatus::Staged);

        view.update(cx, |view, cx| view.toggle_staged(ix, cx));
        cx.run_until_parked();

        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert_eq!(status.staging(), StageStatus::Unstaged);
    }

    // Step 3: discard.

    #[gpui::test(iterations = 10)]
    async fn discard_path_prompts_and_cancel_leaves_file_untouched(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "new\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", FileStatus::worktree(StatusCode::Modified))],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update_in(cx, |view, window, cx| {
            view.discard_path(repo_path("a.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        let prompt_message = cx.pending_prompt().expect("prompt should be pending").0;
        assert!(prompt_message.contains("a.txt"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert_eq!(
            fs.load(path!("/project/a.txt").as_ref()).await.unwrap(),
            "new\n"
        );
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test(iterations = 10)]
    async fn discard_path_restores_worktree_content_to_head(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "new\n" }))
            .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", "old\n".to_string())],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        view.update_in(cx, |view, window, cx| {
            view.discard_path(repo_path("a.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard Changes");
        cx.run_until_parked();

        assert_eq!(
            fs.load(path!("/project/a.txt").as_ref()).await.unwrap(),
            "old\n"
        );
        assert!(
            repo.read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
                .is_none(),
            "file should no longer show as changed after discard"
        );
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test(iterations = 10)]
    async fn discard_path_discards_staged_and_unstaged_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "new\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "a.txt",
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Modified,
                    worktree_status: StatusCode::Modified,
                }),
            )],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();

        view.update_in(cx, |view, window, cx| {
            view.discard_path(repo_path("a.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard Changes");
        cx.run_until_parked();

        assert!(
            repo.read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
                .is_none(),
            "both staged and unstaged changes should be discarded"
        );
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test(iterations = 10)]
    async fn discard_path_reloads_dirty_open_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "new\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", FileStatus::worktree(StatusCode::Modified))],
        );

        let (project, view, cx) = build_view(fs.clone(), cx).await;

        let worktree_id = cx.update(|_window, cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path("a.txt").into(),
        };
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "dirty ")], None, cx);
        });
        cx.run_until_parked();
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));

        view.update_in(cx, |view, window, cx| {
            view.discard_path(repo_path("a.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard Changes");
        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            assert!(!buffer.is_dirty());
        });
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test(iterations = 10)]
    async fn discard_path_on_deleted_file_prompts_restore_and_recreates_file(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {} })).await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", "original\n".to_string())],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;
        let repo = view
            .update(cx, |view, cx| view.active_repository(cx))
            .unwrap();
        let status = repo
            .read_with(cx, |repo, _| repo.status_for_path(&repo_path("a.txt")))
            .unwrap()
            .status;
        assert!(status.is_deleted());

        view.update_in(cx, |view, window, cx| {
            view.discard_path(repo_path("a.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        let prompt_message = cx.pending_prompt().expect("prompt should be pending").0;
        assert!(prompt_message.contains("restore"));
        cx.simulate_prompt_answer("Restore File");
        cx.run_until_parked();

        assert_eq!(
            fs.load(path!("/project/a.txt").as_ref()).await.unwrap(),
            "original\n"
        );
        assert!(!cx.has_pending_prompt());
    }

    // Step 4: trash.

    #[gpui::test(iterations = 10)]
    async fn trash_path_prompts_and_cancel_leaves_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "new.txt": "hi\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("new.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update_in(cx, |view, window, cx| {
            view.trash_path(repo_path("new.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(fs.is_file(path!("/project/new.txt").as_ref()).await);
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test(iterations = 10)]
    async fn trash_path_trashes_untracked_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "new.txt": "hi\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("new.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update_in(cx, |view, window, cx| {
            view.trash_path(repo_path("new.txt"), window, cx);
        });

        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Trash");
        cx.run_until_parked();

        assert!(!fs.is_file(path!("/project/new.txt").as_ref()).await);
        assert!(!cx.has_pending_prompt());
    }

    // Step 5: ignore.

    #[gpui::test]
    async fn add_to_ignore_gitignore_appends_line(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "new.txt": "hi\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("new.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update(cx, |view, cx| {
            view.add_to_ignore(repo_path("new.txt"), super::IgnoreTarget::Gitignore, cx)
        });
        cx.run_until_parked();

        let contents = fs.load(path!("/project/.gitignore").as_ref()).await.unwrap();
        assert!(contents.contains("new.txt"));
    }

    #[gpui::test]
    async fn add_to_ignore_info_exclude_appends_line(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "new.txt": "hi\n" }))
            .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("new.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update(cx, |view, cx| {
            view.add_to_ignore(repo_path("new.txt"), super::IgnoreTarget::InfoExclude, cx)
        });
        cx.run_until_parked();

        let contents = fs
            .load(path!("/project/.git/info/exclude").as_ref())
            .await
            .unwrap();
        assert!(contents.contains("new.txt"));
    }

    // Step 6: copy.

    #[gpui::test]
    async fn copy_path_absolute(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "dir": { "a.txt": "hi\n" } }),
        )
        .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("dir/a.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update(cx, |view, cx| {
            view.copy_path(repo_path("dir/a.txt"), false, cx)
        });

        let clipboard_text = cx.read_from_clipboard().unwrap().text().unwrap();
        assert_eq!(clipboard_text, path!("/project/dir/a.txt"));
    }

    #[gpui::test]
    async fn copy_path_relative(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "dir": { "a.txt": "hi\n" } }),
        )
        .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[("dir/a.txt", FileStatus::Untracked)],
        );

        let (_project, view, cx) = build_view(fs.clone(), cx).await;

        view.update(cx, |view, cx| view.copy_path(repo_path("dir/a.txt"), true, cx));

        let clipboard_text = cx.read_from_clipboard().unwrap().text().unwrap();
        assert_eq!(clipboard_text, "dir/a.txt");
    }
}
