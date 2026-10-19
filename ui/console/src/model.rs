use bviz_input::Input;
use bviz_invocation::{Configurations, EpochMicros, Outcomes, Recorded, Session};
use bviz_terminal::{BuildOutput, Replay};
use bviz_ui_components::{Pending, load_in_background};
use bviz_ui_playback::{Clock, DocumentClock, OnClock, Span};
use gpui::{App, Context, Entity, Task};

pub(crate) type Source = bviz_ui_components::Source<Recorded<Replay>>;

/// What a build event stream is read into.
pub(crate) struct Stream {
    replay: Recorded<Replay>,
    configurations: Configurations,
    outcomes: Outcomes,
}

impl Stream {
    pub(crate) fn read(input: Input) -> Result<Self, String> {
        let name = input.name().to_owned();
        let size = input.size();
        let read = bviz_terminal::read(input.into_reader(), size)
            .map_err(|e| format!("cannot read {name}: {e}"));
        Self::of(read)
    }

    #[cfg(test)]
    pub(crate) fn from_slice(stream: &[u8]) -> Result<Self, String> {
        Self::of(bviz_terminal::read(stream, Some(stream.len() as u64)).map_err(|e| e.to_string()))
    }

    fn of(read: Result<Recorded<BuildOutput>, String>) -> Result<Self, String> {
        let mut output = read?;
        let configurations = std::mem::take(&mut output.get_mut().configurations);
        let outcomes = std::mem::take(&mut output.get_mut().outcomes);
        Ok(Self {
            replay: output.map(Replay::from_output),
            configurations,
            outcomes,
        })
    }

    fn replay_telling(self, session: &mut Session) -> Recorded<Replay> {
        session.add_configurations(self.configurations);
        session.report_outcomes(self.outcomes);
        self.replay
    }
}

/// Where time zero of a replay recording no wall-clock time stands on a clock
/// it plays on alone, there being no other file's times for it to meet.
const UNRECORDED_ORIGIN: EpochMicros = EpochMicros::from_millis(0);

/// A build's captured console output, held apart from the panels replaying it so
/// a view of it can close and be shown again without the build event stream
/// being read and reconstructed afresh.
pub(crate) struct ConsoleModel {
    source: Source,
    on_clock: Option<OnClock>,
    /// The read that fills `source`, held so that it is not cancelled.
    _load: Task<()>,
}

impl ConsoleModel {
    /// The build `stream` is being read into, playable on `clock` once the read
    /// finishes and failed where it does not.
    pub(crate) fn reading(
        stream: Pending<Stream>,
        session: Option<Entity<Session>>,
        clock: Option<DocumentClock>,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = load_in_background(
            cx,
            move || stream.join(),
            move |model, result, cx| {
                model.source = match result {
                    Ok(stream) => Source::Loaded(match session {
                        Some(session) => session.update(cx, |session, cx| {
                            cx.notify();
                            stream.replay_telling(session)
                        }),
                        None => stream.replay,
                    }),
                    Err(err) => Source::Failed(err),
                };
                model.place_on_clock(cx);
            },
        );
        Self::new(load, clock)
    }

    fn new(load: Task<()>, clock: Option<DocumentClock>) -> Self {
        Self {
            source: Source::Loading,
            on_clock: clock.map(OnClock::new),
            _load: load,
        }
    }

    /// Registers the replay with the clock as what it plays, or withdraws it
    /// where the replay gives the clock no time.
    fn place_on_clock(&mut self, cx: &mut Context<Self>) {
        let Some(on_clock) = &mut self.on_clock else {
            return;
        };
        let played = match &self.source {
            Source::Loaded(replay) => {
                let replay = replay.get();
                origin(replay, on_clock.document_clock()).and_then(|at| clock_source(replay, at))
            }
            _ => None,
        };
        on_clock.place(played, cx);
    }

    /// The clock the views follow, where the document plays back.
    pub(crate) fn clock(&self) -> Option<&Entity<Clock>> {
        self.on_clock.as_ref().map(OnClock::clock)
    }

    /// The frame the views show, `None` while there is no replay: the one the
    /// clock's time reaches, else the last, showing the finished build.
    pub(crate) fn shown_frame(&self, cx: &App) -> Option<usize> {
        let replay = self.replay()?;
        let reached = self.on_clock.as_ref().and_then(|on_clock| {
            let origin = origin(replay, on_clock.document_clock())?;
            let now = on_clock.clock().read(cx).now()?;
            // At the instants `clock_source` marks the frames at, so stepping
            // to a mark shows its frame.
            Some(last_reached(
                replay.frame_count(),
                |frame| origin.plus_secs_f64(replay.time_of(frame)),
                now,
            ))
        });
        Some(reached.unwrap_or(replay.frame_count() - 1))
    }

    /// Whether the replay, joined to other files, records no wall-clock time
    /// to stand beside theirs on the clock.
    pub(crate) fn unaligned(&self) -> bool {
        match (self.replay(), &self.on_clock) {
            (Some(replay), Some(on_clock)) => origin(replay, on_clock.document_clock()).is_none(),
            _ => false,
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The timeline a view plays, and `None` while the read has landed none, for
    /// which a view draws [`Source::placeholder`] in its stead.
    pub(crate) fn replay(&self) -> Option<&Replay> {
        match &self.source {
            Source::Loaded(replay) => Some(replay.get()),
            _ => None,
        }
    }

    pub(crate) fn cut_off(&self) -> bool {
        self.source.is_cut_off()
    }
}

/// The instant `replay`'s time zero stands at on `clock`, `None` where it
/// records none yet must meet another file's times.
fn origin(replay: &Replay, clock: &DocumentClock) -> Option<EpochMicros> {
    match clock {
        DocumentClock::Joined(_) => replay.epoch_origin(),
        DocumentClock::Alone(_) => Some(replay.epoch_origin().unwrap_or(UNRECORDED_ORIGIN)),
    }
}

/// The last of `count` frames, each at `instant_of` it in time order, that
/// `now` has reached; the first where `now` reaches none.
fn last_reached(
    count: usize,
    instant_of: impl Fn(usize) -> EpochMicros,
    now: EpochMicros,
) -> usize {
    let (mut reached, mut beyond) = (0, count);
    while reached < beyond {
        let mid = reached + (beyond - reached) / 2;
        if instant_of(mid) <= now {
            reached = mid + 1;
        } else {
            beyond = mid;
        }
    }
    reached.saturating_sub(1)
}

/// What `replay` gives a clock with its time zero at `origin`: its span, a step
/// stopping at each frame, `None` where all its frames share one instant.
fn clock_source(replay: &Replay, origin: EpochMicros) -> Option<bviz_ui_playback::Source> {
    let span = Span::new(origin, origin.plus_secs_f64(replay.duration()))?;
    let frames = (0..replay.frame_count()).map(|frame| origin.plus_secs_f64(replay.time_of(frame)));
    let source = bviz_ui_playback::Source::new(span).with_marks(frames);
    Some(if replay.real_time() {
        source
    } else {
        source.approximate()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_reached_at_the_instant_its_mark_stands_at() {
        let origin = EpochMicros::from_millis(1_781_439_245_231);
        // Each rounds down to its microsecond, which in seconds falls short of
        // the frame's own time.
        let times = [0.0, 1.000_000_2, 2.000_000_4];
        let instant_of = |frame: usize| origin.plus_secs_f64(times[frame]);
        for frame in 0..times.len() {
            assert_eq!(
                last_reached(times.len(), instant_of, instant_of(frame)),
                frame
            );
        }
        assert_eq!(
            last_reached(times.len(), instant_of, origin.plus_micros(-1)),
            0
        );
        assert_eq!(
            last_reached(times.len(), instant_of, origin.plus_micros(1_999_999)),
            1
        );
    }
    use bviz_invocation::{BuildOutcome, ConfiguredTarget};

    /// A build whose read has not finished, for exercising what a view reads
    /// off it before one has.
    fn reading() -> ConsoleModel {
        ConsoleModel::new(Task::ready(()), None)
    }

    /// One `BuildEvent` carrying a `Progress` payload whose `stderr` is `text`,
    /// length-delimited as a `--build_event_binary_file` carries it. Bazel emits
    /// the build's console output as a run of these, one per progress event, and
    /// each is one frame of the replay.
    fn progress_event(text: &str) -> Vec<u8> {
        const PROGRESS_IN_EVENT: u8 = 3;
        const STDERR_IN_PROGRESS: u8 = 2;
        // Field tag byte: the field number, then the wire type of a
        // length-delimited field.
        let tag = |field: u8| (field << 3) | 2;
        let progress = [
            &[tag(STDERR_IN_PROGRESS), text.len() as u8],
            text.as_bytes(),
        ]
        .concat();
        let event = [
            &[tag(PROGRESS_IN_EVENT), progress.len() as u8],
            &progress[..],
        ]
        .concat();
        [&[event.len() as u8], &event[..]].concat()
    }

    const CONFIGURATION: &str = "55817e2200e2f24b506d68c96928ed7b9fa185d1fb462c4f511796269d8d06d4";

    /// One `BuildEvent` introducing the configuration `checksum` names under
    /// `mnemonic`, length-delimited like [`progress_event`]'s.
    fn configuration_event(checksum: &str, mnemonic: &str) -> Vec<u8> {
        const ID_IN_EVENT: u8 = 1;
        const CONFIGURATION_IN_EVENT_ID: u8 = 15;
        const ID_IN_CONFIGURATION_ID: u8 = 1;
        const CONFIGURATION_IN_EVENT: u8 = 17;
        const MNEMONIC_IN_CONFIGURATION: u8 = 1;
        // A varint carries seven bits to a byte, low bits first, the high bit
        // of each byte saying another follows; a field's tag is one of its
        // number and the wire type of a length-delimited field.
        let field = |number: u8, value: &[u8]| {
            let tag = (u16::from(number) << 3) | 2;
            let mut encoded = vec![(tag & 0x7f) as u8];
            if tag > 0x7f {
                encoded[0] |= 0x80;
                encoded.push((tag >> 7) as u8);
            }
            encoded.push(value.len() as u8);
            encoded.extend_from_slice(value);
            encoded
        };
        let id = field(
            ID_IN_EVENT,
            &field(
                CONFIGURATION_IN_EVENT_ID,
                &field(ID_IN_CONFIGURATION_ID, checksum.as_bytes()),
            ),
        );
        let configuration = field(
            CONFIGURATION_IN_EVENT,
            &field(MNEMONIC_IN_CONFIGURATION, mnemonic.as_bytes()),
        );
        let event = [id, configuration].concat();
        [&[event.len() as u8], &event[..]].concat()
    }

    /// A two-frame build, read the way one named on the command line is read.
    fn loaded() -> ConsoleModel {
        let stream = [progress_event("Loading: \n"), progress_event("Analyzing\n")].concat();
        let stream = Stream::from_slice(&stream).expect("the stream parses");
        let mut model = reading();
        model.source = Source::Loaded(stream.replay);
        model
    }

    /// A stream that lands in a session tells it of the configurations it
    /// describes and is replayed whole.
    #[test]
    fn a_stream_read_in_a_session_names_its_configurations_there() {
        let stream = [
            configuration_event(CONFIGURATION, "k8-fastbuild"),
            progress_event("Loading: \n"),
        ]
        .concat();
        let stream = Stream::from_slice(&stream).expect("the stream parses");
        let mut session = Session::default();
        let replay = stream.replay_telling(&mut session);
        assert_eq!(replay.get().frame_count(), 1);
        assert_eq!(
            session.configurations().name(CONFIGURATION).as_deref(),
            Some("k8-fastbuild")
        );
    }

    /// One `BuildEvent` reporting that `label`, in the configuration
    /// `checksum` names, was built.
    fn built_event(label: &str, checksum: &str) -> Vec<u8> {
        use bviz_terminal::bep::build_event_stream::{
            BuildEvent, BuildEventId, TargetComplete,
            build_event::Payload,
            build_event_id::{ConfigurationId, Id, TargetCompletedId},
        };
        use prost::Message;

        BuildEvent {
            id: Some(BuildEventId {
                id: Some(Id::TargetCompleted(TargetCompletedId {
                    label: label.into(),
                    configuration: Some(ConfigurationId {
                        id: checksum.into(),
                    }),
                    ..Default::default()
                })),
            }),
            payload: Some(Payload::Completed(TargetComplete {
                success: true,
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    #[test]
    fn a_stream_read_in_a_session_reports_its_outcomes_there() {
        let stream = built_event("//app", CONFIGURATION);
        let stream = Stream::from_slice(&stream).expect("the stream parses");
        let mut session = Session::default();
        stream.replay_telling(&mut session);
        let app = ConfiguredTarget::new("//app", Some(CONFIGURATION));
        assert_eq!(session.outcomes().build(app), Some(&BuildOutcome::Built));
    }

    /// A stream describing no configuration leaves a session knowing what it
    /// knew.
    #[test]
    fn a_stream_describing_no_configuration_adds_none() {
        let stream = Stream::from_slice(&progress_event("Loading: \n")).expect("the stream parses");
        let mut session = Session::default();
        stream.replay_telling(&mut session);
        assert_eq!(session.configurations().name(CONFIGURATION), None);
    }

    /// What a view plays is read off the build alone, so it answers the same
    /// whether or not a panel is showing one.
    #[test]
    fn a_build_still_being_read_has_nothing_to_play() {
        assert!(reading().replay().is_none());
    }

    /// A read that failed leaves nothing to play and holds on to why, which is
    /// what a view shows in the timeline's place.
    #[test]
    fn a_build_that_could_not_be_read_says_why() {
        let mut model = reading();
        model.source = Source::Failed("build.bep: no such file".into());
        assert!(model.replay().is_none());
        let Source::Failed(why) = model.source() else {
            panic!("the read failed")
        };
        assert_eq!(why, "build.bep: no such file");
    }

    /// A replay whose frames all share one instant has no time for a clock to
    /// play, wherever its time zero stands.
    #[test]
    fn a_replay_of_one_instant_gives_a_clock_nothing_to_play() {
        let stream = Stream::from_slice(&progress_event("Loading: \n")).expect("the stream parses");
        assert!(clock_source(stream.replay.get(), UNRECORDED_ORIGIN).is_none());
    }

    /// The event Bazel ends a stream with, marked its last.
    fn last_event() -> Vec<u8> {
        use bviz_terminal::bep::build_event_stream::BuildEvent;
        use prost::Message;

        BuildEvent {
            last_message: true,
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    /// A stream that ends before the event Bazel marks its last is played as
    /// far as it goes, and said to be cut off.
    #[test]
    fn a_stream_ending_early_is_played_up_to_the_cut() {
        let whole = [progress_event("Loading: \n"), last_event()].concat();
        let cut = &whole[..whole.len() - 1];
        for (stream, cut_off) in [(&whole[..], false), (cut, true)] {
            let mut model = reading();
            model.source = Source::Loaded(
                Stream::from_slice(stream)
                    .expect("the stream parses")
                    .replay,
            );
            assert_eq!(model.cut_off(), cut_off);
            assert_eq!(model.replay().expect("a replay").frame_count(), 1);
        }
    }

    /// Every progress event of the stream read is a frame the reader can seek to.
    #[test]
    fn a_loaded_build_plays_what_was_read() {
        let model = loaded();
        let replay = model.replay().expect("the read landed a build");
        assert_eq!(replay.frame_count(), 2);
    }
}
