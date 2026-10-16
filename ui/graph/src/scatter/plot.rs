//! Where the scatter plot's marks and ticks fall: measures on logarithmic
//! axes, and a mark's size by the count its measurement sizes points by.

use super::axis::{Axes, Axis, Reported};
use crate::hue::Hue;
use bviz_graph_layout::Id;
use bviz_graph_layout::metrics::{Measurement, Metrics, Sizing};
use bviz_graph_layout::select::Verdict;
use bviz_ui_components::amount;
use gpui::{Bounds, Pixels, Point, point, px};

/// An axis scaled by `log10(1 + n)`, so a value of zero sits at the origin and
/// every decade past it spans the same length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LogScale {
    /// The value at the far end of the axis: the first tick of its ladder at
    /// or past every value plotted on it.
    top: u32,
    ladder: Ladder,
}

/// A mark on a [`LogScale`]. The majors are zero and the first tick of each
/// band of the scale's [`Ladder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tick {
    pub(crate) value: u32,
    pub(crate) major: bool,
}

/// The values the ticks of a [`LogScale`] fall on past zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ladder {
    /// Once, twice and five times each power of ten.
    Counts,
    /// Round durations in milliseconds: as [`Counts`](Self::Counts), but for
    /// stepping [as a clock does](CLOCK_BANDS) from ten seconds to the minute
    /// and from ten minutes to the hour.
    Millis,
    /// Round amounts in kibibytes: as [`Counts`](Self::Counts) within each
    /// binary unit, from once the unit to five hundred times it, the next
    /// band starting at the next unit.
    Kibibytes,
}

/// The multiples of a band's unit its ticks fall on, but for a clock band.
const DECADE_STEPS: [u32; 3] = [1, 2, 5];

/// The multiples of a [clock band](CLOCK_BANDS)'s unit its ticks fall on.
const CLOCK_STEPS: [u32; 3] = [1, 2, 3];

pub(super) const SECOND_MS: u32 = 1_000;
pub(super) const MINUTE_MS: u32 = 60 * SECOND_MS;
pub(super) const HOUR_MS: u32 = 60 * MINUTE_MS;

const PER_BINARY_UNIT: u32 = amount::PER_BINARY_UNIT as u32;

/// The unit of the last band of [`Ladder::Kibibytes`] within one binary unit,
/// as a multiple of that unit.
const LAST_BAND_PER_BINARY_UNIT: u32 = 100;

/// The units of the bands of [`Ladder::Millis`] stepping as a clock does:
/// ten seconds, followed by the minute, and ten minutes, by the hour.
const CLOCK_BANDS: [u32; 2] = [10 * SECOND_MS, 10 * MINUTE_MS];

impl Ladder {
    /// The unit of each band of ticks, ascending, with the multiples of it
    /// the band's ticks fall on.
    fn bands(self) -> impl Iterator<Item = (u32, [u32; 3])> {
        let next = move |&unit: &u32| match self {
            Ladder::Millis if unit == CLOCK_BANDS[0] => Some(MINUTE_MS),
            Ladder::Millis if unit == CLOCK_BANDS[1] => Some(HOUR_MS),
            Ladder::Kibibytes => {
                let binary = binary_unit(unit);
                match binary.checked_mul(LAST_BAND_PER_BINARY_UNIT) == Some(unit) {
                    true => binary.checked_mul(PER_BINARY_UNIT),
                    false => unit.checked_mul(10),
                }
            }
            Ladder::Millis | Ladder::Counts => unit.checked_mul(10),
        };
        std::iter::successors(Some(1u32), next).map(move |unit| {
            let steps = match self == Ladder::Millis && CLOCK_BANDS.contains(&unit) {
                true => CLOCK_STEPS,
                false => DECADE_STEPS,
            };
            (unit, steps)
        })
    }

    /// Every tick past zero a `u32` holds, ascending.
    fn rungs(self) -> impl Iterator<Item = Tick> {
        self.bands()
            .flat_map(|(unit, steps)| {
                steps.map(|step| {
                    let value = unit.checked_mul(step)?;
                    Some(Tick {
                        value,
                        major: step == 1,
                    })
                })
            })
            .map_while(|tick| tick)
    }
}

/// The largest binary unit, in kibibytes, at or below `kibibytes`, which is
/// at least one.
fn binary_unit(kibibytes: u32) -> u32 {
    let mut unit: u32 = 1;
    while let Some(next) = unit
        .checked_mul(PER_BINARY_UNIT)
        .filter(|&next| next <= kibibytes)
    {
        unit = next;
    }
    unit
}

impl LogScale {
    /// An axis whose ticks fall on `ladder`, reaching at least `max`, and one
    /// tick even where `max` is zero.
    pub(crate) fn covering(max: u32, ladder: Ladder) -> Self {
        let top = ladder
            .rungs()
            .map(|tick| tick.value)
            .find(|&value| value >= max)
            .unwrap_or(u32::MAX);
        Self { top, ladder }
    }

    /// Where `n` falls along the axis, from 0 at the origin to 1 at its far end.
    pub(crate) fn fraction(self, n: u32) -> f32 {
        (log_count(n) / log_count(self.top)) as f32
    }

    /// The narrowest share of the axis a zoom shows.
    fn narrowest(self) -> f32 {
        (MIN_SPAN_DECADES / log_count(self.top)).min(1.0) as f32
    }

    /// The ticks from zero to the far end, ascending.
    pub(crate) fn ticks(self) -> Vec<Tick> {
        let zero = Tick {
            value: 0,
            major: true,
        };
        let past_zero = self
            .ladder
            .rungs()
            .take_while(|tick| tick.value <= self.top);
        std::iter::once(zero).chain(past_zero).collect()
    }
}

fn log_count(n: u32) -> f64 {
    (f64::from(n) + 1.0).log10()
}

/// How a tick's count reads: a thousand and a million shortened to `k` and `M`.
pub(crate) fn tick_label(value: u32) -> String {
    const THOUSAND: u32 = 1_000;
    const MILLION: u32 = 1_000_000;
    match value {
        v if v >= MILLION && v % MILLION == 0 => format!("{}M", v / MILLION),
        v if v >= THOUSAND && v % THOUSAND == 0 => format!("{}k", v / THOUSAND),
        v => v.to_string(),
    }
}

/// The radius of the mark for a point of size zero.
pub(crate) const MIN_RADIUS: f32 = 2.5;
/// The radius of the mark for the largest point.
pub(crate) const MAX_RADIUS: f32 = 8.0;

/// One point as the plot draws it, at `x` along the horizontal axis's scale
/// and `y` along the vertical one's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Mark {
    pub(crate) target: Metrics,
    pub(crate) x: u32,
    pub(crate) y: u32,
    /// What the overlays place on the point.
    pub(crate) reported: Reported,
    pub(crate) radius: f32,
    /// Whether the filter draws the point as no part of what it selects.
    pub(crate) faded: bool,
    pub(crate) hue: Hue,
}

/// The marks a measurement's points draw as, and the axes that hold them. The
/// mark sizes are scaled to every point plotted of it and of a reference
/// measure, drawn or not, and each axis to every such point with a measure on
/// it, whether or not it has one on the other, so neither a filter, nor
/// measuring over fewer of the inputs the reference reads, nor what the other
/// axis measures moves the scale a mark is read on. A point is plotted only
/// where it has a measure on both axes.
pub(crate) struct Plotted {
    /// The faded before the rest, so none hides a mark the filter selects,
    /// within each those whose hue recedes before the rest, and largest first
    /// within those, so a small mark is drawn over any large one it overlaps.
    pub(crate) marks: Vec<Mark>,
    /// How many points are plotted, of which `marks` draws those the filter
    /// does not hide.
    pub(crate) plotted: usize,
    /// How many points the filter does not hide are left unplotted for want
    /// of a measure on an axis.
    pub(crate) unplaced: usize,
    /// Whether any count measured is an estimate.
    pub(crate) estimated: bool,
    /// What a mark's size stands for.
    pub(crate) sizing: Sizing,
    pub(crate) axes: Axes,
    pub(crate) x: LogScale,
    pub(crate) y: LogScale,
}

impl Plotted {
    /// The marks of those points `measured` that `plots` takes, placed by
    /// `axes` with what `reported` says the overlays place on a node, as the filter whose
    /// verdict on a node `verdict` gives draws them, in the hue `hue` gives,
    /// scaled to `measured` and `reference` alike.
    pub(crate) fn of(
        measured: &Measurement,
        reference: &Measurement,
        axes: Axes,
        reported: impl Fn(Id) -> Reported,
        plots: impl Fn(&Metrics) -> bool,
        verdict: impl Fn(Id) -> Verdict,
        hue: impl Fn(&Metrics) -> Hue,
    ) -> Self {
        let (targets, sizing) = (&measured.points, measured.sizing);
        let place = |t: &Metrics| {
            let reported = reported(t.id);
            let (x, y) = axes.place(t, reported)?;
            Some((x, y, reported))
        };
        let scaled = || targets.iter().chain(&reference.points).filter(|t| plots(t));
        let largest = scaled().map(|t| sizing.of(t)).max().unwrap_or(0);
        let top = |axis: Axis| {
            let values = scaled().filter_map(|t| axis.position(t, reported(t.id)));
            LogScale::covering(values.max().unwrap_or(0), axis.ladder())
        };
        let (mut plotted, mut unplaced) = (0, 0);
        let mut marks: Vec<Mark> = Vec::with_capacity(targets.len());
        marks.extend(targets.iter().filter(|t| plots(t)).filter_map(|&target| {
            let verdict = verdict(target.id);
            let Some((x, y, reported)) = place(&target) else {
                unplaced += usize::from(verdict != Verdict::Hidden);
                return None;
            };
            plotted += 1;
            let faded = match verdict {
                Verdict::Hidden => return None,
                Verdict::Faded => true,
                // The graph draws a target kept only to frame what it holds as
                // it draws one the filter selects.
                Verdict::Shown | Verdict::Context => false,
            };
            Some(Mark {
                target,
                x,
                y,
                reported,
                radius: radius(sizing.of(&target), largest),
                faded,
                hue: hue(&target),
            })
        }));
        marks.sort_by_key(|mark| {
            let size = std::cmp::Reverse(sizing.of(&mark.target));
            (!mark.faded, !mark.hue.recedes(), size)
        });
        Self {
            marks,
            plotted,
            unplaced,
            estimated: measured.estimated(),
            sizing,
            axes,
            x: top(axes.x()),
            y: top(axes.y()),
        }
    }
}

/// The radius of the mark for a point of size `size`, where the largest has
/// `largest`. Its area grows with the logarithm of the size, so the few largest
/// points stand out without burying the rest.
fn radius(size: u32, largest: u32) -> f32 {
    let share = match largest {
        0 => 0.0,
        _ => (log_count(size) / log_count(largest)).sqrt() as f32,
    };
    MIN_RADIUS + (MAX_RADIUS - MIN_RADIUS) * share
}

/// The narrowest stretch of an axis a zoom shows, in decades of `1 + n`:
/// twice the widest gap between neighbouring ticks, `log10(5 / 2)`, which the
/// step from twice to five times a power of ten approaches, so at least two
/// ticks are always in view.
const MIN_SPAN_DECADES: f64 = 2.0 * (1.0 - 2.0 * std::f64::consts::LOG10_2);

/// The part of an axis the plot shows, from `lo` for `len`, both as fractions
/// of the whole axis from its origin: always within the axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Span {
    lo: f32,
    len: f32,
}

impl Span {
    pub(crate) const WHOLE: Span = Span { lo: 0.0, len: 1.0 };

    fn new(lo: f32, len: f32) -> Self {
        let len = len.min(1.0);
        Span {
            lo: lo.clamp(0.0, 1.0 - len),
            len,
        }
    }

    /// Where the axis fraction `f` falls across the span, from 0 at its start
    /// to 1 at its end.
    fn across(self, f: f32) -> f32 {
        (f - self.lo) / self.len
    }

    /// The fraction of the axis that lies `across` this span.
    fn along(self, across: f32) -> f32 {
        self.lo + across * self.len
    }

    /// The span `factor` times narrower, a factor below one widening it, but
    /// no narrower than `narrowest`, about the point `at` across it, which
    /// stays where it was.
    fn zoomed(self, at: f32, factor: f32, narrowest: f32) -> Self {
        let len = (self.len / factor).clamp(narrowest.min(self.len), 1.0);
        Span::new(self.lo + at * (self.len - len), len)
    }

    /// The span showing the stretch of the axis from `lo` to `hi` within the
    /// part of it from `from` to `to` across it: this span where it does so
    /// clear of [`ONTO_MARGIN`] at either end of that part, and otherwise
    /// one as long, or longer where the stretch needs it, centring the
    /// stretch there.
    fn onto(self, lo: f32, hi: f32, from: f32, to: f32) -> Self {
        let room = to - from;
        let inner = |span: Span| {
            let margin = ONTO_MARGIN * room;
            (span.along(from + margin), span.along(to - margin))
        };
        let (start, end) = inner(self);
        if room <= 0.0 || (start <= lo && hi <= end) {
            return self;
        }
        let needed = (hi - lo) / (room * (1.0 - 2.0 * ONTO_MARGIN));
        let len = self.len.max(needed);
        let centre = (lo + hi) / 2.0;
        Span::new(centre - (from + room / 2.0) * len, len)
    }

    /// The span slid `by` of its own length toward the axis's far end.
    fn panned(self, by: f32) -> Self {
        Span::new(self.lo + by * self.len, self.len)
    }
}

/// The share of the room a jump lands in that its marks stay clear of at
/// either end, so none lands against an edge.
const ONTO_MARGIN: f32 = 0.15;

/// The part of each axis the plot shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Zoom {
    pub(crate) x: Span,
    pub(crate) y: Span,
}

impl Zoom {
    pub(crate) const WHOLE: Zoom = Zoom {
        x: Span::WHOLE,
        y: Span::WHOLE,
    };
}

/// The region values are placed in, inset from the axes by the largest mark's
/// radius so that, at the whole of each axis, no mark crosses them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DataArea {
    bounds: Bounds<Pixels>,
    /// The frame the axes are drawn along, outside which nothing is drawn.
    frame: Bounds<Pixels>,
    x: LogScale,
    y: LogScale,
    zoom: Zoom,
}

impl DataArea {
    /// The data region within the frame `axes` draws its axes along, showing
    /// the part of each axis `zoom` names.
    pub(crate) fn within(axes: Bounds<Pixels>, x: LogScale, y: LogScale, zoom: Zoom) -> Self {
        let inset = px(MAX_RADIUS);
        let bounds = Bounds::from_corners(
            point(axes.left() + inset, axes.top() + inset),
            point(axes.right() - inset, axes.bottom() - inset),
        );
        Self {
            bounds,
            frame: axes,
            x,
            y,
            zoom,
        }
    }

    pub(crate) fn frame(&self) -> Bounds<Pixels> {
        self.frame
    }

    /// The horizontal position of `n` along the horizontal axis.
    pub(crate) fn x_of(&self, n: u32) -> Pixels {
        let across = self.zoom.x.across(self.x.fraction(n));
        self.bounds.left() + self.bounds.size.width * across
    }

    /// The vertical position of `n` along the vertical axis, rising up the
    /// plot.
    pub(crate) fn y_of(&self, n: u32) -> Pixels {
        let across = self.zoom.y.across(self.y.fraction(n));
        self.bounds.bottom() - self.bounds.size.height * across
    }

    /// Where `n` falls along the horizontal axis, where that is within the
    /// frame.
    pub(crate) fn visible_x(&self, n: u32) -> Option<Pixels> {
        let x = self.x_of(n);
        (self.frame.left()..=self.frame.right())
            .contains(&x)
            .then_some(x)
    }

    /// Where `n` falls along the vertical axis, where that is within the
    /// frame.
    pub(crate) fn visible_y(&self, n: u32) -> Option<Pixels> {
        let y = self.y_of(n);
        (self.frame.top()..=self.frame.bottom())
            .contains(&y)
            .then_some(y)
    }

    /// The zoom `factor` times closer, a factor below one farther out, about
    /// the point under `at`, which stays under it.
    pub(crate) fn zoomed(&self, at: Point<Pixels>, factor: f32) -> Zoom {
        let across_x = (at.x - self.bounds.left()) / self.bounds.size.width;
        let across_y = (self.bounds.bottom() - at.y) / self.bounds.size.height;
        Zoom {
            x: self.zoom.x.zoomed(across_x, factor, self.x.narrowest()),
            y: self.zoom.y.zoomed(across_y, factor, self.y.narrowest()),
        }
    }

    /// The zoom carried along with a drag by `by`, so the values under the
    /// pointer follow it.
    pub(crate) fn panned(&self, by: Point<Pixels>) -> Zoom {
        Zoom {
            x: self.zoom.x.panned(-(by.x / self.bounds.size.width)),
            y: self.zoom.y.panned(by.y / self.bounds.size.height),
        }
    }

    /// The zoom showing every one of `marks` in the part of the plot below
    /// the height `clear_below`, as [`Span::onto`] moves each axis; the zoom
    /// drawn where it shows them there already.
    pub(crate) fn onto<'m>(
        &self,
        marks: impl IntoIterator<Item = &'m Mark>,
        clear_below: Pixels,
    ) -> Zoom {
        let mut stretch: Option<[f32; 4]> = None;
        for mark in marks {
            let (x, y) = (self.x.fraction(mark.x), self.y.fraction(mark.y));
            let [x_lo, x_hi, y_lo, y_hi] = stretch.get_or_insert([x, x, y, y]);
            (*x_lo, *x_hi) = (x_lo.min(x), x_hi.max(x));
            (*y_lo, *y_hi) = (y_lo.min(y), y_hi.max(y));
        }
        let Some([x_lo, x_hi, y_lo, y_hi]) = stretch else {
            return self.zoom;
        };
        let height = self.bounds.size.height;
        let below = (clear_below - self.bounds.top()).max(px(0.));
        let top = match height > px(0.) {
            true => (1.0 - below / height).max(0.0),
            false => 1.0,
        };
        Zoom {
            x: self.zoom.x.onto(x_lo, x_hi, 0.0, 1.0),
            y: self.zoom.y.onto(y_lo, y_hi, 0.0, top),
        }
    }

    /// Where on the axes a pointer at `at` lies.
    pub(crate) fn spot(&self, at: Point<Pixels>) -> Spot {
        let across_x = (at.x - self.bounds.left()) / self.bounds.size.width;
        let across_y = (self.bounds.bottom() - at.y) / self.bounds.size.height;
        Spot {
            x: self.zoom.x.along(across_x),
            y: self.zoom.y.along(across_y),
        }
    }

    /// Where `spot` is drawn.
    pub(crate) fn at_spot(&self, spot: Spot) -> Point<Pixels> {
        point(
            self.bounds.left() + self.bounds.size.width * self.zoom.x.across(spot.x),
            self.bounds.bottom() - self.bounds.size.height * self.zoom.y.across(spot.y),
        )
    }

    /// Where a point at `x` along the horizontal axis and `y` along the
    /// vertical one is drawn.
    pub(crate) fn point_at(&self, x: u32, y: u32) -> Point<Pixels> {
        point(self.x_of(x), self.y_of(y))
    }

    pub(crate) fn place(&self, mark: &Mark) -> Point<Pixels> {
        self.point_at(mark.x, mark.y)
    }

    /// What a pointer at `at` over `marks` picks: every mark whose rim lies
    /// within [`PICK_SLOP`] of it and whose centre lies within the frame, and
    /// among those the one whose centre is nearest, the one drawn over the
    /// others among equally near ones.
    pub(crate) fn pick(&self, marks: &[Mark], at: Point<Pixels>) -> Option<Picked> {
        let mut covered = Vec::with_capacity(COVERED_RESERVE);
        let mut nearest: Option<(usize, f32)> = None;
        for (index, mark) in marks.iter().enumerate() {
            let center = self.place(mark);
            let distance = f32::from(center.x - at.x).hypot(f32::from(center.y - at.y));
            if distance > mark.radius + PICK_SLOP || !self.frame.contains(&center) {
                continue;
            }
            covered.push(index);
            if nearest.is_none_or(|(_, best)| distance <= best) {
                nearest = Some((index, distance));
            }
        }
        let (mark, _) = nearest?;
        Some(Picked { mark, covered })
    }
}

/// A place on the plot by the fraction of each axis's scale it lies at, which
/// holds however large the plot is drawn and whatever part of it is shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Spot {
    x: f32,
    y: f32,
}

/// How far past a mark's rim a pointer still picks it, so that the smallest
/// marks are not a pixel-hunt.
pub(crate) const PICK_SLOP: f32 = 4.0;

/// Room reserved for the marks under a pointer, which holds those of most
/// crowded spots without growing.
const COVERED_RESERVE: usize = 16;

/// The marks under a pointer, by their indices among the plotted marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Picked {
    /// The mark a tooltip names.
    pub(crate) mark: usize,
    /// Every mark under the pointer, `mark` among them, in drawing order: each
    /// point a click there could mean.
    covered: Vec<usize>,
}

impl Picked {
    /// The marks `covered`, by their indices among the plotted marks, as a
    /// pointer naming the first of them would pick them; none where there
    /// are none.
    pub(crate) fn of(covered: Vec<usize>) -> Option<Self> {
        let mark = *covered.first()?;
        Some(Picked { mark, covered })
    }

    pub(crate) fn covered(&self) -> &[usize] {
        &self.covered
    }

    /// How many marks under the pointer are not the one it names.
    pub(crate) fn others(&self) -> usize {
        self.covered.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::metrics::{Count, Scope, Unit};
    use bviz_graph_layout::{InputSet, Kind};
    use gpui::size;
    use std::time::Duration;

    fn target(closure: u32, blast_radius: u32, fan_in: u32) -> Metrics {
        Metrics {
            id: Id(closure),
            kind: Kind::Target,
            closure: Count::Exact(closure),
            blast_radius: Count::Exact(blast_radius),
            fan_in,
            fan_out: 0,
            actions: 0,
        }
    }

    const AXES: Axes = Axes::OPENING;

    /// Where `target` is drawn through `area`.
    fn drawn_at(area: &DataArea, target: &Metrics) -> Point<Pixels> {
        area.point_at(target.closure.value(), target.blast_radius.value())
    }

    fn unreported(_: Id) -> Reported {
        Reported::default()
    }

    fn no_hue(_: &Metrics) -> Hue {
        Hue::Other
    }

    /// `targets` as measured, sized by fan-in.
    fn measured(targets: &[Metrics]) -> Measurement {
        Measurement {
            scope: Scope::new(Unit::Target, InputSet::ALL).unwrap(),
            points: targets.to_vec(),
            sizing: Sizing::FanIn,
        }
    }

    /// The marks of `targets` through a filter drawing every one.
    fn shown(targets: &[Metrics]) -> Plotted {
        let measured = measured(targets);
        Plotted::of(
            &measured,
            &measured,
            AXES,
            unreported,
            |_| true,
            |_| Verdict::Shown,
            no_hue,
        )
    }

    #[test]
    fn an_axis_ends_on_the_first_step_past_its_largest_count() {
        let top = |max| LogScale::covering(max, Ladder::Counts).top;
        assert_eq!(
            [top(0), top(1), top(3), top(20), top(21), top(999)],
            [1, 1, 5, 20, 50, 1_000]
        );
        assert_eq!(top(u32::MAX), u32::MAX);
    }

    #[test]
    fn zero_sits_at_the_origin_and_the_top_at_the_end() {
        let scale = LogScale::covering(700, Ladder::Counts);
        assert_eq!(scale.fraction(0), 0.0);
        assert_eq!(scale.fraction(1_000), 1.0);
        let decade = |n: u32| scale.fraction(n * 10 - 1) - scale.fraction(n - 1);
        assert!(
            (decade(10) - decade(100)).abs() < 1e-6,
            "decades differ in length"
        );
    }

    #[test]
    fn ticks_mark_each_decade_with_minors_between() {
        let ticks: Vec<(u32, bool)> = LogScale::covering(150, Ladder::Counts)
            .ticks()
            .into_iter()
            .map(|t| (t.value, t.major))
            .collect();
        assert_eq!(
            ticks,
            [
                (0, true),
                (1, true),
                (2, false),
                (5, false),
                (10, true),
                (20, false),
                (50, false),
                (100, true),
                (200, false),
            ]
        );
    }

    #[test]
    fn time_ticks_fall_on_round_durations_with_a_major_at_each_band() {
        let ticks: Vec<(u32, bool)> = LogScale::covering(2 * HOUR_MS, Ladder::Millis)
            .ticks()
            .into_iter()
            .filter(|t| t.value >= SECOND_MS)
            .map(|t| (t.value, t.major))
            .collect();
        let (s, m, h) = (SECOND_MS, MINUTE_MS, HOUR_MS);
        assert_eq!(
            ticks,
            [
                (s, true),
                (2 * s, false),
                (5 * s, false),
                (10 * s, true),
                (20 * s, false),
                (30 * s, false),
                (m, true),
                (2 * m, false),
                (5 * m, false),
                (10 * m, true),
                (20 * m, false),
                (30 * m, false),
                (h, true),
                (2 * h, false),
            ]
        );
    }

    #[test]
    fn byte_ticks_step_within_each_binary_unit_and_start_a_band_at_the_next() {
        const MIB: u32 = 1 << 10;
        let ticks: Vec<(u32, bool)> = LogScale::covering(3 * MIB, Ladder::Kibibytes)
            .ticks()
            .into_iter()
            .map(|t| (t.value, t.major))
            .collect();
        assert_eq!(
            ticks,
            [
                (0, true),
                (1, true),
                (2, false),
                (5, false),
                (10, true),
                (20, false),
                (50, false),
                (100, true),
                (200, false),
                (500, false),
                (MIB, true),
                (2 * MIB, false),
                (5 * MIB, false),
            ]
        );
        let top = LogScale::covering(u32::MAX, Ladder::Kibibytes).top;
        assert_eq!(top, u32::MAX, "a scale past the last tick a u32 holds");
        assert_eq!(binary_unit(MIB * MIB + 1), MIB * MIB);
    }

    #[test]
    fn tick_labels_shorten_round_thousands_and_millions() {
        let labels: Vec<String> = [0, 5, 100, 1_000, 20_000, 2_000_000, 1_500]
            .map(tick_label)
            .into();
        assert_eq!(labels, ["0", "5", "100", "1k", "20k", "2M", "1500"]);
    }

    #[test]
    fn marks_grow_with_fan_in_within_their_bounds_and_draw_largest_first() {
        let plotted = shown(&[target(1, 1, 0), target(1, 1, 100), target(1, 1, 3)]);
        let radii: Vec<f32> = plotted.marks.iter().map(|m| m.radius).collect();
        assert_eq!(radii.first(), Some(&MAX_RADIUS));
        assert_eq!(radii.last(), Some(&MIN_RADIUS));
        assert!(radii.windows(2).all(|pair| pair[0] >= pair[1]));
    }

    #[test]
    fn no_fan_in_anywhere_draws_every_mark_at_the_smallest_size() {
        let plotted = shown(&[target(0, 0, 0), target(4, 2, 0)]);
        assert!(plotted.marks.iter().all(|m| m.radius == MIN_RADIUS));
    }

    #[test]
    fn every_mark_stays_clear_of_the_axes() {
        let targets = [target(0, 0, 9), target(4_000, 17, 9), target(3, 90_000, 9)];
        let plotted = shown(&targets);
        let axes = Bounds::new(point(px(40.), px(10.)), size(px(300.), px(200.)));
        let area = DataArea::within(axes, plotted.x, plotted.y, Zoom::WHOLE);
        for mark in &plotted.marks {
            let at = area.place(mark);
            let r = px(mark.radius);
            assert!(
                at.x - r >= axes.left() && at.x + r <= axes.right(),
                "{at:?}"
            );
            assert!(
                at.y - r >= axes.top() && at.y + r <= axes.bottom(),
                "{at:?}"
            );
        }
    }

    #[test]
    fn a_pointer_picks_the_nearest_mark_within_reach_and_covers_those_it_hides() {
        let targets = [target(0, 0, 0), target(100, 100, 0), target(100, 100, 5)];
        let plotted = shown(&targets);
        let axes = Bounds::new(point(px(0.), px(0.)), size(px(400.), px(300.)));
        let area = DataArea::within(axes, plotted.x, plotted.y, Zoom::WHOLE);
        let origin = drawn_at(&area, &targets[0]);
        let picked = area.pick(&plotted.marks, origin + point(px(1.), px(-1.)));
        let origin_mark = plotted.marks.iter().position(|m| m.target == targets[0]);
        assert_eq!(picked.as_ref().map(|p| p.mark), origin_mark);
        assert_eq!(picked.map(|p| p.others()), Some(0));

        let shared = area.pick(&plotted.marks, drawn_at(&area, &targets[1]));
        // The mark drawn last is the one on top.
        let top = plotted
            .marks
            .iter()
            .rposition(|m| (m.target.closure.value(), m.target.blast_radius.value()) == (100, 100));
        assert_eq!(shared.as_ref().map(|p| p.mark), top);
        assert_eq!(shared.map(|p| p.others()), Some(1));

        let far = origin + point(px(MAX_RADIUS + PICK_SLOP + 1.), px(0.));
        assert_eq!(area.pick(&plotted.marks, far), None);
    }

    /// A click can mean any mark it lands on, so the pointer covers each mark
    /// within reach whether or not it sits at the very same counts, and none
    /// past reach or hidden by the filter.
    #[test]
    fn a_pointer_covers_every_drawn_mark_within_reach() {
        let mut targets = [
            target(100, 100, 0),
            target(101, 100, 0),
            target(100, 102, 0),
            target(100, 900, 0),
            target(99, 100, 0),
        ];
        for (index, target) in targets.iter_mut().enumerate() {
            target.id = Id(index as u32);
        }
        let hidden = targets[4].id;
        let measured = measured(&targets);
        let verdict = |id| match id == hidden {
            true => Verdict::Hidden,
            false => Verdict::Shown,
        };
        let plotted = Plotted::of(
            &measured,
            &measured,
            AXES,
            unreported,
            |_| true,
            verdict,
            no_hue,
        );
        let axes = Bounds::new(point(px(0.), px(0.)), size(px(400.), px(300.)));
        let area = DataArea::within(axes, plotted.x, plotted.y, Zoom::WHOLE);
        let picked = area
            .pick(&plotted.marks, drawn_at(&area, &targets[0]))
            .unwrap();
        let mut covered: Vec<Id> = picked
            .covered
            .iter()
            .map(|&i| plotted.marks[i].target.id)
            .collect();
        covered.sort();
        assert_eq!(covered, [targets[0].id, targets[1].id, targets[2].id]);
        assert_eq!(picked.others(), 2);
    }

    #[test]
    fn a_filter_draws_what_it_keeps_on_axes_scaled_to_every_target() {
        let targets = [target(3, 1, 9), target(400, 2, 0), target(7, 90, 2)];
        let hidden = targets[1].id;
        let faded = targets[2].id;
        let measured = measured(&targets);
        let plotted = Plotted::of(
            &measured,
            &measured,
            AXES,
            unreported,
            |_| true,
            |id| match id {
                id if id == hidden => Verdict::Hidden,
                id if id == faded => Verdict::Faded,
                _ => Verdict::Shown,
            },
            no_hue,
        );
        let drawn: Vec<(Id, bool)> = plotted
            .marks
            .iter()
            .map(|m| (m.target.id, m.faded))
            .collect();
        assert_eq!(drawn, [(faded, true), (targets[0].id, false)]);
        assert_eq!(plotted.plotted, targets.len());
        let whole = shown(&targets);
        assert_eq!((plotted.x, plotted.y), (whole.x, whole.y));
        let radius_of = |plotted: &Plotted, id: Id| {
            plotted
                .marks
                .iter()
                .find(|m| m.target.id == id)
                .map(|m| m.radius)
        };
        assert_eq!(radius_of(&plotted, faded), radius_of(&whole, faded));
    }

    #[test]
    fn a_point_left_unplotted_scales_neither_the_axes_nor_the_sizes() {
        let targets = [target(3, 900, 9), target(400, 2, 1)];
        let unplotted = targets[0].id;
        let measured = measured(&targets);
        let plotted = Plotted::of(
            &measured,
            &measured,
            AXES,
            unreported,
            |t| t.id != unplotted,
            |_| Verdict::Shown,
            no_hue,
        );
        assert_eq!(plotted.plotted, 1);
        assert_eq!(plotted.marks.len(), 1);
        let alone = shown(&targets[1..]);
        assert_eq!((plotted.x, plotted.y), (alone.x, alone.y));
        assert_eq!(plotted.marks[0].radius, MAX_RADIUS);
    }

    /// Leaving an input out shrinks the counts, and the marks move down axes
    /// that keep the span of the whole measure.
    #[test]
    fn a_narrower_measure_is_drawn_on_the_scale_of_its_reference() {
        let whole = measured(&[target(3, 1, 1), target(400, 90, 9)]);
        let narrower = measured(&[target(3, 1, 1), target(40, 2, 3)]);
        let plotted = Plotted::of(
            &narrower,
            &whole,
            AXES,
            unreported,
            |_| true,
            |_| Verdict::Shown,
            no_hue,
        );
        let reference = Plotted::of(
            &whole,
            &whole,
            AXES,
            unreported,
            |_| true,
            |_| Verdict::Shown,
            no_hue,
        );
        assert_eq!((plotted.x, plotted.y), (reference.x, reference.y));
        let largest =
            |plotted: &Plotted| plotted.marks.iter().map(|m| m.radius).fold(0.0, f32::max);
        assert_eq!(largest(&reference), MAX_RADIUS);
        assert!(largest(&plotted) < MAX_RADIUS);
    }

    #[test]
    fn a_zoom_holds_the_point_it_is_taken_about_and_stays_within_the_axis() {
        const NARROWEST: f32 = 0.05;
        let span = Span::WHOLE.zoomed(0.25, 4.0, NARROWEST);
        assert!((span.len - 0.25).abs() < 1e-6);
        assert!((span.across(0.25) - 0.25).abs() < 1e-6);
        let wider = span.zoomed(0.5, 0.1, NARROWEST);
        assert_eq!(wider, Span::WHOLE);
        assert_eq!(Span::WHOLE.zoomed(0.5, 1e6, NARROWEST).len, NARROWEST);
        assert_eq!(span.panned(-10.0).lo, 0.0);
        assert!((span.panned(10.0).lo - 0.75).abs() < 1e-6);
    }

    /// A stretch already well inside the span leaves it be; one outside it,
    /// or against its edge, is centred in the room, the span widened only
    /// where the stretch needs more than it shows.
    #[test]
    fn a_jump_moves_the_span_only_as_far_as_it_must() {
        let span = Span::new(0.25, 0.25);
        assert_eq!(span.onto(0.35, 0.4, 0.0, 1.0), span);
        let slid = span.onto(0.8, 0.8, 0.0, 1.0);
        assert!((slid.len - 0.25).abs() < 1e-6);
        assert!((slid.across(0.8) - 0.5).abs() < 1e-6);
        let wide = span.onto(0.3, 0.6, 0.0, 1.0);
        assert!(wide.len > 0.4, "{wide:?}");
        assert!(wide.across(0.3) >= ONTO_MARGIN - 1e-6, "{wide:?}");
        assert!(wide.across(0.6) <= 1.0 - ONTO_MARGIN + 1e-6, "{wide:?}");
        let below = span.onto(0.8, 0.8, 0.0, 0.5);
        assert!((below.across(0.8) - 0.25).abs() < 1e-6, "{below:?}");
        assert_eq!(Span::WHOLE.onto(0.0, 1.0, 0.0, 1.0), Span::WHOLE);
    }

    #[test]
    fn the_deepest_zoom_keeps_two_ticks_in_view_anywhere_on_the_axis() {
        let scales = [1, 7, 150, 90_000]
            .map(|top| LogScale::covering(top, Ladder::Counts))
            .into_iter()
            .chain([45_000, 7_200_000].map(|top| LogScale::covering(top, Ladder::Millis)));
        for scale in scales {
            let top = scale.top;
            let values: Vec<f32> = scale
                .ticks()
                .iter()
                .map(|t| scale.fraction(t.value))
                .collect();
            let narrowest = scale.narrowest();
            let steps = 100;
            for step in 0..=steps {
                let lo = (1.0 - narrowest) * step as f32 / steps as f32;
                let in_view = values
                    .iter()
                    .filter(|&&f| f >= lo - 1e-6 && f <= lo + narrowest + 1e-6)
                    .count();
                assert!(in_view >= 2, "top {top}, from {lo}: {in_view} ticks");
            }
        }
    }

    #[test]
    fn a_zoomed_plot_keeps_the_counts_under_the_pointer_and_picks_none_it_clips() {
        let targets = [target(0, 0, 0), target(1_000, 1_000, 0), target(30, 30, 0)];
        let plotted = shown(&targets);
        let axes = Bounds::new(point(px(0.), px(0.)), size(px(400.), px(300.)));
        let whole = DataArea::within(axes, plotted.x, plotted.y, Zoom::WHOLE);
        let at = drawn_at(&whole, &targets[2]);
        let zoom = whole.zoomed(at, 3.0);
        let zoomed = DataArea::within(axes, plotted.x, plotted.y, zoom);
        let moved = drawn_at(&zoomed, &targets[2]) - at;
        assert!(f32::from(moved.x).abs() < 0.01 && f32::from(moved.y).abs() < 0.01);
        let corner = drawn_at(&zoomed, &targets[1]);
        assert!(!axes.contains(&corner));
        assert_eq!(zoomed.pick(&plotted.marks, corner), None);
    }

    #[test]
    fn more_blast_radius_places_a_mark_higher() {
        let plotted = shown(&[target(0, 0, 0), target(0, 50, 0)]);
        let axes = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
        let area = DataArea::within(axes, plotted.x, plotted.y, Zoom::WHOLE);
        assert!(area.y_of(50) < area.y_of(0));
        assert_eq!(area.x_of(0), axes.left() + px(MAX_RADIUS));
    }

    /// A spot taken on a zoomed plot is drawn over the same counts once the
    /// plot is drawn at another size.
    #[test]
    fn a_spot_follows_its_counts_as_the_plot_is_resized() {
        let targets = [target(0, 0, 0), target(1_000, 1_000, 0), target(30, 7, 0)];
        let plotted = shown(&targets);
        let wide = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(500.)));
        let zoom = DataArea::within(wide, plotted.x, plotted.y, Zoom::WHOLE)
            .zoomed(point(px(300.), px(200.)), 2.0);
        let before = DataArea::within(wide, plotted.x, plotted.y, zoom);
        let spot = before.spot(drawn_at(&before, &targets[2]));
        let narrow = Bounds::new(point(px(40.), px(60.)), size(px(320.), px(700.)));
        let after = DataArea::within(narrow, plotted.x, plotted.y, zoom);
        let moved = after.at_spot(spot) - drawn_at(&after, &targets[2]);
        assert!(f32::from(moved.x).abs() < 0.01 && f32::from(moved.y).abs() < 0.01);
    }

    /// A point no action time lands on has no place on a time axis, which is
    /// scaled to the timed points of the reference alone, but still scales the
    /// other axis and the mark sizes, so neither moves as the time axis is
    /// chosen.
    #[test]
    fn a_time_axis_leaves_out_and_counts_the_untimed_points() {
        let mut targets = [target(3, 1, 2), target(5, 20, 9), target(7, 3, 0)];
        for (index, target) in targets.iter_mut().enumerate() {
            target.id = Id(index as u32);
        }
        let reference = measured(&targets);
        let measured = measured(&targets[..2]);
        let time = |id: Id| Reported {
            time: match id.0 {
                0 => Some(Duration::from_millis(40)),
                2 => Some(Duration::from_secs(9)),
                _ => None,
            },
            bytes: None,
        };
        let axes = AXES.across(Axis::ActionTime);
        let plotted = Plotted::of(
            &measured,
            &reference,
            axes,
            time,
            |_| true,
            |_| Verdict::Shown,
            no_hue,
        );
        let drawn: Vec<(Id, u32)> = plotted.marks.iter().map(|m| (m.target.id, m.x)).collect();
        assert_eq!(drawn, [(Id(0), 40)]);
        assert_eq!((plotted.plotted, plotted.unplaced), (1, 1));
        assert_eq!(plotted.x, LogScale::covering(9_000, Ladder::Millis));
        let hiding_the_untimed = Plotted::of(
            &measured,
            &reference,
            axes,
            time,
            |_| true,
            |id| match id.0 {
                1 => Verdict::Hidden,
                _ => Verdict::Shown,
            },
            no_hue,
        );
        assert_eq!(
            hiding_the_untimed.unplaced, 0,
            "only a drawn point is left out"
        );
        let by_closure = Plotted::of(
            &measured,
            &reference,
            AXES,
            time,
            |_| true,
            |_| Verdict::Shown,
            no_hue,
        );
        assert_eq!(plotted.y, by_closure.y);
        assert_eq!(plotted.marks[0].radius, by_closure.marks[1].radius);
        assert!(plotted.marks[0].radius < MAX_RADIUS);
    }

    /// Time up is scaled as time across is: to every timed point of the
    /// reference, whatever the filter hides, while the closure across keeps
    /// the scale it has with blast radius up.
    #[test]
    fn a_time_axis_up_is_scaled_to_every_timed_point_of_the_reference() {
        let mut targets = [target(3, 1, 2), target(5, 20, 9), target(7, 3, 0)];
        for (index, target) in targets.iter_mut().enumerate() {
            target.id = Id(index as u32);
        }
        let reference = measured(&targets);
        let measured = measured(&targets[..2]);
        let time = |id: Id| Reported {
            time: match id.0 {
                0 => Some(Duration::from_millis(40)),
                2 => Some(Duration::from_secs(9)),
                _ => None,
            },
            bytes: None,
        };
        let plot = |axes, verdict: fn(Id) -> Verdict| {
            Plotted::of(&measured, &reference, axes, time, |_| true, verdict, no_hue)
        };
        let shown = |_| Verdict::Shown;
        let hiding_the_slowest = |id: Id| match id.0 {
            2 => Verdict::Hidden,
            _ => Verdict::Shown,
        };
        let by_time_up = plot(AXES.up(Axis::ActionTime), hiding_the_slowest);
        let drawn: Vec<(Id, u32, u32)> = by_time_up
            .marks
            .iter()
            .map(|m| (m.target.id, m.x, m.y))
            .collect();
        assert_eq!(drawn, [(Id(0), 3, 40)]);
        assert_eq!(by_time_up.y, LogScale::covering(9_000, Ladder::Millis));
        assert_eq!(by_time_up.y, plot(AXES.across(Axis::ActionTime), shown).x);
        let opening = plot(AXES, shown);
        assert_eq!(by_time_up.x, opening.x);
        assert_eq!(by_time_up.marks[0].radius, opening.marks[1].radius);
    }

    #[test]
    fn an_untimed_mark_is_drawn_beneath_a_timed_one_of_any_size() {
        let targets = [target(1, 1, 0), target(2, 2, 9)];
        let measured = measured(&targets);
        let hue = |t: &Metrics| match t.fan_in {
            0 => Hue::Untimed,
            _ => Hue::Other,
        };
        let plotted = Plotted::of(
            &measured,
            &measured,
            AXES,
            unreported,
            |_| true,
            |_| Verdict::Shown,
            hue,
        );
        let hues: Vec<Hue> = plotted.marks.iter().map(|m| m.hue).collect();
        assert_eq!(hues, [Hue::Untimed, Hue::Other]);
    }
}
