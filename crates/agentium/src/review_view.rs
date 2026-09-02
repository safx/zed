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
use git::repository::RepoPath;
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
    ActiveTheme, Button, ButtonCommon, ButtonStyle, Clickable, Color, ContextMenu, Disableable,
    FluentBuilder, Icon, IconName, LabelSize, Toggleable, Tooltip, h_flex, v_flex,
};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::Toolbar;
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ItemHandle, ProjectItem};
use workspace::searchable::SearchableItemHandle;

use crate::AgentiumWorkspaceHandle;

actions!(
    agentium_review_view,
    [CopyComment, RevealComment, ToggleReading]
);

// prism's unified=3 display window: every hunk is shown with this many
// context lines on each side unless a reading plan narrows it.
const READING_CONTEXT_LINES: u32 = 3;

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
    code_review: Option<CodeReview>,
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
    review_focus: Vec<String>,
    // prism drops the key entirely when a group has no findings.
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    reading: Option<Reading>,
}

#[derive(serde::Deserialize, Clone, PartialEq, Eq, Debug)]
struct ReviewHunk {
    path: String,
    new_start: u32,
    new_lines: u32,
}

#[derive(serde::Deserialize, Clone)]
struct Finding {
    file: String,
    line: u32,
    summary: String,
    #[serde(default)]
    failure_scenario: Option<String>,
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct CodeReview {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    unassigned: Vec<Finding>,
}

#[derive(serde::Deserialize, Clone)]
struct Reading {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    override_hunks: Vec<OverrideHunk>,
}

#[derive(serde::Deserialize, Clone)]
struct OverrideHunk {
    path: String,
    new_start: u32,
    new_lines: u32,
    #[serde(default)]
    hide: bool,
    #[serde(default)]
    show: Vec<ShowBlock>,
}

// Line numbers are absolute, 1-based, new-file side, inclusive on both ends.
#[derive(serde::Deserialize, Clone)]
struct ShowBlock {
    lines: [u32; 2],
    #[serde(default)]
    omit: Vec<[u32; 2]>,
}

#[derive(Copy, Clone)]
enum Verdict {
    Confirmed,
    Plausible,
    Unlabeled,
}

fn parse_verdict(verdict: Option<&str>) -> Verdict {
    match verdict {
        Some("CONFIRMED") => Verdict::Confirmed,
        Some("PLAUSIBLE") => Verdict::Plausible,
        _ => Verdict::Unlabeled,
    }
}

#[derive(Clone)]
struct GroupData {
    tab_label: SharedString,
    title: SharedString,
    summary: Option<SharedString>,
    reading_summary: Option<SharedString>,
    review_focus: Vec<SharedString>,
    hunks: Vec<ReviewHunk>,
    reading: Option<Vec<OverrideHunk>>,
    findings: Vec<Finding>,
}

pub struct ReviewItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    workspace_entity: Entity<Workspace>,
    path_to_loaded: HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)>,
    groups: Vec<GroupData>,
    review_level: Option<SharedString>,
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
                let head_sha =
                    repo.read_with(cx, |r, _| r.head_commit.as_ref().map(|c| c.sha.to_string()));
                let head_sha = head_sha.ok_or_else(|| anyhow!("repository has no HEAD commit"))?;
                if head_sha != document.target {
                    anyhow::bail!(
                        "target {} does not match HEAD {}; checkout target first",
                        document.target,
                        head_sha,
                    );
                }

                let unassigned: Vec<Finding> = document
                    .code_review
                    .as_ref()
                    .map(|review| review.unassigned.clone())
                    .unwrap_or_default();
                let mut unique_paths: Vec<String> = Vec::new();
                let hunk_paths = document
                    .groups
                    .iter()
                    .flat_map(|group| group.hunks.iter().map(|hunk| hunk.path.as_str()));
                let unassigned_paths = unassigned.iter().map(|finding| finding.file.as_str());
                for path_str in hunk_paths.chain(unassigned_paths) {
                    if !unique_paths.iter().any(|p| p == path_str) {
                        unique_paths.push(path_str.to_string());
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
                let tree_diff = tree_diff_recv.await.context("diff_tree task canceled")??;

                struct PathPlan {
                    path_str: String,
                    project_path: ProjectPath,
                    old_oid: Option<Oid>,
                }
                let plans: Vec<PathPlan> = repo.read_with(cx, |repo, cx| {
                    let mut plans: Vec<PathPlan> = Vec::new();
                    for path_str in &unique_paths {
                        // avoid review.json's worktree; hunks may differ
                        let repo_path = match RepoPath::new(path_str) {
                            Ok(p) => p,
                            Err(err) => {
                                log::warn!("review: skipping {path_str}: invalid path: {err}");
                                continue;
                            }
                        };
                        let Some(project_path) = repo.repo_path_to_project_path(&repo_path, cx)
                        else {
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
                        let result: anyhow::Result<(String, Entity<Buffer>, Entity<BufferDiff>)> =
                            async {
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

                let path_to_loaded: HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)> =
                    loaded.into_iter().map(|(p, b, d)| (p, (b, d))).collect();

                let mut groups: Vec<GroupData> = Vec::with_capacity(document.groups.len() + 1);
                for (gi, group) in document.groups.iter().enumerate() {
                    let number = gi + 1;
                    let title = match &group.phase {
                        Some(phase) => format!("{}. [{}] {}", number, phase, group.title),
                        None => format!("{}. {}", number, group.title),
                    };
                    groups.push(GroupData {
                        tab_label: SharedString::from(number.to_string()),
                        title: SharedString::from(title),
                        summary: group.summary.clone().map(SharedString::from),
                        reading_summary: group
                            .reading
                            .as_ref()
                            .and_then(|reading| reading.summary.clone())
                            .map(SharedString::from),
                        review_focus: group
                            .review_focus
                            .iter()
                            .cloned()
                            .map(SharedString::from)
                            .collect(),
                        hunks: group.hunks.clone(),
                        reading: group
                            .reading
                            .as_ref()
                            .map(|reading| reading.override_hunks.clone()),
                        findings: group.findings.clone(),
                    });
                }
                if !unassigned.is_empty() {
                    groups.push(unassigned_group(unassigned));
                }
                let review_level = document
                    .code_review
                    .as_ref()
                    .and_then(|review| review.level.clone())
                    .map(SharedString::from);

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
                    review_level,
                }))
            }
            .await;
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

// Findings prism could not map to a group get a synthetic tab whose
// hunks are windows around each finding line.
fn unassigned_group(findings: Vec<Finding>) -> GroupData {
    let hunks = findings
        .iter()
        .map(|finding| ReviewHunk {
            path: finding.file.clone(),
            new_start: finding.line.saturating_sub(READING_CONTEXT_LINES).max(1),
            new_lines: READING_CONTEXT_LINES * 2 + 1,
        })
        .collect();
    GroupData {
        tab_label: SharedString::from("U"),
        title: SharedString::from("Unassigned findings"),
        summary: Some(SharedString::from(
            "Findings prism could not map to any group.",
        )),
        reading_summary: None,
        review_focus: Vec::new(),
        hunks,
        reading: None,
        findings,
    }
}

pub struct ReviewView {
    item: Entity<ReviewItem>,
    active_group: usize,
    show_reading: bool,
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

    fn set_active_group(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.active_group || index >= self.item.read(cx).groups.len() {
            return;
        }
        self.active_group = index;
        self.rebuild_splittable(window, cx);
    }

    fn toggle_reading(&mut self, _: &ToggleReading, window: &mut Window, cx: &mut Context<Self>) {
        let has_reading = self
            .item
            .read(cx)
            .groups
            .get(self.active_group)
            .is_some_and(|group| group.reading.is_some());
        if !has_reading {
            return;
        }
        self.show_reading = !self.show_reading;
        self.rebuild_splittable(window, cx);
    }

    fn rebuild_splittable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak_review = cx.entity().downgrade();
        let (splittable, sub) = build_splittable_for_group(
            &self.item,
            self.active_group,
            self.show_reading,
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

    fn render_group_header(&self, group: &GroupData, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut content = v_flex().w_full().px_3().py_2().gap_0p5();
        content = content.child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text)
                .child(group.title.clone()),
        );
        let reading_active = self.show_reading && group.reading.is_some();
        let summary = if reading_active {
            group.reading_summary.as_ref().or(group.summary.as_ref())
        } else {
            group.summary.as_ref()
        };
        if let Some(summary) = summary {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(summary.clone()),
            );
        }
        for focus in &group.review_focus {
            content = content.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .w_4()
                            .flex()
                            .justify_center()
                            .text_color(colors.text_muted)
                            .font_weight(FontWeight::BOLD)
                            .child("·"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(colors.text)
                            .child(focus.clone()),
                    ),
            );
        }
        div()
            .w_full()
            .bg(colors.elevated_surface_background)
            .border_b_1()
            .border_color(colors.border)
            .child(content)
    }

    fn render_tab_strip(&self, groups: &[GroupData], cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active = self.active_group;
        let has_reading = groups
            .get(active)
            .is_some_and(|group| group.reading.is_some());
        let reading_active = self.show_reading && has_reading;
        let level = self.item.read(cx).review_level.clone();
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
                Button::new(("review-group-tab", i), group.tab_label.clone())
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
            .child(div().flex_1())
            .when_some(level, |this, level| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(format!("review: {level}")),
                )
            })
            .child(
                Button::new("review-reading-toggle", "Reading")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(reading_active)
                    .disabled(!has_reading)
                    .tooltip(Tooltip::text(if has_reading {
                        "Toggle prism reading plan vs. raw diff"
                    } else {
                        "This group has no reading plan"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_reading(&ToggleReading, window, cx);
                    })),
            )
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
            .on_action(cx.listener(Self::toggle_reading))
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
            false,
            &project,
            &language_registry,
            weak_review,
            window,
            cx,
        );

        Self {
            item,
            active_group: 0,
            show_reading: false,
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

// Per-file aggregation of a group's hunks. Vec preserves first-occurrence
// order so PathKey sort_prefix tracks the path's first appearance within
// the group (tab-local; not comparable across groups).
type PathRanges = Vec<(String, Vec<Range<Point>>)>;

fn push_range(path_to_ranges: &mut PathRanges, path: &str, range: Range<Point>) {
    if let Some(idx) = path_to_ranges.iter().position(|(p, _)| p == path) {
        path_to_ranges[idx].1.push(range);
    } else {
        path_to_ranges.push((path.to_string(), vec![range]));
    }
}

fn raw_ranges(hunks: &[ReviewHunk]) -> PathRanges {
    let mut path_to_ranges = PathRanges::new();
    for hunk in hunks {
        let start_row = hunk.new_start.saturating_sub(1);
        let end_row = start_row.saturating_add(hunk.new_lines);
        let range = Point::new(start_row, 0)..Point::new(end_row, 0);
        push_range(&mut path_to_ranges, &hunk.path, range);
    }
    path_to_ranges
}

// Inclusive 1-based line span to a row range. `build_excerpt_ranges` extends
// the end point to the end of its row, so the last line is fully included.
fn line_span(first_line: u32, last_line: u32) -> Option<Range<Point>> {
    if first_line == 0 || last_line < first_line {
        return None;
    }
    Some(Point::new(first_line - 1, 0)..Point::new(last_line - 1, 0))
}

// Excerpts for prism's reading plan. Must be fed to the multibuffer with
// zero context lines: `set_excerpts_for_path` merges adjacent excerpts, so
// any context would close the gaps that `omit` opens. Hunks sharing a file
// naturally union per row through that same merge, matching prism.
fn reading_ranges(hunks: &[ReviewHunk], overrides: &[OverrideHunk]) -> PathRanges {
    let mut path_to_ranges = PathRanges::new();
    for hunk in hunks {
        let policy = overrides.iter().find(|entry| {
            entry.path == hunk.path
                && entry.new_start == hunk.new_start
                && entry.new_lines == hunk.new_lines
        });
        match policy {
            Some(entry) if entry.hide => {}
            Some(entry) if !entry.show.is_empty() => {
                for block in &entry.show {
                    let [first, last] = block.lines;
                    let mut omits = block.omit.clone();
                    omits.sort_by_key(|omit| omit[0]);
                    let mut cursor = first;
                    for [omit_first, omit_last] in omits {
                        if omit_first > cursor {
                            let visible_last = omit_first.saturating_sub(1).min(last);
                            if let Some(range) = line_span(cursor, visible_last) {
                                push_range(&mut path_to_ranges, &hunk.path, range);
                            }
                        }
                        cursor = cursor.max(omit_last.saturating_add(1));
                    }
                    if let Some(range) = line_span(cursor, last) {
                        push_range(&mut path_to_ranges, &hunk.path, range);
                    }
                }
            }
            _ => {
                let first = hunk.new_start.saturating_sub(READING_CONTEXT_LINES).max(1);
                let last = hunk
                    .new_start
                    .saturating_add(hunk.new_lines)
                    .saturating_add(READING_CONTEXT_LINES);
                if let Some(range) = line_span(first, last) {
                    push_range(&mut path_to_ranges, &hunk.path, range);
                }
            }
        }
    }
    path_to_ranges
}

fn build_splittable_for_group(
    item: &Entity<ReviewItem>,
    group_index: usize,
    show_reading: bool,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    weak_review: WeakEntity<ReviewView>,
    window: &mut Window,
    cx: &mut Context<ReviewView>,
) -> (Entity<SplittableEditor>, Option<Subscription>) {
    let style = EditorSettings::get_global(cx).diff_view_style;

    let (workspace_entity, path_to_loaded, path_to_ranges, context_lines, findings_for_tab) = {
        let item_ref = item.read(cx);
        let group = &item_ref.groups[group_index];
        let (path_to_ranges, context_lines) = match group.reading.as_ref() {
            Some(overrides) if show_reading => (reading_ranges(&group.hunks, overrides), 0),
            _ => (raw_ranges(&group.hunks), multibuffer_context_lines(cx)),
        };
        (
            item_ref.workspace_entity.clone(),
            item_ref.path_to_loaded.clone(),
            path_to_ranges,
            context_lines,
            group.findings.clone(),
        )
    };

    let rhs_multibuffer = cx.new(|cx| {
        let mut mb = MultiBuffer::new(Capability::ReadOnly);
        mb.set_all_diff_hunks_expanded(cx);
        mb
    });

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
            &findings_for_tab,
            language_registry,
            weak_review,
            cx,
        );
        None
    } else {
        let language_registry = language_registry.clone();
        let inserted = Rc::new(Cell::new(false));
        Some(cx.observe(&splittable, move |_this, splittable, cx| {
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
                &findings_for_tab,
                &language_registry,
                weak_review.clone(),
                cx,
            );
        }))
    };

    (splittable, split_subscription)
}

fn insert_review_blocks(
    splittable: &Entity<SplittableEditor>,
    rhs_multibuffer: &Entity<MultiBuffer>,
    path_to_loaded: &HashMap<String, (Entity<Buffer>, Entity<BufferDiff>)>,
    findings: &[Finding],
    language_registry: &Arc<LanguageRegistry>,
    weak_review: WeakEntity<ReviewView>,
    cx: &mut Context<ReviewView>,
) {
    let mb_snapshot = rhs_multibuffer.read(cx).snapshot(cx);
    let mut blocks: Vec<BlockProperties<multi_buffer::Anchor>> = Vec::new();

    let mut path_to_snapshot: HashMap<&str, BufferSnapshot> = HashMap::new();
    for finding in findings {
        let Some((buffer, _)) = path_to_loaded.get(finding.file.as_str()) else {
            log::warn!(
                "review: skipping finding {}:{}: path not loaded",
                finding.file,
                finding.line,
            );
            continue;
        };
        let Some(line_index) = finding.line.checked_sub(1) else {
            log::warn!(
                "review: skipping finding {}:0: line must be 1-based",
                finding.file,
            );
            continue;
        };
        let buffer_snapshot = path_to_snapshot
            .entry(finding.file.as_str())
            .or_insert_with(|| buffer.read(cx).snapshot());
        let max_row = buffer_snapshot.max_point().row;
        if line_index > max_row {
            log::warn!(
                "review: skipping finding {}:{}: past EOF (max row {})",
                finding.file,
                finding.line,
                max_row,
            );
            continue;
        }
        let text_anchor = buffer_snapshot.anchor_after(Point::new(line_index, 0));
        let Some(mb_anchor) = mb_snapshot.anchor_in_excerpt(text_anchor) else {
            log::warn!(
                "review: skipping finding {}:{}: line not in any excerpt",
                finding.file,
                finding.line,
            );
            continue;
        };

        let body = finding_body(finding);
        let markdown = cx.new(|cx| {
            Markdown::new(
                body.clone().into(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });
        let verdict = parse_verdict(finding.verdict.as_deref());
        let category = finding.category.clone().map(SharedString::from);
        let height = estimate_block_height(&body);

        let markdown_for_render = markdown.clone();
        let target = CommentTarget {
            path: finding.file.clone(),
            line: finding.line,
            body,
        };
        let weak_review = weak_review.clone();
        blocks.push(BlockProperties {
            placement: BlockPlacement::Below(mb_anchor),
            height: Some(height),
            style: BlockStyle::Sticky,
            priority: 1,
            render: Arc::new(move |bcx: &mut BlockContext| {
                render_review_comment(
                    verdict,
                    category.clone(),
                    markdown_for_render.clone(),
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

fn finding_body(finding: &Finding) -> String {
    match finding
        .failure_scenario
        .as_deref()
        .filter(|scenario| !scenario.trim().is_empty())
    {
        Some(scenario) => format!("{}\n\n**Failure scenario:** {}", finding.summary, scenario),
        None => finding.summary.clone(),
    }
}

// Findings are long wrapped paragraphs, so count wrapped rows, not just newlines.
fn estimate_block_height(body: &str) -> u32 {
    const CHARS_PER_ROW: usize = 110;
    let rows: usize = body
        .lines()
        .map(|line| line.chars().count() / CHARS_PER_ROW + 1)
        .sum();
    (rows as u32).saturating_add(2).clamp(3, 24)
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Confirmed => "CONFIRMED",
        Verdict::Plausible => "PLAUSIBLE",
        Verdict::Unlabeled => "FINDING",
    }
}

fn render_review_comment(
    verdict: Verdict,
    category: Option<SharedString>,
    markdown: Entity<Markdown>,
    weak_review: WeakEntity<ReviewView>,
    target: CommentTarget,
    bcx: &mut BlockContext,
) -> AnyElement {
    let cx = &*bcx.app;
    let status = cx.theme().status();
    let colors = cx.theme().colors();
    let (verdict_color, bg) = match verdict {
        Verdict::Confirmed => (status.error, status.error_background),
        Verdict::Plausible => (status.warning, status.warning_background),
        Verdict::Unlabeled => (status.info, status.info_background),
    };
    let style = diagnostics_markdown_style(bcx.window, cx);

    let mut header_row = h_flex().gap_2().items_center().child(
        div()
            .px_1p5()
            .rounded_sm()
            .bg(verdict_color)
            .text_color(colors.background)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(verdict_label(verdict)),
    );
    if let Some(category) = category {
        header_row = header_row.child(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(category),
        );
    }

    div()
        .pl_2()
        .pr_2()
        .py_1()
        .border_l_2()
        .bg(bg)
        .border_color(verdict_color)
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
        .child(MarkdownElement::new(markdown, style).code_block_renderer(
            CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::Hidden,
                wrap_button_visibility: WrapButtonVisibility::Hidden,
                border: false,
            },
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // No `use super::*`: it would pull in `gpui::test` and shadow `#[test]`.
    use super::{
        Finding, OverrideHunk, PathRanges, ReviewDocument, ReviewHunk, ShowBlock, finding_body,
        raw_ranges, reading_ranges,
    };
    use anyhow::Context as _;

    fn hunk(path: &str, new_start: u32, new_lines: u32) -> ReviewHunk {
        ReviewHunk {
            path: path.to_string(),
            new_start,
            new_lines,
        }
    }

    fn rows(ranges: &PathRanges, path: &str) -> Vec<(u32, u32)> {
        ranges
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, ranges)| {
                ranges
                    .iter()
                    .map(|range| (range.start.row, range.end.row))
                    .collect()
            })
            .unwrap_or_default()
    }

    const ENVELOPE: &str = r#"{
      "base": "8360ba1f", "target": "e083707f", "lang": "ja",
      "groups": [
        {
          "phase": "foundation", "title": "Store", "summary": "S",
          "review_focus": ["a", "b"], "review_priority": 2,
          "hunks": [{"path": "a.ts", "new_start": 1, "new_lines": 211}],
          "findings": [
            {"file": "a.ts", "line": 41, "summary": "dup", "failure_scenario": "lost"},
            {"file": "a.ts", "line": 118, "summary": "race", "verdict": "CONFIRMED", "category": "correctness"}
          ],
          "reading": {
            "summary": "R",
            "override_hunks": [
              {"path": "a.ts", "new_start": 1, "new_lines": 211,
               "show": [{"lines": [3, 39], "omit": [[31, 38]]}, {"lines": [41, 87]}]}
            ]
          }
        },
        {
          "phase": "integration", "title": "Flag", "review_priority": 1,
          "hunks": [{"path": "b.ts", "new_start": 59, "new_lines": 1}]
        }
      ],
      "code_review": {
        "level": "high",
        "unassigned": [{"file": "z.ts", "line": 5, "summary": "orphan"}],
        "tally": {"total": 3, "by_verdict": {"CONFIRMED": 1, "PLAUSIBLE": 0, "UNLABELED": 2}, "by_category": {}, "unassigned": 1}
      }
    }"#;

    #[test]
    fn parses_current_envelope() -> anyhow::Result<()> {
        let document: ReviewDocument = serde_json::from_str(ENVELOPE)?;
        assert_eq!(document.groups.len(), 2);
        let store = &document.groups[0];
        assert_eq!(store.findings.len(), 2);
        assert_eq!(store.findings[0].verdict, None);
        assert_eq!(store.findings[1].category.as_deref(), Some("correctness"));
        let reading = store.reading.as_ref().context("reading missing")?;
        assert_eq!(reading.summary.as_deref(), Some("R"));
        assert_eq!(reading.override_hunks[0].show.len(), 2);
        assert!(!reading.override_hunks[0].hide);
        let flag = &document.groups[1];
        assert!(flag.findings.is_empty());
        assert!(flag.reading.is_none());
        let review = document.code_review.context("code_review missing")?;
        assert_eq!(review.level.as_deref(), Some("high"));
        assert_eq!(review.unassigned.len(), 1);
        Ok(())
    }

    #[test]
    fn finding_body_appends_scenario() {
        let finding = Finding {
            file: "a.ts".into(),
            line: 1,
            summary: "s".into(),
            failure_scenario: Some("f".into()),
            verdict: None,
            category: None,
        };
        assert_eq!(finding_body(&finding), "s\n\n**Failure scenario:** f");
        let bare = Finding {
            failure_scenario: Some("  ".into()),
            ..finding
        };
        assert_eq!(finding_body(&bare), "s");
    }

    #[test]
    fn reading_ranges_split_on_omit() -> anyhow::Result<()> {
        let document: ReviewDocument = serde_json::from_str(ENVELOPE)?;
        let group = &document.groups[0];
        let overrides = &group.reading.as_ref().context("reading")?.override_hunks;
        let ranges = reading_ranges(&group.hunks, overrides);
        assert_eq!(rows(&ranges, "a.ts"), vec![(2, 29), (38, 38), (40, 86)]);
        Ok(())
    }

    #[test]
    fn reading_ranges_hide_and_default_window() {
        let hunks = vec![
            hunk("b.ts", 59, 1),
            hunk("c.ts", 119, 12),
            hunk("d.ts", 10, 0),
        ];
        let overrides = vec![OverrideHunk {
            path: "b.ts".into(),
            new_start: 59,
            new_lines: 1,
            hide: true,
            show: Vec::new(),
        }];
        let ranges = reading_ranges(&hunks, &overrides);
        assert!(rows(&ranges, "b.ts").is_empty());
        assert_eq!(rows(&ranges, "c.ts"), vec![(115, 133)]);
        assert_eq!(rows(&ranges, "d.ts"), vec![(6, 12)]);
    }

    #[test]
    fn reading_ranges_multiple_omits() {
        let hunks = vec![hunk("a.ts", 1, 211)];
        let overrides = vec![OverrideHunk {
            path: "a.ts".into(),
            new_start: 1,
            new_lines: 211,
            hide: false,
            show: vec![ShowBlock {
                lines: [41, 87],
                omit: vec![[70, 75], [52, 56]],
            }],
        }];
        let ranges = reading_ranges(&hunks, &overrides);
        assert_eq!(rows(&ranges, "a.ts"), vec![(40, 50), (56, 68), (75, 86)]);
    }

    #[test]
    fn raw_ranges_keep_hunk_rows() {
        let ranges = raw_ranges(&[hunk("a.ts", 119, 12), hunk("a.ts", 10, 0)]);
        assert_eq!(rows(&ranges, "a.ts"), vec![(118, 130), (9, 9)]);
    }
}
