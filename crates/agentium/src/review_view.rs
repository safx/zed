use anyhow::{Context as _, anyhow};
use buffer_diff::BufferDiff;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::hover_popover::diagnostics_markdown_style;
use editor::scroll::Autoscroll;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, SelectionEffects, SplittableEditor,
    multibuffer_context_lines,
};
use git::Oid;
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{prelude::*, *};
use language::{Buffer, BufferId, BufferSnapshot, Capability, LanguageRegistry, Point};
use markdown::{CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement};
use multi_buffer::{ExcerptBoundaryInfo, MultiBuffer, PathKey};
use project::{Project, ProjectEntryId, ProjectItem as _, ProjectPath};
use settings::{DiffViewStyle, Settings};
use std::any::TypeId;
use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use ui::{ActiveTheme, Color, ContextMenu, Icon, IconName, Tooltip, h_flex, v_flex};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::ItemHandle as _;
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ProjectItem};
use workspace::searchable::SearchableItemHandle;

use crate::AgentiumWorkspaceHandle;

actions!(agentium_review_view, [CopyComment, RevealComment]);

#[derive(Clone)]
struct CommentTarget {
    path: String,
    line: u32,
    body: String,
}

#[derive(serde::Deserialize)]
struct ReviewDocument {
    base: String,
    target: String,
    #[serde(default)]
    groups: Vec<ReviewGroup>,
    #[serde(default)]
    review: Option<ReviewSection>,
}

#[derive(serde::Deserialize)]
struct ReviewGroup {
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    hunks: Vec<ReviewHunk>,
    #[serde(default)]
    review: Option<ReviewSection>,
    #[serde(default)]
    review_focus: Vec<String>,
}

#[derive(serde::Deserialize, Clone)]
struct ReviewHunk {
    path: String,
    new_start: u32,
    new_lines: u32,
}

#[derive(serde::Deserialize, Default, Clone)]
struct ReviewSection {
    #[serde(default)]
    comments: Vec<ReviewComment>,
    #[serde(default)]
    focus: Vec<ReviewFocus>,
}

#[derive(serde::Deserialize, Default, Clone)]
struct ReviewFocus {
    #[serde(default)]
    desc: String,
    #[serde(default)]
    result: String,
    #[serde(default)]
    reason: String,
}

#[derive(serde::Deserialize, Clone)]
struct ReviewComment {
    path: String,
    line: u32,
    #[serde(default)]
    severity: String,
    body: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    duplicate_of: Option<DuplicateOf>,
    #[serde(skip)]
    overall: bool,
}

#[derive(serde::Deserialize, Clone)]
#[allow(dead_code)]
struct DuplicateOf {
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    reason: String,
}

#[derive(Copy, Clone)]
enum Severity {
    Critical,
    Normal,
    Nit,
}

fn parse_severity(s: &str) -> Severity {
    match s {
        "critical" => Severity::Critical,
        "nit" => Severity::Nit,
        _ => Severity::Normal,
    }
}

#[derive(Copy, Clone)]
enum FocusResult {
    Pass,
    Fail,
    Unsure,
    Pending,
    Other,
}

fn parse_focus_result(s: &str) -> FocusResult {
    match s {
        "pass" => FocusResult::Pass,
        "fail" => FocusResult::Fail,
        "unsure" => FocusResult::Unsure,
        _ => FocusResult::Other,
    }
}

#[derive(Clone)]
struct ResolvedFocus {
    desc: SharedString,
    result: FocusResult,
    reason: SharedString,
}

#[derive(Clone)]
struct GroupHeaderInfo {
    title: SharedString,
    summary: Option<SharedString>,
    focus: Vec<ResolvedFocus>,
}

pub struct ReviewItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    workspace_entity: Entity<Workspace>,
    rhs_multibuffer: Entity<MultiBuffer>,
    path_to_buffer: HashMap<String, Entity<Buffer>>,
    comments: Vec<ReviewComment>,
    buffer_id_to_group_number: Arc<HashMap<BufferId, usize>>,
    buffer_id_to_focus: Arc<HashMap<BufferId, GroupHeaderInfo>>,
}

impl project::ProjectItem for ReviewItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        let is_review_json = path
            .path
            .file_name()
            .is_some_and(|name| name.ends_with(".review.json"));
        if !is_review_json {
            return None;
        }
        log::info!("review: try_open matched for {:?}", path.path);

        let path = path.clone();
        let log_path = format!("{:?}", path.path);
        let project = project.clone();
        Some(cx.spawn(async move |cx| {
            let result: anyhow::Result<Entity<Self>> = async {
            let workspace_entity = cx.update(|cx| -> anyhow::Result<Entity<Workspace>> {
                let handle = cx
                    .try_global::<AgentiumWorkspaceHandle>()
                    .context("AgentiumWorkspaceHandle not registered as global")?;
                Ok(handle.0.clone())
            })?;

            let (fs, abs_path, entry_id, repo) = project.read_with(cx, |project, cx| {
                (
                    project.fs().clone(),
                    project.absolute_path(&path, cx),
                    project.entry_for_path(&path, cx).map(|entry| entry.id),
                    project.git_store().read(cx).active_repository(),
                )
            });
            let abs_path = abs_path.context("no absolute path for review file")?;
            let content = fs
                .load(&abs_path)
                .await
                .with_context(|| format!("reading {}", abs_path.display()))?;
            let document: ReviewDocument = serde_json::from_str(&content)
                .with_context(|| format!("parsing {} as review JSON", abs_path.display()))?;

            let repo = repo.ok_or_else(|| anyhow!("no active git repository"))?;
            let head_sha = repo.read_with(cx, |r, _| {
                r.head_commit.as_ref().map(|c| c.sha.to_string())
            });
            let head_sha = head_sha.ok_or_else(|| anyhow!("repository has no HEAD commit"))?;
            if head_sha != document.target {
                anyhow::bail!(
                    "target {} does not match HEAD {}; checkout target first",
                    document.target,
                    head_sha,
                );
            }

            let worktree_id = path.worktree_id;
            let mut unique_paths: Vec<String> = Vec::new();
            for group in &document.groups {
                for hunk in &group.hunks {
                    if !unique_paths.iter().any(|p| p == &hunk.path) {
                        unique_paths.push(hunk.path.clone());
                    }
                }
            }

            let tree_diff_recv = repo.update(cx, |r, cx| {
                r.diff_tree(
                    DiffTreeType::Since {
                        base: document.base.clone().into(),
                        head: document.target.clone().into(),
                    },
                    cx,
                )
            });
            let tree_diff = tree_diff_recv
                .await
                .context("diff_tree task canceled")??;

            struct PathPlan {
                path_str: String,
                project_path: ProjectPath,
                old_oid: Option<Oid>,
            }
            let plans: Vec<PathPlan> = repo.read_with(cx, |repo, cx| {
                let mut plans: Vec<PathPlan> = Vec::new();
                for path_str in &unique_paths {
                    let rel_path = match RelPath::unix(path_str) {
                        Ok(p) => p.into_arc(),
                        Err(err) => {
                            log::warn!("review: skipping {path_str}: invalid path: {err}");
                            continue;
                        }
                    };
                    let project_path = ProjectPath {
                        worktree_id,
                        path: rel_path,
                    };
                    let Some(repo_path) = repo.project_path_to_repo_path(&project_path, cx) else {
                        log::warn!("review: skipping {path_str}: not found in active repo");
                        continue;
                    };
                    let old_oid = match tree_diff.entries.get(&repo_path) {
                        Some(TreeDiffStatus::Modified { old }) => Some(*old),
                        Some(TreeDiffStatus::Deleted { .. }) => {
                            log::warn!("review: skipping {path_str}: deleted in target");
                            continue;
                        }
                        Some(TreeDiffStatus::Added) | None => None,
                    };
                    plans.push(PathPlan {
                        path_str: path_str.clone(),
                        project_path,
                        old_oid,
                    });
                }
                plans
            });

            let mut tasks = Vec::with_capacity(plans.len());
            for plan in plans {
                let project = project.clone();
                let repo = repo.clone();
                tasks.push(cx.spawn(async move |cx| {
                    let log_path = plan.path_str.clone();
                    let result: anyhow::Result<(String, Entity<Buffer>, Entity<BufferDiff>)> = async {
                        let buffer_task = project.update(cx, |project, cx| {
                            project.open_buffer(plan.project_path.clone(), cx)
                        });
                        let buffer = buffer_task
                            .await
                            .with_context(|| format!("opening buffer {log_path}"))?;
                        let diff_task = project.update(cx, |project, cx| {
                            project.git_store().update(cx, |git_store, cx| {
                                git_store.open_diff_since(
                                    plan.old_oid,
                                    buffer.clone(),
                                    repo.clone(),
                                    cx,
                                )
                            })
                        });
                        let diff = diff_task
                            .await
                            .with_context(|| format!("loading diff for {log_path}"))?;
                        Ok((plan.path_str, buffer, diff))
                    }
                    .await;
                    match result {
                        Ok(loaded) => Some(loaded),
                        Err(err) => {
                            log::warn!("review: skipping {log_path}: {err:?}");
                            None
                        }
                    }
                }));
            }
            let loaded: Vec<(String, Entity<Buffer>, Entity<BufferDiff>)> =
                futures::future::join_all(tasks)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();

            let path_to_loaded: HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)> = loaded
                .into_iter()
                .map(|(p, b, d)| (p, (b, d)))
                .collect();

            // Per-file merging: collect all hunks for each path across all groups.
            // Vec preserves first-occurrence order so PathKey sort_prefix tracks
            // the path's first appearance in the document.
            let mut path_to_ranges: Vec<(String, (u64, Vec<Range<Point>>))> = Vec::new();
            let mut next_prefix: u64 = 0;

            for group in &document.groups {
                for hunk in &group.hunks {
                    let start_row = hunk.new_start.saturating_sub(1);
                    let end_row = start_row.saturating_add(hunk.new_lines);
                    let range = Point::new(start_row, 0)..Point::new(end_row, 0);
                    let idx = if let Some(idx) = path_to_ranges
                        .iter()
                        .position(|(p, _)| p == &hunk.path)
                    {
                        idx
                    } else {
                        let p = next_prefix;
                        next_prefix = next_prefix.saturating_add(1);
                        path_to_ranges.push((hunk.path.clone(), (p, Vec::new())));
                        path_to_ranges.len() - 1
                    };
                    path_to_ranges[idx].1.1.push(range.clone());
                }
            }

            // Collect comments from all groups, plus the overall reviewer's
            // top-level comments. Overall ones are tagged so the model label
            // can be rendered as "model (overall)".
            let mut comments: Vec<ReviewComment> = document
                .groups
                .iter()
                .flat_map(|g| g.review.iter().flat_map(|r| r.comments.iter().cloned()))
                .collect();
            if let Some(overall) = &document.review {
                for c in &overall.comments {
                    let mut c = c.clone();
                    c.overall = true;
                    comments.push(c);
                }
            }

            // Spec (strict 3-way): if review.focus is non-empty use it; otherwise
            // fall back to review_focus as Pending rows; otherwise empty.
            // length mismatch is silent-dropped with a log::warn.
            let group_focus: Vec<Vec<ResolvedFocus>> = document
                .groups
                .iter()
                .enumerate()
                .map(|(gi, g)| {
                    let post: &[ReviewFocus] = g
                        .review
                        .as_ref()
                        .map(|s| s.focus.as_slice())
                        .unwrap_or(&[]);
                    let pre: &[String] = &g.review_focus;
                    if !post.is_empty() {
                        if pre.len() > post.len() {
                            log::warn!(
                                "review: group {gi}: review_focus ({}) > review.focus ({}), {} dropped",
                                pre.len(),
                                post.len(),
                                pre.len() - post.len(),
                            );
                        }
                        post.iter()
                            .map(|f| ResolvedFocus {
                                desc: SharedString::from(f.desc.clone()),
                                result: parse_focus_result(&f.result),
                                reason: SharedString::from(f.reason.clone()),
                            })
                            .collect()
                    } else {
                        pre.iter()
                            .map(|desc| ResolvedFocus {
                                desc: SharedString::from(desc.clone()),
                                result: FocusResult::Pending,
                                reason: SharedString::default(),
                            })
                            .collect()
                    }
                })
                .collect();

            // path_to_buffer for review_view's later block-anchor computations.
            let path_to_buffer: HashMap<String, Entity<Buffer>> = path_to_loaded
                .iter()
                .map(|(p, (b, _))| (p.clone(), b.clone()))
                .collect();

            // Build the unified rhs MultiBuffer synchronously.
            let (rhs_multibuffer, buffer_id_to_group_number, buffer_id_to_focus) = cx.update(
                |cx| -> anyhow::Result<(
                    Entity<MultiBuffer>,
                    Arc<HashMap<BufferId, usize>>,
                    HashMap<BufferId, GroupHeaderInfo>,
                )> {
                    let rhs_multibuffer = cx.new(|cx| {
                        let mut mb = MultiBuffer::new(Capability::ReadOnly);
                        mb.set_all_diff_hunks_expanded(cx);
                        mb
                    });

                    let context_lines = multibuffer_context_lines(cx);
                    rhs_multibuffer.update(cx, |mb, cx| {
                        for (path_str, (prefix, ranges)) in &path_to_ranges {
                            let Some((buffer, diff)) = path_to_loaded.get(path_str) else {
                                continue;
                            };
                            mb.add_diff(diff.clone(), cx);
                            let rel_path = match RelPath::unix(path_str) {
                                Ok(p) => p.into_arc(),
                                Err(err) => {
                                    log::warn!("review: skipping {path_str}: invalid path: {err}");
                                    continue;
                                }
                            };
                            let path_key = PathKey::with_sort_prefix(*prefix, rel_path);
                            let max_row = buffer.read(cx).max_point().row;
                            let clamped = ranges.iter().map(|r| {
                                let start_row = r.start.row.min(max_row);
                                let end_row = r.end.row.min(max_row);
                                Point::new(start_row, 0)..Point::new(end_row, 0)
                            });
                            mb.set_excerpts_for_path(
                                path_key,
                                buffer.clone(),
                                clamped,
                                context_lines,
                                cx,
                            );
                        }
                    });

                    // Map each buffer to the document-order group number it first
                    // appears in. First-occurrence wins when a file participates in
                    // multiple groups; this preserves the review.json's domain
                    // ordering (e.g. review_priority) over display-row order.
                    let mut buffer_id_to_group_number: HashMap<BufferId, usize> = HashMap::new();
                    let mut buffer_id_to_focus: HashMap<BufferId, GroupHeaderInfo> =
                        HashMap::default();
                    for (gi, group) in document.groups.iter().enumerate() {
                        let mut first_buffer_in_group = true;
                        for hunk in &group.hunks {
                            let Some(buffer) = path_to_buffer.get(&hunk.path) else {
                                continue;
                            };
                            let id = buffer.read(cx).remote_id();
                            buffer_id_to_group_number.entry(id).or_insert(gi + 1);
                            if first_buffer_in_group {
                                let title = match &group.phase {
                                    Some(phase) => format!("{}. [{}] {}", gi + 1, phase, group.title),
                                    None => format!("{}. {}", gi + 1, group.title),
                                };
                                buffer_id_to_focus
                                    .entry(id)
                                    .or_insert_with(|| GroupHeaderInfo {
                                        title: SharedString::from(title),
                                        summary: group.summary.clone().map(SharedString::from),
                                        focus: group_focus[gi].clone(),
                                    });
                                first_buffer_in_group = false;
                            }
                        }
                    }

                    Ok((
                        rhs_multibuffer,
                        Arc::new(buffer_id_to_group_number),
                        buffer_id_to_focus,
                    ))
                },
            )?;

            log::info!(
                "review: opened {} (base={}, target={}, {} groups, {} unique files)",
                abs_path.display(),
                document.base,
                document.target,
                document.groups.len(),
                path_to_loaded.len(),
            );

            Ok(cx.new(|_| ReviewItem {
                project_path: path,
                entry_id,
                workspace_entity,
                rhs_multibuffer,
                path_to_buffer,
                comments,
                buffer_id_to_group_number,
                buffer_id_to_focus: Arc::new(buffer_id_to_focus),
            }))
            }.await;
            if let Err(ref err) = result {
                log::error!("review: try_open failed for {log_path}: {err:#}");
            }
            result
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

pub struct ReviewView {
    item: Entity<ReviewItem>,
    splittable: Entity<SplittableEditor>,
    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    context_menu_target: Option<CommentTarget>,
    _split_subscription: Option<Subscription>,
}

impl EventEmitter<EditorEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.splittable.focus_handle(cx)
    }
}

impl ReviewView {
    fn deploy_comment_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        target: CommentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu_target = Some(target);
        let focus_handle = self.focus_handle(cx);
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(focus_handle)
                .action("Copy", Box::new(CopyComment))
                .action("Reveal in Editor", Box::new(RevealComment))
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            this.context_menu_target.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn copy_comment(&mut self, _: &CopyComment, _: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.context_menu_target.as_ref() else {
            return;
        };
        let text = format!("At {}: {}\n{}", target.path, target.line, target.body);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn reveal_comment(&mut self, _: &RevealComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.context_menu_target.clone() else {
            return;
        };
        let buffer = self
            .item
            .read(cx)
            .path_to_buffer
            .get(&target.path)
            .cloned();
        let workspace = self.item.read(cx).workspace_entity.clone();
        let Some(buffer) = buffer else {
            return;
        };
        let Some(project_path) = buffer.read(cx).project_path(cx) else {
            return;
        };
        let open_task = workspace.update(cx, |ws, cx| {
            ws.open_path(project_path, None, true, window, cx)
        });
        cx.spawn_in(window, async move |_this, cx| -> Option<()> {
            let item = open_task.await.log_err()?;
            let editor = item.downcast::<Editor>()?;
            editor
                .update_in(cx, |editor, window, cx| {
                    let row = target.line.saturating_sub(1);
                    let pos = Point::new(row, 0);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |s| s.select_ranges([pos..pos]),
                    );
                })
                .log_err()?;
            Some(())
        })
        .detach();
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("AgentiumReviewView")
            .on_action(cx.listener(Self::copy_comment))
            .on_action(cx.listener(Self::reveal_comment))
            .child(self.splittable.clone())
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

impl Item for ReviewView {
    type Event = EditorEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .project_path
            .path
            .file_name()
            .unwrap_or("review")
            .to_string()
            .into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileGeneric).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.item
                .read(cx)
                .project_path
                .path
                .as_unix_str()
                .to_string()
                .into(),
        )
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("agentium review")
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.splittable.update(cx, |s, cx| {
            s.rhs_editor().update(cx, |editor, cx| {
                editor.deactivated(window, cx);
            });
        });
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.splittable.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.splittable.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.splittable.update(cx, |s, cx| {
            s.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx);
            });
        });
    }
}

impl ProjectItem for ReviewView {
    type Item = ReviewItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();

        let (
            workspace_entity,
            rhs_multibuffer,
            path_to_buffer,
            comments,
            buffer_id_to_group_number,
            buffer_id_to_focus,
        ) = {
            let item_ref = item.read(cx);
            (
                item_ref.workspace_entity.clone(),
                item_ref.rhs_multibuffer.clone(),
                item_ref.path_to_buffer.clone(),
                item_ref.comments.clone(),
                item_ref.buffer_id_to_group_number.clone(),
                item_ref.buffer_id_to_focus.clone(),
            )
        };

        let style = EditorSettings::get_global(cx).diff_view_style;

        let splittable = cx.new(|cx| {
            SplittableEditor::new(
                style,
                rhs_multibuffer.clone(),
                project.clone(),
                workspace_entity,
                window,
                cx,
            )
        });

        // SplittableEditor::new internally calls disable_runnables,
        // disable_inline_diagnostics, set_minimap_visibility(Disabled, ...),
        // and start_temporary_diff_override on the rhs editor (split.rs:506-509),
        // so no post-construction setup is needed.

        let extra_heights: HashMap<BufferId, u32> = buffer_id_to_focus
            .iter()
            .map(|(id, info)| {
                let mut height: u32 = 1; // title
                if info.summary.is_some() {
                    height += 1;
                }
                height += info.focus.len() as u32;
                height += 1; // padding
                (*id, height)
            })
            .collect();
        splittable.update(cx, |s, cx| {
            s.rhs_editor().update(cx, |editor, cx| {
                editor.register_addon(AgentiumReviewAddon {
                    buffer_id_to_group_number,
                    buffer_id_to_focus,
                });
                if !extra_heights.is_empty() {
                    editor.set_extra_buffer_header_heights(extra_heights.clone(), cx);
                }
            });
        });

        let weak_review = cx.entity().downgrade();

        // SplittableEditor::new schedules `split()` via `window.defer` (split.rs:541),
        // and `split()` is what calls `set_companion` on the RHS DisplayMap. Until
        // that runs, custom blocks inserted on the RHS have no entry in the
        // companion's `custom_block_to_balancing_block` map, so neither
        // `BlockMapWriter::insert` (block_map.rs:1812) nor later `resize_blocks`
        // (block_map.rs:1881) create or update a matching LHS `Block::Spacer`.
        // The LHS then drifts below the comment block — most visibly when the
        // markdown text wraps differently at varying widths, which dynamically
        // resizes the RHS block but not the absent LHS spacer.
        //
        // `set_extra_buffer_header_heights` is also per-editor and is *not*
        // propagated to the companion (see block_map.rs:2119), so we mirror the
        // same heights on the LHS once it exists — without registering the addon
        // there, so the reserved space renders empty and acts as a pure spacer
        // for the review_focus card above each hunk.
        let split_subscription = if splittable.read(cx).diff_view_style()
            == DiffViewStyle::Unified
        {
            insert_review_blocks(
                &splittable,
                &rhs_multibuffer,
                &path_to_buffer,
                &comments,
                &language_registry,
                weak_review,
                cx,
            );
            None
        } else {
            let rhs_multibuffer = rhs_multibuffer.clone();
            let language_registry = language_registry.clone();
            let extra_heights_for_lhs = extra_heights;
            let inserted = Rc::new(Cell::new(false));
            Some(
                cx.observe(&splittable, move |_this, splittable, cx| {
                    if inserted.get() {
                        return;
                    }
                    let Some(lhs_editor) = splittable.read(cx).lhs_editor().cloned() else {
                        return;
                    };
                    inserted.set(true);
                    // `extra_heights_for_lhs` is keyed by RHS buffer IDs (from
                    // `path_to_buffer`), but the LHS multibuffer holds the diff's
                    // *base text* buffer entities — different `BufferId`s — so
                    // calling `set_extra_buffer_header_heights` on the LHS with
                    // the RHS map is silently a no-op. Remap RHS IDs to LHS IDs
                    // via the diff that links them (`diff.buffer_id()` is the RHS
                    // ID; the diff is registered on the LHS multibuffer keyed by
                    // its own base-text buffer ID).
                    let lhs_multibuffer = lhs_editor.read(cx).buffer().clone();
                    let mut lhs_heights: HashMap<BufferId, u32> = HashMap::default();
                    {
                        let mb = lhs_multibuffer.read(cx);
                        for lhs_buffer in mb.all_buffers_iter() {
                            let lhs_id = lhs_buffer.read(cx).remote_id();
                            let Some(diff) = mb.diff_for(lhs_id) else {
                                continue;
                            };
                            let rhs_id = diff.read(cx).buffer_id;
                            if let Some(&h) = extra_heights_for_lhs.get(&rhs_id) {
                                lhs_heights.insert(lhs_id, h);
                            }
                        }
                    }
                    if !lhs_heights.is_empty() {
                        lhs_editor.update(cx, |editor, cx| {
                            editor.set_extra_buffer_header_heights(lhs_heights, cx);
                        });
                    }
                    insert_review_blocks(
                        &splittable,
                        &rhs_multibuffer,
                        &path_to_buffer,
                        &comments,
                        &language_registry,
                        weak_review.clone(),
                        cx,
                    );
                }),
            )
        };

        Self {
            item,
            splittable,
            context_menu: None,
            context_menu_target: None,
            _split_subscription: split_subscription,
        }
    }
}

fn insert_review_blocks(
    splittable: &Entity<SplittableEditor>,
    rhs_multibuffer: &Entity<MultiBuffer>,
    path_to_buffer: &HashMap<String, Entity<Buffer>>,
    comments: &[ReviewComment],
    language_registry: &Arc<LanguageRegistry>,
    weak_review: WeakEntity<ReviewView>,
    cx: &mut Context<ReviewView>,
) {
    let mb_snapshot = rhs_multibuffer.read(cx).snapshot(cx);
    let mut blocks: Vec<BlockProperties<multi_buffer::Anchor>> = Vec::new();

    let mut path_to_snapshot: HashMap<&str, BufferSnapshot> = HashMap::new();
    for comment in comments {
        let is_duplicate = comment.duplicate_of.is_some();
        let Some(buffer) = path_to_buffer.get(comment.path.as_str()) else {
            log::warn!(
                "review: skipping comment {}:{}: path not in any group's hunks",
                comment.path,
                comment.line,
            );
            continue;
        };
        let Some(line_index) = comment.line.checked_sub(1) else {
            log::warn!(
                "review: skipping comment {}:0: line must be 1-based",
                comment.path,
            );
            continue;
        };
        let buffer_snapshot = path_to_snapshot
            .entry(comment.path.as_str())
            .or_insert_with(|| buffer.read(cx).snapshot());
        let max_row = buffer_snapshot.max_point().row;
        if line_index > max_row {
            log::warn!(
                "review: skipping comment {}:{}: past EOF (max row {})",
                comment.path,
                comment.line,
                max_row,
            );
            continue;
        }
        let text_anchor = buffer_snapshot.anchor_after(Point::new(line_index, 0));
        let Some(mb_anchor) = mb_snapshot.anchor_in_excerpt(text_anchor) else {
            log::warn!(
                "review: skipping comment {}:{}: line not in any excerpt",
                comment.path,
                comment.line,
            );
            continue;
        };

        let markdown = cx.new(|cx| {
            Markdown::new(
                comment.body.clone().into(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });
        let model = match (comment.model.as_deref(), comment.overall) {
            (Some(m), true) => Some(SharedString::from(format!("{m} (overall)"))),
            (Some(m), false) => Some(SharedString::from(m.to_string())),
            (None, true) => Some(SharedString::from("(overall)")),
            (None, false) => None,
        };
        let severity = parse_severity(&comment.severity);
        let body_lines = comment.body.lines().count() as u32;
        let height = body_lines.saturating_add(2).clamp(3, 24);

        let markdown_for_render = markdown.clone();
        let target = CommentTarget {
            path: comment.path.clone(),
            line: comment.line,
            body: comment.body.clone(),
        };
        let weak_review = weak_review.clone();
        blocks.push(BlockProperties {
            placement: BlockPlacement::Below(mb_anchor),
            height: Some(height),
            style: BlockStyle::Sticky,
            priority: 1,
            render: Arc::new(move |bcx: &mut BlockContext| {
                render_review_comment(
                    severity,
                    model.clone(),
                    markdown_for_render.clone(),
                    is_duplicate,
                    weak_review.clone(),
                    target.clone(),
                    bcx,
                )
            }),
        });
    }

    if blocks.is_empty() {
        return;
    }
    splittable.update(cx, |s, cx| {
        s.rhs_editor().update(cx, |editor, cx| {
            editor.insert_blocks(blocks, None, cx);
        });
    });
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "CRITICAL",
        Severity::Normal => "NORMAL",
        Severity::Nit => "NIT",
    }
}

fn render_review_comment(
    severity: Severity,
    model: Option<SharedString>,
    markdown: Entity<Markdown>,
    is_duplicate: bool,
    weak_review: WeakEntity<ReviewView>,
    target: CommentTarget,
    bcx: &mut BlockContext,
) -> AnyElement {
    let cx = &*bcx.app;
    let status = cx.theme().status();
    let colors = cx.theme().colors();
    let severity_color = match severity {
        Severity::Critical => status.error,
        Severity::Normal => status.warning,
        Severity::Nit => status.hint,
    };
    let (bg, border) = if is_duplicate {
        (colors.element_background, colors.border_variant)
    } else {
        let bg = match severity {
            Severity::Critical => status.error_background,
            Severity::Normal => status.warning_background,
            Severity::Nit => status.hint_background,
        };
        (bg, severity_color)
    };
    let mut style = diagnostics_markdown_style(bcx.window, cx);
    if is_duplicate {
        // The MarkdownElement uses its own MarkdownStyle for body text — a parent
        // div's `text_color` does not propagate. Override the style's base color
        // (and inline-code/link colors) directly so the body actually appears muted.
        let muted = colors.text_muted;
        style.base_text_style.color = muted;
        style.inline_code.color = Some(muted);
        style.link.color = Some(muted);
        if let Some(underline) = style.link.underline.as_mut() {
            underline.color = Some(muted);
        }
    }

    let mut header_row = h_flex().gap_2().items_center().child(
        div()
            .px_1p5()
            .rounded_sm()
            .bg(severity_color)
            .text_color(colors.background)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(severity_label(severity)),
    );
    if is_duplicate {
        header_row = header_row.child(
            div()
                .px_1p5()
                .rounded_sm()
                .bg(colors.element_disabled)
                .text_color(colors.text_muted)
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .child("DUP"),
        );
    }
    if let Some(model) = model {
        header_row = header_row.child(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(model),
        );
    }

    div()
        .pl_2()
        .pr_2()
        .py_1()
        .border_l_2()
        .bg(bg)
        .border_color(border)
        // Stops propagation so the editor's window-level right-click handler
        // (mouse_context_menu::deploy_context_menu in editor::element) does not
        // fire. Depends on block elements being painted after
        // editor::paint_mouse_listeners so they appear later in the LIFO bubble
        // dispatch order.
        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
            cx.stop_propagation();
            let position = event.position;
            let target = target.clone();
            weak_review
                .update(cx, |this, cx| {
                    this.deploy_comment_menu(position, target, window, cx);
                })
                .log_err();
        })
        .child(header_row)
        .child(
            MarkdownElement::new(markdown, style).code_block_renderer(
                CodeBlockRenderer::Default {
                    copy_button_visibility: CopyButtonVisibility::Hidden,
                    border: false,
                },
            ),
        )
        .into_any_element()
}

struct AgentiumReviewAddon {
    buffer_id_to_group_number: Arc<HashMap<BufferId, usize>>,
    buffer_id_to_focus: Arc<HashMap<BufferId, GroupHeaderInfo>>,
}

impl Addon for AgentiumReviewAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_buffer_header_controls(
        &self,
        _: &ExcerptBoundaryInfo,
        buffer: &BufferSnapshot,
        _: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let n = self.buffer_id_to_group_number.get(&buffer.remote_id())?;
        let colors = cx.theme().colors();
        Some(
            div()
                .px_1p5()
                .rounded_sm()
                .bg(colors.element_active)
                .text_color(colors.text)
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("#{n}"))
                .into_any_element(),
        )
    }

    fn render_buffer_header_extra(
        &self,
        _: &ExcerptBoundaryInfo,
        buffer: &BufferSnapshot,
        _: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let info = self.buffer_id_to_focus.get(&buffer.remote_id())?;
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let buffer_id = buffer.remote_id();
        let mut content = v_flex().w_full().px_3().py_1p5().gap_0p5();
        content = content.child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text)
                .child(info.title.clone()),
        );
        if let Some(summary) = &info.summary {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(summary.clone()),
            );
        }
        for (focus_idx, item) in info.focus.iter().enumerate() {
            let (icon, color) = match item.result {
                FocusResult::Pass => ("✓", status.success),
                FocusResult::Fail => ("✗", status.error),
                FocusResult::Unsure => ("?", status.warning),
                FocusResult::Pending | FocusResult::Other => ("·", colors.text_muted),
            };
            let row = h_flex()
                .id(SharedString::from(format!(
                    "buf-focus-{}-{focus_idx}",
                    buffer_id.to_proto()
                )))
                .gap_2()
                .items_center()
                .text_xs()
                .child(
                    div()
                        .w_4()
                        .flex()
                        .justify_center()
                        .text_color(color)
                        .font_weight(FontWeight::BOLD)
                        .child(icon),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(colors.text)
                        .child(item.desc.clone()),
                );
            let row = if !item.reason.is_empty() {
                row.tooltip(Tooltip::text(item.reason.clone()))
            } else {
                row
            };
            content = content.child(row);
        }
        Some(
            div()
                .w_full()
                .bg(colors.elevated_surface_background)
                .border_b_1()
                .border_color(colors.border)
                .child(content)
                .into_any_element(),
        )
    }
}
