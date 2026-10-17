//! Reader for Bazel's compact execution log (`--execution_log_compact_file`):
//! the spawns an invocation executed or answered from a remote or disk cache,
//! with the time each spent in every phase Bazel's spawn metrics report.

use std::cmp::Reverse;
use std::fmt;
use std::path::Path;
use std::time::Duration;

mod frame;
mod read;
mod spawn_fields;
#[cfg(test)]
mod test_log;
mod wire;

use bviz_input::Input;
use bviz_invocation::Recorded;
use bviz_invocation::spawn::{LoggedSpawn, LoggedSpawns};
pub use bviz_invocation::spawn::{Phase, PhaseTiming};
pub use read::probe_invocation_id;

#[derive(Debug)]
pub enum LoadError {
    /// Reading or decompressing the file failed: the decoder reports both
    /// through one error.
    Io(std::io::Error),
    Decode(prost::DecodeError),
    /// The file decodes to no entry: an execution log opens with one.
    NotALog,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Decode(e) => write!(f, "malformed log entry: {e}"),
            LoadError::NotALog => write!(f, "not an execution log"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Io(e) => Some(e),
            LoadError::Decode(e) => Some(e),
            LoadError::NotALog => None,
        }
    }
}

impl From<prost::DecodeError> for LoadError {
    fn from(e: prost::DecodeError) -> Self {
        LoadError::Decode(e)
    }
}

/// The spawns of one execution log.
pub struct ExecLog {
    spawns: LoggedSpawns,
}

impl ExecLog {
    /// The log at `path`, as far as it goes where it is cut off.
    pub fn from_path(path: &Path) -> Result<Recorded<ExecLog>, LoadError> {
        ExecLog::from_input(Input::open(path).map_err(LoadError::Io)?)
    }

    /// The log `input` holds, as far as it goes where it is cut off.
    pub fn from_input(input: Input) -> Result<Recorded<ExecLog>, LoadError> {
        read::read_input(input)
    }

    /// The log's spawns, for readers that join them to what other outputs of
    /// the invocation record.
    pub fn into_spawns(self) -> LoggedSpawns {
        self.spawns
    }

    pub fn spawn_count(&self) -> usize {
        self.spawns.len()
    }

    /// The labels of the targets the log's spawns belong to, one per spawn
    /// naming one.
    pub fn target_labels(&self) -> impl Iterator<Item = &str> {
        self.spawns().filter_map(|spawn| spawn.target_label())
    }

    /// Every spawn's time summed: spawns overlap, so this is no wall time.
    pub fn timing(&self) -> PhaseTiming {
        let mut sum = PhaseTiming::default();
        for spawn in self.spawns() {
            sum.add(spawn.timing());
        }
        sum
    }

    fn spawns(&self) -> impl ExactSizeIterator<Item = LoggedSpawn<'_>> {
        self.spawns.iter()
    }

    /// The spawns of each runner, in descending order of summed total time.
    pub fn by_runner(&self) -> Vec<Group<'_>> {
        self.group(self.spawns.runners(), |spawn| Some(spawn.runner_index()))
    }

    /// The spawns of each mnemonic among `runner`'s spawns, in descending
    /// order of summed total time.
    pub fn by_mnemonic(&self, runner: &str) -> Vec<Group<'_>> {
        let Some(runner) = self.spawns.runners().iter().position(|r| **r == *runner) else {
            return Vec::new();
        };
        self.group(self.spawns.mnemonics(), |spawn| {
            (spawn.runner_index() == runner).then_some(spawn.mnemonic_index())
        })
    }

    /// Sums the spawns `key` assigns a name in `names`, skipping those it
    /// assigns none.
    fn group<'a>(
        &'a self,
        names: &'a [Box<str>],
        key: impl Fn(&LoggedSpawn<'_>) -> Option<usize>,
    ) -> Vec<Group<'a>> {
        let mut groups: Vec<Group<'a>> = names
            .iter()
            .map(|name| Group {
                name,
                spawns: 0,
                timing: PhaseTiming::default(),
            })
            .collect();
        for spawn in self.spawns() {
            if let Some(index) = key(&spawn) {
                let group = &mut groups[index];
                group.spawns += 1;
                group.timing.add(spawn.timing());
            }
        }
        groups.retain(|group| group.spawns > 0);
        groups.sort_by_key(|group| Reverse(group.timing.total()));
        groups
    }

    /// The `n` spawns that spent longest in `phase`, longest first, leaving
    /// out those that spent no time in it. Equal times go in order of target
    /// label, then of primary output, so the ranking does not depend on the
    /// order the log lists its spawns in.
    pub fn top_spawns(&self, phase: Phase, n: usize) -> Vec<LoggedSpawn<'_>> {
        let mut spawns: Vec<LoggedSpawn<'_>> = self
            .spawns()
            .filter(|spawn| !spawn.timing().phase(phase).is_zero())
            .collect();
        fn key<'s>(
            spawn: &LoggedSpawn<'s>,
            phase: Phase,
        ) -> (Reverse<Duration>, Option<&'s str>, Option<&'s str>) {
            (
                Reverse(spawn.timing().phase(phase)),
                spawn.target_label(),
                spawn.primary_output(),
            )
        }
        let rank = |a: &LoggedSpawn<'_>, b: &LoggedSpawn<'_>| key(a, phase).cmp(&key(b, phase));
        if spawns.len() > n {
            spawns.select_nth_unstable_by(n, rank);
            spawns.truncate(n);
        }
        spawns.sort_unstable_by(rank);
        spawns
    }
}

/// Spawns sharing a runner or a mnemonic, summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group<'a> {
    pub name: &'a str,
    pub spawns: usize,
    pub timing: PhaseTiming,
}

#[cfg(test)]
mod tests {

    use bazel_spawn_proto::tools::protos::exec_log_entry::Type;

    use super::*;
    use crate::test_log::{entry, invalid_output, load, metrics, spawn};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn sample() -> ExecLog {
        let spawns = [
            (
                "remote",
                "CppCompile",
                40,
                &[(Phase::Queue, 10), (Phase::Execute, 25)][..],
            ),
            ("remote", "CppLink", 30, &[(Phase::Execute, 30)]),
            (
                "remote",
                "CppCompile",
                20,
                &[(Phase::Queue, 15), (Phase::Execute, 5)],
            ),
            ("linux-sandbox", "CppCompile", 50, &[(Phase::Execute, 50)]),
            ("disk cache hit", "CppCompile", 3, &[(Phase::Fetch, 2)]),
        ];
        let entries: Vec<_> = spawns
            .into_iter()
            .map(|(runner, mnemonic, total, phases)| {
                entry(
                    0,
                    Type::Spawn(spawn(runner, mnemonic, metrics(total, phases))),
                )
            })
            .collect();
        load(&entries)
    }

    fn summary(groups: &[Group<'_>]) -> Vec<(String, usize, Duration)> {
        groups
            .iter()
            .map(|g| (g.name.to_owned(), g.spawns, g.timing.total()))
            .collect()
    }

    #[test]
    fn runners_are_summed_longest_first() {
        let log = sample();
        let runners = log.by_runner();
        assert_eq!(
            summary(&runners),
            [
                ("remote".into(), 3, ms(90)),
                ("linux-sandbox".into(), 1, ms(50)),
                ("disk cache hit".into(), 1, ms(3)),
            ]
        );
        assert_eq!(runners[0].timing.phase(Phase::Queue), ms(25));
        assert_eq!(runners[0].timing.phase(Phase::Other), ms(5));
        let execution_only: Vec<bool> = runners
            .iter()
            .map(|g| g.timing.reports_execution_only())
            .collect();
        assert_eq!(execution_only, [false, true, false]);
        assert_eq!(log.timing().total(), ms(143));
        assert_eq!(log.spawn_count(), 5);
    }

    #[test]
    fn mnemonics_keep_to_the_runner_asked_for() {
        let log = sample();
        assert_eq!(
            summary(&log.by_mnemonic("remote")),
            [
                ("CppCompile".into(), 2, ms(60)),
                ("CppLink".into(), 1, ms(30))
            ]
        );
        assert!(log.by_mnemonic("worker").is_empty());
    }

    #[test]
    fn top_spawns_rank_by_the_phase_alone() {
        let log = sample();
        let queue: Vec<Duration> = log
            .top_spawns(Phase::Queue, 5)
            .iter()
            .map(|s| s.timing().phase(Phase::Queue))
            .collect();
        assert_eq!(queue, [ms(15), ms(10)]);
        let execute: Vec<&str> = log
            .top_spawns(Phase::Execute, 2)
            .iter()
            .map(|s| s.runner())
            .collect();
        assert_eq!(execute, ["linux-sandbox", "remote"]);
        assert!(log.top_spawns(Phase::Retry, 5).is_empty());
        assert!(log.top_spawns(Phase::Execute, 0).is_empty());
    }

    #[test]
    fn equal_times_rank_by_target_then_output() {
        let labelled = |label: &str, output: &str| {
            let mut spawn = spawn("remote", "M", metrics(10, &[(Phase::Execute, 10)]));
            spawn.target_label = label.into();
            spawn.outputs = vec![invalid_output(output)];
            entry(0, Type::Spawn(spawn))
        };
        let log = load(&[
            labelled("//b", "x"),
            labelled("//a", "y"),
            labelled("//a", "x"),
        ]);
        let ranked: Vec<_> = log
            .top_spawns(Phase::Execute, 3)
            .iter()
            .map(|s| (s.target_label(), s.primary_output()))
            .collect();
        assert_eq!(
            ranked,
            [
                (Some("//a"), Some("x")),
                (Some("//a"), Some("y")),
                (Some("//b"), Some("x")),
            ]
        );
    }
}
