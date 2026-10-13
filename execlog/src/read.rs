use std::io::{self, ErrorKind, Read};
use std::num::NonZeroU32;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use bazel_spawn_proto::duration_proto::google::protobuf::Duration as ProtoDuration;
use bazel_spawn_proto::tools::protos::SpawnMetrics;
use bazel_spawn_proto::tools::protos::exec_log_entry::Invocation;
use bviz_input::Input;
use bviz_invocation::spawn::{
    self, Bytes, InputBytes, Logged, MeasuredInputs, PhaseTiming, REPORTED_PHASES,
};
use bviz_invocation::{InvocationId, Recorded};
use bviz_parallel::{worker_count, workers};
use prost::{DecodeError, Message};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::frame::{first_message, for_each_message, for_each_message_pipelined};
use crate::spawn_fields::{digest, exec_log_entry};
use crate::wire::{Value, next_field, text, varint};
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

/// Entries the sets of a log hold per KiB of compressed log, rounded up from the
/// most set-laden logs measured so the held entries are reserved at or above
/// their final count.
const HELD_PER_COMPRESSED_KIB: u64 = 80;

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

/// Ids per held entry [`Slots`]' vector may span: every entry that takes an id
/// is held, and Bazel numbers them from one counter, but its log format
/// promises the ids unique, not contiguous.
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
    fn insert(&mut self, id: NonZeroU32, slot: u32) {
        let index = id.get() as usize;
        if index < DENSE_ID_FLOOR.max((slot as usize + 1) * DENSE_IDS_PER_ENTRY) {
            if index >= self.dense.len() {
                self.dense.resize(index + 1, None);
            }
            self.dense[index] = Some(slot);
        } else {
            self.sparse.insert(id.get(), slot);
        }
    }

    fn get(&self, id: NonZeroU32) -> Option<u32> {
        let id = id.get();
        match self.dense.get(id as usize) {
            Some(Some(slot)) => Some(*slot),
            _ => self.sparse.get(&id).copied(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A file, directory or unresolved symlink.
    Path,
    /// An input set, symlink entry set or runfiles tree.
    Set,
}

/// The entries a spawn's inputs and outputs are drawn from: the files,
/// directories and unresolved symlinks, each with a path, and the input sets,
/// symlink entry sets and runfiles trees that hold them and one another.
///
/// Bazel writes an entry after every entry it refers to, so the entries a set
/// holds are known by the time it is read, and the sets form no cycle.
struct Entries {
    slots: Slots,
    kinds: Vec<EntryKind>,
    text: String,
    /// Where each slot's path ends in `text`, the next slot's starting there;
    /// a set's adds none.
    path_ends: Vec<usize>,
    /// The content of each slot's entry itself, apart from the entries it
    /// holds.
    bytes: Vec<Bytes>,
    /// Where the entries each slot holds end in `held`, the next slot's
    /// starting there.
    held_ends: Vec<usize>,
    /// The slots of the entries the sets hold, set by set.
    held: Vec<u32>,
    /// The most entries one set holds.
    widest_set: usize,
}

impl Entries {
    /// Room for the entries of a log of `entries` entries, their paths
    /// `text_bytes` long together and the sets among them holding `held`
    /// entries between them.
    fn with_capacity(entries: usize, text_bytes: usize, held: usize) -> Entries {
        Entries {
            slots: Slots::with_capacity(entries),
            kinds: Vec::with_capacity(entries),
            text: String::with_capacity(text_bytes),
            path_ends: Vec::with_capacity(entries),
            bytes: Vec::with_capacity(entries),
            held_ends: Vec::with_capacity(entries),
            held: Vec::with_capacity(held),
            widest_set: 0,
        }
    }

    fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Adds entry `id` at `path` with `bytes` of content.
    fn insert_path(&mut self, id: NonZeroU32, path: &str, bytes: Bytes) -> Result<(), DecodeError> {
        self.text.push_str(path);
        self.insert(id, EntryKind::Path, bytes)
    }

    /// Adds set `id` with `bytes` of content of its own, holding the entries
    /// [`hold`](Self::hold) added since the entry added before it.
    fn insert_set(&mut self, id: NonZeroU32, bytes: Bytes) -> Result<(), DecodeError> {
        self.insert(id, EntryKind::Set, bytes)
    }

    fn insert(&mut self, id: NonZeroU32, kind: EntryKind, bytes: Bytes) -> Result<(), DecodeError> {
        let slot =
            u32::try_from(self.len()).map_err(|_| DecodeError::new("more entries than ids"))?;
        self.kinds.push(kind);
        self.path_ends.push(self.text.len());
        self.bytes.push(bytes);
        let holds = self.held.len() - self.held_ends.last().copied().unwrap_or(0);
        self.widest_set = self.widest_set.max(holds);
        self.held_ends.push(self.held.len());
        self.slots.insert(id, slot);
        Ok(())
    }

    /// Adds entry `id` to those the set [`insert_set`](Self::insert_set) adds
    /// next holds; an id [`slot`](Self::slot) finds none for is left out.
    fn hold(&mut self, id: u32) {
        if let Some(slot) = self.slot(id) {
            self.held.push(slot);
        }
    }

    /// The slot of entry `id`, `None` for id 0, which names no entry, and an
    /// id no entry added has.
    fn slot(&self, id: u32) -> Option<u32> {
        self.slots.get(NonZeroU32::new(id)?)
    }

    /// The path and content of entry `id`, `None` where [`slot`](Self::slot)
    /// finds none or it is a set.
    fn get(&self, id: u32) -> Option<(&str, Bytes)> {
        let slot = self.slot(id)? as usize;
        if self.kinds[slot] == EntryKind::Set {
            return None;
        }
        let start = slot
            .checked_sub(1)
            .map_or(0, |before| self.path_ends[before]);
        Some((&self.text[start..self.path_ends[slot]], self.bytes[slot]))
    }
}

/// The entries a log's spawns read and how the sets among them nest, kept
/// apart from what only reading the log needs, to measure the bytes each
/// spawn read.
struct InputGraph {
    bytes: Vec<Bytes>,
    /// As [`Entries`] holds them.
    held_ends: Vec<usize>,
    held: Vec<u32>,
    widest_set: usize,
    /// Each spawn's, in the order the log lists them.
    roots: Vec<SpawnInputs>,
}

impl InputGraph {
    fn new(entries: Entries, roots: Vec<SpawnInputs>) -> InputGraph {
        InputGraph {
            bytes: entries.bytes,
            held_ends: entries.held_ends,
            held: entries.held,
            widest_set: entries.widest_set,
            roots,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn held_by(&self, slot: u32) -> &[u32] {
        let slot = slot as usize;
        let start = slot
            .checked_sub(1)
            .map_or(0, |before| self.held_ends[before]);
        &self.held[start..self.held_ends[slot]]
    }

    /// The bytes each spawn read, walked on the worker pool.
    fn measure(&self) -> MeasuredInputs {
        let pool = workers();
        let walks: Vec<Mutex<Option<Walk>>> = (0..pool.current_num_threads())
            .map(|_| Mutex::new(None))
            .collect();
        let mut per_spawn = vec![InputBytes::default(); self.roots.len()];
        pool.install(|| {
            (per_spawn.par_iter_mut().zip(self.roots.par_iter())).for_each(|(read, &spawn)| {
                let worker = rayon::current_thread_index().unwrap_or(0);
                let mut walk = walks[worker].lock().unwrap_or_else(PoisonError::into_inner);
                *read = walk
                    .get_or_insert_with(|| Walk::new(self))
                    .input_bytes(self, spawn);
            })
        });
        MeasuredInputs::new(per_spawn)
    }
}

/// The input set and the tool set a spawn names, by slot, `None` where it
/// names none.
#[derive(Clone, Copy)]
struct SpawnInputs {
    inputs: Option<u32>,
    tools: Option<u32>,
}

/// Slots one word of a [`SlotSet`] marks.
const SLOTS_PER_WORD: usize = u64::BITS as usize;

/// A set of slots, a bit each.
struct SlotSet {
    words: Vec<u64>,
}

impl SlotSet {
    fn new(slots: usize) -> SlotSet {
        SlotSet {
            words: vec![0; slots.div_ceil(SLOTS_PER_WORD)],
        }
    }

    fn word_and_bit(slot: u32) -> (usize, u64) {
        let slot = slot as usize;
        (slot / SLOTS_PER_WORD, 1 << (slot % SLOTS_PER_WORD))
    }

    /// Adds `slot`, returning whether it was absent.
    fn insert(&mut self, slot: u32) -> bool {
        let (word, bit) = Self::word_and_bit(slot);
        let absent = self.words[word] & bit == 0;
        self.words[word] |= bit;
        absent
    }

    fn contains(&self, slot: u32) -> bool {
        let (word, bit) = Self::word_and_bit(slot);
        self.words[word] & bit != 0
    }

    fn remove(&mut self, slot: u32) {
        let (word, bit) = Self::word_and_bit(slot);
        self.words[word] &= !bit;
    }
}

/// The slots added to a [`Walk`]'s sets since they were last empty.
struct Touched {
    slots: Vec<u32>,
    /// The most slots `slots` keeps: as many as a set has words, past which
    /// emptying the sets whole costs no more than removing slot by slot.
    limit: usize,
    overflowed: bool,
}

impl Touched {
    fn record(&mut self, slot: u32) {
        match self.slots.len() < self.limit {
            true => self.slots.push(slot),
            false => self.overflowed = true,
        }
    }

    /// Empties `sets` of every slot recorded.
    fn empty(&mut self, sets: [&mut SlotSet; 2]) {
        for set in sets {
            match self.overflowed {
                true => set.words.fill(0),
                false => self.slots.iter().for_each(|&slot| set.remove(slot)),
            }
        }
        self.slots.clear();
        self.overflowed = false;
    }
}

/// A walk over the entries a spawn's tool and input sets reach, through every
/// set they hold, that reaches each entry once per set.
struct Walk {
    tools: SlotSet,
    inputs: SlotSet,
    stepper: Stepper,
}

impl Walk {
    fn new(entries: &InputGraph) -> Walk {
        let tools = SlotSet::new(entries.len());
        let limit = tools.words.len();
        Walk {
            inputs: SlotSet::new(entries.len()),
            tools,
            stepper: Stepper {
                touched: Touched {
                    slots: Vec::with_capacity(limit),
                    limit,
                    overflowed: false,
                },
                stack: Vec::with_capacity(entries.widest_set),
            },
        }
    }

    /// The content of the entries `spawn`'s input set reaches, those its tool
    /// set reaches too counted as tools.
    fn input_bytes(&mut self, entries: &InputGraph, spawn: SpawnInputs) -> InputBytes {
        let stepper = &mut self.stepper;
        stepper.walk(entries, spawn.tools, &mut self.tools, |_| {});
        let tools = &self.tools;
        let mut read = InputBytes::default();
        stepper.walk(entries, spawn.inputs, &mut self.inputs, |slot| {
            let bytes = entries.bytes[slot as usize];
            match tools.contains(slot) {
                true => read.tools += bytes,
                false => read.others += bytes,
            }
        });
        stepper.touched.empty([&mut self.tools, &mut self.inputs]);
        read
    }
}

/// What a [`Walk`] reuses from one walk to the next.
struct Stepper {
    touched: Touched,
    stack: Vec<u32>,
}

impl Stepper {
    /// Adds to `reached` each entry `root` reaches that it lacks, calling
    /// `reach` on each.
    fn walk(
        &mut self,
        entries: &InputGraph,
        root: Option<u32>,
        reached: &mut SlotSet,
        mut reach: impl FnMut(u32),
    ) {
        let Stepper { touched, stack } = self;
        let mut enter = |slot: u32, stack: &mut Vec<u32>| {
            if reached.insert(slot) {
                touched.record(slot);
                stack.push(slot);
            }
        };
        if let Some(root) = root {
            enter(root, stack);
        }
        while let Some(slot) = stack.pop() {
            reach(slot);
            for &held in entries.held_by(slot) {
                enter(held, stack);
            }
        }
    }
}

struct Builder {
    entries: Entries,
    spawns: spawn::Builder,
    /// Each spawn's, pushed with it.
    roots: Vec<SpawnInputs>,
}

impl Builder {
    fn with_capacity(compressed: u64) -> Builder {
        let path_bytes = compressed.saturating_mul(PATH_BYTES_PER_COMPRESSED_KIB) >> 10;
        let output_bytes = compressed.saturating_mul(OUTPUT_BYTES_PER_COMPRESSED_KIB) >> 10;
        let held = compressed.saturating_mul(HELD_PER_COMPRESSED_KIB) >> 10;
        Builder {
            entries: Entries::with_capacity(
                estimate(compressed, COMPRESSED_BYTES_PER_ENTRY),
                usize::try_from(path_bytes).unwrap_or(usize::MAX),
                usize::try_from(held).unwrap_or(usize::MAX),
            ),
            roots: Vec::with_capacity(estimate(compressed, COMPRESSED_BYTES_PER_SPAWN)),
            spawns: spawn::Builder::with_capacity(
                estimate(compressed, COMPRESSED_BYTES_PER_SPAWN),
                estimate(compressed, COMPRESSED_BYTES_PER_OUTPUT),
                usize::try_from(output_bytes).unwrap_or(usize::MAX),
            ),
        }
    }

    fn add(&mut self, entry: &[u8]) -> Result<(), DecodeError> {
        let Entry { id, payload } = split_entry(entry)?;
        let Some(Payload { kind, bytes }) = payload else {
            return Ok(());
        };
        if kind == exec_log_entry::SPAWN {
            return self.add_spawn(bytes);
        }
        // Spawns and sets refer to entries by id, and id 0 names none.
        let Some(id) = NonZeroU32::new(id) else {
            return Ok(());
        };
        match kind {
            exec_log_entry::FILE => self.add_file(id, bytes),
            exec_log_entry::DIRECTORY => self.add_directory(id, bytes),
            exec_log_entry::UNRESOLVED_SYMLINK => self.add_unresolved_symlink(id, bytes),
            exec_log_entry::INPUT_SET => self.add_input_set(id, bytes),
            exec_log_entry::SYMLINK_ENTRY_SET => self.add_symlink_entry_set(id, bytes),
            exec_log_entry::RUNFILES_TREE => self.add_runfiles_tree(id, bytes),
            _ => Ok(()),
        }
    }

    fn add_file(&mut self, id: NonZeroU32, mut file: &[u8]) -> Result<(), DecodeError> {
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
        self.entries.insert_path(id, path, bytes)
    }

    fn add_directory(&mut self, id: NonZeroU32, mut directory: &[u8]) -> Result<(), DecodeError> {
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
        self.entries.insert_path(id, path, bytes)
    }

    fn add_unresolved_symlink(
        &mut self,
        id: NonZeroU32,
        mut symlink: &[u8],
    ) -> Result<(), DecodeError> {
        let mut path = "";
        while !symlink.is_empty() {
            if let (exec_log_entry::unresolved_symlink::PATH, Value::Bytes(value)) =
                next_field(&mut symlink)?
            {
                path = text(value)?;
            }
        }
        self.entries.insert_path(id, path, Bytes::ZERO)
    }

    fn add_input_set(&mut self, id: NonZeroU32, mut set: &[u8]) -> Result<(), DecodeError> {
        while !set.is_empty() {
            if let (
                exec_log_entry::input_set::INPUT_IDS
                | exec_log_entry::input_set::TRANSITIVE_SET_IDS,
                ids,
            ) = next_field(&mut set)?
            {
                self.hold_ids(ids)?;
            }
        }
        self.entries.insert_set(id, Bytes::ZERO)
    }

    fn add_symlink_entry_set(&mut self, id: NonZeroU32, mut set: &[u8]) -> Result<(), DecodeError> {
        while !set.is_empty() {
            match next_field(&mut set)? {
                (exec_log_entry::symlink_entry_set::DIRECT_ENTRIES, Value::Bytes(mut symlink)) => {
                    while !symlink.is_empty() {
                        if let (
                            exec_log_entry::symlink_entry_set::direct_entries_entry::VALUE,
                            Value::Varint(target),
                        ) = next_field(&mut symlink)?
                        {
                            self.entries.hold(entry_id(target)?);
                        }
                    }
                }
                (exec_log_entry::symlink_entry_set::TRANSITIVE_SET_IDS, ids) => {
                    self.hold_ids(ids)?
                }
                _ => {}
            }
        }
        self.entries.insert_set(id, Bytes::ZERO)
    }

    /// Adds a runfiles tree as a set holding its artifacts and symlink
    /// targets, its own content its repo mapping manifest's: its empty files
    /// add none.
    fn add_runfiles_tree(&mut self, id: NonZeroU32, mut tree: &[u8]) -> Result<(), DecodeError> {
        let mut bytes = Bytes::ZERO;
        while !tree.is_empty() {
            match next_field(&mut tree)? {
                (
                    exec_log_entry::runfiles_tree::INPUT_SET_ID
                    | exec_log_entry::runfiles_tree::SYMLINKS_ID
                    | exec_log_entry::runfiles_tree::ROOT_SYMLINKS_ID,
                    Value::Varint(held),
                ) => self.entries.hold(entry_id(held)?),
                (exec_log_entry::runfiles_tree::REPO_MAPPING_MANIFEST, Value::Bytes(file)) => {
                    bytes = file_bytes(file)?
                }
                _ => {}
            }
        }
        self.entries.insert_set(id, bytes)
    }

    /// Holds the ids of a repeated `uint32` field, packed or not, in the
    /// entry added next.
    fn hold_ids(&mut self, ids: Value<'_>) -> Result<(), DecodeError> {
        match ids {
            Value::Varint(id) => self.entries.hold(entry_id(id)?),
            Value::Bytes(mut packed) => {
                while !packed.is_empty() {
                    self.entries.hold(entry_id(varint(&mut packed)?)?);
                }
            }
            Value::Fixed => {}
        }
        Ok(())
    }

    fn add_spawn(&mut self, mut spawn: &[u8]) -> Result<(), DecodeError> {
        let mut inputs = None;
        let mut tools = None;
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
                (exec_log_entry::spawn::INPUT_SET_ID, Value::Varint(id)) => {
                    inputs = self.entries.slot(entry_id(id)?)
                }
                (exec_log_entry::spawn::TOOL_SET_ID, Value::Varint(id)) => {
                    tools = self.entries.slot(entry_id(id)?)
                }
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
        self.roots.push(SpawnInputs { inputs, tools });
        Ok(())
    }

    fn finish(self) -> ExecLog {
        let inputs = InputGraph::new(self.entries, self.roots).measure();
        ExecLog {
            spawns: self.spawns.finish(inputs),
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
    use bazel_spawn_proto::tools::protos::ExecLogEntry;
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

    fn reading(inputs: u32, tools: u32) -> ExecLogEntry {
        let mut reads = spawn("remote", "M", metrics(1, &[]));
        reads.input_set_id = inputs;
        reads.tool_set_id = tools;
        entry(0, Type::Spawn(reads))
    }

    fn read(log: &ExecLog) -> Vec<(u64, u64)> {
        log.spawns()
            .map(|s| {
                let read = s.input_bytes();
                (read.tools.get(), read.others.get())
            })
            .collect()
    }

    #[test]
    fn a_spawn_read_each_input_once_its_tools_apart() {
        let log = load(&[
            sized_file(1, "a.h", 10),
            sized_file(2, "b.h", 20),
            sized_file(3, "tool", 100),
            file(4, "empty"),
            unresolved_symlink(5, "link"),
            directory(6, "tree", vec![sized("x", 3), sized("y", 4)]),
            input_set(10, &[1, 2], &[]),
            input_set(11, &[2, 6], &[10]),
            input_set(12, &[1], &[10, 11]),
            input_set(13, &[3], &[]),
            input_set(14, &[4, 5, 3, 99], &[12, 13]),
            reading(14, 13),
            reading(12, 0),
            reading(0, 0),
            reading(13, 13),
        ]);
        assert_eq!(read(&log), [(100, 37), (0, 37), (0, 0), (100, 0)]);
    }

    /// Files enough that a walk's sets span several words.
    const FILES_ACROSS_WORDS: u32 = 4 * SLOTS_PER_WORD as u32;

    #[test]
    fn each_spawn_reads_afresh_after_walks_wide_and_narrow() {
        let mut log: Vec<_> = (1..=FILES_ACROSS_WORDS)
            .map(|id| sized_file(id, &format!("f{id}"), 1))
            .collect();
        let all: Vec<u32> = (1..=FILES_ACROSS_WORDS).collect();
        let wide = FILES_ACROSS_WORDS + 1;
        let narrow = FILES_ACROSS_WORDS + 2;
        log.extend([
            input_set(wide, &all, &[]),
            input_set(narrow, &[1, 2], &[]),
            reading(narrow, 0),
            reading(wide, narrow),
            reading(narrow, narrow),
            reading(wide, 0),
        ]);
        let every = u64::from(FILES_ACROSS_WORDS);
        assert_eq!(
            read(&load(&log)),
            [(0, 2), (2, every - 2), (2, 0), (0, every)]
        );
    }

    #[test]
    fn a_runfiles_tree_reads_its_artifacts_symlink_targets_and_repo_mapping() {
        let log = load(&[
            sized_file(1, "bin/a", 10),
            sized_file(2, "bin/b", 20),
            sized_file(3, "bin/tool", 100),
            sized_file(4, "bin/c", 50),
            input_set(10, &[1, 3], &[]),
            entry(
                11,
                Type::SymlinkEntrySet(exec_log_entry::SymlinkEntrySet {
                    direct_entries: [("x".into(), 2), ("y".into(), 4), ("z".into(), 0)].into(),
                    transitive_set_ids: vec![],
                }),
            ),
            entry(
                12,
                Type::SymlinkEntrySet(exec_log_entry::SymlinkEntrySet {
                    direct_entries: [("w".into(), 1)].into(),
                    transitive_set_ids: vec![11],
                }),
            ),
            entry(
                13,
                Type::RunfilesTree(exec_log_entry::RunfilesTree {
                    path: "bin/tool.runfiles".into(),
                    input_set_id: 10,
                    symlinks_id: 11,
                    root_symlinks_id: 12,
                    empty_files: vec!["__init__.py".into()],
                    repo_mapping_manifest: Some(sized("", 9)),
                }),
            ),
            input_set(14, &[13], &[]),
            input_set(15, &[2, 13], &[14]),
            reading(15, 14),
        ]);
        assert_eq!(read(&log), [(189, 0)]);
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
    fn an_entry_without_an_id_is_none_to_refer_to() {
        let mut writes = spawn("remote", "M", metrics(1, &[]));
        writes.outputs = vec![output_id(0), output_id(1)];
        let log = load(&[
            sized_file(0, "unnamed", 5),
            sized_file(1, "", 7),
            input_set(0, &[1], &[]),
            input_set(2, &[], &[]),
            entry(0, Type::Spawn(writes)),
            reading(2, 0),
        ]);
        let outputs: Vec<Vec<&str>> = log.spawns().map(|s| s.outputs().collect()).collect();
        assert_eq!(outputs, [vec![""], vec![]]);
        assert_eq!(read(&log), [(0, 0), (0, 0)]);
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
        let entries = Entries::with_capacity(0, 0, 0);
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
