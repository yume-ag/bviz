# gpui examples index

Runnable examples live in the Zed repo under `crates/gpui/examples/`, browsable at
https://github.com/zed-industries/zed/tree/v1.24.3/crates/gpui/examples (prefer a local Zed
checkout if available). They are the most reliable, version-accurate API reference. Read
the relevant one before writing similar code. Grouped by what they teach.

## Starting out
- `hello_world.rs` — minimal window, a `Render` view, nested `div()` widgets, the
  `application().run` / `open_window` / `cx.new` bootstrap. **Start here.**
- `window.rs` — window options, multiple/sizing, titlebar.
- `text.rs`, `text_layout.rs`, `text_wrapper.rs` — rendering and laying out text.

## Interaction
- `a11y.rs` — a focusable click counter: `.id()`, `.focusable()`, `cx.listener`, `cx.notify()`.
- `tab_stop.rs` — focus order / keyboard navigation.
- `drag_drop.rs` — drag-and-drop handlers.
- `input.rs` — a full text input (more involved; consult when building editable fields).

## Large data / virtualization (relevant for big Bazel graphs)
- `uniform_list.rs` — fixed-height row virtualization; renders only visible rows.
- `list_example.rs` — variable-height list backed by a sum-tree.
- `scrollable.rs` — scroll containers and scroll handles.
- `data_table.rs` — tabular layout (useful for target/action metadata panels).

## Custom drawing (relevant for the graph canvas)
- `painting.rs` — the `canvas()` element and the low-level paint API.
- `paths_bench.rs` — building and filling Bézier `Path`s (graph edges/curves); also a
  performance reference for many paths.
- `gradient.rs` — gradient fills. `svg/` — SVG rendering.

## Layout & composition
- `grid_layout.rs` — grid layouts. `anchor.rs` / `popover.rs` — overlays/anchored content.
- `animation.rs` — animations (uses `request_animation_frame`; re-renders until settled).
- `image*.rs`, `gif_viewer.rs` — images. `opacity.rs`, `shadow.rs` — visual effects.

## Reusable components
gpui itself ships no widget library, and Zed's is GPL-licensed (see the licence boundary in
`SKILL.md`), so higher-level widgets are built here. `//ui/components` holds them; the
`#[derive(IntoElement)]` + `impl RenderOnce` component idiom is shown by `Button` and
`OneRow` there, and `examples/input.rs` covers a focusable, IME-aware field.
