//! A gpui view that renders a build's display [`Terminal`] as styled monospace
//! text: the visible viewport, reflowed to the window, one shaped line per row
//! with per-cell colors and background fills.

use std::time::Duration;

use crate::model::ConsoleModel;
use bviz_terminal::{CellPos, Replay, Scroll, SelectionType, Side, Terminal, Url};
use bviz_ui_components::gesture::{
    self, Clicks, Entry, GestureRow, Gestures, Held, Input, Motion, Place, Pointer,
};
use bviz_ui_components::{
    Button, KeyScope, Nesting, Scrollbar, TextInput, cut_off_note, declare_scopes,
    find_key_bindings,
};
use bviz_ui_playback::{Clock, unaligned_note};
use bviz_ui_theme::ThemeColors;
use bviz_ui_workspace::{Panel, PanelEvent, PanelKind, Role, Titled, ViewKind};
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, Subscription, Window, actions, div, prelude::*, px,
};

mod element;
mod urls;
use element::TerminalElement;
use urls::DrawnUrls;

actions!(
    bviz_console,
    [
        /// Scroll up
        ScrollLineUp,
        /// Scroll down
        ScrollLineDown,
        /// Scroll page up
        ScrollPageUp,
        /// Scroll page down
        ScrollPageDown,
        /// Scroll to top
        ScrollToTop,
        /// Scroll to bottom
        ScrollToBottom,
        /// Copy selection
        Copy,
        /// Find in output
        Find,
        /// Go to next match
        FindNext,
        /// Go to previous match
        FindPrev,
        /// Close find bar
        FindClose,
        /// Dismiss find bar or selection
        Dismiss,
    ]
);

/// The key context scoping the console's own bindings, so a key pressed in this
/// panel reaches it and no other.
const CONSOLE_KEY_CONTEXT: KeyScope = KeyScope::new("Console", "Console");

/// What the console does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum ConsoleIntent {
    SelectLines,
    SelectWords,
    SelectText,
    OpenLink,
    Scroll,
}

const OPEN_LINK: Entry<ConsoleIntent> = Entry::new(
    Input::click(gesture::Button::Left).holding(Held::SECONDARY),
    Place::On("a link"),
    "Open link",
    ConsoleIntent::OpenLink,
);

const GESTURES: Gestures<ConsoleIntent> = Gestures::new(&[
    Entry::new(
        Input::new(Motion::Press(gesture::Button::Left, Clicks::Triple)),
        Place::Anywhere,
        "Select by lines",
        ConsoleIntent::SelectLines,
    ),
    Entry::new(
        Input::new(Motion::Press(gesture::Button::Left, Clicks::Double)),
        Place::Anywhere,
        "Select by words",
        ConsoleIntent::SelectWords,
    ),
    Entry::new(
        Input::drag(gesture::Button::Left),
        Place::Anywhere,
        "Select text",
        ConsoleIntent::SelectText,
    ),
    OPEN_LINK,
    Entry::new(
        Input::SCROLL,
        Place::Anywhere,
        "Scroll output",
        ConsoleIntent::Scroll,
    ),
]);

/// The key context of the region the scrollback grid is painted in.
const GRID_KEY_CONTEXT: KeyScope = KeyScope::within(
    &CONSOLE_KEY_CONTEXT,
    Nesting::Inside,
    "ConsoleGrid",
    "Output",
);

/// The key context of the bar the find box opens as.
const FIND_KEY_CONTEXT: KeyScope = KeyScope::within(
    &CONSOLE_KEY_CONTEXT,
    Nesting::Inside,
    "ConsoleFind",
    "Find bar",
);

pub fn register_keys(cx: &mut App) {
    declare_scopes(
        cx,
        [CONSOLE_KEY_CONTEXT, GRID_KEY_CONTEXT, FIND_KEY_CONTEXT],
    );
    let grid_unless_find = format!("{GRID_KEY_CONTEXT} && !{FIND_KEY_CONTEXT}");
    let grid_unless_find = Some(grid_unless_find.as_str());
    let console = Some(CONSOLE_KEY_CONTEXT.id());
    let find = Some(FIND_KEY_CONTEXT.id());
    cx.bind_keys([
        // Inert while the find field has focus, so they don't hijack cursor
        // movement there; home and end scroll here rather than seek.
        KeyBinding::new("up", ScrollLineUp, grid_unless_find),
        KeyBinding::new("down", ScrollLineDown, grid_unless_find),
        KeyBinding::new("home", ScrollToTop, grid_unless_find),
        KeyBinding::new("end", ScrollToBottom, grid_unless_find),
        KeyBinding::new("escape", Dismiss, grid_unless_find),
        // Paging and the commands work regardless of which region has focus.
        KeyBinding::new("pageup", ScrollPageUp, console),
        KeyBinding::new("pagedown", ScrollPageDown, console),
        KeyBinding::new("secondary-c", Copy, console),
        KeyBinding::new("enter", FindNext, find),
        KeyBinding::new("shift-enter", FindPrev, find),
        KeyBinding::new("escape", FindClose, find),
    ]);
    cx.bind_keys(find_key_bindings(CONSOLE_KEY_CONTEXT, Find));
}

/// Lines moved per arrow-key press while the terminal is focused.
const SCROLL_STEP: i32 = 3;

/// Lines scrolled per frame while a selection drag is held outside the content.
const SELECTION_AUTOSCROLL_STEP: i32 = 1;

/// How long the scroll bar stays visible after the last scroll interaction.
const SCROLLBAR_HIDE: Duration = Duration::from_millis(1200);

/// Geometry of the painted terminal content, in window pixels, kept so mouse
/// positions can be mapped to grid cells for selection.
#[derive(Clone, Copy)]
struct ContentLayout {
    left: f32,
    top: f32,
    cell_width: f32,
    line_height: f32,
    columns: usize,
    rows: usize,
}

/// A view replaying a build's captured console output as it stood at its
/// document's playback time.
pub struct ConsolePanel {
    /// The build replayed here, shared with every other view of it.
    model: Entity<ConsoleModel>,
    /// Redraws as the build is read.
    _changed: Subscription,
    /// Redraws as a gesture moves the document's playback time.
    _clock_changed: Option<Subscription>,
    /// The display terminal, built at the window size for the shown frame and
    /// rebuilt when the frame changes.
    terminal: Option<Terminal>,
    /// The frame [`terminal`] was built for, to detect when it must be rebuilt.
    terminal_frame: Option<usize>,
    /// Whether to keep the viewport pinned to the newest output. Set by an active
    /// scroll to the bottom; cleared by scrolling up.
    follow_bottom: bool,
    /// The remembered scroll target when not following: the line, counted from the
    /// top of the output, held at the viewport top. Only an active user scroll
    /// changes it, so it survives scrubbing past where that line exists yet — the
    /// view clamps to the bottom meanwhile and returns to it when the line is back.
    scroll_top: usize,
    /// The painted scroll bar geometry, set each layout for hit-testing.
    scrollbar: Option<Scrollbar>,
    /// The painted content geometry, set each layout for mapping clicks to cells.
    content: Option<ContentLayout>,
    /// Whether a click-drag text selection is in progress.
    selecting: bool,
    /// The latest cursor position during a selection drag, kept so the selection
    /// can keep auto-scrolling while the cursor is held still outside the view.
    selection_drag: Option<Point<Pixels>>,
    /// The drag's fixed anchor in content coordinates — a line counted from the top
    /// of the scrollback, a column, and a side — so a rebuild can re-anchor it on
    /// the same text while its far end follows the cursor as the view scrolls.
    selection_anchor: Option<(i32, usize, Side)>,
    /// The URL under the cursor, highlighted and openable while hovered.
    hovered_url: Option<Url>,
    drawn_urls: DrawnUrls,
    /// The URL pressed on, with the click a release that selected nothing makes
    /// of the press.
    pressed_url: Option<(String, Pointer)>,
    /// While dragging the thumb, the cursor's offset from the thumb's top.
    drag_grab: Option<f32>,
    /// Whether the scroll bar is currently shown; it auto-hides after a scroll.
    scrollbar_visible: bool,
    /// Bumped on each reveal so a stale hide timer does not hide a re-revealed bar.
    reveal_seq: u64,
    /// The terminal body's handle, focused so its scroll keys dispatch.
    body_focus: FocusHandle,
    /// Whether the find box is open.
    find_active: bool,
    /// The find box's text field, created on first open and reused.
    find_input: Option<Entity<TextInput>>,
    /// Keeps the subscription that re-runs the search on every find-field edit.
    find_observation: Option<Subscription>,
}

impl ConsolePanel {
    pub(crate) fn new(model: Entity<ConsoleModel>, cx: &mut Context<Self>) -> Self {
        // Tab cycles focus through the regions (handled by the shell's tab action
        // via window.focus_next), in this order: terminal, then the find field
        // when it is open (its tab index is set on creation).
        let body_focus = cx.focus_handle().tab_index(0).tab_stop(true);
        let changed = cx.observe(&model, |_, _, cx| cx.notify());
        let clock_changed = model
            .read(cx)
            .clock()
            .cloned()
            .map(|clock| cx.observe(&clock, |_, _, cx| cx.notify()));
        let mut view = Self {
            model,
            _changed: changed,
            _clock_changed: clock_changed,
            terminal: None,
            terminal_frame: None,
            follow_bottom: true,
            scroll_top: 0,
            scrollbar: None,
            content: None,
            selecting: false,
            selection_drag: None,
            selection_anchor: None,
            hovered_url: None,
            drawn_urls: DrawnUrls::default(),
            pressed_url: None,
            drag_grab: None,
            scrollbar_visible: false,
            reveal_seq: 0,
            body_focus,
            find_active: false,
            find_input: None,
            find_observation: None,
        };
        // Show the bar briefly on launch so the scrollable extent is visible,
        // then let it auto-hide.
        view.reveal_scrollbar(cx);
        view
    }

    /// The timeline this view plays, and `None` while the build's read has
    /// landed none.
    fn replay<'a>(&self, cx: &'a App) -> Option<&'a Replay> {
        self.model.read(cx).replay()
    }

    /// Show the scroll bar and schedule it to hide after a period of no further
    /// scroll interaction.
    fn reveal_scrollbar(&mut self, cx: &mut Context<Self>) {
        self.scrollbar_visible = true;
        self.reveal_seq = self.reveal_seq.wrapping_add(1);
        let seq = self.reveal_seq;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCROLLBAR_HIDE).await;
            let _ = this.update(cx, |this, cx| {
                // Skip if a newer reveal happened or the thumb is being dragged.
                if this.reveal_seq == seq && this.drag_grab.is_none() {
                    this.scrollbar_visible = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn scroll(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        terminal.scroll(scroll);
        // The hovered URL's highlight is in viewport coordinates, which scrolling
        // invalidates; the next pointer move recomputes it.
        self.hovered_url = None;
        self.remember_scroll();
        self.reveal_scrollbar(cx);
    }

    /// Record where the user scrolled to: follow the bottom when there, else the
    /// absolute top line, so a rebuild for a new frame restores that position.
    fn remember_scroll(&mut self) {
        if let Some(terminal) = self.terminal.as_ref() {
            self.follow_bottom = terminal.display_offset() == 0;
            self.scroll_top = terminal.history_size() - terminal.display_offset();
        }
    }

    /// Scroll so the thumb's top sits under the cursor, accounting for the grab
    /// offset, and translate that to a line via the terminal.
    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(bar), Some(grab)) = (self.scrollbar, self.drag_grab) else {
            return;
        };
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        // The bar's offset runs from 0 (the bottom, no scrollback above) to the
        // history size (the top); a fraction of that history maps to a line.
        let history = terminal.history_size().max(1) as f32;
        let fraction = bar.offset_at(position, grab) / history;
        terminal.scroll_to_fraction(fraction);
        self.remember_scroll();
        cx.notify();
    }

    /// The grid cell (column, row from the top) and the side of it under a window
    /// position, or `None` before the first layout.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize, Side)> {
        let c = self.content?;
        let x = (f32::from(position.x) - c.left).max(0.0);
        let y = (f32::from(position.y) - c.top).max(0.0);
        let column = ((x / c.cell_width) as usize).min(c.columns.saturating_sub(1));
        let row = ((y / c.line_height) as usize).min(c.rows.saturating_sub(1));
        let side = if x % c.cell_width > c.cell_width / 2.0 {
            Side::Right
        } else {
            Side::Left
        };
        Some((column, row, side))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bar) = self.scrollbar
            && bar.track_hit(event.position)
        {
            self.begin_scrollbar_drag(bar, event.position, cx);
            return;
        }
        GESTURES.dispatch(Pointer::press(event), |intent| {
            let selection = match intent {
                ConsoleIntent::SelectLines => SelectionType::Lines,
                ConsoleIntent::SelectWords => SelectionType::Semantic,
                ConsoleIntent::SelectText => SelectionType::Simple,
                _ => return false,
            };
            self.begin_selection(event, selection, window, cx);
            true
        });
    }

    /// Grabbing the thumb keeps the cursor at the same spot on it, while
    /// clicking the track jumps the thumb's center to the cursor.
    fn begin_scrollbar_drag(
        &mut self,
        bar: Scrollbar,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.drag_grab = Some(bar.grab(position));
        self.drag_to(position, cx);
    }

    /// A press on the content focuses it and begins a text selection.
    fn begin_selection(
        &mut self,
        event: &MouseDownEvent,
        selection: SelectionType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.body_focus.focus(window, cx);
        let Some((column, row, side)) = self.cell_at(event.position) else {
            return;
        };
        // A link pressed on is let go of without opening where the press
        // selects text, so dragging from a link still selects its text.
        self.pressed_url = self
            .hovered_url
            .as_ref()
            .map(|url| (url.text().to_string(), Pointer::completing(event)));
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.begin_selection(selection, CellPos { column, row }, side);
            // The anchor's line counted from the top of the scrollback, fixed to its
            // text so it survives rebuilds and scrolling under the drag.
            let anchor_line =
                row as i32 + terminal.history_size() as i32 - terminal.display_offset() as i32;
            self.selecting = true;
            self.selection_drag = Some(event.position);
            self.selection_anchor = Some((anchor_line, column, side));
            cx.notify();
        }
    }

    /// Track the URL under the cursor so it can be highlighted and opened.
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            return;
        }
        // Moving the pointer onto the gutter reveals the hidden scroll bar.
        if let Some(bar) = self.scrollbar
            && bar.track_hit(event.position)
            && !self.scrollbar_visible
        {
            self.reveal_scrollbar(cx);
        }
        let cell = self.cell_at(event.position);
        let url = match (cell, self.terminal.as_mut()) {
            (Some((column, row, _)), Some(terminal)) => terminal.url_at(column, row),
            _ => None,
        };
        if url.as_ref().map(Url::text) != self.hovered_url.as_ref().map(Url::text) {
            self.hovered_url = url;
            cx.notify();
        }
    }

    /// Extend the in-progress selection to a window position.
    fn selection_drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.selection_drag = Some(position);
        let Some((column, row, side)) = self.cell_at(position) else {
            return;
        };
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.extend_selection(CellPos { column, row }, side);
            cx.notify();
        }
    }

    /// While a selection drag is held above or below the content, scroll that way
    /// and grow the selection to the revealed edge, repeating each frame so it
    /// continues even when the cursor is still. Inside the content it does nothing,
    /// so the selection simply follows the cursor.
    fn autoscroll_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(content), Some(position)) = (self.content, self.selection_drag) else {
            return;
        };
        if !self.selecting {
            return;
        }
        let top = content.top;
        let bottom = content.top + content.line_height * content.rows as f32;
        let y = f32::from(position.y);
        // Positive scrolls toward earlier output (up); the cursor above the content
        // reveals earlier lines, below it reveals later ones.
        let lines = if y < top {
            SELECTION_AUTOSCROLL_STEP
        } else if y > bottom {
            -SELECTION_AUTOSCROLL_STEP
        } else {
            return;
        };
        let moved = match self.terminal.as_mut() {
            Some(terminal) => {
                let before = terminal.display_offset();
                terminal.scroll(Scroll::Delta(lines));
                terminal.display_offset() != before
            }
            None => return,
        };
        self.remember_scroll();
        // Extend to the cell under the cursor, which `cell_at` clamps to the edge
        // row now showing the revealed content.
        self.selection_drag_to(position, cx);
        // Keep going next frame while the cursor stays outside, cursor still or not;
        // stop once the scroll has bottomed out, to not spin at the limit.
        if moved {
            window.request_animation_frame();
        }
    }

    fn scroll_line_up(&mut self, _: &ScrollLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::Delta(SCROLL_STEP), cx);
    }

    fn scroll_line_down(&mut self, _: &ScrollLineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::Delta(-SCROLL_STEP), cx);
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::PageUp, cx);
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::PageDown, cx);
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::Top, cx);
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll(Scroll::Bottom, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.terminal.as_ref().and_then(Terminal::selection_text) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Open the find box, creating and focusing its text field on first use.
    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        // The field is drawn beside the scrollback grid, which stands only over a
        // loaded replay; opening it before then would focus a field no rendered
        // element carries, leaving the window answering no key at all.
        if self.replay(cx).is_none() {
            return;
        }
        self.find_active = true;
        if self.find_input.is_none() {
            // Tab index 1: after the terminal (0) in the cycle.
            let input = cx.new(|cx| TextInput::new(cx, "Find", 1));
            self.find_observation =
                Some(cx.observe(&input, |this, _input, cx| this.run_search(cx)));
            self.find_input = Some(input);
        }
        if let Some(input) = self.find_input.as_ref() {
            input.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.run_search(cx);
        cx.notify();
    }

    /// Apply the find field's text as the terminal's search query.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        if !self.find_active {
            return;
        }
        let query = match self.find_input.as_ref() {
            Some(input) => input.read(cx).content().to_string(),
            None => return,
        };
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.set_search(&query);
        }
        // Jumping to a match counts as leaving auto-scroll, so the next frame holds
        // this position rather than snapping back to the bottom.
        self.remember_scroll();
        // The hovered URL's highlight is in viewport coordinates, which scrolling
        // to a match invalidates; the next pointer move recomputes it.
        self.hovered_url = None;
        cx.notify();
    }

    /// Focus the next/previous match, holding the scroll there so playback does not
    /// snap the view back to the bottom.
    fn select_next_match(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.search_next();
        }
        self.remember_scroll();
        cx.notify();
    }

    fn select_prev_match(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.search_prev();
        }
        self.remember_scroll();
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_next_match(cx);
    }

    fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select_prev_match(cx);
    }

    fn find_close(&mut self, _: &FindClose, window: &mut Window, cx: &mut Context<Self>) {
        self.find_active = false;
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.clear_search();
        }
        self.body_focus.focus(window, cx);
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_active {
            self.find_close(&FindClose, window, cx);
        } else if let Some(terminal) = self.terminal.as_mut() {
            terminal.clear_selection();
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        GESTURES.dispatch(Pointer::scroll(event), |intent| match intent {
            ConsoleIntent::Scroll => {
                self.scroll_by_wheel(event, window, cx);
                true
            }
            _ => false,
        });
    }

    fn scroll_by_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let lines = (f32::from(event.delta.pixel_delta(line_height).y) / f32::from(line_height))
            .round() as i32;
        if lines != 0 {
            self.scroll(Scroll::Delta(lines), cx);
        }
    }
}

impl EventEmitter<PanelEvent> for ConsolePanel {}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.body_focus.clone()
    }
}

impl Panel for ConsolePanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("console"),
            key_context: CONSOLE_KEY_CONTEXT,
            role: Role::Log,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        GESTURES.rows().collect()
    }

    fn titled() -> Titled {
        Titled::File
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.body_focus.focus(window, cx);
        }
    }
}

impl Render for ConsolePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Clock::keep_pace(self.model.read(cx).clock(), window, cx);
        self.autoscroll_selection(window, cx);
        let theme = bviz_ui_theme::theme(window, cx);
        let colors = &theme.colors;
        let chrome = div()
            .key_context(Self::kind().key_context)
            .on_action(cx.listener(Self::scroll_line_up))
            .on_action(cx.listener(Self::scroll_line_down))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::dismiss))
            .flex()
            .flex_col()
            .size_full();
        match self.model.read(cx).source().placeholder(colors) {
            // One element tracks the body focus per frame: the grid where it is
            // drawn, and the chrome standing in for it where it is not. Tracking
            // it twice enters it in the tab order twice, and the pair then step
            // to each other instead of on to the next region.
            Some(placeholder) => chrome.track_focus(&self.body_focus).child(placeholder),
            None => {
                let find_box = if self.find_active {
                    self.render_find_box(colors, cx)
                } else {
                    None
                };
                let terminal_focused = self.body_focus.is_focused(window);
                let unaligned = self
                    .model
                    .read(cx)
                    .unaligned(cx)
                    .then(|| unaligned_note("build event stream", colors));
                let cut_off = self
                    .model
                    .read(cx)
                    .cut_off()
                    .then(|| cut_off_note("build event stream", "events", colors));
                // Unfocused, the accent takes the canvas it is drawn against, so
                // the grid keeps the same width either way.
                let terminal_accent = if terminal_focused {
                    colors.border_focused
                } else {
                    colors.background
                };
                chrome.children(cut_off).children(unaligned).child(
                    div()
                        .key_context(GRID_KEY_CONTEXT)
                        .track_focus(&self.body_focus)
                        .relative()
                        .flex_1()
                        .overflow_hidden()
                        .border_l_2()
                        .border_color(terminal_accent)
                        .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                        .on_mouse_move(cx.listener(Self::on_mouse_move))
                        .when(self.hovered_url.is_some(), |body| body.cursor_pointer())
                        .font_family(bviz_ui_assets::MONO)
                        .text_size(px(13.))
                        .text_color(colors.text)
                        .child(TerminalElement { view: cx.entity() })
                        .children(find_box),
                )
            }
        }
    }
}

impl ConsolePanel {
    /// The find box overlaid at the top-right of the terminal: the query field,
    /// a match counter, and previous/next/close controls. `None` before the field
    /// has been created (i.e. the box has never been opened).
    fn render_find_box(
        &self,
        colors: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let input = self.find_input.as_ref()?.clone();
        let query_empty = input.read(cx).content().is_empty();
        let (cur, total) = self
            .terminal
            .as_ref()
            .map_or((None, 0), |t| (t.current_match(), t.search_count()));
        let count = if query_empty {
            String::new()
        } else if total == 0 {
            "No results".to_string()
        } else {
            format!("{} / {}", cur.unwrap_or(0), total)
        };

        let find_focus = input.read(cx).focus_handle(cx);
        let view = cx.entity();
        let prev_view = view.clone();
        let next_view = view.clone();
        let close_view = view;
        Some(
            div()
                .key_context(FIND_KEY_CONTEXT)
                .occlude()
                .absolute()
                .top_2()
                .right_4()
                .flex()
                .items_center()
                .gap_2()
                .p_1()
                .rounded_md()
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .on_action(cx.listener(Self::find_next))
                .on_action(cx.listener(Self::find_prev))
                .on_action(cx.listener(Self::find_close))
                .child(div().w_48().child(input))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text)
                        .child(SharedString::from(count)),
                )
                .child(
                    Button::new("find-prev", move |_, _, cx| {
                        prev_view.update(cx, |this, cx| this.select_prev_match(cx))
                    })
                    .ghost()
                    .icon("caret-up")
                    .keys_for(&FindPrev, &find_focus),
                )
                .child(
                    Button::new("find-next", move |_, _, cx| {
                        next_view.update(cx, |this, cx| this.select_next_match(cx))
                    })
                    .ghost()
                    .icon("caret-down")
                    .keys_for(&FindNext, &find_focus),
                )
                .child(
                    Button::new("find-close", move |_, window, cx| {
                        close_view.update(cx, |this, cx| this.find_close(&FindClose, window, cx))
                    })
                    .ghost()
                    .icon("x")
                    .keys_for(&FindClose, &find_focus),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gesture_is_reachable() {
        assert_eq!(GESTURES.unreachable(), []);
    }

    #[test]
    fn every_gesture_fits_the_keys_help() {
        assert_eq!(GESTURES.overlong(), []);
    }
}
