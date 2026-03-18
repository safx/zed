use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use file_icons::FileIcons;
use gpui::{prelude::*, *};
use project::{Entry, Project, ProjectEntryId, ProjectPath, Worktree, WorktreeId};
use ui::{ActiveTheme, Icon, IconName, prelude::*};
use util::ResultExt as _;
use util::rel_path::{RelPath, RelPathBuf};
use workspace::notifications::NotifyResultExt as _;
use workspace::{Item, Workspace};

struct DirTreeEntry {
    entry_id: ProjectEntryId,
    path: Arc<RelPath>,
    name: SharedString,
    depth: usize,
    is_expanded: bool,
}

struct FileListEntry {
    entry_id: ProjectEntryId,
    path: Arc<RelPath>,
    name: SharedString,
    is_dir: bool,
}

pub(crate) struct FileBrowserView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    worktree_id: Option<WorktreeId>,

    dir_tree_entries: Vec<DirTreeEntry>,
    expanded_dirs: HashSet<ProjectEntryId>,
    selected_dir_id: Option<ProjectEntryId>,
    selected_dir_path: Option<Arc<RelPath>>,
    dir_tree_scroll: UniformListScrollHandle,

    file_list_entries: Vec<FileListEntry>,
    file_list_scroll: UniformListScrollHandle,

    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl FileBrowserView {
    pub(crate) fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        worktree_id: Option<WorktreeId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&project, {
            let worktree_id = worktree_id;
            move |this, _, event: &project::Event, cx| match event {
                project::Event::WorktreeUpdatedEntries(wt_id, _) => {
                    if Some(*wt_id) == worktree_id {
                        this.update_dir_tree(cx);
                        this.update_file_list(cx);
                    }
                }
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                    this.update_dir_tree(cx);
                    this.update_file_list(cx);
                }
                _ => {}
            }
        }));

        let mut this = Self {
            project,
            workspace,
            worktree_id,
            dir_tree_entries: Vec::new(),
            expanded_dirs: HashSet::new(),
            selected_dir_id: None,
            selected_dir_path: None,
            dir_tree_scroll: UniformListScrollHandle::new(),
            file_list_entries: Vec::new(),
            file_list_scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };

        if let Some(worktree_id) = this.worktree_id {
            if let Some(worktree) = this.project.read(cx).worktree_for_id(worktree_id, cx) {
                if let Some(root) = worktree.read(cx).root_entry() {
                    this.selected_dir_id = Some(root.id);
                    this.selected_dir_path = Some(root.path.clone());
                }
            }
        }

        this.update_dir_tree(cx);
        this.update_file_list(cx);
        this
    }

    fn update_dir_tree(&mut self, cx: &mut Context<Self>) {
        self.dir_tree_entries.clear();

        let Some(worktree_id) = self.worktree_id else {
            cx.notify();
            return;
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            cx.notify();
            return;
        };
        let worktree = worktree.read(cx);

        let root_path = RelPathBuf::new();
        self.build_dir_tree_recursive(&worktree, root_path.as_ref(), 0);
        cx.notify();
    }

    fn build_dir_tree_recursive(
        &mut self,
        worktree: &Worktree,
        parent_path: &RelPath,
        depth: usize,
    ) {
        let mut dirs: Vec<&Entry> = worktree
            .child_entries(parent_path)
            .filter(|entry| entry.is_dir())
            .collect();

        dirs.sort_by(|a, b| {
            let a_name = a.path.file_name().unwrap_or("");
            let b_name = b.path.file_name().unwrap_or("");
            a_name.to_lowercase().cmp(&b_name.to_lowercase())
        });

        for entry in dirs {
            let is_expanded = self.expanded_dirs.contains(&entry.id);
            let name = entry
                .path
                .file_name()
                .unwrap_or("")
                .to_string();

            self.dir_tree_entries.push(DirTreeEntry {
                entry_id: entry.id,
                path: entry.path.clone(),
                name: SharedString::from(name),
                depth,
                is_expanded,
            });

            if is_expanded {
                self.build_dir_tree_recursive(worktree, &entry.path, depth + 1);
            }
        }
    }

    fn update_file_list(&mut self, cx: &mut Context<Self>) {
        self.file_list_entries.clear();

        let Some(worktree_id) = self.worktree_id else {
            cx.notify();
            return;
        };
        let Some(selected_path) = self.selected_dir_path.as_ref() else {
            cx.notify();
            return;
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            cx.notify();
            return;
        };
        let worktree = worktree.read(cx);

        let mut dirs: Vec<&Entry> = Vec::new();
        let mut files: Vec<&Entry> = Vec::new();

        for entry in worktree.child_entries(selected_path) {
            if entry.is_dir() {
                dirs.push(entry);
            } else {
                files.push(entry);
            }
        }

        let sort_by_name = |a: &&Entry, b: &&Entry| {
            let a_name = a.path.file_name().unwrap_or("");
            let b_name = b.path.file_name().unwrap_or("");
            a_name.to_lowercase().cmp(&b_name.to_lowercase())
        };
        dirs.sort_by(sort_by_name);
        files.sort_by(sort_by_name);

        for entry in dirs {
            let name = entry.path.file_name().unwrap_or("").to_string();
            self.file_list_entries.push(FileListEntry {
                entry_id: entry.id,
                path: entry.path.clone(),
                name: SharedString::from(name),
                is_dir: true,
            });
        }
        for entry in files {
            let name = entry.path.file_name().unwrap_or("").to_string();
            self.file_list_entries.push(FileListEntry {
                entry_id: entry.id,
                path: entry.path.clone(),
                name: SharedString::from(name),
                is_dir: false,
            });
        }

        cx.notify();
    }

    fn toggle_dir(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return;
        };
        let entry_id = entry.entry_id;

        if self.expanded_dirs.contains(&entry_id) {
            self.expanded_dirs.remove(&entry_id);
        } else {
            self.expanded_dirs.insert(entry_id);
            self.try_expand_pending_dir(entry_id, cx);
        }

        self.update_dir_tree(cx);
    }

    fn select_dir(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return;
        };
        self.selected_dir_id = Some(entry.entry_id);
        self.selected_dir_path = Some(entry.path.clone());
        self.update_file_list(cx);
    }

    fn try_expand_pending_dir(&self, entry_id: ProjectEntryId, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.worktree_id else {
            return;
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };

        let entry = worktree.read(cx).entry_for_id(entry_id);
        let needs_expand = entry
            .map(|e| matches!(e.kind, project::EntryKind::PendingDir | project::EntryKind::UnloadedDir))
            .unwrap_or(false);

        if needs_expand {
            if let Some(task) = worktree.update(cx, |wt, cx| wt.expand_entry(entry_id, cx)) {
                cx.spawn(async move |_, _cx| {
                    task.await.log_err();
                })
                .detach();
            }
        }
    }

    fn on_file_list_click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.file_list_entries.get(ix) else {
            return;
        };

        if entry.is_dir {
            self.selected_dir_id = Some(entry.entry_id);
            self.selected_dir_path = Some(entry.path.clone());

            if !self.expanded_dirs.contains(&entry.entry_id) {
                self.expanded_dirs.insert(entry.entry_id);
                self.try_expand_pending_dir(entry.entry_id, cx);
            }

            self.update_dir_tree(cx);
            self.update_file_list(cx);
        } else {
            self.open_file(ix, window, cx);
        }
    }

    fn open_file(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.file_list_entries.get(ix) else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let Some(worktree_id) = self.worktree_id else {
            return;
        };

        let project_path = ProjectPath {
            worktree_id,
            path: entry.path.clone(),
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

    fn render_dir_tree_entry(
        &self,
        ix: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let is_selected = self.selected_dir_id == Some(entry.entry_id);

        let chevron_icon = if entry.is_expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };

        let folder_icon =
            FileIcons::get_folder_icon(entry.is_expanded, entry.path.as_std_path(), cx)
                .map(Icon::from_path)
                .unwrap_or_else(|| Icon::new(IconName::Folder));

        div()
            .id(("dir-tree-entry", ix))
            .pl(px((entry.depth as f32) * 16.0 + 4.0))
            .pr_2()
            .py_0p5()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .text_sm()
            .text_color(colors.text)
            .cursor_pointer()
            .when(is_selected, |el| el.bg(colors.element_selected))
            .hover(|style| style.bg(colors.element_hover))
            .child(
                Icon::new(chevron_icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(folder_icon.size(IconSize::Small).color(Color::Muted))
            .child(entry.name.clone())
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.select_dir(ix, cx);
                this.toggle_dir(ix, cx);
            }))
            .into_any_element()
    }

    fn render_file_list_entry(
        &self,
        ix: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.file_list_entries.get(ix) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();

        let icon = if entry.is_dir {
            FileIcons::get_folder_icon(false, entry.path.as_std_path(), cx)
                .map(Icon::from_path)
                .unwrap_or_else(|| Icon::new(IconName::Folder))
        } else {
            FileIcons::get_icon(entry.path.as_std_path(), cx)
                .map(Icon::from_path)
                .unwrap_or_else(|| Icon::new(IconName::File))
        };

        div()
            .id(("file-list-entry", ix))
            .px_2()
            .py_0p5()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(colors.text)
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .child(icon.size(IconSize::Small).color(Color::Muted))
            .child(entry.name.clone())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.on_file_list_click(ix, window, cx);
            }))
            .into_any_element()
    }
}

impl EventEmitter<()> for FileBrowserView {}

impl Focusable for FileBrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileBrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();

        if self.worktree_id.is_none() {
            return div()
                .track_focus(&self.focus_handle)
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child("No directory")
                .into_any_element();
        }

        let dir_count = self.dir_tree_entries.len();
        let file_count = self.file_list_entries.len();

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .w(px(200.0))
                    .flex_shrink_0()
                    .border_color(colors.border)
                    .border_r_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.text_muted)
                            .child("Directories"),
                    )
                    .child(
                        uniform_list(
                            "dir-tree",
                            dir_count,
                            cx.processor(
                                |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                                    range
                                        .map(|ix| this.render_dir_tree_entry(ix, cx))
                                        .collect()
                                },
                            ),
                        )
                        .flex_1()
                        .track_scroll(&self.dir_tree_scroll),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.text_muted)
                            .child(
                                self.selected_dir_path
                                    .as_ref()
                                    .and_then(|p| {
                                        let s = p.as_unix_str();
                                        if s.is_empty() {
                                            Some("/")
                                        } else {
                                            p.file_name()
                                        }
                                    })
                                    .unwrap_or("Files")
                                    .to_string(),
                            ),
                    )
                    .child(if file_count == 0 {
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(colors.text_muted)
                            .text_sm()
                            .child("Empty directory")
                            .into_any_element()
                    } else {
                        uniform_list(
                            "file-list",
                            file_count,
                            cx.processor(
                                |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                                    range
                                        .map(|ix| this.render_file_list_entry(ix, cx))
                                        .collect()
                                },
                            ),
                        )
                        .flex_1()
                        .track_scroll(&self.file_list_scroll)
                        .into_any_element()
                    }),
            )
            .into_any_element()
    }
}

impl Item for FileBrowserView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "File Browser".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Folder).color(Color::Muted))
    }
}
