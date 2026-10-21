use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::time::Duration;

use bazel_spawn_proto::duration_proto::google::protobuf::Duration as ProtoDuration;
use bazel_spawn_proto::tools::protos::SpawnMetrics;
use bazel_spawn_proto::tools::protos::exec_log_entry::Invocation;
use bviz_input::Input;
use bviz_invocation::spawn::{self, Logged, PhaseTiming, REPORTED_PHASES};
use bviz_invocation::{InvocationId, Recorded};
use prost::{DecodeError, Message};
use rustc_hash::FxHashMap;

use crate::frame::{first_message, for_each_message};
use crate::spawn_fields::exec_log_entry;
use crate::wire::{Value, next_field, text};
use crate::{ExecLog, LoadError};

/// Compressed log bytes per spawn, rounded down from the densest logs measured
/// so the spawn list is reserved at or above its final length.
const COMPRESSED_BYTES_PER_SPAWN: u64 = 512;

/// Compressed log bytes per entry of any kind, rounded down like
/// [`COMPRESSED_BYTES_PER_SPAWN`].
const COMPRESSED_BYTES_PER_ENTRY: u64 = 48;

/// Entry path bytes per KiB of compressed log, rounded up from the most
/// path-laden logs measured so the path text is reserved at or above its final
/// length.
const PATH_BYTES_PER_COMPRESSED_KIB: u64 = 1344;

/// Compressed log bytes per spawn output, rounded down like
/// [`COMPRESSED_BYTES_PER_SPAWN`].
const COMPRESSED_BYTES_PER_OUTPUT: u64 = 192;

/// Output path bytes per KiB of compressed log, rounded up like
/// [`PATH_BYTES_PER_COMPRESSED_KIB`].
const OUTPUT_BYTES_PER_COMPRESSED_KIB: u64 = 192;

/// Decoded bytes read from the log at a time.
const READ_CHUNK_BYTES: usize = 256 << 10;

pub(crate) fn read_input(input: Input) -> Result<Recorded<ExecLog>, LoadError> {
    let compressed = input.size().unwrap_or(0);
    read_compressed(input.into_reader(), compressed)
}

/// Opens every zstd frame, and so every execution log.
const ZSTD_MAGIC: [u8; 4] = zstd::zstd_safe::MAGICNUMBER.to_le_bytes();

/// Reads a log of `compressed` bytes from `log`.
pub(crate) fn read_compressed(
    mut log: impl Read + Send,
    compressed: u64,
) -> Result<Recorded<ExecLog>, LoadError> {
    let mut magic = [0u8; ZSTD_MAGIC.len()];
    match log.read_exact(&mut magic) {
        Ok(()) if magic == ZSTD_MAGIC => {}
        Ok(()) => return Err(LoadError::NotALog),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Err(LoadError::NotALog),
        Err(e) => return Err(LoadError::Io(e)),
    }
    let log = io::Cursor::new(magic).chain(log);
    let decoder = zstd::stream::read::Decoder::new(log).map_err(LoadError::Io)?;
    let mut builder = Builder::with_capacity(compressed);
    let mut any_entry = false;
    let add = |entry: &[u8]| {
        any_entry = true;
        Ok(builder.add(entry)?)
    };
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    let read = match bviz_parallel::worker_count() > 1 {
        true => crate::frame::for_each_message_pipelined(decoder, READ_CHUNK_BYTES, add)?,
        false => for_each_message(decoder, READ_CHUNK_BYTES, add)?,
    };
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    let read = for_each_message(decoder, READ_CHUNK_BYTES, add)?;
    if !any_entry {
        return Err(LoadError::NotALog);
    }
    Ok(read.map(|()| builder.finish()))
}

fn estimate(compressed: u64, bytes_per_item: u64) -> usize {
    usize::try_from(compressed / bytes_per_item).unwrap_or(usize::MAX)
}

/// Ids below this always index [`Paths`]'s vector.
const DENSE_ID_FLOOR: usize = 4 << 10;

/// Ids per held path [`Paths`]'s vector may span: Bazel numbers every entry
/// from one counter, so the path entries' ids skip those of the others between.
const DENSE_IDS_PER_PATH: usize = 4;

/// The path of each file, directory and unresolved symlink entry, by entry id.
///
/// Bazel numbers entries from a counter, so ids run close to dense and index a
/// vector; one beyond what the paths held so far could span goes to a map, so
/// the vector stays within a multiple of the paths it holds.
struct Paths {
    text: String,
    dense: Vec<Option<Range<usize>>>,
    sparse: FxHashMap<u32, Range<usize>>,
    held: usize,
}

impl Paths {
    /// Room for the paths of a log of `entries` entries, `text_bytes` long
    /// together.
    fn with_capacity(entries: usize, text_bytes: usize) -> Paths {
        Paths {
            text: String::with_capacity(text_bytes),
            dense: Vec::with_capacity(entries),
            sparse: FxHashMap::default(),
            held: 0,
        }
    }

    fn insert(&mut self, id: u32, path: &str) {
        let start = self.text.len();
        self.text.push_str(path);
        let range = start..self.text.len();
        self.held += 1;
        let index = id as usize;
        if index < DENSE_ID_FLOOR.max(self.held * DENSE_IDS_PER_PATH) {
            if index >= self.dense.len() {
                self.dense.resize(index + 1, None);
            }
            self.dense[index] = Some(range);
        } else {
            self.sparse.insert(id, range);
        }
    }

    fn get(&self, id: u32) -> Option<&str> {
        let range = match self.dense.get(id as usize) {
            Some(Some(range)) => range,
            _ => self.sparse.get(&id)?,
        };
        Some(&self.text[range.clone()])
    }
}

struct Builder {
    paths: Paths,
    spawns: spawn::Builder,
}

impl Builder {
    fn with_capacity(compressed: u64) -> Builder {
        let path_bytes = compressed.saturating_mul(PATH_BYTES_PER_COMPRESSED_KIB) >> 10;
        let output_bytes = compressed.saturating_mul(OUTPUT_BYTES_PER_COMPRESSED_KIB) >> 10;
        Builder {
            paths: Paths::with_capacity(
                estimate(compressed, COMPRESSED_BYTES_PER_ENTRY),
                usize::try_from(path_bytes).unwrap_or(usize::MAX),
            ),
            spawns: spawn::Builder::with_capacity(
                estimate(compressed, COMPRESSED_BYTES_PER_SPAWN),
                estimate(compressed, COMPRESSED_BYTES_PER_OUTPUT),
                usize::try_from(output_bytes).unwrap_or(usize::MAX),
            ),
        }
    }

    fn add(&mut self, entry: &[u8]) -> Result<(), DecodeError> {
        let Entry { id, payload } = split_entry(entry)?;
        match payload {
            Some(Payload {
                kind: exec_log_entry::FILE,
                bytes,
            }) => self.add_path(id, bytes, exec_log_entry::file::PATH)?,
            Some(Payload {
                kind: exec_log_entry::DIRECTORY,
                bytes,
            }) => self.add_path(id, bytes, exec_log_entry::directory::PATH)?,
            Some(Payload {
                kind: exec_log_entry::UNRESOLVED_SYMLINK,
                bytes,
            }) => self.add_path(id, bytes, exec_log_entry::unresolved_symlink::PATH)?,
            Some(Payload {
                kind: exec_log_entry::SPAWN,
                bytes,
            }) => self.add_spawn(bytes)?,
            _ => {}
        }
        Ok(())
    }

    /// Holds field `path_field` of `payload` as the path of entry `id`.
    fn add_path(
        &mut self,
        id: u32,
        mut payload: &[u8],
        path_field: u32,
    ) -> Result<(), DecodeError> {
        while !payload.is_empty() {
            if let (field, Value::Bytes(path)) = next_field(&mut payload)?
                && field == path_field
            {
                self.paths.insert(id, text(path)?);
            }
        }
        Ok(())
    }

    fn add_spawn(&mut self, mut spawn: &[u8]) -> Result<(), DecodeError> {
        let mut target_label = "";
        let mut mnemonic = "";
        let mut runner = "";
        let mut cache_hit = false;
        let mut metrics = SpawnMetrics::default();
        while !spawn.is_empty() {
            match next_field(&mut spawn)? {
                (exec_log_entry::spawn::OUTPUTS, Value::Bytes(output)) => {
                    if let Some(path) = output_path(&self.paths, output)? {
                        self.spawns.output(path);
                    }
                }
                (exec_log_entry::spawn::TARGET_LABEL, Value::Bytes(bytes)) => {
                    target_label = text(bytes)?
                }
                (exec_log_entry::spawn::MNEMONIC, Value::Bytes(bytes)) => mnemonic = text(bytes)?,
                (exec_log_entry::spawn::RUNNER, Value::Bytes(bytes)) => runner = text(bytes)?,
                (exec_log_entry::spawn::CACHE_HIT, Value::Varint(hit)) => cache_hit = hit != 0,
                (exec_log_entry::spawn::METRICS, Value::Bytes(bytes)) => {
                    metrics = SpawnMetrics::decode(bytes)?;
                }
                _ => {}
            }
        }
        self.spawns.push(Logged {
            runner,
            mnemonic,
            target_label,
            cache_hit,
            timing: timing(metrics),
        });
        Ok(())
    }

    fn finish(self) -> ExecLog {
        ExecLog {
            spawns: self.spawns.finish(),
        }
    }
}

/// The path of the spawn output encoded in `output`.
fn output_path<'a>(paths: &'a Paths, mut output: &'a [u8]) -> Result<Option<&'a str>, DecodeError> {
    let mut path = None;
    while !output.is_empty() {
        match next_field(&mut output)? {
            (exec_log_entry::output::OUTPUT_ID, Value::Varint(id)) => {
                path = paths.get(entry_id(id)?);
            }
            (exec_log_entry::output::INVALID_OUTPUT_PATH, Value::Bytes(bytes)) => {
                path = Some(text(bytes)?);
            }
            _ => {}
        }
    }
    Ok(path)
}

struct Entry<'a> {
    id: u32,
    payload: Option<Payload<'a>>,
}

struct Payload<'a> {
    /// The field number the payload is held at, which names its kind.
    kind: u32,
    bytes: &'a [u8],
}

fn split_entry(mut entry: &[u8]) -> Result<Entry<'_>, DecodeError> {
    let mut id = 0;
    let mut payload = None;
    while !entry.is_empty() {
        match next_field(&mut entry)? {
            (exec_log_entry::ID, Value::Varint(value)) => id = entry_id(value)?,
            (kind, Value::Bytes(bytes)) => payload = Some(Payload { kind, bytes }),
            _ => {}
        }
    }
    Ok(Entry { id, payload })
}

/// The entry id a varint holds: ids are `uint32` fields, so a varint past
/// them is no id Bazel writes.
fn entry_id(value: u64) -> Result<u32, DecodeError> {
    u32::try_from(value).map_err(|_| DecodeError::new("entry id out of range"))
}

fn invocation_id(invocation: &[u8]) -> Result<Option<InvocationId>, DecodeError> {
    Ok(InvocationId::new(&Invocation::decode(invocation)?.id))
}

fn timing(metrics: SpawnMetrics) -> PhaseTiming {
    let reported: [Option<ProtoDuration>; REPORTED_PHASES] = [
        metrics.parse_time,
        metrics.upload_time,
        metrics.network_time,
        metrics.queue_time,
        metrics.setup_time,
        metrics.execution_wall_time,
        metrics.process_outputs_time,
        metrics.fetch_time,
        metrics.retry_time,
    ];
    PhaseTiming::new(duration(metrics.total_time), reported.map(duration))
}

/// `None` and negative durations read as none at all.
fn duration(proto: Option<ProtoDuration>) -> Duration {
    proto
        .and_then(|d| {
            Some(Duration::new(
                u64::try_from(d.seconds).ok()?,
                u32::try_from(d.nanos).ok()?,
            ))
        })
        .unwrap_or_default()
}

/// Decoded bytes [`probe_invocation_id`] reads from the head of a log, many
/// times the invocation entry a log opens with.
const PROBE_HEAD_BYTES: usize = 64 << 10;

/// The invocation that wrote the execution log `input` holds, from the
/// invocation entry it opens with in the input's head. `None` when the head
/// opens with no such entry.
pub fn probe_invocation_id(input: &Input) -> Option<InvocationId> {
    invocation_id_at_head(input.head()?, PROBE_HEAD_BYTES)
}

/// [`probe_invocation_id`] over the first `head_bytes` decoded bytes of a log.
fn invocation_id_at_head(log: impl std::io::BufRead, head_bytes: usize) -> Option<InvocationId> {
    let mut head = Vec::with_capacity(head_bytes);
    // A stream that fails mid-way has still yielded the bytes ahead of the
    // failure.
    let _ = zstd::stream::read::Decoder::with_buffer(log)
        .ok()?
        .take(head_bytes as u64)
        .read_to_end(&mut head);
    match split_entry(first_message(&head)?).ok()?.payload? {
        Payload {
            kind: exec_log_entry::INVOCATION,
            bytes,
        } => invocation_id(bytes).ok()?,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bazel_spawn_proto::tools::protos::exec_log_entry::{self, Type};

    use super::*;
    use crate::Phase;
    use crate::spawn_fields::exec_log_entry as fields;
    use crate::test_log::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn reads_what_each_spawn_names() {
        let mut compile = spawn("remote", "CppCompile", metrics(10, &[]));
        compile.target_label = "//pkg:lib".into();
        compile.outputs = vec![output_id(3), output_id(4)];
        let mut failed = spawn("linux-sandbox", "Genrule", metrics(5, &[]));
        failed.outputs = vec![invalid_output("bazel-out/bin/missing")];
        let mut tree = spawn("disk cache hit", "TreeAction", metrics(1, &[]));
        tree.outputs = vec![output_id(5)];
        tree.cache_hit = true;
        let mut unknown = spawn("remote", "CppCompile", metrics(1, &[]));
        unknown.outputs = vec![output_id(99), output_id(4)];
        let log = load(&[
            invocation("abc"),
            file(3, "bazel-out/bin/lib.o"),
            file(4, "bazel-out/bin/lib.d"),
            entry(
                5,
                Type::Directory(exec_log_entry::Directory {
                    path: "bazel-out/bin/tree".into(),
                    files: vec![exec_log_entry::File {
                        path: "inside".into(),
                        digest: None,
                    }],
                }),
            ),
            entry(
                6,
                Type::InputSet(exec_log_entry::InputSet {
                    input_ids: vec![3, 4],
                    transitive_set_ids: vec![],
                }),
            ),
            entry(0, Type::Spawn(compile)),
            entry(0, Type::Spawn(failed)),
            entry(0, Type::Spawn(tree)),
            entry(0, Type::Spawn(unknown)),
        ]);
        let spawns: Vec<_> = log.spawns().collect();
        assert_eq!(spawns.len(), 4);

        assert_eq!(spawns[0].runner(), "remote");
        assert_eq!(spawns[0].mnemonic(), "CppCompile");
        assert_eq!(spawns[0].target_label(), Some("//pkg:lib"));
        assert_eq!(spawns[0].primary_output(), Some("bazel-out/bin/lib.o"));
        let outputs: Vec<&str> = spawns[0].outputs().collect();
        assert_eq!(outputs, ["bazel-out/bin/lib.o", "bazel-out/bin/lib.d"]);

        assert_eq!(spawns[1].target_label(), None);
        assert_eq!(spawns[1].primary_output(), Some("bazel-out/bin/missing"));
        assert_eq!(spawns[2].primary_output(), Some("bazel-out/bin/tree"));
        let unknown_outputs: Vec<&str> = spawns[3].outputs().collect();
        assert_eq!(unknown_outputs, ["bazel-out/bin/lib.d"]);

        let hits: Vec<bool> = spawns.iter().map(|s| s.cache_hit()).collect();
        assert_eq!(hits, [false, false, true, false]);
    }

    #[test]
    fn each_reported_phase_lands_in_its_own() {
        let reported: Vec<(Phase, u64)> = Phase::ALL[..REPORTED_PHASES]
            .iter()
            .zip(1..)
            .map(|(&phase, millis)| (phase, millis))
            .collect();
        let total = 100;
        let log = load(&[entry(
            0,
            Type::Spawn(spawn("remote", "M", metrics(total, &reported))),
        )]);
        let timing = *log.spawns().next().unwrap().timing();
        for &(phase, millis) in &reported {
            assert_eq!(timing.phase(phase), ms(millis), "{phase:?}");
        }
        let accounted: u64 = reported.iter().map(|&(_, millis)| millis).sum();
        assert_eq!(timing.phase(Phase::Other), ms(total - accounted));
        assert_eq!(timing.total(), ms(total));
    }

    #[test]
    fn a_far_flung_id_still_resolves() {
        let mut far = spawn("remote", "M", metrics(1, &[]));
        far.outputs = vec![output_id(u32::MAX)];
        let mut near = spawn("remote", "M", metrics(1, &[]));
        near.outputs = vec![output_id(1)];
        let log = load(&[
            file(u32::MAX, "far"),
            file(1, "near"),
            entry(0, Type::Spawn(far)),
            entry(0, Type::Spawn(near)),
        ]);
        let outputs: Vec<_> = log.spawns().map(|s| s.primary_output()).collect();
        assert_eq!(outputs, [Some("far"), Some("near")]);
    }

    /// Spawns enough that their log decompresses in several blocks, of which
    /// a cut log keeps those ahead of the cut.
    const SPAWNS_ACROSS_BLOCKS: usize = 4_000;

    #[test]
    fn a_cut_off_log_keeps_the_spawns_ahead_of_the_cut() {
        let entries: Vec<_> = (0..SPAWNS_ACROSS_BLOCKS)
            .map(|i| {
                let mut logged = spawn("remote", "CppCompile", metrics(1, &[]));
                logged.target_label = format!("//package/number/{i}:target_{}", i * 7919);
                entry(0, Type::Spawn(logged))
            })
            .collect();
        let log = encode(&entries);
        let mut kept = 0;
        let cuts = [
            1,
            log.len() / 4,
            log.len() / 2,
            log.len() * 3 / 4,
            log.len() - 1,
        ];
        for cut in cuts {
            let read = match read_compressed(&log[..cut], cut as u64) {
                Ok(Recorded::CutOff(read)) => read,
                // A cut ahead of the first block's end decodes no entry.
                Err(LoadError::NotALog) if kept == 0 => continue,
                _ => panic!("a log cut at {cut} of {} is cut off", log.len()),
            };
            let labels: Vec<_> = read.spawns().map(|s| s.target_label()).collect();
            let ahead: Vec<_> = entries[..labels.len()]
                .iter()
                .map(|entry| match &entry.r#type {
                    Some(Type::Spawn(spawn)) => Some(spawn.target_label.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(labels, ahead, "cut at {cut}");
            assert!(labels.len() >= kept, "cut at {cut}");
            kept = labels.len();
        }
        assert!(kept > 0, "the last cut keeps some spawns");
        assert!(kept < SPAWNS_ACROSS_BLOCKS);
    }

    #[test]
    fn a_file_that_decodes_to_no_entry_is_no_log() {
        let log = encode(&[entry(
            0,
            Type::Spawn(spawn("remote", "CppCompile", metrics(1, &[]))),
        )]);
        for file in [&[][..], &log[..1], &encode(&[])[..]] {
            assert!(
                matches!(
                    read_compressed(file, file.len() as u64),
                    Err(LoadError::NotALog)
                ),
                "{file:?}"
            );
        }
    }

    #[test]
    fn a_file_that_is_no_zstd_stream_is_no_log() {
        let text = b"not an execution log at all";
        assert!(matches!(
            read_compressed(&text[..], text.len() as u64),
            Err(LoadError::NotALog)
        ));
    }

    #[test]
    fn a_file_shorter_than_the_zstd_magic_is_no_log() {
        for len in [0, 1, ZSTD_MAGIC.len() - 1] {
            let short = &ZSTD_MAGIC[..len];
            assert!(
                matches!(read_compressed(short, len as u64), Err(LoadError::NotALog)),
                "{len} bytes"
            );
        }
    }

    /// `fields` as one length-delimited entry of a compressed log.
    fn raw_log(fields: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::with_capacity(crate::wire::MAX_VARINT_BYTES + fields.len());
        prost::encoding::encode_varint(fields.len() as u64, &mut decoded);
        decoded.extend_from_slice(fields);
        zstd::encode_all(decoded.as_slice(), 0).expect("compress in memory")
    }

    /// Field `number` of a message, holding the varint `value`.
    fn varint_field(number: u32, value: u64) -> Vec<u8> {
        let mut field = Vec::with_capacity(2 * crate::wire::MAX_VARINT_BYTES);
        prost::encoding::encode_key(number, prost::encoding::WireType::Varint, &mut field);
        prost::encoding::encode_varint(value, &mut field);
        field
    }

    const PAST_ENTRY_IDS: u64 = u32::MAX as u64 + 1;

    #[test]
    fn an_entry_id_past_the_ids_bazel_writes_fails_the_read() {
        let log = raw_log(&varint_field(fields::ID, PAST_ENTRY_IDS));
        assert!(matches!(
            read_compressed(log.as_slice(), log.len() as u64),
            Err(LoadError::Decode(_))
        ));
    }

    #[test]
    fn an_output_id_past_the_ids_bazel_writes_fails_the_read() {
        let output = varint_field(fields::output::OUTPUT_ID, PAST_ENTRY_IDS);
        let paths = Paths::with_capacity(0, 0);
        assert!(output_path(&paths, &output).is_err());
        let within = varint_field(fields::output::OUTPUT_ID, u64::from(u32::MAX));
        assert!(matches!(output_path(&paths, &within), Ok(None)));
    }

    #[test]
    fn probe_reads_the_opening_invocation() {
        let probe = |entries: &[_]| {
            let log = encode(entries);
            invocation_id_at_head(log.as_slice(), PROBE_HEAD_BYTES)
        };
        assert_eq!(
            probe(&[invocation("abc"), file(1, "a")]),
            InvocationId::new("abc")
        );
        assert_eq!(probe(&[file(1, "a"), invocation("abc")]), None);
        assert_eq!(probe(&[invocation("")]), None);
        assert_eq!(probe(&[]), None);
        assert_eq!(
            invocation_id_at_head(&b"not zstd"[..], PROBE_HEAD_BYTES),
            None
        );
    }

    #[test]
    fn probe_reads_an_opened_file() {
        let path = std::env::temp_dir().join("bviz_execlog_probe.binpb.zst");
        std::fs::write(&path, encode(&[invocation("abc")])).unwrap();
        let input = Input::open(&path).unwrap();
        assert_eq!(probe_invocation_id(&input), InvocationId::new("abc"));
    }
}
