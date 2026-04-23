use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use file_icons::FileIcons;
use gpui::{prelude::*, *};
use project::{Entry, Project, ProjectEntryId, ProjectPath, Worktree, WorktreeId};
use ui::{ActiveTheme, ContextMenu, Icon, IconName, prelude::*};
use util::ResultExt as _;
use util::rel_path::{RelPath, RelPathBuf};
use workspace::notifications::NotifyResultExt as _;
use workspace::{Item, Workspace};

actions!(
    file_browser,
    [
        ExpandSelectedEntry,
        CollapseSelectedEntry,
        ConfirmEntry,
        SwitchPane,
        RevealInFileManager,
        Cut,
        Copy,
        Paste,
        Duplicate,
        Trash,
    ]
);

enum ActivePane {
    DirTree,
    FileList,
}

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

enum ClipboardEntry {
    Copied { entry_id: ProjectEntryId },
    Cut { entry_id: ProjectEntryId },
}

struct ContextMenuTarget {
    entry_id: ProjectEntryId,
    path: Arc<RelPath>,
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
    dir_tree_selected_index: Option<usize>,

    file_list_entries: Vec<FileListEntry>,
    file_list_scroll: UniformListScrollHandle,
    file_list_selected_index: Option<usize>,

    active_pane: ActivePane,
    focus_handle: FocusHandle,
    clipboard: Option<ClipboardEntry>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    context_menu_entry: Option<ContextMenuTarget>,
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
            dir_tree_selected_index: None,
            file_list_entries: Vec::new(),
            file_list_scroll: UniformListScrollHandle::new(),
            file_list_selected_index: None,
            active_pane: ActivePane::DirTree,
            focus_handle: cx.focus_handle(),
            clipboard: None,
            context_menu: None,
            context_menu_entry: None,
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

    fn dispatch_context(&self) -> KeyContext {
        let mut context = KeyContext::new_with_defaults();
        context.add("FileBrowser");
        context.add("menu");
        context
    }

    // --- Keyboard navigation ---

    fn select_next(
        &mut self,
        _: &menu::SelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_pane {
            ActivePane::DirTree => {
                let count = self.dir_tree_entries.len();
                if count == 0 {
                    return;
                }
                let ix = self
                    .dir_tree_selected_index
                    .map(|i| (i + 1).min(count - 1))
                    .unwrap_or(0);
                self.dir_tree_selected_index = Some(ix);
                self.dir_tree_scroll
                    .scroll_to_item(ix, ScrollStrategy::Nearest);
            }
            ActivePane::FileList => {
                let count = self.file_list_entries.len();
                if count == 0 {
                    return;
                }
                let ix = self
                    .file_list_selected_index
                    .map(|i| (i + 1).min(count - 1))
                    .unwrap_or(0);
                self.file_list_selected_index = Some(ix);
                self.file_list_scroll
                    .scroll_to_item(ix, ScrollStrategy::Nearest);
            }
        }
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_pane {
            ActivePane::DirTree => {
                let count = self.dir_tree_entries.len();
                if count == 0 {
                    return;
                }
                let ix = self
                    .dir_tree_selected_index
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0);
                self.dir_tree_selected_index = Some(ix);
                self.dir_tree_scroll
                    .scroll_to_item(ix, ScrollStrategy::Nearest);
            }
            ActivePane::FileList => {
                let count = self.file_list_entries.len();
                if count == 0 {
                    return;
                }
                let ix = self
                    .file_list_selected_index
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0);
                self.file_list_selected_index = Some(ix);
                self.file_list_scroll
                    .scroll_to_item(ix, ScrollStrategy::Nearest);
            }
        }
        cx.notify();
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.active_pane, ActivePane::DirTree) {
            return;
        }
        let Some(ix) = self.dir_tree_selected_index else {
            return;
        };
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return;
        };

        if !entry.is_expanded {
            let entry_id = entry.entry_id;
            self.expanded_dirs.insert(entry_id);
            self.try_expand_pending_dir(entry_id, cx);
            self.update_dir_tree(cx);
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.active_pane, ActivePane::DirTree) {
            return;
        }
        let Some(ix) = self.dir_tree_selected_index else {
            return;
        };
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return;
        };

        if entry.is_expanded {
            let entry_id = entry.entry_id;
            self.expanded_dirs.remove(&entry_id);
            self.update_dir_tree(cx);
        }
    }

    fn confirm_entry(
        &mut self,
        _: &ConfirmEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_pane {
            ActivePane::DirTree => {
                if let Some(ix) = self.dir_tree_selected_index {
                    self.select_dir(ix, cx);
                }
            }
            ActivePane::FileList => {
                if let Some(ix) = self.file_list_selected_index {
                    self.on_file_list_click(ix, window, cx);
                }
            }
        }
    }

    fn switch_pane(
        &mut self,
        _: &SwitchPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane = match self.active_pane {
            ActivePane::DirTree => ActivePane::FileList,
            ActivePane::FileList => ActivePane::DirTree,
        };
        cx.notify();
    }

    // --- Data updates ---

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

        if let Some(ix) = self.dir_tree_selected_index {
            if ix >= self.dir_tree_entries.len() {
                self.dir_tree_selected_index = self.dir_tree_entries.len().checked_sub(1);
            }
        }

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
            let name = entry.path.file_name().unwrap_or("").to_string();

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

        if let Some(ix) = self.file_list_selected_index {
            if ix >= self.file_list_entries.len() {
                self.file_list_selected_index = self.file_list_entries.len().checked_sub(1);
            }
        }

        cx.notify();
    }

    // --- User interactions ---

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
        self.file_list_selected_index = None;
        self.update_file_list(cx);
    }

    fn select_root_dir(&mut self, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.worktree_id else {
            return;
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };
        if let Some(root) = worktree.read(cx).root_entry() {
            self.selected_dir_id = Some(root.id);
            self.selected_dir_path = Some(root.path.clone());
            self.file_list_selected_index = None;
            self.update_file_list(cx);
        }
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
            .map(|e| {
                matches!(
                    e.kind,
                    project::EntryKind::PendingDir | project::EntryKind::UnloadedDir
                )
            })
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

            self.file_list_selected_index = None;
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

    // --- Context menu ---

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry_id: ProjectEntryId,
        path: Arc<RelPath>,
        is_dir: bool,
        is_root: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu_entry = Some(ContextMenuTarget {
            entry_id,
            path,
            is_dir,
        });

        let has_pasteable = self.clipboard.is_some();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _cx| {
            menu.context(self.focus_handle.clone())
                .action("Cut", Box::new(Cut))
                .action("Copy", Box::new(Copy))
                .action("Duplicate", Box::new(Duplicate))
                .action_disabled_when(!has_pasteable, "Paste", Box::new(Paste))
                .separator()
                .action("Copy Path", Box::new(zed_actions::workspace::CopyPath))
                .action(
                    "Copy Relative Path",
                    Box::new(zed_actions::workspace::CopyRelativePath),
                )
                .separator()
                .action("Reveal in Finder", Box::new(RevealInFileManager))
                .when(!is_root, |menu| {
                    menu.separator().action("Trash", Box::new(Trash))
                })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            this.context_menu_entry.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn write_entry_to_system_clipboard(&self, path: &RelPath, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.worktree_id else {
            return;
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };
        let abs_path = worktree.read(cx).absolutize(path);
        cx.write_to_clipboard(ClipboardItem::new_string(
            abs_path.to_string_lossy().to_string(),
        ));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = &self.context_menu_entry else {
            return;
        };
        self.write_entry_to_system_clipboard(&target.path, cx);
        self.clipboard = Some(ClipboardEntry::Cut {
            entry_id: target.entry_id,
        });
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = &self.context_menu_entry else {
            return;
        };
        self.write_entry_to_system_clipboard(&target.path, cx);
        self.clipboard = Some(ClipboardEntry::Copied {
            entry_id: target.entry_id,
        });
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.context_menu_entry.as_ref() else {
            return;
        };
        let Some(clipboard) = self.clipboard.take() else {
            return;
        };
        let Some(worktree_id) = self.worktree_id else {
            return;
        };

        let mut dest_dir = target.path.to_rel_path_buf();
        if !target.is_dir {
            dest_dir.pop();
        }

        let (source_entry_id, is_cut) = match &clipboard {
            ClipboardEntry::Cut { entry_id, .. } => (*entry_id, true),
            ClipboardEntry::Copied { entry_id, .. } => (*entry_id, false),
        };

        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };
        let Some(source_entry) = worktree.read(cx).entry_for_id(source_entry_id) else {
            return;
        };
        let Some(file_name) = source_entry.path.file_name() else {
            return;
        };

        let Some(new_path) =
            self.compute_paste_path(&dest_dir, file_name, source_entry.is_file(), worktree.read(cx))
        else {
            return;
        };
        let destination = ProjectPath {
            worktree_id,
            path: new_path,
        };

        if is_cut {
            let task = self.project.update(cx, |project, cx| {
                project.rename_entry(source_entry_id, destination, cx)
            });
            cx.spawn_in(window, async move |_this, _cx| {
                task.await.log_err();
            })
            .detach();
        } else {
            let task = self.project.update(cx, |project, cx| {
                project.copy_entry(source_entry_id, destination, cx)
            });
            cx.spawn_in(window, async move |_this, _cx| {
                task.await.log_err();
            })
            .detach();
        }

        if !is_cut {
            self.clipboard = Some(clipboard);
        }
    }

    fn compute_paste_path(
        &self,
        dest_dir: &RelPathBuf,
        file_name: &str,
        is_file: bool,
        worktree: &Worktree,
    ) -> Option<Arc<RelPath>> {
        let (stem, extension) = if is_file {
            let path = std::path::Path::new(file_name);
            (
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| file_name.to_string()),
                path.extension().map(|s| s.to_string_lossy().to_string()),
            )
        } else {
            (file_name.to_string(), None)
        };

        let mut candidate = dest_dir.clone();
        candidate.push(RelPath::unix(file_name).ok()?);

        if worktree.entry_for_path(&candidate).is_none() {
            return Some(Arc::from(candidate.as_ref()));
        }

        for ix in 0.. {
            let mut new_name = stem.clone();
            new_name.push_str(" copy");
            if ix > 0 {
                new_name.push_str(&format!(" {ix}"));
            }
            if let Some(ext) = &extension {
                new_name.push('.');
                new_name.push_str(ext);
            }
            candidate = dest_dir.clone();
            candidate.push(RelPath::unix(&new_name).ok()?);
            if worktree.entry_for_path(&candidate).is_none() {
                return Some(Arc::from(candidate.as_ref()));
            }
        }
        None
    }

    fn duplicate(&mut self, _: &Duplicate, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        self.paste(&Paste, window, cx);
    }

    fn trash(&mut self, _: &Trash, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.context_menu_entry.as_ref() else {
            return;
        };
        let entry_id = target.entry_id;
        let file_name = target.path.file_name().unwrap_or("this item").to_string();

        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Do you want to trash {file_name}?"),
            None,
            &["Trash", "Cancel"],
            cx,
        );

        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                if let Some(task) = this.project.update(cx, |project, cx| {
                    project.delete_entry(entry_id, true, cx)
                }) {
                    task.detach_and_log_err(cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn context_menu_abs_path(&self, cx: &App) -> Option<PathBuf> {
        let target = self.context_menu_entry.as_ref()?;
        let worktree_id = self.worktree_id?;
        let worktree = self.project.read(cx).worktree_for_id(worktree_id, cx)?;
        Some(worktree.read(cx).absolutize(&target.path))
    }

    fn copy_path(
        &mut self,
        _: &zed_actions::workspace::CopyPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(abs_path) = self.context_menu_abs_path(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(
                abs_path.to_string_lossy().to_string(),
            ));
        }
    }

    fn copy_relative_path(
        &mut self,
        _: &zed_actions::workspace::CopyRelativePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = &self.context_menu_entry {
            let path_style = self.project.read(cx).path_style(cx);
            cx.write_to_clipboard(ClipboardItem::new_string(
                target.path.display(path_style).into_owned(),
            ));
        }
    }

    fn reveal_in_file_manager(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(abs_path) = self.context_menu_abs_path(cx) {
            self.project
                .update(cx, |project, cx| project.reveal_path(&abs_path, cx));
        }
    }

    // --- Rendering ---

    fn root_dir_name(&self, cx: &App) -> SharedString {
        let name = self
            .worktree_id
            .and_then(|id| self.project.read(cx).worktree_for_id(id, cx))
            .map(|wt| wt.read(cx).root_name_str().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            "/".into()
        } else {
            SharedString::from(name)
        }
    }

    fn is_root_selected(&self, cx: &App) -> bool {
        let root_id = self
            .worktree_id
            .and_then(|id| self.project.read(cx).worktree_for_id(id, cx))
            .and_then(|wt| wt.read(cx).root_entry().map(|e| e.id));
        root_id.is_some() && self.selected_dir_id == root_id
    }

    fn render_dir_tree_entry(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let Some(entry) = self.dir_tree_entries.get(ix) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let is_selected_dir = self.selected_dir_id == Some(entry.entry_id);
        let is_cursor = self.dir_tree_selected_index == Some(ix)
            && matches!(self.active_pane, ActivePane::DirTree);
        let entry_id = entry.entry_id;
        let entry_path = entry.path.clone();

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
            .when(is_cursor, |el| el.bg(colors.element_active))
            .when(is_selected_dir && !is_cursor, |el| {
                el.bg(colors.element_selected)
            })
            .when(!is_cursor && !is_selected_dir, |el| {
                el.hover(|style| style.bg(colors.element_hover))
            })
            .child(
                Icon::new(chevron_icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(folder_icon.size(IconSize::Small).color(Color::Muted))
            .child(entry.name.clone())
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.active_pane = ActivePane::DirTree;
                this.dir_tree_selected_index = Some(ix);
                this.select_dir(ix, cx);
                this.toggle_dir(ix, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.active_pane = ActivePane::DirTree;
                    this.dir_tree_selected_index = Some(ix);
                    this.deploy_context_menu(
                        event.position,
                        entry_id,
                        entry_path.clone(),
                        true,
                        false,
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
    }

    fn render_file_list_entry(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let Some(entry) = self.file_list_entries.get(ix) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let is_selected = self.file_list_selected_index == Some(ix);
        let is_cursor = is_selected && matches!(self.active_pane, ActivePane::FileList);
        let entry_id = entry.entry_id;
        let entry_path = entry.path.clone();
        let is_dir = entry.is_dir;

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
            .when(is_cursor, |el| el.bg(colors.element_active))
            .when(is_selected && !is_cursor, |el| {
                el.bg(colors.element_selected)
            })
            .when(!is_selected, |el| {
                el.hover(|style| style.bg(colors.element_hover))
            })
            .child(icon.size(IconSize::Small).color(Color::Muted))
            .child(entry.name.clone())
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.active_pane = ActivePane::FileList;
                this.file_list_selected_index = Some(ix);
                if event.click_count() >= 2 {
                    this.on_file_list_click(ix, window, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.active_pane = ActivePane::FileList;
                    this.file_list_selected_index = Some(ix);
                    this.deploy_context_menu(
                        event.position,
                        entry_id,
                        entry_path.clone(),
                        is_dir,
                        false,
                        window,
                        cx,
                    );
                }),
            )
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
            .key_context(self.dispatch_context())
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::confirm_entry))
            .on_action(cx.listener(Self::switch_pane))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::duplicate))
            .on_action(cx.listener(Self::trash))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("dir-tree-pane")
                    .w(px(200.0))
                    .flex_shrink_0()
                    .border_color(colors.border)
                    .border_r_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.active_pane = ActivePane::DirTree;
                        cx.notify();
                    }))
                    .child({
                        let root_name = self.root_dir_name(cx);
                        let is_root_selected = self.is_root_selected(cx);
                        let root_path: Arc<RelPath> =
                            Arc::from(RelPathBuf::new().as_ref());
                        let root_entry_id = self
                            .worktree_id
                            .and_then(|id| {
                                self.project.read(cx).worktree_for_id(id, cx)
                            })
                            .and_then(|wt| wt.read(cx).root_entry().map(|e| e.id));
                        div()
                            .id("dir-tree-root")
                            .pl(px(4.0))
                            .pr_2()
                            .py_0p5()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(colors.text)
                            .cursor_pointer()
                            .when(is_root_selected, |el| el.bg(colors.element_selected))
                            .hover(|style| style.bg(colors.element_hover))
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Icon::new(IconName::Folder)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(root_name)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.select_root_dir(cx);
                            }))
                            .when_some(root_entry_id, |el, root_id| {
                                el.on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(
                                        move |this, event: &MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            this.active_pane = ActivePane::DirTree;
                                            this.deploy_context_menu(
                                                event.position,
                                                root_id,
                                                root_path.clone(),
                                                true,
                                                true,
                                                window,
                                                cx,
                                            );
                                        },
                                    ),
                                )
                            })
                    })
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
                    .id("file-list-pane")
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.active_pane = ActivePane::FileList;
                        if this.file_list_selected_index.is_none()
                            && !this.file_list_entries.is_empty()
                        {
                            this.file_list_selected_index = Some(0);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.text_muted)
                            .child({
                                let header = self
                                    .selected_dir_path
                                    .as_ref()
                                    .map(|p| {
                                        if p.as_unix_str().is_empty() {
                                            self.root_dir_name(cx)
                                        } else {
                                            SharedString::from(
                                                p.file_name().unwrap_or("/").to_string(),
                                            )
                                        }
                                    })
                                    .unwrap_or_else(|| "Files".into());
                                header
                            }),
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

impl Item for FileBrowserView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "File Browser".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Folder).color(Color::Muted))
    }
}
