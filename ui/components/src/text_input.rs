//! The application's text field: a focusable editor with cursor and selection
//! movement, mouse selection, clipboard, and IME composition, built on gpui's
//! platform input handler.
//!
//! Modified from source in the Zed editor; NOTICE names it.

use crate::{Host, KeyScope, track_drag};
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent, PaintQuad,
    Pixels, Point, Render, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window, div, fill, point, prelude::*, px, relative, size,
};
use std::ops::Range;

gpui::actions!(
    bviz_text_input,
    [
        /// Delete before cursor
        Backspace,
        /// Delete after cursor
        Delete,
        /// Move cursor left
        Left,
        /// Move cursor right
        Right,
        /// Extend selection left
        SelectLeft,
        /// Extend selection right
        SelectRight,
        /// Move cursor left by word
        WordLeft,
        /// Move cursor right by word
        WordRight,
        /// Extend selection left by word
        SelectWordLeft,
        /// Extend selection right by word
        SelectWordRight,
        /// Delete word before cursor
        DeleteWordLeft,
        /// Delete word after cursor
        DeleteWordRight,
        /// Select all
        SelectAll,
        /// Move cursor to start
        Home,
        /// Move cursor to end
        End,
        /// Select to start
        SelectHome,
        /// Select to end
        SelectEnd,
        /// Copy selection
        Copy,
        /// Cut selection
        Cut,
        /// Paste
        Paste,
        /// Show character palette
        ShowCharacterPalette,
    ]
);

/// The key context scoping the text field's own bindings, so a key pressed
/// while it has focus edits the text rather than reaching an enclosing panel.
pub const KEY_CONTEXT: KeyScope = KeyScope::new("TextInput", "Text field");

/// The predicate of a binding in `scope` that stands down while a text field
/// within it holds the focus, where the keystroke edits the text instead.
pub fn unless_typing(scope: KeyScope) -> String {
    format!("{scope} && !{KEY_CONTEXT}")
}

/// Register the text field's key bindings.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings(Host::CURRENT));
}

/// The modifier that turns a character key into its word key, by the
/// platform's convention.
pub const WORD_MODIFIER: &str = if cfg!(target_os = "macos") {
    "alt"
} else {
    "ctrl"
};

/// The text field's key bindings in an application running in `host`.
pub fn bindings(host: Host) -> Vec<KeyBinding> {
    let ctx = Some(KEY_CONTEXT.id());
    let word = |key: &str| format!("{WORD_MODIFIER}-{key}");
    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new(&word("left"), WordLeft, ctx),
        KeyBinding::new(&word("right"), WordRight, ctx),
        KeyBinding::new(&word("shift-left"), SelectWordLeft, ctx),
        KeyBinding::new(&word("shift-right"), SelectWordRight, ctx),
        KeyBinding::new(&word("backspace"), DeleteWordLeft, ctx),
        KeyBinding::new(&word("delete"), DeleteWordRight, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
    ];
    // A browser reads its clipboard to the page only once the visitor permits
    // it, except for the paste event its own paste key fires, which a field
    // takes through its input handler.
    if host == Host::Desktop {
        bindings.push(KeyBinding::new("secondary-v", Paste, ctx));
    }
    if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("secondary-left", Home, ctx),
            KeyBinding::new("secondary-right", End, ctx),
            KeyBinding::new("secondary-shift-left", SelectHome, ctx),
            KeyBinding::new("secondary-shift-right", SelectEnd, ctx),
        ]);
    }
    bindings
}

/// How wide the cursor is drawn.
const CURSOR_WIDTH: Pixels = px(1.5);

/// What a [`TextInput`] holds changed.
pub struct TextEdited;

impl EventEmitter<TextEdited> for TextInput {}

/// The cursor of a [`TextInput`] moved, or the selection changed, over the
/// text it holds.
pub struct CursorMoved;

impl EventEmitter<CursorMoved> for TextInput {}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the line is scrolled left within the field.
    scrolled: Pixels,
    is_selecting: bool,
}

impl TextInput {
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        tab_index: isize,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle().tab_index(tab_index).tab_stop(true),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scrolled: px(0.),
            is_selecting: false,
        }
    }

    pub fn content(&self) -> SharedString {
        self.content.clone()
    }

    /// Replaces what the field holds, leaving the cursor at the end of it.
    pub fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.hold(content.into(), cx);
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    /// Where the cursor stands, as a byte offset into what the field holds.
    pub fn cursor(&self) -> usize {
        self.cursor_offset()
    }

    /// Moves the cursor to the byte offset `offset` into what the field holds.
    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.move_to(offset.min(self.content.len()), cx);
    }

    /// Replaces the byte range `range` of what the field holds with `text`,
    /// leaving the cursor just past it.
    pub fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.splice(&range, text, cx);
        let cursor = range.start + text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())
        } else {
            self.selected_range.start
        };
        self.move_to(to, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected_range.is_empty() {
            self.next_boundary(self.selected_range.end)
        } else {
            self.selected_range.end
        };
        self.move_to(to, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(word_start_before(&self.content, self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(word_start_after(&self.content, self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(word_start_before(&self.content, self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(word_start_after(&self.content, self.cursor_offset()), cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(word_start_before(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(word_start_after(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            EntityInputHandler::paste(self, item, window, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.index_for_mouse_position(event.position);
        match event.click_count {
            // Double-click selects the word, triple-click the whole (single) line.
            2 => self.select_range(self.word_range(ix), cx),
            n if n >= 3 => self.select_range(0..self.content.len(), cx),
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(ix, cx);
                } else {
                    self.move_to(ix, cx);
                }
            }
        }
    }

    fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.is_selecting = false;
        self.selection_reversed = false;
        self.selected_range = range;
        cx.emit(CursorMoved);
        cx.notify();
    }

    /// The word around `ix`.
    fn word_range(&self, ix: usize) -> Range<usize> {
        let text = &self.content;
        text[..ix].trim_end_matches(is_word).len()..ix + word_len(&text[ix..])
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.emit(CursorMoved);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.emit(CursorMoved);
        cx.notify();
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - self.line_left(bounds))
    }

    /// Where the line starts in the field laid out at `bounds`, scrolled.
    fn line_left(&self, bounds: &Bounds<Pixels>) -> Pixels {
        bounds.left() - self.scrolled
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| offset + i)
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let (mut utf8, mut utf16) = (0, 0);
        for ch in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let (mut utf16, mut utf8) = (0, 0);
        for ch in self.content.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// The byte range an edit replaces: the explicit utf16 range if given, else
    /// the marked (in-progress IME composition) range, else the selection.
    fn replacement_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone())
    }

    fn splice(&mut self, range: &Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let spliced =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        self.hold(spliced.into(), cx);
    }

    fn hold(&mut self, content: SharedString, cx: &mut Context<Self>) {
        if content != self.content {
            self.content = content;
            cx.emit(TextEdited);
        }
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.replacement_range(range_utf16);
        self.replace(range, new_text, cx);
    }

    fn paste(&mut self, item: ClipboardItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            // Single-line field: flatten any newlines.
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.replacement_range(range_utf16);
        self.splice(&range, new_text, cx);
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.end)
            .unwrap_or_else(|| {
                let c = range.start + new_text.len();
                c..c
            });
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let left = self.line_left(&bounds);
        Some(Bounds::from_corners(
            point(left + line.x_for_index(range.start), bounds.top()),
            point(left + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line = self.last_layout.as_ref()?;
        let bounds = self.last_bounds.as_ref()?;
        let ix = line.closest_index_for_x(point.x - self.line_left(bounds));
        Some(self.offset_to_utf16(ix))
    }
}

/// Whether `c` belongs to a word: a word is a run of letters, digits and
/// underscores, so the punctuation of a label or a call parts its words.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How many bytes the word opening `text` runs; none where no word opens it.
fn word_len(text: &str) -> usize {
    text.char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(i, _)| i)
}

/// The next stop leftwards from `offset` in `text`: the start of the word the
/// offset is in or after, past whatever parts that word from it.
fn word_start_before(text: &str, offset: usize) -> usize {
    let before = text[..offset].trim_end_matches(|c| !is_word(c));
    before.trim_end_matches(is_word).len()
}

/// The next stop rightwards from `offset` in `text`: the start of the word
/// after the one the offset is in, or the end of the text where no word
/// follows.
fn word_start_after(text: &str, offset: usize) -> usize {
    let after = &text[offset..];
    let past_word = &after[word_len(after)..];
    text.len() - past_word.trim_start_matches(|c| !is_word(c)).len()
}

/// Paints the field's text and cursor and registers the platform input handler.
struct TextElement {
    input: Entity<TextInput>,
}

impl TextElement {
    fn install_selection_drag(&self, window: &mut Window, cx: &App) {
        if self.input.read(cx).is_selecting {
            track_drag(
                window,
                &self.input,
                |input, position, _, cx| {
                    let ix = input.index_for_mouse_position(position);
                    input.select_to(ix, cx);
                },
                |input, _, _, _, _| input.is_selecting = false,
            );
        }
    }
}

struct TextPrepaint {
    line: ShapedLine,
    /// Where the line is painted from.
    origin: Point<Pixels>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

/// Split the text into runs, underlining the in-progress IME composition (the
/// marked range).
fn ime_marked_runs(
    text_len: usize,
    font: Font,
    color: Hsla,
    marked_range: Option<&Range<usize>>,
) -> Vec<TextRun> {
    let run = TextRun {
        len: text_len,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    match marked_range {
        Some(marked) => vec![
            TextRun {
                len: marked.start,
                ..run.clone()
            },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text_len - marked.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|run| run.len > 0)
        .collect(),
        None => vec![run],
    }
}

/// Caret quad when the selection is empty, otherwise the selection rect.
fn cursor_selection_quads(
    line: &ShapedLine,
    selected: &Range<usize>,
    cursor: usize,
    bounds: Bounds<Pixels>,
    cursor_color: Hsla,
    selection_color: Hsla,
) -> (Option<PaintQuad>, Option<PaintQuad>) {
    if selected.is_empty() {
        let x = line.x_for_index(cursor);
        (
            Some(fill(
                Bounds::new(
                    point(bounds.left() + x, bounds.top()),
                    size(CURSOR_WIDTH, bounds.bottom() - bounds.top()),
                ),
                cursor_color,
            )),
            None,
        )
    } else {
        (
            None,
            Some(fill(
                Bounds::from_corners(
                    point(
                        bounds.left() + line.x_for_index(selected.start),
                        bounds.top(),
                    ),
                    point(
                        bounds.left() + line.x_for_index(selected.end),
                        bounds.bottom(),
                    ),
                ),
                selection_color,
            )),
        )
    }
}

/// How far a line `width` wide scrolls left within a field `visible` wide,
/// from `scrolled`, so the cursor at `cursor` along the line stays in view:
/// as little as that takes, and never so far the line's end leaves room
/// behind it.
fn scrolled_to_cursor(scrolled: Pixels, cursor: Pixels, width: Pixels, visible: Pixels) -> Pixels {
    let reach = (width + CURSOR_WIDTH - visible).max(px(0.));
    let scrolled = scrolled.min(reach);
    let scrolled = scrolled.min(cursor);
    scrolled
        .max(cursor + CURSOR_WIDTH - visible)
        .clamp(px(0.), reach)
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = TextPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> TextPrepaint {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let input = self.input.read(cx);
        let selected = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (text, color) = if input.content.is_empty() {
            (input.placeholder.clone(), colors.text_muted)
        } else {
            (input.content.clone(), colors.text)
        };
        let runs = ime_marked_runs(text.len(), style.font(), color, input.marked_range.as_ref());
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);

        let scrolled = match input.content.is_empty() {
            true => px(0.),
            false => scrolled_to_cursor(
                input.scrolled,
                line.x_for_index(cursor),
                line.width,
                bounds.size.width,
            ),
        };
        self.input.update(cx, |input, _| input.scrolled = scrolled);
        let origin = point(bounds.left() - scrolled, bounds.top());
        let (cursor_quad, selection_quad) = cursor_selection_quads(
            &line,
            &selected,
            cursor,
            Bounds::new(origin, bounds.size),
            colors.text,
            colors.selection,
        );
        TextPrepaint {
            line,
            origin,
            cursor: cursor_quad,
            selection: selection_quad,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut TextPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let line = std::mem::take(&mut prepaint.line);
        // The selection and the cursor mark where typing lands, which only a
        // focused field takes.
        let focused = focus_handle.is_focused(window);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if focused && let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            let _ = line.paint(
                prepaint.origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
            if focused && let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
        self.install_selection_drag(window, cx);
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let focused = self.focus_handle.is_focused(window);
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(colors.background)
            .border_1()
            .border_color(if focused {
                colors.border_focused
            } else {
                colors.border
            })
            .child(TextElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap_probe;

    #[test]
    fn a_browser_leaves_its_paste_key_to_the_page() {
        let in_field = &[KEY_CONTEXT.id()];
        let pasted = |host| keymap_probe::dispatched(bindings(host), "secondary-v", in_field);
        assert!(pasted(Host::Desktop).is_some_and(|action| action.as_any().is::<Paste>()));
        assert!(pasted(Host::Browser).is_none());
    }

    const VISIBLE: Pixels = px(100.);

    #[test]
    fn a_line_that_fits_never_scrolls() {
        assert_eq!(
            scrolled_to_cursor(px(0.), px(60.), px(60.), VISIBLE),
            px(0.)
        );
        assert_eq!(
            scrolled_to_cursor(px(30.), px(10.), px(60.), VISIBLE),
            px(0.)
        );
    }

    /// The cursor typed past the field's right edge pulls the line along
    /// just far enough to show it there.
    #[test]
    fn a_cursor_past_the_edge_scrolls_the_line_to_it() {
        let scrolled = scrolled_to_cursor(px(0.), px(250.), px(250.), VISIBLE);
        assert_eq!(scrolled, px(250.) + CURSOR_WIDTH - VISIBLE);
    }

    /// Moving the cursor within what is in view leaves the line where it is;
    /// moving it before the left edge brings the line back to it.
    #[test]
    fn the_line_moves_only_as_far_as_the_cursor_needs() {
        let width = px(400.);
        assert_eq!(
            scrolled_to_cursor(px(120.), px(150.), width, VISIBLE),
            px(120.)
        );
        assert_eq!(
            scrolled_to_cursor(px(120.), px(40.), width, VISIBLE),
            px(40.)
        );
    }

    /// Every word stop over `text`, from the start rightwards and from the end
    /// leftwards.
    fn word_stops(text: &str) -> (Vec<usize>, Vec<usize>) {
        let walk = |mut at: usize, step: fn(&str, usize) -> usize| {
            let mut stops = Vec::new();
            loop {
                let next = step(text, at);
                if next == at {
                    return stops;
                }
                stops.push(next);
                at = next;
            }
        };
        (
            walk(0, word_start_after),
            walk(text.len(), word_start_before),
        )
    }

    /// Each segment of a label and the function calling it is a word of its
    /// own, and motion either way stops just before each, the punctuation
    /// between them parting them without a stop of its own.
    #[test]
    fn word_motion_stops_before_each_segment_of_a_label() {
        let text = "deps(//foo/bar:baz)";
        let (rightwards, leftwards) = word_stops(text);
        let at = |part: &str| text.find(part).unwrap();
        assert_eq!(rightwards, [at("foo"), at("bar"), at("baz"), text.len()]);
        assert_eq!(leftwards, [at("baz"), at("bar"), at("foo"), 0]);
    }

    /// Spaces, operators and a word's own underscores and digits read as word
    /// motion reads them in prose and identifiers alike.
    #[test]
    fn word_motion_steps_over_spaces_and_operators() {
        let text = "kind(cc_library, x) except  y2";
        let (rightwards, leftwards) = word_stops(text);
        assert_eq!(rightwards, [5, 17, 20, 28, 30]);
        assert_eq!(leftwards, [28, 20, 17, 5, 0]);
    }

    /// From inside a word, motion goes to its own start leftwards and to the
    /// next word's start rightwards.
    #[test]
    fn word_motion_from_inside_a_word_goes_to_word_starts() {
        let text = "//foo/bar:baz";
        let inside = text.find("ar").unwrap();
        assert_eq!(word_start_before(text, inside), text.find("bar").unwrap());
        assert_eq!(word_start_after(text, inside), text.find("baz").unwrap());
    }

    /// Words outside ASCII are words, and motion lands only on character
    /// boundaries.
    #[test]
    fn word_motion_steps_over_whole_characters() {
        let text = "größe-ü";
        assert_eq!(word_start_after(text, 0), text.find('ü').unwrap());
        assert_eq!(word_start_before(text, text.len()), text.find('ü').unwrap());
    }

    /// Text cut from the end leaves no empty room behind the line's end.
    #[test]
    fn a_shortened_line_scrolls_back_to_fill_the_field() {
        let scrolled = scrolled_to_cursor(px(300.), px(120.), px(150.), VISIBLE);
        assert_eq!(scrolled, px(150.) + CURSOR_WIDTH - VISIBLE);
    }
}
