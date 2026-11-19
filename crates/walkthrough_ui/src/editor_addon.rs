use std::{ops::Range, sync::Arc};

use collections::{HashMap, HashSet};
use editor::{
    Addon, Anchor, Editor, EditorEvent,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, HighlightKey,
        RenderBlock,
    },
};
use gpui::{
    Action as _, AnyElement, App, Context, Entity, Hsla, Subscription, WeakEntity, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use ui::{IconButton, IconButtonShape, Tooltip, prelude::*};
use walkthrough::{
    DataChange, GoToStep, HighlightColor, StepData, StepId, WalkthroughEvent, WalkthroughStep,
    WalkthroughStore,
};

const MAX_COMMENT_ROWS: f32 = 14.;
const MAX_DATA_ROWS: f32 = 10.;
const MAX_DATA_VALUE_LINES: usize = 3;
const MAX_DATA_NAME_WIDTH_IN_EMS: f32 = 24.;
const MAX_BLOCK_WIDTH_IN_EMS: f32 = 110.;
const STALE_MESSAGE: &str = "This code has changed since the walkthrough was written";

pub(crate) fn highlight_hsla(color: HighlightColor, theme: &theme::Theme) -> Hsla {
    // Taken from the built-in accent scales rather than the theme's own
    // accents, which a theme may reorder or replace, so that a color named in
    // the legend is always that hue.
    let accents = match theme.appearance() {
        theme::Appearance::Light => theme::AccentColors::light(),
        theme::Appearance::Dark => theme::AccentColors::dark(),
    };
    let index = match color {
        HighlightColor::Blue => 0,
        HighlightColor::Orange => 1,
        HighlightColor::Purple => 4,
        HighlightColor::Yellow => 5,
        HighlightColor::Green => 6,
        HighlightColor::Red => 7,
    };
    accents.color_for_index(index)
}

pub(crate) fn legend_chip(
    color: HighlightColor,
    label: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .child(
            div()
                .size_2()
                .rounded_full()
                .bg(highlight_hsla(color, cx.theme())),
        )
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

#[derive(Clone, Copy)]
enum RangeStyle {
    Step {
        color: Option<HighlightColor>,
        is_active: bool,
    },
    Highlight {
        color: HighlightColor,
        in_active_step: bool,
    },
}

impl RangeStyle {
    fn background(self, theme: &theme::Theme) -> Hsla {
        match self {
            Self::Step {
                color: None,
                is_active: false,
            } => theme.colors().editor_document_highlight_read_background,
            Self::Step {
                color: None,
                is_active: true,
            } => theme.colors().text_accent.opacity(0.18),
            Self::Step {
                color: Some(color),
                is_active,
            } => highlight_hsla(color, theme).opacity(if is_active { 0.18 } else { 0.08 }),
            Self::Highlight {
                color,
                in_active_step,
            } => highlight_hsla(color, theme).opacity(if in_active_step { 0.35 } else { 0.15 }),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window, cx| register_editor(editor, cx))
        .detach();
}

struct WalkthroughAddon {
    store: Entity<WalkthroughStore>,
    blocks: Vec<StepBlock>,
    _subscriptions: Vec<Subscription>,
}

impl Addon for WalkthroughAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[derive(Clone)]
struct StepBlock {
    block_id: CustomBlockId,
    range: Range<Anchor>,
    renderer: StepBlockRenderer,
}

#[derive(Clone)]
struct StepBlockRenderer {
    store: WeakEntity<WalkthroughStore>,
    editor: WeakEntity<Editor>,
    step: WalkthroughStep,
    index: usize,
    step_count: usize,
    previous_step: Option<StepId>,
    next_step: Option<StepId>,
    markdown: Entity<Markdown>,
    collapsed: bool,
}

fn register_editor(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
        return;
    }
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let store = WalkthroughStore::for_project(&project, cx);
    let subscriptions = vec![
        cx.subscribe(&store, |editor, _, event, cx| match event {
            WalkthroughEvent::Created(_) | WalkthroughEvent::Dismissed => {
                refresh_annotations(editor, false, cx)
            }
            WalkthroughEvent::Updated { .. } => refresh_annotations(editor, true, cx),
            WalkthroughEvent::ActiveStepChanged { .. } => refresh_highlights(editor, cx),
        }),
        cx.subscribe_self(|editor, event: &EditorEvent, cx| {
            // A step whose code was deleted loses its highlight.
            if matches!(event, EditorEvent::BufferEdited) {
                refresh_highlights(editor, cx);
            }
        }),
    ];
    editor.register_addon(WalkthroughAddon {
        store,
        blocks: Vec::new(),
        _subscriptions: subscriptions,
    });
    refresh_annotations(editor, false, cx);
}

/// Rebuilds the comment blocks. `keep_collapsed` carries over which blocks
/// the user collapsed, for a walkthrough that was edited in place.
fn refresh_annotations(editor: &mut Editor, keep_collapsed: bool, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon_mut::<WalkthroughAddon>() else {
        return;
    };
    let store = addon.store.clone();
    let old_blocks = std::mem::take(&mut addon.blocks);
    let collapsed_steps = old_blocks
        .iter()
        .filter(|block| keep_collapsed && block.renderer.collapsed)
        .map(|block| block.renderer.step.id.clone())
        .collect::<HashSet<_>>();
    let old_block_ids = old_blocks
        .into_iter()
        .map(|block| block.block_id)
        .collect::<HashSet<_>>();
    if !old_block_ids.is_empty() {
        editor.remove_blocks(old_block_ids, None, cx);
    }

    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let buffer_id = buffer.read(cx).remote_id();
    let language_registry = editor
        .project()
        .map(|project| project.read(cx).languages().clone());
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let weak_editor = cx.entity().downgrade();

    let mut renderers = Vec::new();
    if let Some(walkthrough) = store.read(cx).active() {
        let step_count = walkthrough.steps.len();
        for (index, step) in walkthrough.steps.iter().enumerate() {
            if step.range.start.buffer_id != buffer_id {
                continue;
            }
            let Some(range) = snapshot.anchor_range_in_buffer(step.range.clone()) else {
                continue;
            };
            let neighbor = |index: Option<usize>| {
                index
                    .and_then(|index| walkthrough.steps.get(index))
                    .map(|step| step.id.clone())
            };
            renderers.push((
                range,
                step.clone(),
                index,
                step_count,
                neighbor(index.checked_sub(1)),
                neighbor(index.checked_add(1)),
            ));
        }
    }

    let mut blocks = Vec::new();
    let mut properties = Vec::new();
    for (range, step, index, step_count, previous_step, next_step) in renderers {
        let markdown =
            cx.new(|cx| Markdown::new(step.comment.clone(), language_registry.clone(), None, cx));
        let collapsed = collapsed_steps.contains(&step.id);
        let renderer = StepBlockRenderer {
            store: store.downgrade(),
            editor: weak_editor.clone(),
            step,
            index,
            step_count,
            previous_step,
            next_step,
            markdown,
            collapsed,
        };
        properties.push(BlockProperties {
            placement: BlockPlacement::Below(range.end),
            // The editor measures the rendered block and corrects this.
            height: Some(2),
            style: BlockStyle::Sticky,
            render: renderer.clone().into_render_block(),
            priority: 0,
        });
        blocks.push((range, renderer));
    }

    let block_ids = editor.insert_blocks(properties, None, cx);
    let blocks = blocks
        .into_iter()
        .zip(block_ids)
        .map(|((range, renderer), block_id)| StepBlock {
            block_id,
            range,
            renderer,
        })
        .collect();
    if let Some(addon) = editor.addon_mut::<WalkthroughAddon>() {
        addon.blocks = blocks;
    }
    refresh_highlights(editor, cx);
}

fn refresh_highlights(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon::<WalkthroughAddon>() else {
        return;
    };
    let active_step = addon
        .store
        .read(cx)
        .active_step()
        .map(|step| step.id.clone());

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut active_ranges = Vec::new();
    let mut inactive_ranges = Vec::new();
    let mut inner_ranges = Vec::new();
    for block in &addon.blocks {
        let step = &block.renderer.step;
        if step.is_stale(cx) {
            continue;
        }
        let is_active = active_step.as_ref() == Some(&step.id);
        let style = RangeStyle::Step {
            color: step.color,
            is_active,
        };
        if is_active {
            active_ranges.push((block.range.clone(), style));
        } else {
            inactive_ranges.push((block.range.clone(), style));
        }
        for highlight in &step.highlights {
            let Some(range) = snapshot.anchor_range_in_buffer(highlight.range.clone()) else {
                continue;
            };
            inner_ranges.push((
                range,
                RangeStyle::Highlight {
                    color: highlight.color,
                    in_active_step: is_active,
                },
            ));
        }
    }

    set_highlights(editor, HighlightKey::WalkthroughStep, inactive_ranges, cx);
    set_highlights(
        editor,
        HighlightKey::WalkthroughActiveStep,
        active_ranges,
        cx,
    );
    set_highlights(editor, HighlightKey::WalkthroughHighlight, inner_ranges, cx);
    // Blocks restyle themselves from the store when they are next drawn.
    cx.notify();
}

fn set_highlights(
    editor: &mut Editor,
    key: HighlightKey,
    mut ranges: Vec<(Range<Anchor>, RangeStyle)>,
    cx: &mut Context<Editor>,
) {
    if ranges.is_empty() {
        editor.clear_background_highlights(key, cx);
        return;
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    ranges.sort_by(|(a, _), (b, _)| a.start.cmp(&b.start, &snapshot));
    let (ranges, styles): (Vec<_>, Vec<_>) = ranges.into_iter().unzip();
    // The editor passes each range's index in `ranges`, which is how several
    // colors share one highlight key.
    editor.highlight_background(
        key,
        &ranges,
        move |index, theme| {
            styles
                .get(*index)
                .map(|style| style.background(theme))
                .unwrap_or_else(gpui::transparent_black)
        },
        cx,
    );
}

#[cfg(test)]
pub(crate) fn step_block_count(editor: &Editor) -> usize {
    editor
        .addon::<WalkthroughAddon>()
        .map(|addon| addon.blocks.len())
        .unwrap_or_default()
}

fn toggle_collapsed(editor: &mut Editor, step: &StepId, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon_mut::<WalkthroughAddon>() else {
        return;
    };
    let Some(block) = addon
        .blocks
        .iter_mut()
        .find(|block| &block.renderer.step.id == step)
    else {
        return;
    };
    block.renderer.collapsed = !block.renderer.collapsed;
    let renderers =
        HashMap::from_iter([(block.block_id, block.renderer.clone().into_render_block())]);
    editor.replace_blocks(renderers, None, cx);
}

impl StepBlockRenderer {
    fn into_render_block(self) -> RenderBlock {
        Arc::new(move |block_context| self.render(block_context))
    }

    fn render(&self, block_context: &mut BlockContext) -> AnyElement {
        let cx = &*block_context.app;
        let colors = cx.theme().colors();
        let is_active = self.store.upgrade().is_some_and(|store| {
            store
                .read(cx)
                .active_step()
                .is_some_and(|step| step.id == self.step.id)
        });
        let is_stale = self.step.is_stale(cx);
        let index = self.index;
        let step_color = self
            .step
            .color
            .map(|color| highlight_hsla(color, cx.theme()));
        let legend = self
            .store
            .upgrade()
            .and_then(|store| {
                let walkthrough = store.read(cx).active()?;
                Some(
                    self.step
                        .colors()
                        .into_iter()
                        .filter_map(|color| Some((color, walkthrough.legend_label(color)?.clone())))
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        let go_to_step = |step: StepId| {
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                window.dispatch_action(
                    GoToStep {
                        id: step.to_string(),
                    }
                    .boxed_clone(),
                    cx,
                );
            }
        };

        let navigation_button =
            |id: &'static str, icon: IconName, tooltip: &'static str, target: &Option<StepId>| {
                IconButton::new((id, index), icon)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .disabled(target.is_none())
                    .tooltip(Tooltip::text(tooltip))
                    .when_some(target.clone(), |button, target| {
                        button.on_click(go_to_step(target))
                    })
            };

        let header = h_flex()
            .gap_2()
            .child(
                Label::new(format!("{} / {}", index + 1, self.step_count))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .id(("walkthrough-step-title", index))
                    .flex_1()
                    .min_w_0()
                    .cursor_pointer()
                    .on_click(go_to_step(self.step.id.clone()))
                    .child(
                        Label::new(self.step.title.clone())
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .truncate(),
                    ),
            )
            .child(navigation_button(
                "walkthrough-previous-step",
                IconName::ChevronLeft,
                "Previous Step",
                &self.previous_step,
            ))
            .child(navigation_button(
                "walkthrough-next-step",
                IconName::ChevronRight,
                "Next Step",
                &self.next_step,
            ))
            .child(
                IconButton::new(
                    ("walkthrough-collapse-step", index),
                    if self.collapsed {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronUp
                    },
                )
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(if self.collapsed {
                    "Expand Comment"
                } else {
                    "Collapse Comment"
                }))
                .on_click({
                    let editor = self.editor.clone();
                    let step = self.step.id.clone();
                    move |_, _, cx| {
                        editor
                            .update(cx, |editor, cx| toggle_collapsed(editor, &step, cx))
                            .ok();
                    }
                }),
            );

        let body = (!self.collapsed).then(|| {
            v_flex()
                .when(is_stale, |this| {
                    this.child(
                        Label::new(STALE_MESSAGE)
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                })
                .child(
                    div()
                        .id(("walkthrough-step-comment", index))
                        .max_h(block_context.line_height * MAX_COMMENT_ROWS)
                        .overflow_y_scroll()
                        .child(MarkdownElement::new(
                            self.markdown.clone(),
                            MarkdownStyle::themed(MarkdownFont::Editor, block_context.window, cx),
                        )),
                )
                .when(!legend.is_empty(), |this| {
                    this.child(
                        h_flex().pt_1().gap_3().flex_wrap().children(
                            legend
                                .iter()
                                .map(|(color, label)| legend_chip(*color, label.clone(), cx)),
                        ),
                    )
                })
                .when(!self.step.data.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .id(("walkthrough-step-data", index))
                            .pt_1()
                            .gap_0p5()
                            .max_h(block_context.line_height * MAX_DATA_ROWS)
                            .overflow_y_scroll()
                            .children(self.step.data.iter().enumerate().map(|(row, entry)| {
                                render_data_row(
                                    entry,
                                    (index, row),
                                    block_context.em_width * MAX_DATA_NAME_WIDTH_IN_EMS,
                                    cx,
                                )
                            })),
                    )
                })
        });

        div()
            .pl(block_context.margins.gutter.full_width())
            .pr(block_context.margins.right)
            .w_full()
            .child(
                v_flex()
                    .id(("walkthrough-step", index))
                    .block_mouse_except_scroll()
                    .cursor_default()
                    .max_w(block_context.em_width * MAX_BLOCK_WIDTH_IN_EMS)
                    .px_2()
                    .py_1()
                    .gap_1()
                    .rounded_sm()
                    .border_l_2()
                    .border_color(match (step_color, is_active) {
                        (Some(step_color), true) => step_color,
                        (Some(step_color), false) => step_color.opacity(0.5),
                        (None, true) => colors.text_accent,
                        (None, false) => colors.border,
                    })
                    .bg(colors.editor_subheader_background)
                    .text_ui(cx)
                    .child(header)
                    .children(body),
            )
            .into_any_element()
    }
}

fn render_data_row(
    entry: &StepData,
    (step_index, row): (usize, usize),
    max_name_width: Pixels,
    cx: &App,
) -> impl IntoElement {
    let is_marked = !matches!(entry.change, DataChange::Unchanged);
    let previous = match &entry.change {
        DataChange::Changed { previous } => Some(previous.clone()),
        DataChange::New | DataChange::Unchanged => None,
    };
    h_flex()
        .debug_selector(|| format!("walkthrough-step-data-{step_index}-{row}"))
        .gap_2()
        .items_start()
        .child(
            h_flex()
                .flex_none()
                .max_w(max_name_width)
                .gap_1()
                .child(
                    div()
                        .flex_none()
                        .size_1p5()
                        .rounded_full()
                        .when(is_marked, |marker| {
                            marker.bg(cx.theme().colors().text_accent)
                        }),
                )
                .child(
                    Label::new(entry.name.clone())
                        .size(LabelSize::Small)
                        .color(if is_marked {
                            Color::Accent
                        } else {
                            Color::Muted
                        })
                        .truncate(),
                ),
        )
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .line_clamp(MAX_DATA_VALUE_LINES)
                        .child(Label::new(entry.value.clone()).buffer_font(cx)),
                )
                .children(previous.map(|previous| {
                    div().line_clamp(1).child(
                        Label::new(format!("was {previous}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use editor::display_map::{BlockId, ToDisplayPoint as _};
    use gpui::Focusable as _;
    use gpui::{TestAppContext, VisualTestContext};
    use language::Point;
    use walkthrough::{Dismiss, NextStep, WalkthroughEditJson};
    use workspace::Item as _;

    #[derive(Debug, PartialEq)]
    struct Annotations {
        blocks: usize,
        active_rows: Vec<Range<u32>>,
        inactive_rows: Vec<Range<u32>>,
    }

    fn annotations(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Annotations {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let blocks = editor
                .addon::<WalkthroughAddon>()
                .map(|addon| {
                    addon
                        .blocks
                        .iter()
                        .filter(|block| {
                            snapshot
                                .block_for_id(BlockId::Custom(block.block_id))
                                .is_some()
                        })
                        .count()
                })
                .unwrap_or_default();

            let theme = cx.theme().clone();
            let active_color = theme.colors().text_accent.opacity(0.18);
            let inactive_color = theme.colors().editor_document_highlight_read_background;
            let highlights = editor.sorted_background_highlights_in_range(
                Anchor::Min..Anchor::Max,
                &snapshot,
                &theme,
            );
            let rows_with_color = |color: Hsla| {
                highlights
                    .iter()
                    .filter(|(_, highlight_color)| *highlight_color == color)
                    .map(|(range, _)| {
                        range.start.to_point(&snapshot).row..range.end.to_point(&snapshot).row
                    })
                    .collect::<Vec<_>>()
            };
            Annotations {
                blocks,
                active_rows: rows_with_color(active_color),
                inactive_rows: rows_with_color(inactive_color),
            }
        })
    }

    #[gpui::test]
    async fn test_editor_annotations(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        let one = open_editor("one.rs", &test, cx).await;
        let unannotated = Annotations {
            blocks: 0,
            active_rows: vec![],
            inactive_rows: vec![],
        };
        assert_eq!(annotations(&one, cx), unannotated);

        load_walkthrough(sample_walkthrough(true), &test, cx).await;
        assert_eq!(
            annotations(&one, cx),
            Annotations {
                blocks: 2,
                active_rows: vec![1..3],
                inactive_rows: vec![9..11],
            }
        );
        one.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let addon = editor.addon::<WalkthroughAddon>().expect("addon");
            let block_rows = addon
                .blocks
                .iter()
                .map(|block| {
                    let block_row = snapshot
                        .block_for_id(BlockId::Custom(block.block_id))
                        .map(|_| block.range.end.to_display_point(&snapshot).row().0);
                    (block.renderer.step.id.to_string(), block_row)
                })
                .collect::<Vec<_>>();
            // The second block is pushed down by the height of the first.
            assert_eq!(block_rows[0], ("a".to_string(), Some(3)));
            assert_eq!(block_rows[1].0, "c");
            assert!(block_rows[1].1.is_some_and(|row| row > 11));
        });

        // An editor opened after the walkthrough was created is annotated too.
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        let two = active_editor(&test, cx);
        assert_ne!(two, one);
        assert_eq!(
            annotations(&two, cx),
            Annotations {
                blocks: 1,
                active_rows: vec![4..5],
                inactive_rows: vec![],
            }
        );
        assert_eq!(
            annotations(&one, cx),
            Annotations {
                blocks: 2,
                active_rows: vec![],
                inactive_rows: vec![1..3, 9..11],
            }
        );

        let three = open_editor("three.rs", &test, cx).await;
        assert_eq!(annotations(&three, cx), unannotated);

        // Deleting a step's code leaves its comment block without a highlight.
        one.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(Point::new(9, 0)..Point::new(11, 7), "")], None, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            annotations(&one, cx),
            Annotations {
                blocks: 2,
                active_rows: vec![],
                inactive_rows: vec![1..3],
            }
        );
        one.update(cx, |editor, cx| {
            let addon = editor.addon::<WalkthroughAddon>().expect("addon");
            let stale = addon
                .blocks
                .iter()
                .map(|block| block.renderer.step.is_stale(cx))
                .collect::<Vec<_>>();
            assert_eq!(stale, [false, true]);
        });

        // Typing above a range moves its highlight with the code.
        one.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "new\n")], None, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(annotations(&one, cx).inactive_rows, vec![2..4]);

        cx.dispatch_action(Dismiss);
        cx.run_until_parked();
        assert_eq!(annotations(&one, cx), unannotated);
        assert_eq!(annotations(&two, cx), unannotated);
        assert!(one.read_with(cx, |editor, cx| editor.is_dirty(cx)));
        assert!(!two.read_with(cx, |editor, cx| editor.is_dirty(cx)));
    }

    #[gpui::test]
    async fn test_colored_highlights(cx: &mut TestAppContext) {
        use walkthrough::{LegendEntryJson, StepHighlightJson};

        let (test, cx) = init_test(cx).await;
        let mut json = sample_walkthrough(false);
        json.steps[0].color = Some(HighlightColor::Blue);
        json.steps[0].highlights = vec![StepHighlightJson {
            start_line: 3,
            end_line: 3,
            text: Some("line".to_string()),
            color: HighlightColor::Red,
        }];
        json.steps[2].highlights = vec![StepHighlightJson {
            start_line: 11,
            end_line: 12,
            text: None,
            color: HighlightColor::Green,
        }];
        json.legend = [
            (HighlightColor::Blue, "Input"),
            (HighlightColor::Red, "Rejected"),
            (HighlightColor::Green, "Accepted"),
        ]
        .into_iter()
        .map(|(color, label)| LegendEntryJson {
            color,
            label: label.to_string(),
        })
        .collect();
        load_walkthrough(json, &test, cx).await;
        let one = active_editor(&test, cx);

        let highlights = |cx: &mut VisualTestContext| {
            one.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let theme = cx.theme().clone();
                editor
                    .sorted_background_highlights_in_range(
                        Anchor::Min..Anchor::Max,
                        &snapshot,
                        &theme,
                    )
                    .into_iter()
                    .map(|(range, color)| {
                        let start = range.start.to_point(&snapshot);
                        let end = range.end.to_point(&snapshot);
                        ((start.row, start.column)..(end.row, end.column), color)
                    })
                    .collect::<Vec<_>>()
            })
        };
        let theme = cx.update(|_, cx| cx.theme().clone());
        let blue = highlight_hsla(HighlightColor::Blue, &theme);
        let red = highlight_hsla(HighlightColor::Red, &theme);
        let green = highlight_hsla(HighlightColor::Green, &theme);
        assert_ne!(blue, red);
        assert_ne!(red, green);

        assert_eq!(
            highlights(cx),
            [
                ((1, 0)..(3, 6), blue.opacity(0.18)),
                ((2, 0)..(2, 4), red.opacity(0.35)),
                (
                    (9, 0)..(11, 7),
                    theme.colors().editor_document_highlight_read_background
                ),
                ((10, 0)..(11, 7), green.opacity(0.15)),
            ]
        );

        one.update(cx, |editor, cx| {
            let addon = editor.addon::<WalkthroughAddon>().expect("addon");
            addon.store.update(cx, |store, cx| {
                store.set_active_step(&StepId::new("c"), cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            highlights(cx),
            [
                ((1, 0)..(3, 6), blue.opacity(0.08)),
                ((2, 0)..(2, 4), red.opacity(0.15)),
                ((9, 0)..(11, 7), theme.colors().text_accent.opacity(0.18)),
                ((10, 0)..(11, 7), green.opacity(0.35)),
            ]
        );
    }

    #[gpui::test]
    async fn test_step_data_rows(cx: &mut TestAppContext) {
        use walkthrough::{DataChange, StepDataJson};

        let (test, cx) = init_test(cx).await;
        let data = |name: &str, value: &str| StepDataJson {
            name: name.to_string(),
            value: value.to_string(),
        };
        let mut json = sample_walkthrough(false);
        json.example = Some("A user adds 2 units to an empty cart".to_string());
        json.steps[0].data = vec![data("cart", "[]"), data("quantity", "2")];
        json.steps[2].data = vec![data("cart", "[A-12]"), data("quantity", "2")];
        load_walkthrough(json, &test, cx).await;
        let one = active_editor(&test, cx);

        one.read_with(cx, |editor, _| {
            let addon = editor.addon::<WalkthroughAddon>().expect("addon");
            let changes = addon
                .blocks
                .iter()
                .map(|block| {
                    block
                        .renderer
                        .step
                        .data
                        .iter()
                        .map(|entry| entry.change.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                changes,
                [
                    vec![DataChange::New, DataChange::New],
                    vec![
                        DataChange::Changed {
                            previous: "[]".into()
                        },
                        DataChange::Unchanged
                    ],
                ]
            );
        });
        assert_eq!(annotations(&one, cx).blocks, 2);
        const SELECTORS: [[&str; 2]; 3] = [
            ["walkthrough-step-data-0-0", "walkthrough-step-data-0-1"],
            ["walkthrough-step-data-1-0", "walkthrough-step-data-1-1"],
            ["walkthrough-step-data-2-0", "walkthrough-step-data-2-1"],
        ];
        let drawn_rows = |cx: &mut VisualTestContext| {
            let mut drawn = Vec::new();
            for (step, selectors) in SELECTORS.iter().enumerate() {
                for (row, selector) in selectors.iter().enumerate() {
                    if cx.debug_bounds(selector).is_some() {
                        drawn.push((step, row));
                    }
                }
            }
            drawn
        };
        assert_eq!(drawn_rows(cx), [(0, 0), (0, 1), (2, 0), (2, 1)]);

        one.update(cx, |editor, cx| {
            toggle_collapsed(editor, &StepId::new("c"), cx)
        });
        cx.run_until_parked();
        assert_eq!(drawn_rows(cx), [(0, 0), (0, 1)]);

        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        assert_eq!(drawn_rows(cx), []);
    }

    #[gpui::test]
    async fn test_collapsing_a_step_block(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        let one = active_editor(&test, cx);

        let collapsed = |cx: &mut VisualTestContext| {
            one.read_with(cx, |editor, _| {
                editor
                    .addon::<WalkthroughAddon>()
                    .expect("addon")
                    .blocks
                    .iter()
                    .map(|block| block.renderer.collapsed)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(collapsed(cx), [false, false]);
        one.update(cx, |editor, cx| {
            toggle_collapsed(editor, &StepId::new("c"), cx)
        });
        cx.run_until_parked();
        assert_eq!(collapsed(cx), [false, true]);
        assert_eq!(annotations(&one, cx).blocks, 2);
    }

    fn edit_comment(
        id: &str,
        path: &str,
        lines: std::ops::Range<u32>,
        comment: &str,
    ) -> WalkthroughEditJson {
        let mut step = sample_walkthrough(false)
            .steps
            .into_iter()
            .find(|step| step.id == id)
            .expect("sample step");
        step.path = path.to_string();
        step.start_line = lines.start;
        step.end_line = lines.end;
        step.comment = comment.to_string();
        WalkthroughEditJson {
            steps: vec![walkthrough::WalkthroughStepEditJson { step, after: None }],
            ..Default::default()
        }
    }

    fn block_comments(
        editor: &Entity<Editor>,
        cx: &mut VisualTestContext,
    ) -> Vec<(String, String)> {
        editor.read_with(cx, |editor, _| {
            editor
                .addon::<WalkthroughAddon>()
                .expect("addon")
                .blocks
                .iter()
                .map(|block| {
                    (
                        block.renderer.step.id.to_string(),
                        block.renderer.step.comment.to_string(),
                    )
                })
                .collect()
        })
    }

    #[gpui::test]
    async fn test_editing_another_step_keeps_the_user_in_place(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        let one = active_editor(&test, cx);
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        let two = active_editor(&test, cx);
        assert_ne!(one, two);
        let cursor_before = cursor(&two, cx);
        let annotations_before = annotations(&two, cx);

        one.update(cx, |editor, cx| {
            toggle_collapsed(editor, &StepId::new("c"), cx)
        });
        let project = test.project.clone();
        let message = cx
            .update(|_, cx| {
                walkthrough::edit_walkthrough(
                    edit_comment("a", "root/src/one.rs", 2..4, "Rewritten comment."),
                    &project,
                    cx,
                )
            })
            .await;
        cx.run_until_parked();
        assert_eq!(
            message.as_deref(),
            Ok("Walkthrough updated: 3 steps across 2 files. Step order: a, b, c")
        );

        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        assert_eq!(active_editor(&test, cx), two);
        assert_eq!(cursor(&two, cx), cursor_before);
        assert_eq!(annotations(&two, cx), annotations_before);
        assert!(two.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));

        assert_eq!(
            block_comments(&one, cx),
            [
                ("a".to_string(), "Rewritten comment.".to_string()),
                ("c".to_string(), "Comment for **c**.".to_string()),
            ]
        );
        let collapsed = one.read_with(cx, |editor, _| {
            editor
                .addon::<WalkthroughAddon>()
                .expect("addon")
                .blocks
                .iter()
                .map(|block| block.renderer.collapsed)
                .collect::<Vec<_>>()
        });
        assert_eq!(collapsed, [false, true]);
    }

    #[gpui::test]
    async fn test_editing_away_the_current_step_moves_to_the_clamped_index(
        cx: &mut TestAppContext,
    ) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        let one = active_editor(&test, cx);
        cx.dispatch_action(NextStep);
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("c"));

        let project = test.project.clone();
        cx.update(|_, cx| {
            walkthrough::edit_walkthrough(
                WalkthroughEditJson {
                    remove_steps: vec!["c".to_string()],
                    ..Default::default()
                },
                &project,
                cx,
            )
        })
        .await
        .expect("edit is valid");
        cx.run_until_parked();

        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        let two = active_editor(&test, cx);
        assert_ne!(one, two);
        assert_eq!(cursor(&two, cx), Point::new(4, 0));
        assert_eq!(annotations(&one, cx).blocks, 1);
    }
}
