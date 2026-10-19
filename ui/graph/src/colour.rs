//! What the fill of a box says, and the toolbar control choosing it.

use crate::GraphPanel;
use crate::model::GraphModel;
use crate::timing::{NodeTimes, Untimed};
use bviz_bazel_flags::{PROFILE_PRIMARY_OUTPUT, PROFILE_TARGET_LABEL};
use bviz_ui_components::Dropdown;
use gpui::{Context, SharedString};

/// What a box's fill says.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum Colouring {
    /// The kind of node the box stands for.
    #[default]
    Kind,
    /// How long the actions landing on the box and on all it contains took.
    Time,
    /// Where those actions ran.
    Runner,
    /// Where the execution attempts of those actions stand at the document's
    /// playback time.
    Playback,
}

impl Colouring {
    /// Every colouring, in the order the picker offers them.
    pub(crate) const ALL: [Self; 4] = [Self::Kind, Self::Time, Self::Runner, Self::Playback];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Time => "time",
            Self::Runner => "runner",
            Self::Playback => "playback",
        }
    }

    /// This colouring's place in [`ALL`](Self::ALL), which the picker reports
    /// its choice by.
    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn at(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    /// The colouring offered after this one, wrapping round to the first,
    /// passing over those a graph carrying what `offered` says cannot answer.
    fn next(self, offered: Offered) -> Self {
        let after = Self::ALL.iter().cycle().skip(self.index() + 1);
        after
            .take(Self::ALL.len())
            .copied()
            .find(|colouring| colouring.available(offered))
            .unwrap_or(self)
    }

    /// Whether a graph carrying what `offered` says can be coloured so.
    fn available(self, offered: Offered) -> bool {
        match self {
            Self::Kind => true,
            Self::Time | Self::Runner => offered.timed,
            Self::Playback => offered.playable,
        }
    }
}

/// What the loaded graph carries that a colouring reads.
#[derive(Clone, Copy)]
pub(crate) struct Offered {
    /// Its nodes carry the time of the actions landing on them.
    timed: bool,
    /// Its nodes carry the execution attempts landing on them, which its
    /// document's clock plays back.
    playable: bool,
}

impl Offered {
    pub(crate) fn by(model: &GraphModel) -> Self {
        let loaded = model.loaded();
        Offered {
            timed: loaded.is_some_and(|loaded| loaded.times().is_ok()),
            playable: loaded.is_some_and(|loaded| loaded.states().is_some()),
        }
    }
}

/// The hint of a control reading the graph's action times, which `name`s it
/// where the graph places any, and says why it places none otherwise.
pub(crate) fn note(times: Result<&NodeTimes, Untimed>, name: &str) -> SharedString {
    match times {
        Ok(_) => name.to_owned(),
        Err(Untimed::NoProfile) => "No profile loaded".to_owned(),
        Err(Untimed::NoActions) => "Profile records no actions".to_owned(),
        Err(Untimed::Unnamed) => {
            format!("Needs {PROFILE_PRIMARY_OUTPUT} or {PROFILE_TARGET_LABEL}")
        }
        Err(Untimed::Unplaced) => "No profiled action in graph".to_owned(),
    }
    .into()
}

/// Makes the picker offer the colourings the loaded graph can answer, and
/// falls back to colouring by kind where the one chosen is no longer among
/// them.
pub(crate) fn sync(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let offered = Offered::by(view.model.read(cx));
    let hint = hint(view.model.read(cx));
    view.colour_picker.update(cx, |picker, cx| {
        offer(picker, offered, cx);
        picker.set_tooltip(hint, cx);
    });
    if !view.colouring.available(offered) {
        set(view, Colouring::Kind, cx);
    }
}

/// Makes `picker` offer the colourings a graph carrying what `offered` says
/// can be coloured by.
pub(crate) fn offer(picker: &mut Dropdown, offered: Offered, cx: &mut Context<Dropdown>) {
    for colouring in Colouring::ALL {
        picker.set_available(colouring.index(), colouring.available(offered), cx);
    }
}

/// Colours the boxes by `colouring`.
pub(crate) fn set(view: &mut GraphPanel, colouring: Colouring, cx: &mut Context<GraphPanel>) {
    if view.colouring == colouring {
        return;
    }
    view.colouring = colouring;
    view.colour_picker
        .update(cx, |picker, cx| picker.set_selected(colouring.index(), cx));
    view.invalidate_canvas(cx);
}

/// Colours the boxes by the colouring offered after the one standing.
pub(crate) fn cycle(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let offered = Offered::by(view.model.read(cx));
    set(view, view.colouring.next(offered), cx);
}

/// What the picker's hint says of the action times `model` holds.
pub(crate) fn hint(model: &GraphModel) -> SharedString {
    let times = model
        .loaded()
        .map_or(Err(Untimed::NoProfile), |loaded| loaded.times());
    note(times, "Box colouring")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_colouring_sits_at_the_index_it_reports() {
        for colouring in Colouring::ALL {
            assert_eq!(Colouring::at(colouring.index()), Some(colouring));
        }
        assert_eq!(Colouring::at(Colouring::ALL.len()), None);
    }

    /// The colourings are taken in the picker's order, round to the first,
    /// a colouring the graph cannot answer passed over.
    #[test]
    fn cycling_passes_over_the_colourings_the_graph_cannot_answer() {
        let timed = Offered {
            timed: true,
            playable: false,
        };
        assert_eq!(Colouring::Kind.next(timed), Colouring::Time);
        assert_eq!(Colouring::Runner.next(timed), Colouring::Kind);
        let bare = Offered {
            timed: false,
            playable: false,
        };
        assert_eq!(Colouring::Kind.next(bare), Colouring::Kind);
        let playable = Offered {
            timed: false,
            playable: true,
        };
        assert_eq!(Colouring::Kind.next(playable), Colouring::Playback);
        assert_eq!(Colouring::Playback.next(playable), Colouring::Kind);
    }

    #[test]
    fn a_profile_naming_no_action_says_which_flags_name_them() {
        let said = note(Err(Untimed::Unnamed), "Box colouring");
        assert!(said.contains(PROFILE_PRIMARY_OUTPUT.as_str()), "{said}");
        assert!(said.contains(PROFILE_TARGET_LABEL.as_str()), "{said}");
    }
}
