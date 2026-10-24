use crate::spinner;
use bviz_invocation::Recorded;
use bviz_ui_theme::ThemeColors;
use gpui::{Div, Pixels, SharedString, div, prelude::*, px};

/// How a load in flight is labelled on screen.
const LOADING: &str = "Loading…";

/// Size of the text a [`note_strip`] is set in.
const NOTE_TEXT_SIZE: Pixels = px(12.);

/// The load state of the document a view renders.
pub enum Source<T> {
    Loading,
    Loaded(T),
    /// Why the document could not be loaded, worded for the reader.
    Failed(String),
}

impl<T> Source<T> {
    /// What the view draws in place of the document until one is loaded, and
    /// `None` once one is.
    pub fn placeholder(&self, colors: &ThemeColors) -> Option<Div> {
        match self {
            Self::Loading => Some(centered_spinner(LOADING, colors)),
            Self::Loaded(_) => None,
            Self::Failed(why) => Some(centered_notice(why.clone(), colors)),
        }
    }

    /// The status line for this state: the load label while one is in flight,
    /// `loaded` describing the document once one is loaded, and `absent` naming
    /// what the reader does not have once the load has failed.
    pub fn status(&self, absent: &str, loaded: impl FnOnce(&T) -> String) -> String {
        match self {
            Self::Loading => LOADING.into(),
            Self::Loaded(document) => loaded(document),
            Self::Failed(_) => absent.into(),
        }
    }
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
