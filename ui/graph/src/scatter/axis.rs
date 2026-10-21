//! The measures the scatter plot places its points by.

use super::plot::{HOUR_MS, Ladder, MINUTE_MS, SECOND_MS, tick_label};
use crate::duration::lasted;
use crate::spawn_bytes::{NodeBytes, Unsized};
use crate::timing::{NodeTimes, Ran, Untimed};
use bviz_graph_layout::Id;
use bviz_graph_layout::metrics::Metrics;
use bviz_invocation::spawn::{ByteMeasure, Bytes, SpawnBytes};
use bviz_ui_components::amount;
use rustc_hash::FxHashMap;
use std::time::Duration;

/// What an axis of the plot places a point by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Axis {
    /// How much of the build the point pulls in.
    Closure,
    /// How much of the build depends on the point.
    BlastRadius,
    /// How long the actions the point stands for took, summed; placed in
    /// milliseconds.
    ActionTime,
    /// The bytes the spawns landing on the point wrote, summed; placed, as
    /// every measure of bytes is, in kibibytes.
    BytesWritten,
    /// The bytes those spawns read, their tools' among them.
    BytesRead,
    /// The bytes of those spawns' tools.
    ToolBytes,
}

/// A report on an invocation that places measures on a graph's nodes, which a
/// point can lack, and which changes as the session's report does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Overlay {
    /// The action times of the invocation's profile.
    Times,
    /// The spawn bytes of its execution log.
    Bytes,
}

/// Which [overlays](Overlay) place a measure on some point of the unit
/// plotted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Carried {
    pub(crate) times: bool,
    pub(crate) bytes: bool,
    /// The bytes count what the spawns read.
    pub(crate) inputs: bool,
}

impl Carried {
    pub(crate) fn carries(self, overlay: Overlay) -> bool {
        match overlay {
            Overlay::Times => self.times,
            Overlay::Bytes => self.bytes,
        }
    }
}

/// What the overlays place on one point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reported {
    /// How long the actions the point stands for took.
    pub(crate) time: Option<Duration>,
    /// What the spawns landing on the point wrote and read.
    pub(crate) bytes: Option<SpawnBytes>,
}

impl Axis {
    pub(crate) const ALL: [Axis; 6] = [
        Axis::Closure,
        Axis::BlastRadius,
        Axis::ActionTime,
        Axis::BytesWritten,
        Axis::BytesRead,
        Axis::ToolBytes,
    ];

    /// Where along the axis `point`, on which the overlays place `reported`,
    /// falls, in the values its scale is laid out in; absent where the point
    /// has no such measure.
    pub(crate) fn position(self, point: &Metrics, reported: Reported) -> Option<u32> {
        let capped = |value: f64| value.round().min(f64::from(u32::MAX)) as u32;
        match self {
            Axis::Closure => Some(point.closure.value()),
            Axis::BlastRadius => Some(point.blast_radius.value()),
            Axis::ActionTime => Some(capped(reported.time?.as_secs_f64() * f64::from(SECOND_MS))),
            Axis::BytesWritten | Axis::BytesRead | Axis::ToolBytes => {
                let bytes = self.bytes(reported)?;
                Some(capped(bytes.get() as f64 / amount::PER_BINARY_UNIT as f64))
            }
        }
    }

    /// The bytes the axis measures of `reported`, where it measures bytes and
    /// the point has any.
    fn bytes(self, reported: Reported) -> Option<Bytes> {
        reported.bytes?.measure(self.measure()?)
    }

    /// What the axis measures of a spawn's bytes, where it measures them.
    fn measure(self) -> Option<ByteMeasure> {
        match self {
            Axis::BytesWritten => Some(ByteMeasure::Outputs),
            Axis::BytesRead => Some(ByteMeasure::Inputs),
            Axis::ToolBytes => Some(ByteMeasure::Tools),
            Axis::Closure | Axis::BlastRadius | Axis::ActionTime => None,
        }
    }

    /// Where a picker offers the measure, in the order of [`ALL`](Self::ALL),
    /// which is the order they are declared in.
    pub(crate) fn index(self) -> usize {
        self as usize
    }

    /// The overlay the axis places a point by, where it reads one.
    pub(crate) fn overlay(self) -> Option<Overlay> {
        match self {
            Axis::Closure | Axis::BlastRadius => None,
            Axis::ActionTime => Some(Overlay::Times),
            Axis::BytesWritten | Axis::BytesRead | Axis::ToolBytes => Some(Overlay::Bytes),
        }
    }

    /// Whether a unit carrying what `carried` says has any point the axis
    /// places.
    pub(crate) fn available(self, carried: Carried) -> bool {
        let of_inputs = self.measure().is_some_and(ByteMeasure::of_inputs);
        self.overlay()
            .is_none_or(|overlay| carried.carries(overlay))
            && (!of_inputs || carried.inputs)
    }

    /// How a line of text names the measure.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Axis::Closure => "closure",
            Axis::BlastRadius => "blast radius",
            Axis::ActionTime => "action time",
            Axis::BytesWritten => amount::measure_name(ByteMeasure::Outputs),
            Axis::BytesRead => amount::measure_name(ByteMeasure::Inputs),
            Axis::ToolBytes => amount::measure_name(ByteMeasure::Tools),
        }
    }

    /// What the plot reads along the axis.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Axis::Closure => "closure size (transitive dependencies)",
            Axis::BlastRadius => "blast radius (transitive dependents)",
            Axis::ActionTime => "action time (wall time of its actions, summed)",
            Axis::BytesWritten => "bytes written (outputs of its spawns, summed)",
            Axis::BytesRead => "bytes read (inputs of its spawns, tools included, summed)",
            Axis::ToolBytes => "tool bytes (tools of its spawns, summed)",
        }
    }

    /// How the measure of `point`, on which the overlays place `reported`,
    /// reads, marked where it is an estimate.
    pub(crate) fn reading(self, point: &Metrics, reported: Reported) -> String {
        let none = || "none".to_owned();
        match self {
            Axis::Closure => point.closure.to_string(),
            Axis::BlastRadius => point.blast_radius.to_string(),
            Axis::ActionTime => reported.time.map_or_else(none, lasted),
            Axis::BytesWritten | Axis::BytesRead | Axis::ToolBytes => {
                self.bytes(reported).map_or_else(none, amount::bytes)
            }
        }
    }

    /// The values the axis's ticks fall on.
    pub(crate) fn ladder(self) -> Ladder {
        match self {
            Axis::Closure | Axis::BlastRadius => Ladder::Counts,
            Axis::ActionTime => Ladder::Millis,
            Axis::BytesWritten | Axis::BytesRead | Axis::ToolBytes => Ladder::Kibibytes,
        }
    }

    /// How a tick at `value` along the axis reads.
    pub(crate) fn tick_label(self, value: u32) -> String {
        match (self.ladder(), value) {
            (_, 0) | (Ladder::Counts, _) => tick_label(value),
            (Ladder::Millis, _) => time_tick_label(value),
            (Ladder::Kibibytes, kibibytes) => {
                amount::whole_bytes(Bytes::new(u64::from(kibibytes) * amount::PER_BINARY_UNIT))
            }
        }
    }
}

/// How a tick `millis` past zero along the time axis reads: in the largest
/// unit it reaches, which a tick of [`Ladder::Millis`] is a whole number of.
fn time_tick_label(millis: u32) -> String {
    match millis {
        _ if millis < SECOND_MS => format!("{millis}ms"),
        _ if millis < MINUTE_MS => format!("{}s", millis / SECOND_MS),
        _ if millis < HOUR_MS => format!("{}m", millis / MINUTE_MS),
        _ => format!("{}h", millis / HOUR_MS),
    }
}

/// The measures a plot places its points by: across, and up, never the same
/// measure on both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Axes {
    x: Axis,
    y: Axis,
}

impl Axes {
    /// Closure across, blast radius up.
    pub(crate) const OPENING: Axes = Axes {
        x: Axis::Closure,
        y: Axis::BlastRadius,
    };

    pub(crate) fn x(self) -> Axis {
        self.x
    }

    pub(crate) fn y(self) -> Axis {
        self.y
    }

    /// These axes with `axis` across, swapped with the measure up where that
    /// is `axis`.
    pub(crate) fn across(self, axis: Axis) -> Axes {
        match axis == self.y {
            true => Axes { x: axis, y: self.x },
            false => Axes { x: axis, ..self },
        }
    }

    /// These axes with `axis` up, by the rule [`across`](Self::across) keeps.
    pub(crate) fn up(self, axis: Axis) -> Axes {
        self.transposed().across(axis).transposed()
    }

    fn transposed(self) -> Axes {
        Axes {
            x: self.y,
            y: self.x,
        }
    }

    /// These axes with each one a unit carrying what `carried` says has no
    /// point on replaced by closure, or blast radius where the other axis
    /// shows closure, which every point has.
    pub(crate) fn available(self, carried: Carried) -> Axes {
        let spare = |other: Axis| match other {
            Axis::Closure => Axis::BlastRadius,
            _ => Axis::Closure,
        };
        let x = match self.x.available(carried) {
            true => self.x,
            false => spare(self.y),
        };
        let y = match self.y.available(carried) {
            true => self.y,
            false => spare(x),
        };
        Axes { x, y }
    }

    /// Where `point`, on which the overlays place `reported`, falls along
    /// each axis's scale; absent where it has no measure on one of them.
    pub(crate) fn place(self, point: &Metrics, reported: Reported) -> Option<(u32, u32)> {
        Some((
            self.x.position(point, reported)?,
            self.y.position(point, reported)?,
        ))
    }

    /// Whether either axis places a point by `overlay`.
    pub(crate) fn reads(self, overlay: Overlay) -> bool {
        [self.x, self.y]
            .into_iter()
            .any(|axis| axis.overlay() == Some(overlay))
    }

    /// How `point`, on which the overlays place `reported`, reads on both
    /// axes, each measure named.
    pub(crate) fn readings(self, point: &Metrics, reported: Reported) -> String {
        let reads = |axis: Axis| format!("{} {}", axis.name(), axis.reading(point, reported));
        format!("{} · {}", reads(self.x), reads(self.y))
    }

    /// How `point`, on which the overlays place `reported`, reads on every
    /// measure it has that neither axis places it by, each named; absent
    /// where it has none.
    pub(crate) fn unplaced(self, point: &Metrics, reported: Reported) -> Option<String> {
        let rest: Vec<String> = Axis::ALL
            .into_iter()
            .filter(|&axis| axis != self.x && axis != self.y)
            .filter(|axis| axis.position(point, reported).is_some())
            .map(|axis| format!("{} {}", axis.name(), axis.reading(point, reported)))
            .collect();
        (!rest.is_empty()).then(|| rest.join(" · "))
    }

    /// The names of both measures, as a header over a column of
    /// [`values`](Self::values) reads.
    pub(crate) fn names(self) -> String {
        format!("{} · {}", self.x.name(), self.y.name())
    }

    /// How `point`, on which the overlays place `reported`, reads on both
    /// axes, under a header of their [`names`](Self::names).
    pub(crate) fn values(self, point: &Metrics, reported: Reported) -> String {
        format!(
            "{} · {}",
            self.x.reading(point, reported),
            self.y.reading(point, reported)
        )
    }
}

/// What the spawns landing on each point of a measure wrote and read, of the
/// points any spawn lands on, held apart from the graph so that a plot can be
/// drawn off the UI thread.
#[derive(Debug, Default)]
pub(crate) struct Sizes(FxHashMap<Id, SpawnBytes>);

impl Sizes {
    /// The spawn bytes `bytes` places on each of `points`.
    pub(crate) fn of(bytes: Result<&NodeBytes, Unsized>, points: &[Metrics]) -> Self {
        let Ok(bytes) = bytes else {
            return Sizes::default();
        };
        let mut sizes = FxHashMap::with_capacity_and_hasher(points.len(), Default::default());
        sizes.extend(points.iter().filter_map(|point| {
            let sized = bytes.get(point.id)?;
            Some((point.id, sized.held.bytes))
        }));
        Sizes(sizes)
    }

    pub(crate) fn get(&self, id: Id) -> Option<SpawnBytes> {
        self.0.get(&id).copied()
    }
}

/// How long the actions each point of a measure stands for took and where
/// they ran, of the points any action time lands on, held apart from the
/// graph so that a plot can be drawn off the UI thread.
#[derive(Debug, Default)]
pub(crate) struct Clocks(FxHashMap<Id, Clock>);

/// How long the actions of one point took, and where they ran.
#[derive(Clone, Copy, Debug)]
struct Clock {
    wall: Duration,
    ran: Ran,
}

impl Clocks {
    /// The action time `times` places on each of `points`, and where those
    /// actions ran.
    pub(crate) fn of(times: Result<&NodeTimes, Untimed>, points: &[Metrics]) -> Self {
        let Ok(times) = times else {
            return Clocks::default();
        };
        let mut clocks = FxHashMap::with_capacity_and_hasher(points.len(), Default::default());
        clocks.extend(points.iter().filter_map(|point| {
            let timed = times.get(point.id)?;
            let clock = Clock {
                wall: timed.held.total().wall,
                ran: Ran::of(&timed.held),
            };
            Some((point.id, clock))
        }));
        Clocks(clocks)
    }

    pub(crate) fn time(&self, id: Id) -> Option<Duration> {
        Some(self.0.get(&id)?.wall)
    }

    pub(crate) fn ran(&self, id: Id) -> Option<Ran> {
        Some(self.0.get(&id)?.ran)
    }

    /// The clocks of the points `runs` names, by id, time and runner.
    #[cfg(test)]
    pub(crate) fn of_runs(runs: &[(Id, Duration, Ran)]) -> Self {
        let clocks = runs
            .iter()
            .map(|&(id, wall, ran)| (id, Clock { wall, ran }));
        Clocks(clocks.collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::Kind;
    use bviz_graph_layout::metrics::Count;
    use bviz_invocation::spawn::InputBytes;

    fn point(closure: u32, blast_radius: u32) -> Metrics {
        Metrics {
            id: Id(0),
            kind: Kind::Action,
            closure: Count::Exact(closure),
            blast_radius: Count::Estimated(blast_radius),
            fan_in: 0,
            fan_out: 0,
            actions: 1,
        }
    }

    fn timed(time: Option<Duration>) -> Reported {
        Reported { time, bytes: None }
    }

    fn sized(outputs: u64, others: u64, tools: u64) -> Reported {
        Reported {
            time: None,
            bytes: Some(SpawnBytes {
                outputs: Bytes::new(outputs),
                inputs: Some(InputBytes {
                    tools: Bytes::new(tools),
                    others: Bytes::new(others),
                    read_alone: Bytes::ZERO,
                }),
            }),
        }
    }

    #[test]
    fn action_time_places_a_point_by_its_milliseconds_and_leaves_an_untimed_one_out() {
        let at = |time| Axis::ActionTime.position(&point(3, 4), timed(time));
        assert_eq!(at(Some(Duration::from_micros(2_499_600))), Some(2_500));
        assert_eq!(at(Some(Duration::from_micros(400))), Some(0));
        assert_eq!(at(None), None);
        assert_eq!(
            Axis::Closure.position(&point(3, 4), Reported::default()),
            Some(3)
        );
    }

    #[test]
    fn a_byte_axis_places_a_point_by_the_kibibytes_it_measures() {
        let reported = sized(3 << 20, 1536, 511);
        let at = |axis: Axis| axis.position(&point(3, 4), reported);
        assert_eq!(at(Axis::BytesWritten), Some(3 << 10));
        assert_eq!(
            at(Axis::BytesRead),
            Some(2),
            "a kibibyte and a half rounds up"
        );
        assert_eq!(at(Axis::ToolBytes), Some(0));
        assert_eq!(Axis::BytesRead.position(&point(3, 4), timed(None)), None);
        assert_eq!(
            Axis::BytesWritten.position(&point(3, 4), sized(u64::MAX, 0, 0)),
            Some(u32::MAX)
        );
        assert_eq!(
            Axis::BytesWritten.reading(&point(3, 4), reported),
            "3.0 MiB"
        );
        assert_eq!(Axis::ToolBytes.reading(&point(3, 4), timed(None)), "none");
    }

    #[test]
    fn a_byte_tick_reads_as_a_whole_binary_unit() {
        let ticks = [0, 1, 500, 1 << 10, 200 << 10, 1 << 20, 5 << 20];
        let labels = [
            "0", "1 KiB", "500 KiB", "1 MiB", "200 MiB", "1 GiB", "5 GiB",
        ];
        assert_eq!(
            ticks.map(|value| Axis::BytesWritten.tick_label(value)),
            labels
        );
    }

    #[test]
    fn a_time_tick_reads_in_the_largest_unit_it_reaches() {
        let ticks = [
            0,
            5,
            500,
            SECOND_MS,
            30 * SECOND_MS,
            MINUTE_MS,
            20 * MINUTE_MS,
            HOUR_MS,
            50 * HOUR_MS,
        ];
        let labels = ["0", "5ms", "500ms", "1s", "30s", "1m", "20m", "1h", "50h"];
        assert_eq!(
            ticks.map(|value| Axis::ActionTime.tick_label(value)),
            labels
        );
    }

    #[test]
    fn a_tooltip_reads_the_plotted_measures_and_then_those_left_off_the_axes() {
        let timed = timed(Some(Duration::from_millis(3_200)));
        let by_time = Axes::OPENING.across(Axis::ActionTime);
        assert_eq!(
            by_time.readings(&point(3, 4), timed),
            "action time 3.2s · blast radius ≈4"
        );
        assert_eq!(
            by_time.unplaced(&point(3, 4), timed).as_deref(),
            Some("closure 3")
        );
        let by_closure = by_time.across(Axis::Closure);
        assert_eq!(
            by_closure.unplaced(&point(3, 4), timed).as_deref(),
            Some("action time 3.2s")
        );
        assert_eq!(by_closure.unplaced(&point(3, 4), Reported::default()), None);
        assert_eq!(
            by_closure
                .unplaced(&point(3, 4), sized(2 << 10, 0, 0))
                .as_deref(),
            Some("bytes written 2.0 KiB · bytes read 0 B · tool bytes 0 B")
        );
    }

    #[test]
    fn choosing_the_measure_the_other_axis_shows_swaps_the_axes() {
        let opening = Axes::OPENING;
        let swapped = Axes {
            x: Axis::BlastRadius,
            y: Axis::Closure,
        };
        assert_eq!(opening.across(Axis::BlastRadius), swapped);
        assert_eq!(opening.up(Axis::Closure), swapped);
        assert_eq!(opening.across(Axis::Closure), opening);
        assert_eq!(
            opening.up(Axis::ActionTime),
            Axes {
                x: Axis::Closure,
                y: Axis::ActionTime,
            }
        );
        let closure_by_time = opening.up(Axis::Closure).across(Axis::ActionTime);
        assert_eq!(
            closure_by_time,
            Axes {
                x: Axis::ActionTime,
                y: Axis::Closure,
            }
        );
        for x in Axis::ALL {
            for y in Axis::ALL {
                let axes = opening.across(x).up(y);
                assert_ne!(axes.x, axes.y);
                assert_eq!(axes.y, y);
            }
        }
    }

    #[test]
    fn an_axis_reading_an_overlay_not_carried_falls_back_to_one_every_point_has() {
        let carried = Carried {
            times: false,
            bytes: true,
            inputs: true,
        };
        let fallen = |x, y| Axes { x, y }.available(carried);
        let (closure, blast, time) = (Axis::Closure, Axis::BlastRadius, Axis::ActionTime);
        assert_eq!(
            fallen(time, blast),
            Axes {
                x: closure,
                y: blast
            }
        );
        assert_eq!(
            fallen(time, closure),
            Axes {
                x: blast,
                y: closure
            }
        );
        assert_eq!(
            fallen(closure, time),
            Axes {
                x: closure,
                y: blast
            }
        );
        assert_eq!(
            fallen(blast, time),
            Axes {
                x: blast,
                y: closure
            }
        );
        assert_eq!(
            fallen(blast, closure),
            Axes {
                x: blast,
                y: closure
            }
        );
        let (written, read) = (Axis::BytesWritten, Axis::BytesRead);
        assert_eq!(
            fallen(written, read),
            Axes {
                x: written,
                y: read
            },
            "an axis whose overlay is carried stays"
        );
        let measuring = Carried {
            inputs: false,
            ..carried
        };
        assert_eq!(
            Axes {
                x: written,
                y: read
            }
            .available(measuring),
            Axes {
                x: written,
                y: closure
            },
            "an axis of what spawns read waits on their measure"
        );
        let bare = Carried {
            times: false,
            bytes: false,
            inputs: false,
        };
        assert_eq!(
            Axes {
                x: written,
                y: time
            }
            .available(bare),
            Axes {
                x: closure,
                y: blast
            }
        );
    }
}
