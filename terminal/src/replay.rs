//! A build's output organized for playback over time.
//!
//! Each Bazel progress event is one frame — a prefix of the captured output that
//! reconstructs the terminal as it stood at that point. The challenge is timing:
//! Bazel does not timestamp progress events. When the build was captured with
//! `--show_timestamps`, each progress repaint and message carries a `(HH:MM:SS)`
//! prefix in its text, giving real wall-clock anchors that are parsed back out
//! here. Otherwise the timeline is interpolated between the sparse events that do
//! carry timestamps (build start and finish, action and test completions).

use std::cmp::Reverse;

use bviz_invocation::EpochMicros;

use crate::columns::Columns;
use crate::display::DisplaySource;
use crate::output::{Anchor, BuildOutput};

/// One playback frame: the output reconstructs the terminal at byte offset `end`,
/// shown `time` seconds into the build.
struct Frame {
    end: usize,
    time: f64,
}

/// A build's output laid out as a timed sequence of frames.
pub struct Replay {
    bytes: Vec<u8>,
    columns: Columns,
    frames: Vec<Frame>,
    real_time: bool,
    epoch_origin: Option<EpochMicros>,
}

impl Replay {
    /// Lay out a parsed build's output for playback, choosing real timestamps when
    /// the capture carries them and interpolating otherwise.
    pub fn from_output(output: BuildOutput) -> Replay {
        let BuildOutput {
            columns,
            bytes,
            frames,
            anchors,
            show_timestamps,
            ..
        } = output;
        // A build with no progress events still plays as a single frame holding
        // whatever output there is.
        let ends = if frames.is_empty() {
            vec![bytes.len()]
        } else {
            frames
        };
        let count = ends.len();

        // Read the real `(HH:MM:SS)` markers only when the build was run with
        // --show_timestamps, so a stray time-shaped token in ordinary output never
        // masquerades as a timeline; otherwise interpolate from event timestamps.
        let markers = if show_timestamps {
            parse_markers(&bytes)
        } else {
            Vec::new()
        };
        let events = event_points(&anchors, count);
        let (times, real_time, epoch_origin) = match markers.split_first() {
            Some((first, rest)) => {
                let local = interpolate(marker_points(first, rest, &ends), count);
                let origin = local_lead(&local, &events).map(|lead| local.base - lead);
                (local.times, true, origin)
            }
            None => match Points::new(events) {
                Some(points) => {
                    let Timeline { times, base } = interpolate(points, count);
                    (times, false, Some(base))
                }
                None => (vec![0.0; count], false, None),
            },
        };
        let epoch_origin = epoch_origin.map(EpochMicros::from_secs_f64);
        let frames = ends
            .into_iter()
            .zip(times)
            .map(|(end, time)| Frame { end, time })
            .collect();
        Replay {
            bytes,
            columns,
            frames,
            real_time,
            epoch_origin,
        }
    }

    /// The number of frames; always at least one.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Total playback length in seconds: the time of the last frame.
    pub fn duration(&self) -> f64 {
        self.frames.last().map_or(0.0, |frame| frame.time)
    }

    /// Whether the timeline is from real `--show_timestamps` markers (`true`) or
    /// interpolated from the build's sparse event timestamps (`false`).
    pub fn real_time(&self) -> bool {
        self.real_time
    }

    /// The wall-clock instant of time zero; `None` when the stream timestamps
    /// no event. On a `--show_timestamps` timeline it is exact only to the
    /// markers' truncation to the second.
    pub fn epoch_origin(&self) -> Option<EpochMicros> {
        self.epoch_origin
    }

    /// The time in seconds at which a frame is shown.
    pub fn time_of(&self, frame: usize) -> f64 {
        self.frames[frame.min(self.frames.len() - 1)].time
    }

    /// The last frame whose time is at or before `seconds`.
    pub fn frame_at(&self, seconds: f64) -> usize {
        let next = self.frames.partition_point(|frame| frame.time <= seconds);
        next.saturating_sub(1)
    }

    /// Reconstruct the terminal as it stood at the end of `frame`.
    pub fn display_at(&self, frame: usize) -> DisplaySource {
        let end = self.frames[frame.min(self.frames.len() - 1)].end;
        DisplaySource::from_bytes(&self.bytes[..end], self.columns)
    }
}

/// A `(HH:MM:SS)` timestamp Bazel emits with `--show_timestamps`, recovered from
/// the output: its byte offset and its time of day in seconds.
struct Marker {
    offset: usize,
    secs_of_day: u32,
}

/// Find every `(HH:MM:SS)` marker in the output, in offset order.
fn parse_markers(bytes: &[u8]) -> Vec<Marker> {
    // The fixed shape `(dd:dd:dd)` is ten bytes; scan for it directly.
    let mut markers = Vec::new();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        if let Some(secs) = parse_marker(&bytes[i..i + 10]) {
            markers.push(Marker {
                offset: i,
                secs_of_day: secs,
            });
            i += 10;
        } else {
            i += 1;
        }
    }
    markers
}

/// Parse the time of day from a ten-byte `(HH:MM:SS)` window, if it is one.
fn parse_marker(window: &[u8]) -> Option<u32> {
    let digit = |b: u8| (b as char).to_digit(10);
    if window[0] != b'(' || window[3] != b':' || window[6] != b':' || window[9] != b')' {
        return None;
    }
    let h = digit(window[1])? * 10 + digit(window[2])?;
    let m = digit(window[4])? * 10 + digit(window[5])?;
    let s = digit(window[7])? * 10 + digit(window[8])?;
    if h >= 24 || m >= 60 || s >= 60 {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

const SECONDS_PER_DAY: f64 = 24.0 * 3600.0;

/// Just before midnight and just after, for telling a real day rollover from a
/// stray out-of-order marker.
const LATE_NIGHT: u32 = 23 * 3600;
const EARLY_MORNING: u32 = 3600;

/// Every UTC offset in civil use is a multiple of this many seconds.
const UTC_OFFSET_STEP: f64 = 15.0 * 60.0;

/// How far the local clock that timed `local` runs ahead of the epoch clock
/// that timed `events`, in seconds: a whole number of [`UTC_OFFSET_STEP`]s,
/// spanning the zone's offset and the day the local times count from. Each
/// event votes with what its frame read locally less its own time, rounded to
/// a step, which is that lead while the two instants are within half a step
/// of each other. The lead most events vote for wins; a tie goes to the
/// smaller, since an event is tied to the frame painted after it and so tends
/// to be read late.
fn local_lead(local: &Timeline, events: &[(usize, f64)]) -> Option<f64> {
    let mut votes: Vec<i64> = events
        .iter()
        .map(|&(frame, epoch)| {
            let read = local.base + local.times[frame];
            ((read - epoch) / UTC_OFFSET_STEP).round() as i64
        })
        .collect();
    votes.sort_unstable();
    let (_, Reverse(steps)) = votes
        .chunk_by(|a, b| a == b)
        .map(|agreeing| (agreeing.len(), Reverse(agreeing[0])))
        .max()?;
    Some(steps as f64 * UTC_OFFSET_STEP)
}

/// Anchor points `(frame_index, seconds)`, of which there is at least one.
struct Points {
    first: (usize, f64),
    rest: Vec<(usize, f64)>,
}

impl Points {
    fn new(points: Vec<(usize, f64)>) -> Option<Points> {
        let mut points = points.into_iter();
        let first = points.next()?;
        Some(Points {
            first,
            rest: points.collect(),
        })
    }

    fn into_vec(self) -> Vec<(usize, f64)> {
        let mut points = Vec::with_capacity(self.rest.len() + 1);
        points.push(self.first);
        points.extend(self.rest);
        points
    }
}

/// Anchor points from `--show_timestamps` markers. Times of day are unwrapped
/// across midnight; markers that step backwards without a plausible rollover
/// are dropped as coincidental matches in the output.
fn marker_points(first: &Marker, rest: &[Marker], ends: &[usize]) -> Points {
    let mut points = Vec::with_capacity(rest.len());
    let mut day = 0i64;
    let mut prev_of_day = first.secs_of_day;
    let mut last_abs = first.secs_of_day as f64;
    for marker in rest {
        if marker.secs_of_day < prev_of_day {
            if prev_of_day >= LATE_NIGHT && marker.secs_of_day < EARLY_MORNING {
                day += 1;
            } else {
                // Backwards without a midnight rollover: not a timestamp.
                continue;
            }
        }
        let abs = day as f64 * SECONDS_PER_DAY + marker.secs_of_day as f64;
        if abs < last_abs {
            continue;
        }
        prev_of_day = marker.secs_of_day;
        last_abs = abs;
        points.push((frame_of(ends, marker.offset), abs));
    }
    Points {
        first: (frame_of(ends, first.offset), first.secs_of_day as f64),
        rest: points,
    }
}

/// The frame an output offset falls in: the first frame whose output extends past
/// it.
fn frame_of(ends: &[usize], offset: usize) -> usize {
    let frame = ends.partition_point(|&end| end <= offset);
    frame.min(ends.len() - 1)
}

/// Anchor points from the build's timestamped events, each clamped to a real
/// frame.
fn event_points(anchors: &[Anchor], count: usize) -> Vec<(usize, f64)> {
    anchors
        .iter()
        .map(|anchor| (anchor.frame.min(count - 1), anchor.time))
        .collect()
}

/// Anchor points sorted by frame, collapsed to one per frame (keeping the latest
/// time seen there), forced non-decreasing in time, and shifted so the timeline
/// starts at zero; paired with the time the first of them held before the
/// shift.
fn sanitize_anchors(points: Points) -> (f64, Vec<(usize, f64)>) {
    let mut points = points.into_vec();
    points.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut anchors: Vec<(usize, f64)> = Vec::with_capacity(points.len());
    for (frame, time) in points {
        let time = anchors.last().map_or(time, |&(_, t)| time.max(t));
        match anchors.last_mut() {
            Some(last) if last.0 == frame => last.1 = time,
            _ => anchors.push((frame, time)),
        }
    }
    let base = anchors[0].1;
    for anchor in &mut anchors {
        anchor.1 -= base;
    }
    (base, anchors)
}

/// A time for every frame, starting at zero.
struct Timeline {
    times: Vec<f64>,
    /// What time zero read on the clock the anchor points were taken from.
    base: f64,
}

/// Assign every frame a time by linear interpolation between anchor points.
///
/// Frames before the first anchor or after the last take that anchor's time.
fn interpolate(points: Points, count: usize) -> Timeline {
    let (base, anchors) = sanitize_anchors(points);

    let mut times = Vec::with_capacity(count);
    for frame in 0..count {
        let time = match anchors.partition_point(|&(f, _)| f <= frame) {
            // Before the first anchor.
            0 => anchors[0].1,
            // At or after the last anchor.
            next if next == anchors.len() => anchors[next - 1].1,
            next => {
                let (fa, ta) = anchors[next - 1];
                let (fb, tb) = anchors[next];
                if fb == fa {
                    ta
                } else {
                    ta + (tb - ta) * (frame - fa) as f64 / (fb - fa) as f64
                }
            }
        };
        times.push(time);
    }
    Timeline { times, base }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = include_bytes!("../testdata/sample.bep");

    fn points(points: &[(usize, f64)]) -> Points {
        Points::new(points.to_vec()).expect("at least one point")
    }

    /// A `--show_timestamps` output of one frame per group of epoch times,
    /// each time printed as the marker of a clock `utc_offset` seconds ahead
    /// of UTC.
    fn marked(frames: &[&[f64]], utc_offset: f64) -> BuildOutput {
        let mut bytes = Vec::new();
        let mut ends = Vec::with_capacity(frames.len());
        for frame in frames {
            for epoch in *frame {
                let of_day = (epoch + utc_offset).rem_euclid(SECONDS_PER_DAY) as u32;
                let (h, m, s) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
                bytes.extend_from_slice(format!("({h:02}:{m:02}:{s:02}) x\n").as_bytes());
            }
            ends.push(bytes.len());
        }
        BuildOutput {
            frames: ends,
            show_timestamps: true,
            ..BuildOutput::from_bytes(40, bytes)
        }
    }

    /// An epoch time that reads `of_day` seconds past midnight on a clock
    /// `utc_offset` seconds ahead of UTC.
    fn epoch_reading(of_day: f64, utc_offset: f64) -> f64 {
        20_000.0 * SECONDS_PER_DAY + of_day - utc_offset
    }

    const QUARTER_TO_THREE: f64 = (14 * 3600 + 47 * 60 + 41) as f64;

    /// UTC, offsets off the hour, and the extremes in civil use.
    const UTC_OFFSETS: [f64; 5] = [
        0.0,
        (5 * 3600 + 45 * 60) as f64,
        -(3 * 3600 + 30 * 60) as f64,
        (14 * 3600) as f64,
        -(12 * 3600) as f64,
    ];

    #[test]
    fn parses_markers_and_rejects_non_markers() {
        let bytes = b"(14:47:41) start\nnot (99:99:99) a time\n(14:47:43) next";
        let markers = parse_markers(bytes);
        let times: Vec<u32> = markers.iter().map(|m| m.secs_of_day).collect();
        assert_eq!(
            times,
            vec![14 * 3600 + 47 * 60 + 41, 14 * 3600 + 47 * 60 + 43]
        );
    }

    #[test]
    fn marker_points_unwrap_real_midnight_but_drop_strays() {
        let ends = [10, 20, 30, 40];
        // A genuine rollover (23:59:59 → 00:00:01) advances a day; a stray earlier
        // time mid-stream is dropped, not treated as another rollover.
        let markers = [
            Marker {
                offset: 0,
                secs_of_day: 23 * 3600 + 59 * 60 + 59,
            },
            Marker {
                offset: 12,
                secs_of_day: 1,
            },
            Marker {
                offset: 22,
                secs_of_day: 12 * 3600,
            },
            Marker {
                offset: 32,
                secs_of_day: 2,
            },
        ];
        let (first, rest) = markers.split_first().expect("markers");
        let points = marker_points(first, rest, &ends).into_vec();
        // The 12:00:00 stray after 00:00:01 is dropped; the rollover is kept.
        assert_eq!(points.len(), 3);
        assert!(points.windows(2).all(|w| w[0].1 <= w[1].1));
        // 00:00:01 the day after 23:59:59 is two seconds later.
        assert_eq!(points[1].1 - points[0].1, 2.0);
    }

    #[test]
    fn interpolation_spreads_frames_between_anchors() {
        // Anchors at frames 0 and 4, ten seconds apart; the middle frames fall
        // evenly between, and the timeline starts at zero.
        let timeline = interpolate(points(&[(0, 100.0), (4, 110.0)]), 5);
        assert_eq!(timeline.times, vec![0.0, 2.5, 5.0, 7.5, 10.0]);
        assert_eq!(timeline.base, 100.0);
    }

    #[test]
    fn interpolation_clamps_outside_anchors_and_forces_monotonic() {
        // One interior anchor: frames before it hold its time, frames after hold
        // the last; a backwards anchor time is clamped up, not allowed to dip.
        let times = interpolate(points(&[(1, 10.0), (2, 5.0)]), 4).times;
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        assert_eq!(times[0], 0.0);
    }

    #[test]
    fn frame_at_finds_the_frame_for_a_time() {
        let replay = Replay {
            bytes: Vec::new(),
            columns: Columns::DEFAULT,
            frames: vec![
                Frame { end: 0, time: 0.0 },
                Frame { end: 0, time: 2.0 },
                Frame { end: 0, time: 4.0 },
            ],
            real_time: true,
            epoch_origin: None,
        };
        assert_eq!(replay.frame_at(-1.0), 0);
        assert_eq!(replay.frame_at(0.0), 0);
        assert_eq!(replay.frame_at(1.9), 0);
        assert_eq!(replay.frame_at(2.0), 1);
        assert_eq!(replay.frame_at(100.0), 2);
        assert_eq!(replay.duration(), 4.0);
    }

    #[test]
    fn display_at_shows_only_the_frame_prefix() {
        // Two progress events: the first writes "alpha", the second "beta". Frame 0
        // shows only the first; frame 1 shows both.
        let output = BuildOutput {
            frames: vec![6, 11],
            ..BuildOutput::from_bytes(40, b"alpha\nbeta\n".to_vec())
        };
        let replay = Replay::from_output(output);
        assert_eq!(replay.frame_count(), 2);

        let at = |frame| {
            let viewport =
                crate::Terminal::from_display(&replay.display_at(frame), 40, 10).viewport();
            crate::screen::rows_to_text(&viewport)
                .trim_end()
                .to_string()
        };
        assert_eq!(at(0), "alpha");
        assert_eq!(at(1), "alpha\nbeta");
    }

    #[test]
    fn sample_without_markers_falls_back_to_event_anchors() {
        // The committed sample was captured without --show_timestamps, so the
        // timeline is interpolated from build events, spanning a real duration.
        let output = crate::output::read_slice(SAMPLE)
            .expect("parse sample")
            .into_inner();
        let replay = Replay::from_output(output);
        assert!(!replay.real_time());
        assert!(replay.frame_count() > 1);
        assert!(replay.duration() > 0.0, "events should span real time");
        // Time zero is the build's start.
        assert_eq!(
            replay.epoch_origin(),
            Some(EpochMicros::from_millis(1_781_439_245_231))
        );
        // Frame times are non-decreasing and start at zero.
        assert_eq!(replay.time_of(0), 0.0);
        for f in 1..replay.frame_count() {
            assert!(replay.time_of(f) >= replay.time_of(f - 1));
        }
    }

    #[test]
    fn real_markers_used_only_with_the_flag() {
        // Three timestamped lines, one per frame, two seconds apart.
        let bytes = b"(00:00:01) a\n(00:00:03) b\n(00:00:05) c\n".to_vec();
        let frames = vec![13, 26, bytes.len()];
        let with = Replay::from_output(BuildOutput {
            frames: frames.clone(),
            show_timestamps: true,
            ..BuildOutput::from_bytes(40, bytes.clone())
        });
        assert!(with.real_time());
        assert_eq!(with.duration(), 4.0);

        // The same bytes without the flag: the markers are not a timeline, so it
        // falls back (here to nothing, as there are no event anchors).
        let without = Replay::from_output(BuildOutput {
            frames,
            ..BuildOutput::from_bytes(40, bytes)
        });
        assert!(!without.real_time());
        assert_eq!(without.duration(), 0.0);
    }

    #[test]
    fn an_interpolated_timeline_originates_where_its_first_anchored_frame_is_timed() {
        // Frame 1 is the first an event is tied to, and the later of its two
        // events times it.
        let replay = Replay::from_output(BuildOutput {
            frames: vec![1, 2, 3],
            anchors: vec![
                Anchor {
                    frame: 1,
                    time: 1_000.5,
                },
                Anchor {
                    frame: 1,
                    time: 1_002.25,
                },
                Anchor {
                    frame: 2,
                    time: 1_010.0,
                },
            ],
            ..BuildOutput::from_bytes(40, b"abc".to_vec())
        });
        assert!(!replay.real_time());
        assert_eq!(replay.time_of(0), 0.0);
        assert_eq!(replay.time_of(1), 0.0);
        assert_eq!(
            replay.epoch_origin(),
            Some(EpochMicros::from_secs_f64(1_002.25))
        );
    }

    #[test]
    fn a_marker_timeline_originates_at_the_marker_timing_frame_zero_in_any_zone() {
        for utc_offset in UTC_OFFSETS {
            let start = epoch_reading(QUARTER_TO_THREE, utc_offset) + 0.4;
            // The last of a frame's markers times the frame.
            let output = marked(&[&[start, start + 2.3], &[start + 9.0]], utc_offset);
            let replay = Replay::from_output(BuildOutput {
                anchors: vec![Anchor {
                    frame: 0,
                    time: start - 0.2,
                }],
                ..output
            });
            assert!(replay.real_time());
            assert_eq!(
                replay.epoch_origin(),
                Some(EpochMicros::from_secs_f64((start + 2.3).floor())),
                "{utc_offset}"
            );
        }
    }

    #[test]
    fn a_marker_timeline_is_placed_by_the_events_that_agree() {
        // Bazel dates the start before a wait for the server lock, here more
        // than half an offset step ahead of the first marker.
        let lock_wait = UTC_OFFSET_STEP * 0.75;
        for utc_offset in UTC_OFFSETS {
            let first = epoch_reading(QUARTER_TO_THREE, utc_offset);
            let started = (0, first - lock_wait);
            let finished = (2, first + 59.5);
            let completed = (1, first + 29.5);
            // The start and the finish alone disagree, and the smaller lead
            // wins; a further event outvotes the start.
            for events in [vec![started, finished], vec![started, completed, finished]] {
                let replay = Replay::from_output(BuildOutput {
                    anchors: events
                        .iter()
                        .map(|&(frame, time)| Anchor { frame, time })
                        .collect(),
                    ..marked(&[&[first], &[first + 30.0], &[first + 60.0]], utc_offset)
                });
                assert_eq!(
                    replay.epoch_origin(),
                    Some(EpochMicros::from_secs_f64(first)),
                    "{utc_offset} {events:?}"
                );
            }
        }
    }

    #[test]
    fn a_marker_timeline_is_placed_across_midnight() {
        for utc_offset in UTC_OFFSETS {
            let before = epoch_reading(SECONDS_PER_DAY - 2.0, utc_offset);
            // Only the frame painted after local midnight is tied to an event.
            let replay = Replay::from_output(BuildOutput {
                anchors: vec![Anchor {
                    frame: 1,
                    time: before + 4.5,
                }],
                ..marked(&[&[before], &[before + 5.0]], utc_offset)
            });
            assert_eq!(replay.duration(), 5.0);
            assert_eq!(
                replay.epoch_origin(),
                Some(EpochMicros::from_secs_f64(before)),
                "{utc_offset}"
            );
        }
    }

    #[test]
    fn a_timeline_without_a_timestamped_event_has_no_origin() {
        let bytes = b"(00:00:01) a\n".to_vec();
        let markers_only = Replay::from_output(BuildOutput {
            show_timestamps: true,
            ..BuildOutput::from_bytes(40, bytes.clone())
        });
        assert!(markers_only.real_time());
        assert_eq!(markers_only.epoch_origin(), None);
        let no_clock = Replay::from_output(BuildOutput::from_bytes(40, bytes));
        assert_eq!(no_clock.epoch_origin(), None);
    }
}
