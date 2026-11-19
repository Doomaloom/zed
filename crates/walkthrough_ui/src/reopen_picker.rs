use std::sync::Arc;

use chrono::{DateTime, Local};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{
    ListItem, ListItemSpacing,
    prelude::*,
    utils::{DateTimeType, format_distance_from_now},
};
use util::ResultExt as _;
use walkthrough::HistoryEntry;
use workspace::{ModalView, Workspace};

use crate::navigation::show_from_history;

pub(crate) struct ReopenPicker {
    picker: Entity<Picker<ReopenPickerDelegate>>,
}

impl ReopenPicker {
    pub(crate) fn new(
        entries: Vec<HistoryEntry>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ReopenPickerDelegate {
            reopen_picker: cx.entity().downgrade(),
            workspace,
            entries,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::nonsearchable_uniform_list(delegate, window, cx));
        Self { picker }
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<ReopenPickerDelegate>> {
        &self.picker
    }
}

impl Render for ReopenPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for ReopenPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ReopenPicker {}
impl ModalView for ReopenPicker {}

pub(crate) struct ReopenPickerDelegate {
    reopen_picker: WeakEntity<ReopenPicker>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<HistoryEntry>,
    selected_index: usize,
}

impl ReopenPickerDelegate {
    #[cfg(test)]
    pub(crate) fn titles(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.walkthrough.title.as_str())
            .collect()
    }
}

fn detail(entry: &HistoryEntry) -> SharedString {
    let step_count = entry.walkthrough.steps.len();
    let steps = if step_count == 1 {
        "1 step".to_string()
    } else {
        format!("{step_count} steps")
    };
    let shown_at = i64::try_from(entry.shown_at)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map(|time| {
            format_distance_from_now(
                DateTimeType::Local(time.with_timezone(&Local)),
                false,
                true,
                false,
            )
        });
    match shown_at {
        Some(shown_at) => format!("{steps} \u{b7} {shown_at}").into(),
        None => steps.into(),
    }
}

impl PickerDelegate for ReopenPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "walkthrough reopen"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Reopen a walkthrough".into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(entry) = self.entries.get(self.selected_index) {
            let json = entry.walkthrough.clone();
            self.workspace
                .update(cx, |workspace, cx| {
                    show_from_history(workspace, json, window, cx);
                })
                .log_err();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.reopen_picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(index)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(entry.walkthrough.title.clone()))
                        .child(
                            Label::new(detail(entry))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
        )
    }
}
