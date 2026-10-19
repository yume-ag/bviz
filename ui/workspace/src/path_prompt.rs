//! The window's own dialog asking for the paths of files to open, for a
//! platform that offers no file chooser of its own.

use bviz_ui_components::dialog::{self, Accept, Cancel};
use bviz_ui_components::{Button, TextInput};
use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window, div, prelude::*,
};
use std::path::PathBuf;

/// What a [`PathPrompt`] tells its holder.
pub(crate) enum PathPromptEvent {
    /// The user named these paths, at least one, to open.
    Chosen(Vec<PathBuf>),
    Cancelled,
}

impl EventEmitter<PathPromptEvent> for PathPrompt {}

/// What the dialog says of how its field takes several paths.
const HOW_TO_NAME: &str = "Give each file's full path, separated by spaces, and quote a path \
     holding spaces as a shell would.";

pub(crate) struct PathPrompt {
    field: Entity<TextInput>,
    /// Why the paths last offered could not be taken.
    problem: Option<SharedString>,
}

impl PathPrompt {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            field: cx.new(|cx| TextInput::new(cx, "Paths of files to open", 0)),
            problem: None,
        }
    }

    fn accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match paths(&self.field.read(cx).content()) {
            Ok(paths) => cx.emit(PathPromptEvent::Chosen(paths)),
            Err(problem) => {
                self.problem = Some(problem.into());
                self.field.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        }
    }
}

/// The paths `typed` names, split as a POSIX shell splits words: at unquoted
/// white space, with quotes and backslashes keeping a path that holds spaces
/// whole.
fn paths(typed: &str) -> Result<Vec<PathBuf>, &'static str> {
    let words = shlex::split(typed).ok_or("A quote is left open.")?;
    if words.is_empty() {
        return Err("Name a file to open.");
    }
    Ok(words.into_iter().map(PathBuf::from).collect())
}

impl Focusable for PathPrompt {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.field.focus_handle(cx)
    }
}

impl Render for PathPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let field = self.field.focus_handle(cx);
        let open = Button::new(
            "path-prompt-open",
            cx.listener(|prompt, _, window, cx| prompt.accept(window, cx)),
        )
        .label("Open")
        .keys_for(&Accept, &field);
        let cancel = Button::new(
            "path-prompt-cancel",
            cx.listener(|_, _, _, cx| cx.emit(PathPromptEvent::Cancelled)),
        )
        .ghost()
        .label("Cancel")
        .keys_for(&Cancel, &field);
        dialog::sheet("Open files", &colors)
            .on_action(cx.listener(|prompt, _: &Accept, window, cx| prompt.accept(window, cx)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(PathPromptEvent::Cancelled)))
            .child(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(HOW_TO_NAME),
            )
            .child(self.field.clone())
            .children(
                self.problem
                    .clone()
                    .map(|problem| div().text_sm().text_color(colors.error).child(problem)),
            )
            .child(dialog::buttons([cancel, open]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_at_white_space_outside_quotes() {
        assert_eq!(
            paths("a.profile.gz  'my build/b.bep'\t\"c d\\\".zst\" e\\ f").unwrap(),
            [
                PathBuf::from("a.profile.gz"),
                PathBuf::from("my build/b.bep"),
                PathBuf::from("c d\".zst"),
                PathBuf::from("e f"),
            ]
        );
    }

    #[test]
    fn an_open_quote_or_no_path_is_a_problem_to_show() {
        assert!(paths("'a.bep").is_err());
        assert!(paths("   ").is_err());
    }
}
