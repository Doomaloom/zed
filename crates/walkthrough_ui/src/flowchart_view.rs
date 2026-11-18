use std::sync::Arc;

use crate::editor_addon::{highlight_hsla, legend_chip};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use feature_flags::{FeatureFlagAppExt as _, WalkthroughFeatureFlag};
use gpui::{
    Action as _, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, RenderImage, Rgba,
    SharedString, Size, Subscription, SvgRenderer, Task, Window, canvas, img, point, prelude::*,
    px, size,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use mermaid_render::MermaidTheme;
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};
use settings::Settings as _;
use theme::GlobalTheme;
use theme_settings::ThemeSettings;
use ui::prelude::*;
use util::ResultExt as _;
use walkthrough::{
    Flowchart, GlobalFlowchartCheck, GoToStep, StepId, ToggleFlowchart, WalkthroughEvent,
    WalkthroughStore,
};
use workspace::{
    Item, SaveIntent, Workspace,
    item::{ItemHandle as _, TabContentParams},
};

pub fn init(cx: &mut App) {
    let svg_renderer = cx.svg_renderer();
    cx.set_global(GlobalFlowchartCheck(Arc::new(move |source| {
        parse_check(source, &svg_renderer)
    })));

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let store = WalkthroughStore::for_project(workspace.project(), cx);
        cx.subscribe_in(
            &store,
            window,
            |workspace, store, event: &WalkthroughEvent, window, cx| match event {
                WalkthroughEvent::Created(_) => {
                    let has_flowchart = store
                        .read(cx)
                        .active()
                        .is_some_and(|walkthrough| walkthrough.flowchart.is_some());
                    if has_flowchart {
                        FlowchartView::open(workspace, window, cx);
                    }
                }
                WalkthroughEvent::Dismissed => FlowchartView::close(workspace, window, cx),
                WalkthroughEvent::ActiveStepChanged { .. } => {}
            },
        )
        .detach();

        workspace.register_action_renderer(move |div, _, _, cx| {
            if store.read(cx).active().is_none() || !cx.has_flag::<WalkthroughFeatureFlag>() {
                return div;
            }
            div.on_action(cx.listener(|workspace, _: &ToggleFlowchart, window, cx| {
                if workspace.item_of_type::<FlowchartView>(cx).is_some() {
                    FlowchartView::close(workspace, window, cx);
                } else {
                    FlowchartView::open(workspace, window, cx);
                }
            }))
        });
    })
    .detach();
}

/// Checks that `mermaid` parses and lays out with the renderer the flowchart
/// view uses, returning the renderer's error text otherwise.
pub fn parse_check(mermaid: &str, svg_renderer: &SvgRenderer) -> Result<(), String> {
    let monochrome = gpui::black();
    let paper = gpui::white();
    let theme = MermaidTheme {
        dark_mode: false,
        font_family: "sans-serif".to_string(),
        background: paper,
        primary_color: paper,
        primary_text_color: monochrome,
        primary_border_color: monochrome,
        secondary_color: paper,
        tertiary_color: paper,
        line_color: monochrome,
        text_color: monochrome,
        edge_label_background: paper,
        cluster_background: paper,
        cluster_border: monochrome,
        note_background: paper,
        note_border: monochrome,
        actor_background: paper,
        actor_border: monochrome,
        activation_background: paper,
        activation_border: monochrome,
        git_branch_colors: [monochrome; 8],
        git_branch_label_colors: [paper; 8],
        er_attr_bg_odd: paper,
        er_attr_bg_even: paper,
        error_color: monochrome,
        warning_color: monochrome,
        accent_colors: Vec::new(),
    };
    mermaid_render::render_to_svg(mermaid, &theme, svg_renderer)
        .map(|_| ())
        .map_err(|error| error.root_cause().to_string())
}

fn mermaid_theme(cx: &App) -> MermaidTheme {
    let colors = cx.theme().colors();
    let players = cx.theme().players();
    let git_branch_colors = std::array::from_fn(|index| players.0[index % players.0.len()].cursor);
    MermaidTheme {
        dark_mode: !cx.theme().appearance.is_light(),
        font_family: gpui::font_name_with_fallbacks(
            ThemeSettings::get_global(cx).mermaid_font_family().as_ref(),
            "system-ui",
        )
        .to_string(),
        background: colors.editor_background,
        primary_color: colors.surface_background,
        primary_text_color: colors.text,
        primary_border_color: colors.border,
        secondary_color: colors.element_background,
        tertiary_color: colors.ghost_element_hover,
        line_color: colors.border,
        text_color: colors.text,
        edge_label_background: colors.editor_background,
        cluster_background: colors.panel_background,
        cluster_border: colors.border_variant,
        note_background: colors.surface_background,
        note_border: colors.border_variant,
        actor_background: colors.element_background,
        actor_border: colors.border,
        activation_background: colors.ghost_element_hover,
        activation_border: colors.border,
        git_branch_colors,
        git_branch_label_colors: git_branch_colors.map(mermaid_render::text_color_for_background),
        er_attr_bg_odd: colors.surface_background,
        er_attr_bg_even: colors.element_background,
        error_color: cx.theme().status().error,
        warning_color: cx.theme().status().warning,
        // Left empty so nodes share one colour and the active node stands out.
        accent_colors: Vec::new(),
    }
}

fn css_hex(color: Hsla) -> String {
    let color = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(color.r),
        channel(color.g),
        channel(color.b)
    )
}

/// Appends a `style` directive that makes `active_node` stand out. The node
/// must be one of the flowchart's parsed ids, otherwise the source is
/// returned unchanged so that it still parses.
fn styled_source(flowchart: &Flowchart, active_node: Option<&str>, cx: &App) -> SharedString {
    let Some(active_node) = active_node.filter(|node| {
        flowchart
            .node_ids
            .iter()
            .any(|node_id| node_id.as_ref() == *node)
    }) else {
        return flowchart.mermaid.clone();
    };
    let colors = cx.theme().colors();
    let stroke = colors.text_accent;
    // Blended by hand because node labels keep the theme's text colour, so
    // the fill has to stay close to the regular node background.
    let fill = colors.surface_background.blend(stroke.opacity(0.3));
    format!(
        "{}\n  style {active_node} fill:{},stroke:{},stroke-width:2px",
        flowchart.mermaid.trim_end(),
        css_hex(fill),
        css_hex(stroke),
    )
    .into()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeRect {
    pub id: SharedString,
    pub bounds: Bounds<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiagramLayout {
    /// The diagram's size in SVG user units, which `nodes` are relative to.
    pub size: Size<f32>,
    pub nodes: Vec<NodeRect>,
}

fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    element
        .try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|attribute| {
            attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|value| value.into_owned())
        })
}

fn numbers(text: &str) -> impl Iterator<Item = f32> + '_ {
    text.split(|character: char| character == ',' || character.is_whitespace())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f32>().ok())
}

fn translation(element: &BytesStart) -> Point<f32> {
    let transform = attribute(element, "transform").unwrap_or_default();
    let Some(arguments) = transform
        .split("translate(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
    else {
        return point(0., 0.);
    };
    let mut values = numbers(arguments);
    let x = values.next().unwrap_or(0.);
    let y = values.next().unwrap_or(0.);
    point(x, y)
}

/// The bounds of a node's shape element in its own coordinate system.
fn shape_bounds(element: &BytesStart) -> Option<Bounds<f32>> {
    let number = |name: &str| attribute(element, name).and_then(|value| value.parse::<f32>().ok());
    let offset = translation(element);
    let bounds = match element.local_name().as_ref() {
        b"rect" => Bounds::new(
            point(number("x").unwrap_or(0.), number("y").unwrap_or(0.)),
            size(number("width")?, number("height")?),
        ),
        b"circle" => {
            let radius = number("r")?;
            Bounds::new(
                point(
                    number("cx").unwrap_or(0.) - radius,
                    number("cy").unwrap_or(0.) - radius,
                ),
                size(radius * 2., radius * 2.),
            )
        }
        b"ellipse" => {
            let (radius_x, radius_y) = (number("rx")?, number("ry")?);
            Bounds::new(
                point(
                    number("cx").unwrap_or(0.) - radius_x,
                    number("cy").unwrap_or(0.) - radius_y,
                ),
                size(radius_x * 2., radius_y * 2.),
            )
        }
        b"polygon" => {
            let values = numbers(&attribute(element, "points")?).collect::<Vec<_>>();
            let xs = values.iter().step_by(2).copied();
            let ys = values.iter().skip(1).step_by(2).copied();
            let (min_x, max_x) = (xs.clone().reduce(f32::min)?, xs.reduce(f32::max)?);
            let (min_y, max_y) = (ys.clone().reduce(f32::min)?, ys.reduce(f32::max)?);
            Bounds::new(point(min_x, min_y), size(max_x - min_x, max_y - min_y))
        }
        _ => return None,
    };
    Some(Bounds::new(bounds.origin + offset, bounds.size))
}

/// Extracts the node id from an element id such as
/// `merman-3-flowchart-parse-0`.
fn node_id_from_element_id(element_id: &str) -> Option<&str> {
    let (_, rest) = element_id.split_once("flowchart-")?;
    let (node_id, suffix) = rest.rsplit_once('-')?;
    suffix
        .chars()
        .all(|character| character.is_ascii_digit())
        .then_some(node_id)
}

/// Reads the diagram size and the rectangle of every flowchart node from the
/// SVG the Mermaid renderer emits. Nodes are `<g class="node …">` groups
/// positioned by `translate` transforms, whose first shape child gives the
/// node's extent.
pub(crate) fn diagram_layout(svg: &str) -> Result<DiagramLayout> {
    let mut reader = Reader::from_str(svg);
    let mut view_box = None;
    let mut nodes: Vec<NodeRect> = Vec::new();
    let mut offsets: Vec<Point<f32>> = Vec::new();
    // The node whose shape has not been seen yet, with the depth of its group.
    let mut pending_node: Option<(SharedString, usize)> = None;

    loop {
        let event = reader.read_event().context("reading flowchart SVG")?;
        let (element, is_empty) = match &event {
            Event::Start(element) => (element, false),
            Event::Empty(element) => (element, true),
            Event::End(_) => {
                offsets.pop();
                if pending_node
                    .as_ref()
                    .is_some_and(|(_, depth)| *depth > offsets.len())
                {
                    pending_node = None;
                }
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };

        let parent_offset = offsets.last().copied().unwrap_or(point(0., 0.));
        match element.local_name().as_ref() {
            b"svg" if view_box.is_none() => {
                let values = attribute(element, "viewBox")
                    .map(|view_box| numbers(&view_box).collect::<Vec<_>>())
                    .unwrap_or_default();
                if let [min_x, min_y, width, height] = values[..] {
                    view_box = Some(Bounds::new(point(min_x, min_y), size(width, height)));
                }
            }
            b"g" => {
                let is_node = attribute(element, "class")
                    .is_some_and(|class| class.split_whitespace().any(|name| name == "node"));
                if is_node
                    && !is_empty
                    && let Some(element_id) = attribute(element, "id")
                    && let Some(node_id) = node_id_from_element_id(&element_id)
                {
                    pending_node = Some((node_id.to_string().into(), offsets.len() + 1));
                }
            }
            _ => {
                if let Some((node_id, _)) = &pending_node
                    && let Some(bounds) = shape_bounds(element)
                {
                    nodes.push(NodeRect {
                        id: node_id.clone(),
                        bounds: Bounds::new(bounds.origin + parent_offset, bounds.size),
                    });
                    pending_node = None;
                }
            }
        }
        if !is_empty {
            offsets.push(parent_offset + translation(element));
        }
    }

    let view_box = view_box.ok_or_else(|| anyhow!("flowchart SVG has no viewBox"))?;
    for node in &mut nodes {
        node.bounds.origin = node.bounds.origin - view_box.origin;
    }
    Ok(DiagramLayout {
        size: view_box.size,
        nodes,
    })
}

/// Maps a position in the view to the node drawn there, given where the
/// diagram is painted. The painted size may differ from the diagram's own
/// size by any scale.
pub(crate) fn node_at(
    position: Point<Pixels>,
    painted_bounds: Bounds<Pixels>,
    layout: &DiagramLayout,
) -> Option<&NodeRect> {
    if !painted_bounds.contains(&position)
        || painted_bounds.size.width <= px(0.)
        || painted_bounds.size.height <= px(0.)
    {
        return None;
    }
    let relative = position - painted_bounds.origin;
    let diagram_position = point(
        relative.x / painted_bounds.size.width * layout.size.width,
        relative.y / painted_bounds.size.height * layout.size.height,
    );
    layout
        .nodes
        .iter()
        .find(|node| node.bounds.contains(&diagram_position))
}

struct RenderedDiagram {
    image: Arc<RenderImage>,
    layout: DiagramLayout,
}

enum DiagramState {
    Rendering,
    Ready(RenderedDiagram),
    Failed(SharedString),
}

pub struct FlowchartView {
    store: Entity<WalkthroughStore>,
    focus_handle: FocusHandle,
    summary: Option<Entity<Markdown>>,
    /// Rendered diagrams keyed by the styled source sent to the renderer, so
    /// that revisiting a step does not render again.
    diagrams: HashMap<SharedString, DiagramState>,
    current_source: Option<SharedString>,
    painted_bounds: Option<Bounds<Pixels>>,
    hovered_step: Option<StepId>,
    render_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl FlowchartView {
    fn new(store: Entity<WalkthroughStore>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, event, cx| {
                if matches!(
                    event,
                    WalkthroughEvent::Created(_) | WalkthroughEvent::Dismissed
                ) {
                    this.reset(cx);
                }
                this.refresh(cx);
            }),
            cx.observe_global::<GlobalTheme>(|this, cx| {
                this.clear_diagrams(cx);
                this.refresh(cx);
            }),
        ];
        let mut this = Self {
            store,
            focus_handle: cx.focus_handle(),
            summary: None,
            diagrams: HashMap::default(),
            current_source: None,
            painted_bounds: None,
            hovered_step: None,
            render_tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.reset(cx);
        this.refresh(cx);
        this
    }

    /// Shows the flowchart view in a pane to the right of the code, reusing
    /// the existing view if there is one. Focus stays where it was.
    pub fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        if let Some(view) = workspace.item_of_type::<Self>(cx) {
            if let Some(pane) = workspace.pane_for(&view) {
                pane.update(cx, |pane, cx| {
                    if let Some(index) = pane.index_for_item(&view) {
                        pane.activate_item(index, false, false, window, cx);
                    }
                });
            }
            return view;
        }

        let store = WalkthroughStore::for_project(workspace.project(), cx);
        let view = cx.new(|cx| Self::new(store, cx));
        let previously_focused = window.focused(cx);
        let pane = workspace.adjacent_pane(window, cx);
        workspace.add_item(pane, Box::new(view.clone()), None, false, false, window, cx);
        // Creating the adjacent pane focuses it, but reading continues in
        // the code.
        if let Some(previously_focused) = previously_focused {
            window.focus(&previously_focused, cx);
        }
        view
    }

    pub fn close(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let views = workspace.items_of_type::<Self>(cx).collect::<Vec<_>>();
        for view in views {
            let Some(pane) = workspace.pane_for(&view) else {
                continue;
            };
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(view.item_id(), SaveIntent::Skip, window, cx)
            })
            .detach_and_log_err(cx);
        }
    }

    fn clear_diagrams(&mut self, cx: &mut Context<Self>) {
        for (_, diagram) in self.diagrams.drain() {
            if let DiagramState::Ready(diagram) = diagram {
                cx.drop_image(diagram.image, None);
            }
        }
        self.render_tasks.clear();
        self.current_source = None;
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.clear_diagrams(cx);
        self.hovered_step = None;
        let summary = self
            .store
            .read(cx)
            .active()
            .map(|walkthrough| walkthrough.summary.clone());
        self.summary = summary.map(|summary| cx.new(|cx| Markdown::new(summary, None, None, cx)));
    }

    /// Makes sure the diagram for the active step is rendered or rendering.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let source = store
            .active()
            .and_then(|walkthrough| walkthrough.flowchart.as_ref())
            .map(|flowchart| {
                let active_node = store
                    .active_step()
                    .and_then(|step| step.node_id.as_ref())
                    .map(|node| node.as_ref());
                styled_source(flowchart, active_node, cx)
            });
        self.current_source = source.clone();
        cx.notify();

        let Some(source) = source else {
            return;
        };
        if self.diagrams.contains_key(&source) {
            return;
        }
        self.diagrams
            .insert(source.clone(), DiagramState::Rendering);

        let svg_renderer = cx.svg_renderer();
        let theme = mermaid_theme(cx);
        let render = cx.background_spawn({
            let source = source.clone();
            async move {
                let svg = mermaid_render::render_to_svg(&source, &theme, &svg_renderer)
                    .map_err(|error| anyhow!("{}", error.root_cause()))?;
                let layout = diagram_layout(&svg)?;
                let parsed = svg_renderer
                    .parse_svg(svg.as_bytes())
                    .map_err(|error| anyhow!("{error}"))?;
                let image = svg_renderer
                    .render_parsed(&parsed, 1.)
                    .map_err(|error| anyhow!("{error}"))?;
                anyhow::Ok(RenderedDiagram { image, layout })
            }
        });
        self.render_tasks.push(cx.spawn(async move |this, cx| {
            let state = match render.await {
                Ok(diagram) => DiagramState::Ready(diagram),
                Err(error) => DiagramState::Failed(error.to_string().into()),
            };
            this.update(cx, |this, cx| {
                // The cache may have been cleared for a new walkthrough or
                // theme while this was rendering.
                if let Some(entry @ DiagramState::Rendering) = this.diagrams.get_mut(&source) {
                    *entry = state;
                } else if let DiagramState::Ready(diagram) = state {
                    cx.drop_image(diagram.image, None);
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    #[cfg(test)]
    pub(crate) fn current_source(&self) -> Option<&SharedString> {
        self.current_source.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn rendered_node_ids(&self) -> Option<Vec<String>> {
        match self.current_diagram()? {
            DiagramState::Ready(diagram) => Some(
                diagram
                    .layout
                    .nodes
                    .iter()
                    .map(|node| node.id.to_string())
                    .collect(),
            ),
            DiagramState::Rendering | DiagramState::Failed(_) => None,
        }
    }

    fn current_diagram(&self) -> Option<&DiagramState> {
        self.diagrams.get(self.current_source.as_ref()?)
    }

    /// The step that clicking at `position` jumps to: the first step linked
    /// to the node drawn there.
    fn step_at(&self, position: Point<Pixels>, cx: &App) -> Option<StepId> {
        let Some(DiagramState::Ready(diagram)) = self.current_diagram() else {
            return None;
        };
        let node = node_at(position, self.painted_bounds?, &diagram.layout)?;
        step_for_node(&self.store, &node.id, cx)
    }

    fn render_diagram(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let source = self.current_source.clone()?;
        let element = match self.diagrams.get(&source)? {
            DiagramState::Rendering => Label::new("Rendering flowchart…")
                .color(Color::Muted)
                .into_any_element(),
            DiagramState::Failed(error) => v_flex()
                .gap_2()
                .child(
                    Label::new(format!("The flowchart could not be rendered: {error}"))
                        .color(Color::Warning),
                )
                .child(
                    div()
                        .id("walkthrough-flowchart-source")
                        .debug_selector(|| "walkthrough-flowchart-source".to_string())
                        .p_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .bg(cx.theme().colors().editor_background)
                        .font_buffer(cx)
                        .text_buffer(cx)
                        .overflow_x_scroll()
                        .child(source),
                )
                .into_any_element(),
            DiagramState::Ready(diagram) => {
                let diagram_size = diagram.layout.size;
                let view = cx.entity();
                div()
                    .id("walkthrough-flowchart-diagram")
                    .relative()
                    .w_full()
                    .max_w(px(diagram_size.width))
                    .when(diagram_size.height > 0., |this| {
                        this.aspect_ratio(diagram_size.width / diagram_size.height)
                    })
                    .when(self.hovered_step.is_some(), |this| this.cursor_pointer())
                    .child(img(ImageSource::Render(diagram.image.clone())).size_full())
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                view.update(cx, |this, _| this.painted_bounds = Some(bounds));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        let hovered_step = this.step_at(event.position, cx);
                        if hovered_step != this.hovered_step {
                            this.hovered_step = hovered_step;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            if let Some(step) = this.step_at(event.position, cx) {
                                go_to_step(&step, window, cx);
                            }
                        }),
                    )
                    .into_any_element()
            }
        };
        Some(element)
    }

    fn render_step_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let walkthrough = store.active()?;
        let active_index = store.active_step_index();
        let colors = cx.theme().colors();

        let rows = walkthrough.steps.iter().enumerate().map(|(index, step)| {
            let file_label = &step.file_label;
            let location = if step.is_stale(cx) {
                format!("{file_label} (code changed)")
            } else {
                format!("{file_label}:{}", step.point_range(cx).start.row + 1)
            };
            let step_id = step.id.clone();
            h_flex()
                .id(("walkthrough-step-row", index))
                .debug_selector(|| format!("walkthrough-step-row-{index}"))
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .items_start()
                .rounded_sm()
                .cursor_pointer()
                .when(active_index == Some(index), |this| {
                    this.bg(colors.element_selected)
                })
                .hover(|style| style.bg(colors.element_hover))
                .child(
                    Label::new(format!("{}.", index + 1))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .when_some(step.color, |this, color| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .size_2()
                                            .rounded_full()
                                            .bg(highlight_hsla(color, cx.theme())),
                                    )
                                })
                                .child(Label::new(step.title.clone())),
                        )
                        .child(
                            Label::new(location)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .on_click(move |_, window, cx| go_to_step(&step_id, window, cx))
        });
        let legend = (!walkthrough.legend.is_empty()).then(|| {
            h_flex().px_2().pb_1().gap_3().flex_wrap().children(
                walkthrough
                    .legend
                    .iter()
                    .map(|entry| legend_chip(entry.color, entry.label.clone(), cx)),
            )
        });
        let example = walkthrough.example.clone().map(|example| {
            div()
                .id("walkthrough-example")
                .debug_selector(|| "walkthrough-example".to_string())
                .px_2()
                .pb_1()
                .child(Label::new(example).size(LabelSize::Small))
        });
        Some(
            v_flex()
                .gap_0p5()
                .children(example)
                .children(legend)
                .children(rows)
                .into_any_element(),
        )
    }
}

fn step_for_node(store: &Entity<WalkthroughStore>, node_id: &str, cx: &App) -> Option<StepId> {
    store
        .read(cx)
        .active()?
        .steps
        .iter()
        .find(|step| step.node_id.as_deref() == Some(node_id))
        .map(|step| step.id.clone())
}

fn go_to_step(step: &StepId, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        GoToStep {
            id: step.to_string(),
        }
        .boxed_clone(),
        cx,
    );
}

impl EventEmitter<()> for FlowchartView {}

impl Focusable for FlowchartView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for FlowchartView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.store.read(cx).active() {
            Some(walkthrough) => format!("Walkthrough: {}", walkthrough.title).into(),
            None => "Walkthrough".into(),
        }
    }

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(params.text_color())
            .truncate()
            .into_any_element()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }
}

impl Render for FlowchartView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .store
            .read(cx)
            .active()
            .map(|walkthrough| walkthrough.title.clone());

        v_flex()
            .id("walkthrough-flowchart")
            .key_context("WalkthroughFlowchart")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_4()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .map(|this| match title {
                Some(title) => this
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Headline::new(title).size(HeadlineSize::Small))
                            .children(self.summary.clone().map(|summary| {
                                MarkdownElement::new(
                                    summary,
                                    MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                )
                            })),
                    )
                    .children(self.render_diagram(cx))
                    .children(self.render_step_list(cx)),
                None => this.child(Label::new("No active walkthrough").color(Color::Muted)),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use editor::Editor;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use walkthrough::{Dismiss, NextStep, Walkthrough};

    fn flowchart_view(
        test: &TestWorkspace,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<FlowchartView>> {
        test.workspace.read_with(cx, |workspace, cx| {
            workspace.item_of_type::<FlowchartView>(cx)
        })
    }

    fn current_source(view: &Entity<FlowchartView>, cx: &mut VisualTestContext) -> Option<String> {
        view.read_with(cx, |view, _| {
            view.current_source
                .as_ref()
                .map(|source| source.to_string())
        })
    }

    fn layout(view: &Entity<FlowchartView>, cx: &mut VisualTestContext) -> DiagramLayout {
        view.read_with(cx, |view, _| match view.current_diagram() {
            Some(DiagramState::Ready(diagram)) => diagram.layout.clone(),
            Some(DiagramState::Rendering) => panic!("diagram is still rendering"),
            Some(DiagramState::Failed(error)) => panic!("diagram failed to render: {error}"),
            None => panic!("there is no diagram"),
        })
    }

    #[gpui::test]
    async fn test_example_scenario_is_shown_above_the_steps(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        cx.dispatch_action(ToggleFlowchart);
        cx.run_until_parked();
        assert!(cx.debug_bounds("walkthrough-step-row-0").is_some());
        assert!(cx.debug_bounds("walkthrough-example").is_none());

        let mut json = sample_walkthrough(false);
        json.example = Some("A user adds 2 units to an empty cart".to_string());
        load_walkthrough(json, &test, cx).await;
        cx.run_until_parked();
        let example = cx
            .debug_bounds("walkthrough-example")
            .expect("the scenario should be drawn");
        let first_row = cx
            .debug_bounds("walkthrough-step-row-0")
            .expect("step rows should be drawn");
        assert!(example.bottom() <= first_row.top());
    }

    #[gpui::test]
    async fn test_view_opens_only_for_walkthroughs_with_a_flowchart(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;

        load_walkthrough(sample_walkthrough(false), &test, cx).await;
        assert_eq!(flowchart_view(&test, cx), None);

        // Without a flowchart the view can still be opened as a step list.
        cx.dispatch_action(ToggleFlowchart);
        cx.run_until_parked();
        let view = flowchart_view(&test, cx).expect("toggle should open the view");
        assert_eq!(current_source(&view, cx), None);
        assert!(cx.debug_bounds("walkthrough-step-row-0").is_some());
        cx.dispatch_action(ToggleFlowchart);
        cx.run_until_parked();
        assert_eq!(flowchart_view(&test, cx), None);

        load_walkthrough(sample_walkthrough(true), &test, cx).await;
        let view = flowchart_view(&test, cx).expect("flowchart should open on creation");
        test.workspace.read_with(cx, |workspace, cx| {
            let flowchart_pane = workspace.pane_for(&view).expect("flowchart pane");
            assert_ne!(&flowchart_pane, workspace.active_pane());
            assert!(
                workspace.active_item_as::<Editor>(cx).is_some(),
                "focus stays in the code"
            );
        });

        cx.dispatch_action(Dismiss);
        cx.run_until_parked();
        assert_eq!(flowchart_view(&test, cx), None);
    }

    #[gpui::test]
    async fn test_active_node_is_styled(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(true), &test, cx).await;
        let view = flowchart_view(&test, cx).expect("flowchart should be open");
        let base = sample_walkthrough(true).flowchart.expect("flowchart");

        let source = current_source(&view, cx).expect("source");
        assert!(source.starts_with(&base));
        assert!(source.contains("\n  style parse fill:#"), "{source}");
        assert_eq!(
            layout(&view, cx)
                .nodes
                .iter()
                .map(|node| node.id.to_string())
                .collect::<Vec<_>>(),
            ["parse", "verify"]
        );

        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        let source = current_source(&view, cx).expect("source");
        assert!(source.contains("\n  style verify fill:#"), "{source}");
        assert!(!source.contains("style parse"), "{source}");
        layout(&view, cx);

        // A step without a node leaves the diagram unstyled.
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(current_source(&view, cx), Some(base));

        // Revisited steps reuse their rendered diagram.
        assert_eq!(view.read_with(cx, |view, _| view.diagrams.len()), 3);
        cx.dispatch_action(GoToStep {
            id: "a".to_string(),
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.diagrams.len()), 3);
    }

    #[gpui::test]
    async fn test_clicking_step_rows_and_nodes(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(sample_walkthrough(true), &test, cx).await;
        let view = flowchart_view(&test, cx).expect("flowchart should be open");
        assert_eq!(active_step(&test, cx).as_deref(), Some("a"));

        let row = cx
            .debug_bounds("walkthrough-step-row-2")
            .expect("step rows should be drawn");
        cx.simulate_click(row.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("c"));

        // Clicking a node jumps to the step linked to it.
        let diagram_layout = layout(&view, cx);
        let painted_bounds = view
            .read_with(cx, |view, _| view.painted_bounds)
            .expect("diagram should be painted");
        let position_in_view = |diagram_position: Point<f32>| {
            painted_bounds.origin
                + point(
                    painted_bounds.size.width * (diagram_position.x / diagram_layout.size.width),
                    painted_bounds.size.height * (diagram_position.y / diagram_layout.size.height),
                )
        };
        let verify = diagram_layout
            .nodes
            .iter()
            .find(|node| node.id.as_ref() == "verify")
            .expect("verify node");
        cx.simulate_click(position_in_view(verify.bounds.center()), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));

        // Empty space between the two nodes is not clickable.
        let parse = diagram_layout
            .nodes
            .iter()
            .find(|node| node.id.as_ref() == "parse")
            .expect("parse node");
        let gap = point(
            parse.bounds.center().x,
            (parse.bounds.bottom() + verify.bounds.top()) / 2.,
        );
        cx.simulate_click(position_in_view(gap), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
    }

    #[gpui::test]
    async fn test_render_failure_shows_fallback(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;

        // Loaded without the parse check, as if the renderer rejected at view
        // time something that validation had accepted.
        let mut json = sample_walkthrough(true);
        json.flowchart = Some("flowchart TD\n  parse[ --> ]]verify".to_string());
        for step in &mut json.steps {
            step.node = None;
        }
        let project = test.project.clone();
        let walkthrough = cx
            .update(|_, cx| Walkthrough::from_json(json, &project, |_| Ok(()), cx))
            .await
            .expect("walkthrough should load without the parse check");
        test.store
            .update(cx, |store, cx| store.set_walkthrough(walkthrough, cx));
        cx.run_until_parked();

        let view = flowchart_view(&test, cx).expect("flowchart should be open");
        view.read_with(cx, |view, _| {
            assert!(matches!(
                view.current_diagram(),
                Some(DiagramState::Failed(_))
            ));
        });
        assert!(cx.debug_bounds("walkthrough-flowchart-source").is_some());
        assert!(cx.debug_bounds("walkthrough-step-row-0").is_some());
    }

    #[gpui::test]
    fn test_parse_check(cx: &mut TestAppContext) {
        let svg_renderer = cx.update(|cx| cx.svg_renderer());
        assert_eq!(
            parse_check(
                "flowchart TD\n  parse[Parse token] --> verify{Valid?}",
                &svg_renderer
            ),
            Ok(())
        );
        let error = parse_check("flowchart TD\n  parse[ --> ]]verify", &svg_renderer)
            .expect_err("invalid flowchart should be rejected");
        assert!(error.contains("parse error"), "{error}");
        assert!(parse_check("not a diagram", &svg_renderer).is_err());
    }

    #[gpui::test]
    fn test_diagram_layout_from_rendered_svg(cx: &mut TestAppContext) {
        let svg_renderer = cx.update(|cx| cx.svg_renderer());
        let theme = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            mermaid_theme(cx)
        });
        let svg = mermaid_render::render_to_svg(
            "flowchart TD\n  parse[Parse token] --> verify{Valid?}\n  verify -- yes --> handler((Run))\n  subgraph errors\n    reject[401]\n  end\n  verify -- no --> reject",
            &theme,
            &svg_renderer,
        )
        .expect("flowchart should render");
        let layout = diagram_layout(&svg).expect("layout should be readable");

        let node = |id: &str| {
            layout
                .nodes
                .iter()
                .find(|node| node.id.as_ref() == id)
                .unwrap_or_else(|| panic!("node {id} should have a rectangle"))
                .bounds
        };
        assert_eq!(layout.nodes.len(), 4);
        let diagram = Bounds::new(point(0., 0.), layout.size);
        for id in ["parse", "verify", "handler", "reject"] {
            let bounds = node(id);
            assert!(bounds.size.width > 10. && bounds.size.height > 10., "{id}");
            assert!(
                diagram.contains(&bounds.origin) && diagram.contains(&bounds.bottom_right()),
                "{id} should lie within the diagram: {bounds:?} in {diagram:?}"
            );
        }
        // Top-down: parse above verify, which is above both of its targets.
        assert!(node("parse").bottom() <= node("verify").top());
        assert!(node("verify").bottom() <= node("handler").top());
        assert!(node("verify").bottom() <= node("reject").top());
        assert!(!node("handler").intersects(&node("reject")));
    }

    #[test]
    fn test_node_at_maps_clicks_at_any_scale() {
        let layout = DiagramLayout {
            size: size(200., 100.),
            nodes: vec![
                NodeRect {
                    id: "first".into(),
                    bounds: Bounds::new(point(10., 10.), size(60., 30.)),
                },
                NodeRect {
                    id: "second".into(),
                    bounds: Bounds::new(point(120., 50.), size(60., 40.)),
                },
            ],
        };
        let node_id_at = |x: f32, y: f32, painted_bounds: Bounds<Pixels>| {
            node_at(point(px(x), px(y)), painted_bounds, &layout).map(|node| node.id.to_string())
        };

        // Painted at half size, offset within the view.
        let half = Bounds::new(point(px(50.), px(20.)), size(px(100.), px(50.)));
        assert_eq!(node_id_at(60., 30., half).as_deref(), Some("first"));
        assert_eq!(node_id_at(130., 60., half).as_deref(), Some("second"));
        assert_eq!(node_id_at(100., 30., half), None);
        assert_eq!(node_id_at(40., 30., half), None, "outside the diagram");

        // Painted at double size: the same view positions land elsewhere.
        let double = Bounds::new(point(px(50.), px(20.)), size(px(400.), px(200.)));
        assert_eq!(node_id_at(60., 30., double), None);
        assert_eq!(node_id_at(130., 60., double).as_deref(), Some("first"));
        assert_eq!(node_id_at(300., 130., double).as_deref(), Some("second"));
        assert_eq!(node_id_at(189., 99., double).as_deref(), Some("first"));
        assert_eq!(node_id_at(191., 101., double), None);
    }
}
