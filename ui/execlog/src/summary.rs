//! What the spawn-time view shows of a log, derived once off the UI thread.

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
}

impl From<Group<'_>> for GroupRow {
    fn from(group: Group<'_>) -> Self {
        Self {
            name: group.name.to_owned().into(),
            spawns: group.spawns,
            timing: group.timing,
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
}

#[derive(Clone)]
pub(crate) struct Summary {
    pub(crate) spawns: usize,
    pub(crate) timing: PhaseTiming,
    /// Longest first.
    pub(crate) runners: Vec<Runner>,
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
            top: Phase::ALL.map(|phase| top_spawns(log, phase)),
            biggest: ByteMeasure::ALL.map(|measure| biggest_spawns(log, measure)),
            repos: Arc::new(RepoNames::of_labels(log.target_labels())),
        }
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
        })
        .collect()
}
