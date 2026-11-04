use gpui::{App, FocusHandle, Window};

/// Whether focus, as a focus-out listener on `held` hears it leave, went
/// nowhere: it still rests on `held` or within it, as when the window goes
/// inactive or stops drawing `held`, or on nothing. A popover closing then
/// hands focus back itself; focus on any other handle was moved there by a
/// click or a key, and stays.
pub fn focus_went_nowhere(held: &FocusHandle, window: &Window, cx: &App) -> bool {
    window.focused(cx).is_none() || held.contains_focused(window, cx)
}
