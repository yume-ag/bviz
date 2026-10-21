use crate::summary::Summary;
use bviz_execlog::{ExecLog, UnmeasuredLog};
use bviz_input::Reopener;
use bviz_invocation::spawn::{LoggedSpawns, MeasuredInputs};
use bviz_invocation::{InvocationId, Recorded, Session};
use bviz_parallel::{StopOnDrop, StopSignal, stop_on_drop};
use bviz_ui_components::{Pending, load_in_background};
use bviz_ui_playback::{Alignment, BecameJoined, Clock};
use bviz_ui_session::{Reports, Sharing};
use gpui::{App, Context, Entity, Subscription, Task};
use std::sync::Arc;

pub(crate) type Source = bviz_ui_components::Source<Recorded<Summary>>;

/// What reading an execution log yields.
pub(crate) struct Read {
    /// Of `log`, the bytes its spawns read unknown.
    pub(crate) summary: Recorded<Summary>,
    pub(crate) log: Recorded<UnmeasuredLog>,
    /// `None` for a log read from a stream, and for one naming no invocation.
    pub(crate) reread: Option<Reread>,
}

/// How to read a log again: what opens it afresh, and the invocation it
/// named when first read, which the log opened again must name to be the
/// one the document was joined by.
pub(crate) struct Reread {
    pub(crate) reopener: Reopener,
    pub(crate) invocation: InvocationId,
}

/// How a log alone in its document, which another output of its invocation
/// may join, comes by its spawns for the session once that file joins. Its
/// spawns are many times the summary's size, and nothing reads them before
/// then, so it keeps them only where it cannot read them again.
enum Awaiting {
    /// What its spawns read is kept, a measure per spawn, to be taken up
    /// again by the spawns read afresh.
    Reread {
        reread: Reread,
        inputs: Option<Arc<MeasuredInputs>>,
    },
    /// The spawns of a log read from a stream, which yields its bytes once.
    Kept(LoggedSpawns),
}

/// Where a log's spawns stand between its read and the session.
enum Spawns {
    /// Their inputs being measured, these spawns reported meanwhile should
    /// the document be joined.
    Measuring(LoggedSpawns),
    /// Measured, for a report once the document is joined.
    Awaiting(Awaiting),
    /// Reported, or never to be.
    Settled,
}

/// What becomes of a log's spawns once a document is joined.
enum Joined {
    Report(LoggedSpawns),
    Reread {
        reread: Reread,
        inputs: Option<Arc<MeasuredInputs>>,
    },
    Nothing,
}

impl Spawns {
    /// Takes up `spawns`, measured, returning those to report: all of them
    /// where the document is `joined`, and otherwise none, kept for a join
    /// where `sharing` lets one come.
    fn measured(
        &mut self,
        spawns: LoggedSpawns,
        reread: Option<Reread>,
        joined: bool,
        sharing: Sharing,
    ) -> Option<LoggedSpawns> {
        *self = Spawns::Settled;
        if joined {
            return Some(spawns);
        }
        if sharing == Sharing::Invocation {
            *self = Spawns::Awaiting(match reread {
                Some(reread) => Awaiting::Reread {
                    reread,
                    inputs: spawns.measured_inputs().cloned(),
                },
                None => Awaiting::Kept(spawns),
            });
        }
        None
    }

    /// What the document being joined asks of the spawns.
    fn joined(&mut self) -> Joined {
        if let Spawns::Measuring(unmeasured) = self {
            return Joined::Report(unmeasured.clone());
        }
        match std::mem::replace(self, Spawns::Settled) {
            Spawns::Awaiting(Awaiting::Kept(spawns)) => Joined::Report(spawns),
            Spawns::Awaiting(Awaiting::Reread { reread, inputs }) => {
                Joined::Reread { reread, inputs }
            }
            Spawns::Measuring(_) | Spawns::Settled => Joined::Nothing,
        }
    }
}

/// What measuring a log's spawns yields.
struct Measured {
    summary: Recorded<Summary>,
    spawns: LoggedSpawns,
    reread: Option<Reread>,
}

/// Work off the UI thread on a log's spawns, stopping once dropped.
struct Background {
    _wanted: StopOnDrop,
    _task: Task<()>,
}

/// The summary of an execution log, held apart from the panel showing it so
/// the panel can close and be shown again without the log being read afresh.
pub(crate) struct ExecLogModel {
    source: Source,
    /// Tells the log's spawns to its document's session.
    reports: Reports,
    clock: Entity<Clock>,
    sharing: Sharing,
    spawns: Spawns,
    /// The measure of the log's spawns' inputs.
    _measuring: Option<Background>,
    /// The second read of a log whose document was joined after the first.
    _rereading: Option<Background>,
    /// The read that fills `source`, held so that it is not cancelled.
    _load: Task<()>,
    _joined: Subscription,
}

impl ExecLogModel {
    /// The summary of the log `log` reads, its spawns reported to `session`
    /// once `clock` is joined, where `sharing` lets another file read them.
    pub(crate) fn reading(
        log: Pending<Read>,
        session: Entity<Session>,
        clock: Entity<Clock>,
        sharing: Sharing,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            source: Source::Loading,
            reports: Reports::new(session, cx),
            _joined: cx.subscribe(&clock, |model, _, _: &BecameJoined, cx| {
                model.report_awaited(cx)
            }),
            clock,
            sharing,
            spawns: Spawns::Settled,
            _measuring: None,
            _rereading: None,
            _load: load_in_background(
                cx,
                move || log.join(),
                move |model, result, cx| {
                    model.source = match result {
                        Ok(Read {
                            summary,
                            log,
                            reread,
                        }) => {
                            model.measure(log, reread, cx);
                            Source::Loaded(summary)
                        }
                        Err(why) => Source::Failed(why),
                    };
                },
            ),
        }
    }

    fn joined(&self, cx: &App) -> bool {
        self.clock.read(cx).alignment() == Alignment::Joined
    }

    fn report(&self, spawns: LoggedSpawns, cx: &mut App) {
        self.reports.report(cx, |session, by, cx| {
            session.report_logged_spawns(by, spawns);
            cx.notify();
        });
    }

    /// Reports `log`'s spawns where the document is joined, then measures
    /// what they read off the UI thread, the summary and the session taking
    /// up the measure.
    fn measure(
        &mut self,
        log: Recorded<UnmeasuredLog>,
        reread: Option<Reread>,
        cx: &mut Context<Self>,
    ) {
        let unmeasured = log.get().log().logged_spawns().clone();
        if self.joined(cx) {
            self.report(unmeasured.clone(), cx);
        }
        self.spawns = Spawns::Measuring(unmeasured);
        let (wanted, stop) = stop_on_drop();
        let task = load_in_background(
            cx,
            move || {
                let read = log.as_ref().map(|_| ());
                let log = log.into_inner().measure_inputs(&stop)?;
                Some(Measured {
                    summary: read.map(|()| Summary::of(&log)),
                    spawns: log.into_spawns(),
                    reread,
                })
            },
            |model, measured, cx| {
                if let Some(measured) = measured {
                    model.take_up_measured(measured, cx);
                }
            },
        );
        self._measuring = Some(Background {
            _wanted: wanted,
            _task: task,
        });
    }

    fn take_up_measured(&mut self, measured: Measured, cx: &mut App) {
        let Measured {
            summary,
            spawns,
            reread,
        } = measured;
        self.source = Source::Loaded(summary);
        let joined = self.joined(cx);
        if let Some(spawns) = self.spawns.measured(spawns, reread, joined, self.sharing) {
            self.report(spawns, cx);
        }
    }

    /// Reports the spawns of a log whose document was just joined, reading
    /// the log again where it kept none.
    fn report_awaited(&mut self, cx: &mut Context<Self>) {
        match self.spawns.joined() {
            Joined::Nothing => {}
            Joined::Report(spawns) => self.report(spawns, cx),
            Joined::Reread { reread, inputs } => {
                let (wanted, stop) = stop_on_drop();
                let task = load_in_background(
                    cx,
                    move || spawns_of(&reread, inputs, &stop),
                    |model, spawns, cx| {
                        if let Some(spawns) = spawns {
                            model.report(spawns, cx);
                        }
                    },
                );
                self._rereading = Some(Background {
                    _wanted: wanted,
                    _task: task,
                });
            }
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    pub(crate) fn cut_off(&self) -> bool {
        self.source.is_cut_off()
    }
}

/// The spawns of the log `reread` opens again, having read what `inputs`
/// measured of the first read where they are as many, and measured afresh
/// otherwise; `None` where the log no longer reads or names another
/// invocation, as a file written over since does, or `stop` is raised.
fn spawns_of(
    reread: &Reread,
    inputs: Option<Arc<MeasuredInputs>>,
    stop: &StopSignal,
) -> Option<LoggedSpawns> {
    let log = log_of(reread)?;
    let kept = inputs.and_then(|inputs| log.log().logged_spawns().measured(inputs));
    match kept {
        Some(spawns) => Some(spawns),
        None => Some(log.measure_inputs(stop)?.into_spawns()),
    }
}

/// The log `reread` opens again, `None` where it no longer reads or names
/// another invocation.
fn log_of(reread: &Reread) -> Option<UnmeasuredLog> {
    let input = reread.reopener.open().ok()?;
    if crate::invocation_id(&input).as_ref() != Some(&reread.invocation) {
        return None;
    }
    Some(ExecLog::from_input(input).ok()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_input::Input;
    use bviz_invocation::spawn::{Builder, InputBytes, Logged, PhaseTiming};

    /// A file at `name` that no execution log is written to.
    fn written_over(name: &str) -> Reread {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(name);
        std::fs::write(&path, b"no execution log").unwrap();
        let input = Input::open(&path).expect("the file opens");
        let (_, reopener) = input.reopenable().ok().expect("a file reopens");
        Reread {
            reopener,
            invocation: InvocationId::new("a5f7f3e6").expect("a non-empty id"),
        }
    }

    /// A file that no longer names the invocation its document was joined by
    /// yields no spawns to report.
    #[test]
    fn a_log_read_again_naming_another_invocation_yields_nothing() {
        let reread = written_over("written_over.execlog.zst");
        assert!(log_of(&reread).is_none());
        let (_wanted, stop) = stop_on_drop();
        assert!(spawns_of(&reread, None, &stop).is_none());
    }

    /// Two spawns, the bytes they read unmeasured.
    fn unmeasured() -> LoggedSpawns {
        let mut builder = Builder::with_capacity(2, 0, 0);
        for mnemonic in ["A", "B"] {
            builder.push(Logged {
                runner: "remote",
                mnemonic,
                target_label: "",
                cache_hit: false,
                timing: PhaseTiming::default(),
            });
        }
        builder.finish()
    }

    fn measured(unmeasured: &LoggedSpawns) -> LoggedSpawns {
        let read = Arc::new(MeasuredInputs::new(vec![InputBytes::default(); 2]));
        unmeasured.measured(read).expect("one measure per spawn")
    }

    #[test]
    fn spawns_measured_once_joined_are_reported_and_settle() {
        let unmeasured = unmeasured();
        let mut spawns = Spawns::Measuring(unmeasured.clone());
        let reported = spawns.measured(measured(&unmeasured), None, true, Sharing::Invocation);
        assert!(reported.is_some_and(|spawns| spawns.inputs_measured()));
        assert!(matches!(spawns, Spawns::Settled));
        assert!(matches!(spawns.joined(), Joined::Nothing));
    }

    #[test]
    fn a_join_while_measuring_reports_the_spawns_unmeasured_then_measured() {
        let unmeasured = unmeasured();
        let mut spawns = Spawns::Measuring(unmeasured.clone());
        let Joined::Report(first) = spawns.joined() else {
            panic!("a join while measuring reports the spawns");
        };
        assert!(!first.inputs_measured());
        assert!(matches!(spawns, Spawns::Measuring(_)));
        let reported = spawns.measured(measured(&unmeasured), None, true, Sharing::Invocation);
        assert!(reported.is_some_and(|spawns| spawns.inputs_measured()));
    }

    #[test]
    fn a_lone_log_keeps_what_its_spawns_read_for_a_join_that_reads_it_again() {
        let unmeasured = unmeasured();
        let mut spawns = Spawns::Measuring(unmeasured.clone());
        let reread = written_over("kept_inputs.execlog.zst");
        let reported = spawns.measured(
            measured(&unmeasured),
            Some(reread),
            false,
            Sharing::Invocation,
        );
        assert!(reported.is_none());
        let Joined::Reread { inputs, .. } = spawns.joined() else {
            panic!("a join reads the log again");
        };
        let inputs = inputs.expect("the measure is kept");
        assert!(
            unmeasured.measured(inputs).is_some(),
            "the read afresh takes it up"
        );
        assert!(matches!(spawns, Spawns::Settled));
    }

    #[test]
    fn a_lone_log_read_from_a_stream_keeps_its_spawns_for_a_join() {
        let unmeasured = unmeasured();
        let mut spawns = Spawns::Measuring(unmeasured.clone());
        let reported = spawns.measured(measured(&unmeasured), None, false, Sharing::Invocation);
        assert!(reported.is_none());
        let Joined::Report(kept) = spawns.joined() else {
            panic!("a join reports the spawns kept");
        };
        assert!(kept.inputs_measured());
    }

    #[test]
    fn a_log_no_other_file_joins_keeps_nothing() {
        let unmeasured = unmeasured();
        let mut spawns = Spawns::Measuring(unmeasured.clone());
        let reported = spawns.measured(measured(&unmeasured), None, false, Sharing::Alone);
        assert!(reported.is_none());
        assert!(matches!(spawns, Spawns::Settled));
    }
}
