mod editor_addon;
mod flowchart_view;
mod navigation;

use gpui::App;

pub use flowchart_view::{FlowchartView, parse_check};

pub fn init(cx: &mut App) {
    editor_addon::init(cx);
    navigation::init(cx);
    flowchart_view::init(cx);
}
