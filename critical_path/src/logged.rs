use crate::{Phase, SpawnRunner, Step, Work};
use bviz_invocation::spawn::{LoggedSpawn, LoggedSpawns, SpawnBytes};
use std::collections::HashMap;
use std::time::Duration;

/// The mnemonic of a test's action. Its primary output, the test's log, is
/// no output of a spawn: the spawns running the test and writing its XML
/// report list the files beside the log instead.
const TEST_MNEMONIC: &str = "TestRunner";

/// The spawns joined to one step, summed.
pub(crate) struct Tally {
    /// What answered the first spawn joined.
    first: SpawnRunner,
    /// How long the spawns held each runner, `None` for one none held.
    held: [Option<Duration>; SpawnRunner::ALL.len()],
    pub(crate) spawns: usize,
    pub(crate) logged: Duration,
    pub(crate) phases: [Duration; Phase::ALL.len()],
    pub(crate) bytes: SpawnBytes,
}

impl Tally {
    fn of(spawn: LoggedSpawn<'_>) -> Tally {
        let mut tally = Tally {
            first: spawn.ran_on(),
            held: [None; SpawnRunner::ALL.len()],
            spawns: 0,
            logged: Duration::ZERO,
            phases: [Duration::ZERO; Phase::ALL.len()],
            bytes: SpawnBytes::default(),
        };
        tally.add(spawn);
        tally
    }

    /// Counts `spawn` into `tally`, starting it where it holds none.
    fn count(tally: &mut Option<Tally>, spawn: LoggedSpawn<'_>) {
        match tally {
            Some(tally) => tally.add(spawn),
            None => *tally = Some(Tally::of(spawn)),
        }
    }

    fn add(&mut self, spawn: LoggedSpawn<'_>) {
        let timing = spawn.timing();
        let held = &mut self.held[spawn.ran_on().index()];
        *held = Some(held.unwrap_or_default().saturating_add(timing.total()));
        self.spawns += 1;
        self.logged = self.logged.saturating_add(timing.total());
        for (sum, phase) in self.phases.iter_mut().zip(Phase::ALL) {
            *sum = sum.saturating_add(timing.phase(phase));
        }
        self.bytes += SpawnBytes::of(&spawn);
    }

    /// What answered the step's action, by the rule [`crate::Spawned::runner`]
    /// states.
    pub(crate) fn runner(&self) -> SpawnRunner {
        let rank =
            |runner: SpawnRunner| self.held[runner.index()].map(|held| (runner.executed(), held));
        SpawnRunner::ALL
            .into_iter()
            .fold(self.first, |best, runner| {
                if rank(runner) > rank(best) {
                    runner
                } else {
                    best
                }
            })
    }
}

/// The spawns of `log` each of `steps` ran, by the step's place: those
/// listing the step's primary output among their outputs, or failing that,
/// for a test whose log is known, those of the test's target that wrote
/// beside its log. `None` for a step `log` records no spawn of.
pub(crate) fn join(steps: &[Step], log: &LoggedSpawns) -> Vec<Option<Tally>> {
    let mut tallies: Vec<Option<Tally>> = steps.iter().map(|_| None).collect();
    let mut by_output: HashMap<&str, Vec<usize>> = HashMap::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        if let Some(out) = ran_spawns(step).then(|| step.primary_output()).flatten() {
            by_output.entry(out).or_default().push(i);
        }
    }
    for spawn in log.iter() {
        for output in spawn.outputs() {
            for &i in by_output.get(output).into_iter().flatten() {
                Tally::count(&mut tallies[i], spawn);
            }
        }
    }
    let mut tests: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        if tallies[i].is_none()
            && ran_spawns(step)
            && step.mnemonic() == Some(TEST_MNEMONIC)
            && let Some(target) = step.target()
            && let Some(log) = step.primary_output()
        {
            tests.entry(target).or_default().push((i, log));
        }
    }
    if tests.is_empty() {
        return tallies;
    }
    for spawn in log.iter().filter(|spawn| spawn.mnemonic() == TEST_MNEMONIC) {
        let Some(target) = spawn.target_label() else {
            continue;
        };
        for &(i, log) in tests.get(target).into_iter().flatten() {
            if wrote_beside(spawn, log) {
                Tally::count(&mut tallies[i], spawn);
            }
        }
    }
    tallies
}

/// Whether `step` is an action that can have run spawns.
fn ran_spawns(step: &Step) -> bool {
    !matches!(step.work(), Work::ChangePruned)
}

/// Whether `spawn` wrote an output in the directory of `log`. A test's runs
/// and shards each keep their log in a directory of their own.
fn wrote_beside(spawn: LoggedSpawn<'_>, log: &str) -> bool {
    let directory = parent(log);
    spawn.outputs().any(|output| parent(output) == directory)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}
