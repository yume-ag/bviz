//! Reusable, window-agnostic widgets and gpui interaction helpers.

mod background;
mod button;
mod by_room;
mod canvas;
mod choice_dialog;
pub mod dialog;
mod divider;
mod drag;
mod dropdown;
mod find_keys;
pub mod gesture;
mod hint;
mod host;
mod hover_card;
mod key_scope;
pub mod keymap_probe;
mod keys;
mod keys_help;
mod label_line;
mod layer;
mod legend;
mod list_scroll;
mod menu;
pub mod menu_chrome;
mod multi_select;
mod notice;
mod popup;
mod query_field;
mod scrollbar;
mod scrubber;
mod sort;
mod spinner;
pub mod text_input;
mod toggle_group;
mod toolbar;
mod tooltip;
mod truncating_label;
mod zoom_keys;

pub use background::{Pending, load_in_background};
pub use button::Button;
pub use by_room::ByRoom;
pub use canvas::{Inked, LabelBoxStyle, paint_labeled_box, shape_truncated};
pub use choice_dialog::{ChoiceDialog, ChoiceDialogEvent, ChoiceItem};
pub use divider::{Divider, DividerSide};
pub use drag::{DragEnd, track_drag};
pub use dropdown::{Dropdown, DropdownEvent};
pub use find_keys::find_key_bindings;
pub use hint::Hint;
pub use host::Host;
pub use hover_card::{HoverCard, ShownCard, defer_hover_card, hover_card_canvas};
pub use key_scope::{KeyScope, Nesting, UNSCOPED_LABEL, declare_scopes, declared_scopes};
pub use keys::{
    KeyStyle, action_label, binding_text, binding_within, key_text, key_text_in, key_text_within,
    keystroke_text,
};
pub use keys_help::{
    GESTURE_MAX_CHARS, GestureGroup, KEYS_HELP_NAME, KeyGroup, KeyRow, KeysHelp, KeysHelpEvent,
    LABEL_MAX_CHARS, keys_in_effect,
};
pub use label_line::{HintedLabelLine, LabelLine};
pub use layer::{cached_layer, overlay_canvas};
pub use legend::{
    CloseLegend, Legend, LegendEntry, LegendPopover, LegendState, LegendToggle, PaintSwatch,
    ShowLegend, Swatch, legend_gestures, legend_key_bindings,
};
pub use list_scroll::{ListScroll, list_scrollbar};
pub use menu::{Menu, MenuEvent, MenuItem, SubmenuItem, over_window};
pub use multi_select::{MultiOption, MultiSelect, MultiSelectEvent};
pub use notice::{
    NOTE_TEXT_SIZE, Source, centered_notice, centered_spinner, cut_off_note, note_frame, note_strip,
};
pub use query_field::{QueryField, QueryFieldEvent};
pub use scrollbar::{Axis, DEFAULT_MIN_THUMB, SCROLLBAR_THICKNESS, Scrollbar, ScrollbarColors};
pub use scrubber::{ScrubberColors, ScrubberState, scrubber};
pub use sort::{Sort, sort_gesture, sort_header, sort_hint};
pub use spinner::spinner;
pub use text_input::TextInput;
pub use toggle_group::{Presentation, Switch, ToggleGroup, ToggleSegment};
pub use toolbar::{
    OneRow, OverflowRows, Priority, ToolbarGroup, ToolbarSeparator, compact_band_end,
};
pub use tooltip::{
    CardAnchor, TextTooltip, beside_anchor, defer_tooltip, place_tooltips, room_beside,
    tooltip_extent,
};
pub use truncating_label::TruncatingLabel;
pub use zoom_keys::{ZOOM_KEY_STEP, ZoomIn, ZoomOut, zoom_key_bindings};

use gpui::{App, Pixels, px};

/// Height every control renders at, so controls sharing a row line up.
pub(crate) const CONTROL_HEIGHT: Pixels = px(28.);

/// Room added past a measured text width, so that sub-pixel rounding between
/// measuring a line and laying the glyph run out cannot push it onto a second
/// line.
const TEXT_SLACK: Pixels = px(6.);

/// The width to lay a line of text out at, from the width it measured.
pub(crate) fn laid_out_width(measured: Pixels) -> Pixels {
    px(f32::from(measured).ceil()) + TEXT_SLACK
}

/// Register the key bindings the components rely on.
pub fn register_keys(cx: &mut App) {
    declare_scopes(
        cx,
        [
            text_input::KEY_CONTEXT,
            dialog::KEY_CONTEXT,
            choice_dialog::KEY_CONTEXT,
            multi_select::KEY_CONTEXT,
            dropdown::KEY_CONTEXT,
            keys_help::KEY_CONTEXT,
            legend::KEY_CONTEXT,
        ],
    );
    keys_help::bind_keys(cx);
    dialog::bind_keys(cx);
    choice_dialog::bind_keys(cx);
    text_input::bind_keys(cx);
    multi_select::bind_keys(cx);
    dropdown::bind_keys(cx);
    legend::bind_keys(cx);
}
