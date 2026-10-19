//! The frame both panels of an execution log share: the log's placeholder
//! until it is read, then a toolbar over a cached layer of tables.

use crate::model::{ExecLogModel, Source};
use crate::palette::Palette;
use crate::summary::Summary;
use crate::table::GESTURES;
use bviz_ui_components::gesture::GestureRow;
use bviz_ui_components::{OneRow, cached_layer, cut_off_note};
use bviz_ui_workspace::{Panel, PanelEvent, PanelKind};
use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*,
};

/// What a panel shows of a loaded log, embedded in its [`SpawnPanel`] as a
/// cached layer so that a frame another panel of the window asks for replays
/// it: only a notify of the tables changes what they show.
pub trait Tables: Render {
    fn kind() -> PanelKind;

    /// The pointer gestures of the tables besides [`GESTURES`].
    fn gestures() -> Vec<GestureRow> {
        Vec::new()
    }

    /// The tables of `summary`, in a panel whose keys go where `focus` stands.
    fn new(summary: Summary, focus: &FocusHandle, cx: &mut Context<Self>) -> Self;

    /// The toolbar over `tables`, which a choice on it notifies.
    fn toolbar(tables: &Entity<Self>, focus: &FocusHandle, window: &Window, cx: &App) -> OneRow;

    /// `panel` answering the actions of its tables, `tables`.
    fn on_actions(panel: Div, _tables: &Entity<Self>) -> Div {
        panel
    }
}

/// A panel of an execution log, its content being `T`'s.
pub struct SpawnPanel<T> {
    focus: FocusHandle,
    model: Entity<ExecLogModel>,
    /// `None` until the log is loaded, the model's placeholder showing instead.
    tables: Option<Entity<T>>,
    _model_observer: Subscription,
}

impl<T: Tables> SpawnPanel<T> {
    pub(crate) fn new(model: Entity<ExecLogModel>, cx: &mut Context<Self>) -> Self {
        let model_observer = cx.observe(&model, |this, _, cx| {
            this.show_loaded(cx);
            cx.notify();
        });
        let mut panel = Self {
            focus: cx.focus_handle(),
            model,
            tables: None,
            _model_observer: model_observer,
        };
        panel.show_loaded(cx);
        panel
    }

    fn show_loaded(&mut self, cx: &mut Context<Self>) {
        if self.tables.is_some() {
            return;
        }
        let Source::Loaded(summary) = self.model.read(cx).source() else {
            return;
        };
        let summary = summary.get().clone();
        let focus = self.focus.clone();
        self.tables = Some(cx.new(|cx| T::new(summary, &focus, cx)));
    }
}

impl<T: Tables> EventEmitter<PanelEvent> for SpawnPanel<T> {}

impl<T: Tables> Focusable for SpawnPanel<T> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl<T: Tables> Panel for SpawnPanel<T> {
    fn kind() -> PanelKind {
        T::kind()
    }

    fn gestures() -> Vec<GestureRow> {
        GESTURES.rows().chain(T::gestures()).collect()
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        let status = self
            .model
            .read(cx)
            .source()
            .status("No execution log", |summary| {
                format!("{} spawns", summary.get().spawns)
            });
        Some(status.into())
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.focus.focus(window, cx);
        }
    }
}

impl<T: Tables> Render for SpawnPanel<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let panel = div()
            .key_context(T::kind().key_context)
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(palette.colors.background)
            .text_color(palette.colors.text)
            .text_sm();
        let Some(tables) = &self.tables else {
            return panel.children(self.model.read(cx).source().placeholder(&palette.colors));
        };
        T::on_actions(panel, tables)
            .children(
                self.model
                    .read(cx)
                    .cut_off()
                    .then(|| cut_off_note("execution log", "spawns", &palette.colors)),
            )
            .child(T::toolbar(tables, &self.focus, window, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(cached_layer(tables.clone())),
            )
    }
}
