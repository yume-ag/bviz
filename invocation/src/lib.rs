//! The vocabulary shared by the readers of one Bazel invocation's outputs:
//! each output states the same facts in its own encoding, and what the readers
//! return must be comparable to be joined.

use std::collections::HashMap;
use std::sync::Arc;

mod actions;
mod attempts;
mod critical;
mod outcomes;
mod recorded;
mod runner;
pub mod spawn;
mod timings;

pub use actions::{ActionGraph, ActionIndex};
pub use attempts::{Attempt, Attempts, Done, Phase, State, Venue};
pub use critical::CriticalStep;
pub use outcomes::{
    BuildOutcome, Cause, ConfiguredTarget, FailedAction, Outcomes, TestOutcome, TestStatus,
};
pub use recorded::Recorded;
pub use runner::{Runner, Spawns};
pub use timings::{Profiled, Share, Spent, Timing, Timings};

use spawn::LoggedSpawns;

const MICROS_PER_MILLI: i64 = 1_000;
const MICROS_PER_SECOND: f64 = 1e6;

/// A wall-clock instant, in microseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpochMicros(i64);

impl EpochMicros {
    /// The instant `millis` milliseconds after the Unix epoch, saturating at
    /// the representable range.
    pub const fn from_millis(millis: i64) -> EpochMicros {
        EpochMicros(millis.saturating_mul(MICROS_PER_MILLI))
    }

    /// The instant `seconds` seconds after the Unix epoch, placed as
    /// [`plus_secs_f64`](Self::plus_secs_f64) places it.
    pub fn from_secs_f64(seconds: f64) -> EpochMicros {
        EpochMicros(0).plus_secs_f64(seconds)
    }

    pub const fn micros(self) -> i64 {
        self.0
    }

    /// The instant `micros` microseconds after this one (before it for a
    /// negative count), saturating at the representable range.
    pub const fn plus_micros(self, micros: i64) -> EpochMicros {
        EpochMicros(self.0.saturating_add(micros))
    }

    /// The instant `seconds` seconds after this one, to the nearest
    /// microsecond and saturating at the representable range; this one for a
    /// NaN.
    pub fn plus_secs_f64(self, seconds: f64) -> EpochMicros {
        self.plus_micros((seconds * MICROS_PER_SECOND).round() as i64)
    }
}

/// Bazel's id for one run of a command (its command id), which outputs of that
/// run name alike. Distinct from the build request id, which several
/// invocations can share.
#[derive(Debug, Clone, PartialEq)]
pub struct InvocationId(Arc<str>);

impl InvocationId {
    /// `None` for the empty string, which an output holds in place of an id it
    /// does not know.
    pub fn new(id: &str) -> Option<InvocationId> {
        (!id.is_empty()).then(|| InvocationId(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Leading characters of a configuration checksum a reader is shown: what
/// [`Configurations::name`] tells apart configurations sharing a mnemonic by.
const CHECKSUM_SHOWN: usize = 6;

/// The mnemonic of each configuration known of an invocation, keyed by the
/// checksum Bazel's outputs name a configuration by.
#[derive(Default)]
pub struct Configurations(HashMap<String, String>);

impl Configurations {
    pub fn with_capacity(configurations: usize) -> Self {
        Self(HashMap::with_capacity(configurations))
    }

    /// Describes the configuration `checksum` names by its `mnemonic`. A
    /// checksum names the same configuration in every output, so one already
    /// described keeps the description it has.
    pub fn insert(&mut self, checksum: String, mnemonic: String) {
        self.0.entry(checksum).or_insert(mnemonic);
    }

    /// [Describes](Self::insert) every configuration `described` holds.
    pub fn extend(&mut self, described: Configurations) {
        self.0.reserve(described.0.len());
        for (checksum, mnemonic) in described.0 {
            self.insert(checksum, mnemonic);
        }
    }

    /// How to spell the configuration `checksum` names for a reader: its
    /// mnemonic, followed by the start of the checksum when another known
    /// configuration shares the mnemonic, so a name gains that suffix when a
    /// second configuration of its mnemonic becomes known. `None` for an
    /// unknown checksum.
    pub fn name(&self, checksum: &str) -> Option<String> {
        let mnemonic = self.0.get(checksum)?;
        let shared = self
            .0
            .iter()
            .any(|(other, described)| other != checksum && described == mnemonic);
        Some(match shared {
            true => format!("{mnemonic} {}", short(checksum)),
            false => mnemonic.clone(),
        })
    }

    /// [`name`](Self::name), and for an unknown checksum its start.
    pub fn name_or_checksum(&self, checksum: &str) -> String {
        self.name(checksum).unwrap_or_else(|| short(checksum))
    }
}

fn short(checksum: &str) -> String {
    checksum.chars().take(CHECKSUM_SHOWN).collect()
}

/// Which of the reports of one kind a [`Session`] has taken what it holds of
/// that kind from. A reader that keeps the one its reading was taken at can
/// tell whether what it read is still what the session holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revision(u64);

/// One of the readers reporting to a [`Session`], minted by
/// [`Session::reporter`], by which the session tells its reports from the
/// others' and withdraws them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReporterId(u64);

/// What a [`Session`] holds of one kind of report: each reporter's latest,
/// the latest of them standing for the invocation. An invocation writes one
/// of each output a kind is reported from, so a report stands in place of any
/// taken before, and the revision moves whenever the report standing changes.
#[derive(Default)]
struct Reported<T> {
    /// In the order they were reported, a reporter's latest only.
    reports: Vec<(ReporterId, T)>,
    /// What stands while no report does.
    unreported: T,
    revision: Revision,
}

impl<T> Reported<T> {
    fn current(&self) -> &T {
        self.reports
            .last()
            .map_or(&self.unreported, |(_, value)| value)
    }

    fn report(&mut self, by: ReporterId, value: T) {
        self.put(by, value);
        self.revise();
    }

    /// Takes `value` for what `by` reports, standing for the invocation,
    /// without revising.
    fn put(&mut self, by: ReporterId, value: T) {
        self.reports.retain(|(reporter, _)| *reporter != by);
        self.reports.push((by, value));
    }

    /// Withdraws what `by` reported, revising where it stood.
    fn withdraw(&mut self, by: ReporterId) -> bool {
        let stood = self.take_back(by).is_some();
        if stood {
            self.revise();
        }
        stood
    }

    /// Withdraws what `by` reported without revising; returns it where it
    /// stood.
    fn take_back(&mut self, by: ReporterId) -> Option<T> {
        let place = self
            .reports
            .iter()
            .position(|(reporter, _)| *reporter == by)?;
        let (_, withdrawn) = self.reports.remove(place);
        (place == self.reports.len()).then_some(withdrawn)
    }

    fn revise(&mut self) {
        self.revision.0 += 1;
    }
}

/// What the views of one invocation's outputs share. Each output's reader adds
/// what it learns of the invocation, for the views of the other outputs to read,
/// and withdraws it when it closes.
#[derive(Default)]
pub struct Session {
    next_reporter: u64,
    /// Shared, as a derivation off the UI thread reads it.
    action_graph: Reported<Option<Arc<ActionGraph>>>,
    attempts: Reported<Attempts>,
    chosen: Option<Chosen>,
    chosen_revision: Revision,
    /// Never withdrawn: a checksum names the same configuration in every
    /// output, so a description stays true once its output closes.
    configurations: Configurations,
    critical_path: Reported<Traced>,
    /// Shared, as a derivation off the UI thread reads it. Held, every
    /// spawn's outputs with it, until its reporter withdraws it, whether or
    /// not a view of the invocation reads them.
    logged_spawns: Reported<Option<Arc<LoggedSpawns>>>,
    outcomes: Reported<Outcomes>,
    timings: Reported<Timings>,
}

/// What a [`Session`] holds of the critical path its invocation's profile
/// records.
#[derive(Default)]
struct Traced {
    /// In the order they started.
    steps: Vec<CriticalStep>,
    /// Least slack first.
    nearly_critical: Vec<CriticalStep>,
}

/// What a reader chose of an invocation's critical path in a view of one of
/// its outputs, for the views of the others to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chosen {
    /// A step, by its place among the path's steps.
    Step(usize),
    /// An action nearly critical, by its place among those.
    NearlyCritical(usize),
}

impl Session {
    /// A reporter of its own for a reader to report to this session by.
    pub fn reporter(&mut self) -> ReporterId {
        let id = ReporterId(self.next_reporter);
        self.next_reporter += 1;
        id
    }

    /// Withdraws everything `by` reported, the invocation's reports of each
    /// kind falling back to the latest another reporter made. Says whether
    /// what the session holds changed.
    pub fn withdraw(&mut self, by: ReporterId) -> bool {
        let Self {
            next_reporter: _,
            action_graph,
            attempts,
            chosen: _,
            chosen_revision: _,
            configurations: _,
            critical_path,
            logged_spawns,
            outcomes,
            timings,
        } = self;
        let mut changed = action_graph.withdraw(by);
        changed |= attempts.withdraw(by);
        changed |= logged_spawns.withdraw(by);
        changed |= outcomes.withdraw(by);
        changed |= timings.withdraw(by);
        if let Some(withdrawn) = critical_path.take_back(by) {
            let retraced = Retraced::from(&withdrawn, critical_path.current());
            changed |= self.retrace(retraced);
        }
        changed
    }

    pub fn configurations(&self) -> &Configurations {
        &self.configurations
    }

    /// Adds the configurations one of the invocation's outputs describes.
    pub fn add_configurations(&mut self, described: Configurations) {
        self.configurations.extend(described);
    }

    /// What the invocation's build reported.
    pub fn outcomes(&self) -> &Outcomes {
        self.outcomes.current()
    }

    pub fn outcomes_revision(&self) -> Revision {
        self.outcomes.revision
    }

    pub fn report_outcomes(&mut self, by: ReporterId, reported: Outcomes) {
        self.outcomes.report(by, reported);
    }

    /// What the invocation's profile recorded of its actions.
    pub fn timings(&self) -> &Timings {
        self.timings.current()
    }

    pub fn timings_revision(&self) -> Revision {
        self.timings.revision
    }

    pub fn report_timings(&mut self, by: ReporterId, reported: Timings) {
        self.timings.report(by, reported);
    }

    /// When the invocation's actions were executed; none where its profile
    /// cannot place them on the wall clock.
    pub fn attempts(&self) -> &Attempts {
        self.attempts.current()
    }

    pub fn attempts_revision(&self) -> Revision {
        self.attempts.revision
    }

    pub fn report_attempts(&mut self, by: ReporterId, reported: Attempts) {
        self.attempts.report(by, reported);
    }

    /// The spawns the invocation's execution log records, `None` while none
    /// is reported.
    pub fn logged_spawns(&self) -> Option<&Arc<LoggedSpawns>> {
        self.logged_spawns.current().as_ref()
    }

    pub fn logged_spawns_revision(&self) -> Revision {
        self.logged_spawns.revision
    }

    pub fn report_logged_spawns(&mut self, by: ReporterId, reported: LoggedSpawns) {
        self.logged_spawns.report(by, Some(Arc::new(reported)));
    }

    /// The invocation's action graph, `None` while none is reported.
    pub fn action_graph(&self) -> Option<&Arc<ActionGraph>> {
        self.action_graph.current().as_ref()
    }

    pub fn action_graph_revision(&self) -> Revision {
        self.action_graph.revision
    }

    pub fn report_action_graph(&mut self, by: ReporterId, reported: ActionGraph) {
        self.action_graph.report(by, Some(Arc::new(reported)));
    }

    /// The steps of the invocation's critical path in the order they
    /// started, none while its profile's path is not reported.
    pub fn critical_path(&self) -> &[CriticalStep] {
        &self.critical_path.current().steps
    }

    pub fn critical_path_revision(&self) -> Revision {
        self.critical_path.revision
    }

    /// The actions off the invocation's critical path closest to becoming
    /// critical, least slack first, none while not reported.
    pub fn nearly_critical(&self) -> &[CriticalStep] {
        &self.critical_path.current().nearly_critical
    }

    /// Takes `steps` for the steps of the critical path the invocation's
    /// profile records and `nearly_critical` for the actions closest to
    /// becoming critical, as `by` reports them. Says whether either differs
    /// from what is held: the same lists again are no new report, and a
    /// choice among a list that changes is let go of.
    pub fn report_critical_path(
        &mut self,
        by: ReporterId,
        steps: Vec<CriticalStep>,
        nearly_critical: Vec<CriticalStep>,
    ) -> bool {
        let traced = Traced {
            steps,
            nearly_critical,
        };
        let retraced = Retraced::from(self.critical_path.current(), &traced);
        self.critical_path.put(by, traced);
        self.retrace(retraced)
    }

    /// Revises the critical path where a list of it changed, letting go of
    /// a choice among that list; says whether one did.
    fn retrace(&mut self, retraced: Retraced) -> bool {
        if !(retraced.steps || retraced.nearly_critical) {
            return false;
        }
        self.critical_path.revise();
        let changed = match self.chosen {
            Some(Chosen::Step(_)) => retraced.steps,
            Some(Chosen::NearlyCritical(_)) => retraced.nearly_critical,
            None => false,
        };
        if changed {
            self.set_chosen(None);
        }
        true
    }

    /// What of the [critical path](Self::critical_path) a reader last chose,
    /// always one the path holds.
    pub fn chosen(&self) -> Option<Chosen> {
        self.chosen
    }

    /// Changes with every choice, of something or of nothing, the same
    /// chosen again included, so that a view shows each choice once.
    pub fn chosen_revision(&self) -> Revision {
        self.chosen_revision
    }

    /// Takes `chosen` for what a reader chose of the critical path, or
    /// nothing for `None`. Says whether it took it: a place past the list it
    /// indexes holds nothing to choose.
    pub fn choose(&mut self, chosen: Option<Chosen>) -> bool {
        let held = match chosen {
            Some(Chosen::Step(index)) => index < self.critical_path().len(),
            Some(Chosen::NearlyCritical(index)) => index < self.nearly_critical().len(),
            None => true,
        };
        if held {
            self.set_chosen(chosen);
        }
        held
    }

    fn set_chosen(&mut self, chosen: Option<Chosen>) {
        self.chosen = chosen;
        self.chosen_revision.0 += 1;
    }
}

/// Which of the lists of a critical path differ between two reports of it.
#[derive(Clone, Copy)]
struct Retraced {
    steps: bool,
    nearly_critical: bool,
}

impl Retraced {
    fn from(before: &Traced, after: &Traced) -> Self {
        Self {
            steps: before.steps != after.steps,
            nearly_critical: before.nearly_critical != after.nearly_critical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_critical_path_reported_again_alike_is_no_new_report() {
        let mut session = Session::default();
        let by = session.reporter();
        let steps = || vec![CriticalStep::new(Some("bazel-out/a.o"), None)];
        assert!(session.report_critical_path(by, steps(), Vec::new()));
        let traced = session.critical_path_revision();
        assert!(session.choose(Some(Chosen::Step(0))));
        assert!(!session.report_critical_path(by, steps(), Vec::new()));
        assert_eq!(session.critical_path_revision(), traced);
        assert_eq!(
            session.chosen(),
            Some(Chosen::Step(0)),
            "still a step of the path"
        );
        assert!(session.report_critical_path(by, Vec::new(), Vec::new()));
        assert_ne!(session.critical_path_revision(), traced);
    }

    #[test]
    fn a_critical_path_reported_anew_holds_no_step_chosen_of_the_last() {
        let mut session = Session::default();
        let by = session.reporter();
        session.report_critical_path(
            by,
            vec![CriticalStep::new(Some("bazel-out/a.o"), None)],
            Vec::new(),
        );
        let unchosen = session.chosen_revision();
        assert!(session.choose(Some(Chosen::Step(0))));
        assert_eq!(session.chosen(), Some(Chosen::Step(0)));
        let chosen = session.chosen_revision();
        assert_ne!(chosen, unchosen);

        session.report_critical_path(
            by,
            vec![CriticalStep::new(Some("bazel-out/b.o"), None)],
            Vec::new(),
        );
        assert_eq!(session.chosen(), None);
        assert_ne!(session.chosen_revision(), chosen);
    }

    #[test]
    fn a_step_past_the_critical_path_is_not_chosen() {
        let mut session = Session::default();
        let by = session.reporter();
        session.report_critical_path(
            by,
            vec![CriticalStep::new(Some("bazel-out/a.o"), None)],
            Vec::new(),
        );
        assert!(session.choose(Some(Chosen::Step(0))));
        let chosen = session.chosen_revision();
        assert!(!session.choose(Some(Chosen::Step(1))));
        assert_eq!(session.chosen(), Some(Chosen::Step(0)));
        assert_eq!(session.chosen_revision(), chosen);
        assert!(session.choose(None));
        assert_eq!(session.chosen(), None);
    }

    #[test]
    fn a_choice_is_let_go_of_only_where_the_list_it_indexes_changes() {
        let mut session = Session::default();
        let by = session.reporter();
        let action = |output: &str| CriticalStep::new(Some(output), None);
        let steps = || vec![action("bazel-out/a.o")];
        session.report_critical_path(by, steps(), vec![action("bazel-out/n.o")]);
        assert!(session.choose(Some(Chosen::NearlyCritical(0))));
        assert!(!session.choose(Some(Chosen::NearlyCritical(1))));
        assert_eq!(session.chosen(), Some(Chosen::NearlyCritical(0)));

        assert!(session.report_critical_path(by, Vec::new(), vec![action("bazel-out/n.o")]));
        assert_eq!(session.chosen(), Some(Chosen::NearlyCritical(0)));
        assert!(session.report_critical_path(by, Vec::new(), vec![action("bazel-out/m.o")]));
        assert_eq!(session.chosen(), None);

        session.report_critical_path(by, steps(), Vec::new());
        assert!(session.choose(Some(Chosen::Step(0))));
        assert!(session.report_critical_path(by, steps(), vec![action("bazel-out/n.o")]));
        assert_eq!(session.chosen(), Some(Chosen::Step(0)));
    }

    #[test]
    fn millis_scale_to_micros_and_saturate() {
        assert_eq!(EpochMicros::from_millis(1_500).micros(), 1_500_000);
        assert_eq!(EpochMicros::from_millis(-2).micros(), -2_000);
        assert_eq!(EpochMicros::from_millis(i64::MAX).micros(), i64::MAX);
        assert_eq!(EpochMicros::from_millis(i64::MIN).micros(), i64::MIN);
    }

    #[test]
    fn seconds_round_to_the_nearest_micro_and_saturate() {
        assert_eq!(
            EpochMicros::from_secs_f64(1_781_439_245.231),
            EpochMicros::from_millis(1_781_439_245_231)
        );
        assert_eq!(EpochMicros::from_secs_f64(-0.000_001_6).micros(), -2);
        assert_eq!(EpochMicros::from_secs_f64(f64::INFINITY).micros(), i64::MAX);
        assert_eq!(EpochMicros::from_secs_f64(f64::NAN).micros(), 0);
    }

    #[test]
    fn an_offset_moves_an_instant_in_order_and_saturates() {
        let origin = EpochMicros::from_millis(1_781_439_245_231);
        let later = origin.plus_micros(1_500);
        assert_eq!(later.micros() - origin.micros(), 1_500);
        assert!(origin < later && origin.plus_micros(-1) < origin);
        assert_eq!(later.max(origin), later);
        assert_eq!(origin.plus_micros(i64::MAX).micros(), i64::MAX);
        assert_eq!(
            origin.plus_secs_f64(1.000_000_6),
            origin.plus_micros(1_000_001)
        );
        assert_eq!(origin.plus_secs_f64(f64::NAN), origin);
    }

    #[test]
    fn an_invocation_id_is_never_empty() {
        assert_eq!(InvocationId::new(""), None);
        let id = InvocationId::new("a5f7f3e6").expect("non-empty");
        assert_eq!(id.as_str(), "a5f7f3e6");
    }

    #[test]
    fn a_name_carries_the_checksum_only_to_tell_a_shared_mnemonic_apart() {
        let mut known = Configurations::default();
        known.insert("55817e2200e2".into(), "k8-fastbuild".into());
        assert_eq!(known.name("55817e2200e2").as_deref(), Some("k8-fastbuild"));

        known.insert("91e24984663c".into(), "k8-fastbuild".into());
        known.insert("0c3d9a7714be".into(), "k8-opt-exec".into());
        let name = |checksum| known.name(checksum);
        assert_eq!(name("55817e2200e2").as_deref(), Some("k8-fastbuild 55817e"));
        assert_eq!(name("91e24984663c").as_deref(), Some("k8-fastbuild 91e249"));
        assert_eq!(name("0c3d9a7714be").as_deref(), Some("k8-opt-exec"));
        assert_eq!(name("ffffffffffff"), None);
        assert_eq!(known.name_or_checksum("0c3d9a7714be"), "k8-opt-exec");
        assert_eq!(known.name_or_checksum("ffffffffffff"), "ffffff");
    }

    fn configurations(described: &[(&str, &str)]) -> Configurations {
        let mut configurations = Configurations::default();
        for (checksum, mnemonic) in described {
            configurations.insert((*checksum).into(), (*mnemonic).into());
        }
        configurations
    }

    #[test]
    fn a_session_knows_the_configurations_every_output_described() {
        let mut session = Session::default();
        assert_eq!(session.configurations().name("55817e2200e2"), None);

        session.add_configurations(configurations(&[("55817e2200e2", "k8-fastbuild")]));
        session.add_configurations(configurations(&[("0c3d9a7714be", "k8-opt-exec")]));
        let name = |checksum| session.configurations().name(checksum);
        assert_eq!(name("55817e2200e2").as_deref(), Some("k8-fastbuild"));
        assert_eq!(name("0c3d9a7714be").as_deref(), Some("k8-opt-exec"));
    }

    #[test]
    fn a_configuration_a_second_output_describes_keeps_its_first_description() {
        let mut session = Session::default();
        session.add_configurations(configurations(&[("55817e2200e2", "k8-fastbuild")]));
        session.add_configurations(configurations(&[
            ("55817e2200e2", "k8-opt"),
            ("91e24984663c", "k8-fastbuild"),
        ]));
        let name = |checksum| session.configurations().name(checksum);
        assert_eq!(name("55817e2200e2").as_deref(), Some("k8-fastbuild 55817e"));
        assert_eq!(name("91e24984663c").as_deref(), Some("k8-fastbuild 91e249"));
    }

    #[test]
    fn a_session_holds_the_outcomes_last_reported_to_it() {
        let app = ConfiguredTarget::new("//app".to_owned(), Some("55817e2200e2".to_owned()));
        let mut session = Session::default();
        let by = session.reporter();
        assert!(session.outcomes().is_empty());

        let mut first = Outcomes::default();
        first.report_build(app.clone(), BuildOutcome::NotBuilt);
        session.report_outcomes(by, first);
        let mut second = Outcomes::default();
        second.report_build(app.clone(), BuildOutcome::Built);
        session.report_outcomes(by, second);

        assert_eq!(
            session.outcomes().build(app.borrowed()),
            Some(&BuildOutcome::Built)
        );
    }

    #[test]
    fn a_sessions_outcomes_are_revised_by_each_report_of_them_and_by_nothing_else() {
        let mut session = Session::default();
        let by = session.reporter();
        let unreported = session.outcomes_revision();
        assert_eq!(unreported, Revision::default());

        session.add_configurations(configurations(&[("55817e2200e2", "k8-fastbuild")]));
        assert_eq!(session.outcomes_revision(), unreported);

        session.report_outcomes(by, Outcomes::default());
        let first = session.outcomes_revision();
        assert_ne!(first, unreported);
        session.add_configurations(configurations(&[("0c3d9a7714be", "k8-opt-exec")]));
        assert_eq!(session.outcomes_revision(), first);

        session.report_outcomes(by, Outcomes::default());
        assert_ne!(session.outcomes_revision(), first);
        assert_ne!(session.outcomes_revision(), unreported);
    }

    #[test]
    fn a_sessions_timings_are_revised_by_each_report_of_them_and_by_nothing_else() {
        let mut session = Session::default();
        let by = session.reporter();
        let untimed = session.timings_revision();
        session.report_outcomes(by, Outcomes::default());
        assert_eq!(session.timings_revision(), untimed);

        let mut timings = Timings::default();
        let wall = std::time::Duration::from_millis(3);
        timings.record(Profiled {
            primary_output: Some("bazel-out/a.o"),
            owner: None,
            wall,
            spawns: Spawns::on(Runner::Local, wall),
        });
        let timing = Timing {
            wall,
            runner: Runner::Local,
        };
        session.report_timings(by, timings);
        let timed = session.timings_revision();
        assert_ne!(timed, untimed);
        assert_eq!(session.timings().action("bazel-out/a.o"), Some(timing));
        let outcomes = session.outcomes_revision();
        session.report_outcomes(by, Outcomes::default());
        assert_eq!(session.timings_revision(), timed);
        assert_ne!(session.outcomes_revision(), outcomes);

        session.report_timings(by, Timings::default());
        assert_ne!(session.timings_revision(), timed);
        assert!(session.timings().is_empty());
    }

    #[test]
    fn a_sessions_attempts_are_revised_by_each_report_of_them_and_by_nothing_else() {
        let mut session = Session::default();
        let by = session.reporter();
        let unattempted = session.attempts_revision();
        session.report_timings(by, Timings::default());
        assert_eq!(session.attempts_revision(), unattempted);

        let attempt = attempts::tests::attempt(0, 10, &[]);
        let mut attempts = Attempts::default();
        attempts.record(Some("bazel-out/a.o"), None, attempt);
        session.report_attempts(by, attempts);
        let attempted = session.attempts_revision();
        assert_ne!(attempted, unattempted);
        assert_eq!(
            session.attempts().action("bazel-out/a.o"),
            Some(&[attempt][..])
        );
        session.report_timings(by, Timings::default());
        assert_eq!(session.attempts_revision(), attempted);

        session.report_attempts(by, Attempts::default());
        assert_ne!(session.attempts_revision(), attempted);
        assert!(session.attempts().is_empty());
    }

    #[test]
    fn a_sessions_logged_spawns_are_revised_by_each_report_of_them_and_by_nothing_else() {
        let mut session = Session::default();
        let by = session.reporter();
        let unlogged = session.logged_spawns_revision();
        assert!(session.logged_spawns().is_none());
        session.report_timings(by, Timings::default());
        assert_eq!(session.logged_spawns_revision(), unlogged);

        session.report_logged_spawns(by, LoggedSpawns::default());
        let logged = session.logged_spawns_revision();
        assert_ne!(logged, unlogged);
        assert!(
            session
                .logged_spawns()
                .is_some_and(|spawns| spawns.is_empty())
        );
        session.report_outcomes(by, Outcomes::default());
        assert_eq!(session.logged_spawns_revision(), logged);

        session.report_logged_spawns(by, LoggedSpawns::default());
        assert_ne!(session.logged_spawns_revision(), logged);
    }

    #[test]
    fn a_sessions_action_graph_is_revised_by_each_report_of_it_and_by_nothing_else() {
        let mut session = Session::default();
        let by = session.reporter();
        let ungraphed = session.action_graph_revision();
        assert!(session.action_graph().is_none());
        session.report_logged_spawns(by, LoggedSpawns::default());
        assert_eq!(session.action_graph_revision(), ungraphed);

        session.report_action_graph(
            by,
            ActionGraph::new(vec![Some("bazel-out/a.o".to_owned())], Vec::new()),
        );
        let graphed = session.action_graph_revision();
        assert_ne!(graphed, ungraphed);
        assert_eq!(session.action_graph().map(|graph| graph.len()), Some(1));
        session.report_timings(by, Timings::default());
        assert_eq!(session.action_graph_revision(), graphed);

        session.report_action_graph(by, ActionGraph::default());
        assert_ne!(session.action_graph_revision(), graphed);
        assert!(session.action_graph().is_some_and(|graph| graph.is_empty()));
    }

    fn built(target: &str, outcome: BuildOutcome) -> Outcomes {
        let mut outcomes = Outcomes::default();
        outcomes.report_build(ConfiguredTarget::new(target.to_owned(), None), outcome);
        outcomes
    }

    fn outcome(session: &Session, target: &str) -> Option<BuildOutcome> {
        let target = ConfiguredTarget::new(target.to_owned(), None);
        session.outcomes().build(target.borrowed()).cloned()
    }

    #[test]
    fn a_withdrawn_report_gives_way_to_the_latest_another_reporter_made() {
        let mut session = Session::default();
        let (first, second) = (session.reporter(), session.reporter());
        session.report_outcomes(first, built("//a", BuildOutcome::Built));
        session.report_outcomes(second, built("//a", BuildOutcome::NotBuilt));
        assert_eq!(outcome(&session, "//a"), Some(BuildOutcome::NotBuilt));

        let standing = session.outcomes_revision();
        assert!(session.withdraw(second));
        assert_ne!(session.outcomes_revision(), standing);
        assert_eq!(outcome(&session, "//a"), Some(BuildOutcome::Built));

        assert!(session.withdraw(first));
        assert!(session.outcomes().is_empty());
    }

    #[test]
    fn withdrawing_a_report_that_does_not_stand_revises_nothing() {
        let mut session = Session::default();
        let (first, second) = (session.reporter(), session.reporter());
        session.report_outcomes(first, built("//a", BuildOutcome::Built));
        session.report_outcomes(second, built("//a", BuildOutcome::NotBuilt));
        let standing = session.outcomes_revision();
        assert!(!session.withdraw(first));
        assert_eq!(session.outcomes_revision(), standing);
        assert!(session.withdraw(second));
        assert!(session.outcomes().is_empty(), "the first report is gone");
    }

    #[test]
    fn a_reporter_reporting_again_stands_in_place_of_its_last_report() {
        let mut session = Session::default();
        let (first, second) = (session.reporter(), session.reporter());
        session.report_outcomes(first, built("//a", BuildOutcome::Built));
        session.report_outcomes(second, built("//b", BuildOutcome::Built));
        session.report_outcomes(first, built("//c", BuildOutcome::Built));
        assert_eq!(outcome(&session, "//c"), Some(BuildOutcome::Built));
        session.withdraw(first);
        assert_eq!(outcome(&session, "//b"), Some(BuildOutcome::Built));
        assert_eq!(outcome(&session, "//a"), None);
    }

    #[test]
    fn a_reporter_that_reported_nothing_withdraws_nothing() {
        let mut session = Session::default();
        let (reporting, silent) = (session.reporter(), session.reporter());
        assert_ne!(reporting, silent);
        session.report_timings(reporting, Timings::default());
        session.add_configurations(configurations(&[("55817e2200e2", "k8-fastbuild")]));
        let timed = session.timings_revision();
        assert!(!session.withdraw(silent));
        assert_eq!(session.timings_revision(), timed);
    }

    #[test]
    fn configurations_outlast_the_reporters_that_described_them() {
        let mut session = Session::default();
        let by = session.reporter();
        session.add_configurations(configurations(&[("55817e2200e2", "k8-fastbuild")]));
        session.report_outcomes(by, Outcomes::default());
        session.withdraw(by);
        assert_eq!(
            session.configurations().name("55817e2200e2").as_deref(),
            Some("k8-fastbuild")
        );
    }

    #[test]
    fn a_withdrawn_critical_path_lets_go_of_a_choice_only_where_its_list_changes() {
        let mut session = Session::default();
        let (first, second) = (session.reporter(), session.reporter());
        let steps = || vec![CriticalStep::new(Some("bazel-out/a.o"), None)];
        session.report_critical_path(first, steps(), Vec::new());
        session.report_critical_path(second, steps(), Vec::new());
        assert!(session.choose(Some(Chosen::Step(0))));
        let traced = session.critical_path_revision();
        assert!(!session.withdraw(second), "the same path stands");
        assert_eq!(session.critical_path_revision(), traced);
        assert_eq!(session.chosen(), Some(Chosen::Step(0)));

        let chosen = session.chosen_revision();
        assert!(session.withdraw(first));
        assert!(session.critical_path().is_empty());
        assert_ne!(session.critical_path_revision(), traced);
        assert_eq!(session.chosen(), None);
        assert_ne!(session.chosen_revision(), chosen);
    }

    #[test]
    fn a_checksum_described_again_keeps_its_first_description() {
        let mut known = Configurations::default();
        known.insert("55817e2200e2".into(), "k8-fastbuild".into());
        known.insert("55817e2200e2".into(), "k8-opt".into());
        assert_eq!(known.name("55817e2200e2").as_deref(), Some("k8-fastbuild"));
    }
}
