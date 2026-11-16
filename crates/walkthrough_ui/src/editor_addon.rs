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
use walkthrough::{GoToStep, StepId, WalkthroughEvent, WalkthroughStep, WalkthroughStore};

const MAX_COMMENT_ROWS: f32 = 14.;
const MAX_BLOCK_WIDTH_IN_EMS: f32 = 110.;
const STALE_MESSAGE: &str = "This code has changed since the walkthrough was written";

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
                refresh_annotations(editor, cx)
            }
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
    refresh_annotations(editor, cx);
}

fn refresh_annotations(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon_mut::<WalkthroughAddon>() else {
        return;
    };
    let store = addon.store.clone();
    let old_block_ids = std::mem::take(&mut addon.blocks)
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
        let renderer = StepBlockRenderer {
            store: store.downgrade(),
            editor: weak_editor.clone(),
            step,
            index,
            step_count,
            previous_step,
            next_step,
            markdown,
            collapsed: false,
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

    let mut active_ranges = Vec::new();
    let mut inactive_ranges = Vec::new();
    for block in &addon.blocks {
        let step = &block.renderer.step;
        if step.is_stale(cx) {
            continue;
        }
        if active_step.as_ref() == Some(&step.id) {
            active_ranges.push(block.range.clone());
        } else {
            inactive_ranges.push(block.range.clone());
        }
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    inactive_ranges.sort_by(|a, b| a.start.cmp(&b.start, &snapshot));

    set_highlights(
        editor,
        HighlightKey::WalkthroughStep,
        inactive_ranges,
        |theme| theme.colors().editor_document_highlight_read_background,
        cx,
    );
    set_highlights(
        editor,
        HighlightKey::WalkthroughActiveStep,
        active_ranges,
        |theme| theme.colors().text_accent.opacity(0.18),
        cx,
    );
    // Blocks restyle themselves from the store when they are next drawn.
    cx.notify();
}

fn set_highlights(
    editor: &mut Editor,
    key: HighlightKey,
    ranges: Vec<Range<Anchor>>,
    color: fn(&theme::Theme) -> Hsla,
    cx: &mut Context<Editor>,
) {
    if ranges.is_empty() {
        editor.clear_background_highlights(key, cx);
    } else {
        editor.highlight_background(key, &ranges, move |_, theme| color(theme), cx);
    }
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
                    .border_color(if is_active {
                        colors.text_accent
                    } else {
                        colors.border
                    })
                    .bg(colors.editor_subheader_background)
                    .text_ui(cx)
                    .child(header)
                    .children(body),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use editor::display_map::{BlockId, ToDisplayPoint as _};
    use gpui::{TestAppContext, VisualTestContext};
    use language::Point;
    use walkthrough::{Dismiss, NextStep};
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
}
