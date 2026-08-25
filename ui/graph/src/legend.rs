use crate::Palette;
use bviz_graph_layout::{EdgeKind, LaidOut};
use gpui::{Div, div, prelude::*, px};

/// Lists the edge kinds actually drawn, in enum order, so it explains only the
/// kinds present in this graph. `None` when none are drawn.
pub(crate) fn legend(g: &LaidOut, palette: &Palette) -> Option<Div> {
    let present: Vec<EdgeKind> = [
        EdgeKind::Query,
        EdgeKind::Cquery,
        EdgeKind::Aquery,
        EdgeKind::Transitive,
    ]
    .into_iter()
    .filter(|k| g.connectors.iter().any(|c| c.kinds.contains(k)))
    .collect();
    (!present.is_empty()).then(|| {
        div()
            .absolute()
            .left_2()
            .bottom_2()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(palette.container)
            .children(present.into_iter().map(|k| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(16.)).h(px(2.)).bg(palette.edge_color(k, false)))
                    .child(div().text_xs().text_color(palette.label).child(k.label()))
            }))
    })
}
