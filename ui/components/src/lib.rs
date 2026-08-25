//! Reusable, window-agnostic widgets and gpui interaction helpers.

mod background;
mod button;
mod canvas;
mod drag;
mod dropdown;
mod layer;
mod multi_select;
mod notice;
mod scrollbar;
mod scrubber;
mod spinner;
pub mod text_input;
mod toolbar;
mod tooltip;

pub use background::load_in_background;
pub use button::Button;
pub use canvas::{LabelBoxStyle, paint_labeled_box, shape_truncated};
pub use drag::{DragEnd, track_drag};
pub use dropdown::{Dropdown, DropdownEvent};
pub use layer::{cached_layer, overlay_canvas};
pub use multi_select::{MultiSelect, MultiSelectEvent};
pub use notice::{Source, centered_notice, centered_spinner};
pub use scrollbar::{Axis, DEFAULT_MIN_THUMB, SCROLLBAR_THICKNESS, Scrollbar, ScrollbarColors};
pub use scrubber::{ScrubberColors, ScrubberState, scrubber};
pub use spinner::spinner;
pub use text_input::TextInput;
pub use toolbar::Toolbar;
pub use tooltip::{TooltipColors, paint_tooltip_box};

use gpui::{App, Pixels, px};

/// Height every control renders at, so controls sharing a row line up.
pub(crate) const CONTROL_HEIGHT: Pixels = px(28.);

/// Register the key bindings the components rely on (currently the text field's).
pub fn register_keys(cx: &mut App) {
    text_input::bind_keys(cx);
}
