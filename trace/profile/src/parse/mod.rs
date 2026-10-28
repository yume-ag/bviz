use std::fmt;
use std::io::{BufRead, Read};
use std::path::Path;

use bviz_input::Input;
use bviz_invocation::{InvocationId, Recorded};
use flate2::bufread::MultiGzDecoder;
use serde::de::DeserializeSeed;
use serde_json::{Map, Value};

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow, Writer};
use crate::model::Profile;

mod build;
mod decode;
mod pipeline;
mod scanner;
mod serde_events;
mod stream;

use build::{Builder, Merge, seal};
use decode::{decode, latin1_cow};
use pipeline::{TraceEventsArray, find_trace_events_array, find_trace_events_key, parse_parallel};
use scanner::object_end;
use serde_events::{ArgEntry, ProfileVisitor, RawEventSeed};
#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
use stream::pipelined::{parse_gz_pipelined, parse_gz_pipelined_input};
#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
use stream::serial::{Ceding, parse_serial_input};
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
use stream::{Unpaused, run_here, serial::parse_serial};

pub use decode::gzip_size_hint;
#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
pub use stream::pipelined::pipeline_chunk_bytes;

/// Estimated distinct-string density of a Bazel profile — about one interned
/// string per this many decoded bytes — used to pre-size the shared interner's
/// dedup index. Measured near one distinct string per 167 bytes on a large
/// profile; rounding down errs toward a larger index, which only wastes a
/// transient, freed-at-seal allocation, whereas over-counting the bytes
/// under-sizes the shard tables and costs a rehash.
const BYTES_PER_DISTINCT: usize = 160;

/// Densest slice density of a Bazel profile — the fewest decoded bytes per
/// `X`/`i` event (staged slice) any one chunk reaches — used to pre-size a
/// parse chunk's `staging` buffer to the worst-case, not average, density so no
/// chunk regrows. The densest chunks of a large profile pack a slice per ~112
/// bytes; sizing below that reserves at least as many entries as even the
/// densest chunk stages. The extra capacity an average chunk over-reserves is
/// transient, freed at the merge, in exchange for no regrowth.
const BYTES_PER_SLICE: usize = 100;

/// Fraction of decoded bytes to pre-fault into the interner arena, as
/// [`INTERN_PREFAULT_NUM`]/[`INTERN_PREFAULT_DEN`]: distinct text measures about
/// 30% of a Bazel profile's decoded bytes and the arena bump-fills from offset 0,
/// so the real faults live in that leading prefix. Pre-faulting ~40% (≈1.3× the
/// measured density) covers them with headroom while limiting the never-written
/// tail beyond the interned text.
const INTERN_PREFAULT_NUM: usize = 2;
const INTERN_PREFAULT_DEN: usize = 5;

/// Advisory size for the interner arena's page pre-fault — the byte prefix each
/// parse worker faults in (its own page-aligned share) before its receive loop,
/// so the workers' first touches do not fault lazily and serially in the hot
/// loop. Unlike [`BYTES_PER_DISTINCT`], which sizes the dedup index, this is not
/// a capacity bound: [`ConcurrentInterner::prefault_shard`] clamps it to the
/// arena's capacity, so too small merely leaves a lazy tail and too large only
/// over-faults — it can cost bandwidth, never correctness.
fn intern_prefault_bytes(total_bytes: usize) -> usize {
    total_bytes / INTERN_PREFAULT_DEN * INTERN_PREFAULT_NUM
}

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Decompress(std::io::Error),
    Json(serde_json::Error),
    Corrupted(String),
    /// A string pool sized for the profile would reach past the offsets its
    /// `u32` handles index.
    TooLarge,
    /// The profile's distinct text outgrew the string pool sized for it.
    PoolExhausted,
}

impl From<PoolOverflow> for LoadError {
    fn from(overflow: PoolOverflow) -> LoadError {
        match overflow {
            PoolOverflow::TooLarge => LoadError::TooLarge,
            PoolOverflow::Exhausted => LoadError::PoolExhausted,
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "i/o error: {e}"),
            LoadError::Decompress(e) => write!(f, "decompression error: {e}"),
            LoadError::Json(e) => write!(f, "json error: {e}"),
            LoadError::Corrupted(m) => write!(f, "corrupted profile: {m}"),
            LoadError::TooLarge => write!(f, "profile too large for a 4 GiB string pool"),
            LoadError::PoolExhausted => {
                write!(f, "the profile's distinct text outgrew its string pool")
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Io(e) | LoadError::Decompress(e) => Some(e),
            LoadError::Json(e) => Some(e),
            LoadError::Corrupted(_) | LoadError::TooLarge | LoadError::PoolExhausted => None,
        }
    }
}

/// Streams the top-level profile object straight into a [`Builder`]: each
/// `traceEvents` element is interned into the arena as it is parsed, so no
/// intermediate `Vec<RawEvent>` is materialized and each event's strings are
/// visited once. `otherData` may appear before or after the array; it is
/// independent of the events, so it is folded in whenever it is seen.
fn parse_streaming(text: &str) -> Result<Profile, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_str(text);
    let profile = serde::Deserializer::deserialize_map(
        &mut de,
        ProfileVisitor {
            text_len: text.len(),
        },
    )?;
    de.end()?;
    Ok(profile)
}

/// The switch that has each stage of loading and presenting a profile report
/// how long it took.
pub const TIMING_SWITCH: &str = "BVIZ_TRACE_TIMING";

/// Wall-clock timing of a load stage, reported when [`TIMING_SWITCH`] is set.
/// Returns the closure's value so a stage can be wrapped inline.
fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let Some(start) = timing_start() else {
        return f();
    };
    let out = f();
    report_timing(label, start);
    out
}

/// [`timed`] of a stage that awaits, timed from its first poll to its last.
#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
async fn timed_future<T>(label: &str, stage: impl Future<Output = T>) -> T {
    let Some(start) = timing_start() else {
        return stage.await;
    };
    let out = stage.await;
    report_timing(label, start);
    out
}

/// Now, where [`TIMING_SWITCH`] is set.
fn timing_start() -> Option<bviz_time::Instant> {
    bviz_switches::is_set(TIMING_SWITCH).then(bviz_time::now)
}

fn report_timing(label: &str, start: bviz_time::Instant) {
    bviz_switches::report(format_args!(
        "[timing] {label}: {:.3}s",
        start.elapsed().as_secs_f64()
    ));
}

impl Profile {
    /// Loads the profile at `path`, as [`from_input`](Self::from_input) does.
    pub fn from_path(path: &Path) -> Result<Recorded<Profile>, LoadError> {
        Profile::from_input(Input::open(path).map_err(LoadError::Io)?)
    }

    /// Loads a profile. A seekable input goes first to this build's streamed
    /// decode — `parse_gz_pipelined_input` built with threads,
    /// `parse_serial_input` built without — which reads it without pulling
    /// it in whole; what that declines is read whole and goes through
    /// [`from_whole_buffer`]. A stream, which cannot be read twice, is read
    /// whole and goes through [`from_bytes`](Self::from_bytes).
    pub fn from_input(input: Input) -> Result<Recorded<Profile>, LoadError> {
        if input.seekable().is_none() {
            let bytes = timed("read input", || input.read_all().map_err(LoadError::Io))?;
            return Profile::from_bytes(&bytes);
        }
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        if let Some(profile) = parse_gz_pipelined_input(&input) {
            return Ok(Recorded::Complete(profile));
        }
        #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
        if let Some(recorded) = run_here(parse_serial_input(&input, &mut Unpaused)) {
            return Ok(recorded);
        }
        from_whole_input(input)
    }

    /// [`from_input`](Self::from_input) built without threads, handing the
    /// thread back to whatever shares it: the serial decode awaits `cede`
    /// between its pieces once it has run for `every` since it last did.
    #[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
    pub async fn from_input_ceding<F: Future<Output = ()> + Send>(
        input: Input,
        every: std::time::Duration,
        cede: impl FnMut() -> F + Send,
    ) -> Result<Recorded<Profile>, LoadError> {
        if input.seekable().is_none() {
            return Profile::from_input(input);
        }
        if let Some(recorded) = parse_serial_input(&input, &mut Ceding::new(every, cede)).await {
            return Ok(recorded);
        }
        from_whole_input(input)
    }

    /// [`from_input`](Self::from_input), whose decode built with threads runs
    /// on threads of its own and so never cedes.
    #[cfg(all(target_family = "wasm", target_feature = "atomics"))]
    pub async fn from_input_ceding<F: Future<Output = ()> + Send>(
        input: Input,
        _every: std::time::Duration,
        _cede: impl FnMut() -> F + Send,
    ) -> Result<Recorded<Profile>, LoadError> {
        Profile::from_input(input)
    }

    /// Parses a profile. It goes first to this build's decode —
    /// `parse_gz_pipelined` built with threads, `parse_serial` built without
    /// — and what that declines goes through [`from_whole_buffer`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Recorded<Profile>, LoadError> {
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        if bytes.starts_with(&GZIP_MAGIC)
            && let Some(profile) = timed("decode+parse (pipelined)", || parse_gz_pipelined(bytes))
        {
            return Ok(Recorded::Complete(profile));
        }
        #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
        if let Some(recorded) = parse_serial(bytes) {
            return Ok(recorded);
        }
        from_whole_buffer(bytes)
    }
}

/// [`from_whole_buffer`] of all of `input`, read into memory.
fn from_whole_input(input: Input) -> Result<Recorded<Profile>, LoadError> {
    let bytes = timed("read input", || input.read_all().map_err(LoadError::Io))?;
    from_whole_buffer(&bytes)
}

/// Decodes a profile whole into memory and parses that text: the parser works
/// over a slice (`SliceRead`), far faster than the byte-at-a-time
/// `from_reader` (`IoRead`) on a trace of millions of events, and a profile
/// compresses small enough that the decoded text fits comfortably. A parse
/// failure — most often a profile truncated by an interrupted Bazel — falls
/// back to the best-effort [`recover`], as does a gzip stream cut off before
/// its end, without the parses its text cannot pass. Whole text whose
/// traceEvents array closes was not cut off, so it fails with where the parse
/// stopped, as does text that recovers nothing. A recovered profile is cut
/// off.
fn from_whole_buffer(bytes: &[u8]) -> Result<Recorded<Profile>, LoadError> {
    let decoded = match timed("decode (decompress+transcode)", || decode(bytes))? {
        Recorded::Complete(text) => text,
        Recorded::CutOff(text) => {
            return timed("recover", || recover(&text))
                .map(|(profile, _)| Recorded::CutOff(profile));
        }
    };
    if let Some(profile) = timed("parse+build (parallel)", || parse_parallel(&decoded)) {
        return Ok(Recorded::Complete(profile));
    }
    match timed("parse+build (serial)", || parse_streaming(&decoded)) {
        Ok(profile) => Ok(Recorded::Complete(profile)),
        Err(json) => match recover(&decoded) {
            Ok((profile, ArrayEnd::CutOff)) => Ok(Recorded::CutOff(profile)),
            Ok((_, ArrayEnd::Closed)) | Err(_) => Err(LoadError::Json(json)),
        },
    }
}

/// Where [`recover_events`] stopped reading the traceEvents array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayEnd {
    Closed,
    /// The text ends inside the array.
    CutOff,
}

/// Best-effort parse of a truncated profile: read otherData if present and pull
/// whole events out of the traceEvents array until the first incomplete one.
fn recover(text: &str) -> Result<(Profile, ArrayEnd), LoadError> {
    let (events_at, array_start) = match find_trace_events_array(text.as_bytes()) {
        TraceEventsArray::Found {
            key_at,
            content_start,
        } => (key_at, content_start),
        TraceEventsArray::NoKey => return Err(LoadError::Corrupted("no traceEvents array".into())),
        TraceEventsArray::NoArray => {
            return Err(LoadError::Corrupted("traceEvents not an array".into()));
        }
    };

    let other_data = recover_other_data(&text[..events_at]);

    let interner = ConcurrentInterner::new(text.len(), text.len() / BYTES_PER_DISTINCT)?;
    let mut builder = Builder::new();
    let (recovered, end) = {
        let mut writer = interner.writer();
        recover_events(text, array_start, &mut builder, &mut writer)?
    };

    if !recovered {
        return Err(LoadError::Corrupted("no complete events recovered".into()));
    }
    builder.seal_slices();
    let mut merge = Merge::new();
    merge.fold(builder);
    let profile = seal(interner, merge, other_data)?;
    Ok((profile, end))
}

/// Folds whole event objects from the traceEvents array into `builder`, starting
/// at `array_start`, until the array closes or the text ends. Returns whether
/// any event was recovered, and where the scan stopped. A whole object that is
/// no event, or anything but a separator between objects, fails: it is no
/// part of a cut.
fn recover_events(
    text: &str,
    array_start: usize,
    builder: &mut Builder,
    writer: &mut Writer,
) -> Result<(bool, ArrayEnd), LoadError> {
    let mut recovered = false;
    let mut scratch: Vec<ArgEntry> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = array_start;
    loop {
        match bytes.get(i) {
            None => return Ok((recovered, ArrayEnd::CutOff)),
            Some(b']') => return Ok((recovered, ArrayEnd::Closed)),
            Some(b',') => i += 1,
            Some(b) if b.is_ascii_whitespace() => i += 1,
            Some(b'{') => {
                let start = i;
                let Some(end) = object_end(bytes, start) else {
                    return Ok((recovered, ArrayEnd::CutOff));
                };
                let mut de = serde_json::Deserializer::from_str(&text[start..=end]);
                let ev = RawEventSeed(&mut scratch)
                    .deserialize(&mut de)
                    .and_then(|ev| de.end().map(|()| ev))
                    .map_err(|e| {
                        LoadError::Corrupted(format!(
                            "the event at byte {start} does not parse: {e}"
                        ))
                    })?;
                builder.event(writer, ev, &scratch);
                recovered = true;
                i = end + 1;
            }
            Some(_) => {
                return Err(LoadError::Corrupted(format!(
                    "byte {i} of the traceEvents array is no event"
                )));
            }
        }
    }
}

/// The first `otherData` object in `prefix`, or `None` when it holds none or
/// the object does not parse.
fn recover_other_data(prefix: &str) -> Option<Map<String, Value>> {
    let key = prefix.find("\"otherData\"")?;
    let obj_start = prefix[key..].find('{').map(|i| key + i)?;
    let end = object_end(prefix.as_bytes(), obj_start)?;
    serde_json::from_str(&prefix[obj_start..=end]).ok()
}

/// The `otherData` key holding the id of the invocation that wrote the
/// profile: Bazel's command id, despite the key's name.
pub(crate) const BUILD_ID_KEY: &str = "build_id";

/// Decoded bytes [`probe_invocation_id`] reads from the head of a profile,
/// many times the `otherData` object Bazel opens a profile with.
const PROBE_HEAD_BYTES: u64 = 64 << 10;

/// The invocation that wrote the profile `input` holds, from the `otherData`
/// ahead of its events in the input's head. `None` when the head holds no such
/// id.
pub fn probe_invocation_id(input: &Input) -> Option<InvocationId> {
    invocation_id_at_head(input.head()?, PROBE_HEAD_BYTES)
}

/// [`probe_invocation_id`] over the first `head_bytes` decoded bytes of a
/// profile.
fn invocation_id_at_head(mut profile: impl BufRead, head_bytes: u64) -> Option<InvocationId> {
    let gzipped = profile.fill_buf().ok()?.starts_with(&GZIP_MAGIC);
    let mut head = Vec::new();
    // A stream that fails mid-way, as a truncated profile does, has still
    // yielded the bytes ahead of the failure.
    let _ = match gzipped {
        true => MultiGzDecoder::new(profile)
            .take(head_bytes)
            .read_to_end(&mut head),
        false => profile.take(head_bytes).read_to_end(&mut head),
    };
    let head = latin1_cow(&head);
    let before_events = match find_trace_events_key(head.as_bytes()) {
        Some(key_at) => &head[..key_at],
        None => &head,
    };
    let other_data = recover_other_data(before_events)?;
    InvocationId::new(other_data.get(BUILD_ID_KEY)?.as_str()?)
}

/// Combines two `otherData` objects into the metadata of a profile that
/// carries the key on both sides of its `traceEvents` array, splitting its
/// keys across them. `head` is the object appearing earlier in the stream, and
/// wins a key both carry.
pub(crate) fn merge_other_data(
    head: Option<Map<String, Value>>,
    tail: Option<Map<String, Value>>,
) -> Option<Map<String, Value>> {
    match (head, tail) {
        (Some(mut head), Some(tail)) => {
            for (key, value) in tail {
                head.entry(key).or_insert(value);
            }
            Some(head)
        }
        (head, tail) => head.or(tail),
    }
}

#[cfg(test)]
use crate::model::{Metadata, PhaseMarker, ProfileParts, ThreadTrack, TimeSpan};

/// Seals an interner and the given top-level vectors into a profile with empty
/// slice/counter pools, for model tests that need only phase markers and threads.
#[cfg(test)]
pub(crate) fn build_for_test(
    interner: ConcurrentInterner,
    phase_markers: Vec<PhaseMarker>,
    span: TimeSpan,
    threads: Vec<ThreadTrack>,
) -> Profile {
    let distinct_strings = interner.distinct();
    Profile::from_parts(ProfileParts {
        text: interner.into_text().expect("a test pool holds its text"),
        slice_pool: Vec::new(),
        detail_pool: Vec::new(),
        end_max_pool: Vec::new(),
        args_pool: Vec::new(),
        series_pool: Vec::new(),
        point_pool: Vec::new(),
        metadata: Metadata::default(),
        threads,
        counters: Vec::new(),
        phase_markers,
        span,
        distinct_strings,
        categories: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::decode::{GZIP_ISIZE_BYTES, MAX_PLAUSIBLE_GZIP_RATIO, latin1_to_utf8};
    use super::pipeline::parse_chunks;
    use super::scanner::{find_special, scan_event};
    use super::serde_events::{arg_i64, arg_str};
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    use super::stream::pipelined::{parse_gz_stream, parse_gz_stream_into};
    use super::stream::serial::{
        Declined, MIN_STREAMED_BYTES, first_pool, parse_serial, parse_serial_into,
        parse_serial_retrying,
    };
    use super::stream::{Unpaused, run_here};
    use super::*;
    use bviz_invocation::EpochMicros;
    use std::io::Write;

    #[cfg(any(unix, windows))]
    fn test_dir() -> std::path::PathBuf {
        std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    fn parse(json: &str) -> Profile {
        let Ok(Recorded::Complete(profile)) = Profile::from_bytes(json.as_bytes()) else {
            panic!("the profile parses whole");
        };
        profile
    }

    #[test]
    fn escaped_strings_resolve_correctly() {
        // Escaped JSON strings reach the parser as owned values rather than
        // slices borrowed from the input, exercising a distinct interning
        // path. The \u escape keeps the raw bytes ASCII, which the Latin-1
        // transcode passes through untouched.
        let json = r#"{
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"c","name":"quote \" caf\u00e9","ph":"X","ts":0,"dur":1,"tid":1,"args":{"target":"//a:b\"c","note":"tab\there","ke\u0079 2":"v"}}
            ]
        }"#;
        let p = parse(json);
        let t = &p.threads()[0];
        let s = &p.slices(t)[0];
        let d = &p.details(t)[0];
        assert_eq!(p.str(s.name), "quote \" café");
        assert_eq!(p.opt_str(d.target), Some("//a:b\"c"));
        let others: Vec<_> = p
            .slice_others(d)
            .iter()
            .map(|&(k, v)| (p.str(k), p.str(v)))
            .collect();
        // The second key arrives escaped, exercising the owned-key path.
        assert_eq!(others, vec![("note", "tab\there"), ("key 2", "v")]);
    }

    #[test]
    fn the_primary_output_lands_on_the_detail_and_any_other_value_reads_as_none() {
        let json = r#"{
            "traceEvents": [
                {"cat":"action processing","name":"a","ph":"X","ts":0,"dur":2,"tid":1,"out":"bazel-out/a","args":{"mnemonic":"M"}},
                {"cat":"action processing","name":"b","ph":"X","ts":2,"dur":2,"tid":1,"out":"bazel-out/b"},
                {"cat":"action processing","name":"c","ph":"X","ts":4,"dur":2,"tid":1},
                {"cat":"action processing","name":"d","ph":"X","ts":6,"dur":2,"tid":1,"out":3},
                {"cat":"action processing","name":"e","ph":"X","ts":8,"dur":2,"tid":1,"out":["x"]}
            ]
        }"#;
        let p = parse(json);
        let outs: Vec<_> = p
            .details(&p.threads()[0])
            .iter()
            .map(|d| p.opt_str(d.out))
            .collect();
        assert_eq!(
            outs,
            [Some("bazel-out/a"), Some("bazel-out/b"), None, None, None]
        );
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        let json = r#"{
            "traceEvents": [
                {"ph":"X","cat":"c","name":"broken","ts":100,"dur":-40,"tid":1}
            ]
        }"#;
        let p = parse(json);
        let t = &p.threads()[0];
        assert_eq!(p.slices(t)[0].dur_us, 0);
        assert_eq!(p.end_maxes(t), [100]);
    }

    #[test]
    fn null_fields_read_as_absent() {
        let json = r#"{
            "traceEvents": [
                {"ph":"X","cat":null,"name":null,"ts":0,"dur":1,"tid":1,"args":null}
            ]
        }"#;
        let p = parse(json);
        let t = &p.threads()[0];
        assert_eq!(p.slices(t).len(), 1);
        let s = &p.slices(t)[0];
        assert_eq!(p.str(s.name), "");
        assert_eq!(p.str(p.details(t)[0].category), "");
        assert!(p.slice_others(&p.details(t)[0]).is_empty());
    }

    /// A canonical, interning-order-independent dump of everything a profile
    /// carries, so two profiles built by different paths can be compared by
    /// meaning rather than by `Str` offsets (which depend on intern order).
    fn semantic(p: &Profile) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let m = p.metadata();
        writeln!(
            out,
            "meta {:?} {:?} {:?} {:?} {:?}",
            p.opt_str(m.bazel_version),
            p.opt_str(m.build_id),
            p.opt_str(m.output_base),
            p.opt_str(m.date),
            m.epoch_origin
        )
        .unwrap();
        writeln!(out, "span {} {}", p.span().start_us, p.span().end_us).unwrap();
        let categories: Vec<&str> = p.categories().map(|c| p.str(c)).collect();
        writeln!(out, "categories {categories:?}").unwrap();
        for t in p.threads() {
            writeln!(
                out,
                "thread tid={} name={:?} sort={} depth={} class={:?}",
                t.tid,
                p.str(t.name),
                t.sort_index,
                t.max_depth,
                t.lane_class
            )
            .unwrap();
            for (s, d) in p.slices(t).iter().zip(p.details(t)) {
                writeln!(
                    out,
                    "  slice {:?} cat={:?} ts={} dur={} depth={} role={:?} tgt={:?} mn={:?} cfg={:?} out={:?}",
                    p.str(s.name),
                    p.str(d.category),
                    s.start_us,
                    s.dur_us,
                    s.depth,
                    s.role,
                    p.opt_str(d.target),
                    p.opt_str(d.mnemonic),
                    p.opt_str(d.configuration),
                    p.opt_str(d.out),
                )
                .unwrap();
            }
        }
        for marker in p.phase_markers() {
            writeln!(out, "marker {:?} {}", p.str(marker.name), marker.start_us).unwrap();
        }
        for lane in p.counters() {
            writeln!(out, "counter {:?}", p.str(lane.name)).unwrap();
            for series in p.series(lane) {
                let pts: Vec<_> = p
                    .points(series)
                    .iter()
                    .map(|pt| (pt.ts_us, pt.value))
                    .collect();
                writeln!(out, "  series {:?} {:?}", p.str(series.name), pts).unwrap();
            }
        }
        out
    }

    /// One event per line, several threads, nested slices, a phase marker, and
    /// a counter sampled at two times — so chunking at newlines splits a
    /// thread's slices and a counter's points across chunks, exercising merge.
    fn chunkable_profile_json() -> String {
        "{\"otherData\":{\"bazel_version\":\"9.0.0\",\"profile_start_ts\":1781439245231},\"traceEvents\":[\n".to_string()
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":1,\"args\":{\"name\":\"Lane A\"}},\n"
            + "{\"name\":\"thread_sort_index\",\"ph\":\"M\",\"tid\":1,\"args\":{\"sort_index\":10}},\n"
            + "{\"cat\":\"skyframe evaluator\",\"name\":\"parent\",\"ph\":\"X\",\"ts\":0,\"dur\":100,\"tid\":1,\"args\":{\"mnemonic\":\"CppCompile\",\"target\":\"//a:b\"}},\n"
            + "{\"cat\":\"skyframe evaluator\",\"name\":\"child\",\"ph\":\"X\",\"ts\":0,\"dur\":40,\"tid\":1},\n"
            + "{\"name\":\"Usage\",\"ph\":\"C\",\"ts\":0,\"tid\":1,\"args\":{\"cpu\":1.0}},\n"
            + "{\"cat\":\"build phase marker\",\"name\":\"Analyzing\",\"ph\":\"i\",\"ts\":5,\"tid\":1},\n"
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":2,\"args\":{\"name\":\"Lane B\"}},\n"
            + "{\"cat\":\"remote action execution\",\"name\":\"work\",\"ph\":\"X\",\"ts\":10,\"dur\":80,\"tid\":2},\n"
            + "{\"name\":\"Usage\",\"ph\":\"C\",\"ts\":20,\"tid\":1,\"args\":{\"cpu\":2.0}},\n"
            + "{\"cat\":\"local action execution\",\"name\":\"more\",\"ph\":\"X\",\"ts\":30,\"dur\":10,\"tid\":2}\n"
            + "]}"
    }

    #[test]
    fn a_slice_carries_the_position_of_its_category_in_text_order() {
        let json = chunkable_profile_json();
        for p in [
            parse_streaming(&json).expect("serial"),
            parse_chunks(&json, 4, 0).expect("parallel"),
        ] {
            let categories: Vec<&str> = p.categories().map(|c| p.str(c)).collect();
            assert_eq!(
                categories,
                [
                    "local action execution",
                    "remote action execution",
                    "skyframe evaluator"
                ]
            );
            for t in p.threads() {
                for (s, d) in p.slices(t).iter().zip(p.details(t)) {
                    let id = s.category.expect("a category within reach of an id");
                    assert_eq!(p.category(id), d.category);
                }
            }
        }
    }

    #[test]
    fn a_category_past_the_last_an_id_reaches_leaves_its_slices_unnumbered() {
        // Zero-padded so that text order is numeric order.
        let categories = usize::from(u16::MAX) + 2;
        let events: Vec<String> = (0..categories)
            .map(|i| format!(r#"{{"cat":"c{i:05}","name":"s","ph":"X","ts":{i},"dur":1,"tid":1}}"#))
            .collect();
        let p = parse(&format!(r#"{{"traceEvents":[{}]}}"#, events.join(",")));
        assert_eq!(p.categories().len(), categories);
        let thread = &p.threads()[0];
        let reached = usize::from(u16::MAX);
        for (i, (s, d)) in p.slices(thread).iter().zip(p.details(thread)).enumerate() {
            let category = s.category.map(|id| p.category(id));
            assert_eq!(category, (i < reached).then_some(d.category), "slice {i}");
        }
    }

    #[test]
    fn parallel_matches_serial() {
        let json = chunkable_profile_json();
        let serial = parse_streaming(&json).expect("serial");
        // Force four chunks regardless of size so the merge path is exercised.
        let parallel = parse_chunks(&json, 4, 0).expect("parallel");
        assert_eq!(semantic(&serial), semantic(&parallel));
    }

    #[test]
    fn parse_chunks_declines_without_a_usable_array() {
        for text in [
            // No `traceEvents` key.
            r#"{"otherData":{"bazel_version":"9.0.0"}}"#,
            // The key, with nothing opening an array after it.
            r#"{"traceEvents":null}"#,
            // An array that never closes.
            "{\"traceEvents\":[\n{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n",
            // An empty array, whose close sits on the start of its contents.
            r#"{"traceEvents":[]}"#,
        ] {
            assert!(parse_chunks(text, 4, 0).is_none(), "{text}");
        }
    }

    #[test]
    fn parse_chunks_declines_an_array_under_the_threshold() {
        let json = chunkable_profile_json();
        assert!(parse_chunks(&json, 4, 0).is_some());
        assert!(parse_chunks(&json, 4, json.len() + 1).is_none());
    }

    #[test]
    fn parse_chunks_declines_an_array_on_one_line() {
        // Chunks are cut at newlines, so an array written on a single line
        // offers no cut, though it parses serially.
        let json = "{\"traceEvents\":[".to_string()
            + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1},"
            + "{\"cat\":\"c\",\"name\":\"b\",\"ph\":\"X\",\"ts\":1,\"dur\":1,\"tid\":1}"
            + "]}";
        assert!(parse_streaming(&json).is_ok());
        assert!(parse_chunks(&json, 4, 0).is_none());
    }

    #[test]
    fn parse_chunks_declines_a_chunk_that_fails_to_parse() {
        let with_last_event =
            |event: &str| chunkable_profile_json().replace("\n]}", &format!(",\n{event}\n]}}"));
        let event = "{\"cat\":\"c\",\"name\":\"z\",\"ph\":\"X\",\"ts\":40,\"dur\":1,\"tid\":2}";
        // A trailing comma inside the object: the scanner defers it and the
        // serde fallback rejects it, so whichever chunk holds the line fails.
        let trailing_comma = event.replace('}', ",}");
        // The well-formed array is of the same shape and clears every gate
        // ahead of the parse, leaving only the failed chunk to decline the
        // other.
        assert!(parse_chunks(&with_last_event(event), 4, 0).is_some());
        assert!(parse_chunks(&with_last_event(&trailing_comma), 4, 0).is_none());
    }

    #[test]
    fn a_bracket_after_the_array_declines_the_chunked_paths() {
        let profile = |build_id: &str| {
            "{\"traceEvents\":[\n".to_string()
                + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":1,\"args\":{\"name\":\"L\"}},\n"
                + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1},\n"
                + "{\"cat\":\"c\",\"name\":\"b\",\"ph\":\"X\",\"ts\":1,\"dur\":1,\"tid\":1}\n"
                + &format!("],\"otherData\":{{\"build_id\":\"{build_id}\"}}}}")
        };
        // The same profile with a bracket-free value clears every gate ahead of
        // the parse, leaving only the bracket to decline the other.
        assert!(parse_chunks(&profile("abc"), 4, 0).is_some());
        let json = profile("a]b");
        assert!(parse_chunks(&json, 4, 0).is_none());
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        {
            assert!(parse_gz_stream(&gzip(&profile("abc")), 2, 64).is_some());
            assert!(parse_gz_stream(&gzip(&json), 2, 64).is_none());
        }
        assert!(parse_gz_serial_chunked(&gzip(&profile("abc")), 64).is_some());
        assert!(parse_gz_serial_chunked(&gzip(&json), 64).is_none());

        // Read whole, the profile parses intact — events and the
        // bracket-bearing metadata alike.
        let p = Profile::from_bytes(json.as_bytes())
            .expect("serial")
            .into_inner();
        let names: Vec<_> = p
            .slices(&p.threads()[0])
            .iter()
            .map(|s| p.str(s.name))
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(p.opt_str(p.metadata().build_id), Some("a]b"));
    }

    /// Constructs the chunked path can only take through the scanner's serde
    /// fallback (floats, `\uXXXX` escapes, huge magnitudes) mixed with
    /// fast-path constructs (nulls, duplicate keys, ignored composites, arg
    /// scalars of every kept shape), so the equality check covers both routes
    /// and their interleaving.
    fn scanner_edge_case_json() -> String {
        "{\"traceEvents\":[\n".to_string()
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":2,\"args\":{\"name\":\"L\\u0061ne \\\"B\\\"\"}},\n"
            // serde reads -0 as the float -0.0, which arg_i64 must not treat
            // as sort index 0.
            + "{\"name\":\"thread_sort_index\",\"ph\":\"M\",\"tid\":2,\"args\":{\"sort_index\":-0}},\n"
            + "{\"cat\":\"c\",\"name\":\"caf\\u00e9 \\\"x\\\"\",\"ph\":\"X\",\"ts\":1.5,\"dur\":2.5,\"tid\":1},\n"
            + "{\"cat\":null,\"name\":\"n1\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":2,\"args\":null},\n"
            + "{\"name\":\"first\",\"name\":\"second\",\"ph\":\"X\",\"ts\":3,\"dur\":1,\"tid\":2,\"args\":{\"a\":\"x\"},\"args\":{\"b\":\"y\"}},\n"
            + "{\"pid\":7,\"weird\":[1,{\"q\":\"]\"},\"s\\\"t\"],\"other\":{\"deep\":[true,null]},\"name\":\"u1\",\"ph\":\"X\",\"ts\":4,\"dur\":1,\"tid\":2},\n"
            + "{\"name\":\"argsy\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"args\":{\"s\":\"v\",\"i\":-3,\"big\":9223372036854775808,\"b\":true,\"nb\":false,\"z\":null,\"arr\":[1,2],\"obj\":{\"k\":\"v\"},\"esc\":\"a\\tb\"}},\n"
            + "{\"name\":\"outs\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"out\":\"bazel-out/a\"},\n"
            + "{\"name\":\"outu\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"out\":\"bazel-out/\\u0062\",\"args\":{\"target\":\"//a:b\"}},\n"
            + "{\"name\":\"outn\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"out\":7},\n"
            + "{\"name\":\"outo\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"out\":{\"k\":[1]}},\n"
            + "{\"name\":\"floatarg\",\"ph\":\"X\",\"ts\":5,\"dur\":1,\"tid\":2,\"args\":{\"f\":2.75}},\n"
            // The saturated i64::MAX start plus a positive dur exercises the
            // depth pass's saturating end computation.
            + "{\"name\":\"big\",\"ph\":\"X\",\"ts\":92233720368547758070,\"dur\":1,\"tid\":2},\n"
            + "{\"name\":\"neg\",\"ph\":\"X\",\"ts\":6,\"dur\":1,\"tid\":-2},\n"
            + "{ \"name\" : \"ws\" , \"ph\" : \"X\" , \"ts\" : 7 , \"dur\" : 1 , \"tid\" : 2 },\n"
            + "{\"name\":\"Empty\",\"ph\":\"C\",\"ts\":8,\"tid\":1,\"args\":{}},\n"
            + "{\"name\":\"Usage\",\"ph\":\"C\",\"ts\":9,\"tid\":1,\"args\":{\"cpu\":0.5}},\n"
            + "{}\n"
            + "]}"
    }

    #[test]
    fn scanner_edge_cases_match_serial() {
        let json = scanner_edge_case_json();
        let serial = parse_streaming(&json).expect("serial");
        let parallel = parse_chunks(&json, 4, 0).expect("parallel");
        assert_eq!(semantic(&serial), semantic(&parallel));
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        // The whole-buffer path parses the Latin-1 transcode, in which every
        // high byte became a two-byte sequence, so a cut target computed from
        // the region's byte length can land inside a character. This name is
        // long enough to hold every target.
        let name = "é".repeat(200);
        let json = "{\"traceEvents\":[\n".to_string()
            + &format!(
                "{{\"cat\":\"c\",\"name\":\"{name}\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}},\n"
            )
            + "{\"cat\":\"c\",\"name\":\"b\",\"ph\":\"X\",\"ts\":1,\"dur\":1,\"tid\":1}\n"
            + "]}";
        let serial = parse_streaming(&json).expect("serial");
        let parallel = parse_chunks(&json, 4, 0).expect("parallel");
        assert_eq!(semantic(&serial), semantic(&parallel));
    }

    #[test]
    fn scan_event_reads_a_typical_event() {
        let json = r#"{"cat":"skyframe evaluator","name":"a b","ph":"X","ts":10,"dur":20,"tid":3,"out":"bazel-out/x","args":{"target":"//a:b","n":4}} ,"#;
        let mut scratch = Vec::new();
        let (ev, end) = scan_event(json, 0, &mut scratch).expect("fast path");
        assert_eq!(&json[end..], " ,");
        assert_eq!(ev.cat.as_deref(), Some("skyframe evaluator"));
        assert_eq!(ev.name.as_deref(), Some("a b"));
        assert_eq!(ev.ph.as_deref(), Some("X"));
        assert_eq!(ev.ts, Some(10.0));
        assert_eq!(ev.dur, Some(20.0));
        assert_eq!(ev.tid, Some(3));
        assert_eq!(ev.out.as_deref(), Some("bazel-out/x"));
        assert!(ev.has_args);
        assert_eq!(scratch.len(), 2);
        assert_eq!(arg_str(&scratch, "target"), Some("//a:b"));
        assert_eq!(arg_i64(&scratch, "n"), Some(4));
    }

    #[test]
    fn scan_event_unescapes_and_defers() {
        let mut scratch = Vec::new();
        let (ev, _) = scan_event(r#"{"name":"q\"\t\\z"}"#, 0, &mut scratch).expect("escapes");
        assert_eq!(ev.name.as_deref(), Some("q\"\t\\z"));
        // Constructs whose acceptance or value must come from serde defer.
        for deferred in [
            r#"{"ts":1.5}"#,
            r#"{"ts":1e3}"#,
            r#"{"ts":01}"#,
            r#"{"name":"caf\u00e9"}"#,
            r#"{"tid":9223372036854775808}"#,
            r#"{"tid":-0}"#,
            r#"{"ph":7}"#,
            r#"{"args":[1]}"#,
        ] {
            assert!(
                scan_event(deferred, 0, &mut scratch).is_none(),
                "{deferred}"
            );
        }
    }

    #[test]
    fn find_special_matches_a_naive_scan() {
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..64u8 {
            bytes.extend_from_slice(b"plain text ");
            bytes.push(match i % 5 {
                0 => b'"',
                1 => b'\\',
                2 => 0x1f,
                3 => 0xc3, // a multibyte UTF-8 lead, never special
                _ => b'x',
            });
        }
        for start in 0..bytes.len() {
            let naive = (start..bytes.len()).find(|&j| matches!(bytes[j], b'"' | b'\\' | 0..=0x1f));
            assert_eq!(find_special(&bytes, start), naive, "from {start}");
        }
        assert_eq!(find_special(b"no specials at all", 0), None);
    }

    #[test]
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    fn pipelined_matches_serial() {
        let json = chunkable_profile_json();
        let serial = parse_streaming(&json).expect("serial");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        // Chunks far smaller than an event line force many pieces, boundary
        // carries, and the no-newline buffer doubling.
        let pipelined = parse_gz_stream(&gz, 4, 64).expect("pipelined");
        assert_eq!(semantic(&serial), semantic(&pipelined));
    }

    /// Events short enough that every piece takes a ring buffer, and enough of
    /// them to outnumber the ring, so the producer has to wait on a buffer a
    /// worker has finished with instead of always finding a fresh one.
    #[test]
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    fn pipelined_recycles_ring_buffers() {
        let mut json = String::from("{\"traceEvents\":[\n");
        for i in 0..24 {
            json += &format!(
                "{{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":{i},\"dur\":1,\"tid\":{}}},\n",
                i % 3
            );
        }
        // Drop the last event's separator, which no element follows.
        json.truncate(json.len() - 2);
        json += "\n]}";
        let serial = parse_streaming(&json).expect("serial");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        // Two workers, so the ring holds five buffers.
        let pipelined = parse_gz_stream(&gz, 2, 64).expect("pipelined");
        assert_eq!(semantic(&serial), semantic(&pipelined));
    }

    /// The serial decode of `gz` in pieces of about `chunk_bytes`, into a pool
    /// sized from the stream's own [`gzip_size_hint`].
    fn parse_gz_serial_chunked(gz: &[u8], chunk_bytes: usize) -> Option<Profile> {
        let decoded_size = gzip_size_hint(gz).unwrap_or(0);
        let interner =
            ConcurrentInterner::new(decoded_size, decoded_size / BYTES_PER_DISTINCT).unwrap();
        match run_here(parse_serial_into(
            MultiGzDecoder::new(gz),
            interner,
            chunk_bytes,
            decoded_size,
            &mut Unpaused,
        ))
        .ok()?
        {
            Recorded::Complete(profile) => Some(profile),
            Recorded::CutOff(_) => None,
        }
    }

    /// A profile of at least [`MIN_STREAMED_BYTES`] and `min_bytes`, one event
    /// per line, each named by a distinct run of `name_bytes` pseudo-random
    /// alphanumerics, which compress about as poorly as real names.
    fn streamable_profile_json(name_bytes: usize, min_bytes: usize) -> String {
        use std::fmt::Write;
        // An event line's bytes besides its name, rounded up.
        const LINE_FRAMING_BYTES: usize = 128;
        const THREADS: usize = 4;
        const TARGETS: usize = 64;
        const SEED: u64 = 7;
        let len = MIN_STREAMED_BYTES.max(min_bytes);
        // The last event line overshoots the length, and a closing line follows.
        let mut json = String::with_capacity(len + name_bytes + 2 * LINE_FRAMING_BYTES);
        json.push_str("{\"otherData\":{\"bazel_version\":\"9.0.0\"},\"traceEvents\":[\n");
        let mut rng = fastrand::Rng::with_seed(SEED);
        let mut i = 0usize;
        while json.len() < len {
            json.push_str("{\"cat\":\"c\",\"name\":\"");
            let name_start = json.len();
            write!(json, "{i:x}-").unwrap();
            let fill = name_bytes.saturating_sub(json.len() - name_start);
            json.extend(std::iter::repeat_with(|| rng.alphanumeric()).take(fill));
            writeln!(
                json,
                "\",\"ph\":\"X\",\"ts\":{i},\"dur\":1,\"tid\":{},\"args\":{{\"target\":\"//p:t{}\"}}}},",
                i % THREADS,
                i % TARGETS,
            )
            .unwrap();
            i += 1;
        }
        json.push_str(
            "{\"cat\":\"c\",\"name\":\"last\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":0}\n]}",
        );
        json
    }

    #[test]
    fn serially_streamed_matches_serial() {
        let json = chunkable_profile_json();
        let serial = parse_streaming(&json).expect("serial");
        // Chunks far smaller than an event line force many pieces, boundary
        // carries, and the no-newline buffer doubling.
        let streamed = parse_gz_serial_chunked(&gzip(&json), 64).expect("serially streamed");
        assert_eq!(semantic(&serial), semantic(&streamed));
    }

    #[test]
    fn serially_streamed_transcodes_latin1() {
        // 0xe9 is Latin-1 'é', as Bazel writes it.
        let mut raw = b"{\"traceEvents\":[\n".to_vec();
        raw.extend(
            b"{\"cat\":\"c\",\"name\":\"caf\xe9\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n",
        );
        raw.extend(b"]}");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();
        let p = parse_gz_serial_chunked(&gz, 64).expect("serially streamed");
        assert_eq!(p.str(p.slices(&p.threads()[0])[0].name), "café");
    }

    fn memory_input(bytes: Vec<u8>) -> Input {
        Input::new("memory".into(), bviz_input::Access::Memory(bytes))
            .expect("an in-memory input reads")
    }

    #[test]
    fn an_input_held_in_memory_streams_serially() {
        let json = streamable_profile_json(8, 0);
        let whole = semantic(&parse_streaming(&json).expect("serial"));
        for bytes in [gzip(&json), json.clone().into_bytes()] {
            let Some(Recorded::Complete(streamed)) = run_here(parse_serial_input(
                &memory_input(bytes.clone()),
                &mut Unpaused,
            )) else {
                panic!("serially streamed whole");
            };
            assert_eq!(semantic(&streamed), whole);
            let Some(Recorded::Complete(from_bytes)) = parse_serial(&bytes) else {
                panic!("serially streamed whole");
            };
            assert_eq!(semantic(&from_bytes), whole);
        }
    }

    #[test]
    fn a_short_profile_is_left_to_the_whole_buffer_parse() {
        let json = chunkable_profile_json();
        for bytes in [gzip(&json), json.into_bytes()] {
            assert!(parse_serial(&bytes).is_none());
            assert!(run_here(parse_serial_input(&memory_input(bytes), &mut Unpaused)).is_none());
        }
    }

    /// Returns pending on its first poll, woken at once, as handing a thread
    /// back does.
    #[cfg(not(target_family = "wasm"))]
    async fn pend_once() {
        let mut pended = false;
        std::future::poll_fn(|cx| match std::mem::replace(&mut pended, true) {
            true => std::task::Poll::Ready(()),
            false => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await
    }

    // Ceding reads the clock, which a WebAssembly test runs without.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn a_ceding_decode_cedes_between_pieces_and_reads_alike() {
        let json = chunkable_profile_json();
        let mut cedes = 0;
        let mut ceding = Ceding::new(std::time::Duration::ZERO, || {
            cedes += 1;
            pend_once()
        });
        let interner = ConcurrentInterner::new(json.len(), 1).unwrap();
        let Ok(Recorded::Complete(streamed)) = run_here(parse_serial_into(
            json.as_bytes(),
            interner,
            64,
            json.len(),
            &mut ceding,
        )) else {
            panic!("serially streamed whole");
        };
        assert_eq!(
            semantic(&streamed),
            semantic(&parse_streaming(&json).expect("serial"))
        );
        assert!(cedes > 1);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn an_input_read_ceding_reads_as_one_read_whole() {
        let json = streamable_profile_json(8, 0);
        let whole = semantic(&parse_streaming(&json).expect("serial"));
        for bytes in [gzip(&json), json.clone().into_bytes()] {
            let Ok(Recorded::Complete(p)) = run_here(Profile::from_input_ceding(
                memory_input(bytes),
                std::time::Duration::ZERO,
                pend_once,
            )) else {
                panic!("read whole");
            };
            assert_eq!(semantic(&p), whole);
        }
    }

    #[test]
    fn plain_text_streams_serially() {
        let json = chunkable_profile_json();
        let interner = ConcurrentInterner::new(json.len(), 1).unwrap();
        let Ok(Recorded::Complete(streamed)) = run_here(parse_serial_into(
            json.as_bytes(),
            interner,
            64,
            json.len(),
            &mut Unpaused,
        )) else {
            panic!("serially streamed whole");
        };
        assert_eq!(
            semantic(&streamed),
            semantic(&parse_streaming(&json).expect("serial"))
        );
    }

    #[test]
    fn a_cut_off_stream_recovers_as_the_whole_buffer_does() {
        let json = chunkable_profile_json();
        let gz = gzip(&json);
        let unclosed = gzip(json.trim_end_matches("]}"));
        let cuts = [
            &gz[..gz.len() / 2],
            &gz[..gz.len() * 3 / 4],
            // Every decoded byte, short of the length field that checks them.
            &gz[..gz.len() - GZIP_ISIZE_BYTES],
            // A whole stream whose array never closes.
            &unclosed[..],
        ];
        let recovers_alike = |bytes: &[u8], text: &mut dyn Read| {
            let Ok(Recorded::CutOff(whole)) = from_whole_buffer(bytes) else {
                panic!("the whole-buffer path recovers it cut off");
            };
            let interner = ConcurrentInterner::new(json.len(), 1).unwrap();
            let Ok(Recorded::CutOff(streamed)) = run_here(parse_serial_into(
                text,
                interner,
                64,
                json.len(),
                &mut Unpaused,
            )) else {
                panic!("the serial decode recovers it cut off");
            };
            assert_eq!(semantic(&streamed), semantic(&whole));
        };
        for cut in cuts {
            recovers_alike(cut, &mut MultiGzDecoder::new(cut));
        }
        let unclosed = json.trim_end_matches("]}").as_bytes();
        recovers_alike(unclosed, &mut &unclosed[..]);
        // Cut before its events start, a stream is left to the whole-buffer
        // path, which reports why it fails.
        const GZIP_HEADER_BYTES: usize = 10;
        let interner = ConcurrentInterner::new(json.len(), 1).unwrap();
        let header_only = &gz[..GZIP_HEADER_BYTES + 2];
        assert!(
            run_here(parse_serial_into(
                MultiGzDecoder::new(header_only),
                interner,
                64,
                json.len(),
                &mut Unpaused,
            ))
            .is_err()
        );
    }

    #[test]
    fn merge_keeps_event_order_for_equal_keys() {
        // One thread with several slices sharing an identical (ts, dur) key,
        // interleaved with distinct ones and spread across chunks. Slices with
        // equal keys keep event order in the lane, so every parse path must
        // produce the serial path's order.
        let json = "{\"traceEvents\":[\n".to_string()
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":1,\"args\":{\"name\":\"L\"}},\n"
            + "{\"cat\":\"c\",\"name\":\"outer\",\"ph\":\"X\",\"ts\":0,\"dur\":100,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"tie1\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"tie2\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"mid\",\"ph\":\"X\",\"ts\":12,\"dur\":2,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"tie3\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"tie4\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"tie5\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"last\",\"ph\":\"X\",\"ts\":60,\"dur\":10,\"tid\":1}\n"
            + "]}";
        let serial = parse_streaming(&json).expect("serial");
        let lane = &serial.threads()[0];
        let names: Vec<_> = serial
            .slices(lane)
            .iter()
            .map(|s| serial.str(s.name))
            .collect();
        assert_eq!(
            names,
            [
                "outer", "tie1", "tie2", "tie3", "tie4", "tie5", "mid", "last"
            ]
        );
        // Chunking at newlines splits the ties across several builders.
        let parallel = parse_chunks(&json, 4, 0).expect("parallel");
        assert_eq!(semantic(&serial), semantic(&parallel));
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        {
            let pipelined = parse_gz_stream(&gzip(&json), 4, 64).expect("pipelined");
            assert_eq!(semantic(&serial), semantic(&pipelined));
        }
        let streamed = parse_gz_serial_chunked(&gzip(&json), 64).expect("serially streamed");
        assert_eq!(semantic(&serial), semantic(&streamed));
    }

    #[test]
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    fn pipelined_reads_trailing_other_data() {
        let json = "{\"traceEvents\":[\n".to_string()
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":1,\"args\":{\"name\":\"L\"}},\n"
            + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n"
            + "],\"otherData\":{\"bazel_version\":\"9.0.0\"}}";
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let p = parse_gz_stream(&gz, 2, 64).expect("pipelined");
        assert_eq!(p.opt_str(p.metadata().bazel_version), Some("9.0.0"));
    }

    #[test]
    fn other_data_merges_across_both_ends() {
        // Keys split either side of the array, with `bazel_version` repeated so
        // the leading object's value must win.
        let json = "{\"otherData\":{\"bazel_version\":\"9.0.0\"},\"traceEvents\":[\n".to_string()
            + "{\"name\":\"thread_name\",\"ph\":\"M\",\"tid\":1,\"args\":{\"name\":\"L\"}},\n"
            + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n"
            + "],\"otherData\":{\"build_id\":\"abc\",\"bazel_version\":\"0.0.0\",\"profile_start_ts\":1781439245231}}";
        let check = |p: &Profile| {
            assert_eq!(p.opt_str(p.metadata().bazel_version), Some("9.0.0"));
            assert_eq!(p.opt_str(p.metadata().build_id), Some("abc"));
            assert_eq!(
                p.metadata().epoch_origin,
                Some(EpochMicros::from_millis(1_781_439_245_231))
            );
        };
        check(&parse_streaming(&json).expect("serial"));
        check(&parse_chunks(&json, 4, 0).expect("parallel"));
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        check(&parse_gz_stream(&gzip(&json), 2, 64).expect("pipelined"));
        check(&parse_gz_serial_chunked(&gzip(&json), 64).expect("serially streamed"));
    }

    #[test]
    fn probe_reads_the_id_at_the_head() {
        let id = "a5f7f3e6-4fda-4675-bc16-8ede5420b210";
        let leading = format!(
            "{{\"otherData\":{{\"bazel_version\":\"9.0.0\",\"build_id\":\"{id}\"}},\"traceEvents\":[\n"
        ) + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n]}";
        let gzipped = |json: &str| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(json.as_bytes()).unwrap();
            enc.finish().unwrap()
        };
        let probe = |bytes: &[u8], head_bytes| invocation_id_at_head(bytes, head_bytes);
        let whole = leading.len() as u64;
        assert_eq!(probe(leading.as_bytes(), whole), InvocationId::new(id));
        assert_eq!(probe(&gzipped(&leading), whole), InvocationId::new(id));
        // A profile cut off inside its events still opens with its id, plain
        // or as a gzip stream missing its tail.
        let cut = leading.len() - 20;
        assert_eq!(
            probe(&leading.as_bytes()[..cut], whole),
            InvocationId::new(id)
        );
        let gz = gzipped(&leading);
        assert_eq!(probe(&gz[..gz.len() - 12], whole), InvocationId::new(id));

        // A head that ends inside the object holds no id.
        let inside_other_data = leading.find(id).unwrap() as u64;
        assert_eq!(probe(leading.as_bytes(), inside_other_data), None);
        assert_eq!(probe(&gzipped(&leading), inside_other_data), None);

        // Nor does an object that trails the events or their key, an empty id, or no
        // profile at all.
        let trailing = "{\"traceEvents\":[\n".to_string()
            + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1}\n"
            + &format!("],\"otherData\":{{\"build_id\":\"{id}\"}}}}");
        assert_eq!(probe(trailing.as_bytes(), trailing.len() as u64), None);
        let cut_at_key = &trailing[..trailing.find('[').unwrap()];
        let after_key = format!("{cut_at_key},\"otherData\":{{\"build_id\":\"{id}\"}}}}");
        assert_eq!(probe(after_key.as_bytes(), after_key.len() as u64), None);
        let unnamed = r#"{"otherData":{"build_id":""},"traceEvents":[]}"#;
        assert_eq!(probe(unnamed.as_bytes(), unnamed.len() as u64), None);
        assert_eq!(probe(b"", PROBE_HEAD_BYTES), None);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn probe_reads_a_gzipped_profile_from_disk() {
        let json = r#"{"otherData":{"build_id":"abc"},"traceEvents":[]}"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let dir = test_dir();
        let path = std::path::Path::new(&dir).join("probe.profile.gz");
        std::fs::write(&path, enc.finish().unwrap()).unwrap();
        let input = Input::open(&path).expect("opens");
        assert_eq!(probe_invocation_id(&input), InvocationId::new("abc"));
    }

    #[test]
    fn metadata_and_thread_order() {
        let json = r#"{
            "otherData": {"bazel_version":"7.0.0","build_id":"abc","output_base":"/ob","date":"today","profile_start_ts":1781439245231},
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":5,"args":{"name":"Lane B"}},
                {"name":"thread_sort_index","ph":"M","tid":5,"args":{"sort_index":20}},
                {"name":"thread_name","ph":"M","tid":3,"args":{"name":"Lane A"}},
                {"name":"thread_sort_index","ph":"M","tid":3,"args":{"sort_index":10}},
                {"cat":"c","name":"b","ph":"X","ts":0,"dur":1,"pid":1,"tid":5},
                {"cat":"c","name":"a","ph":"X","ts":0,"dur":1,"pid":1,"tid":3}
            ]
        }"#;
        let p = parse(json);
        assert_eq!(p.opt_str(p.metadata().bazel_version), Some("7.0.0"));
        assert_eq!(p.opt_str(p.metadata().build_id), Some("abc"));
        assert_eq!(p.opt_str(p.metadata().output_base), Some("/ob"));
        assert_eq!(p.opt_str(p.metadata().date), Some("today"));
        assert_eq!(
            p.metadata().epoch_origin,
            Some(EpochMicros::from_millis(1_781_439_245_231))
        );
        let names: Vec<_> = p.threads().iter().map(|t| p.str(t.name)).collect();
        assert_eq!(names, vec!["Lane A", "Lane B"]);
    }

    #[test]
    fn an_epoch_origin_that_is_not_an_integer_is_absent() {
        for origin in ["1781439245231.5", "\"1781439245231\"", "null"] {
            let json = format!(
                r#"{{"otherData":{{"build_id":"abc","profile_start_ts":{origin}}},"traceEvents":[]}}"#
            );
            let p = parse(&json);
            assert_eq!(p.opt_str(p.metadata().build_id), Some("abc"));
            assert_eq!(p.metadata().epoch_origin, None, "{origin}");
        }
    }

    #[test]
    fn nesting_depth() {
        let json = r#"{
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"c","name":"parent","ph":"X","ts":0,"dur":100,"pid":1,"tid":1},
                {"cat":"c","name":"child1","ph":"X","ts":0,"dur":40,"pid":1,"tid":1},
                {"cat":"c","name":"child2","ph":"X","ts":50,"dur":40,"pid":1,"tid":1}
            ]
        }"#;
        let p = parse(json);
        let t = &p.threads()[0];
        let depth = |name: &str| {
            p.slices(t)
                .iter()
                .find(|s| p.str(s.name) == name)
                .unwrap()
                .depth
        };
        assert_eq!(depth("parent"), 0);
        assert_eq!(depth("child1"), 1);
        assert_eq!(depth("child2"), 1);
        assert_eq!(t.max_depth, 2);
    }

    #[test]
    fn end_maxes_hold_running_max_and_reset_per_thread() {
        // tid 1: "outer" outlasts the short slices nested after it, so the
        // running max stays flat at outer's end until "late" passes it; tid 2
        // is its own segment, whose max restarts at its first slice's end.
        // One event per line so the chunked paths split the lanes across
        // builders and exercise the k-way merge's end-max accumulation.
        let json = "{\"traceEvents\":[\n".to_string()
            + "{\"cat\":\"c\",\"name\":\"outer\",\"ph\":\"X\",\"ts\":0,\"dur\":100,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"a\",\"ph\":\"X\",\"ts\":10,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"b\",\"ph\":\"X\",\"ts\":30,\"dur\":5,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"c\",\"ph\":\"X\",\"ts\":60,\"dur\":10,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"late\",\"ph\":\"X\",\"ts\":150,\"dur\":30,\"tid\":1},\n"
            + "{\"cat\":\"c\",\"name\":\"w\",\"ph\":\"X\",\"ts\":5,\"dur\":10,\"tid\":2},\n"
            + "{\"cat\":\"c\",\"name\":\"z\",\"ph\":\"X\",\"ts\":50,\"dur\":5,\"tid\":2}\n"
            + "]}";
        let check = |p: &Profile| {
            let maxes: Vec<Vec<i64>> = p
                .threads()
                .iter()
                .map(|t| p.end_maxes(t).to_vec())
                .collect();
            assert_eq!(maxes, [vec![100, 100, 100, 100, 180], vec![15, 55]]);
            for t in p.threads() {
                assert_eq!(p.end_maxes(t).len(), p.slices(t).len());
            }
        };
        check(&parse_streaming(&json).expect("serial"));
        check(&parse_chunks(&json, 4, 0).expect("parallel"));
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        check(&parse_gz_stream(&gzip(&json), 4, 64).expect("pipelined"));
        check(&parse_gz_serial_chunked(&gzip(&json), 64).expect("serially streamed"));
    }

    #[test]
    fn phase_markers_are_not_slices() {
        let json = r#"{
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"build phase marker","name":"Launch Blaze","ph":"X","ts":-2463000,"dur":0,"pid":1,"tid":1},
                {"cat":"build phase marker","name":"Loading","ph":"i","ts":5,"pid":1,"tid":1},
                {"cat":"c","name":"real","ph":"X","ts":10,"dur":5,"pid":1,"tid":1}
            ]
        }"#;
        let p = parse(json);
        let marker_names: Vec<_> = p.phase_markers().iter().map(|m| p.str(m.name)).collect();
        assert_eq!(marker_names, vec!["Launch Blaze", "Loading"]);
        assert_eq!(p.phase_markers()[0].start_us, -2463000);
        let slice_names: Vec<_> = p
            .slices(&p.threads()[0])
            .iter()
            .map(|s| p.str(s.name))
            .collect();
        assert_eq!(slice_names, vec!["real"]);
    }

    #[test]
    fn critical_path_stays_on_tid_zero() {
        let json = r#"{
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":0,"args":{"name":"Critical Path"}},
                {"cat":"critical path component","name":"step","ph":"X","ts":0,"dur":3,"pid":1,"tid":0,"args":{"tid":7}}
            ]
        }"#;
        let p = parse(json);
        let crit = p.threads().iter().find(|t| t.tid == 0).expect("tid 0 lane");
        assert_eq!(p.str(crit.name), "Critical Path");
        assert_eq!(p.slices(crit).len(), 1);
        assert!(!p.threads().iter().any(|t| t.tid == 7));
    }

    #[test]
    fn counters_split_per_series_skipping_non_numeric_keys() {
        let json = r#"{
            "traceEvents": [
                {"name":"Usage","ph":"C","ts":0,"pid":1,"tid":1,"args":{"cpu":1.0}},
                {"name":"Usage","ph":"C","ts":10,"pid":1,"tid":1,"args":{"cpu":2.0,"mem":5.0,"tag":"x"}}
            ]
        }"#;
        let p = parse(json);
        // One lane per numeric series; the non-numeric "tag" arg is dropped. A
        // multi-series counter labels each lane with its arg key.
        let labels: Vec<_> = p.counters().iter().map(|l| p.str(l.name)).collect();
        assert_eq!(labels, ["Usage · cpu", "Usage · mem"]);
        let cpu = &p.counters()[0];
        assert_eq!(p.series(cpu).len(), 1);
        assert_eq!(p.points(&p.series(cpu)[0]).len(), 2);
        assert_eq!(p.series(cpu)[0].max_value, 2.0);
        let mem = &p.counters()[1];
        let mem_points = p.points(&p.series(mem)[0]);
        assert_eq!(mem_points.len(), 1);
        assert_eq!(mem_points[0].ts_us, 10);
        assert_eq!(mem_points[0].value, 5.0);
        assert_eq!(p.series(mem)[0].max_value, 5.0);
    }

    #[test]
    fn counter_points_are_sorted_by_time() {
        let json = r#"{
            "traceEvents": [
                {"name":"Usage","ph":"C","ts":20,"pid":1,"tid":1,"args":{"cpu":3.0}},
                {"name":"Usage","ph":"C","ts":0,"pid":1,"tid":1,"args":{"cpu":1.0}},
                {"name":"Usage","ph":"C","ts":10,"pid":1,"tid":1,"args":{"cpu":2.0}}
            ]
        }"#;
        let p = parse(json);
        let series = &p.series(&p.counters()[0])[0];
        let times: Vec<_> = p.points(series).iter().map(|pt| pt.ts_us).collect();
        assert_eq!(times, [0, 10, 20]);
        assert_eq!(series.max_value, 3.0);
    }

    #[test]
    fn counters_cluster_by_kind_and_keep_empty_lanes() {
        // Emitted out of cluster order, with a worker-memory counter that (as on a
        // build with no persistent workers) carries only empty args.
        let json = r#"{
            "traceEvents": [
                {"name":"Memory usage (Bazel)","ph":"C","ts":0,"pid":1,"tid":1,"args":{"memory":100.0}},
                {"name":"CPU usage (total)","ph":"C","ts":0,"pid":1,"tid":1,"args":{"system cpu":2.0}},
                {"name":"action count","ph":"C","ts":0,"pid":1,"tid":1,"args":{"action":3.0,"local action cache":0.1}},
                {"name":"Total worker memory usage","ph":"C","ts":0,"pid":1,"tid":1,"args":{}},
                {"name":"CPU usage (Bazel)","ph":"C","ts":0,"pid":1,"tid":1,"args":{"cpu":1.0}},
                {"name":"action count (local)","ph":"C","ts":0,"pid":1,"tid":1,"args":{"local action":1.0}},
                {"name":"Memory usage (total)","ph":"C","ts":0,"pid":1,"tid":1,"args":{"system memory":500.0}}
            ]
        }"#;
        let p = parse(json);
        let labels: Vec<_> = p.counters().iter().map(|l| p.str(l.name)).collect();
        assert_eq!(
            labels,
            [
                "action count · action",
                "action count (local)",
                "action count · local action cache",
                "CPU usage (total)",
                "CPU usage (Bazel)",
                "Memory usage (total)",
                "Memory usage (Bazel)",
                "Total worker memory usage",
            ]
        );
        let worker = p.counters().last().unwrap();
        assert!(p.series(worker).is_empty());
    }

    #[test]
    fn span_covers_negative_and_max() {
        let json = r#"{
            "traceEvents": [
                {"cat":"build phase marker","name":"Launch","ph":"X","ts":-2463000,"dur":0,"pid":1,"tid":1},
                {"cat":"c","name":"x","ph":"X","ts":100,"dur":900,"pid":1,"tid":1},
                {"name":"Usage","ph":"C","ts":2000,"pid":1,"tid":1,"args":{"cpu":1.0}}
            ]
        }"#;
        let p = parse(json);
        assert_eq!(p.span().start_us, -2463000);
        // The trailing counter sample occupies the microsecond it sits in.
        assert_eq!(p.span().end_us, 2001);
    }

    #[test]
    fn a_profile_whose_last_event_is_zero_duration_reaches_past_its_instant() {
        // A trailing record of each kind the extent is folded from: a slice, a
        // phase marker, and a counter sample.
        for last in [
            r#"{"cat":"c","name":"mark","ph":"X","ts":1000,"dur":0,"pid":1,"tid":1}"#,
            r#"{"cat":"build phase marker","name":"Execute","ph":"X","ts":1000,"pid":1,"tid":1}"#,
            r#"{"name":"Usage","ph":"C","ts":1000,"pid":1,"tid":1,"args":{"cpu":1.0}}"#,
        ] {
            let json = format!(
                r#"{{"traceEvents": [
                    {{"cat":"c","name":"x","ph":"X","ts":100,"dur":900,"pid":1,"tid":1}},
                    {last}
                ]}}"#
            );
            assert_eq!(parse(&json).span().end_us, 1001, "{last}");
        }
    }

    #[test]
    fn a_profile_whose_last_event_has_a_duration_ends_exactly_at_its_end() {
        let json = r#"{
            "traceEvents": [
                {"cat":"c","name":"mark","ph":"X","ts":100,"dur":0,"pid":1,"tid":1},
                {"cat":"c","name":"x","ph":"X","ts":100,"dur":900,"pid":1,"tid":1}
            ]
        }"#;
        assert_eq!(parse(json).span().end_us, 1000);
    }

    #[test]
    fn an_empty_profile_reports_a_zero_extent() {
        let p = parse(r#"{"traceEvents": []}"#);
        assert_eq!(
            p.span(),
            TimeSpan {
                start_us: 0,
                end_us: 0
            }
        );
    }

    #[test]
    fn gzip_round_trip() {
        let json = r#"{
            "otherData": {"bazel_version":"7.0.0"},
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"c","name":"a","ph":"X","ts":0,"dur":1,"pid":1,"tid":1}
            ]
        }"#;
        let Ok(Recorded::Complete(plain)) = Profile::from_bytes(json.as_bytes()) else {
            panic!("the profile reads whole");
        };
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let Ok(Recorded::Complete(from_gz)) = Profile::from_bytes(&gz) else {
            panic!("the gzipped profile reads whole");
        };
        assert_eq!(plain, from_gz);
    }

    #[test]
    fn truncation_recovery() {
        let json = r#"{
            "otherData": {"bazel_version":"7.0.0"},
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"c","name":"a","ph":"X","ts":0,"dur":1,"pid":1,"tid":1},
                {"cat":"c","name":"b","ph":"X","ts":2,"dur":1,"pid":1,"tid":1},
                {"cat":"c","name":"trunca"#;
        let Ok(Recorded::CutOff(p)) = Profile::from_bytes(json.as_bytes()) else {
            panic!("the profile is cut off");
        };
        assert_eq!(p.opt_str(p.metadata().bazel_version), Some("7.0.0"));
        let names: Vec<_> = p
            .slices(&p.threads()[0])
            .iter()
            .map(|s| p.str(s.name))
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn a_whole_profile_with_an_event_that_does_not_parse_fails_where_it_stops() {
        let json = "{\n  \"traceEvents\": [\n    {\"name\":\"a\",\"ph\":\"X\",\"ts\":0,\"dur\":1,\"tid\":1},\n    {\"name\":\"b\",\"ph\":\"X\",\"ts\":\"late\",\"dur\":1,\"tid\":1}\n  ]\n}";
        let Err(LoadError::Json(e)) = Profile::from_bytes(json.as_bytes()) else {
            panic!("a whole profile with a malformed event does not load");
        };
        assert_eq!(e.line(), 4);
    }

    #[test]
    fn a_profile_whose_events_array_closes_is_not_cut_off() {
        let json = r#"{"traceEvents":[{"name":"a","ph":"X","ts":0,"dur":1,"tid":1}]} trailing"#;
        let Err(LoadError::Json(e)) = Profile::from_bytes(json.as_bytes()) else {
            panic!("a profile with trailing text does not load");
        };
        assert_eq!(e.column(), 64);
    }

    #[test]
    fn a_cut_off_profile_with_an_event_that_does_not_parse_fails() {
        let json = r#"{"traceEvents":[
            {"name":"a","ph":"X","ts":0,"dur":1,"tid":1},
            {"name":"b","ph":"X","ts":"late","dur":1,"tid":1},
            {"name":"c","ph":"X","ts":2,"dur":1,"tid":1},
            {"name":"trunca"#;
        assert!(matches!(
            Profile::from_bytes(json.as_bytes()),
            Err(LoadError::Json(_))
        ));
    }

    #[test]
    fn text_that_recovers_nothing_fails_where_it_stops_parsing() {
        for (text, line, column) in [
            ("<html>", 1, 1),
            (
                "{\n  \"traceEvents\": [\n    {\"name\": oops}\n  ]\n}",
                3,
                14,
            ),
        ] {
            let Err(LoadError::Json(e)) = Profile::from_bytes(text.as_bytes()) else {
                panic!("{text:?} is no profile");
            };
            assert_eq!((e.line(), e.column()), (line, column), "{text:?}");
        }
    }

    #[test]
    fn latin1_to_utf8_transcodes_high_bytes() {
        // Pure ASCII passes through unchanged (the moved-buffer fast path).
        assert_eq!(latin1_to_utf8(b"Az0".to_vec()), "Az0");
        // 0xe9 is Latin-1 'é', two bytes in UTF-8; surrounding ASCII runs are kept.
        assert_eq!(latin1_to_utf8(vec![b'A', 0xe9, b'z']), "Aéz");
        // Consecutive leading high bytes (0xe9='é', 0xe8='è') then ASCII, and a
        // trailing high byte (0xff='ÿ').
        assert_eq!(latin1_to_utf8(vec![0xe9, 0xe8, b'x']), "éèx");
        assert_eq!(latin1_to_utf8(vec![b'x', 0xff]), "xÿ");
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn from_path_reads_gzipped_profile() {
        let json = r#"{
            "otherData": {"bazel_version":"7.0.0"},
            "traceEvents": [
                {"name":"thread_name","ph":"M","tid":1,"args":{"name":"L"}},
                {"cat":"c","name":"a","ph":"X","ts":0,"dur":1,"pid":1,"tid":1}
            ]
        }"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let dir = test_dir();
        let path = std::path::Path::new(&dir).join("from_path_streams.profile.gz");
        std::fs::write(&path, &gz).unwrap();
        assert_eq!(
            Profile::from_path(&path).expect("stream").into_inner(),
            Profile::from_bytes(json.as_bytes())
                .expect("bytes")
                .into_inner(),
        );
    }

    #[test]
    fn a_streamed_input_loads_whole() {
        let json = chunkable_profile_json();
        let stream = |bytes: Vec<u8>| {
            let access = bviz_input::Access::Stream {
                reader: Box::new(std::io::Cursor::new(bytes)),
                size: None,
            };
            Input::new("stream".into(), access).expect("an in-memory input reads")
        };
        let serial = semantic(&parse_streaming(&json).expect("serial"));
        for bytes in [json.clone().into_bytes(), gzip(&json)] {
            let loaded = Profile::from_input(stream(bytes)).expect("loads");
            assert_eq!(semantic(&loaded.into_inner()), serial);
        }
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn from_path_rejects_sub_frame_gzip() {
        // A gzip-magic file shorter than a full frame: its last four bytes
        // overlap the header, so read as a trailer they decode to a ~4 GiB bogus
        // ISIZE. from_path must not size the streamed parse from that trailer; it
        // falls back to the whole-buffer path and reports a clean error rather
        // than over-allocating or panicking.
        let dir = test_dir();
        let path = std::path::Path::new(&dir).join("sub_frame.profile.gz");
        std::fs::write(&path, [0x1f, 0x8b, 0xff, 0xff, 0xff, 0xff]).unwrap();
        assert!(Profile::from_path(&path).is_err());
    }

    fn gzip(json: &str) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    /// `gz` with its ISIZE trailer replaced by `isize`.
    fn with_trailer(mut gz: Vec<u8>, isize: u32) -> Vec<u8> {
        let at = gz.len() - GZIP_ISIZE_BYTES;
        gz[at..].copy_from_slice(&isize.to_le_bytes());
        gz
    }

    #[test]
    fn a_garbage_trailer_fails_cleanly() {
        let gz = with_trailer(gzip(&chunkable_profile_json()), u32::MAX);
        assert!(matches!(
            Profile::from_bytes(&gz),
            Err(LoadError::Decompress(_))
        ));
    }

    #[test]
    fn a_size_hint_is_bounded_by_the_compressed_length() {
        let gz = with_trailer(gzip(&chunkable_profile_json()), u32::MAX);
        let hint = gzip_size_hint(&gz).expect("a gzip stream");
        assert!(hint < u32::MAX as usize);
        assert!(hint <= gz.len() * MAX_PLAUSIBLE_GZIP_RATIO);
    }

    #[test]
    fn a_stream_its_trailer_understates_loads_through_the_fallback() {
        let json = chunkable_profile_json();
        let serial = semantic(&parse_streaming(&json).expect("serial"));
        // Two members, so the trailer declares only the short last one.
        let last_member_at = json.len() - 2;
        let mut gz = gzip(&json[..last_member_at]);
        gz.extend(gzip(&json[last_member_at..]));
        assert_eq!(gzip_size_hint(&gz), Some(json.len() - last_member_at));
        // A pool as short as that hint runs out mid-parse.
        const PAGE: usize = 16;
        let short_pool = || ConcurrentInterner::with_exact_pool(4 * PAGE, PAGE);
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        assert!(parse_gz_stream_into(&gz[..], short_pool(), 2, 64, 0).is_none());
        assert!(matches!(
            run_here(parse_serial_into(
                MultiGzDecoder::new(&gz[..]),
                short_pool(),
                64,
                0,
                &mut Unpaused,
            )),
            Err(Declined::PoolExhausted)
        ));
        let loaded = Profile::from_bytes(&gz)
            .expect("the fallback loads it")
            .into_inner();
        assert_eq!(semantic(&loaded), serial);
    }

    #[test]
    fn a_serial_decode_whose_pool_runs_out_decodes_again_into_a_full_one() {
        let json = chunkable_profile_json();
        const PAGE: usize = 16;
        let mut reopened = false;
        let Some(Recorded::Complete(p)) = run_here(parse_serial_retrying(
            json.as_bytes(),
            || {
                reopened = true;
                Some(json.as_bytes())
            },
            ConcurrentInterner::with_exact_pool(4 * PAGE, PAGE),
            || ConcurrentInterner::new(json.len(), 1).ok(),
            64,
            json.len(),
            &mut Unpaused,
        )) else {
            panic!("the full pool holds it");
        };
        assert!(reopened);
        assert_eq!(
            semantic(&p),
            semantic(&parse_streaming(&json).expect("serial"))
        );
    }

    #[test]
    fn a_streamed_input_whose_pool_runs_out_is_read_again() {
        // Text nearly all distinct, and long enough beside the pool's fixed
        // padding that it outgrows the first pool.
        const NAME_BYTES: usize = 2000;
        const PADDINGS: usize = 3;
        let padding = first_pool(0).unwrap().pool_bytes();
        let json = streamable_profile_json(NAME_BYTES, PADDINGS * padding);
        let decoded_size = json.len();
        assert!(matches!(
            run_here(parse_serial_into(
                json.as_bytes(),
                first_pool(decoded_size).unwrap(),
                MIN_STREAMED_BYTES,
                decoded_size,
                &mut Unpaused,
            )),
            Err(Declined::PoolExhausted)
        ));
        let whole = semantic(&parse_streaming(&json).expect("serial"));
        for bytes in [gzip(&json), json.into_bytes()] {
            let Some(Recorded::Complete(p)) =
                run_here(parse_serial_input(&memory_input(bytes), &mut Unpaused))
            else {
                panic!("read again into a full pool");
            };
            assert_eq!(semantic(&p), whole);
        }
    }

    #[test]
    fn a_truncated_gzip_loads_by_recovery() {
        let json = chunkable_profile_json();
        let gz = gzip(&json);
        let truncated = &gz[..gz.len() * 3 / 4];
        let Ok(Recorded::CutOff(p)) = Profile::from_bytes(truncated) else {
            panic!("the profile is cut off");
        };
        assert_eq!(p.opt_str(p.metadata().bazel_version), Some("9.0.0"));
        assert!(!p.threads().is_empty());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_truncated_gzip_file_loads_by_recovery() {
        let json = chunkable_profile_json();
        let gz = gzip(&json);
        let dir = test_dir();
        let path = std::path::Path::new(&dir).join("truncated.profile.gz");
        std::fs::write(&path, &gz[..gz.len() * 3 / 4]).unwrap();
        let Ok(Recorded::CutOff(p)) = Profile::from_path(&path) else {
            panic!("the profile is cut off");
        };
        assert_eq!(p.opt_str(p.metadata().bazel_version), Some("9.0.0"));
        assert!(!p.threads().is_empty());
    }
}
