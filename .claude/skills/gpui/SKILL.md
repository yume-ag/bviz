---
name: gpui
description: How to write, build, and run gpui (Zed's GPU-accelerated UI framework) code in the bviz repo — opening an application window, defining views and elements/widgets, styling with the div() builder, wiring click/keyboard interaction, and running work off the UI thread (background tasks, bridging tokio async runtimes, and rayon data parallelism). Use this whenever a task involves gpui, a `Render` view, `div()` element styling, or the desktop UI layer — even if "gpui" is not named explicitly. Also use it when wiring tokio, reqwest/gRPC/SDK async, rayon, or any off-UI-thread background/parallel work into a gpui app, since gpui's executor is not tokio and needs an explicit bridge.
---

# Writing gpui apps

gpui is Zed's GPU-accelerated UI framework. bviz draws all of its UI with it. The
authoritative reference is real gpui code at a known release tag — read it rather than
guessing APIs, since the API surface shifts between versions.
The links below pin to **Zed v1.24.3**. If a local Zed checkout is available, prefer reading
it over fetching online; confirm its tag matches the version this repo targets.

- Runnable examples: https://github.com/zed-industries/zed/tree/v1.24.3/crates/gpui/examples
- Framework source: https://github.com/zed-industries/zed/tree/v1.24.3/crates/gpui/src

See `references/examples.md` for which example demonstrates what.

**Only part of the Zed tree may be used as a reference.** bviz targets a permissive
licence. `gpui`, `gpui_platform`, `gpui_linux`, `gpui_apple`, `gpui_macos`, `gpui_web`,
`gpui_wgpu`, `gpui_tokio`, `gpui_macros`, `gpui_util`, `gpui_shared_string`, `util`,
`collections`, `sum_tree`, `refineable`, `scheduler`, `ztracing` and `zlog` are Apache-2.0
and are open to read and adapt. Everything else — `ui`, `ui_input`, `theme`, `settings`, `menu`,
`zed_actions`, `workspace`, `cli`, `editor`, `project_panel`, `title_bar` and the rest —
is GPL-3.0-or-later and must not be opened for design or implementation guidance, because
what is written afterwards from a fresh memory of it is a derived work just the same. Each
crate states its own licence in its `Cargo.toml`, and its `LICENSE-APACHE` or `LICENSE-GPL`
symlink says which. Check before reading, not after. Widgets that only the GPL half
provides — buttons, scroll bars, tabs, docks, tables — get written here from the gpui
primitives and the platform documentation.

## Shared crates in this repo — reach for these first

Common gpui mechanics are factored into shared `//ui` crates. Before re-deriving raw
gpui for one of these, use the wrapper, so every view stays visually coherent and
behaves consistently:

- `bviz_ui_shell::{application, setup, open_window, Chrome}` — window bootstrap + client-side decorations.
- `bviz_ui_workspace::{run_viewer, run_panels, Workspace, Panel, AnyPanel}` — the app
  entry points that boot the shell and open the first window (a standalone viewer's, and
  bviz's, whose documents open into the window once it stands), the window root view,
  and the trait a view implements to be the thing it frames.
- `bviz_ui_components::{Button, TextInput}` — a button and a focusable text field.
- `bviz_ui_components::{QueryField, QueryFieldEvent}` — a text field with a button that
  empties it, which emits one event per edit and another once typing settles.
- `bviz_ui_components::track_drag` — a drag that follows the cursor past the window.
- `bviz_ui_components::{KeyScope, declare_scopes, zoom_key_bindings, find_key_bindings, gesture}`
  — labelled key scopes, the shared key bindings, and the gesture tables pointer handlers
  dispatch through.
- `bviz_ui_components::load_in_background` — run work off the UI thread, then apply it.
- `bviz_ui_assets::{text_metrics, shape_line, icon, UI, MONO, load_fonts}` — fonts,
  text metrics, single-run line shaping, Phosphor icons.
- `bviz_ui_theme::{theme, mix}` — the appearance-aware theme a window paints with, and a
  blend helper.

The sections below cover each in context. `ui/console/src/console/` and
`ui/graph/src/` are the worked examples for everything here; read them rather
than reinventing a pattern they already show.

## Mental model: retained state, immediate element tree

This is the one thing to internalize; everything else follows from it.

- **Views/entities are retained.** A struct implementing `Render` is an *entity* held by
  gpui across frames. It owns your mutable state (selection, scroll, the parsed graph).
  Create one with `cx.new(|cx| MyView { .. })`, which returns an `Entity<MyView>`.
- **The element tree is immediate.** `render()` returns a fresh tree of elements
  (`div()`, text, widgets) describing *this frame*. The tree is rebuilt and dropped each
  frame — never store element references; store state in the view struct or child entities.
- **Rendering is notify-driven, not a vsync loop.** Any `cx.notify()` re-renders the whole
  window from the root view — there is no per-view scoped redraw. A notify marks the view's
  *ancestors* dirty, and gpui rebuilds the element tree from the root, cascading `render` down.
  With no notify there is no frame, so idle views do no work. To make a frequent cheap repaint
  skip expensive content, see *Incremental redraw and view caching* below.

Consequence: mutate `self` in an event handler, call `cx.notify()`, and the UI updates.

## Minimal app: window + widgets + a click counter

```rust
use gpui::{
    div, prelude::*, px, rgb, App, Bounds, Context, IntoElement, Render, Window,
    WindowBounds, WindowOptions, size,
};
use gpui_platform::application;

struct Counter {
    count: usize,
}

impl Render for Counter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()                         // enable flex layout (required before flex_col)
            .flex_col()
            .gap_3()
            .size_full()                    // fill the window; unsized content collapses
            .justify_center()
            .items_center()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xffffff))
            .child(format!("Count: {}", self.count))
            .child(
                div()
                    .id("increment")        // .id() makes the element interactive
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x3b82f6))
                    .cursor_pointer()
                    .on_click(_cx.listener(|this, _event, _window, cx| {
                        this.count += 1;
                        cx.notify();         // re-render after mutating state
                    }))
                    .child("Increment"),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| Counter { count: 0 }),
        )
        .unwrap();
        cx.activate(true);
    });
}
```

Note: the entrypoint (`gpui_platform::application()` vs `gpui::Application::new()`) is
version-dependent — confirm against `examples/hello_world.rs` at the referenced tag.

## Building UI: `div()` and the `Styled` builder

`div()` is the workhorse element. `use gpui::prelude::*` brings in the traits that provide
the fluent methods. The vocabulary is Tailwind-like (defined in `gpui/src/styled.rs`):

- Layout: `.flex()`, `.flex_col()` / `.flex_row()`, `.gap_2()`, `.justify_center()`, `.items_center()`, `.flex_1()`
- Box: `.size_full()`, `.w_full()`, `.h(px(40.))`, `.size(px(80.))`, `.p_4()`, `.px_2()`, `.py_1()`, `.m_2()`
- Paint: `.bg(rgb(0x..))`, `.text_color(..)`, `.rounded_md()`, `.border_1()`, `.border_color(..)`, `.shadow_lg()`
- Text: `.text_xl()`, `.text_sm()`
- Children: `.child(x)` for one, `.children(iter)` for many; `x` can be a string, another element, or any `impl IntoElement`.

## Colors and dark/light

Prefer gpui's built-in palette over hardcoded `rgb(0x…)`, so the UI follows the system
appearance for free. `gpui::colors::Colors` has fields like `background`, `text`, `border`,
`container`, `selected`; `Colors::for_appearance(window)` returns the light or dark set from
`window.appearance()` (on Linux, the xdg-desktop-portal color scheme). Use it in `render`:

```rust
let colors = gpui::colors::Colors::for_appearance(window);
div().bg(colors.background).text_color(colors.text)
```

To follow live OS theme switches, re-render on appearance change (once, at window setup):

```rust
window.observe_window_appearance(|window, _cx| window.refresh()).detach();
```

For many views, set `GlobalColors` from the appearance once and read `cx.default_colors()`
instead of threading `window` everywhere — but note the registered global defaults to light
until you set it. Zed's full `theme` crate (`cx.theme()`, named themes, registry) is much
heavier and not needed here. Raw `rgb(0x…)` / `gpui::white()` are still fine for one-off accents.

Fonts: register font bytes with `cx.text_system().add_fonts(vec![bytes…])` at startup, then
select them with `.font_family("Inter")` (UI) / `.font_family("JetBrains Mono")` (mono), which
cascade to children; glyphs missing from the chosen family fall back to system fonts
automatically. In this repo the font files are downloaded by a Bazel `http_archive`, listed as
`data`, and **embedded into the binary** at build time — the `//ui/assets` library's build
script copies them into `OUT_DIR` so `include_bytes!` can bake them in — so there are no runtime
file dependencies. See `//ui/assets` and the `inter` / `jetbrains_mono` archives in `MODULE.bazel`.
A view shown through an element's `.tooltip(...)` is drawn outside the element tree, so it inherits
no family: its root must set `.font_family(bviz_ui_assets::UI)` itself, or gpui falls back to
`.SystemUIFont` and panics when it cannot resolve it (see `bviz_ui_components::TextTooltip`).
A custom element that measures or paints text should resolve its font, size, and line
height from the *inherited* text style with `bviz_ui_assets::text_metrics(window)` (so it
renders in the family cascaded to it — overriding to `MONO` only where it must, like
terminal cells), and shape one styled run with `bviz_ui_assets::shape_line(window, text,
font, size, color)` instead of hand-building a `TextRun`.

Icons: gpui renders SVGs with `svg().path("…").size(px(16.)).text_color(…)`, loading the path
through the app's `AssetSource` (`application().with_assets(MyAssets)`) and tinting the result
with the text color (so single-color icon sets like Phosphor just work). In this repo the
Phosphor SVGs come from an `http_archive` and are embedded the same way; the `AssetSource`
serves them from the baked-in bytes — see the `//ui/assets` library and the `phosphor` archive.
`Svg::paint` reads that colour from the element's **own** `StyleRefinement`, not from the
inherited text cascade, so an icon under a `text_color`ed ancestor draws nothing until it
carries its own `.text_color(…)`.

Reach for a Phosphor icon (`bviz_ui_assets::icon("name")`) before drawing a shape by hand:
the set is large and consistent, and an icon rotated/animated (e.g. `circle-notch` for a
spinner) beats a bespoke `canvas` path. Prefer one over a Unicode shape glyph too — the
embedded Inter carries `U+25B2/25B6/25BC/25C0` (`▲▶▼◀`) but none of the small
`U+25B4/25B8/25BE/25C2` (`▴▸▾◂`), so those resolve through whatever the host offers, in
another typeface or not at all. Only fall back to hand-drawn `canvas`/`PathBuilder` graphics
when no icon fits.

Only an explicit allowlist of icons is embedded, not the whole Phosphor set (which is
thousands of files). To use a new icon, add it to the allowlist — that is the right move, not
a reason to avoid icons. It lives in **two places** that must stay in sync: the `icons`
filegroup in `3rd-party/phosphor.BUILD` (so the SVG is checked out and provided as `data`), and
the `icon_svg` match in `ui/assets/src/lib.rs` (so it is `include_bytes!`-embedded). An icon
missing from either resolves to no bytes and renders as an empty button. Reference it by bare
name (`Button::new(..).icon("caret-up")`); the helper expands it to `assets/regular/<name>.svg`.

## Window chrome: title, decorations, controls, status bar

Set the OS title and grouping id in `WindowOptions` (the title shows in the taskbar and in
any server-drawn title bar):

```rust
WindowOptions {
    titlebar: Some(TitlebarOptions { title: Some("bviz".into()), ..Default::default() }),
    app_id: Some("bviz".into()),
    window_decorations: Some(WindowDecorations::Server), // prefer the compositor's chrome
    ..Default::default()
}
```

Decorations on Linux are negotiated: some compositors draw server-side decorations (SSD),
others (notably GNOME) only support client-side (CSD), where **the app must draw its own
title bar**. Query `window.window_decorations()` each render and adapt:

- `Decorations::Server` — the compositor drew the title bar; draw only your content.
- `Decorations::Client { tiling }` — draw your own title bar (and ideally a border).

A title bar is just a `div`: drag it with `window.start_window_move()` on mouse-down, and wire
control buttons to `window.minimize_window()`, `window.zoom_window()` (maximize), and
`window.remove_window()` (close). Put `cx.stop_propagation()` on the controls' `on_mouse_down`
so pressing a button doesn't also start a window move. A status bar is likewise just a `div` of
app content — gpui has no dedicated widget for it. the `//ui/shell` `Chrome` wraps app content with it.

Resize, rounded corners, and a drop shadow (the rest of client-side decorations) need a
two-layer structure, which `//ui/shell`'s `Chrome` implements in full:

- An outer **backdrop** `div` reserves room for the shadow with padding and calls
  `window.set_client_inset(shadow)`. On mouse-down near an edge it calls
  `window.start_window_resize(edge)` (compute the edge from the cursor vs. window bounds);
  a behind-the-content `canvas` sets the resize cursor as the mouse nears an edge.
- An inner `div` draws the border, rounded corners, and the `BoxShadow`.
- gpui's content mask is **rectangular**, so rounding a parent does not clip children — give
  the title bar and status bar matching rounded corners rather than relying on `overflow_hidden`.
- Respect `Decorations::Client { tiling }`: skip the border/rounding/shadow on any tiled edge
  (e.g. when a tiling WM snaps the window), and skip them entirely for `Decorations::Server`.

`examples/window_shadow.rs` in the gpui repo is the upstream reference for the same pattern.

For client-side chrome, reflect focus: read `window.is_window_active()` in `render` (gpui
re-renders automatically when activation changes) and dim the title bar / soften the shadow
when it returns false. Defer the title-bar move to the first mouse-move after the press (a flag
set on `on_mouse_down`, acted on in `on_mouse_move`) so a press near a corner that the
compositor takes for an edge resize doesn't also become a window move.

## Interactivity

To handle input, an element needs an `.id(..)` (this makes it a `Stateful` element).
Then attach handlers; wrap them in `cx.listener(..)` to get mutable access to your view:

```rust
.on_click(cx.listener(|this, event: &ClickEvent, window, cx| { /* mutate this; cx.notify() */ }))
```

`cx.listener` adapts `|&mut View, &Event, &mut Window, &mut Context<View>|` into the raw
handler signature gpui expects. Other handlers (same pattern): `.on_mouse_down(button, ..)`,
`.on_key_down(..)`, `.hover(|style| style.bg(..))`, `.on_scroll_wheel(..)`. For focus, add
`.focusable()` / `.track_focus(&handle)`. See `examples/a11y.rs` for a focusable counter.

**`.hover(..)` needs the id too, and fails quietly without it.** Unlike `on_click`, it is
declared on `InteractiveElement`, so it compiles on an id-less element — and paints the
right fill, resolved from the live hitbox every frame. What it cannot do is schedule the
frame: the repaint on a hover flip is notified only through the element state an `.id(..)`
creates, so an id-less hovered element updates when something else happens to repaint it and
the fill then sits wherever that frame found the cursor. A list whose rows look like they
"stick" the highlight on the first row entered is this. The id belongs on the element that
carries `.hover(..)` — a child holding the click needs its own.

**Dragging past the window edge.** An element's own `on_mouse_move` is bounds-gated,
so a drag (a scroll thumb, a text selection, a canvas pan) freezes the moment the
cursor leaves the element. To keep it live, register *window-global* handlers while a
drag is in progress — a compositor holds the pointer's implicit grab on the window a
press landed in until every button is released, so its motion and its release go on
arriving there with the cursor well outside it. A drag position outside the window is
therefore ordinary, and each caller bounds it for itself.
`bviz_ui_components::track_drag` wraps the registration and the rules below; its doc
carries the contract.

Equally, do not take every delivered move for a drag move. Leaving a window synthesizes
an off-screen move to clear hover (`patches/gpui_linux/gpui_linux-clear-hover-on-leave.patch`),
which lands just after a release out there: a handler that reads it as a position seeks
to the corner, and one that re-arms its own drag flag on the way keeps the gesture alive
for good. That move reports no button held, which also ends a drag the compositor takes
the pointer away from — an overview, a popup grab — since no release follows one.
`track_drag` ends on the first `MouseUpEvent` *or* the first `MouseMoveEvent` whose
`dragging()` is false. Check `event.dragging()` the same way in any drag handler you
write by hand.

Anything a drag does that is really a *click* — opening a URL, toggling what was
pressed, clearing a selection a bare click clears — must require `DragEnd::Released`; an abandoned gesture reports only where the cursor happened to be,
so acting on it fires the click from a pointer move with no button down.

## Actions, keybindings, and focus

Keyboard interaction goes through actions, not raw key handling. Declare unit actions with
`actions!(namespace, [Quit, CloseWindow])`, bind keys with `cx.bind_keys([KeyBinding::new(
"secondary-w", CloseWindow, Some("Context"))])` (the `secondary-` modifier is cmd on macOS and
ctrl elsewhere; the optional context scopes the binding to a `key_context`), and handle them
with `.on_action(|_: &CloseWindow, window, cx| …)` on an element in the focus path. Actions
dispatch up from the focused element, so a view needs `.track_focus(&handle)` (with
`focus_handle: FocusHandle` created via `cx.focus_handle()`) to receive them.

Every panel follows the keys and gestures convention in `references/keys-and-gestures.md`,
which keeps the Keys & Gestures sheet complete and key text out of string literals. Read it
before adding an action, a key binding, a key context or a pointer handler.

Focus and tab order: configure a handle with `.tab_index(n).tab_stop(true)`, bind `tab` /
`shift-tab` to actions that call `window.focus_next(cx)` / `window.focus_prev(cx)`, and style
the focused state with `.focus(|s| …)`. App lifecycle: closing a window does not quit — wire
`cx.on_window_closed(|cx, _| if cx.windows().is_empty() { cx.quit() })`.

Text fields aren't a built-in widget: a focusable view implements `EntityInputHandler` (the
platform routes typed text and IME through it) and a custom `Element` shapes the line and
paints the cursor, registering itself via `window.handle_input(&focus_handle,
ElementInputHandler::new(bounds, entity), cx)`. `//ui/components`'s `TextInput` is a minimal
one (and a `Button` lives alongside it); `examples/input.rs` is the full version with clipboard.

## Scrolling and scroll bars

gpui gives you scroll *state* and clipping, not a scroll bar. `ScrollHandle` with
`.overflow_y_scroll()` / `.track_scroll(handle)`, and `uniform_list` /
`UniformListScrollHandle`, move and clip content but draw no bar. gpui ships no scroll-bar
element, so render one with `bviz_ui_components::Scrollbar` — a geometry/hit-test/drag-mapping
helper that owns no scroll state. Make it interactive: a scroll bar drags and its track clicks,
not just a drawn thumb. Drive the drag with `track_drag` (see Interactivity) so it keeps
following the cursor past the window edge and ends wherever the button is let go.

Four rules carry most of the weight. **`references/scroll-model.md` has the reasoning,
the three questions it turns on, and the verification method — read it before
designing or changing a scroll model.**

- **The offset is the model; the bar is a rendering of it.** Build the bar in one accessor
  that the paint *and* the mouse handlers both call. Do not keep an `Option<Scrollbar>` on
  the view and write it back from inside the paint closure: that is one fact in two
  places, hit-tested against a rect from a frame that may since have been replayed rather
  than repainted. `ui/graph/src/scroll.rs` is the model; every trace panel follows it.
- **For a list, derive the bar from the handle itself** — `bounds()`, `max_offset()`,
  `offset()` are all live on `ScrollHandle`/`UniformListScrollHandle`, rewritten every
  prepaint. Asking the handle every frame is what keeps the bar from disagreeing
  with what actually scrolls.
- **Hold your own offset unclamped and bound it where it is used**, in one accessor every
  reader goes through. Clamping into stored state destroys what it truncates, so a resize
  or a reflow silently discards where the user was. (gpui's div clamps in prepaint — fine
  for a list, wrong for a custom-painted panel.)
- **Bound a gesture's target before deciding whether it moved anything.** Comparing an
  unclamped request against the clamped drawn offset reduces the guard to "delta ≠ 0", so
  a wheel notch at the bottom that moves nothing on screen still overwrites the held
  position. `drawn_scroll`/`scroll_request` in `ui/trace/src/lib.rs` are the shared rule.

If the content itself reflows — rows collapsing, being filtered or reordered — a pixel
offset cannot survive it; anchor to a row identity instead. That is the second half of
`references/scroll-model.md`.

For a line-based terminal, bridge the drag fraction to alacritty's `display_offset`
(`ui/console/src/console/mod.rs`), which also implements auto-hide: reveal on scroll /
drag / gutter-hover, hide after an idle delay with `cx.spawn` +
`cx.background_executor().timer(..)`, guarding the hide with a reveal counter so a later
reveal isn't undone by an earlier timer firing.

**Wheel modifiers need window focus, and on Wayland an unfocused window cannot get them** —
so `Ctrl/Shift+wheel` silently does nothing until the window is clicked, and no client-side
workaround exists (activating the window on hover is dead code with a false premise; do not
add it). Plain-wheel scroll is unaffected. For an affordance that works while unfocused, use
pure-pointer controls. `references/wayland-input.md` has the protocol-level reasoning and the
evidence, and is worth reading before designing any modifier-gated gesture.

## Aligned tables

Build the header and every body cell through **one shared cell builder** sharing a single
column spec, so their widths can never disagree. A fixed column needs
`w(px).flex_shrink_0().overflow_hidden().whitespace_nowrap()`: a bare `w(px)` is only a flex
*basis*, and since `flex_shrink` defaults to 1.0 it gets shrunk — by different amounts in
the header versus each body row when a row overflows — so columns "float left" and drift out
of alignment at some widths. The one flexible column is `flex_1().min_w_0()` and takes
`text_ellipsis()` to cut a long value. Do NOT put `text_ellipsis()` on a fixed, right-aligned
(`justify_end`) numeric column: the ellipsis measurement misjudges a justify-end cell and
truncates values that fit (you'll see `467…` next to a wider value rendered in full). Size
such a column to its longest value and let it clip only on true overflow. Virtualize many fixed-height rows with `uniform_list(id, count, |range,
window, cx| -> Vec<_>)` + a `UniformListScrollHandle` (renders only visible rows; requires
uniform row height), keeping the header a separate non-scrolling row above it; drive a scroll
bar from the handle's `base_handle` offset/extents as above. The stats table in
`ui/trace/src/stats_panel.rs` (`table_cell`, `stats_table`) is the worked example here.

## Stateful views vs stateless components

- **`impl Render for Foo`** — a stateful view (`fn render(&mut self, ..)`), held as an
  `Entity<Foo>`. Use for anything with its own state.
- **`#[derive(IntoElement)]` + `impl RenderOnce`** — a stateless, reusable component
  (`fn render(self, window, cx) -> impl IntoElement`, consumes `self`). Use for
  parameterized, stateless widgets — a button, a label, a badge.

For the first widgets, plain `div()` trees inside a `Render` view are enough; reach for
`RenderOnce` only once you're repeating a widget shape.

## Child entities

Compose larger UIs from child entities rather than one giant view:

- Create: `let child = cx.new(|cx| Child::new(cx));` (store the `Entity<Child>` in a field).
- Render it: add the entity (cloned) as a `.child(self.child.clone())`.
- Update from outside: `self.child.update(cx, |child, cx| { child.foo = 1; cx.notify(); })`.
- React to a child: `cx.observe(&child, ..)` fires on **every** `cx.notify()` the child
  makes — every repaint reason (open/close, hover, focus), not just the state change you
  care about — and the subscription must be stored in a field (it's dropped otherwise). To
  react to ONE specific change, don't observe: have the child `impl EventEmitter<MyEvent>`,
  `cx.emit(MyEvent::..)` only when that change happens, and `cx.subscribe(&child, ..)`. (A
  dropdown that reset a sibling control on every hover was the bug from observing instead of
  subscribing — see `bviz_ui_components::Dropdown`/`DropdownEvent`.)

## Custom rendering: `canvas` and `Element`

`div()` trees cover most UI, but per-glyph or per-shape rendering — a terminal grid, a
node graph — needs to paint directly. There are two levels; reach for the lighter one
first.

**A `canvas`** hands you the laid-out `bounds` and lets you paint into them:
`canvas(prepaint_fn, paint_fn)`. The prepaint closure captures `bounds` (e.g. to fit
content to the viewport on the first frame); the paint closure draws with
`window.paint_quad(quad(..))`, `window.paint_path(..)`, and a shaped line's
`.paint(origin, ..)`. Both closures are `'static`, so capture **owned snapshots** of what
you paint — a `Vec` of boxes, a `Palette` of theme-resolved colors — not borrows of the
view. The graph viewer is the worked example: `ui/graph/src/paint/` (`Scene`,
`canvas_body`, `paint_box`, `paint_connector`).

**A custom `Element`** is heavier, for when you also need a typed layout → prepaint →
paint pipeline. Implement `Element` (plus a trivial `IntoElement`) with two associated
state types and three methods:

- `request_layout(.., window, cx) -> (LayoutId, RequestLayoutState)` — resolve metrics
  (see `text_metrics` above), return a `Style` via `window.request_layout(style, [], cx)`,
  and stash the measured metrics in the layout state.
- `prepaint(.., bounds, &mut RequestLayoutState, ..) -> PrepaintState` — compute what to
  paint (shaped lines, background quads) and collect it into the prepaint state.
- `paint(.., bounds, &mut RequestLayoutState, &mut PrepaintState, ..)` — drain those
  quads and lines onto the window.

`id()` / `source_location()` return `None`. The terminal (`ui/console/src/console/element.rs`:
`TerminalElement`) is the worked example; the text field's `TextElement` is another.

**Hit-testing.** A `canvas`/`Element` paints one surface, so gpui's per-`div`
`on_click`/`hover` don't reach what's inside it — hit-test by hand. Store the painted
geometry on the view each layout (the terminal keeps a `ContentLayout`;
the graph keeps the viewport `bounds`), then in the mouse handler invert the same transform
the paint used: the terminal's `cell_at` maps a pixel to a grid cell, the graph's
`from_screen` + `hit_box` map a pixel to the innermost box.

**Geometry measured by a paint is for hit-testing, never for layout.** Stashing the
painted bounds on the view is right for a mouse handler: it runs between frames, and the
geometry the user clicked is exactly the geometry last painted. Styling a *sibling*
element from that same stash is not. Sizes are resolved during layout, which runs before
any paint, so an element positioned from a measured field is a frame behind whatever it
overlays — and on the first frame there is no measurement at all, so it renders at a
degenerate position (everything stacked at the origin). Whether a corrective frame ever
arrives is incidental: it takes some unrelated `cx.notify()`, which makes the opening
frame right or wrong depending on timing rather than on the code.

When a child's placement depends on the parent's own size, put the placement in a custom
`Element`. Its `prepaint` receives `bounds`, which is where the geometry is first known
and still early enough to lay children out:

```rust
fn prepaint(&mut self, .., bounds: Bounds<Pixels>, .., window, cx) {
    for (child, place) in &mut self.children {
        let rect = place(bounds);
        let space = size(
            AvailableSpace::Definite(rect.size.width),
            AvailableSpace::Definite(rect.size.height),
        );
        child.prepaint_as_root(rect.origin, space, window, cx);   // AnyElement
    }
}
fn paint(&mut self, ..) {
    for (child, _) in &mut self.children { child.paint(window, cx); }
}
```

The children then carry no position of their own — `size_full()` inside the rect they are
given. `request_layout` returns a `Style` with `position: Absolute` and `inset` 0 so the
container overlays its siblings rather than displacing them. Placement order is paint
order is hit-test order, so a child that must win a press over another goes last.
`RegionOverlays` in `ui/trace/src/lib.rs` places a tab bar, a bottom panel, and a drag
handle this way.

## View state: plain fields, not `Cell`/`RefCell`/`Rc`

**Default to plain fields.** `Render::render(&mut self, ..)`, every `cx.listener` handler,
and every `Entity::update` closure hand you `&mut View`, so almost nothing a view owns
needs interior mutability.

**The one thing that pushes back** is the geometry a paint discovers: a `canvas`
prepaint/paint closure is `'static` and reaches the view through `parent.read(cx)` — a
*shared* reference — so writing the captured bounds, the fitted zoom, or a scroll-bar
layout through it seems to demand a `Cell`. It doesn't: those closures also receive
`&mut App`, so they can write through `parent.update(cx, |view, _| ..)` instead. Shape it
as read-then-commit — pull what you need out of `parent.read(cx)`, let that borrow end,
then commit in one `update`:

```rust
let old_bounds = parent.read(cx).viewport;   // shared read; the borrow ends here
let zoom = fit_zoom(old_bounds, bounds);     // whatever the geometry needs
parent.update(cx, |view, _| {                // exclusive write
    view.viewport = bounds;
    view.zoom = zoom;
});
```

A helper that a paint uses to stash geometry should **return** it rather than take a
`&Cell<T>` to fill.

**First ask whether it needs stashing at all.** Geometry a paint can *derive* — a scroll
bar's track and thumb, which follow from the offset and the layout — belongs in an
accessor both the paint and the handlers call, not in a field the paint writes back. What
genuinely has to be captured is what only the layout knows: the bounds a `canvas` was
given. Capture that in the **prepaint**, where it is known before anything reads it, and
prefer `Option<Bounds<Pixels>>` over a zero default so "not laid out yet" is
distinguishable from "laid out at the origin" instead of quietly producing a wrong
ceiling.

**This is safe and does not loop.** By the time elements lay out, prepaint, and paint, the
view's own `render` lease has ended, so `update` there does not double-lease — and a
descendant's prepaint never runs with an ancestor leased, which is what makes reading a
*sibling* entity (the model) from inside a `parent.update(..)` sound. Note that `read` is
not exempt: `EntityMap` panics on the same condition as `update`, so a shared read of an
already-leased entity is just as fatal as a second `update`. And `update`
alone does not notify, so it schedules no extra frame — confirm on a real profile by
counting paint calls (an `eprintln!` in the paint closure should fire once for a settled
idle window, not continuously).

**An `Rc` around a cell is almost always vestigial.** It buys something only if the handle
itself is cloned into a `'static` closure — a `cx.spawn` background block that writes the
result back. If nothing clones it, drop the `Rc` and keep only the cell. And if the value
is written *only* from a background task's completion, that runs inside `Entity::update`
with `&mut View`, so it needs neither the `Rc` nor the cell.

**What the cell costs.** Dynamic borrow checking on a field nothing shares, a `RefCell`
that can panic at runtime, and read-modify-write (`let mut v = f.get(); …; f.set(v)`) where
in-place mutation would do. `Timeline` in `ui/trace/src/timeline/` holds its time
view, viewport, fit latch, scroll anchor, and memoized cull list as plain fields, and
`fit_prepaint` / `slice_paint` are the worked examples of the read-then-commit shape.

Reach for a cell only for a writer with no path to `&mut App` or `&mut self` at all.

## Incremental redraw and view caching

- **A notify re-renders everything.** `cx.notify()` on any entity re-runs the root view's
  `render` and cascades down the tree — there is no scoped "just this view" redraw. So on its
  own, moving expensive paint into a separate view or `canvas` does not save that paint: a
  `canvas` has no id and no paint cache, and its `'static` paint closure runs every time the
  element is painted.
- **`AnyView::cached(style)` is the one paint-reuse lever.** Embed an expensive child
  `Entity<V: Render>` as `.child(self.layer.clone())` styled with `.cached(style)`. gpui then
  *replays* that child's recorded scene from the previous frame — skipping the child's `render`
  and paint — as long as the child was not itself notified this frame, its bounds, content mask,
  and text style are all unchanged, and the window is not refreshing. A resize changes bounds, so
  the cache key breaks and the child repaints on its own; `Window::refresh()` (e.g. an appearance
  change) bypasses caching entirely.
- **A cached child's prepaint and paint are reused together** — gpui never replays one
  without the other, and a `canvas`'s prepaint and paint are handed the *same* `bounds`. So
  geometry a prepaint captures always describes the frame that was painted, and a hit-test
  reading it cannot be half a frame stale. What it *can* be is a whole frame old, which is
  why derived geometry (a scroll bar) belongs in an accessor rather than in a field the
  paint wrote.
- **Parent-notify vs child-notify is the control.** Because a notify marks *ancestors* dirty and
  never descendants, a parent notify does not dirty an embedded child — its cached scene replays.
  This is the lever for cheap frequent repaints over expensive content: put the expensive,
  data-dependent content in a child layer embedded `.cached(style)`, and put the cheap overlay (a
  hover crosshair, a tooltip, an in-progress drag) in the parent above it. Notify the **parent**
  for a cheap overlay change — the child scene replays and only the overlay repaints; notify the
  **child** for any change to what it paints — it rebuilds its scene.
- **A refresh suspends caching for the whole window, and gpui's own drag calls one per mouse
  move.** The refreshing flag is a conjunct of the replay condition itself, so while it is set no
  cached view in the window replays, and a cached view that misses sets it again for its own
  subtree — nesting an expensive layer deeper shields nothing. A gesture driven through
  `cx.active_drag` (the `on_drag` / `on_drop` API) refreshes on every mouse move, which makes
  every frame it lasts a full uncached redraw of every panel. Drive a gesture over expensive
  content with `track_drag` and a parent-only notify instead, and the cached scenes go on
  replaying; only the press and release frames are full ones, which `div` refreshes anyway.
- **The stale-cache gate.** The one real bug class: a state change that alters what the cached
  child paints but leaves the child un-notified shows stale content until the next child notify —
  and a static screenshot won't reveal it. Route every content-mutating handler through one helper
  that notifies the child (and the parent), and keep only genuinely overlay-only handlers as
  parent-only notify.
- **`deferred(child)` is paint *ordering* only** — it draws its subtree on top, for overlays and
  popups. It has nothing to do with caching.
- **Worked examples in this repo.** `ui/trace/src/timeline/paint.rs` `SliceCanvas` (timeline slices
  cached; crosshair/tooltip live in the parent's `overlay_paint`) and
  `ui/trace/src/flame_panel.rs` `FlameCanvas` (flame boxes cached; tooltip overlay). Each panel
  routes every content change through its own `invalidate_canvas`, which notifies the cached
  child as well as the parent. A complementary `SliceDrawCache` (in `timeline/slices.rs`) memoizes the
  *cull* list — which slices are visible — keyed by viewport; it avoids re-culling but not
  re-painting, so it is a separate, view-held cache from the cached child view that avoids
  re-painting.

## Background work: loading off the UI thread

Heavy work — reading and parsing a file, laying out a graph, reconstructing a terminal —
must not run on the UI thread, or the window won't appear and the first frame freezes.
Run it on the background executor and apply the result back on the UI thread.
`bviz_ui_components::load_in_background(cx, work, apply)` wraps the idiom: `work: FnOnce()
-> T + Send` runs in the background; `apply(&mut View, T, &mut Context)` runs on the UI
thread, and the helper calls `cx.notify()` for you. It returns a `Task<()>` — **store it
in a view field**, or dropping it cancels the work.

Pair it with a load-state enum (`Loading` / `Loaded(T)` / `Error(String)`) the view matches
on in `render`, showing a spinner while loading (`bviz_ui_assets::icon("circle-notch")`
with `.with_animation(..)`). `GraphPanel` and `ConsolePanel` both follow this shape. For
a one-shot *delay* rather than background work (e.g. an auto-hiding scroll bar), use
`cx.spawn` + `cx.background_executor().timer(..)` directly instead.

## Foreign async (tokio) and CPU parallelism (rayon)

gpui runs its own executor (a UI-thread foreground queue plus a background thread pool).
It is **neither a tokio runtime nor rayon**, so two kinds of foreign work need an explicit
bridge. `references/async-tokio-rayon.md` has the full patterns, the bridge source to
adapt, and the rules for gating parallel work and keeping the pools apart; the essentials:

- **tokio** — futures from libraries that need tokio's reactor (`reqwest`, `tonic`, cloud
  SDKs, `tokio::net`/`time`) panic *"there is no reactor running"* on gpui's executor. Own
  one tokio runtime as a gpui `Global` and turn each tokio `JoinHandle` into a gpui `Task`;
  dropping the `Task` aborts the tokio work. This is Zed's `crates/gpui_tokio` (v1.24.3, ~100
  lines) — there is no such crate here yet, so adapt it into `//ui`. For a synchronous
  constructor that calls `Handle::current()`, hold a `Tokio::handle(cx).enter()` guard.
- **rayon** — a parallel iterator (`par_iter`, `par_sort`) blocks its calling thread until
  the whole job finishes, so it must **only** run inside a background task (e.g. the `work`
  closure of `load_in_background`, or `cx.background_executor().spawn(..)`), never on the UI
  thread. Gate the parallel path by input size, and run it inside
  `bviz_parallel::workers().install(..)`, the process's one pool. For a few coarse chunks,
  `cx.background_executor().scoped(..)` (or `bviz_parallel::scope`) avoids pulling in rayon
  at all.

## Building and running in this repo

Build and run through Bazel (`rust_binary` via `rules_rs`), never cargo — see `CLAUDE.md`.
Crate deps come from the Cargo workspace and are exposed to Bazel as `@crates//:<name>`
through the `crate.from_cargo` extension in `MODULE.bazel`.

To add a dependency: add it under `[workspace.dependencies]` in the root `Cargo.toml` and
the consuming crate's `Cargo.toml`, repin with the Bazel-provided cargo (never system
cargo), commit `Cargo.lock`, then list it in the target's `deps` (e.g. `@crates//:gpui`).
Run with `bazel run //bviz`.

## Screenshots (visual verification)

gpui windows are Wayland surfaces. Under GNOME, both the `org.gnome.Shell.Screenshot`
D-Bus API and the XDG screenshot portal refuse non-interactive callers, so they cannot
capture a window unattended. Instead use `screenshot.sh` (next to this file), which runs
the app inside a headless wlroots compositor (`cage`) and captures its offscreen output
with `grim`. The headless backend keeps it working while the GNOME screen is locked or
idle (a nested window would not be presented then). Both screenshot scripts need
[Nix](https://nixos.org/download/), from which they fetch their tools at the nixpkgs
revision `nixpkgs.sh` pins:

```
.claude/skills/gpui/screenshot.sh <out.png> <app-binary> [args...]
```

**Build and resolve the binary with `-c opt`, always:**

```
bazel build -c opt //path:target
bazel cquery -c opt --output=files //path:target
```

A `fastbuild` binary parses and lays out real input roughly an order of magnitude
slower, so on anything beyond a toy graph the capture lands on the loading spinner
instead of the view. That failure is worse than it sounds, because it does not look
like a failure: the spinner is drawn at whatever angle it had reached, so two captures
of the *same* binary differ, and an A/B comparison reports a change that is only the
animation. `-c opt` costs one slower build and removes the whole class.

`SCREENSHOT_DELAY` (default 5s) sets how long to wait before capturing, and must cover
load *and* layout, not just the first frame — an app that reads a file off the disk and
lays a graph out reaches its first frame almost at once and the view many seconds later.
Size it from the app's own measured load time with room to spare, and confirm the
capture shows the view rather than a spinner before drawing any conclusion from it.

**Driving input.** `cage` delivers no injected input. `screenshot-input.sh` runs the app in
headless `sway` instead, with a virtual pointer and `wtype`'s virtual keyboard, and sources a
steps file that moves, clicks, scrolls, types and captures (its header lists the commands):

```
.claude/skills/gpui/screenshot-input.sh <steps-file> <app-binary> [args...]
```

It holds one `wtype` keyboard on the seat for the whole run, which is what lets the keys of
each later `wtype` call reach the app. Click into the window before a modified key or wheel,
so the window holds keyboard focus (see the wheel-modifier note under Scrolling).

**A capture proves the frame, not the behaviour.** Bit-identical PNGs across a refactor are
strong evidence that nothing moved — build the binary before and after, capture both, and
compare with `sha256sum`; that is the cheapest proof a "no visible change" claim can have.
But they say nothing about state that only a gesture reaches. For a behavioural claim (a
position surviving a reflow, a selection surviving a rebuild) drive the same sequence on both
revisions, through `screenshot-input.sh` or the *same* throwaway instrumentation, and take
**three** captures — control, old-revision cycle, new-revision cycle —
so the result distinguishes "fixed" from merely "different". `references/scroll-model.md`
has the full method, including which region to diff and how to pick a perturbation that
actually moves the bound.

## Gotchas

- No `.id()` → no interactivity. `on_click`/focus/state need a `Stateful` element.
- `.hover(..)` compiles without an `.id()` and paints correctly, but never schedules its own repaint — the fill sticks until an unrelated notify.
- Forgetting `cx.notify()` after a state change → the UI silently won't update.
- `.flex_col()`/`.flex_row()` do nothing without `.flex()` first.
- Unsized content collapses; set an explicit size or `size_full()`/`flex_1()` in a flex parent.
- The element tree is rebuilt every render — hold `Entity<_>` references, never element ones.
- A `canvas`/`Element` paint closure is `'static` — capture owned snapshots, not view borrows.
- A `Cell`/`RefCell` on a view field is usually avoidable — a prepaint/paint closure holds `&mut App`, so it can `parent.update(..)` and write a plain field.
- `read`ing an entity that is already leased panics exactly like a second `update` — a shared borrow is not a way around a double-lease.
- A view field holding geometry a paint could derive (a scroll bar) is one fact in two places — derive it in an accessor the paint and the handlers share.
- Clamping a scroll offset into stored state destroys the position it truncates — hold it unclamped and bound it at the read, or a resize silently loses where the user was.
- Styling an element from bounds a paint measured puts it a frame behind, and at the origin on the first frame — place children from a custom `Element`'s prepaint instead.
- A background `Task` you don't store is cancelled on drop — keep it in a view field.
- An element's mouse handlers stop at its own bounds — use `track_drag` for a drag that must follow the cursor past them, and past the window's edge.
- A drag through `on_drag`/`on_drop` refreshes the window on every mouse move, so nothing `.cached(..)` replays for as long as it lasts.
- A drag ended only on `MouseUpEvent` sticks when the compositor takes the pointer away mid-gesture — also end it on a move whose `dragging()` is false.
- gpui's executor is not tokio — a `reqwest`/`tonic`/SDK future run directly on it panics "no reactor running"; bridge it through a tokio runtime (see the tokio/rayon section).
- A rayon `par_iter`/`par_sort` on the UI thread freezes the frame — it blocks its caller; run it inside a background task, inside `bviz_parallel::workers().install(..)`.
