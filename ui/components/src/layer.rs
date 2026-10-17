//! Layering an embedded view under a canvas drawn over it.

use gpui::{
    AnyView, App, Bounds, Canvas, IntoElement, Pixels, StyleRefinement, Styled, Window, canvas,
};

/// Embeds `view` filling its parent as a layer whose recorded scene replays
/// until `view` itself is notified: a `cx.notify()` marks the notified view and
/// its ancestors dirty, never its descendants.
pub fn cached_layer(view: impl Into<AnyView>) -> impl IntoElement {
    view.into()
        .cached(StyleRefinement::default().absolute().inset_0().size_full())
}

/// A canvas filling its parent from the parent's origin, so it paints over any
/// sibling declared before it. `paint` is handed the canvas's own bounds. It
/// takes no press, so what it covers goes on hit-testing as it would without it.
pub fn overlay_canvas(
    paint: impl 'static + FnOnce(Bounds<Pixels>, &mut Window, &mut App),
) -> Canvas<()> {
    let paint = move |bounds, (), window: &mut Window, cx: &mut App| paint(bounds, window, cx);
    canvas(|_, _, _| {}, paint)
        .absolute()
        .left_0()
        .top_0()
        .size_full()
}
