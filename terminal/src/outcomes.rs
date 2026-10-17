//! Reads a build's [`Outcomes`] out of the events of its build event stream.

use std::num::{NonZeroI32, NonZeroU32};
use std::time::Duration;

use bviz_invocation::{
    BuildOutcome, Cause, ConfiguredTarget, FailedAction, Outcomes, TestOutcome, TestStatus,
};

use crate::bep::build_event_stream::{
    self as bes, Aborted, ActionExecuted, BuildEventId, TestSummary,
    aborted::AbortReason,
    build_event_id::{ConfigurationId, Id},
};
use crate::bep::failure_details::failure_detail::Category;

/// The `ConfigurationId` of the null configuration, which unconfigurable
/// targets such as source files carry.
pub(crate) const NULL_CONFIGURATION: &str = "none";

/// The checksum of the configuration `id` names; `None` for the null
/// configuration and for an id that names none.
pub(crate) fn checksum(id: Option<ConfigurationId>) -> Option<String> {
    id.map(|id| id.id)
        .filter(|id| !id.is_empty() && id != NULL_CONFIGURATION)
}

fn target(label: String, configuration: Option<ConfigurationId>) -> ConfiguredTarget {
    ConfiguredTarget::new(label, checksum(configuration))
}

/// The root causes among the `children` of a failed target's event, which
/// under `--experimental_bep_target_summary` also announce the target's
/// summary.
fn causes(children: Vec<BuildEventId>) -> Vec<Cause> {
    let mut causes = Vec::with_capacity(children.len());
    causes.extend(children.into_iter().filter_map(|child| match child.id? {
        Id::ActionCompleted(action) => Some(Cause::Action {
            primary_output: action.primary_output,
        }),
        Id::ConfiguredLabel(id) => Some(Cause::Target(target(id.label, id.configuration))),
        Id::UnconfiguredLabel(id) => Some(Cause::Target(target(id.label, None))),
        _ => None,
    }));
    causes
}

/// A `TargetComplete` event, of a target or of an aspect on it.
pub(crate) fn completed(
    outcomes: &mut Outcomes,
    id: Option<Id>,
    children: Vec<BuildEventId>,
    success: bool,
) {
    let Some(Id::TargetCompleted(id)) = id else {
        return;
    };
    let outcome = match success {
        true => BuildOutcome::Built,
        false => BuildOutcome::Failed {
            causes: causes(children),
        },
    };
    outcomes.report_build(target(id.label, id.configuration), outcome);
}

/// An `Aborted` event, which stands in for the event its id announced.
pub(crate) fn aborted(
    outcomes: &mut Outcomes,
    id: Option<Id>,
    children: Vec<BuildEventId>,
    aborted: Aborted,
) {
    let reason = aborted.reason();
    let target = match id {
        Some(Id::TargetCompleted(id)) => target(id.label, id.configuration),
        // The event of a target not yet configured, whose id names no
        // configuration: the one a target is under when its analysis fails.
        Some(Id::TargetConfigured(id)) => target(id.label, None),
        Some(Id::ConfiguredLabel(id)) => {
            return root_cause(outcomes, target(id.label, id.configuration), aborted);
        }
        Some(Id::UnconfiguredLabel(id)) => {
            return root_cause(outcomes, target(id.label, None), aborted);
        }
        _ => return,
    };
    let outcome = match reason {
        AbortReason::AnalysisFailure => BuildOutcome::AnalysisFailed {
            causes: causes(children),
        },
        AbortReason::Skipped => BuildOutcome::Skipped,
        // A build that ends early aborts the targets it leaves unfinished,
        // and gives no reason for those a failure elsewhere stopped.
        _ => BuildOutcome::NotBuilt,
    };
    outcomes.report_build(target, outcome);
}

/// An `Aborted` event under the id of the root cause `cause`, which describes
/// what went wrong loading or analysing it.
fn root_cause(outcomes: &mut Outcomes, cause: ConfiguredTarget, aborted: Aborted) {
    // A build that ends early aborts a root cause it announced and did not
    // come to, under the reason and the description of its ending.
    let described = matches!(
        aborted.reason(),
        AbortReason::AnalysisFailure | AbortReason::LoadingFailure
    );
    if described && !aborted.description.is_empty() {
        outcomes.report_target_error(cause, aborted.description);
    }
}

/// An `ActionExecuted` event, which a stream holds for a successful action
/// too under `--build_event_publish_all_actions`.
pub(crate) fn action(outcomes: &mut Outcomes, id: Option<Id>, action: ActionExecuted) {
    let Some(Id::ActionCompleted(id)) = id else {
        return;
    };
    if action.success {
        return;
    }
    let owner = (!id.label.is_empty()).then(|| target(id.label, id.configuration));
    // `ActionExecuted.exit_code` is the code Bazel itself exits with over the
    // failure, so the only code of the action's process is the spawn's.
    let (message, exit_code) = match action.failure_detail {
        Some(detail) => {
            let exit_code = match detail.category {
                Some(Category::Spawn(spawn)) => NonZeroI32::new(spawn.spawn_exit_code),
                _ => None,
            };
            (detail.message, exit_code)
        }
        None => (String::new(), None),
    };
    outcomes.report_failed_action(
        id.primary_output,
        FailedAction {
            owner,
            mnemonic: action.r#type,
            exit_code,
            message,
        },
    );
}

pub(crate) fn test_summary(outcomes: &mut Outcomes, id: Option<Id>, summary: TestSummary) {
    let Some(Id::TestSummary(id)) = id else {
        return;
    };
    let outcome = TestOutcome {
        status: status(summary.overall_status()),
        attempts: count(summary.attempt_count),
        runs: count(summary.run_count),
        shards: NonZeroU32::new(count(summary.shard_count)),
        actions: count(summary.total_run_count),
        cached_actions: count(summary.total_num_cached),
        run_duration: summary
            .total_run_duration
            .map_or(Duration::ZERO, |duration| {
                Duration::new(
                    duration.seconds.try_into().unwrap_or(0),
                    duration.nanos.try_into().unwrap_or(0),
                )
            }),
    };
    outcomes.report_test(target(id.label, id.configuration), outcome);
}

fn status(status: bes::TestStatus) -> TestStatus {
    match status {
        bes::TestStatus::NoStatus => TestStatus::NoStatus,
        bes::TestStatus::Passed => TestStatus::Passed,
        bes::TestStatus::Flaky => TestStatus::Flaky,
        bes::TestStatus::Timeout => TestStatus::Timeout,
        bes::TestStatus::Failed => TestStatus::Failed,
        bes::TestStatus::Incomplete => TestStatus::Incomplete,
        bes::TestStatus::RemoteFailure => TestStatus::RemoteFailure,
        bes::TestStatus::FailedToBuild => TestStatus::FailedToBuild,
        bes::TestStatus::ToolHaltedBeforeTesting => TestStatus::ToolHaltedBeforeTesting,
    }
}

/// A count the stream holds signed; zero for a negative one.
fn count(count: i32) -> u32 {
    count.try_into().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    use crate::bep::build_event_stream::{
        BuildEvent, TargetComplete, TargetConfigured, TestResult,
        build_event::Payload,
        build_event_id::{
            ActionCompletedId, ConfiguredLabelId, PatternExpandedId, TargetCompletedId,
            TargetConfiguredId, TargetSummaryId, TestResultId, TestSummaryId, UnconfiguredLabelId,
        },
    };
    use crate::bep::duration_proto::google::protobuf::Duration as ProtoDuration;
    use crate::bep::failure_details::{FailureDetail, Spawn};
    use crate::bep::timestamp_proto::google::protobuf::Timestamp;

    const BUILD_CONFIG: &str = "a7a71fd16b14";
    const TEST_CONFIG: &str = "5d1c0e7fb2a9";
    const BROKEN_OUT: &str = "bazel-out/k8-fastbuild/bin/codegen/broken.out";
    const LINT: &str = "//lint:defs.bzl%lint";

    /// When an event that anchors the timeline says it happened.
    const ANCHORED: Timestamp = Timestamp {
        seconds: 1_790_952_715,
        nanos: 0,
    };

    fn event(id: Id, children: Vec<Id>, payload: Payload) -> Vec<u8> {
        BuildEvent {
            id: Some(BuildEventId { id: Some(id) }),
            children: children
                .into_iter()
                .map(|child| BuildEventId { id: Some(child) })
                .collect(),
            payload: Some(payload),
            ..Default::default()
        }
        .encode_length_delimited_to_vec()
    }

    fn outcomes(events: &[Vec<u8>]) -> Outcomes {
        crate::output::read_slice(&events.concat())
            .expect("parse stream")
            .into_inner()
            .outcomes
    }

    /// How many things `outcomes` reports on.
    fn reported(outcomes: &Outcomes) -> usize {
        outcomes.builds().count() + outcomes.tests().count() + outcomes.failed_actions().count()
    }

    fn configuration(id: &str) -> Option<ConfigurationId> {
        Some(ConfigurationId { id: id.into() })
    }

    fn target_completed(label: &str, configuration: &str) -> Id {
        aspect_completed(label, configuration, "")
    }

    fn aspect_completed(label: &str, configuration: &str, aspect: &str) -> Id {
        Id::TargetCompleted(TargetCompletedId {
            label: label.into(),
            configuration: self::configuration(configuration),
            aspect: aspect.into(),
        })
    }

    fn target_configured(label: &str) -> Id {
        Id::TargetConfigured(TargetConfiguredId {
            label: label.into(),
            ..Default::default()
        })
    }

    fn action_completed(primary_output: &str, label: &str, configuration: &str) -> Id {
        Id::ActionCompleted(ActionCompletedId {
            primary_output: primary_output.into(),
            label: label.into(),
            configuration: self::configuration(configuration),
        })
    }

    fn configured_label(label: &str, configuration: &str) -> Id {
        Id::ConfiguredLabel(ConfiguredLabelId {
            label: label.into(),
            configuration: self::configuration(configuration),
        })
    }

    fn unconfigured_label(label: &str) -> Id {
        Id::UnconfiguredLabel(UnconfiguredLabelId {
            label: label.into(),
        })
    }

    fn test_summary_id(label: &str, configuration: &str) -> Id {
        Id::TestSummary(TestSummaryId {
            label: label.into(),
            configuration: self::configuration(configuration),
        })
    }

    fn test_result_id(label: &str, configuration: &str) -> Id {
        Id::TestResult(TestResultId {
            label: label.into(),
            configuration: self::configuration(configuration),
            run: 1,
            shard: 1,
            attempt: 1,
        })
    }

    fn complete(success: bool) -> Payload {
        Payload::Completed(TargetComplete {
            success,
            ..Default::default()
        })
    }

    fn abort(reason: AbortReason, description: &str) -> Payload {
        Payload::Aborted(Aborted {
            reason: reason.into(),
            description: description.into(),
        })
    }

    fn failure(message: &str, spawn_exit_code: Option<i32>) -> Option<FailureDetail> {
        Some(FailureDetail {
            message: message.into(),
            category: spawn_exit_code.map(|spawn_exit_code| {
                Category::Spawn(Spawn {
                    spawn_exit_code,
                    ..Default::default()
                })
            }),
        })
    }

    fn failed_action(executed: ActionExecuted) -> Payload {
        Payload::Action(ActionExecuted {
            r#type: "Genrule".into(),
            ..executed
        })
    }

    fn at<'a>(label: &'a str, configuration: Option<&'a str>) -> ConfiguredTarget<&'a str> {
        ConfiguredTarget::new(label, configuration)
    }

    fn owned(label: &str, configuration: Option<&str>) -> ConfiguredTarget {
        ConfiguredTarget::new(label.into(), configuration.map(Into::into))
    }

    fn action_cause(primary_output: &str) -> Cause {
        Cause::Action {
            primary_output: primary_output.into(),
        }
    }

    fn failed(causes: Vec<Cause>) -> BuildOutcome {
        BuildOutcome::Failed { causes }
    }

    #[test]
    fn a_completed_target_is_built() {
        let outcomes = outcomes(&[event(
            target_completed("//app:cli", BUILD_CONFIG),
            vec![],
            complete(true),
        )]);
        assert_eq!(
            outcomes.build(at("//app:cli", Some(BUILD_CONFIG))),
            Some(&BuildOutcome::Built)
        );
        assert_eq!(outcomes.build(at("//app:cli", None)), None);
        assert_eq!(outcomes.build(at("//app:hello", Some(BUILD_CONFIG))), None);
        assert_eq!(reported(&outcomes), 1);
    }

    #[test]
    fn an_event_that_names_a_target_and_states_no_outcome_reports_none() {
        let outcomes = outcomes(&[
            event(
                target_configured("//tests:timeout"),
                vec![target_completed("//tests:timeout", TEST_CONFIG)],
                Payload::Configured(TargetConfigured::default()),
            ),
            event(
                test_result_id("//tests:timeout", TEST_CONFIG),
                vec![],
                Payload::TestResult(TestResult {
                    status: bes::TestStatus::Timeout.into(),
                    ..Default::default()
                }),
            ),
            event(
                test_result_id("//tests:cut_short", TEST_CONFIG),
                vec![],
                abort(AbortReason::Incomplete, ""),
            ),
            event(
                test_summary_id("//tests:cut_short", TEST_CONFIG),
                vec![],
                abort(AbortReason::Incomplete, ""),
            ),
            event(
                Id::Pattern(PatternExpandedId {
                    pattern: vec!["//missing/...".into()],
                }),
                vec![],
                abort(AbortReason::LoadingFailure, "no targets found beneath"),
            ),
        ]);
        assert_eq!(reported(&outcomes), 0);
    }

    #[test]
    fn a_failed_target_names_its_causes_and_not_its_summary() {
        let broken = action_completed(BROKEN_OUT, "//codegen:broken_gen", BUILD_CONFIG);
        let outcomes = outcomes(&[
            event(
                target_completed("//codegen:broken_gen", BUILD_CONFIG),
                vec![broken.clone()],
                complete(false),
            ),
            event(
                target_completed("//tests:needs_broken", TEST_CONFIG),
                vec![
                    broken,
                    configured_label("//core:private", BUILD_CONFIG),
                    unconfigured_label("//core:missing.h"),
                    Id::TargetSummary(TargetSummaryId {
                        label: "//tests:needs_broken".into(),
                        configuration: configuration(TEST_CONFIG),
                    }),
                ],
                complete(false),
            ),
        ]);
        assert_eq!(
            outcomes.build(at("//codegen:broken_gen", Some(BUILD_CONFIG))),
            Some(&failed(vec![action_cause(BROKEN_OUT)]))
        );
        assert_eq!(
            outcomes.build(at("//tests:needs_broken", Some(TEST_CONFIG))),
            Some(&failed(vec![
                action_cause(BROKEN_OUT),
                Cause::Target(owned("//core:private", Some(BUILD_CONFIG))),
                Cause::Target(owned("//core:missing.h", None)),
            ]))
        );
        assert_eq!(reported(&outcomes), 2);
    }

    #[test]
    fn a_failed_action_has_the_exit_code_of_its_process_or_none() {
        const SITE_OUT: &str = "bazel-out/k8-fastbuild/bin/docs/site.out";
        const REMOTE_OUT: &str = "bazel-out/k8-fastbuild/bin/docs/remote.out";
        const INDEX_OUT: &str = "bazel-out/k8-fastbuild/bin/docs/index.out";
        let outcomes = outcomes(&[
            event(
                action_completed(BROKEN_OUT, "//codegen:broken_gen", BUILD_CONFIG),
                vec![],
                failed_action(ActionExecuted {
                    exit_code: 1,
                    failure_detail: failure("bash failed", Some(2)),
                    ..Default::default()
                }),
            ),
            event(
                action_completed(SITE_OUT, "//docs:site", BUILD_CONFIG),
                vec![],
                failed_action(ActionExecuted {
                    exit_code: 1,
                    failure_detail: failure("not all outputs were created", None),
                    ..Default::default()
                }),
            ),
            event(
                action_completed(REMOTE_OUT, "//docs:remote", BUILD_CONFIG),
                vec![],
                failed_action(ActionExecuted {
                    exit_code: 1,
                    failure_detail: failure("Exit 2", Some(0)),
                    ..Default::default()
                }),
            ),
            event(
                action_completed(INDEX_OUT, "//docs:index", BUILD_CONFIG),
                vec![],
                failed_action(ActionExecuted {
                    exit_code: 1,
                    ..Default::default()
                }),
            ),
        ]);
        assert_eq!(
            outcomes.failed_action(BROKEN_OUT),
            Some(&FailedAction {
                owner: Some(owned("//codegen:broken_gen", Some(BUILD_CONFIG))),
                mnemonic: "Genrule".into(),
                exit_code: NonZeroI32::new(2),
                message: "bash failed".into(),
            })
        );
        assert_eq!(
            outcomes.failed_action(SITE_OUT),
            Some(&FailedAction {
                owner: Some(owned("//docs:site", Some(BUILD_CONFIG))),
                mnemonic: "Genrule".into(),
                exit_code: None,
                message: "not all outputs were created".into(),
            })
        );
        let exit_code = |primary_output| {
            let action = outcomes.failed_action(primary_output).expect("reported");
            action.exit_code
        };
        assert_eq!(exit_code(REMOTE_OUT), None);
        assert_eq!(exit_code(INDEX_OUT), None);
        assert_eq!(reported(&outcomes), 4);
    }

    #[test]
    fn a_failed_action_is_owned_by_the_target_its_id_names() {
        let outcomes = outcomes(&[
            event(
                Id::ActionCompleted(ActionCompletedId {
                    primary_output: "bazel-out/bin/unconfigured.out".into(),
                    label: "//docs:site".into(),
                    configuration: None,
                }),
                vec![],
                failed_action(ActionExecuted::default()),
            ),
            event(
                Id::ActionCompleted(ActionCompletedId {
                    primary_output: "bazel-out/bin/unowned.out".into(),
                    ..Default::default()
                }),
                vec![],
                failed_action(ActionExecuted::default()),
            ),
        ]);
        let owner = |primary_output| {
            let action = outcomes.failed_action(primary_output).expect("reported");
            action.owner.clone()
        };
        assert_eq!(
            owner("bazel-out/bin/unconfigured.out"),
            Some(owned("//docs:site", None))
        );
        assert_eq!(owner("bazel-out/bin/unowned.out"), None);
    }

    #[test]
    fn a_successful_action_is_no_outcome_and_still_anchors_the_timeline() {
        let executed = |success| {
            event(
                action_completed("bazel-out/bin/app/cli", "//app:cli", BUILD_CONFIG),
                vec![],
                Payload::Action(ActionExecuted {
                    success,
                    end_time: Some(ANCHORED),
                    ..Default::default()
                }),
            )
        };
        let succeeded = crate::output::read_slice(&executed(true))
            .expect("parse stream")
            .into_inner();
        assert_eq!(reported(&succeeded.outcomes), 0);
        assert_eq!(succeeded.anchors.len(), 1);

        let failed = crate::output::read_slice(&executed(false))
            .expect("parse stream")
            .into_inner();
        assert_eq!(failed.outcomes.failed_actions().count(), 1);
        assert_eq!(failed.anchors.len(), 1);
    }

    #[test]
    fn an_aborted_target_is_skipped_only_when_the_stream_says_so() {
        let outcomes = outcomes(&[
            event(
                target_completed("//app:windows_only", BUILD_CONFIG),
                vec![],
                abort(
                    AbortReason::Skipped,
                    "Target //app:windows_only build was skipped.",
                ),
            ),
            event(
                target_completed("//app:release", BUILD_CONFIG),
                vec![],
                Payload::Aborted(Aborted::default()),
            ),
            event(
                target_completed("//app:package", BUILD_CONFIG),
                vec![],
                abort(AbortReason::Incomplete, ""),
            ),
            event(
                target_completed("//app:app", BUILD_CONFIG),
                vec![],
                abort(AbortReason::UserInterrupted, ""),
            ),
        ]);
        let build = |label| outcomes.build(at(label, Some(BUILD_CONFIG)));
        assert_eq!(build("//app:windows_only"), Some(&BuildOutcome::Skipped));
        assert_eq!(build("//app:release"), Some(&BuildOutcome::NotBuilt));
        assert_eq!(build("//app:package"), Some(&BuildOutcome::NotBuilt));
        assert_eq!(build("//app:app"), Some(&BuildOutcome::NotBuilt));
    }

    #[test]
    fn a_target_aborted_before_it_was_configured_is_not_built() {
        let outcomes = outcomes(&[
            event(
                target_configured("//app:interrupted"),
                vec![],
                abort(AbortReason::UserInterrupted, ""),
            ),
            event(
                target_configured("//app:unanalysed"),
                vec![],
                abort(AbortReason::NoAnalyze, ""),
            ),
        ]);
        for label in ["//app:interrupted", "//app:unanalysed"] {
            assert_eq!(
                outcomes.build(at(label, None)),
                Some(&BuildOutcome::NotBuilt),
                "{label}"
            );
        }
        assert_eq!(reported(&outcomes), 2);
    }

    #[test]
    fn a_failed_analysis_is_reported_of_the_target_and_described_by_its_root_cause() {
        const NOT_VISIBLE: &str = "in deps attribute of cc_binary rule //app:app: \
                                   target '//core:private' is not visible from target '//app:app'";
        let private = configured_label("//core:private", BUILD_CONFIG);
        let outcomes = outcomes(&[
            event(
                private.clone(),
                vec![],
                abort(AbortReason::AnalysisFailure, NOT_VISIBLE),
            ),
            event(
                target_configured("//app:app"),
                vec![private.clone()],
                abort(AbortReason::AnalysisFailure, ""),
            ),
            event(
                target_configured("//app:other"),
                vec![private],
                abort(AbortReason::AnalysisFailure, ""),
            ),
        ]);
        let cause = owned("//core:private", Some(BUILD_CONFIG));
        for label in ["//app:app", "//app:other"] {
            assert_eq!(
                outcomes.build(at(label, None)),
                Some(&BuildOutcome::AnalysisFailed {
                    causes: vec![Cause::Target(cause.clone())],
                }),
                "{label}"
            );
            assert_eq!(outcomes.build(at(label, Some(BUILD_CONFIG))), None);
        }
        assert_eq!(outcomes.target_error(cause.borrowed()), Some(NOT_VISIBLE));
        assert_eq!(outcomes.target_error(at("//core:private", None)), None);
        assert_eq!(reported(&outcomes), 2);
    }

    #[test]
    fn a_repository_that_could_not_be_fetched_is_described_under_its_label() {
        let outcomes = outcomes(&[event(
            unconfigured_label("//external:toolchains"),
            vec![],
            abort(AbortReason::LoadingFailure, "no such repository"),
        )]);
        assert_eq!(
            outcomes.target_error(at("//external:toolchains", None)),
            Some("no such repository")
        );
        assert_eq!(reported(&outcomes), 0);
    }

    #[test]
    fn a_root_cause_the_build_did_not_come_to_has_no_error() {
        let outcomes = outcomes(&[
            event(
                configured_label("//core:private", BUILD_CONFIG),
                vec![],
                abort(AbortReason::UserInterrupted, "Build interrupted"),
            ),
            event(
                unconfigured_label("//core:missing.h"),
                vec![],
                abort(AbortReason::Incomplete, ""),
            ),
            event(
                unconfigured_label("//core:undescribed.h"),
                vec![],
                abort(AbortReason::LoadingFailure, ""),
            ),
        ]);
        assert_eq!(
            outcomes.target_error(at("//core:private", Some(BUILD_CONFIG))),
            None
        );
        assert_eq!(outcomes.target_error(at("//core:missing.h", None)), None);
        assert_eq!(
            outcomes.target_error(at("//core:undescribed.h", None)),
            None
        );
    }

    #[test]
    fn an_action_conflict_fails_the_analysis_of_a_configured_target() {
        let cli = action_completed("bazel-out/bin/app/cli.o", "//app:cli", BUILD_CONFIG);
        let conflict = configured_label("//app:cli", BUILD_CONFIG);
        let outcomes = outcomes(&[
            event(
                target_completed("//app:conflicting", BUILD_CONFIG),
                vec![configured_label("//app:conflicting", BUILD_CONFIG)],
                abort(AbortReason::AnalysisFailure, ""),
            ),
            event(
                target_completed("//app:cli", BUILD_CONFIG),
                vec![cli],
                complete(false),
            ),
            event(
                aspect_completed("//app:cli", BUILD_CONFIG, LINT),
                vec![conflict],
                abort(AbortReason::AnalysisFailure, ""),
            ),
        ]);
        assert_eq!(
            outcomes.build(at("//app:conflicting", Some(BUILD_CONFIG))),
            Some(&BuildOutcome::AnalysisFailed {
                causes: vec![Cause::Target(owned(
                    "//app:conflicting",
                    Some(BUILD_CONFIG)
                ))],
            })
        );
        assert_eq!(
            outcomes.build(at("//app:cli", Some(BUILD_CONFIG))),
            Some(&BuildOutcome::AnalysisFailed {
                causes: vec![
                    Cause::Target(owned("//app:cli", Some(BUILD_CONFIG))),
                    action_cause("bazel-out/bin/app/cli.o"),
                ],
            })
        );
        assert_eq!(reported(&outcomes), 2);
    }

    #[test]
    fn the_null_configuration_is_no_configuration() {
        let outcomes = outcomes(&[
            event(
                target_completed("//base:version.txt", NULL_CONFIGURATION),
                vec![],
                complete(true),
            ),
            event(
                target_completed("//base:empty.txt", ""),
                vec![],
                complete(true),
            ),
            event(
                Id::TargetCompleted(TargetCompletedId {
                    label: "//base:absent.txt".into(),
                    ..Default::default()
                }),
                vec![],
                complete(true),
            ),
            event(
                action_completed(
                    "bazel-out/bin/base/gen.out",
                    "//base:gen",
                    NULL_CONFIGURATION,
                ),
                vec![],
                failed_action(ActionExecuted::default()),
            ),
            event(
                target_completed("//base:all", BUILD_CONFIG),
                vec![configured_label("//base:dep", NULL_CONFIGURATION)],
                complete(false),
            ),
            event(
                configured_label("//base:dep", NULL_CONFIGURATION),
                vec![],
                abort(AbortReason::AnalysisFailure, "no such target"),
            ),
        ]);
        for label in [
            "//base:version.txt",
            "//base:empty.txt",
            "//base:absent.txt",
        ] {
            assert_eq!(
                outcomes.build(at(label, None)),
                Some(&BuildOutcome::Built),
                "{label}"
            );
            assert_eq!(
                outcomes.build(at(label, Some(NULL_CONFIGURATION))),
                None,
                "{label}"
            );
        }
        let action = outcomes.failed_action("bazel-out/bin/base/gen.out");
        assert_eq!(
            action.and_then(|action| action.owner.clone()),
            Some(owned("//base:gen", None))
        );
        assert_eq!(
            outcomes.build(at("//base:all", Some(BUILD_CONFIG))),
            Some(&failed(vec![Cause::Target(owned("//base:dep", None))]))
        );
        assert_eq!(
            outcomes.target_error(at("//base:dep", None)),
            Some("no such target")
        );
    }

    #[test]
    fn an_aspect_that_fails_fails_its_target() {
        let lint = |aspect, primary_output: &str, success| {
            event(
                aspect_completed("//app:cli", BUILD_CONFIG, aspect),
                match success {
                    true => vec![],
                    false => vec![action_completed(primary_output, "//app:cli", BUILD_CONFIG)],
                },
                complete(success),
            )
        };
        let own = event(
            target_completed("//app:cli", BUILD_CONFIG),
            vec![],
            complete(true),
        );
        let outcomes = outcomes(&[
            lint(LINT, "bazel-out/bin/app/cli.lint", false),
            own,
            lint("//lint:defs.bzl%format", "", true),
            lint("//lint:defs.bzl%tidy", "bazel-out/bin/app/cli.tidy", false),
        ]);
        assert_eq!(
            outcomes.build(at("//app:cli", Some(BUILD_CONFIG))),
            Some(&failed(vec![
                action_cause("bazel-out/bin/app/cli.lint"),
                action_cause("bazel-out/bin/app/cli.tidy"),
            ]))
        );
        assert_eq!(reported(&outcomes), 1);
    }

    #[test]
    fn a_built_target_whose_aspect_was_cut_short_is_not_built() {
        let outcomes = outcomes(&[
            event(
                target_completed("//app:cli", BUILD_CONFIG),
                vec![],
                complete(true),
            ),
            event(
                aspect_completed("//app:cli", BUILD_CONFIG, LINT),
                vec![],
                abort(AbortReason::Incomplete, ""),
            ),
        ]);
        assert_eq!(
            outcomes.build(at("//app:cli", Some(BUILD_CONFIG))),
            Some(&BuildOutcome::NotBuilt)
        );
    }

    #[test]
    fn a_test_summary_counts_attempts_shards_and_cached_actions() {
        let stream = [
            event(
                test_summary_id("//tests:flaky", TEST_CONFIG),
                vec![],
                Payload::TestSummary(TestSummary {
                    overall_status: bes::TestStatus::Flaky.into(),
                    total_run_count: 2,
                    run_count: 1,
                    attempt_count: 2,
                    total_run_duration: Some(ProtoDuration {
                        seconds: 0,
                        nanos: 80_000_000,
                    }),
                    last_stop_time: Some(ANCHORED),
                    ..Default::default()
                }),
            ),
            event(
                test_summary_id("//tests:sharded", TEST_CONFIG),
                vec![],
                Payload::TestSummary(TestSummary {
                    overall_status: bes::TestStatus::Passed.into(),
                    total_run_count: 6,
                    run_count: 2,
                    attempt_count: 1,
                    shard_count: 3,
                    total_num_cached: 4,
                    total_run_duration: Some(ProtoDuration {
                        seconds: 2,
                        nanos: 745_000_000,
                    }),
                    ..Default::default()
                }),
            ),
        ]
        .concat();
        let output = crate::output::read_slice(&stream)
            .expect("parse stream")
            .into_inner();
        assert_eq!(output.anchors.len(), 1);
        assert_eq!(
            output.outcomes.test(at("//tests:flaky", Some(TEST_CONFIG))),
            Some(&TestOutcome {
                status: TestStatus::Flaky,
                attempts: 2,
                runs: 1,
                shards: None,
                actions: 2,
                cached_actions: 0,
                run_duration: Duration::from_millis(80),
            })
        );
        assert_eq!(
            output
                .outcomes
                .test(at("//tests:sharded", Some(TEST_CONFIG))),
            Some(&TestOutcome {
                status: TestStatus::Passed,
                attempts: 1,
                runs: 2,
                shards: NonZeroU32::new(3),
                actions: 6,
                cached_actions: 4,
                run_duration: Duration::from_millis(2_745),
            })
        );
        assert_eq!(output.outcomes.builds().count(), 0);
    }

    #[test]
    fn a_negative_count_or_duration_of_a_test_summary_reads_as_zero() {
        let outcomes = outcomes(&[event(
            test_summary_id("//tests:timeout", TEST_CONFIG),
            vec![],
            Payload::TestSummary(TestSummary {
                overall_status: bes::TestStatus::Timeout.into(),
                total_run_count: -1,
                run_count: -1,
                attempt_count: -1,
                shard_count: -1,
                total_num_cached: -1,
                total_run_duration: Some(ProtoDuration {
                    seconds: -3,
                    nanos: -38_000_000,
                }),
                ..Default::default()
            }),
        )]);
        assert_eq!(
            outcomes.test(at("//tests:timeout", Some(TEST_CONFIG))),
            Some(&TestOutcome {
                status: TestStatus::Timeout,
                attempts: 0,
                runs: 0,
                shards: None,
                actions: 0,
                cached_actions: 0,
                run_duration: Duration::ZERO,
            })
        );
    }
}
