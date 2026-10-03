use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use file_icons::FileIcons;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, RenderImage, SharedString, Styled,
    Subscription, Task, Window, div, img,
};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectEntryId, ProjectPath};
use ui::{
    ActiveTheme, Button, Clickable, Color, Icon, IconName, Label, LabelCommon, LabelSize, h_flex,
    v_flex,
};
use util::ResultExt as _;
use workspace::item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions};

use crate::questionnaire_view::OpenAsText;

pub struct SvgItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    buffer: Entity<Buffer>,
    dirty: bool,
    _subscription: Subscription,
}

impl SvgItem {
    fn is_svg_path(path: &ProjectPath) -> bool {
        path.path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    }
}

impl project::ProjectItem for SvgItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !Self::is_svg_path(path) {
            return None;
        }
        let path = path.clone();
        let project = project.clone();
        Some(cx.spawn(async move |cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                .await?;
            let entry_id = project.read_with(cx, |project, cx| {
                project.entry_for_path(&path, cx).map(|entry| entry.id)
            });
            Ok(cx.new(|cx| {
                // `is_dirty` has no `cx`, so mirror the buffer's flag here.
                let subscription =
                    cx.subscribe(&buffer, |this: &mut Self, buffer, _: &BufferEvent, cx| {
                        this.dirty = buffer.read(cx).is_dirty();
                    });
                Self {
                    project_path: path,
                    entry_id,
                    dirty: buffer.read(cx).is_dirty(),
                    buffer,
                    _subscription: subscription,
                }
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
        self.dirty
    }
}

pub enum SvgViewEvent {
    Changed,
}

pub struct SvgView {
    item: Entity<SvgItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    current_image: Option<Result<Arc<RenderImage>, SharedString>>,
    _refresh: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SvgView {
    pub fn buffer(&self, cx: &App) -> Entity<Buffer> {
        self.item.read(cx).buffer.clone()
    }

    fn file_name(&self, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .project_path
            .path
            .file_name()
            .unwrap_or("image.svg")
            .to_string()
            .into()
    }

    fn render_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.buffer(cx).read(cx).text();
        // Relative `<image href>` paths are resolved against the SVG's own directory.
        let resources_dir = self
            .project
            .read(cx)
            .absolute_path(&self.item.read(cx).project_path, cx)
            .and_then(|path| path.parent().map(|parent| parent.to_path_buf()));
        let renderer = cx.svg_renderer();
        let render_task = cx.background_spawn(async move {
            let svg =
                renderer.parse_svg_with_resources_dir(text.as_bytes(), resources_dir.as_deref())?;
            renderer.render_parsed(&svg, 1.0)
        });
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = render_task.await;
            this.update_in(cx, |this, window, cx| {
                let image = result.map_err(|error| SharedString::from(error.to_string()));
                if let Some(Ok(previous)) = this.current_image.replace(image) {
                    window.drop_image(previous).log_err();
                }
                cx.notify();
            })
            .log_err();
        });
    }
}

impl EventEmitter<SvgViewEvent> for SvgView {}

impl Focusable for SvgView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SvgView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.current_image.clone() {
            Some(Ok(image)) => div()
                .size_full()
                .flex()
                .justify_center()
                .items_center()
                .p_4()
                .child(img(image).max_w_full().max_h_full())
                .into_any_element(),
            Some(Err(error)) => div()
                .p_4()
                .child(Label::new(error).color(Color::Error))
                .into_any_element(),
            None => div().into_any_element(),
        };

        v_flex()
            .id("agentium-svg-view")
            .key_context("SvgView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(self.file_name(cx))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Button::new("open-as-text", "Open as text").on_click(
                        |_, window, cx| {
                            window.dispatch_action(Box::new(OpenAsText), cx);
                        },
                    )),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().child(body))
    }
}

impl Item for SvgView {
    type Event = SvgViewEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.file_name(cx)
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        FileIcons::get_icon(self.item.read(cx).project_path.path.as_std_path(), cx)
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Image)))
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
        Some("agentium svg view")
    }

    fn to_item_events(event: &SvgViewEvent, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            SvgViewEvent::Changed => f(ItemEvent::UpdateTab),
        }
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

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).has_conflict()
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer(cx);
        project.update(cx, |project, cx| project.save_buffer(buffer, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer(cx);
        let reload = project.update(cx, |project, cx| {
            project.reload_buffers(HashSet::from_iter([buffer]), true, cx)
        });
        cx.spawn(async move |_this, _cx| {
            reload.await?;
            Ok(())
        })
    }
}

impl ProjectItem for SvgView {
    type Item = SvgItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = item.read(cx).buffer.clone();
        let subscription = cx.subscribe_in(
            &buffer,
            window,
            |this, _, event: &BufferEvent, window, cx| match event {
                BufferEvent::Edited { .. }
                | BufferEvent::Reloaded
                | BufferEvent::FileHandleChanged => {
                    this.render_image(window, cx);
                    cx.emit(SvgViewEvent::Changed);
                }
                BufferEvent::DirtyChanged | BufferEvent::Saved => {
                    cx.emit(SvgViewEvent::Changed);
                }
                _ => {}
            },
        );
        let mut view = Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            current_image: None,
            _refresh: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        view.render_image(window, cx);
        view
    }
}

#[cfg(test)]
mod tests {
    use project::ProjectItem as _;

    #[gpui::test]
    async fn claims_only_svg_files(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                "a.svg": "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
                "B.SVG": "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
                "c.svgz": "",
                "d.svg.md": "",
                "svg": "",
            }),
        )
        .await;
        let project = project::Project::test(fs.clone(), [std::path::Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        let project_path = |name: &str| project::ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path(name).into(),
        };

        for name in ["c.svgz", "d.svg.md", "svg"] {
            let opened =
                cx.update(|cx| super::SvgItem::try_open(&project, &project_path(name), cx));
            assert!(opened.is_none(), "{name} must fall through to the editor");
        }
        for name in ["a.svg", "B.SVG"] {
            cx.update(|cx| super::SvgItem::try_open(&project, &project_path(name), cx))
                .unwrap_or_else(|| panic!("{name} must be claimed"))
                .await
                .unwrap_or_else(|error| panic!("{name} must open: {error:#}"));
        }
    }
}
