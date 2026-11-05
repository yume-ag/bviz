use bviz_terminal::{Cell, Flags, Terminal};
use bviz_ui_components::{
    Axis, DEFAULT_MIN_THUMB, DragEnd, Hint, Scrollbar, ScrollbarColors, track_drag,
};
use bviz_ui_theme::{ThemeColors, TooltipColors};
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, Font, GlobalElementId,
    Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Point, ShapedLine, Style,
    TextRun, Window, div, fill, point, prelude::*, px, relative, size,
};

use super::{ConsoleIntent, ConsolePanel, ContentLayout, GESTURES, OPEN_LINK};
use crate::color::{CellStyle, Defaults, style_key};

/// Width of the scroll bar gutter reserved on the right edge. The bar is an
/// overlay that auto-hides and paints only a thumb, so this strip is empty
/// whenever the bar is at rest: it is kept to the narrowest that stays grabbable,
/// leaving the width to the text grid it floats over.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// The share of the highlight a search match is washed with: enough for the run
/// to be picked out of the canvas, light enough that what it says is still read.
const MATCH_TINT: f32 = 0.16;

/// The share the focused match is washed with, which the reader has to tell from
/// the wash on every other match while still reading the text set over it.
const FOCUSED_MATCH_TINT: f32 = 0.31;

/// Custom element that sizes the terminal to its bounds, then shapes the visible
/// viewport and paints background fills and glyphs.
pub(super) struct TerminalElement {
    pub(super) view: Entity<ConsolePanel>,
}

/// Font metrics resolved once per layout from the inherited monospace text style.
pub(super) struct Metrics {
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    font: Font,
}

/// A shaped run of text and the origin it paints at. Runs are positioned per
/// grid column rather than as one line per row, so wide glyphs and the cells
/// after them stay column-aligned instead of drifting by the shaped advances.
struct PositionedLine {
    origin: Point<Pixels>,
    line: ShapedLine,
}

pub(super) struct Prepaint {
    lines: Vec<PositionedLine>,
    backgrounds: Vec<PaintQuad>,
    /// Quads painted over the text, such as the hovered URL's underline.
    overlays: Vec<PaintQuad>,
    scrollbar: Option<PaintQuad>,
    /// The hint over each row of every URL drawn.
    url_hints: Vec<AnyElement>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl TerminalElement {
    /// Size and place the scroll bar from how much is off-screen and how far the
    /// viewport is scrolled: the content is the whole scrollback plus the visible
    /// rows, and the offset is how far below the top the viewport sits
    /// (`history - offset`). The geometry is kept on the view for hit-testing even
    /// while hidden; the returned thumb quad is painted only while the bar is
    /// revealed or being dragged, with no track.
    fn scrollbar_quad(
        &self,
        bounds: Bounds<Pixels>,
        history: usize,
        offset: usize,
        rows: usize,
        colors: &ThemeColors,
        cx: &mut App,
    ) -> Option<PaintQuad> {
        let scrollbar_layout = Scrollbar::new(
            Axis::Vertical,
            Bounds::new(
                point(
                    px(f32::from(bounds.right()) - SCROLLBAR_WIDTH),
                    bounds.top(),
                ),
                size(px(SCROLLBAR_WIDTH), bounds.size.height),
            ),
            (history + rows) as f32,
            rows as f32,
            (history - offset) as f32,
            DEFAULT_MIN_THUMB,
        );
        self.view
            .update(cx, |view, _| view.scrollbar = scrollbar_layout);
        let view = self.view.read(cx);
        let show_scrollbar = view.scrollbar_visible || view.drag_grab.is_some();
        scrollbar_layout
            .filter(|_| show_scrollbar)
            .map(|bar| fill(bar.thumb(), ScrollbarColors::from_colors(colors).thumb))
    }

    /// Shape the visible grid into one [`PositionedLine`] per contiguous
    /// same-style span and collect the non-default cell background fills.
    fn shape_viewport(
        viewport: &[Vec<Cell>],
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        defaults: &Defaults,
        window: &mut Window,
    ) -> (Vec<PositionedLine>, Vec<PaintQuad>) {
        let mut lines = Vec::with_capacity(viewport.len());
        let mut backgrounds = Vec::new();

        for (r, row) in viewport.iter().enumerate() {
            let y = bounds.top() + metrics.line_height * r as f32;

            let mut col = 0;
            while col < row.len() {
                let start = col;
                let key = style_key(&row[col]);
                // A wide character occupies two columns: its glyph cell carries
                // the character, the following spacer cell carries none.
                let mut run_text = String::new();
                while col < row.len() && style_key(&row[col]) == key {
                    if !row[col].flags.contains(Flags::WIDE_CHAR_SPACER) {
                        run_text.push(row[col].c);
                    }
                    col += 1;
                }
                let style = CellStyle::resolve(&row[start], defaults);
                let x = bounds.left() + metrics.cell_width * start as f32;

                if style.background != defaults.bg {
                    let width = metrics.cell_width * (col - start) as f32;
                    backgrounds.push(fill(
                        Bounds::new(point(x, y), size(width, metrics.line_height)),
                        style.background,
                    ));
                }

                if run_text.is_empty() {
                    continue;
                }
                let mut font = metrics.font.clone();
                font.weight = style.weight;
                font.style = style.font_style;
                let run = TextRun {
                    len: run_text.len(),
                    font,
                    color: style.foreground.into(),
                    background_color: None,
                    underline: style.underline,
                    strikethrough: style.strikethrough,
                };
                lines.push(PositionedLine {
                    origin: point(x, y),
                    line: window.text_system().shape_line(
                        run_text.into(),
                        metrics.font_size,
                        std::slice::from_ref(&run),
                        None,
                    ),
                });
            }
        }
        (lines, backgrounds)
    }

    /// Full-cell fill covering the inclusive column span `first..=last` on `row`.
    fn span_fill(
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        row: usize,
        first: usize,
        last: usize,
        color: Hsla,
    ) -> PaintQuad {
        let x = bounds.left() + metrics.cell_width * first as f32;
        let y = bounds.top() + metrics.line_height * row as f32;
        let width = metrics.cell_width * (last - first + 1) as f32;
        fill(
            Bounds::new(point(x, y), size(width, metrics.line_height)),
            color,
        )
    }

    /// The selection wash over the selected cells; painted under the text so it
    /// stays legible.
    fn selection_quads(
        selection: &[Option<(usize, usize)>],
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        selection_color: Hsla,
    ) -> Vec<PaintQuad> {
        let mut quads = Vec::with_capacity(selection.len());
        for (r, span) in selection.iter().enumerate() {
            let Some((first, last)) = span else { continue };
            quads.push(Self::span_fill(
                bounds,
                metrics,
                r,
                *first,
                *last,
                selection_color,
            ));
        }
        quads
    }

    /// Highlight wash over search matches, the focused match more strongly.
    fn search_quads(
        search: &[Vec<(usize, usize, bool)>],
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        highlight: Hsla,
    ) -> Vec<PaintQuad> {
        let mut quads = Vec::with_capacity(search.iter().map(Vec::len).sum());
        for (r, spans) in search.iter().enumerate() {
            for &(first, last, focused) in spans {
                let tint = if focused {
                    FOCUSED_MATCH_TINT
                } else {
                    MATCH_TINT
                };
                let color = highlight.opacity(tint);
                quads.push(Self::span_fill(bounds, metrics, r, first, last, color));
            }
        }
        quads
    }

    /// Underline the hovered URL to mark it as one that can be opened.
    fn url_overlay(
        &self,
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        text_accent: Hsla,
        cx: &App,
    ) -> Vec<PaintQuad> {
        let mut overlays = Vec::new();
        if let Some(url) = self.view.read(cx).hovered_url.as_ref() {
            for (r, span) in url.spans().iter().enumerate() {
                let Some((first, last)) = span else { continue };
                let x = bounds.left() + metrics.cell_width * *first as f32;
                let y = bounds.top() + metrics.line_height * (r as f32 + 1.0) - px(2.0);
                let width = metrics.cell_width * (*last - *first + 1) as f32;
                overlays.push(fill(
                    Bounds::new(point(x, y), size(width, px(1.5))),
                    text_accent,
                ));
            }
        }
        overlays
    }

    /// An element over each row of every URL drawn, whose hint names the URL
    /// in full and how to open it: an element of its own, so that it shows
    /// after the delay every control's hint keeps, and laid out before the
    /// pointer reaches the URL, whose move onto it is what starts that delay.
    fn url_hints(
        &self,
        bounds: Bounds<Pixels>,
        metrics: &Metrics,
        columns: usize,
        colors: TooltipColors,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let urls = self.view.update(cx, |view, _| {
            // A drag selecting text crosses links on its way, not to read them.
            if view.selecting {
                return None;
            }
            let frame = view.terminal_frame?;
            let terminal = view.terminal.as_mut()?;
            Some(view.drawn_urls.of(terminal, frame))
        });
        let Some(urls) = urls else {
            return Vec::new();
        };
        let mut hints = Vec::with_capacity(urls.len());
        for url in urls.iter() {
            let hint = Hint::default()
                .text(url.text.clone())
                .gesture(OPEN_LINK.row());
            for (r, span) in url.spans.iter().enumerate() {
                let Some((first, last)) = *span else { continue };
                let origin = point(
                    bounds.left() + metrics.cell_width * first as f32,
                    bounds.top() + metrics.line_height * r as f32,
                );
                let width = metrics.cell_width * (last - first + 1) as f32;
                let space = size(width, metrics.line_height).map(AvailableSpace::Definite);
                let cell = r * columns + first;
                let id = ElementId::named_usize(url.text.clone(), cell);
                let over = hint.clone().on(div().id(id).size_full(), colors);
                let mut over = over.into_any_element();
                over.prepaint_as_root(origin, space, window, cx);
                hints.push(over);
            }
        }
        hints
    }

    fn install_thumb_drag(&self, window: &mut Window, cx: &App) {
        if self.view.read(cx).drag_grab.is_some() {
            track_drag(
                window,
                &self.view,
                |view, position, _, cx| view.drag_to(position, cx),
                |view, _, _, _, cx| {
                    if view.drag_grab.take().is_some() {
                        view.reveal_scrollbar(cx);
                    }
                },
            );
        }
    }

    fn install_selection_drag(&self, window: &mut Window, cx: &App) {
        if self.view.read(cx).selecting {
            track_drag(
                window,
                &self.view,
                |view, position, _, cx| view.selection_drag_to(position, cx),
                |view, _, end, _, cx| {
                    if view.selecting {
                        view.selecting = false;
                        view.selection_drag = None;
                        view.selection_anchor = None;
                        if let Some((url, click)) = view.pressed_url.take() {
                            let dragged = view
                                .terminal
                                .as_ref()
                                .and_then(Terminal::selection_text)
                                .is_some();
                            if !dragged && matches!(end, DragEnd::Released) {
                                GESTURES.dispatch(click, |intent| match intent {
                                    ConsoleIntent::OpenLink => {
                                        cx.open_url(&url);
                                        true
                                    }
                                    _ => false,
                                });
                            }
                        }
                        cx.notify();
                    }
                },
            );
        }
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = Metrics;
    type PrepaintState = Prepaint;

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
    ) -> (LayoutId, Metrics) {
        let metrics = bviz_ui_assets::text_metrics(window);
        // A monospace advance width, measured from a single glyph.
        let color = window.text_style().color;
        let cell_width =
            bviz_ui_assets::shape_line(window, "M", metrics.font.clone(), metrics.font_size, color)
                .width;

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        (
            window.request_layout(style, [], cx),
            Metrics {
                cell_width,
                line_height: metrics.line_height,
                font_size: metrics.font_size,
                font: metrics.font,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        metrics: &mut Metrics,
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = bviz_ui_theme::theme(window, cx);
        let colors = &theme.colors;
        let defaults = Defaults::from_theme(&theme);

        // Columns fit the width less the scroll bar gutter; rows fit the height.
        let cols = ((f32::from(bounds.size.width) - SCROLLBAR_WIDTH)
            / f32::from(metrics.cell_width))
        .floor()
        .max(1.0) as usize;
        let rows = (f32::from(bounds.size.height) / f32::from(metrics.line_height))
            .floor()
            .max(1.0) as usize;
        (self.view).update(cx, |view, cx| view.sync_terminal(cols, rows, cx));

        let (viewport, history, offset, selection, search) =
            match self.view.read(cx).terminal.as_ref() {
                Some(terminal) => (
                    terminal.viewport(),
                    terminal.history_size(),
                    terminal.display_offset(),
                    terminal.selection_spans(),
                    terminal.search_spans(),
                ),
                None => (Vec::new(), 0, 0, Vec::new(), Vec::new()),
            };
        self.view.update(cx, |view, _| {
            view.content = Some(ContentLayout {
                left: f32::from(bounds.left()),
                top: f32::from(bounds.top()),
                cell_width: f32::from(metrics.cell_width),
                line_height: f32::from(metrics.line_height),
                columns: cols,
                rows,
            });
        });

        let scrollbar = self.scrollbar_quad(bounds, history, offset, rows, colors, cx);

        let (lines, mut backgrounds) =
            Self::shape_viewport(&viewport, bounds, metrics, &defaults, window);
        backgrounds.extend(Self::selection_quads(
            &selection,
            bounds,
            metrics,
            colors.selection,
        ));
        backgrounds.extend(Self::search_quads(
            &search,
            bounds,
            metrics,
            colors.highlight,
        ));
        let overlays = self.url_overlay(bounds, metrics, colors.text_accent, cx);
        let url_hints = self.url_hints(bounds, metrics, cols, colors.tooltip(), window, cx);

        Prepaint {
            lines,
            backgrounds,
            overlays,
            scrollbar,
            url_hints,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        metrics: &mut Metrics,
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        for quad in prepaint.backgrounds.drain(..) {
            window.paint_quad(quad);
        }
        for positioned in prepaint.lines.iter() {
            let _ = positioned.line.paint(
                positioned.origin,
                metrics.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        for quad in prepaint.overlays.drain(..) {
            window.paint_quad(quad);
        }
        if let Some(scrollbar) = prepaint.scrollbar.take() {
            window.paint_quad(scrollbar);
        }
        for hint in &mut prepaint.url_hints {
            hint.paint(window, cx);
        }

        self.install_thumb_drag(window, cx);
        self.install_selection_drag(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_theme::{
        MIN_DISTINCT_COLOR, MIN_TEXT_CONTRAST, built_in_themes, contrast_ratio, distance, over,
    };

    /// Marking the matches is only worth anything if the reader can see which
    /// runs matched, which one the find box is on, and what any of them says.
    /// The one the find box is on is the one asking to be looked at, so it is the
    /// wash that stands further off the canvas; the ranking read the other way
    /// would send the eye to every match but that one.
    #[test]
    fn a_search_match_is_seen_the_focused_one_ranks_above_it_and_both_stay_readable() {
        for theme in built_in_themes() {
            let c = &theme.colors;
            let wash = |tint| over(c.highlight.opacity(tint), c.background);
            let (matched, focused) = (wash(MATCH_TINT), wash(FOCUSED_MATCH_TINT));
            let off_canvas = |wash| distance(wash, c.background);

            let marked = off_canvas(matched);
            assert!(marked >= MIN_DISTINCT_COLOR, "{}: {marked}", theme.name);
            let apart = distance(focused, matched);
            assert!(apart >= MIN_DISTINCT_COLOR, "{}: {apart}", theme.name);
            let lifted = off_canvas(focused);
            assert!(lifted > marked, "{}: {lifted} against {marked}", theme.name);
            let contrast = contrast_ratio(c.text, focused);
            assert!(
                contrast >= MIN_TEXT_CONTRAST,
                "{}: {contrast}:1",
                theme.name
            );
        }
    }
}
