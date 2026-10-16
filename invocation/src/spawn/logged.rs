use std::collections::HashMap;
use std::ops::Range;

use super::{Bytes, InputBytes, PhaseTiming};

/// The runner name Bazel logs for a spawn sent to a remote executor.
const REMOTE_RUNNER: &str = "remote";
/// The runner name Bazel logs for a spawn its local disk cache answered.
const DISK_CACHE_RUNNER: &str = "disk cache hit";
/// The runner name Bazel logs for a spawn that reused the outputs of an
/// identical spawn running in the same build.
const DEDUPLICATED_RUNNER: &str = "deduplicated";
/// What the names of Bazel's sandboxed runners hold, as `linux-sandbox` and
/// `processwrapper-sandbox` do.
const SANDBOX_RUNNER_PART: &str = "sandbox";
/// Bazel's runner of spawns in a Docker container, which sandboxes them.
const DOCKER_RUNNER: &str = "docker";
/// What the names of Bazel's persistent-worker runners hold.
const WORKER_RUNNER_PART: &str = "worker";

/// What answered a spawn an execution log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpawnRunner {
    RemoteExecution,
    Worker,
    Sandboxed,
    /// A local subprocess outside any sandbox.
    Local,
    RemoteCacheHit,
    DiskCacheHit,
    /// The outputs of an identical spawn running in the same build, which
    /// Bazel waited for and reused rather than run this one.
    Deduplicated,
}

impl SpawnRunner {
    pub const ALL: [SpawnRunner; 7] = [
        SpawnRunner::RemoteExecution,
        SpawnRunner::Worker,
        SpawnRunner::Sandboxed,
        SpawnRunner::Local,
        SpawnRunner::RemoteCacheHit,
        SpawnRunner::DiskCacheHit,
        SpawnRunner::Deduplicated,
    ];

    /// The runner's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Whether the spawn's command ran, rather than a result being reused.
    pub fn executed(self) -> bool {
        matches!(
            self,
            SpawnRunner::RemoteExecution
                | SpawnRunner::Worker
                | SpawnRunner::Sandboxed
                | SpawnRunner::Local
        )
    }
}

/// The spawns an execution log records of an invocation.
#[derive(Default)]
pub struct LoggedSpawns {
    /// Distinct runner names, which [`Record::runner`] indexes.
    runners: Vec<Box<str>>,
    /// Distinct mnemonics, which [`Record::mnemonic`] indexes.
    mnemonics: Vec<Box<str>>,
    /// Distinct target labels, which [`Record::target`] indexes.
    targets: Vec<Box<str>>,
    records: Vec<Record>,
    /// Every spawn's outputs, each spawn's a run [`Record::outputs`] names.
    outputs: Paths,
    inputs: MeasuredInputs,
}

/// The bytes each spawn of a [`LoggedSpawns`] read, in the order it lists them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MeasuredInputs {
    per_spawn: Box<[InputBytes]>,
}

impl MeasuredInputs {
    pub fn new(per_spawn: Vec<InputBytes>) -> Self {
        Self {
            per_spawn: per_spawn.into_boxed_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.per_spawn.len()
    }

    pub fn is_empty(&self) -> bool {
        self.per_spawn.is_empty()
    }
}

struct Record {
    runner: usize,
    mnemonic: usize,
    /// `None` for an action no target owns.
    target: Option<usize>,
    outputs: Range<usize>,
    output_bytes: Bytes,
    cache_hit: bool,
    timing: PhaseTiming,
}

/// Exec paths end to end, each named by its place in the order added.
#[derive(Default)]
struct Paths {
    text: String,
    /// Where each path ends in `text`, the next starting there.
    ends: Vec<usize>,
}

impl Paths {
    fn with_capacity(paths: usize, text_bytes: usize) -> Paths {
        Paths {
            text: String::with_capacity(text_bytes),
            ends: Vec::with_capacity(paths),
        }
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn push(&mut self, path: &str) {
        self.text.push_str(path);
        self.ends.push(self.text.len());
    }

    /// Keeps the first `paths` paths.
    fn truncate(&mut self, paths: usize) {
        self.ends.truncate(paths);
        self.text.truncate(self.ends.last().copied().unwrap_or(0));
    }

    fn get(&self, index: usize) -> &str {
        let start = index.checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.text[start..self.ends[index]]
    }
}

impl LoggedSpawns {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Distinct runner names, each at the place [`LoggedSpawn::runner_index`]
    /// names.
    pub fn runners(&self) -> &[Box<str>] {
        &self.runners
    }

    /// Distinct mnemonics, each at the place [`LoggedSpawn::mnemonic_index`]
    /// names.
    pub fn mnemonics(&self) -> &[Box<str>] {
        &self.mnemonics
    }

    /// Distinct labels of the targets owning the spawns' actions.
    pub fn targets(&self) -> &[Box<str>] {
        &self.targets
    }

    /// The spawns in the order the log lists them.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = LoggedSpawn<'_>> {
        (self.records.iter().zip(&self.inputs.per_spawn)).map(|(record, &inputs)| LoggedSpawn {
            log: self,
            record,
            inputs,
        })
    }
}

/// One spawn of [`LoggedSpawns`].
#[derive(Clone, Copy)]
pub struct LoggedSpawn<'a> {
    log: &'a LoggedSpawns,
    record: &'a Record,
    inputs: InputBytes,
}

impl<'a> LoggedSpawn<'a> {
    /// What answered the spawn: a strategy that executed it, such as `remote`
    /// or `linux-sandbox`, or a cache, such as `disk cache hit`.
    pub fn runner(&self) -> &'a str {
        &self.log.runners[self.record.runner]
    }

    /// What answered the spawn, from its [runner](Self::runner) and whether a
    /// cache answered it. Bazel names a cache's answer after the cache, and
    /// a runner it names otherwise than remote, sandboxed or worker execution
    /// ran a local subprocess.
    pub fn ran_on(&self) -> SpawnRunner {
        let name = self.runner();
        if name == DEDUPLICATED_RUNNER {
            return SpawnRunner::Deduplicated;
        }
        if self.cache_hit() {
            return match name {
                DISK_CACHE_RUNNER => SpawnRunner::DiskCacheHit,
                _ => SpawnRunner::RemoteCacheHit,
            };
        }
        match name {
            REMOTE_RUNNER => SpawnRunner::RemoteExecution,
            DOCKER_RUNNER => SpawnRunner::Sandboxed,
            _ if name.contains(SANDBOX_RUNNER_PART) => SpawnRunner::Sandboxed,
            _ if name.contains(WORKER_RUNNER_PART) => SpawnRunner::Worker,
            _ => SpawnRunner::Local,
        }
    }

    pub fn runner_index(&self) -> usize {
        self.record.runner
    }

    pub fn mnemonic(&self) -> &'a str {
        &self.log.mnemonics[self.record.mnemonic]
    }

    pub fn mnemonic_index(&self) -> usize {
        self.record.mnemonic
    }

    /// The label of the target owning the spawn's action, `None` for an
    /// action no target owns.
    pub fn target_label(&self) -> Option<&'a str> {
        Some(&self.log.targets[self.record.target?])
    }

    /// The exec paths of the spawn's outputs the log names, in the order it
    /// declares them.
    pub fn outputs(&self) -> impl ExactSizeIterator<Item = &'a str> + use<'a> {
        let outputs = &self.log.outputs;
        self.record.outputs.clone().map(|index| outputs.get(index))
    }

    /// The content of the spawn's [`outputs`](Self::outputs) the log names
    /// as written, a tree artifact's being that of the files in it.
    pub fn output_bytes(&self) -> Bytes {
        self.record.output_bytes
    }

    pub fn input_bytes(&self) -> InputBytes {
        self.inputs
    }

    /// The exec path of the first of [`outputs`](Self::outputs).
    pub fn primary_output(&self) -> Option<&'a str> {
        self.outputs().next()
    }

    /// Whether a cache answered the spawn rather than an execution of it.
    pub fn cache_hit(&self) -> bool {
        self.record.cache_hit
    }

    pub fn timing(&self) -> &'a PhaseTiming {
        &self.record.timing
    }
}

/// One spawn as a reader of a log finds it, for [`Builder::push`].
pub struct Logged<'a> {
    pub runner: &'a str,
    pub mnemonic: &'a str,
    /// Empty where no target owns the spawn's action.
    pub target_label: &'a str,
    pub cache_hit: bool,
    pub timing: PhaseTiming,
}

/// [`LoggedSpawns`] gathered one spawn at a time.
pub struct Builder {
    runners: Names,
    mnemonics: Names,
    targets: Names,
    records: Vec<Record>,
    outputs: Paths,
    /// Where the outputs of the spawn pushed next start in `outputs`.
    next_outputs: usize,
    /// The content of the outputs added since the spawn pushed last.
    next_output_bytes: Bytes,
}

impl Builder {
    /// Room for `spawns` spawns with `outputs` outputs between them, their
    /// paths `output_bytes` long together.
    pub fn with_capacity(spawns: usize, outputs: usize, output_bytes: usize) -> Builder {
        Builder {
            runners: Names::default(),
            mnemonics: Names::default(),
            targets: Names::default(),
            records: Vec::with_capacity(spawns),
            outputs: Paths::with_capacity(outputs, output_bytes),
            next_outputs: 0,
            next_output_bytes: Bytes::ZERO,
        }
    }

    /// Adds an output of the spawn [`push`](Self::push) adds next, with
    /// `bytes` of content.
    pub fn output(&mut self, path: &str, bytes: Bytes) {
        self.outputs.push(path);
        self.next_output_bytes += bytes;
    }

    /// Adds `spawn`, with the outputs added since the spawn before it.
    pub fn push(&mut self, spawn: Logged<'_>) {
        let outputs = self.next_outputs..self.outputs.len();
        self.next_outputs = outputs.end;
        let record = Record {
            runner: self.runners.intern(spawn.runner),
            mnemonic: self.mnemonics.intern(spawn.mnemonic),
            target: (!spawn.target_label.is_empty())
                .then(|| self.targets.intern(spawn.target_label)),
            outputs,
            output_bytes: std::mem::take(&mut self.next_output_bytes),
            cache_hit: spawn.cache_hit,
            timing: spawn.timing,
        };
        self.records.push(record);
    }

    /// The spawns pushed, having read what `inputs` measures of them; outputs
    /// added after the last spawn are dropped.
    ///
    /// # Panics
    ///
    /// Where `inputs` measures another number of spawns.
    pub fn finish(mut self, inputs: MeasuredInputs) -> LoggedSpawns {
        assert_eq!(inputs.len(), self.records.len(), "one measure per spawn");
        self.outputs.truncate(self.next_outputs);
        LoggedSpawns {
            runners: self.runners.into_vec(),
            mnemonics: self.mnemonics.into_vec(),
            targets: self.targets.into_vec(),
            records: self.records,
            outputs: self.outputs,
            inputs,
        }
    }
}

/// Distinct strings, each named by its place in the order first seen.
#[derive(Default)]
struct Names {
    index: HashMap<Box<str>, usize>,
}

impl Names {
    fn intern(&mut self, name: &str) -> usize {
        let next = self.index.len();
        match self.index.get(name) {
            Some(&index) => index,
            None => {
                self.index.insert(name.into(), next);
                next
            }
        }
    }

    fn into_vec(self) -> Vec<Box<str>> {
        let mut names = vec![Box::<str>::default(); self.index.len()];
        for (name, index) in self.index {
            names[index] = name;
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::REPORTED_PHASES;
    use std::time::Duration;

    fn logged<'a>(runner: &'a str, mnemonic: &'a str) -> Logged<'a> {
        Logged {
            runner,
            mnemonic,
            target_label: "",
            cache_hit: false,
            timing: PhaseTiming::new(Duration::ZERO, [Duration::ZERO; REPORTED_PHASES]),
        }
    }

    /// The spawns `builder` holds, reading no input.
    fn finish(builder: Builder) -> LoggedSpawns {
        let read = vec![InputBytes::default(); builder.records.len()];
        builder.finish(MeasuredInputs::new(read))
    }

    #[test]
    fn each_spawn_reads_what_was_measured_of_it() {
        let mut builder = Builder::with_capacity(2, 0, 0);
        builder.push(logged("remote", "A"));
        builder.push(logged("remote", "B"));
        let read = |tools, others| InputBytes {
            tools: Bytes::new(tools),
            others: Bytes::new(others),
        };
        let log = builder.finish(MeasuredInputs::new(vec![read(1, 2), read(0, 5)]));
        let all: Vec<u64> = log.iter().map(|s| s.input_bytes().all().get()).collect();
        assert_eq!(all, [3, 5]);
        assert_eq!(log.iter().next().unwrap().input_bytes(), read(1, 2));
    }

    #[test]
    #[should_panic(expected = "one measure per spawn")]
    fn a_measure_of_other_spawns_is_refused() {
        let mut builder = Builder::with_capacity(1, 0, 0);
        builder.push(logged("remote", "A"));
        builder.finish(MeasuredInputs::default());
    }

    #[test]
    fn names_are_shared_between_spawns() {
        let mut builder = Builder::with_capacity(3, 0, 0);
        builder.push(logged("remote", "A"));
        builder.push(logged("local", "B"));
        builder.push(logged("remote", "B"));
        let log = finish(builder);
        assert_eq!(*log.runners(), ["remote".into(), "local".into()]);
        assert_eq!(*log.mnemonics(), ["A".into(), "B".into()]);
        let runners: Vec<&str> = log.iter().map(|spawn| spawn.runner()).collect();
        assert_eq!(runners, ["remote", "local", "remote"]);
    }

    #[test]
    fn an_empty_target_label_names_no_target() {
        let mut builder = Builder::with_capacity(3, 0, 0);
        builder.push(logged("remote", "A"));
        for mnemonic in ["A", "B"] {
            builder.push(Logged {
                target_label: "//a",
                ..logged("remote", mnemonic)
            });
        }
        let log = finish(builder);
        let labels: Vec<_> = log.iter().map(|s| s.target_label()).collect();
        assert_eq!(labels, [None, Some("//a"), Some("//a")]);
        assert_eq!(*log.targets(), ["//a".into()]);
    }

    #[test]
    fn each_spawn_keeps_the_outputs_added_before_it() {
        let mut builder = Builder::with_capacity(3, 3, 0);
        builder.output("bazel-out/a.o", Bytes::new(100));
        builder.output("bazel-out/a.d", Bytes::new(20));
        builder.push(logged("remote", "A"));
        builder.push(logged("remote", "B"));
        builder.output("bazel-out/c", Bytes::new(3));
        builder.push(Logged {
            cache_hit: true,
            ..logged("disk cache hit", "C")
        });
        builder.output("bazel-out/dropped", Bytes::new(1));
        let log = finish(builder);
        let outputs: Vec<Vec<&str>> = log.iter().map(|s| s.outputs().collect()).collect();
        assert_eq!(
            outputs,
            [
                vec!["bazel-out/a.o", "bazel-out/a.d"],
                vec![],
                vec!["bazel-out/c"]
            ]
        );
        let primary: Vec<_> = log.iter().map(|s| s.primary_output()).collect();
        assert_eq!(primary, [Some("bazel-out/a.o"), None, Some("bazel-out/c")]);
        let written: Vec<u64> = log.iter().map(|s| s.output_bytes().get()).collect();
        assert_eq!(written, [120, 0, 3]);
        let hits: Vec<bool> = log.iter().map(|s| s.cache_hit()).collect();
        assert_eq!(hits, [false, false, true]);
    }

    #[test]
    fn a_spawn_ran_on_what_its_runner_and_cache_hit_name() {
        // The runner names and cache-hit flags Bazel logs.
        let ran = [
            ("remote", false, SpawnRunner::RemoteExecution),
            ("remote cache hit", true, SpawnRunner::RemoteCacheHit),
            ("disk cache hit", true, SpawnRunner::DiskCacheHit),
            ("deduplicated", false, SpawnRunner::Deduplicated),
            ("linux-sandbox", false, SpawnRunner::Sandboxed),
            ("processwrapper-sandbox", false, SpawnRunner::Sandboxed),
            ("docker", false, SpawnRunner::Sandboxed),
            ("worker", false, SpawnRunner::Worker),
            ("multiplex-worker", false, SpawnRunner::Worker),
            ("local", false, SpawnRunner::Local),
        ];
        let mut builder = Builder::with_capacity(ran.len(), 0, 0);
        for (runner, cache_hit, _) in ran {
            builder.push(Logged {
                cache_hit,
                ..logged(runner, "M")
            });
        }
        let log = finish(builder);
        let on: Vec<SpawnRunner> = log.iter().map(|spawn| spawn.ran_on()).collect();
        let expected: Vec<SpawnRunner> = ran.iter().map(|&(_, _, runner)| runner).collect();
        assert_eq!(on, expected);
    }

    #[test]
    fn all_lists_every_spawn_runner_at_its_index() {
        for (i, runner) in SpawnRunner::ALL.into_iter().enumerate() {
            assert_eq!(runner.index(), i);
        }
    }
}
