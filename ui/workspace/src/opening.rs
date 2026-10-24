//! Files named to a window together, as the window shows them from when they
//! are named until their views stand.

use crate::named_label;
use bviz_input::Named;
use gpui::SharedString;
use std::fmt::Display;

/// A file of an [`Opening`].
pub(crate) struct OpeningFile {
    pub(crate) label: SharedString,
    pub(crate) state: FileState,
}

pub(crate) enum FileState {
    /// Handed to the app, which is telling what it holds and opening its views.
    Opening,
    /// Why it does not open.
    Failed(SharedString),
}

/// A file handed to the app that did not open, and why.
pub struct Failure {
    at: usize,
    reason: SharedString,
}

impl Failure {
    /// The file at `at` among those handed to the app did not open, for
    /// `reason`.
    pub fn new(at: usize, reason: impl Display) -> Self {
        Self {
            at,
            reason: reason.to_string().into(),
        }
    }
}

/// A file of a settled [`Opening`] that did not open, and why.
struct Unopenable {
    label: SharedString,
    reason: SharedString,
}

/// Files named to a window together, some still on their way to their views.
pub(crate) struct Opening(Vec<OpeningFile>);

/// The files of an [`Opening`] that did not open once it settled, at least
/// one.
pub(crate) struct Unopened(Vec<Unopenable>);

/// How a line shows its file's state.
pub(crate) enum Shown<'a> {
    /// Work under way with no progress to report, which the line marks as
    /// busy so it does not read as stalled.
    Busy(&'static str),
    Error(&'a SharedString),
}

/// What a file still being opened shows in place of a measure of progress,
/// which the app's opening does not report.
const OPENING: &str = "opening";

/// Why a file the app was handed but never settled did not open.
const LOST: &str = "did not finish opening";

impl Opening {
    /// `named`, each handed to the app to open.
    pub(crate) fn of<'a>(named: impl IntoIterator<Item = &'a Named>) -> Self {
        Self(
            (named.into_iter())
                .map(|named| OpeningFile {
                    label: named_label(named),
                    state: FileState::Opening,
                })
                .collect(),
        )
    }

    #[cfg(any(test, target_family = "wasm"))]
    pub(crate) fn new(files: Vec<OpeningFile>) -> Self {
        Self(files)
    }

    /// What is left to show once the app has opened what it could, with
    /// `failures` it did not, each on the line of the file it names among
    /// those still being opened; `None` where every file opened.
    pub(crate) fn settle(mut self, failures: Vec<Failure>) -> Option<Unopened> {
        let opening: Vec<usize> = (self.0.iter().enumerate())
            .filter(|(_, file)| matches!(file.state, FileState::Opening))
            .map(|(line, _)| line)
            .collect();
        for Failure { at, reason } in failures {
            if let Some(&line) = opening.get(at) {
                self.0[line].state = FileState::Failed(reason);
            }
        }
        self.unopened()
    }

    /// What is left to show where the app never settles the opening: every
    /// file still on its way failed.
    pub(crate) fn lose(mut self) -> Option<Unopened> {
        for file in &mut self.0 {
            if let FileState::Opening = file.state {
                file.state = FileState::Failed(LOST.into());
            }
        }
        self.unopened()
    }

    fn unopened(self) -> Option<Unopened> {
        let failed: Vec<Unopenable> = (self.0.into_iter())
            .filter_map(|file| match file.state {
                FileState::Failed(reason) => Some(Unopenable {
                    label: file.label,
                    reason,
                }),
                FileState::Opening => None,
            })
            .collect();
        (!failed.is_empty()).then_some(Unopened(failed))
    }

    pub(crate) fn heading(&self) -> String {
        format!("Opening {}…", files_counted(self.0.len()))
    }

    /// Each file's label and state, in the order the files were named.
    pub(crate) fn lines(&self) -> Vec<(&SharedString, Shown<'_>)> {
        (self.0.iter())
            .map(|file| (&file.label, file.state.shown()))
            .collect()
    }
}

impl Unopened {
    pub(crate) fn heading(&self) -> String {
        format!("Cannot open {}", files_counted(self.0.len()))
    }

    /// Each file's label and why it did not open, in the order the files
    /// were named.
    pub(crate) fn lines(&self) -> Vec<(&SharedString, Shown<'_>)> {
        (self.0.iter())
            .map(|file| (&file.label, Shown::Error(&file.reason)))
            .collect()
    }
}

impl FileState {
    fn shown(&self) -> Shown<'_> {
        match self {
            Self::Opening => Shown::Busy(OPENING),
            Self::Failed(reason) => Shown::Error(reason),
        }
    }
}

/// `count` files, as a heading counts them.
fn files_counted(count: usize) -> String {
    match count {
        1 => "1 file".to_owned(),
        count => format!("{count} files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path(name: &str) -> Named {
        Named::Path(PathBuf::from("/out").join(name))
    }

    fn failed(label: &str, reason: &str) -> OpeningFile {
        OpeningFile {
            label: label.to_owned().into(),
            state: FileState::Failed(reason.to_owned().into()),
        }
    }

    fn texts(lines: Vec<(&SharedString, Shown<'_>)>) -> Vec<(String, String)> {
        (lines.into_iter())
            .map(|(label, shown)| {
                let shown = match shown {
                    Shown::Busy(text) => text.to_owned(),
                    Shown::Error(reason) => format!("error: {reason}"),
                };
                (label.to_string(), shown)
            })
            .collect()
    }

    #[test]
    fn a_heading_counts_one_file_in_the_singular() {
        assert_eq!(Opening::of(&[path("a.bep")]).heading(), "Opening 1 file…");
        let three = [path("a.bep"), path("b.pb"), path("c.profile.gz")];
        assert_eq!(Opening::of(&three).heading(), "Opening 3 files…");
    }

    #[test]
    fn a_file_being_opened_reads_by_its_name_with_no_figure() {
        let opening = Opening::of(&[path("build.bep")]);
        assert_eq!(
            texts(opening.lines()),
            [("build.bep".into(), "opening".into())]
        );
    }

    #[test]
    fn an_opening_every_file_of_which_opens_settles_to_nothing() {
        let opening = Opening::of(&[path("a.bep"), path("b.pb")]);
        assert!(opening.settle(Vec::new()).is_none());
    }

    #[test]
    fn a_settled_opening_keeps_only_its_failures_in_the_order_named() {
        let opening = Opening::new(vec![
            OpeningFile {
                label: "a.bep".into(),
                state: FileState::Opening,
            },
            failed("b.pb", "unreadable"),
            OpeningFile {
                label: "c.log".into(),
                state: FileState::Opening,
            },
        ]);
        let unopened = opening
            .settle(vec![Failure::new(0, "no such file")])
            .expect("two files failed");
        assert_eq!(unopened.heading(), "Cannot open 2 files");
        assert_eq!(
            texts(unopened.lines()),
            [
                ("a.bep".into(), "error: no such file".into()),
                ("b.pb".into(), "error: unreadable".into()),
            ]
        );
    }

    #[test]
    fn a_failure_lands_on_the_line_of_the_file_at_its_place_among_those_opening() {
        let opening = Opening::new(vec![
            failed("a.pb", "unreadable"),
            OpeningFile {
                label: "b.pb".into(),
                state: FileState::Opening,
            },
            OpeningFile {
                label: "c.pb".into(),
                state: FileState::Opening,
            },
        ]);
        let unopened = opening
            .settle(vec![Failure::new(1, "no such file")])
            .expect("two files failed");
        assert_eq!(
            texts(unopened.lines()),
            [
                ("a.pb".into(), "error: unreadable".into()),
                ("c.pb".into(), "error: no such file".into()),
            ]
        );
    }

    #[test]
    fn failures_of_files_sharing_a_label_keep_the_order_of_the_files() {
        let opening = Opening::of(&[path("a.pb"), path("a.pb")]);
        let failures = vec![Failure::new(1, "second"), Failure::new(0, "first")];
        let unopened = opening.settle(failures).expect("both failed");
        assert_eq!(
            texts(unopened.lines()),
            [
                ("a.pb".into(), "error: first".into()),
                ("a.pb".into(), "error: second".into()),
            ]
        );
    }

    #[test]
    fn a_lost_opening_fails_every_file_still_on_its_way() {
        let opening = Opening::new(vec![
            failed("a.bep", "unreadable"),
            OpeningFile {
                label: "b.pb".into(),
                state: FileState::Opening,
            },
        ]);
        let unopened = opening.lose().expect("both failed");
        assert_eq!(
            texts(unopened.lines()),
            [
                ("a.bep".into(), "error: unreadable".into()),
                ("b.pb".into(), "error: did not finish opening".into()),
            ]
        );
    }
}
