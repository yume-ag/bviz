use crate::search::search_field;
use crate::{Controls, GraphView, Palette};
use bviz_ui_components::{Button, Toolbar, spinner};
use gpui::{ClickEvent, Context, IntoElement, div, prelude::*, px};

pub(crate) fn toolbar(
    controls: Controls,
    palette: &Palette,
    cx: &mut Context<GraphView>,
) -> Toolbar {
    let mut bar = Toolbar::new()
        .child(controls.picker)
        .child(controls.kind_picker)
        .child(search_field(
            controls.search_input,
            controls.search_focused,
            controls.search_matches,
            controls.search_over,
            palette,
            cx,
        ))
        .child(
            Button::new(
                "neighborhood-toggle",
                cx.listener(|view, _: &ClickEvent, _, cx| view.toggle_neighborhood(cx)),
            )
            .label("neighbors")
            .selected(controls.neighborhood_enabled),
        );
    // The strip only ever offers a control the mode's layout can answer, so
    // everything on it acts on what is drawn.
    if controls.mode.honors_reduce() {
        bar = bar.child(
            Button::new(
                "reduce-toggle",
                cx.listener(|view, _: &ClickEvent, _, cx| view.toggle_reduce(cx)),
            )
            .label("reduce")
            .selected(controls.reduced),
        );
    }
    bar.when(controls.relaying, |bar| {
        bar.trailing(relaying_indicator(palette))
    })
}

/// Shown while a relayout runs, so the prior layout stays interactive with a
/// visible cue that a new one is in flight.
fn relaying_indicator(palette: &Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(palette.label)
        .child(spinner("relaying-spinner", px(14.), palette.label))
        .child("laying out…")
}
