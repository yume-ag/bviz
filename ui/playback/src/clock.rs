use bviz_invocation::EpochMicros;
use gpui::{App, AppContext, Context, Entity, Subscription, Window};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

/// The playback speeds, as multiples of real time, in the order stepping
/// through them takes.
const SPEEDS: [f64; 4] = [1.0, 2.0, 4.0, 8.0];

/// How far a step moves along a timeline no source marks stops on, as a share
/// of the whole timeline.
const STEP_SHARE: f64 = 0.02;

/// A stretch of time that holds some: its start lies before its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    start: EpochMicros,
    end: EpochMicros,
}

impl Span {
    /// The stretch from `start` to `end`, or `None` where it holds no time.
    pub fn new(start: EpochMicros, end: EpochMicros) -> Option<Span> {
        (start < end).then_some(Span { start, end })
    }

    pub fn start(self) -> EpochMicros {
        self.start
    }

    pub fn end(self) -> EpochMicros {
        self.end
    }

    pub fn length_micros(self) -> i64 {
        self.end.micros() - self.start.micros()
    }

    /// `at`, held within this span.
    pub fn clamp(self, at: EpochMicros) -> EpochMicros {
        at.clamp(self.start, self.end)
    }

    /// Microseconds from the start to `at`, held within this span.
    pub fn elapsed_micros(self, at: EpochMicros) -> i64 {
        self.clamp(at).micros() - self.start.micros()
    }

    /// The share of this span lying before `at`, from 0 to 1.
    pub fn fraction_at(self, at: EpochMicros) -> f32 {
        (self.elapsed_micros(at) as f64 / self.length_micros() as f64) as f32
    }

    /// The instant `fraction` of the way through this span, `fraction` held
    /// from 0 to 1.
    pub fn at_fraction(self, fraction: f32) -> EpochMicros {
        let share = f64::from(fraction.clamp(0.0, 1.0));
        let offset = (self.length_micros() as f64 * share).round() as i64;
        self.start.plus_micros(offset)
    }

    /// The least span holding both.
    fn hull(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// How fast a clock plays, one of a fixed list of multiples of real time.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Speed(usize);

impl Speed {
    /// Every speed, slowest first.
    pub fn all() -> impl ExactSizeIterator<Item = Speed> {
        (0..SPEEDS.len()).map(Speed)
    }

    /// The speed at `index` among [`all`](Self::all), if there is one.
    pub fn from_index(index: usize) -> Option<Speed> {
        (index < SPEEDS.len()).then_some(Speed(index))
    }

    pub fn index(self) -> usize {
        self.0
    }

    /// Played microseconds per real one.
    pub fn factor(self) -> f64 {
        SPEEDS[self.0]
    }

    /// The speed `steps` places along [`all`](Self::all), wrapping past
    /// either end.
    fn shifted(self, steps: isize) -> Speed {
        let count = SPEEDS.len() as isize;
        Speed((self.0 as isize + steps).rem_euclid(count) as usize)
    }
}

/// What one file of a document gives the document's [`Clock`] to play.
#[derive(Clone, Debug)]
pub struct Source {
    span: Span,
    /// The instants a step stops at, ascending, without repeats.
    marks: Vec<EpochMicros>,
    approximate: bool,
}

impl Source {
    pub fn new(span: Span) -> Self {
        Self {
            span,
            marks: Vec::new(),
            approximate: false,
        }
    }

    /// This source, a step stopping at each of `marks`.
    pub fn with_marks(mut self, marks: impl IntoIterator<Item = EpochMicros>) -> Self {
        self.marks = marks.into_iter().collect();
        self.marks.sort_unstable();
        self.marks.dedup();
        self
    }

    /// This source, its times interpolated between the few it recorded.
    pub fn approximate(mut self) -> Self {
        self.approximate = true;
        self
    }
}

/// A source registered with a [`Clock`], by which its registrant replaces or
/// removes it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SourceId(u64);

/// What a clock's sources make of its timeline together.
#[derive(Debug, PartialEq)]
struct Shape {
    /// The least span holding every source's.
    range: Span,
    /// Every source's marks, ascending, without repeats.
    marks: Vec<EpochMicros>,
    /// Whether any source's times are estimates.
    approximate: bool,
}

impl Shape {
    /// The shape `sources` give a timeline, or `None` where there are none.
    fn of<'a>(sources: impl Iterator<Item = &'a Source> + Clone) -> Option<Shape> {
        let mut spans = sources.clone().map(|source| source.span);
        let first = spans.next()?;
        let range = spans.fold(first, Span::hull);
        let mut marks = Vec::with_capacity(sources.clone().map(|s| s.marks.len()).sum());
        for source in sources.clone() {
            marks.extend_from_slice(&source.marks);
        }
        marks.sort_unstable();
        marks.dedup();
        let approximate = sources.clone().any(|source| source.approximate);
        Some(Shape {
            range,
            marks,
            approximate,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Motion {
    Paused {
        at: EpochMicros,
    },
    /// Advancing from `from`, which it stood at at `since`, at the timeline's
    /// speed until it reaches the end.
    Playing {
        since: Instant,
        from: EpochMicros,
    },
}

/// A clock's time while at least one source gives it a span to play.
#[derive(Debug)]
pub struct Timeline {
    shape: Shape,
    speed: Speed,
    motion: Motion,
}

impl Timeline {
    /// A timeline parked at the end of `shape`, showing the whole of what its
    /// sources recorded.
    fn parked(shape: Shape, speed: Speed) -> Self {
        let at = shape.range.end;
        Self {
            shape,
            speed,
            motion: Motion::Paused { at },
        }
    }

    pub fn range(&self) -> Span {
        self.shape.range
    }

    /// Whether any source's times are estimates, so the time shown is too.
    pub fn approximate(&self) -> bool {
        self.shape.approximate
    }

    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// The time this timeline stands at, computed afresh from the wall clock
    /// on every read.
    pub fn now(&self) -> EpochMicros {
        self.at(Instant::now())
    }

    /// Whether the time is advancing: it plays and has not reached the end.
    pub fn is_playing(&self) -> bool {
        self.playing_at(Instant::now())
    }

    fn at(&self, instant: Instant) -> EpochMicros {
        let range = self.shape.range;
        match self.motion {
            Motion::Paused { at } => range.clamp(at),
            Motion::Playing { since, from } => {
                let real = instant.saturating_duration_since(since).as_micros() as f64;
                let played = (real * self.speed.factor()).round() as i64;
                range.clamp(from.plus_micros(played))
            }
        }
    }

    fn playing_at(&self, instant: Instant) -> bool {
        matches!(self.motion, Motion::Playing { .. }) && self.at(instant) < self.shape.range.end
    }

    /// Pauses a timeline that played to its end, where it stopped.
    fn settle(&mut self, instant: Instant) {
        if !self.playing_at(instant) {
            self.motion = Motion::Paused {
                at: self.at(instant),
            };
        }
    }

    /// Plays on from where the time stands, or from the start where it stands
    /// at the end.
    fn play(&mut self, instant: Instant) {
        self.settle(instant);
        if let Motion::Paused { at } = self.motion {
            let range = self.shape.range;
            let at = range.clamp(at);
            let from = if at >= range.end { range.start } else { at };
            self.motion = Motion::Playing {
                since: instant,
                from,
            };
        }
    }

    fn pause(&mut self, instant: Instant) {
        self.motion = Motion::Paused {
            at: self.at(instant),
        };
    }

    fn play_pause(&mut self, instant: Instant) {
        if self.playing_at(instant) {
            self.pause(instant);
        } else {
            self.play(instant);
        }
    }

    /// Moves the time to `to`, held within the range, playing on from there
    /// if it was playing.
    fn seek(&mut self, to: EpochMicros, instant: Instant) {
        self.settle(instant);
        let to = self.shape.range.clamp(to);
        self.motion = match self.motion {
            Motion::Paused { .. } => Motion::Paused { at: to },
            Motion::Playing { .. } => Motion::Playing {
                since: instant,
                from: to,
            },
        };
    }

    /// Seeks to the next stop past the time, or the last one before it: the
    /// nearest mark a source gave, else a fixed share of the range away.
    fn step(&mut self, forward: bool, instant: Instant) {
        let now = self.at(instant);
        let Shape { range, marks, .. } = &self.shape;
        let to = if marks.is_empty() {
            let share = (range.length_micros() as f64 * STEP_SHARE).round() as i64;
            now.plus_micros(if forward { share } else { -share })
        } else if forward {
            let next = marks.partition_point(|mark| *mark <= now);
            marks.get(next).copied().unwrap_or(range.end)
        } else {
            let earlier = marks.partition_point(|mark| *mark < now);
            match earlier.checked_sub(1) {
                Some(last) => marks[last],
                None => range.start,
            }
        };
        self.seek(to, instant);
    }

    /// Plays at `speed` from where the time stands.
    fn set_speed(&mut self, speed: Speed, instant: Instant) {
        let now = self.at(instant);
        if let Motion::Playing { .. } = self.motion {
            self.motion = Motion::Playing {
                since: instant,
                from: now,
            };
        }
        self.speed = speed;
    }

    /// Takes `shape` for what the sources now give. A time parked at the end
    /// moves to the new end, so a timeline left showing all that is recorded
    /// goes on showing all of it as more arrives.
    fn reshape(&mut self, shape: Shape, instant: Instant) {
        self.settle(instant);
        let parked = matches!(self.motion, Motion::Paused { at } if at >= self.shape.range.end);
        self.shape = shape;
        if parked {
            self.motion = Motion::Paused {
                at: self.shape.range.end,
            };
        }
    }
}

/// The time a document's views show, shared by every view of it in every
/// window. Its sources give it the timeline it plays; with none it has no time
/// and nothing to play. Views following the time observe it for every change
/// a gesture makes.
#[derive(Default)]
pub struct Clock {
    sources: Sources,
    timeline: Option<Timeline>,
}

/// The sources registered with a clock, in the order they were added.
#[derive(Default)]
struct Sources {
    held: Vec<(SourceId, Source)>,
    next: u64,
}

impl Sources {
    fn add(&mut self, source: Source) -> SourceId {
        let id = SourceId(self.next);
        self.next += 1;
        self.held.push((id, source));
        id
    }

    /// Says whether `id` names a source held, which `source` then replaces.
    fn replace(&mut self, id: SourceId, source: Source) -> bool {
        let held = self.held.iter_mut().find(|(held, _)| *held == id);
        held.map(|(_, held)| *held = source).is_some()
    }

    /// Says whether `id` named a source held.
    fn remove(&mut self, id: SourceId) -> bool {
        let before = self.held.len();
        self.held.retain(|(held, _)| *held != id);
        self.held.len() != before
    }

    fn shape(&self) -> Option<Shape> {
        Shape::of(self.held.iter().map(|(_, source)| source))
    }
}

impl Clock {
    pub fn timeline(&self) -> Option<&Timeline> {
        self.timeline.as_ref()
    }

    /// The time the document's views show, `None` while no source gives one.
    pub fn now(&self) -> Option<EpochMicros> {
        self.timeline.as_ref().map(Timeline::now)
    }

    /// Asks `window` for another frame while `clock` plays, for a view painting
    /// its time to call from its render: the time is read afresh each frame,
    /// never ticked.
    pub fn keep_pace(clock: Option<&Entity<Self>>, window: &mut Window, cx: &App) {
        let playing = clock.and_then(|clock| clock.read(cx).timeline.as_ref());
        if playing.is_some_and(Timeline::is_playing) {
            window.request_animation_frame();
        }
    }

    pub fn add_source(&mut self, source: Source, cx: &mut Context<Self>) -> SourceId {
        let id = self.sources.add(source);
        self.reshape(cx);
        id
    }

    pub fn replace_source(&mut self, id: SourceId, source: Source, cx: &mut Context<Self>) {
        if self.sources.replace(id, source) {
            self.reshape(cx);
        }
    }

    pub fn remove_source(&mut self, id: SourceId, cx: &mut Context<Self>) {
        if self.sources.remove(id) {
            self.reshape(cx);
        }
    }

    pub fn play_pause(&mut self, cx: &mut Context<Self>) {
        self.drive(cx, Timeline::play_pause);
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.drive(cx, Timeline::pause);
    }

    pub fn seek(&mut self, to: EpochMicros, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| timeline.seek(to, instant));
    }

    pub fn seek_start(&mut self, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| {
            timeline.seek(timeline.range().start, instant);
        });
    }

    pub fn seek_end(&mut self, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| {
            timeline.seek(timeline.range().end, instant);
        });
    }

    pub fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| timeline.step(forward, instant));
    }

    pub fn set_speed(&mut self, speed: Speed, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| timeline.set_speed(speed, instant));
    }

    /// Moves the speed `steps` places along [`Speed::all`], wrapping.
    pub fn shift_speed(&mut self, steps: isize, cx: &mut Context<Self>) {
        self.drive(cx, |timeline, instant| {
            timeline.set_speed(timeline.speed.shifted(steps), instant);
        });
    }

    fn drive(&mut self, cx: &mut Context<Self>, gesture: impl FnOnce(&mut Timeline, Instant)) {
        if let Some(timeline) = &mut self.timeline {
            gesture(timeline, Instant::now());
            cx.notify();
        }
    }

    fn reshape(&mut self, cx: &mut Context<Self>) {
        self.timeline = reshaped(self.timeline.take(), self.sources.shape(), Instant::now());
        cx.notify();
    }
}

/// What `timeline` becomes once its sources give `shape`: none without a
/// shape, and a timeline parked at the end where there was none.
fn reshaped(
    timeline: Option<Timeline>,
    shape: Option<Shape>,
    instant: Instant,
) -> Option<Timeline> {
    match (timeline, shape) {
        (_, None) => None,
        (None, Some(shape)) => Some(Timeline::parked(shape, Speed::default())),
        (Some(mut timeline), Some(shape)) => {
            timeline.reshape(shape, instant);
            Some(timeline)
        }
    }
}

/// A document's clock as the views of one of its files receive it, saying
/// whether that file's times must meet another's.
#[derive(Clone)]
pub enum DocumentClock {
    /// The clock of a document the file makes up by itself, which no other
    /// file's times share.
    Alone(Entity<Clock>),
    /// The clock of a document joining several files, on which each file's
    /// times stand where the wall clock puts them.
    Joined(Entity<Clock>),
}

impl DocumentClock {
    pub fn alone(cx: &mut App) -> Self {
        Self::Alone(cx.new(|_| Clock::default()))
    }

    pub fn joined(cx: &mut App) -> Self {
        Self::Joined(cx.new(|_| Clock::default()))
    }

    pub fn clock(&self) -> &Entity<Clock> {
        match self {
            Self::Alone(clock) | Self::Joined(clock) => clock,
        }
    }
}

/// A file's place on its document's clock, through which the file registers,
/// replaces or withdraws what it plays there as that changes, and which
/// withdraws it when the model holding it is released.
pub struct OnClock {
    clock: DocumentClock,
    /// `None` while the file gives the clock no time to play. Shared with the
    /// withdrawal, which runs once the holder is gone.
    source: Rc<Cell<Option<SourceId>>>,
    _withdrawal: Subscription,
}

impl OnClock {
    /// A place on `clock` for as long as the model `cx` builds is held.
    pub fn new<M: 'static>(clock: DocumentClock, cx: &mut Context<M>) -> Self {
        let source = Rc::new(Cell::new(None));
        let withdrawn = (clock.clock().clone(), source.clone());
        let withdrawal = cx.on_release(move |_, cx| {
            let (clock, source) = withdrawn;
            if let Some(id) = source.get() {
                clock.update(cx, |clock, cx| clock.remove_source(id, cx));
            }
        });
        Self {
            clock,
            source,
            _withdrawal: withdrawal,
        }
    }

    pub fn document_clock(&self) -> &DocumentClock {
        &self.clock
    }

    pub fn clock(&self) -> &Entity<Clock> {
        self.clock.clock()
    }

    /// Registers `played` with the clock as what the file plays, or withdraws
    /// the file's source where it plays nothing.
    pub fn place(&mut self, played: Option<Source>, cx: &mut App) {
        let source = &self.source;
        self.clock
            .clock()
            .update(cx, |clock, cx| match (source.get(), played) {
                (Some(id), Some(played)) => clock.replace_source(id, played, cx),
                (None, Some(played)) => source.set(Some(clock.add_source(played, cx))),
                (Some(id), None) => {
                    clock.remove_source(id, cx);
                    source.set(None);
                }
                (None, None) => {}
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const ORIGIN: EpochMicros = EpochMicros::from_millis(1_781_439_245_000);

    fn at(seconds: i64) -> EpochMicros {
        ORIGIN.plus_micros(seconds * 1_000_000)
    }

    fn span(start: i64, end: i64) -> Span {
        Span::new(at(start), at(end)).unwrap()
    }

    fn shape(sources: &[Source]) -> Shape {
        Shape::of(sources.iter()).unwrap()
    }

    fn timeline(sources: &[Source]) -> Timeline {
        Timeline::parked(shape(sources), Speed::default())
    }

    fn later(instant: Instant, seconds: u64) -> Instant {
        instant + Duration::from_secs(seconds)
    }

    #[test]
    fn a_span_holds_some_time() {
        assert_eq!(Span::new(at(1), at(1)), None);
        assert_eq!(Span::new(at(2), at(1)), None);
        assert_eq!(span(0, 10).length_micros(), 10_000_000);
    }

    #[test]
    fn a_fraction_maps_to_an_instant_and_back() {
        let span = span(0, 10);
        assert_eq!(span.at_fraction(0.5), at(5));
        assert_eq!(span.at_fraction(2.0), at(10));
        assert_eq!(span.fraction_at(at(5)), 0.5);
        assert_eq!(span.fraction_at(at(-3)), 0.0);
    }

    #[test]
    fn no_source_gives_no_timeline() {
        let none: [Source; 0] = [];
        assert_eq!(Shape::of(none.iter()), None);
    }

    #[test]
    fn the_range_holds_every_source_and_their_marks_merge() {
        let shape = shape(&[
            Source::new(span(2, 8)).with_marks([at(3), at(5)]),
            Source::new(span(0, 4))
                .with_marks([at(5), at(1)])
                .approximate(),
        ]);
        assert_eq!(shape.range, span(0, 8));
        assert_eq!(shape.marks, [at(1), at(3), at(5)]);
        assert!(shape.approximate);
    }

    #[test]
    fn a_timeline_opens_parked_at_the_end() {
        let timeline = timeline(&[Source::new(span(0, 10))]);
        assert_eq!(timeline.now(), at(10));
        assert!(!timeline.is_playing());
    }

    #[test]
    fn a_timeline_parked_at_the_end_follows_it_as_the_range_grows() {
        let start = Instant::now();
        let mut timeline = timeline(&[Source::new(span(0, 10))]);
        timeline.reshape(shape(&[Source::new(span(0, 20))]), start);
        assert_eq!(timeline.at(start), at(20));
        timeline.seek(at(5), start);
        timeline.reshape(shape(&[Source::new(span(0, 30))]), start);
        assert_eq!(timeline.at(start), at(5));
    }

    #[test]
    fn playing_advances_at_the_speed_and_stops_at_the_end() {
        let start = Instant::now();
        let mut timeline = timeline(&[Source::new(span(0, 10))]);
        timeline.seek(at(2), start);
        timeline.set_speed(Speed::from_index(1).unwrap(), start);
        timeline.play(start);
        assert_eq!(timeline.at(later(start, 3)), at(8));
        assert!(timeline.playing_at(later(start, 3)));
        assert_eq!(timeline.at(later(start, 60)), at(10));
        assert!(!timeline.playing_at(later(start, 60)));
    }

    #[test]
    fn playing_from_the_end_starts_over() {
        let start = Instant::now();
        let mut timeline = timeline(&[Source::new(span(0, 10))]);
        timeline.play(start);
        assert_eq!(timeline.at(later(start, 1)), at(1));
        timeline.play_pause(later(start, 60));
        assert_eq!(timeline.at(later(start, 61)), at(1));
    }

    #[test]
    fn a_speed_change_keeps_the_time_where_it_stands() {
        let start = Instant::now();
        let mut timeline = timeline(&[Source::new(span(0, 100))]);
        timeline.seek(at(0), start);
        timeline.play(start);
        timeline.set_speed(Speed::from_index(2).unwrap(), later(start, 4));
        assert_eq!(timeline.at(later(start, 4)), at(4));
        assert_eq!(timeline.at(later(start, 5)), at(8));
    }

    #[test]
    fn a_step_stops_at_the_nearest_mark() {
        let start = Instant::now();
        let source = Source::new(span(0, 10)).with_marks([at(2), at(6)]);
        let mut timeline = timeline(&[source]);
        timeline.seek(at(2), start);
        timeline.step(true, start);
        assert_eq!(timeline.at(start), at(6));
        timeline.step(true, start);
        assert_eq!(timeline.at(start), at(10));
        timeline.step(false, start);
        assert_eq!(timeline.at(start), at(6));
        timeline.seek(at(1), start);
        timeline.step(false, start);
        assert_eq!(timeline.at(start), at(0));
    }

    #[test]
    fn a_step_with_no_marks_moves_a_share_of_the_range() {
        let start = Instant::now();
        let mut timeline = timeline(&[Source::new(span(0, 100))]);
        timeline.step(false, start);
        assert_eq!(timeline.at(start), at(98));
        timeline.seek(at(1), start);
        timeline.step(false, start);
        assert_eq!(timeline.at(start), at(0));
    }

    #[test]
    fn removing_a_source_reshapes_the_timeline_to_the_rest() {
        let start = Instant::now();
        let mut sources = Sources::default();
        let early = sources.add(Source::new(span(0, 4)).with_marks([at(1)]));
        let late = sources.add(Source::new(span(2, 10)).approximate());
        let timeline = reshaped(None, sources.shape(), start).expect("sources give a timeline");
        assert_eq!(timeline.range(), span(0, 10));

        assert!(sources.remove(late));
        let timeline = reshaped(Some(timeline), sources.shape(), start).expect("one source left");
        assert_eq!(timeline.range(), span(0, 4));
        assert!(!timeline.approximate());
        assert_eq!(
            timeline.at(start),
            at(4),
            "parked at the end, it follows it"
        );

        assert!(sources.remove(early));
        assert!(reshaped(Some(timeline), sources.shape(), start).is_none());
    }

    #[test]
    fn a_source_removed_or_replaced_by_an_id_not_held_changes_nothing() {
        let mut sources = Sources::default();
        let held = sources.add(Source::new(span(0, 4)));
        assert!(sources.remove(held));
        assert!(!sources.remove(held));
        assert!(!sources.replace(held, Source::new(span(0, 8))));
        assert_eq!(sources.shape(), None);
        let other = sources.add(Source::new(span(1, 2)));
        assert_ne!(other, held, "ids are never reused");
    }

    #[test]
    fn a_replaced_source_gives_the_shape_its_new_span() {
        let mut sources = Sources::default();
        let first = sources.add(Source::new(span(0, 4)));
        sources.add(Source::new(span(2, 6)));
        assert!(sources.replace(first, Source::new(span(1, 3))));
        assert_eq!(sources.shape().map(|shape| shape.range), Some(span(1, 6)));
    }

    #[test]
    fn speeds_wrap_past_either_end() {
        let fastest = Speed::from_index(Speed::all().len() - 1).unwrap();
        assert_eq!(fastest.shifted(1), Speed::default());
        assert_eq!(Speed::default().shifted(-1), fastest);
        assert_eq!(Speed::from_index(Speed::all().len()), None);
    }
}
