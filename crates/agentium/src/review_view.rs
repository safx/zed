use anyhow::{Context as _, anyhow};
use buffer_diff::BufferDiff;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::hover_popover::diagnostics_markdown_style;
use editor::{Addon, Editor, EditorEvent, EditorSettings, SplittableEditor, multibuffer_context_lines};
use git::Oid;
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{prelude::*, *};
use language::{Buffer, BufferId, BufferSnapshot, Capability, LanguageRegistry, Point};
use markdown::{CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement};
use multi_buffer::{ExcerptBoundaryInfo, MultiBuffer, PathKey, ToOffset as _};
use project::{Project, ProjectEntryId, ProjectPath};
use settings::Settings;
use std::any::TypeId;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use ui::{ActiveTheme, Color, Icon, IconName, h_flex};
use util::rel_path::RelPath;
use workspace::ItemHandle as _;
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ProjectItem};
use workspace::searchable::SearchableItemHandle;

use crate::AgentiumWorkspaceHandle;

#[derive(serde::Deserialize)]
struct ReviewDocument {
    base: String,
    target: String,
    #[serde(default)]
    groups: Vec<ReviewGroup>,
}

#[derive(serde::Deserialize)]
struct ReviewGroup {
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    hunks: Vec<ReviewHunk>,
    #[serde(default)]
    review: Option<ReviewSection>,
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

#[derive(Clone)]
struct GroupAnchor {
    index: usize,
    title: String,
    summary: Option<String>,
    first_hunk_anchor: multi_buffer::Anchor,
}

pub struct ReviewItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    workspace_entity: Entity<Workspace>,
    rhs_multibuffer: Entity<MultiBuffer>,
    group_anchors: Vec<GroupAnchor>,
    path_to_buffer: HashMap<String, Entity<Buffer>>,
    comments: Vec<ReviewComment>,
    buffer_id_to_group_number: Arc<HashMap<BufferId, usize>>,
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

        let path = path.clone();
        let project = project.clone();
        Some(cx.spawn(async move |cx| {
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
            // For each group, the location (path + range) of its first hunk; used to
            // compute the group header anchor after MultiBuffer is built.
            let mut group_first_hunk_loc: Vec<Option<(String, Range<Point>)>> =
                Vec::with_capacity(document.groups.len());

            for group in &document.groups {
                let mut group_first: Option<(String, Range<Point>)> = None;
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
                    if group_first.is_none() {
                        group_first = Some((hunk.path.clone(), range.clone()));
                    }
                }
                group_first_hunk_loc.push(group_first);
            }

            // Collect comments from all groups, flattened.
            let comments: Vec<ReviewComment> = document
                .groups
                .iter()
                .flat_map(|g| g.review.iter().flat_map(|r| r.comments.iter().cloned()))
                .collect();

            let group_titles_summaries: Vec<(String, Option<String>)> = document
                .groups
                .iter()
                .map(|g| (g.title.clone(), g.summary.clone()))
                .collect();

            // path_to_buffer for review_view's later block-anchor computations.
            let path_to_buffer: HashMap<String, Entity<Buffer>> = path_to_loaded
                .iter()
                .map(|(p, (b, _))| (p.clone(), b.clone()))
                .collect();

            // Build the unified rhs MultiBuffer synchronously.
            let (rhs_multibuffer, group_anchors, buffer_id_to_group_number) = cx.update(
                |cx| -> anyhow::Result<(
                    Entity<MultiBuffer>,
                    Vec<GroupAnchor>,
                    Arc<HashMap<BufferId, usize>>,
                )> {
                    let rhs_multibuffer = cx.new(|cx| {
                        let mut mb = MultiBuffer::new(Capability::ReadOnly);
                        mb.set_all_diff_hunks_expanded(cx);
                        mb
                    });

                    let context_lines = multibuffer_context_lines(cx);
                    let mut path_keys: HashMap<String, PathKey> = HashMap::new();
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
                            path_keys.insert(path_str.clone(), path_key.clone());
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

                    let mb_snapshot = rhs_multibuffer.read(cx).snapshot(cx);
                    let mut group_anchors = Vec::with_capacity(group_first_hunk_loc.len());
                    for (gi, loc) in group_first_hunk_loc.iter().enumerate() {
                        let Some((path_str, _)) = loc else {
                            log::warn!("review: group {gi} has no hunks, header skipped");
                            continue;
                        };
                        let Some(path_key) = path_keys.get(path_str) else {
                            log::warn!(
                                "review: group {gi} first hunk path {path_str} not loaded, header skipped",
                            );
                            continue;
                        };
                        // Anchor at the start of the file's first excerpt so the group
                        // header renders right below the file's BufferHeader, above all
                        // hunk content (including context lines).
                        let Some(mb_anchor) =
                            rhs_multibuffer.read(cx).location_for_path(path_key, cx)
                        else {
                            log::warn!(
                                "review: group {gi} no excerpt for {path_str}, header skipped",
                            );
                            continue;
                        };
                        let (title, summary) = group_titles_summaries[gi].clone();
                        group_anchors.push(GroupAnchor {
                            index: gi,
                            title,
                            summary,
                            first_hunk_anchor: mb_anchor,
                        });
                    }
                    // Sort by buffer offset so the on-screen order of group headers
                    // matches reading order. When a path is shared across groups,
                    // document order may differ from row order.
                    group_anchors.sort_by_key(|ga| ga.first_hunk_anchor.to_offset(&mb_snapshot));

                    // Map each buffer to the document-order group number it first
                    // appears in. First-occurrence wins when a file participates in
                    // multiple groups; this preserves the review.json's domain
                    // ordering (e.g. review_priority) over display-row order.
                    let mut buffer_id_to_group_number: HashMap<BufferId, usize> = HashMap::new();
                    for (gi, group) in document.groups.iter().enumerate() {
                        for hunk in &group.hunks {
                            let Some(buffer) = path_to_buffer.get(&hunk.path) else {
                                continue;
                            };
                            let id = buffer.read(cx).remote_id();
                            buffer_id_to_group_number.entry(id).or_insert(gi + 1);
                        }
                    }

                    Ok((
                        rhs_multibuffer,
                        group_anchors,
                        Arc::new(buffer_id_to_group_number),
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
                group_anchors,
                path_to_buffer,
                comments,
                buffer_id_to_group_number,
            }))
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
}

impl EventEmitter<EditorEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.splittable.focus_handle(cx)
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.splittable.clone()
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
            group_anchors,
            path_to_buffer,
            comments,
            buffer_id_to_group_number,
        ) = {
            let item_ref = item.read(cx);
            (
                item_ref.workspace_entity.clone(),
                item_ref.rhs_multibuffer.clone(),
                item_ref.group_anchors.clone(),
                item_ref.path_to_buffer.clone(),
                item_ref.comments.clone(),
                item_ref.buffer_id_to_group_number.clone(),
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

        splittable.update(cx, |s, cx| {
            s.rhs_editor().update(cx, |editor, _cx| {
                editor.register_addon(AgentiumReviewAddon {
                    buffer_id_to_group_number,
                });
            });
        });

        insert_review_blocks(
            &splittable,
            &rhs_multibuffer,
            &group_anchors,
            &path_to_buffer,
            &comments,
            &language_registry,
            cx,
        );

        Self { item, splittable }
    }
}

fn insert_review_blocks(
    splittable: &Entity<SplittableEditor>,
    rhs_multibuffer: &Entity<MultiBuffer>,
    group_anchors: &[GroupAnchor],
    path_to_buffer: &HashMap<String, Entity<Buffer>>,
    comments: &[ReviewComment],
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut Context<ReviewView>,
) {
    let mb_snapshot = rhs_multibuffer.read(cx).snapshot(cx);
    let mut blocks: Vec<BlockProperties<multi_buffer::Anchor>> = Vec::new();

    for ga in group_anchors {
        let title = SharedString::from(format!("#{} {}", ga.index + 1, ga.title));
        let summary = ga.summary.clone().map(SharedString::from);
        let height = if summary.is_some() { 3 } else { 2 };
        blocks.push(BlockProperties {
            placement: BlockPlacement::Above(ga.first_hunk_anchor),
            height: Some(height),
            style: BlockStyle::Sticky,
            priority: 0,
            render: Arc::new(move |bcx: &mut BlockContext| {
                render_group_header(title.clone(), summary.clone(), bcx)
            }),
        });
    }

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
        let model = comment.model.clone().map(SharedString::from);
        let severity = parse_severity(&comment.severity);
        let body_lines = comment.body.lines().count() as u32;
        let height = body_lines.saturating_add(2).clamp(3, 24);

        let markdown_for_render = markdown.clone();
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

fn render_group_header(
    title: SharedString,
    summary: Option<SharedString>,
    bcx: &mut BlockContext,
) -> AnyElement {
    let cx = &*bcx.app;
    let colors = cx.theme().colors();
    let mut content = div().flex().flex_col().child(
        div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors.text)
            .child(title),
    );
    if let Some(summary) = summary {
        content = content.child(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(summary),
        );
    }
    div()
        .px_3()
        .py_1p5()
        .bg(colors.elevated_surface_background)
        .border_b_1()
        .border_color(colors.border)
        .child(content)
        .into_any_element()
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
}
