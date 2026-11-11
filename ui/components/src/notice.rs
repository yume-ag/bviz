use crate::spinner;
use bviz_input::ReadProgress;
use bviz_invocation::Recorded;
use bviz_ui_theme::ThemeColors;
use gpui::{Div, Pixels, SharedString, div, prelude::*, px};

/// How a load in flight is labelled on screen.
const LOADING: &str = "Loading…";

/// Size of the text a [`note_strip`] is set in.
pub const NOTE_TEXT_SIZE: Pixels = px(12.);

/// The load state of the document a view renders.
pub enum Source<T> {
    /// Being loaded, with how far its read has got where the load counts it.
    Loading(Option<ReadProgress>),
    Loaded(T),
    /// Why the document could not be loaded, worded for the reader.
    Failed(String),
}

impl<T> Source<T> {
    /// What the view draws in place of the document until one is loaded, and
    /// `None` once one is.
    pub fn placeholder(&self, colors: &ThemeColors) -> Option<Div> {
        match self {
            Self::Loading(progress) => Some(centered_spinner(loading(progress), colors)),
            Self::Loaded(_) => None,
            Self::Failed(why) => Some(centered_notice(why.clone(), colors)),
        }
    }

    /// The status line for this state: the load label while one is in flight,
    /// `loaded` describing the document once one is loaded, and `absent` naming
    /// what the reader does not have once the load has failed.
    pub fn status(&self, absent: &str, loaded: impl FnOnce(&T) -> String) -> String {
        match self {
            Self::Loading(progress) => loading(progress),
            Self::Loaded(document) => loaded(document),
            Self::Failed(_) => absent.into(),
        }
    }
}

/// The label of a load in flight, telling how far its read has got until the
/// read is done and only the work on what it read remains.
fn loading(progress: &Option<ReadProgress>) -> String {
    match progress {
        Some(progress) if !progress.is_done() => format!(
            "{LOADING} {}",
            byte_progress(progress.read(), progress.total())
        ),
        _ => LOADING.into(),
    }
}

/// The units a count of bytes reads in, smallest first, each this many of the
/// one before, as file choosers count them.
const BYTE_UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
const BYTE_UNIT_STEP: f64 = 1000.0;

/// `read` of `total` bytes, both in the unit `total` reads best in.
pub fn byte_progress(read: u64, total: u64) -> String {
    let mut scale = BYTE_UNIT_STEP;
    let mut unit = BYTE_UNITS[0];
    for larger in &BYTE_UNITS[1..] {
        if (total as f64) < scale * BYTE_UNIT_STEP {
            break;
        }
        scale *= BYTE_UNIT_STEP;
        unit = larger;
    }
    format!(
        "{:.1} of {:.1} {unit}",
        read as f64 / scale,
        total as f64 / scale
    )
}

impl<T> Source<Recorded<T>> {
    /// Whether the document loaded is of a file that ends early.
    pub fn is_cut_off(&self) -> bool {
        matches!(self, Self::Loaded(document) if document.is_cut_off())
    }
}

/// Shown while work is in flight.
pub fn centered_spinner(label: impl Into<SharedString>, colors: &ThemeColors) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap_3()
        .child(spinner("spinner", px(36.), colors.text))
        .child(
            div()
                .text_sm()
                .text_color(colors.text_muted)
                .child(label.into()),
        )
}

/// Shown in place of a document that could not be loaded, as the body of the view
/// the document would have filled.
pub fn centered_notice(message: impl Into<SharedString>, colors: &ThemeColors) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap_4()
        .p_4()
        .child(
            div()
                .max_w(px(400.))
                .text_sm()
                .text_color(colors.text)
                .child(message.into()),
        )
}

/// A strip above the document a view shows, telling the reader something of
/// the document to bear in mind while reading it.
pub fn note_strip(note: impl Into<SharedString>, colors: &ThemeColors) -> Div {
    note_frame(colors).child(note.into())
}

/// The strip a [`note_strip`] is, holding nothing yet, for a note that is
/// more than a line of text.
pub fn note_frame(colors: &ThemeColors) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_1()
        .bg(colors.surface)
        .border_b_1()
        .border_color(colors.border)
        .font_family(bviz_ui_assets::UI)
        .text_size(NOTE_TEXT_SIZE)
        .text_color(colors.text_muted)
}

/// Says the `file` a view shows ends early, as an invocation interrupted or
/// still running leaves it, and the view shows the `records` ahead of the cut.
pub fn cut_off_note(file: &str, records: &str, colors: &ThemeColors) -> Div {
    note_strip(
        format!("This {file} ends early; showing the {records} before the cut."),
        colors,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_input::Counted;
    use std::io::Read;

    const MB: u64 = 1_000_000;

    #[test]
    fn progress_reads_in_the_unit_of_the_whole() {
        assert_eq!(byte_progress(12 * MB, 48 * MB), "12.0 of 48.0 MB");
        assert_eq!(byte_progress(500, 1000), "0.5 of 1.0 KB");
        assert_eq!(byte_progress(MB - 1000, MB - 1000), "999.0 of 999.0 KB");
        assert_eq!(byte_progress(1000 * MB, 5000 * MB), "1.0 of 5.0 GB");
        assert_eq!(byte_progress(0, 0), "0.0 of 0.0 KB");
    }

    #[test]
    fn a_load_tells_how_far_its_read_has_got_until_it_is_done() {
        let progress = ReadProgress::new(2000);
        let source = Source::<()>::Loading(Some(progress.clone()));
        let status = |source: &Source<()>| source.status("", |_| String::new());
        assert_eq!(status(&source), "Loading… 0.0 of 2.0 KB");
        let mut read = Counted::new(&[0u8; 2000][..], &progress);
        std::io::copy(&mut (&mut read).take(500), &mut std::io::sink()).unwrap();
        assert_eq!(status(&source), "Loading… 0.5 of 2.0 KB");
        std::io::copy(&mut read, &mut std::io::sink()).unwrap();
        assert_eq!(status(&source), "Loading…");
        assert_eq!(status(&Source::Loading(None)), "Loading…");
    }
}
