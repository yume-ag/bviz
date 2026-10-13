use std::io::{self, ErrorKind, Read};
use std::time::Duration;

use bazel_spawn_proto::duration_proto::google::protobuf::Duration as ProtoDuration;
use bazel_spawn_proto::tools::protos::SpawnMetrics;
use bazel_spawn_proto::tools::protos::exec_log_entry::Invocation;
use bviz_input::Input;
use bviz_invocation::spawn::{self, Bytes, Logged, PhaseTiming, REPORTED_PHASES};
use bviz_invocation::{InvocationId, Recorded};
use bviz_parallel::worker_count;
use prost::{DecodeError, Message};
use rustc_hash::FxHashMap;

use crate::frame::{first_message, for_each_message, for_each_message_pipelined};
use crate::spawn_fields::{digest, exec_log_entry};
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
    let read = match worker_count() > 1 {
        true => for_each_message_pipelined(decoder, READ_CHUNK_BYTES, add)?,
        false => for_each_message(decoder, READ_CHUNK_BYTES, add)?,
    };
    if !any_entry {
        return Err(LoadError::NotALog);
    }
    Ok(read.map(|()| builder.finish()))
}

fn estimate(compressed: u64, bytes_per_item: u64) -> usize {
    usize::try_from(compressed / bytes_per_item).unwrap_or(usize::MAX)
}

/// Ids below this always index [`Slots`]' vector.
const DENSE_ID_FLOOR: usize = 4 << 10;

/// Ids per held entry [`Slots`]' vector may span: Bazel numbers every entry
/// from one counter, so the held entries' ids skip those of the others between.
const DENSE_IDS_PER_ENTRY: usize = 4;

/// The slot each held entry's data lies at in [`Entries`], by entry id.
///
/// Bazel numbers entries from a counter, so ids run close to dense and index a
/// vector; one beyond what the entries held so far could span goes to a map, so
/// the vector stays within a multiple of the entries it holds.
struct Slots {
    dense: Vec<Option<u32>>,
    sparse: FxHashMap<u32, u32>,
}

impl Slots {
    fn with_capacity(entries: usize) -> Slots {
        Slots {
            dense: Vec::with_capacity(entries),
            sparse: FxHashMap::default(),
        }
    }

    /// Points `id` at `slot`, the `slot + 1`th entry held.
    fn insert(&mut self, id: u32, slot: u32) {
        let index = id as usize;
        if index < DENSE_ID_FLOOR.max((slot as usize + 1) * DENSE_IDS_PER_ENTRY) {
            if index >= self.dense.len() {
                self.dense.resize(index + 1, None);
            }
            self.dense[index] = Some(slot);
        } else {
            self.sparse.insert(id, slot);
        }
    }

    fn get(&self, id: u32) -> Option<u32> {
        match self.dense.get(id as usize) {
            Some(Some(slot)) => Some(*slot),
            _ => self.sparse.get(&id).copied(),
        }
    }
}

/// The path and content of each file, directory and unresolved symlink entry.
struct Entries {
    slots: Slots,
    text: String,
    /// Where each slot's path ends in `text`, the next slot's starting there.
    path_ends: Vec<usize>,
    bytes: Vec<Bytes>,
}

impl Entries {
    /// Room for the entries of a log of `entries` entries, their paths
    /// `text_bytes` long together.
    fn with_capacity(entries: usize, text_bytes: usize) -> Entries {
        Entries {
            slots: Slots::with_capacity(entries),
            text: String::with_capacity(text_bytes),
            path_ends: Vec::with_capacity(entries),
            bytes: Vec::with_capacity(entries),
        }
    }

    fn insert(&mut self, id: u32, path: &str, bytes: Bytes) -> Result<(), DecodeError> {
        let slot = u32::try_from(self.path_ends.len())
            .map_err(|_| DecodeError::new("more entries than ids"))?;
        self.text.push_str(path);
        self.path_ends.push(self.text.len());
        self.bytes.push(bytes);
        self.slots.insert(id, slot);
        Ok(())
    }

    /// The path and content of entry `id`.
    fn get(&self, id: u32) -> Option<(&str, Bytes)> {
        let slot = self.slots.get(id)? as usize;
        let start = slot
            .checked_sub(1)
            .map_or(0, |before| self.path_ends[before]);
        Some((&self.text[start..self.path_ends[slot]], self.bytes[slot]))
    }
}

struct Builder {
    entries: Entries,
    spawns: spawn::Builder,
}

impl Builder {
    fn with_capacity(compressed: u64) -> Builder {
        let path_bytes = compressed.saturating_mul(PATH_BYTES_PER_COMPRESSED_KIB) >> 10;
        let output_bytes = compressed.saturating_mul(OUTPUT_BYTES_PER_COMPRESSED_KIB) >> 10;
        Builder {
            entries: Entries::with_capacity(
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
            }) => self.add_file(id, bytes)?,
            Some(Payload {
                kind: exec_log_entry::DIRECTORY,
                bytes,
            }) => self.add_directory(id, bytes)?,
            Some(Payload {
                kind: exec_log_entry::UNRESOLVED_SYMLINK,
                bytes,
            }) => self.add_unresolved_symlink(id, bytes)?,
            Some(Payload {
                kind: exec_log_entry::SPAWN,
                bytes,
            }) => self.add_spawn(bytes)?,
            _ => {}
        }
        Ok(())
    }

    fn add_file(&mut self, id: u32, mut file: &[u8]) -> Result<(), DecodeError> {
        let mut path = "";
        let mut bytes = Bytes::ZERO;
        while !file.is_empty() {
            match next_field(&mut file)? {
                (exec_log_entry::file::PATH, Value::Bytes(value)) => path = text(value)?,
                (exec_log_entry::file::DIGEST, Value::Bytes(digest)) => {
                    bytes = digest_bytes(digest)?
                }
                _ => {}
            }
        }
        self.entries.insert(id, path, bytes)
    }

    fn add_directory(&mut self, id: u32, mut directory: &[u8]) -> Result<(), DecodeError> {
        let mut path = "";
        let mut bytes = Bytes::ZERO;
        while !directory.is_empty() {
            match next_field(&mut directory)? {
                (exec_log_entry::directory::PATH, Value::Bytes(value)) => path = text(value)?,
                (exec_log_entry::directory::FILES, Value::Bytes(file)) => {
                    bytes += file_bytes(file)?
                }
                _ => {}
            }
        }
        self.entries.insert(id, path, bytes)
    }

    fn add_unresolved_symlink(&mut self, id: u32, mut symlink: &[u8]) -> Result<(), DecodeError> {
        let mut path = "";
        while !symlink.is_empty() {
            if let (exec_log_entry::unresolved_symlink::PATH, Value::Bytes(value)) =
                next_field(&mut symlink)?
            {
                path = text(value)?;
            }
        }
        self.entries.insert(id, path, Bytes::ZERO)
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
                    if let Some((path, bytes)) = spawn_output(&self.entries, output)? {
                        self.spawns.output(path, bytes);
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

/// The path and content of the spawn output encoded in `output`.
fn spawn_output<'a>(
    entries: &'a Entries,
    mut output: &'a [u8],
) -> Result<Option<(&'a str, Bytes)>, DecodeError> {
    let mut found = None;
    while !output.is_empty() {
        match next_field(&mut output)? {
            (exec_log_entry::output::OUTPUT_ID, Value::Varint(id)) => {
                found = entries.get(entry_id(id)?);
            }
            (exec_log_entry::output::INVALID_OUTPUT_PATH, Value::Bytes(bytes)) => {
                found = Some((text(bytes)?, Bytes::ZERO));
            }
            _ => {}
        }
    }
    Ok(found)
}

/// The content of the file encoded in `file`, none where it has no digest.
fn file_bytes(mut file: &[u8]) -> Result<Bytes, DecodeError> {
    let mut bytes = Bytes::ZERO;
    while !file.is_empty() {
        if let (exec_log_entry::file::DIGEST, Value::Bytes(digest)) = next_field(&mut file)? {
            bytes = digest_bytes(digest)?;
        }
    }
    Ok(bytes)
}

/// The content size the digest encoded in `digest` names; a negative one
/// reads as none.
fn digest_bytes(mut digest: &[u8]) -> Result<Bytes, DecodeError> {
    let mut bytes = Bytes::ZERO;
    while !digest.is_empty() {
        if let (digest::SIZE_BYTES, Value::Varint(size)) = next_field(&mut digest)? {
            bytes = Bytes::new(u64::try_from(size as i64).unwrap_or(0));
        }
    }
    Ok(bytes)
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
    fn a_spawn_wrote_the_content_of_its_outputs() {
        let mut writes = spawn("remote", "M", metrics(1, &[]));
        writes.outputs = vec![
            output_id(1),
            output_id(2),
            output_id(3),
            output_id(4),
            invalid_output("bazel-out/missing"),
        ];
        let mut writes_nothing = spawn("remote", "M", metrics(1, &[]));
        writes_nothing.outputs = vec![output_id(3), output_id(4)];
        let log = load(&[
            sized_file(1, "bazel-out/lib.o", 1000),
            directory(
                2,
                "bazel-out/tree",
                vec![sized("a", 30), sized("b", 7), file_in("empty")],
            ),
            file(3, "bazel-out/empty"),
            unresolved_symlink(4, "bazel-out/link"),
            entry(0, Type::Spawn(writes)),
            entry(0, Type::Spawn(writes_nothing)),
        ]);
        let written: Vec<u64> = log.spawns().map(|s| s.output_bytes().get()).collect();
        assert_eq!(written, [1037, 0]);
    }

    fn file_in(path: &str) -> exec_log_entry::File {
        exec_log_entry::File {
            path: path.into(),
            digest: None,
        }
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
        let entries = Entries::with_capacity(0, 0);
        assert!(spawn_output(&entries, &output).is_err());
        let within = varint_field(fields::output::OUTPUT_ID, u64::from(u32::MAX));
        assert!(matches!(spawn_output(&entries, &within), Ok(None)));
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
