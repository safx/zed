use std::collections::HashMap;

use gpui::{prelude::*, *};
use ui::{Button, Clickable as _, Icon, IconSize, Tooltip, h_flex, v_flex};
use workspace::item::{Item, ItemEvent};

use crate::{AgentiumApp, inbox_todo_view::InboxTodoView};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverviewScope {
    Task(uuid::Uuid),
    Arena(EntityId),
}

pub(crate) struct TaskOverview {
    pub(crate) scope: OverviewScope,
    app: WeakEntity<AgentiumApp>,
    focus_handle: FocusHandle,
    pub(crate) todos: HashMap<u64, Entity<InboxTodoView>>,
    pub(crate) todo_owners: HashMap<u64, EntityId>,
    pub(crate) expanded: std::collections::HashSet<u64>,
    expanded_messages: HashMap<(EntityId, &'static str), String>,
    _subscription: Subscription,
}

impl TaskOverview {
    pub(crate) fn new(
        scope: OverviewScope,
        app: Entity<AgentiumApp>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&app, |_, _, cx| cx.notify());
        Self {
            scope,
            app: app.downgrade(),
            focus_handle: cx.focus_handle(),
            todos: HashMap::new(),
            expanded: Default::default(),
            todo_owners: HashMap::new(),
            expanded_messages: HashMap::new(),
            _subscription: subscription,
        }
    }

    pub(crate) fn render_message(
        &self,
        item_id: EntityId,
        label: &'static str,
        text: String,
        view: Entity<Self>,
    ) -> AnyElement {
        let text = text.trim().to_owned();
        let first_sentence_end = text
            .char_indices()
            .find(|(_, character)| matches!(character, '.' | '。'))
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or(text.len());
        let has_more = !text[first_sentence_end..].trim().is_empty();
        let key = (item_id, label);
        let expanded = self.expanded_messages.get(&key) == Some(&text);
        let visible = if expanded {
            text.clone()
        } else {
            text[..first_sentence_end].to_owned()
        };
        let icon_path = if label == "Request" {
            "icons/user-round.svg"
        } else {
            "icons/bot.svg"
        };
        let message = v_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .child(div().text_sm().child(visible))
            .when(has_more, |column| {
                column.child(
                    Button::new(
                        SharedString::from(format!("overview-message-{item_id:?}-{label}")),
                        if expanded { "Show less" } else { "Show more" },
                    )
                    .on_click(move |_, _, cx| {
                        view.update(cx, |overview, cx| {
                            if expanded {
                                overview.expanded_messages.remove(&key);
                            } else {
                                overview.expanded_messages.insert(key, text.clone());
                            }
                            cx.notify();
                        })
                    }),
                )
            });
        h_flex()
            .min_w_0()
            .flex_shrink_0()
            .items_start()
            .gap_3()
            .child(
                div()
                    .id(SharedString::from(format!("overview-icon-{item_id:?}-{label}")))
                    .flex_shrink_0()
                    .child(Icon::from_path(icon_path).size(IconSize::Custom(rems(1.5))))
                    .tooltip(Tooltip::text(label)),
            )
            .child(message)
            .into_any_element()
    }
}

impl Render for TaskOverview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let app = self.app.clone();
        let content = match app.update(cx, |app, cx| {
            app.render_task_overview(self, view, window, cx)
        }) {
            Ok(element) => element,
            Err(_) => div()
                .child("Task is no longer available.")
                .into_any_element(),
        };
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(content)
    }
}

impl Focusable for TaskOverview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TaskOverview {}

impl Item for TaskOverview {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Task Overview".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }
}

pub(crate) fn todo_matches_scope(
    live_in_scope: Option<bool>,
    saved_path: Option<&std::path::Path>,
    saved_name: &str,
    paths: &[std::path::PathBuf],
    names: &[String],
) -> bool {
    if let Some(in_scope) = live_in_scope {
        return in_scope;
    }
    match saved_path {
        Some(path) => paths.iter().any(|candidate| candidate == path),
        None => names.iter().any(|name| name == saved_name),
    }
}

#[cfg(test)]
mod tests {
    use super::todo_matches_scope;
    use std::path::{Path, PathBuf};

    #[test]
    fn overview_follows_live_owner_after_move() {
        let paths = vec![PathBuf::from("/task/a")];
        assert!(!todo_matches_scope(
            Some(false),
            Some(Path::new("/task/a")),
            "a",
            &paths,
            &[]
        ));
        assert!(todo_matches_scope(
            Some(true),
            Some(Path::new("/other")),
            "other",
            &paths,
            &[]
        ));
    }

    #[test]
    fn overview_includes_closed_owners_across_task_worktrees() {
        let paths = vec![PathBuf::from("/task/a"), PathBuf::from("/task/b")];
        assert!(todo_matches_scope(
            None,
            Some(Path::new("/task/b")),
            "b",
            &paths,
            &[]
        ));
        assert!(!todo_matches_scope(
            None,
            Some(Path::new("/other/b")),
            "b",
            &paths,
            &["b".into()]
        ));
        assert!(!todo_matches_scope(
            None,
            Some(Path::new("/task/b/nested")),
            "b",
            &paths,
            &[]
        ));
    }

    #[test]
    fn overview_uses_name_only_for_legacy_lists_without_paths() {
        assert!(todo_matches_scope(
            None,
            None,
            "legacy",
            &[],
            &["legacy".into()]
        ));
        assert!(!todo_matches_scope(
            None,
            None,
            "other",
            &[],
            &["legacy".into()]
        ));
    }
}
