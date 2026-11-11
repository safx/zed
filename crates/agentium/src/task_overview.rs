use std::collections::HashMap;

use gpui::{prelude::*, *};
use ui::{Button, Clickable as _, Icon, IconSize, Tooltip, h_flex, v_flex};

use crate::{AgentiumApp, inbox_todo_view::InboxTodoView};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum OverviewScope {
    Task(uuid::Uuid),
    Arena(EntityId),
}

/// The always-visible column beside the sidebar. It is not a pane item: it
/// shows whichever task (or task-less arena) the sidebar selection points at.
pub(crate) struct TaskOverview {
    pub(crate) scope: Option<OverviewScope>,
    app: WeakEntity<AgentiumApp>,
    focus_handle: FocusHandle,
    /// The task's To do list; rebuilt when the scope changes.
    pub(crate) todo_view: Option<Entity<InboxTodoView>>,
    pub(crate) show_context: std::collections::HashSet<EntityId>,
    /// Question sections the user folded, by their first question id.
    pub(crate) collapsed_questions: std::collections::HashSet<u64>,
    expanded_messages: HashMap<(EntityId, &'static str), String>,
    _subscription: Subscription,
}

impl TaskOverview {
    pub(crate) fn new(app: Entity<AgentiumApp>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&app, |_, _, cx| cx.notify());
        Self {
            scope: None,
            app: app.downgrade(),
            focus_handle: cx.focus_handle(),
            todo_view: None,
            collapsed_questions: Default::default(),
            show_context: Default::default(),
            expanded_messages: HashMap::new(),
            _subscription: subscription,
        }
    }

    pub(crate) fn render_message(
        &self,
        item_id: EntityId,
        label: &'static str,
        text: String,
        summary: Option<String>,
        view: Entity<Self>,
    ) -> AnyElement {
        let text = text.trim().to_owned();
        let first_sentence_end = text
            .char_indices()
            .find(|(_, character)| matches!(character, '.' | '。'))
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or(text.len());
        let has_summary = summary.is_some();
        let has_more = has_summary || !text[first_sentence_end..].trim().is_empty();
        let key = (item_id, label);
        let expanded = self.expanded_messages.get(&key) == Some(&text);
        let visible = if expanded {
            text.clone()
        } else {
            summary.unwrap_or_else(|| text[..first_sentence_end].to_owned())
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
                        if expanded { "Show less" } else if has_summary { "Show original" } else { "Show more" },
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

