//! Live node-label search: a text field whose dropdown of matching labels
//! opens while it holds focus, each row toggling that node's visibility.

use crate::model;
use crate::{GraphView, Palette};
use bviz_graph_layout::Id;
use bviz_graph_layout::graph::{Graph, Node};
use bviz_ui_components::TextInput;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, IntoElement, Pixels, Subscription, deferred, div,
    prelude::*, px,
};

/// Matches listed below the field are capped to this many, alphabetical, so a
/// broad or empty query does not render thousands of rows.
const MAX_MATCHES: usize = 30;

/// Width of the field and the dropdown beneath it.
const FIELD_WIDTH: Pixels = px(220.);

/// Cap on the dropdown's own height before it scrolls.
const MENU_MAX_HEIGHT: Pixels = px(320.);

/// Where the dropdown is anchored: approximately the field's own rendered
/// height (its line height plus padding and border) plus a small gap. Not
/// measured, since [`crate::toolbar`] has no window to measure it against.
const MENU_TOP: Pixels = px(34.);

/// The search field's own state. Its dropdown has no separate open/closed
/// flag: it is open exactly when the field holds focus, so clicking anywhere
/// else `track_focus`ed (including the canvas) closes it for free.
pub(crate) struct Search {
    pub(crate) input: Entity<TextInput>,
    /// Re-renders the toolbar as the query changes; `TextInput` notifies on
    /// every edit.
    _observer: Subscription,
}

impl Search {
    pub(crate) fn new(cx: &mut Context<GraphView>) -> Self {
        let input = cx.new(|cx| TextInput::new(cx, "Search labels…", 0));
        let observer = cx.observe(&input, |_, _, cx| cx.notify());
        Self {
            input,
            _observer: observer,
        }
    }
}

/// One label the current query matches, and whether that node is in the
/// visible set — a search row's checkmark.
pub(crate) struct Match {
    pub(crate) id: Id,
    pub(crate) label: String,
    pub(crate) visible: bool,
}

/// `raw`'s nodes whose label contains `query` (case-insensitive), sorted by
/// label, and how many more than [`MAX_MATCHES`] matched beyond the ones
/// returned.
pub(crate) fn matches<'a>(raw: &'a Graph, query: &str) -> (Vec<&'a Node>, usize) {
    let query = query.to_lowercase();
    let mut found: Vec<&Node> = raw
        .nodes
        .iter()
        .filter(|n| n.label.to_lowercase().contains(&query))
        .collect();
    found.sort_by_key(|n| n.label.to_lowercase());
    let over = found.len().saturating_sub(MAX_MATCHES);
    found.truncate(MAX_MATCHES);
    (found, over)
}

/// The field, and — while it holds focus — the dropdown of `matches`, each row
/// toggling that node's visibility, and a trailing count of any matches the
/// list did not have room for.
pub(crate) fn search_field(
    input: Entity<TextInput>,
    focused: bool,
    matches: Vec<Match>,
    over: usize,
    palette: &Palette,
    cx: &mut Context<GraphView>,
) -> impl IntoElement {
    let popover = focused.then(|| {
        let colors = palette.colors;
        let mut rows: Vec<AnyElement> = Vec::with_capacity(matches.len());
        for m in matches {
            rows.push(row(m, palette, cx).into_any_element());
        }
        deferred(
            div()
                .id("search-matches")
                .occlude()
                .absolute()
                .top(MENU_TOP)
                .left_0()
                .w(FIELD_WIDTH)
                .flex()
                .flex_col()
                .max_h(MENU_MAX_HEIGHT)
                .overflow_y_scroll()
                .bg(palette.container)
                .border_1()
                .border_color(palette.border)
                .rounded_md()
                .children(rows)
                .when(over > 0, |menu| {
                    menu.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_color(colors.text_muted)
                            .child(format!("{over} more…")),
                    )
                }),
        )
    });
    div()
        .relative()
        .w(FIELD_WIDTH)
        .flex_none()
        .child(input)
        .children(popover)
}

/// A matched label, checked when its node is visible; clicking toggles it.
fn row(m: Match, palette: &Palette, cx: &mut Context<GraphView>) -> impl IntoElement {
    let colors = palette.colors;
    let hover = colors.element_hover;
    let id = m.id;
    div()
        .id(("search-match", id.0 as usize))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .cursor_pointer()
        .text_color(colors.text)
        .when(m.visible, |row| row.bg(colors.selection))
        .hover(move |style| style.bg(hover))
        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
            model::toggle_node_visible(view, id, cx);
        }))
        .child(div().size(px(12.)).flex_none().when(m.visible, |mark| {
            mark.child(
                bviz_ui_assets::icon("check")
                    .size(px(12.))
                    .text_color(colors.text),
            )
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(m.label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::Kind;

    fn graph(labels: &[&str]) -> Graph {
        let mut g = Graph::new();
        for (i, label) in labels.iter().enumerate() {
            g.add_node(Id(i as u32), *label, Kind::Target, None);
        }
        g
    }

    #[test]
    fn matching_is_case_insensitive_and_alphabetical() {
        let g = graph(&["//foo:Bar", "//foo:baz", "//foo:qux"]);
        let (found, over) = matches(&g, "BA");
        assert_eq!(
            found.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(),
            ["//foo:Bar", "//foo:baz"]
        );
        assert_eq!(over, 0);
    }

    #[test]
    fn a_query_matching_nothing_returns_no_matches() {
        let g = graph(&["//foo:bar"]);
        let (found, over) = matches(&g, "nope");
        assert!(found.is_empty());
        assert_eq!(over, 0);
    }

    #[test]
    fn matches_beyond_the_cap_are_counted_not_returned() {
        let labels: Vec<String> = (0..MAX_MATCHES + 5).map(|i| format!("n{i:03}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let g = graph(&refs);
        let (found, over) = matches(&g, "n");
        assert_eq!(found.len(), MAX_MATCHES);
        assert_eq!(over, 5);
    }
}
