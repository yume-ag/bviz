//! Reader for `--build_event_binary_file` streams.
//!
//! The file is a sequence of length-delimited `BuildEvent` messages.

use std::io::Read;

use bviz_input::delimited::{self, Delimited, Next};
use bviz_input::{Input, reserved};
use bviz_invocation::{Configurations, InvocationId, Outcomes, Recorded};
use prost::Message;

use crate::bep::build_event_stream::{BuildEvent, build_event::Payload, build_event_id::Id};
use crate::bep::command_line::{CommandLine, command_line_section::SectionType};
use crate::bep::timestamp_proto::google::protobuf::Timestamp;
use crate::columns::Columns;
use crate::outcomes::{self, checksum};

/// What a build event stream tells of the build that wrote it.
pub struct BuildOutput {
    /// The configurations the stream describes (its `Configuration` events).
    pub configurations: Configurations,
    pub outcomes: Outcomes,
    /// Terminal width Bazel rendered for (`--terminal_columns`), or Bazel's
    /// fallback where the stream names none the emulator can run at.
    pub(crate) columns: Columns,
    /// stdout and stderr concatenated in stream order. `bazel build` writes
    /// everything — progress, messages, forwarded action output — to stderr and
    /// leaves stdout empty; the query-family commands (`query`/`cquery`/`aquery`/
    /// `info`) emit their results on stdout instead. Within one progress event
    /// stderr is appended before stdout, keeping progress ahead of any such results.
    pub bytes: Vec<u8>,
    /// The end offset into [`bytes`](Self::bytes) of each progress event, in stream
    /// order: replaying `bytes[..frames[i]]` reproduces the terminal as it stood
    /// after the i-th progress event. One entry per progress event; the last is
    /// `bytes.len()`. This is the unit of playback — each progress event is a frame.
    pub(crate) frames: Vec<usize>,
    /// Wall-clock anchors recovered from the timestamp-bearing events, each tying a
    /// time to the point in the frame sequence where the event occurred. In stream
    /// order. Bazel does not timestamp progress events themselves, so these anchor
    /// an interpolated timeline when the output carries no `--show_timestamps`
    /// markers; see [`crate::Replay`].
    pub(crate) anchors: Vec<Anchor>,
    /// Whether the build was run with `--show_timestamps`, read from its structured
    /// command line. When set, the output carries `(HH:MM:SS)` markers that give a
    /// real timeline; otherwise the timeline is interpolated from [`anchors`].
    pub(crate) show_timestamps: bool,
}

/// A wall-clock time tied to a position in the progress-frame sequence.
pub(crate) struct Anchor {
    /// The number of progress frames emitted before the anchoring event, i.e. an
    /// index into [`BuildOutput::frames`].
    pub(crate) frame: usize,
    /// The event's wall-clock time, in seconds since the Unix epoch.
    pub(crate) time: f64,
}

#[cfg(test)]
impl BuildOutput {
    /// A single-frame output that carries no clock: the base whose fields a
    /// test overrides.
    pub(crate) fn from_bytes(columns: u16, bytes: Vec<u8>) -> BuildOutput {
        BuildOutput {
            configurations: Configurations::default(),
            outcomes: Outcomes::default(),
            columns: Columns::new(columns).expect("a width the emulator runs at"),
            frames: vec![bytes.len()],
            bytes,
            anchors: Vec::new(),
            show_timestamps: false,
        }
    }
}

/// Stream bytes per byte of console output, rounded down from the most
/// output-laden streams measured so the output is reserved at or above its
/// final length.
const STREAM_BYTES_PER_OUTPUT_BYTE: usize = 3;

/// Stream bytes per progress event, rounded down like
/// [`STREAM_BYTES_PER_OUTPUT_BYTE`].
const STREAM_BYTES_PER_FRAME: usize = 2 << 10;

/// Stream bytes per timestamped event, rounded down like
/// [`STREAM_BYTES_PER_OUTPUT_BYTE`].
const STREAM_BYTES_PER_ANCHOR: usize = 8 << 10;

/// Why a build event binary stream cannot be read.
#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    Decode(prost::DecodeError),
    /// The stream holds no whole event, as no build event stream does.
    NoEvent,
}

impl From<prost::DecodeError> for ReadError {
    fn from(e: prost::DecodeError) -> Self {
        ReadError::Decode(e)
    }
}

impl From<delimited::Error> for ReadError {
    fn from(e: delimited::Error) -> Self {
        match e {
            delimited::Error::Io(e) => ReadError::Io(e),
            delimited::Error::Delimiter(e) => ReadError::Decode(e),
        }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "cannot read build event stream: {e}"),
            ReadError::Decode(e) => write!(f, "cannot parse build event stream: {e}"),
            ReadError::NoEvent => write!(f, "not a build event stream"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Io(e) => Some(e),
            ReadError::Decode(e) => Some(e),
            ReadError::NoEvent => None,
        }
    }
}

/// Read a build event binary stream event by event: whole, where it runs to
/// the event Bazel marks its last, and up to where it ends otherwise. An event
/// that does not decode fails the read, as does a stream that holds no whole
/// event. `size`, the stream's length in bytes where known, sizes the output.
pub fn read(stream: impl Read, size: Option<u64>) -> Result<Recorded<BuildOutput>, ReadError> {
    let size = size.map_or(0, |size| usize::try_from(size).unwrap_or(usize::MAX));
    let mut bytes = reserved(size / STREAM_BYTES_PER_OUTPUT_BYTE);
    let mut frames = reserved(size / STREAM_BYTES_PER_FRAME);
    let mut anchors = reserved(size / STREAM_BYTES_PER_ANCHOR);
    let mut configurations = Configurations::default();
    let mut outcomes = Outcomes::default();
    let mut columns = None;
    let mut show_timestamps = false;
    let mut last_read = false;
    let mut any_event = false;
    let mut events = Delimited::buffered(stream);
    let ended_whole = loop {
        let event = match events.next(|event: &[u8]| BuildEvent::decode(event))? {
            Next::Message(event) => event?,
            Next::End => break true,
            Next::CutOff => break false,
        };
        let BuildEvent {
            id,
            children,
            payload,
            last_message,
            ..
        } = event;
        any_event = true;
        last_read |= last_message;
        let id = id.and_then(|id| id.id);
        // An anchoring event ties its time to the frame boundary it falls on: the
        // number of progress frames already emitted.
        let frame = frames.len();
        match payload {
            Some(Payload::Progress(progress)) => {
                bytes.extend_from_slice(progress.stderr.as_bytes());
                bytes.extend_from_slice(progress.stdout.as_bytes());
                frames.push(bytes.len());
            }
            Some(Payload::Started(s)) => record(&mut anchors, frame, epoch_opt(s.start_time)),
            Some(Payload::Finished(f)) => record(&mut anchors, frame, epoch_opt(f.finish_time)),
            Some(Payload::Action(a)) => {
                record(&mut anchors, frame, epoch_opt(a.end_time));
                outcomes::action(&mut outcomes, id, a);
            }
            Some(Payload::TestSummary(s)) => {
                record(&mut anchors, frame, epoch_opt(s.last_stop_time));
                outcomes::test_summary(&mut outcomes, id, s);
            }
            Some(Payload::Completed(c)) => {
                outcomes::completed(&mut outcomes, id, children, c.success)
            }
            Some(Payload::Aborted(a)) => outcomes::aborted(&mut outcomes, id, children, a),
            Some(Payload::Configuration(c)) => {
                if let Some(Id::Configuration(id)) = id
                    && let Some(checksum) = checksum(Some(id))
                {
                    configurations.insert(checksum, c.mnemonic);
                }
            }
            Some(Payload::StructuredCommandLine(command_line)) => {
                columns = columns.or_else(|| terminal_columns(&command_line));
                show_timestamps |= show_timestamps_set(&command_line);
            }
            _ => {}
        }
    };
    if !any_event {
        return Err(ReadError::NoEvent);
    }
    let output = BuildOutput {
        configurations,
        outcomes,
        columns: columns.unwrap_or(Columns::DEFAULT),
        bytes,
        frames,
        anchors,
        show_timestamps,
    };
    Ok(match ended_whole && last_read {
        true => Recorded::Complete(output),
        false => Recorded::CutOff(output),
    })
}

/// [`read`] over a stream held in memory.
#[cfg(test)]
pub(crate) fn read_slice(stream: &[u8]) -> Result<Recorded<BuildOutput>, ReadError> {
    read(stream, Some(stream.len() as u64))
}

/// The invocation that wrote the build event binary stream `input` holds: the
/// first `BuildStarted.uuid` that is not empty, among the events wholly within
/// the input's head. `None` when the head holds no such event.
pub fn probe_invocation_id(input: &Input) -> Option<InvocationId> {
    invocation_id_in_head(input.head()?)
}

/// [`probe_invocation_id`] over the events wholly within `head`.
fn invocation_id_in_head(head: &[u8]) -> Option<InvocationId> {
    let mut events = Delimited::new(head);
    loop {
        let Next::Message(event) = events.next(|event: &[u8]| BuildEvent::decode(event)).ok()?
        else {
            return None;
        };
        if let Some(Payload::Started(started)) = event.ok()?.payload
            && let Some(id) = InvocationId::new(&started.uuid)
        {
            return Some(id);
        }
    }
}

/// Record a wall-clock anchor at a frame boundary, skipping events whose
/// timestamp is absent.
fn record(anchors: &mut Vec<Anchor>, frame: usize, time: Option<f64>) {
    if let Some(time) = time {
        anchors.push(Anchor { frame, time });
    }
}

/// Seconds since the Unix epoch for a `google.protobuf.Timestamp`'s components.
fn epoch(seconds: i64, nanos: i32) -> f64 {
    seconds as f64 + nanos as f64 / 1e9
}

fn epoch_opt(ts: Option<Timestamp>) -> Option<f64> {
    ts.map(|t| epoch(t.seconds, t.nanos))
}

/// The value of a named option in a structured command line, if present.
fn option_value<'a>(command_line: &'a CommandLine, name: &str) -> Option<&'a str> {
    command_line
        .sections
        .iter()
        .filter_map(|section| match &section.section_type {
            Some(SectionType::OptionList(options)) => Some(options),
            _ => None,
        })
        .flat_map(|options| &options.option)
        .find(|option| option.option_name == name)
        .map(|option| option.option_value.as_str())
}

fn terminal_columns(command_line: &CommandLine) -> Option<Columns> {
    option_value(command_line, "terminal_columns")
        .and_then(|value| value.parse().ok())
        .and_then(Columns::new)
}

/// Whether `--show_timestamps` is enabled, as a boolean option Bazel canonicalizes
/// to `1`/`true`.
fn show_timestamps_set(command_line: &CommandLine) -> bool {
    option_value(command_line, "show_timestamps")
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bep::build_event_stream::{self as bes, BuildEventId, BuildStarted, build_event_id};
    use crate::outcomes::NULL_CONFIGURATION;

    // A real `--build_event_binary_file` from `bazel build … --terminal_columns=100`.
    const SAMPLE: &[u8] = include_bytes!("../testdata/sample.bep");

    #[test]
    fn reads_columns_and_output() {
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        assert_eq!(output.columns.get(), 100);
        // Bazel's curses progress display emits cursor-control escape sequences.
        assert!(
            output.bytes.contains(&0x1b),
            "expected ANSI escapes in output"
        );
    }

    /// A length-delimited `Configuration` event introducing `id`.
    fn configuration_event(id: &str, mnemonic: &str) -> Vec<u8> {
        BuildEvent {
            id: Some(BuildEventId {
                id: Some(Id::Configuration(build_event_id::ConfigurationId {
                    id: id.into(),
                })),
            }),
            payload: Some(Payload::Configuration(bes::Configuration {
                mnemonic: mnemonic.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    #[test]
    fn reads_configurations_by_checksum() {
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        // The sample's two configurations share a mnemonic.
        let target = "55817e2200e2f24b506d68c96928ed7b9fa185d1fb462c4f511796269d8d06d4";
        let other = "91e24984663c795f7d9f904758d4b84ba1954389b2ab756f70a6f32db2a79020";
        let known = &output.configurations;
        assert_eq!(known.name(target).as_deref(), Some("k8-fastbuild 55817e"));
        assert_eq!(known.name(other).as_deref(), Some("k8-fastbuild 91e249"));
    }

    #[test]
    fn skips_a_configuration_event_that_names_no_configuration() {
        let stream = [
            configuration_event(NULL_CONFIGURATION, "null"),
            configuration_event("", "unnamed"),
            configuration_event("0c3d9a77", "k8-opt-exec"),
        ]
        .concat();
        let known = read_slice(&stream)
            .expect("parse stream")
            .into_inner()
            .configurations;
        assert_eq!(known.name(NULL_CONFIGURATION), None);
        assert_eq!(known.name(""), None);
        assert_eq!(known.name("0c3d9a77").as_deref(), Some("k8-opt-exec"));
    }

    /// A length-delimited `StructuredCommandLine` event naming
    /// `--terminal_columns=value`.
    fn terminal_columns_event(value: &str) -> Vec<u8> {
        use crate::bep::command_line::{self as cl, CommandLineSection};
        BuildEvent {
            payload: Some(Payload::StructuredCommandLine(CommandLine {
                sections: vec![CommandLineSection {
                    section_type: Some(SectionType::OptionList(cl::OptionList {
                        option: vec![cl::Option {
                            option_name: "terminal_columns".into(),
                            option_value: value.into(),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    /// A length-delimited `Progress` event writing `stderr`.
    fn progress_event(stderr: &str) -> Vec<u8> {
        BuildEvent {
            payload: Some(Payload::Progress(bes::Progress {
                stderr: stderr.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    #[test]
    fn falls_back_from_a_width_the_emulator_cannot_hold() {
        for width in ["0", "1", "65535", "-3", "wide"] {
            let stream = [terminal_columns_event(width), progress_event("built\n")].concat();
            let output = read_slice(&stream).expect("parse stream").into_inner();
            assert_eq!(
                output.columns,
                Columns::DEFAULT,
                "--terminal_columns={width}"
            );
            let screen = crate::screen::Screen::from_output(&output);
            assert_eq!(screen.text(), "built", "--terminal_columns={width}");
        }
    }

    #[test]
    fn takes_the_first_width_the_emulator_can_hold() {
        let stream = [terminal_columns_event("0"), terminal_columns_event("120")].concat();
        assert_eq!(
            read_slice(&stream)
                .expect("parse stream")
                .into_inner()
                .columns
                .get(),
            120
        );
    }

    /// The offset each of the sample's events ends at, in stream order.
    fn sample_event_ends() -> Vec<usize> {
        let mut rest = SAMPLE;
        let mut ends = Vec::new();
        while !rest.is_empty() {
            BuildEvent::decode_length_delimited(&mut rest).expect("sample event");
            ends.push(SAMPLE.len() - rest.len());
        }
        ends
    }

    #[test]
    fn reads_a_stream_ending_in_its_last_event_whole() {
        assert!(matches!(read_slice(SAMPLE), Ok(Recorded::Complete(_))));
    }

    #[test]
    fn a_stream_cut_off_keeps_the_events_ahead_of_the_cut() {
        let cut_off = |cut: usize| match read_slice(&SAMPLE[..cut]) {
            Ok(Recorded::CutOff(output)) => output,
            _ => panic!("a stream cut at {cut} is cut off"),
        };
        let ends = sample_event_ends();
        // Each event after the first but the last, which Bazel marks the last,
        // cut before, inside, and right after its start.
        for (&start, &end) in ends.iter().zip(&ends[1..ends.len() - 1]) {
            let kept = cut_off(start);
            for cut in [start + 1, (start + end) / 2, end - 1] {
                let output = cut_off(cut);
                assert_eq!(output.bytes, kept.bytes, "cut at {cut}");
                assert_eq!(output.frames, kept.frames, "cut at {cut}");
            }
        }
    }

    #[test]
    fn a_file_that_holds_no_whole_event_is_no_stream() {
        const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
        let first_end = sample_event_ends()[0];
        for stream in [
            &[][..],
            &SAMPLE[..1],
            &SAMPLE[..first_end - 1],
            PNG_SIGNATURE,
        ] {
            assert!(
                matches!(read_slice(stream), Err(ReadError::NoEvent)),
                "{stream:?}"
            );
        }
    }

    #[test]
    fn an_event_that_does_not_decode_fails_the_read() {
        let mut stream = progress_event("built\n");
        let garbled = [0xff; 3];
        prost::encode_length_delimiter(garbled.len(), &mut stream).expect("delimiter");
        stream.extend(garbled);
        assert!(read_slice(&stream).is_err());
    }

    /// Every byte continuing the varint, one past its longest encoding.
    const UNENDING_DELIMITER: [u8; 11] = [0x80; 11];

    #[test]
    fn a_delimiter_running_past_its_longest_encoding_fails_the_read() {
        assert!(matches!(
            read_slice(&UNENDING_DELIMITER),
            Err(ReadError::Decode(_))
        ));
    }

    #[test]
    fn probe_reads_the_id_within_the_head() {
        let id = InvocationId::new("a5f7f3e6-4fda-4675-bc16-8ede5420b210");
        assert_eq!(invocation_id_in_head(SAMPLE), id);

        // `BuildStarted` is the sample's first event; a head ending inside
        // it holds no id, and one ending right after it does.
        let first_event = sample_event_ends()[0];
        assert_eq!(invocation_id_in_head(&SAMPLE[..first_event - 1]), None);
        assert_eq!(invocation_id_in_head(&SAMPLE[..first_event]), id);

        // Events ahead of `BuildStarted` are passed over.
        let mut late = configuration_event("0c3d9a77", "k8-opt-exec");
        let events_ahead = late.len();
        late.extend(SAMPLE);
        assert_eq!(invocation_id_in_head(&late), id);
        assert_eq!(invocation_id_in_head(&late[..events_ahead]), None);
        assert_eq!(invocation_id_in_head(&[]), None);
    }

    /// A length-delimited `BuildStarted` event of the invocation `uuid`.
    fn started_event(uuid: &str) -> Vec<u8> {
        BuildEvent {
            payload: Some(Payload::Started(BuildStarted {
                uuid: uuid.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    #[test]
    fn probe_reads_the_first_id_that_is_not_empty() {
        let stream = ["", "first", "second"].map(started_event).concat();
        assert_eq!(invocation_id_in_head(&stream), InvocationId::new("first"));
    }

    #[test]
    fn probe_stops_at_an_event_longer_than_the_head() {
        let mut stream = Vec::new();
        prost::encode_length_delimiter(usize::MAX, &mut stream).expect("delimiter");
        stream.extend(started_event("late"));
        assert_eq!(invocation_id_in_head(&stream), None);
    }

    #[test]
    fn probe_reads_a_stream_from_disk() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join("probe.bep");
        std::fs::write(&path, SAMPLE).unwrap();
        let input = Input::open(&path).unwrap();
        assert_eq!(
            probe_invocation_id(&input),
            InvocationId::new("a5f7f3e6-4fda-4675-bc16-8ede5420b210")
        );
    }

    #[test]
    fn frames_partition_the_output() {
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        // Every progress event yields a frame, and frame ends are non-decreasing
        // offsets running up to the full output.
        assert!(!output.frames.is_empty());
        assert!(output.frames.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(output.frames.last().copied(), Some(output.bytes.len()));
    }

    #[test]
    fn detects_show_timestamps_off() {
        // The sample was captured without --show_timestamps.
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        assert!(!output.show_timestamps);
    }

    #[test]
    fn anchors_bracket_the_build() {
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        // A completed build carries at least its start and finish times, in
        // non-decreasing wall-clock order, each tied to a real frame boundary.
        assert!(
            output.anchors.len() >= 2,
            "expected start and finish anchors"
        );
        assert!(output.anchors.windows(2).all(|w| w[0].time <= w[1].time));
        assert!(
            output
                .anchors
                .iter()
                .all(|a| a.frame <= output.frames.len())
        );
    }
}
