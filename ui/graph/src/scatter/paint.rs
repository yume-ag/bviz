//! Painting the scatter plot: its axes and the marks placed along them.

use super::plot::{DataArea, LogScale, Mark, Plotted, Tick, Zoom};
use crate::Palette;
use crate::paint::UNMARKED_FADE;
use bviz_ui_theme::mix;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Hsla, Pixels, Rgba, ShapedLine, TextAlign, Window, fill,
    point, px, quad, size,
};

/// Room left of the axes for the vertical tick labels.
const LEFT_MARGIN: f32 = 60.0;
/// Room right of the axes for the last horizontal tick label, centred on the
/// axes' right edge, to reach past it.
const RIGHT_MARGIN: f32 = 24.0;
/// Room above the axes for the vertical axis title.
const TOP_MARGIN: f32 = 32.0;
/// Room below the axes for the horizontal tick labels and axis title.
const BOTTOM_MARGIN: f32 = 50.0;

/// Below this the axes leave no room worth plotting in, and nothing is drawn.
const MIN_FRAME: f32 = 40.0;

const MAJOR_TICK: f32 = 6.0;
const MINOR_TICK: f32 = 3.0;
/// Between a tick's end and its label, and between the labels and a title.
const LABEL_GAP: f32 = 4.0;

const TICK_LABEL_SIZE: f32 = 11.0;
const TITLE_SIZE: f32 = 12.0;

/// How far a grid line recedes into the background: faint enough that it does
/// not compete with the marks.
const GRID_BLEND: f32 = 0.5;
/// Opacity of a mark, so a dense cluster reads darker than a lone target.
const MARK_ALPHA: f32 = 0.5;

/// The frame the axes are drawn along within a plot filling `bounds`; `None`
/// where that leaves no room worth plotting in.
fn axes_within(bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let axes = Bounds::from_corners(
        point(
            bounds.left() + px(LEFT_MARGIN),
            bounds.top() + px(TOP_MARGIN),
        ),
        point(
            bounds.right() - px(RIGHT_MARGIN),
            bounds.bottom() - px(BOTTOM_MARGIN),
        ),
    );
    (axes.size.width >= px(MIN_FRAME) && axes.size.height >= px(MIN_FRAME)).then_some(axes)
}

/// Where the points of `plotted` are placed in a plot filling `bounds` at
/// `zoom`, which every pointer gesture over the plot resolves through as its
/// paint does.
pub(crate) fn data_area(plotted: &Plotted, zoom: Zoom, bounds: Bounds<Pixels>) -> Option<DataArea> {
    Some(DataArea::within(
        axes_within(bounds)?,
        plotted.x,
        plotted.y,
        zoom,
    ))
}

/// Paints `plotted` filling `bounds` at `zoom`.
pub(crate) fn paint_plot(
    plotted: &Plotted,
    zoom: Zoom,
    palette: &Palette,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(area) = data_area(plotted, zoom, bounds) else {
        return;
    };
    let axes = area.frame();
    let ink = Ink::new(palette);
    paint_grid(&area, plotted, axes, &ink, window);
    paint_axes(&area, plotted, axes, &ink, window, cx);
    // One layer shares one draw order among the marks, so a mark is inserted
    // without searching what it overlaps; submission order still stacks them.
    let mask = ContentMask { bounds: axes };
    window.with_content_mask(Some(mask), |window| {
        window.paint_layer(axes, |window| {
            for mark in &plotted.marks {
                let center = area.place(mark);
                let r = px(mark.radius);
                let fade = if mark.faded { UNMARKED_FADE } else { 1.0 };
                let color = Rgba {
                    a: MARK_ALPHA * fade,
                    ..mark.hue.color(palette)
                };
                window.paint_quad(quad(
                    Bounds::new(center - point(r, r), size(r * 2., r * 2.)),
                    r,
                    color,
                    px(0.),
                    Hsla::transparent_black(),
                    BorderStyle::Solid,
                ));
            }
        })
    });
}

/// Between a ringed mark's rim and the ring drawn round it.
const RING_GAP: f32 = 2.0;
const RING_WIDTH: f32 = 1.5;

/// Rings `mark`, placed through `area`.
pub(crate) fn paint_ring(mark: &Mark, area: &DataArea, palette: &Palette, window: &mut Window) {
    let ring = ring_bounds(mark, area);
    window.paint_quad(quad(
        ring,
        ring.size.width / 2.,
        Hsla::transparent_black(),
        px(RING_WIDTH),
        palette.label,
        BorderStyle::Solid,
    ));
}

/// How far outside the hover ring the ring marking a selected point stands,
/// so the two read apart on one mark.
const SELECTION_GAP: f32 = 2.0;
/// The selection ring's stroke, wide enough to read over any mark's fill.
const SELECTION_WIDTH: f32 = 2.5;
/// How far past the selection ring a pulse spreads before it fades out.
const PULSE_REACH: f32 = 18.0;

/// Rings `mark` in `area` as selected, in the accent emphasis is drawn in,
/// with a pulse spreading from the ring `pulse` of its way out, where one
/// is under way.
pub(crate) fn paint_selection(
    mark: &Mark,
    area: &DataArea,
    palette: &Palette,
    pulse: Option<f32>,
    window: &mut Window,
) {
    let center = area.place(mark);
    let circle = |r: f32| {
        let r = px(r);
        Bounds::new(center - point(r, r), size(r * 2., r * 2.))
    };
    let r = mark.radius + RING_GAP + RING_WIDTH + SELECTION_GAP + SELECTION_WIDTH;
    let ring = circle(r);
    window.paint_quad(quad(
        ring,
        ring.size.width / 2.,
        Hsla::transparent_black(),
        px(SELECTION_WIDTH),
        palette.highlight,
        BorderStyle::Solid,
    ));
    if let Some(progress) = pulse {
        let spread = circle(r + PULSE_REACH * progress);
        let mut fading = palette.highlight;
        fading.a *= 1.0 - progress;
        window.paint_quad(quad(
            spread,
            spread.size.width / 2.,
            Hsla::transparent_black(),
            px(SELECTION_WIDTH),
            fading,
            BorderStyle::Solid,
        ));
    }
}

/// Where [`paint_ring`] rings `mark` in `area`.
pub(crate) fn ring_bounds(mark: &Mark, area: &DataArea) -> Bounds<Pixels> {
    let center = area.place(mark);
    let r = px(mark.radius + RING_GAP + RING_WIDTH);
    Bounds::new(center - point(r, r), size(r * 2., r * 2.))
}

/// The colours the plot's frame is drawn in.
struct Ink {
    axis: Rgba,
    grid: Rgba,
    label: Hsla,
    muted: Hsla,
}

impl Ink {
    fn new(palette: &Palette) -> Self {
        let muted: Rgba = palette.colors.text_muted.into();
        Self {
            axis: palette.border,
            grid: mix(palette.border, palette.background, GRID_BLEND),
            label: palette.label.into(),
            muted: muted.into(),
        }
    }
}

/// How few major ticks in view leave an axis to be read from its minor ones.
const MIN_LABELLED: usize = 2;

/// A tick of an axis in view, where it falls along the axis, and whether it
/// is labelled.
struct ShownTick {
    at: Pixels,
    tick: Tick,
    labelled: bool,
}

/// The ticks of `scale` in view, `visible` giving where one falls where it is.
/// Every major tick is labelled, and every minor one too where fewer than
/// [`MIN_LABELLED`] majors are in view, so a zoom always reads against
/// labelled values.
fn shown_ticks(scale: LogScale, visible: impl Fn(u32) -> Option<Pixels>) -> Vec<ShownTick> {
    let ticks = scale.ticks();
    let mut shown: Vec<ShownTick> = Vec::with_capacity(ticks.len());
    shown.extend(ticks.into_iter().filter_map(|tick| {
        Some(ShownTick {
            at: visible(tick.value)?,
            tick,
            labelled: tick.major,
        })
    }));
    if shown.iter().filter(|shown| shown.labelled).count() < MIN_LABELLED {
        for shown in &mut shown {
            shown.labelled = true;
        }
    }
    shown
}

/// A faint line across the axes at every labelled tick.
fn paint_grid(
    area: &DataArea,
    plotted: &Plotted,
    axes: Bounds<Pixels>,
    ink: &Ink,
    window: &mut Window,
) {
    let labelled = |ticks: Vec<ShownTick>| {
        ticks
            .into_iter()
            .filter(|shown| shown.labelled)
            .map(|shown| shown.at)
    };
    for x in labelled(shown_ticks(plotted.x, |n| area.visible_x(n))) {
        window.paint_quad(fill(
            Bounds::new(point(x, axes.top()), size(px(1.), axes.size.height)),
            ink.grid,
        ));
    }
    for y in labelled(shown_ticks(plotted.y, |n| area.visible_y(n))) {
        window.paint_quad(fill(
            Bounds::new(point(axes.left(), y), size(axes.size.width, px(1.))),
            ink.grid,
        ));
    }
}

/// The axis lines, their ticks and tick labels, and the axis titles.
fn paint_axes(
    area: &DataArea,
    plotted: &Plotted,
    axes: Bounds<Pixels>,
    ink: &Ink,
    window: &mut Window,
    cx: &mut App,
) {
    let (left, bottom) = (axes.left(), axes.bottom());
    window.paint_quad(fill(
        Bounds::new(
            point(left - px(1.), axes.top()),
            size(px(1.), axes.size.height + px(1.)),
        ),
        ink.axis,
    ));
    window.paint_quad(fill(
        Bounds::new(
            point(left - px(1.), bottom),
            size(axes.size.width + px(1.), px(1.)),
        ),
        ink.axis,
    ));
    let length = |tick: &Tick| px(if tick.major { MAJOR_TICK } else { MINOR_TICK });

    let label_top = bottom + px(MAJOR_TICK + LABEL_GAP);
    for shown in shown_ticks(plotted.x, |n| area.visible_x(n)) {
        let (x, tick) = (shown.at, shown.tick);
        window.paint_quad(fill(
            Bounds::new(point(x, bottom), size(px(1.), length(&tick))),
            ink.axis,
        ));
        if shown.labelled {
            let text = plotted.axes.x().tick_label(tick.value);
            paint_text(
                &text,
                TICK_LABEL_SIZE,
                ink.muted,
                Align::Center(x),
                label_top,
                window,
                cx,
            );
        }
    }
    let label_right = left - px(MAJOR_TICK + LABEL_GAP);
    for shown in shown_ticks(plotted.y, |n| area.visible_y(n)) {
        let (y, tick) = (shown.at, shown.tick);
        let tick_left = left - length(&tick);
        window.paint_quad(fill(
            Bounds::new(point(tick_left, y), size(length(&tick), px(1.))),
            ink.axis,
        ));
        if shown.labelled {
            let text = plotted.axes.y().tick_label(tick.value);
            let top = y - px(TICK_LABEL_SIZE / 2.);
            paint_text(
                &text,
                TICK_LABEL_SIZE,
                ink.muted,
                Align::End(label_right),
                top,
                window,
                cx,
            );
        }
    }

    let x_title_top = label_top + px(TICK_LABEL_SIZE + LABEL_GAP * 2.);
    let x_center = axes.left() + axes.size.width / 2.;
    paint_text(
        plotted.axes.x().title(),
        TITLE_SIZE,
        ink.label,
        Align::Center(x_center),
        x_title_top,
        window,
        cx,
    );
    let y_title_top = axes.top() - px(TITLE_SIZE + LABEL_GAP * 2.);
    paint_text(
        plotted.axes.y().title(),
        TITLE_SIZE,
        ink.label,
        Align::Start(left),
        y_title_top,
        window,
        cx,
    );
}

/// Which point of a line of text sits at a given horizontal position.
#[derive(Clone, Copy)]
enum Align {
    Start(Pixels),
    Center(Pixels),
    End(Pixels),
}

fn paint_text(
    text: &str,
    font_size: f32,
    color: Hsla,
    align: Align,
    top: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let (line, left) = shaped(text, font_size, color, align, window);
    let _ = line.paint(
        point(left, top),
        px(font_size),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// `text` shaped as one line, and where its left edge falls for `align`.
fn shaped(
    text: &str,
    font_size: f32,
    color: Hsla,
    align: Align,
    window: &mut Window,
) -> (ShapedLine, Pixels) {
    let font = bviz_ui_assets::text_metrics(window).font;
    let line = bviz_ui_assets::shape_line(window, text.to_owned(), font, px(font_size), color);
    let left = match align {
        Align::Start(x) => x,
        Align::Center(x) => x - line.width() / 2.,
        Align::End(x) => x - line.width(),
    };
    (line, left)
}
