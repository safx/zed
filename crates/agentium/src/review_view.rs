use anyhow::{Context as _, anyhow};
use buffer_diff::BufferDiff;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::hover_popover::diagnostics_markdown_style;
use editor::scroll::Autoscroll;
use editor::{
    Editor, EditorEvent, EditorSettings, SelectionEffects, SplittableEditor,
    multibuffer_context_lines,
};
use git::Oid;
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{prelude::*, *};
use language::{Buffer, BufferSnapshot, Capability, LanguageRegistry, Point};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, WrapButtonVisibility,
};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectEntryId, ProjectItem as _, ProjectPath};
use settings::{DiffViewStyle, Settings};
use std::any::TypeId;
use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use ui::{
    ActiveTheme, Button, ButtonCommon, ButtonStyle, Clickable, Color, ContextMenu, FluentBuilder,
    Icon, IconName, LabelSize, Tooltip, h_flex, v_flex,
};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::Toolbar;
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ItemHandle, ProjectItem};
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
struct GroupData {
    number: usize,
    title: SharedString,
    summary: Option<SharedString>,
    focus: Vec<ResolvedFocus>,
    hunks: Vec<ReviewHunk>,
    comments: Vec<ReviewComment>,
}

pub struct ReviewItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    workspace_entity: Entity<Workspace>,
    path_to_loaded: HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)>,
    groups: Vec<GroupData>,
    overall_comments: Vec<ReviewComment>,
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
            anyhow::ensure!(
                !document.groups.is_empty(),
                "review.json {} has no groups",
                abs_path.display(),
            );

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
                    let rel_path = match RelPath::from_unix_str(path_str) {
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

            // Spec (strict 3-way): if review.focus is non-empty use it; otherwise
            // fall back to review_focus as Pending rows; otherwise empty.
            // length mismatch is silent-dropped with a log::warn.
            let resolve_focus = |gi: usize, g: &ReviewGroup| -> Vec<ResolvedFocus> {
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
            };

            let mut groups: Vec<GroupData> = Vec::with_capacity(document.groups.len());
            for (gi, group) in document.groups.iter().enumerate() {
                let number = gi + 1;
                let title = match &group.phase {
                    Some(phase) => format!("{}. [{}] {}", number, phase, group.title),
                    None => format!("{}. {}", number, group.title),
                };
                let comments: Vec<ReviewComment> = group
                    .review
                    .iter()
                    .flat_map(|r| r.comments.iter().cloned())
                    .collect();
                groups.push(GroupData {
                    number,
                    title: SharedString::from(title),
                    summary: group.summary.clone().map(SharedString::from),
                    focus: resolve_focus(gi, group),
                    hunks: group.hunks.clone(),
                    comments,
                });
            }

            let overall_comments: Vec<ReviewComment> =
                document.review.as_ref().map_or_else(Vec::new, |overall| {
                    overall
                        .comments
                        .iter()
                        .map(|c| {
                            let mut c = c.clone();
                            c.overall = true;
                            c
                        })
                        .collect()
                });

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
                path_to_loaded,
                groups,
                overall_comments,
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
    active_group: usize,
    splittable: Entity<SplittableEditor>,
    project: Entity<Project>,
    language_registry: Arc<LanguageRegistry>,
    toolbar: Option<WeakEntity<Toolbar>>,
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
            .path_to_loaded
            .get(&target.path)
            .map(|(b, _)| b.clone());
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

    fn set_active_group(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index == self.active_group || index >= self.item.read(cx).groups.len() {
            return;
        }
        self.active_group = index;

        let weak_review = cx.entity().downgrade();
        let (splittable, sub) = build_splittable_for_group(
            &self.item,
            index,
            &self.project,
            &self.language_registry,
            weak_review,
            window,
            cx,
        );
        self.splittable = splittable;
        self._split_subscription = sub;

        // BufferSearchBar (and other ToolbarItemViews) caches a `WeakEntity<SplittableEditor>`
        // that becomes stale when we swap the inner splittable. Re-fire the toolbar's
        // `set_active_item` path — the same operation `Pane::update_toolbar` performs —
        // so each ToolbarItemView's `set_active_pane_item` re-runs and refreshes its
        // cached handles. Deferred because `Toolbar::set_active_item` calls
        // `item.act_as_type(...)` which reads the ReviewView entity, but we're
        // currently inside its update closure.
        if let Some(toolbar) = self.toolbar.as_ref().and_then(|w| w.upgrade()) {
            let self_weak = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                let Some(self_handle) = self_weak.upgrade() else {
                    return;
                };
                toolbar.update(cx, |toolbar, cx| {
                    toolbar.set_active_item(Some(&self_handle as &dyn ItemHandle), window, cx);
                });
            });
        }

        window.focus(&self.splittable.focus_handle(cx), cx);
        cx.notify();
    }

    fn render_group_header(
        &self,
        group: &GroupData,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let mut content = v_flex().w_full().px_3().py_2().gap_0p5();
        content = content.child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text)
                .child(group.title.clone()),
        );
        if let Some(summary) = group.summary.as_ref() {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(summary.clone()),
            );
        }
        for (focus_idx, item) in group.focus.iter().enumerate() {
            let (icon, color) = match item.result {
                FocusResult::Pass => ("✓", status.success),
                FocusResult::Fail => ("✗", status.error),
                FocusResult::Unsure => ("?", status.warning),
                FocusResult::Pending | FocusResult::Other => ("·", colors.text_muted),
            };
            let row = h_flex()
                .id(SharedString::from(format!(
                    "group-focus-{}-{focus_idx}",
                    group.number
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
        div()
            .w_full()
            .bg(colors.elevated_surface_background)
            .border_b_1()
            .border_color(colors.border)
            .child(content)
    }

    fn render_tab_strip(
        &self,
        groups: &[GroupData],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active = self.active_group;
        h_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .bg(colors.editor_background)
            .border_b_1()
            .border_color(colors.border)
            .children(groups.iter().enumerate().map(|(i, group)| {
                let is_active = i == active;
                Button::new(
                    ("review-group-tab", i),
                    group.number.to_string(),
                )
                .label_size(LabelSize::Small)
                .style(if is_active {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_active_group(i, window, cx);
                }))
            }))
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_group;
        let item = self.item.read(cx);
        let tabs = (!item.groups.is_empty())
            .then(|| self.render_tab_strip(&item.groups, cx).into_any_element());
        let header = item
            .groups
            .get(active)
            .map(|g| self.render_group_header(g, cx).into_any_element());

        v_flex()
            .size_full()
            .key_context("AgentiumReviewView")
            .on_action(cx.listener(Self::copy_comment))
            .on_action(cx.listener(Self::reveal_comment))
            .when_some(tabs, |this, t| this.child(t))
            .when_some(header, |this, h| this.child(h))
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
        pane: Option<&workspace::Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let toolbar = pane.map(|p| p.toolbar().downgrade());

        let weak_review = cx.entity().downgrade();
        let (splittable, split_subscription) = build_splittable_for_group(
            &item,
            0,
            &project,
            &language_registry,
            weak_review,
            window,
            cx,
        );

        Self {
            item,
            active_group: 0,
            splittable,
            project,
            language_registry,
            toolbar,
            context_menu: None,
            context_menu_target: None,
            _split_subscription: split_subscription,
        }
    }
}

fn build_splittable_for_group(
    item: &Entity<ReviewItem>,
    group_index: usize,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    weak_review: WeakEntity<ReviewView>,
    window: &mut Window,
    cx: &mut Context<ReviewView>,
) -> (Entity<SplittableEditor>, Option<Subscription>) {
    let style = EditorSettings::get_global(cx).diff_view_style;

    let (workspace_entity, path_to_loaded, group_hunks, comments_for_tab) = {
        let item_ref = item.read(cx);
        let group = &item_ref.groups[group_index];
        let mut comments: Vec<ReviewComment> = group.comments.clone();
        comments.extend(item_ref.overall_comments.iter().cloned());
        (
            item_ref.workspace_entity.clone(),
            item_ref.path_to_loaded.clone(),
            group.hunks.clone(),
            comments,
        )
    };

    // Per-file aggregation of this group's hunks. Vec preserves first-occurrence
    // order so PathKey sort_prefix tracks the path's first appearance within
    // this group (tab-local; not comparable across groups).
    let mut path_to_ranges: Vec<(String, Vec<Range<Point>>)> = Vec::new();
    for hunk in &group_hunks {
        let start_row = hunk.new_start.saturating_sub(1);
        let end_row = start_row.saturating_add(hunk.new_lines);
        let range = Point::new(start_row, 0)..Point::new(end_row, 0);
        if let Some(idx) = path_to_ranges.iter().position(|(p, _)| p == &hunk.path) {
            path_to_ranges[idx].1.push(range);
        } else {
            path_to_ranges.push((hunk.path.clone(), vec![range]));
        }
    }

    let rhs_multibuffer = cx.new(|cx| {
        let mut mb = MultiBuffer::new(Capability::ReadOnly);
        mb.set_all_diff_hunks_expanded(cx);
        mb
    });

    let context_lines = multibuffer_context_lines(cx);
    rhs_multibuffer.update(cx, |mb, cx| {
        for (prefix, (path_str, ranges)) in path_to_ranges.iter().enumerate() {
            let Some((buffer, diff)) = path_to_loaded.get(path_str) else {
                continue;
            };
            mb.add_diff(diff.clone(), cx);
            let rel_path = match RelPath::from_unix_str(path_str) {
                Ok(p) => p.into_arc(),
                Err(err) => {
                    log::warn!("review: skipping {path_str}: invalid path: {err}");
                    continue;
                }
            };
            let path_key = PathKey::with_sort_prefix(prefix as u64, rel_path);
            let max_row = buffer.read(cx).max_point().row;
            let clamped = ranges.iter().map(|r| {
                let start_row = r.start.row.min(max_row);
                let end_row = r.end.row.min(max_row);
                Point::new(start_row, 0)..Point::new(end_row, 0)
            });
            mb.set_excerpts_for_path(path_key, buffer.clone(), clamped, context_lines, cx);
        }
    });

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

    // `SplittableEditor::new` schedules `split()` via `window.defer` (split.rs:541)
    // and `split()` is what calls `set_companion` on the RHS DisplayMap. Until
    // that runs, custom blocks inserted on the RHS have no entry in the
    // companion's `custom_block_to_balancing_block` map. For side-by-side mode
    // we therefore defer the block insertion until the LHS editor appears.
    let split_subscription = if splittable.read(cx).diff_view_style() == DiffViewStyle::Unified {
        insert_review_blocks(
            &splittable,
            &rhs_multibuffer,
            &path_to_loaded,
            &comments_for_tab,
            language_registry,
            weak_review,
            cx,
        );
        None
    } else {
        let language_registry = language_registry.clone();
        let inserted = Rc::new(Cell::new(false));
        Some(
            cx.observe(&splittable, move |_this, splittable, cx| {
                if inserted.get() {
                    return;
                }
                if splittable.read(cx).lhs_editor().is_none() {
                    return;
                }
                inserted.set(true);
                insert_review_blocks(
                    &splittable,
                    &rhs_multibuffer,
                    &path_to_loaded,
                    &comments_for_tab,
                    &language_registry,
                    weak_review.clone(),
                    cx,
                );
            }),
        )
    };

    (splittable, split_subscription)
}

fn insert_review_blocks(
    splittable: &Entity<SplittableEditor>,
    rhs_multibuffer: &Entity<MultiBuffer>,
    path_to_loaded: &HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)>,
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
        let Some((buffer, _)) = path_to_loaded.get(comment.path.as_str()) else {
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
                    wrap_button_visibility: WrapButtonVisibility::Hidden,
                    border: false,
                },
            ),
        )
        .into_any_element()
}

