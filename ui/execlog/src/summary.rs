//! What the spawn time and spawn bytes panels show of a log, derived once
//! off the UI thread.

use bviz_execlog::{ByteMeasure, ExecLog, Group, Phase, PhaseTiming, SpawnBytes};
use bviz_invocation::spawn::LoggedSpawn;
use bviz_label::names::RepoNames;
use gpui::SharedString;
use std::sync::Arc;
use std::time::Duration;

/// How the spawns of a log are summed for one row of a table.
#[derive(Clone)]
pub(crate) struct GroupRow {
    pub(crate) name: SharedString,
    pub(crate) spawns: usize,
    pub(crate) timing: PhaseTiming,
    pub(crate) bytes: SpawnBytes,
}

impl From<Group<'_>> for GroupRow {
    fn from(group: Group<'_>) -> Self {
        Self {
            name: group.name.to_owned().into(),
            spawns: group.spawns,
            timing: group.timing,
            bytes: group.bytes,
        }
    }
}

/// One runner's spawns, in sum and by mnemonic.
#[derive(Clone)]
pub(crate) struct Runner {
    pub(crate) sum: GroupRow,
    pub(crate) mnemonics: Vec<GroupRow>,
}

/// How many spawns a list of those largest in some measure holds.
pub(crate) const LISTED_SPAWNS: usize = 100;

/// What names a spawn in a list of spawns.
#[derive(Clone)]
pub(crate) struct SpawnName {
    pub(crate) target_label: Option<SharedString>,
    pub(crate) primary_output: Option<SharedString>,
    pub(crate) mnemonic: SharedString,
    pub(crate) runner: SharedString,
}

impl SpawnName {
    fn of(spawn: &LoggedSpawn<'_>) -> Self {
        let owned = |text: &str| SharedString::from(text.to_owned());
        Self {
            target_label: spawn.target_label().map(owned),
            primary_output: spawn.primary_output().map(owned),
            mnemonic: owned(spawn.mnemonic()),
            runner: owned(spawn.runner()),
        }
    }
}

/// One spawn of the list of those longest in a phase.
#[derive(Clone)]
pub(crate) struct TopSpawn {
    pub(crate) name: SpawnName,
    pub(crate) time: Duration,
}

/// One spawn of a list of those largest in a [`ByteMeasure`].
#[derive(Clone)]
pub(crate) struct BigSpawn {
    pub(crate) name: SpawnName,
    pub(crate) bytes: SpawnBytes,
    pub(crate) time: Duration,
}

#[derive(Clone)]
pub struct Summary {
    pub(crate) spawns: usize,
    pub(crate) timing: PhaseTiming,
    /// Longest first.
    pub(crate) runners: Vec<Runner>,
    /// Every runner's spawns by mnemonic.
    pub(crate) mnemonics: Vec<GroupRow>,
    /// The spawns longest in each phase, longest first, indexed as
    /// [`Phase::ALL`].
    pub(crate) top: [Vec<TopSpawn>; Phase::ALL.len()],
    /// The spawns largest in each measure, largest first, indexed as
    /// [`ByteMeasure::ALL`].
    pub(crate) biggest: [Vec<BigSpawn>; ByteMeasure::ALL.len()],
    /// The names the repositories the log's labels name are shown by.
    pub(crate) repos: Arc<RepoNames>,
}

impl Summary {
    pub(crate) fn of(log: &ExecLog) -> Self {
        let runners = log
            .by_runner()
            .into_iter()
            .map(|group| Runner {
                mnemonics: log
                    .by_mnemonic(group.name)
                    .into_iter()
                    .map(GroupRow::from)
                    .collect(),
                sum: GroupRow::from(group),
            })
            .collect();
        Self {
            spawns: log.spawn_count(),
            timing: log.timing(),
            runners,
            mnemonics: (log.by_mnemonic_of_every_runner().into_iter())
                .map(GroupRow::from)
                .collect(),
            top: Phase::ALL.map(|phase| top_spawns(log, phase)),
            biggest: ByteMeasure::ALL.map(|measure| biggest_spawns(log, measure)),
            repos: Arc::new(RepoNames::of_labels(log.target_labels())),
        }
    }

    /// What the log's spawns wrote and read in sum.
    pub(crate) fn bytes(&self) -> SpawnBytes {
        let mut sum = SpawnBytes::default();
        for runner in &self.runners {
            sum += runner.sum.bytes;
        }
        sum
    }

    /// The phase the log spent longest in, execution where none is longer.
    pub(crate) fn longest_phase(&self) -> Phase {
        Phase::ALL
            .into_iter()
            .fold(Phase::Execute, |longest, phase| {
                if self.timing.phase(phase) > self.timing.phase(longest) {
                    phase
                } else {
                    longest
                }
            })
    }
}

fn top_spawns(log: &ExecLog, phase: Phase) -> Vec<TopSpawn> {
    log.top_spawns(phase, LISTED_SPAWNS)
        .iter()
        .map(|spawn| TopSpawn {
            name: SpawnName::of(spawn),
            time: spawn.timing().phase(phase),
        })
        .collect()
}

fn biggest_spawns(log: &ExecLog, measure: ByteMeasure) -> Vec<BigSpawn> {
    log.biggest_spawns(measure, LISTED_SPAWNS)
        .iter()
        .map(|spawn| BigSpawn {
            name: SpawnName::of(spawn),
            bytes: SpawnBytes::of(spawn),
            time: spawn.timing().total(),
        })
        .collect()
}
