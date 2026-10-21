//! The application shell: window bootstrap, client-side decorations, lifecycle,
//! and the global interactivity (quit/close/tab) shared by every app window.

mod chrome;
pub use chrome::{Chrome, title_bar_gestures};

use bviz_ui_components::{KeyScope, declare_scopes};
use gpui::{
    App, Application, Bounds, Entity, KeyBinding, Pixels, Render, Result, Size, TitlebarOptions,
    Window, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, actions, px,
};

actions!(
    bviz_window,
    [
        /// Quit application
        Quit,
        /// Close window
        CloseWindow,
        /// Focus next control
        Tab,
        /// Focus previous control
        TabPrev,
    ]
);

/// The key context of the window frame [`Chrome`] draws around a window's
/// content. gpui prefers the binding whose context matches deepest, and a
/// binding scoped to none matches at the focused element itself, so tab is
/// scoped here for any context inside the frame to take it.
pub const KEY_CONTEXT: KeyScope = KeyScope::frame("Window");

/// The smallest the window may be resized to, so its chrome and content can't be
/// crushed to nothing. Smaller than every app's opening size.
const MIN_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(480.0),
    height: px(360.0),
};

/// The application builder with the bundled asset source installed. Exits with
/// an explanation when there is no display the platform can open windows on.
pub fn application() -> Application {
    #[cfg(target_os = "linux")]
    exit_without_wayland();
    #[cfg(not(target_family = "wasm"))]
    let application = gpui_platform::application();
    #[cfg(target_family = "wasm")]
    let application = web_application();
    application.with_assets(bviz_ui_assets::Assets)
}

/// gpui's web platform as `gpui_platform::application` builds it, logging and
/// reporting panics to the browser console, and leaving the end of the
/// application to [`last_window_closed`].
#[cfg(target_family = "wasm")]
fn web_application() -> Application {
    gpui_web::init_logging();
    let allow_multi_threading = true;
    let platform = std::rc::Rc::new(gpui_web::WebPlatform::new(allow_multi_threading));
    let http_client = std::sync::Arc::new(platform.fetch_http_client());
    Application::with_platform(platform)
        .with_http_client(http_client)
        .with_quit_mode(gpui::QuitMode::Explicit)
}

/// gpui's Linux platform is built for Wayland alone, and without a Wayland
/// display it falls back to a headless platform whose error names the wrong
/// cause on an X11 session.
#[cfg(target_os = "linux")]
fn exit_without_wayland() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty()) {
        return;
    }
    eprintln!(
        "error: bviz needs a Wayland session, and WAYLAND_DISPLAY is not set.\n\
         On X11, run it inside a nested Wayland compositor, e.g. `cage -- bviz ...`."
    );
    std::process::exit(1);
}

/// Register fonts, the themes, global key bindings, and the end of the
/// application with its last window. Call once at startup before opening
/// windows.
pub fn setup(cx: &mut App) {
    bviz_ui_assets::load_fonts(cx);
    bviz_ui_theme::init(cx).expect("the built-in themes load");
    declare_scopes(cx, [KEY_CONTEXT]);
    cx.bind_keys(bindings());
    bviz_ui_components::register_keys(cx);
    bviz_ui_components::place_tooltips(cx);
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            last_window_closed(cx);
        }
    })
    .detach();
}

#[cfg(not(target_family = "wasm"))]
fn last_window_closed(cx: &mut App) {
    cx.quit();
}

/// A page cannot end the script running in it, so it is told the window is
/// gone instead.
#[cfg(target_family = "wasm")]
fn last_window_closed(_: &mut App) {
    bviz_ui_web::closed();
}

pub fn bindings() -> Vec<KeyBinding> {
    let window = Some(KEY_CONTEXT.id());
    vec![
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-shift-w", CloseWindow, None),
        KeyBinding::new("tab", Tab, window),
        KeyBinding::new("shift-tab", TabPrev, window),
    ]
}

/// Open a decorated floating window of `window_size` whose root view comes from
/// `build_root`. Wrap the view's body in [`Chrome`] to get the decorations.
/// Callable any number of times once [`setup`] has run.
pub fn open_window<V, F>(
    app_id: &str,
    window_size: Size<Pixels>,
    cx: &mut App,
    build_root: F,
) -> Result<WindowHandle<V>>
where
    V: 'static + Render,
    F: FnOnce(&mut Window, &mut App) -> Entity<V>,
{
    // Centered on the active display, at an absolute size rather than a fraction
    // of the resolution, so it works even on Wayland, where the display list is
    // still empty at startup.
    let bounds = Bounds::centered(None, window_size, cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(app_id.to_string().into()),
                ..Default::default()
            }),
            app_id: Some(app_id.to_string()),
            window_min_size: Some(MIN_WINDOW_SIZE),
            // Prefer the compositor's decorations; Chrome draws our own when
            // only client-side is supported (e.g. GNOME).
            window_decorations: Some(WindowDecorations::Server),
            ..Default::default()
        },
        |window, cx| {
            window
                .observe_window_appearance(|window, _cx| window.refresh())
                .detach();
            build_root(window, cx)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_components::keymap_probe::{self, is};
    use gpui::Action;

    actions!(bviz_window_test, [Inner]);

    /// The action `keys` dispatch with focus in the innermost of `contexts`,
    /// under the window's bindings and a binding of tab in `Inner`.
    fn dispatched(keys: &str, contexts: &[&str]) -> Option<Box<dyn Action>> {
        let mut bindings = bindings();
        bindings.push(KeyBinding::new("tab", Inner, Some("Inner")));
        keymap_probe::dispatched(bindings, keys, contexts)
    }

    /// Tab cycles the focus wherever nothing inside the window binds it, and
    /// yields to a context that does, however deep the focus sits below it.
    #[test]
    fn a_context_inside_the_window_takes_tab_from_focus_cycling() {
        assert!(is(dispatched("tab", &[KEY_CONTEXT.id()]), Tab));
        assert!(is(
            dispatched("shift-tab", &[KEY_CONTEXT.id(), "Field"]),
            TabPrev
        ));
        assert!(is(
            dispatched("tab", &[KEY_CONTEXT.id(), "Panel", "Field"]),
            Tab
        ));
        assert!(is(
            dispatched("tab", &[KEY_CONTEXT.id(), "Panel", "Inner", "Field"]),
            Inner
        ));
        assert!(is(dispatched("tab", &[KEY_CONTEXT.id(), "Inner"]), Inner));
    }
}
