use anyhow::{Context as _, anyhow};
use buffer_diff::BufferDiff;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::hover_popover::diagnostics_markdown_style;
use editor::{Editor, EditorMode, MinimapVisibility, multibuffer_context_lines};
use git::Oid;
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{prelude::*, *};
use language::{Buffer, BufferSnapshot, Capability, LanguageRegistry, Point};
use markdown::{CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectEntryId, ProjectPath};
use settings::Settings;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use theme_settings::ThemeSettings;
use ui::{ActiveTheme, Color, Icon, IconName, h_flex};
use util::rel_path::RelPath;
use workspace::item::{Item, ItemBufferKind, ProjectItem};

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

struct PreparedFile {
    path: String,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    excerpt_ranges: Vec<Range<Point>>,
}

struct PreparedGroup {
    files: Vec<PreparedFile>,
}

pub struct ReviewItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    document: ReviewDocument,
    prepared: Vec<PreparedGroup>,
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
            let plans = repo.read_with(cx, |repo, cx| {
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

            let mut prepared_groups: Vec<PreparedGroup> = Vec::with_capacity(document.groups.len());
            for group in &document.groups {
                let mut path_to_ranges: Vec<(String, Vec<Range<Point>>)> = Vec::new();
                for hunk in &group.hunks {
                    let start_row = hunk.new_start.saturating_sub(1);
                    let end_row = start_row.saturating_add(hunk.new_lines);
                    let range = Point::new(start_row, 0)..Point::new(end_row, 0);
                    if let Some((_, ranges)) = path_to_ranges
                        .iter_mut()
                        .find(|(p, _)| p == &hunk.path)
                    {
                        ranges.push(range);
                    } else {
                        path_to_ranges.push((hunk.path.clone(), vec![range]));
                    }
                }
                let files = path_to_ranges
                    .into_iter()
                    .filter_map(|(path_str, ranges)| {
                        let (buffer, diff) = path_to_loaded.get(&path_str)?;
                        Some(PreparedFile {
                            path: path_str,
                            buffer: buffer.clone(),
                            diff: diff.clone(),
                            excerpt_ranges: ranges,
                        })
                    })
                    .collect();
                prepared_groups.push(PreparedGroup { files });
            }

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
                document,
                prepared: prepared_groups,
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

struct GroupView {
    title: String,
    summary: Option<String>,
    editor: Option<Entity<Editor>>,
}

pub struct ReviewView {
    item: Entity<ReviewItem>,
    groups: Vec<GroupView>,
    focus_handle: FocusHandle,
}

impl EventEmitter<()> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(editor) = self.groups.iter().find_map(|g| g.editor.as_ref()) {
            editor.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut root = div()
            .id("review-view-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .px_4()
            .py_3();

        if self.groups.is_empty() {
            return root
                .items_center()
                .justify_center()
                .text_color(colors.text_muted)
                .child("No review groups")
                .into_any_element();
        }

        for (idx, group) in self.groups.iter().enumerate() {
            let number = idx + 1;
            let mut header = div().flex().flex_col().gap_1().child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child(format!("{number}. {}", group.title)),
            );
            if let Some(summary) = &group.summary {
                header = header.child(
                    div()
                        .text_sm()
                        .text_color(colors.text_muted)
                        .child(summary.clone()),
                );
            }

            let body: AnyElement = if let Some(editor) = &group.editor {
                div()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .overflow_hidden()
                    .child(editor.clone())
                    .into_any_element()
            } else {
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child("(no hunks)")
                    .into_any_element()
            };

            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(header)
                    .child(body),
            );
        }

        root.into_any_element()
    }
}

impl Item for ReviewView {
    type Event = ();

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

        let groups_data: Vec<(
            String,
            Option<String>,
            Vec<(String, Entity<BufferDiff>, Entity<Buffer>, Vec<Range<Point>>)>,
            Vec<ReviewComment>,
        )> = {
            let item_ref = item.read(cx);
            item_ref
                .document
                .groups
                .iter()
                .zip(item_ref.prepared.iter())
                .map(|(doc_group, prepared_group)| {
                    let files = prepared_group
                        .files
                        .iter()
                        .map(|file| {
                            (
                                file.path.clone(),
                                file.diff.clone(),
                                file.buffer.clone(),
                                file.excerpt_ranges.clone(),
                            )
                        })
                        .collect();
                    let comments = doc_group
                        .review
                        .as_ref()
                        .map(|r| r.comments.clone())
                        .unwrap_or_default();
                    (
                        doc_group.title.clone(),
                        doc_group.summary.clone(),
                        files,
                        comments,
                    )
                })
                .collect()
        };

        let context_lines = multibuffer_context_lines(cx);
        let groups = groups_data
            .into_iter()
            .map(|(title, summary, files, comments)| {
                let editor = if files.is_empty() {
                    None
                } else {
                    let multibuffer = cx.new(|cx| {
                        let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
                        multibuffer.set_all_diff_hunks_expanded(cx);
                        multibuffer
                    });
                    multibuffer.update(cx, |multibuffer, cx| {
                        for (_path, diff, buffer, ranges) in &files {
                            multibuffer.add_diff(diff.clone(), cx);
                            let path_key = PathKey::for_buffer(buffer, cx);
                            let max_row = buffer.read(cx).max_point().row;
                            let clamped = ranges.iter().map(|r| {
                                let start_row = r.start.row.min(max_row);
                                let end_row = r.end.row.min(max_row);
                                Point::new(start_row, 0)..Point::new(end_row, 0)
                            });
                            multibuffer.set_excerpts_for_path(
                                path_key,
                                buffer.clone(),
                                clamped,
                                context_lines,
                                cx,
                            );
                        }
                    });
                    let editor_entity = cx.new(|cx| {
                        let mut editor = Editor::new(
                            EditorMode::AutoHeight {
                                min_lines: 1,
                                max_lines: None,
                            },
                            multibuffer,
                            Some(project.clone()),
                            window,
                            cx,
                        );
                        editor.start_temporary_diff_override();
                        editor.disable_inline_diagnostics();
                        editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
                        let settings = ThemeSettings::get_global(cx);
                        editor.set_text_style_refinement(TextStyleRefinement {
                            font_family: Some(settings.buffer_font.family.clone()),
                            font_features: Some(settings.buffer_font.features.clone()),
                            font_fallbacks: settings.buffer_font.fallbacks.clone(),
                            font_size: Some(settings.buffer_font_size(cx).into()),
                            font_weight: Some(settings.buffer_font.weight),
                            line_height: Some(relative(settings.buffer_line_height.value()).into()),
                            ..Default::default()
                        });
                        editor
                    });
                    insert_review_comment_blocks(
                        &editor_entity,
                        &files,
                        &comments,
                        &language_registry,
                        cx,
                    );
                    Some(editor_entity)
                };
                GroupView {
                    title,
                    summary,
                    editor,
                }
            })
            .collect();

        Self {
            item,
            groups,
            focus_handle: cx.focus_handle(),
        }
    }
}

fn insert_review_comment_blocks(
    editor: &Entity<Editor>,
    files: &[(String, Entity<BufferDiff>, Entity<Buffer>, Vec<Range<Point>>)],
    comments: &[ReviewComment],
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut App,
) {
    if comments.is_empty() {
        return;
    }
    let path_to_buffer: HashMap<&str, Entity<Buffer>> = files
        .iter()
        .map(|(p, _, b, _)| (p.as_str(), b.clone()))
        .collect();

    editor.update(cx, |editor, cx| {
        let mb_snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut path_to_snapshot: HashMap<&str, BufferSnapshot> = HashMap::new();
        let mut blocks = Vec::new();
        for comment in comments {
            let Some(buffer) = path_to_buffer.get(comment.path.as_str()) else {
                log::warn!(
                    "review: skipping comment {}:{}: path not in this group's hunks",
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
                style: BlockStyle::Flex,
                priority: 1,
                render: Arc::new(move |bcx: &mut BlockContext| {
                    render_review_comment(
                        severity,
                        model.clone(),
                        markdown_for_render.clone(),
                        bcx,
                    )
                }),
            });
        }
        if !blocks.is_empty() {
            editor.insert_blocks(blocks, None, cx);
        }
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
    bcx: &mut BlockContext,
) -> AnyElement {
    let cx = &*bcx.app;
    let status = cx.theme().status();
    let colors = cx.theme().colors();
    let (bg, border) = match severity {
        Severity::Critical => (status.error_background, status.error),
        Severity::Normal => (status.warning_background, status.warning),
        Severity::Nit => (status.hint_background, status.hint),
    };
    let style = diagnostics_markdown_style(bcx.window, cx);

    let mut header_row = h_flex().gap_2().items_center().child(
        div()
            .px_1p5()
            .rounded_sm()
            .bg(border)
            .text_color(colors.background)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(severity_label(severity)),
    );
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
