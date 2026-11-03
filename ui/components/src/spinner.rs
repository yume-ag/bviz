use gpui::{
    AbsoluteLength, Animation, AnimationExt, ElementId, Hsla, IntoElement, Length, Transformation,
    percentage, prelude::*,
};
use std::time::Duration;

/// One full turn per this period.
const SPIN_PERIOD: Duration = Duration::from_millis(900);

/// Frames a turning spinner asks for per second. Each one redraws every view
/// the window does not cache, so a spinner at the display's rate keeps the UI
/// thread busy for as long as the work it marks lasts.
const SPIN_FPS: f32 = 15.0;

/// A circle-notch icon turning continuously — the busy cue for work in flight.
/// `id` must be distinct per instance so each spinner keeps its own animation state.
pub fn spinner(
    id: impl Into<ElementId>,
    size: impl Into<AbsoluteLength>,
    color: impl Into<Hsla>,
) -> impl IntoElement {
    bviz_ui_assets::icon("circle-notch")
        .size(Length::from(size.into()))
        .text_color(color)
        .with_animation(
            id,
            Animation::new(SPIN_PERIOD).repeat().with_max_fps(SPIN_FPS),
            |icon, delta| icon.with_transformation(Transformation::rotate(percentage(delta))),
        )
}
