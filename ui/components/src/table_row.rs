//! A row of a table the reader picks rows of.

use crate::scrollbar::SCROLLBAR_THICKNESS;
use bviz_ui_theme::{ThemeColors, over};
use gpui::{Div, ElementId, Hsla, Rgba, Stateful, div, prelude::*, px};

/// A row of a table on the theme's background, filled as the reader's pick
/// where `picked`, its fill spanning the strip the table's scroll bar is drawn
/// over. It sets the row's fill and its hover style, the one gpui takes on an
/// element only once, so a table marks its rows here and nowhere else.
pub fn table_row(id: impl Into<ElementId>, colors: &ThemeColors, picked: bool) -> Stateful<Div> {
    let hover = if picked {
        picked_hover(colors, colors.background)
    } else {
        colors.element_hover
    };
    div()
        .id(id)
        .flex()
        .flex_row()
        .w_full()
        .pr(px(SCROLLBAR_THICKNESS))
        .when(picked, |row| row.bg(colors.selection))
        .hover(move |style| style.bg(hover))
}

/// The fill under the cursor on a picked row standing on `surface`, composited
/// so the row keeps its pick while the cursor is on it.
pub(crate) fn picked_hover(colors: &ThemeColors, surface: impl Into<Rgba>) -> Hsla {
    Hsla::from(over(colors.element_hover, over(colors.selection, surface)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_theme::{
        MIN_DISTINCT_COLOR, MIN_VISIBLE_STEP, built_in_themes, distance, level_step,
    };

    /// The fill is the whole of what marks a picked row, so bringing the cursor
    /// onto one must neither wipe the mark nor leave the row reading like any
    /// other row under the cursor.
    #[test]
    fn a_picked_row_stays_marked_under_the_cursor() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let canvas = colors.background;
            let hovered_pick = picked_hover(colors, canvas);
            let apart = distance(hovered_pick, over(colors.element_hover, canvas));
            assert!(apart >= MIN_DISTINCT_COLOR, "{}: {apart}", theme.name);
            let step = level_step(hovered_pick, over(colors.selection, canvas));
            assert!(step >= MIN_VISIBLE_STEP, "{}: {step} levels", theme.name);
        }
    }
}
