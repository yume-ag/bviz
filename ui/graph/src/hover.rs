//! The card describing the box under the pointer.

use crate::GraphPanel;
use crate::jump::{container, node_text, shown};
use crate::model::Loaded;
use crate::point::{configuration, kind_names};
use crate::spawn_bytes::{Sized, Spawned};
use crate::status::Statuses;
use crate::timecard::part_of;
use crate::timing::{Ran, Slowest, Timed};
use bviz_graph_layout::{Id, Kind};
use bviz_label::shown::written_line;
use bviz_ui_components::{CardAnchor, HoverCard, ShownCard, amount, hover_card_canvas};
use gpui::{
    App, Context, Entity, IntoElement, Pixels, Point, Render, WeakEntity, Window, prelude::*,
};
use std::rc::Rc;

/// The box under the pointer, as its card reads it.
#[derive(Clone, PartialEq)]
pub(crate) struct Pointed {
    id: Id,
    kind: Kind,
    /// Drawn open, over the boxes it holds.
    open: bool,
    /// Stands for interchangeable files folded together, a node of the
    /// drawing alone.
    group: bool,
    /// How the drawing captions it.
    caption: String,
}

impl Pointed {
    pub(crate) fn of(b: &bviz_graph_layout::Box) -> Self {
        Pointed {
            id: b.id,
            kind: b.kind,
            open: b.compound,
            group: b.group,
            caption: b.label.clone(),
        }
    }

    pub(crate) fn id(&self) -> Id {
        self.id
    }
}

/// A box the pointer has moved onto, as the card takes it.
pub(crate) enum Landed {
    /// The box the card describes already.
    Described(Id),
    /// Another box, for the card to describe afresh.
    Fresh(Pointed),
}

impl Landed {
    pub(crate) fn id(&self) -> Id {
        match self {
            Landed::Described(id) => *id,
            Landed::Fresh(pointed) => pointed.id,
        }
    }
}

/// What the panel holds of the card describing the box under the pointer.
pub(crate) struct BoxCard {
    card: HoverCard<Id>,
    pointed: Option<Pointed>,
    /// Kept until the panel notifies, which every change to what the card
    /// reads makes it do.
    read: Option<Read>,
    /// Notified alone for a change only the card shows.
    layer: Entity<CardLayer>,
}

impl BoxCard {
    pub(crate) fn new(cx: &mut Context<GraphPanel>) -> Self {
        let parent = cx.entity().downgrade();
        BoxCard {
            card: HoverCard::new(
                cx,
                |view| &mut view.box_card.card,
                |view, _| view.box_card.described(),
            ),
            pointed: None,
            read: None,
            layer: cx.new(|_| CardLayer { parent }),
        }
    }

    pub(crate) fn layer(&self) -> Entity<CardLayer> {
        self.layer.clone()
    }

    /// The box the card describes, where it describes one.
    pub(crate) fn described(&self) -> Option<Id> {
        self.pointed.as_ref().map(Pointed::id)
    }

    /// Repaints the card, and nothing else.
    fn repaint(&self, cx: &mut App) {
        self.layer.update(cx, |_, cx| cx.notify());
    }

    /// Repaints the card after a change to what it reads.
    pub(crate) fn restate(&mut self, cx: &mut App) {
        self.read = None;
        self.repaint(cx);
    }

    /// Follows the pointer to `at`, over the box `landed` on, with no drag
    /// under way.
    pub(crate) fn hover(&mut self, at: Point<Pixels>, landed: Option<Landed>, cx: &mut App) {
        let id = landed.as_ref().map(Landed::id);
        let repaint = self.card.hover(at, id.as_ref());
        match landed {
            Some(Landed::Described(_)) => {}
            Some(Landed::Fresh(pointed)) => self.pointed = Some(pointed),
            None => self.pointed = None,
        }
        if repaint {
            self.repaint(cx);
        }
    }

    /// Follows the pointer to `at` as it carries a drag, which moves the
    /// drawing along with it, so the box under it stays the one it was over.
    pub(crate) fn drag(&mut self, at: Point<Pixels>, cx: &mut App) {
        self.card.drag(at);
        if self.pointed.is_some() {
            self.repaint(cx);
        }
    }

    /// Lets go of the box under the pointer: the pointer has left, or the
    /// drawing has moved the box away from under it.
    pub(crate) fn forget(&mut self, cx: &mut App) {
        self.card.leave();
        if self.pointed.take().is_some() {
            self.repaint(cx);
        }
    }

    /// The box described, and where the pointer rests beside it, while the
    /// card is shown.
    fn shown(&self) -> Option<(&Pointed, Point<Pixels>)> {
        let pointed = self.pointed.as_ref()?;
        let at = self.card.pointer()?;
        self.card.shows(&pointed.id).then_some((pointed, at))
    }
}

/// What the card reads of a box.
struct Read {
    id: Id,
    lines: Rc<[String]>,
}

/// The layer the card is painted in, apart from the drawing and the chrome,
/// so that following the pointer repaints neither.
pub(crate) struct CardLayer {
    parent: WeakEntity<GraphPanel>,
}

impl Render for CardLayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let parent = self.parent.clone();
        let colors = bviz_ui_theme::theme(window, cx).colors.tooltip();
        hover_card_canvas(colors, move |_, _, cx| {
            let view = parent.upgrade()?;
            let (lines, at) = view.update(cx, |view, cx| view.card_lines(cx))?;
            let anchor = CardAnchor::Pointer(at);
            Some(ShownCard { lines, anchor })
        })
    }
}

impl GraphPanel {
    /// What the card shown reads, and where the pointer rests beside it.
    fn card_lines(&mut self, cx: &App) -> Option<(Rc<[String]>, Point<Pixels>)> {
        let (pointed, at) = self.box_card.shown()?;
        let id = pointed.id;
        if let Some(read) = &self.box_card.read
            && read.id == id
        {
            return Some((Rc::clone(&read.lines), at));
        }
        let loaded = self.model.read(cx).loaded()?;
        let lines: Rc<[String]> = describe(pointed, loaded).into();
        let read = Read {
            id,
            lines: Rc::clone(&lines),
        };
        self.box_card.read = Some(read);
        Some((lines, at))
    }
}

/// The most lines a card reads.
const CARD_LINES: usize = 10;

/// What the card describing `pointed` reads of what `loaded` knows: its name,
/// then what it is, how its build went, what its actions took and what their
/// spawns wrote and read.
fn describe(pointed: &Pointed, loaded: &Loaded) -> Vec<String> {
    let shared = &loaded.shared;
    let (id, kind) = (pointed.id, pointed.kind);
    let mut lines = Vec::with_capacity(CARD_LINES);
    if pointed.group {
        lines.push(pointed.caption.clone());
        lines.push(kind_names(Kind::MergeGroup).one.to_owned());
    } else {
        let written = node_text(shared, id, kind);
        match written.is_empty() {
            true => lines.push(pointed.caption.clone()),
            false => {
                let named = shown(shared, &written);
                let written = written_line(&written, &named);
                lines.push(named);
                lines.extend(written);
            }
        }
        let what = kind_names(kind).one;
        lines.push(match configuration(id, shared) {
            Some(config) => format!("{what} · {config}"),
            None => what.to_owned(),
        });
        if kind != Kind::Package {
            lines.extend(container(shared, id, kind).map(|written| shown(shared, &written)));
        }
    }
    lines.extend(outcome(id, loaded.statuses()));
    let timed = loaded.times().ok().and_then(|times| times.get(id));
    lines.extend(timed.into_iter().flat_map(|timed| time_taken(id, timed)));
    let sized = loaded.bytes().ok().and_then(|bytes| bytes.get(id));
    if let Some(sized) = sized {
        lines.push(bytes_moved(kind, sized));
        lines.extend(amount::read_alone(sized.held.bytes));
    }
    if loaded.critical().marks(id, !pointed.open) {
        lines.push("critical path".to_owned());
    }
    lines
}

/// What the build reports of the box `id`: of the box itself, or of what it
/// holds where that went worse.
fn outcome(id: Id, statuses: &Statuses) -> Option<String> {
    let status = statuses.get(id)?;
    Some(match statuses.own(id) == Some(status) {
        true => status.label().to_owned(),
        false => format!("{} within", status.label()),
    })
}

/// What the actions landing on the box `id`, which `timed` times, took: the
/// action's time and runner where the box is that action, and otherwise how
/// many it holds, their time, and each runner's share of it.
fn time_taken(id: Id, timed: &Timed) -> Vec<String> {
    let held = timed.held.total();
    let took = crate::duration::lasted(held.wall);
    if let Some(Slowest { action, .. }) = timed.slowest
        && action == id
    {
        return vec![format!("{took} · {}", Ran::of(&timed.held).label())];
    }
    let actions = match held.actions {
        1 => "1 action".to_owned(),
        count => format!("{count} actions"),
    };
    let shares: Vec<String> = (timed.held.runners())
        .map(|(runner, share)| format!("{} {}", Ran::On(runner).label(), part_of(share, held)))
        .collect();
    vec![format!("{actions} · {took}"), shares.join(" · ")]
}

/// What the spawns landing on a box of `kind`, which `sized` sizes, wrote
/// and read, counted unless the box is an action only one lands on.
fn bytes_moved(kind: Kind, sized: &Sized) -> String {
    let Spawned { spawns, bytes } = sized.held;
    let moved = amount::moved(bytes);
    match spawns {
        1 if kind == Kind::Action => moved,
        1 => format!("1 spawn · {moved}"),
        _ => format!("{spawns} spawns · {moved}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jump::fixture;
    use crate::model::Opening;
    use std::collections::HashSet;

    fn loaded() -> Loaded {
        let opening = Opening {
            expanded: HashSet::new(),
            over_budget: None,
        };
        Loaded::new(fixture(), opening)
    }

    fn pointed(loaded: &Loaded, name: &str, open: bool) -> Pointed {
        let id = loaded.shared.names.get(name).expect("an interned node");
        let node = &loaded.shared.base.nodes[id.0 as usize];
        Pointed {
            id,
            kind: node.kind,
            open,
            group: false,
            caption: node.label.clone(),
        }
    }

    #[test]
    fn a_card_names_an_action_in_full_with_its_configuration_and_owner() {
        let loaded = loaded();
        let action = pointed(&loaded, "action:bazel-out/app/liblib.a", false);
        assert_eq!(
            describe(&action, &loaded),
            ["CppArchive liblib.a", "action · k8-fastbuild", "//app:lib"]
        );
    }

    #[test]
    fn a_card_names_a_package_by_its_whole_path() {
        let loaded = loaded();
        let package = pointed(&loaded, "pkg:app", true);
        assert_eq!(describe(&package, &loaded), ["//app", "package"]);
    }

    #[test]
    fn a_card_counts_the_actions_a_box_holds_and_each_runners_share() {
        use bviz_invocation::{Runner, Spent, Timing};
        use std::time::Duration;
        let timing = |millis, runner| Timing {
            wall: Duration::from_millis(millis),
            runner,
        };
        let mut held = Spent::default();
        held.add_action(timing(9_000, Runner::RemoteExecution));
        held.add_action(timing(1_000, Runner::DiskCacheHit));
        let slowest = Slowest {
            action: Id(7),
            wall: Duration::from_secs(9),
        };
        let timed = Timed {
            own: Spent::default(),
            held,
            slowest: Some(slowest),
        };
        assert_eq!(
            time_taken(Id(2), &timed),
            [
                "2 actions · 10.0s",
                "remote execution 90% · disk cache hit 10%"
            ]
        );
        let mut own = Spent::default();
        own.add_action(timing(9_000, Runner::RemoteExecution));
        let action = Timed {
            own: own.clone(),
            held: own,
            slowest: Some(slowest),
        };
        assert_eq!(time_taken(Id(7), &action), ["9.0s · remote execution"]);
    }

    #[test]
    fn a_card_reads_what_a_boxs_spawns_wrote_and_read() {
        use bviz_invocation::spawn::{Bytes, InputBytes, SpawnBytes};
        let spawned = |spawns, outputs, others, tools| Spawned {
            spawns,
            bytes: SpawnBytes {
                outputs: Bytes::new(outputs),
                inputs: Some(InputBytes {
                    tools: Bytes::new(tools),
                    others: Bytes::new(others),
                    read_alone: Bytes::ZERO,
                }),
            },
        };
        let action = Sized {
            own: spawned(1, 3 << 20, 512, 0),
            held: spawned(1, 3 << 20, 512, 0),
        };
        assert_eq!(
            bytes_moved(Kind::Action, &action),
            "wrote 3.0 MiB · read 512 B"
        );
        let target = Sized {
            own: Spawned::default(),
            held: spawned(4, 2 << 10, 3 << 20, 1 << 20),
        };
        assert_eq!(
            bytes_moved(Kind::Target, &target),
            "4 spawns · wrote 2.0 KiB · read 4.0 MiB (tools 1.0 MiB)"
        );
        let retried = Sized {
            own: spawned(2, 1 << 10, 0, 0),
            held: spawned(2, 1 << 10, 0, 0),
        };
        assert_eq!(
            bytes_moved(Kind::Action, &retried),
            "2 spawns · wrote 1.0 KiB · read 0 B"
        );
    }
}
