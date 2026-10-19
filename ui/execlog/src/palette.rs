//! The colours the execution log panels paint with, resolved from the theme.

use bviz_execlog::Phase;
use bviz_ui_components::ScrollbarColors;
use bviz_ui_spawn_phase::phase_fill;
use bviz_ui_theme::{Theme, ThemeColors};
use gpui::Hsla;

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) colors: ThemeColors,
    phases: [Hsla; Phase::ALL.len()],
    /// The fill of a bar of execution alone, outlined in the execution fill: a
    /// neutral wash, which no phase's fill can be taken for.
    pub(crate) unbroken: Hsla,
    pub(crate) scroll: ScrollbarColors,
}

impl Palette {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        let phases = Phase::ALL.map(|phase| phase_fill(phase).resolve(theme));
        Self {
            colors: theme.colors,
            phases,
            unbroken: theme.colors.element_hover,
            scroll: ScrollbarColors::from_colors(&theme.colors),
        }
    }

    pub(crate) fn phase(&self, phase: Phase) -> Hsla {
        self.phases[phase.index()]
    }
}
