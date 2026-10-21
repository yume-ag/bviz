//! The menu of rearrangements a panel offers: what it offers where the panel
//! stands, and the rows a [`Menu`] puts that up as.
//!
//! [`Menu`]: bviz_ui_components::Menu

use crate::workspace::{
    ClosePanel, Destination, DetachPanel, EvenSplit, ShowKeysAndGestures, SplitDown, SplitLeft,
    SplitRight, SplitUp, ToggleMaximize, Workspace, re_homes,
};
use crate::{Arrangement, Pane, Side};
use bviz_ui_components::{Host, MenuItem, SubmenuItem};
use gpui::{Action, App, Entity, EntityId, FocusHandle, SharedString, WeakEntity, Window};

/// The sides a panel can be split off towards or moved to, in the order a menu
/// offers them.
const SIDES: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

/// A rearrangement a row of a panel's menu offers, which an action also does
/// to the active panel.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Offer {
    /// A pane of its own on that side of the pane holding it.
    Split(Side),
    NewWindow,
    /// Draw the pane holding it in place of the whole window's body, or put
    /// every pane back where one is drawn in place of them already.
    Maximize,
    EvenSplit,
    KeysHelp,
    Close,
}

/// A place a row of a submenu offers to move a panel to, `W` naming a window.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Place<W> {
    /// A pane of its own spanning that edge of the window.
    Edge(Side),
    Window(W),
}

/// One row of a panel's menu, as [`offered`] works it out.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Row<W> {
    /// A row reading the label of the action that does what it offers.
    Offer(Offer),
    Submenu {
        label: SharedString,
        under: Vec<Under<W>>,
    },
}

/// One row of the submenu a row opens beside itself. Every such row stands for
/// a place of its own, a menu opening no list beside a submenu.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Under<W> {
    pub label: SharedString,
    pub place: Place<W>,
}

/// Where a panel stands, as far as what its menu can offer goes.
pub(crate) struct Standing<'a, P = Entity<Pane>> {
    pub arrangement: &'a Arrangement<P>,
    /// The pane holding the panel.
    pub pane: &'a P,
    /// Whether that pane holds the panel and no other.
    pub alone: bool,
}

/// What choosing a row of a panel's menu does to the panel it was opened for.
/// The pane and the window are type parameters, as [`Destination`]'s pane is.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Deed<P = Entity<Pane>, W = WeakEntity<Workspace>> {
    /// Re-homes the panel.
    Move(Destination<P>),
    /// Moves the panel into the window that workspace frames.
    ToWindow(W),
    /// Draws the pane holding the panel in place of the whole window's body, or
    /// puts every pane back where one is drawn in place of them already.
    Maximize,
    /// Evens the split laying the pane holding the panel out.
    EvenSplit,
    /// Puts the panel's keys help up, or takes it down.
    KeysHelp,
    Close,
}

/// What the menu of a panel standing at `standing` offers in an application
/// running in `host`, `windows` naming the session's other open windows. A
/// rearrangement that would leave the window laid out as it already is
/// ([`re_homes`]) is left out rather than offered and refused, as is one there
/// is nothing in this window to carry out.
pub(crate) fn offered<P: Clone + PartialEq, W: Clone>(
    standing: &Standing<'_, P>,
    windows: &[(W, SharedString)],
    host: Host,
) -> Vec<Row<W>> {
    let Standing {
        arrangement,
        pane,
        alone,
    } = *standing;
    let layout = arrangement.layout();
    let mut rows = Vec::new();
    for side in SIDES {
        let to = Destination::Beside {
            pane: pane.clone(),
            side,
        };
        if re_homes(&to, Some(pane), alone, layout, host) {
            rows.push(Row::Offer(Offer::Split(side)));
        }
    }
    let edges: Vec<Under<W>> = SIDES
        .into_iter()
        .filter(|&side| {
            re_homes(
                &Destination::WindowEdge(side),
                Some(pane),
                alone,
                layout,
                host,
            )
        })
        .map(|side| Under {
            label: edge_name(side).into(),
            place: Place::Edge(side),
        })
        .collect();
    if !edges.is_empty() {
        rows.push(Row::Submenu {
            label: "Move to window edge".into(),
            under: edges,
        });
    }
    if re_homes(&Destination::NewWindow, Some(pane), alone, layout, host) {
        rows.push(Row::Offer(Offer::NewWindow));
    }
    if !windows.is_empty() {
        rows.push(Row::Submenu {
            label: "Move to window".into(),
            under: windows
                .iter()
                .map(|(window, title)| Under {
                    label: title.clone(),
                    place: Place::Window(window.clone()),
                })
                .collect(),
        });
    }
    if arrangement.maximized().is_some() || arrangement.maximizes(pane) {
        rows.push(Row::Offer(Offer::Maximize));
    }
    if layout.innermost_divider(pane).is_some() {
        rows.push(Row::Offer(Offer::EvenSplit));
    }
    rows.push(Row::Offer(Offer::KeysHelp));
    rows.push(Row::Offer(Offer::Close));
    rows
}

/// The menu `standing` offers for `panel`, as rows a [`Menu`] puts up: choosing
/// one asks `workspace` to carry it out. A row names the keys that do the same
/// in `keys_in`, which [`keys_focus`] gives.
///
/// [`Menu`]: bviz_ui_components::Menu
/// [`keys_focus`]: crate::panel::keys_focus
pub(crate) fn items(
    panel: EntityId,
    workspace: &WeakEntity<Workspace>,
    standing: &Standing<'_>,
    windows: &[(WeakEntity<Workspace>, SharedString)],
    keys_in: Option<&FocusHandle>,
    window: &Window,
    cx: &App,
) -> Vec<MenuItem> {
    let offered = offered(standing, windows, Host::CURRENT);
    let mut items = Vec::with_capacity(offered.len());
    for row in offered {
        items.push(match row {
            Row::Submenu { label, under } => MenuItem::submenu(
                label,
                under
                    .into_iter()
                    .map(|under| {
                        let deed = place_deed(under.place);
                        SubmenuItem::new(under.label, taking(deed, panel, workspace))
                    })
                    .collect(),
            ),
            Row::Offer(offer) => {
                let action = offer.action();
                let deed = offer_deed(offer, standing.pane);
                MenuItem::action(action, keys_in, window, cx, taking(deed, panel, workspace))
            }
        });
    }
    items
}

/// What choosing a row whose choice comes to `deed` does: asks `workspace` to
/// carry it out for `panel`.
fn taking(
    deed: Deed,
    panel: EntityId,
    workspace: &WeakEntity<Workspace>,
) -> impl Fn(&mut Window, &mut App) + use<> {
    let workspace = workspace.clone();
    move |window, cx| {
        let deed = deed.clone();
        workspace
            .update(cx, |workspace, cx| {
                workspace.take_deed(panel, deed, window, cx);
            })
            .ok();
    }
}

/// What carrying `offer` out for a panel held by `pane` comes to.
fn offer_deed<P: Clone, W>(offer: Offer, pane: &P) -> Deed<P, W> {
    match offer {
        Offer::Split(side) => Deed::Move(Destination::Beside {
            pane: pane.clone(),
            side,
        }),
        Offer::NewWindow => Deed::Move(Destination::NewWindow),
        Offer::Maximize => Deed::Maximize,
        Offer::EvenSplit => Deed::EvenSplit,
        Offer::KeysHelp => Deed::KeysHelp,
        Offer::Close => Deed::Close,
    }
}

/// What moving a panel to `place` comes to.
fn place_deed<P, W>(place: Place<W>) -> Deed<P, W> {
    match place {
        Place::Edge(side) => Deed::Move(Destination::WindowEdge(side)),
        Place::Window(workspace) => Deed::ToWindow(workspace),
    }
}

impl Offer {
    /// The action that does to the active panel what the offer does to the
    /// panel its menu is for.
    fn action(&self) -> &'static dyn Action {
        match self {
            Offer::Split(Side::Left) => &SplitLeft,
            Offer::Split(Side::Right) => &SplitRight,
            Offer::Split(Side::Top) => &SplitUp,
            Offer::Split(Side::Bottom) => &SplitDown,
            Offer::NewWindow => &DetachPanel,
            Offer::Maximize => &ToggleMaximize,
            Offer::EvenSplit => &EvenSplit,
            Offer::KeysHelp => &ShowKeysAndGestures,
            Offer::Close => &ClosePanel,
        }
    }
}

/// How a side reads where it names an edge of the window.
fn edge_name(side: Side) -> &'static str {
    match side {
        Side::Left => "Left",
        Side::Right => "Right",
        Side::Top => "Top",
        Side::Bottom => "Bottom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Layout, Maximize};
    use gpui::Axis;

    /// A window naming itself, as the menu of another window's panel lists it.
    type Window = &'static str;

    /// The other windows a session holds open, each named by its own title.
    fn windows<const N: usize>(titles: [Window; N]) -> Vec<(Window, SharedString)> {
        titles.map(|title| (title, title.into())).to_vec()
    }

    /// What the menu of a panel held by `pane` offers, that pane holding it
    /// `alone` or among others, out of an arrangement of plain panes, on the
    /// desktop and with no other window open.
    fn offers(arrangement: &Arrangement<char>, pane: char, alone: bool) -> Vec<Row<Window>> {
        offered(&standing(arrangement, &pane, alone), &[], Host::Desktop)
    }

    fn standing<'a>(
        arrangement: &'a Arrangement<char>,
        pane: &'a char,
        alone: bool,
    ) -> Standing<'a, char> {
        Standing {
            arrangement,
            pane,
            alone,
        }
    }

    /// What each row of a menu offers, an offer by its name and a row opening
    /// a submenu by its label followed by the rows it opens, each set in from it.
    fn labels<W>(rows: &[Row<W>]) -> Vec<String> {
        let mut read = Vec::with_capacity(rows.len());
        for row in rows {
            match row {
                Row::Offer(offer) => read.push(format!("{offer:?}")),
                Row::Submenu { label, under } => {
                    read.push(label.to_string());
                    read.extend(under.iter().map(|under| format!("  {}", under.label)));
                }
            }
        }
        read
    }

    /// The rows a menu opens beside the row reading `label`.
    fn under<'a, W>(rows: &'a [Row<W>], label: &str) -> &'a [Under<W>] {
        let opened = rows.iter().find_map(|row| match row {
            Row::Submenu { label: read, under } if read == label => Some(under),
            _ => None,
        });
        opened.unwrap_or_else(|| panic!("{label} opens a submenu"))
    }

    /// Panes 'a' and 'b' side by side.
    fn split() -> Arrangement<char> {
        Arrangement::new(Layout::split_at(
            Axis::Horizontal,
            0.5,
            Layout::Pane('a'),
            Layout::Pane('b'),
        ))
    }

    /// Pane 'a' beside 'b' above 'c', so that a pane is laid out against some
    /// of the window's edges and away from the rest.
    fn nested() -> Arrangement<char> {
        Arrangement::new(Layout::split_at(
            Axis::Horizontal,
            0.5,
            Layout::Pane('a'),
            Layout::split_at(Axis::Vertical, 0.5, Layout::Pane('b'), Layout::Pane('c')),
        ))
    }

    /// A window laying out one pane holding one panel is that panel: it stands
    /// over the whole of the window already, so every place a rearrangement
    /// could put it is the place it stands — every place but a window of its
    /// own.
    #[test]
    fn a_lone_panel_of_a_lone_pane_is_offered_only_what_would_move_it() {
        let arrangement = Arrangement::new(Layout::Pane('a'));
        assert_eq!(
            labels(&offers(&arrangement, 'a', true)),
            ["NewWindow", "KeysHelp", "Close"]
        );
    }

    /// A pane holding several panels can give one of them a pane of its own,
    /// wherever in the window that pane goes.
    #[test]
    fn a_panel_beside_others_in_its_pane_is_offered_every_split() {
        let arrangement = Arrangement::new(Layout::Pane('a'));
        assert_eq!(
            labels(&offers(&arrangement, 'a', false)),
            [
                "Split(Left)",
                "Split(Right)",
                "Split(Top)",
                "Split(Bottom)",
                "Move to window edge",
                "  Left",
                "  Right",
                "  Top",
                "  Bottom",
                "NewWindow",
                "KeysHelp",
                "Close",
            ]
        );
    }

    /// A lone panel of a pane laid out beside another is offered the edges of
    /// the window its pane does not already span, the pane over the whole body,
    /// and an even share of the split laying it out.
    #[test]
    fn a_lone_panel_in_a_split_is_offered_the_edges_its_pane_does_not_span() {
        assert_eq!(
            labels(&offers(&split(), 'a', true)),
            [
                "Move to window edge",
                "  Right",
                "  Top",
                "  Bottom",
                "NewWindow",
                "Maximize",
                "EvenSplit",
                "KeysHelp",
                "Close",
            ]
        );
    }

    /// A panel among others in a pane that is itself laid out beside another is
    /// the one standing offered every rearrangement at once: a pane of its own
    /// on any side, any edge of the window, the body, and an even split.
    #[test]
    fn a_panel_among_others_in_a_split_pane_is_offered_everything() {
        assert_eq!(
            labels(&offers(&split(), 'a', false)),
            [
                "Split(Left)",
                "Split(Right)",
                "Split(Top)",
                "Split(Bottom)",
                "Move to window edge",
                "  Left",
                "  Right",
                "  Top",
                "  Bottom",
                "NewWindow",
                "Maximize",
                "EvenSplit",
                "KeysHelp",
                "Close",
            ]
        );
    }

    /// The pane already drawn in place of the whole window's body is offered
    /// the panes back, which the same offer as the body carries out.
    #[test]
    fn a_maximized_pane_is_offered_the_panes_it_hides_back() {
        let mut arrangement = split();
        assert_eq!(arrangement.toggle_maximize(&'a'), Maximize::Maximized);
        let read = labels(&offers(&arrangement, 'a', true));
        assert!(read.contains(&"Maximize".to_string()), "{read:?}");
    }

    /// The body is offered exactly where asking for it would be carried out, so
    /// no row is offered that the arrangement would refuse.
    #[test]
    fn the_body_is_offered_only_where_the_arrangement_would_give_it() {
        for arrangement in [Arrangement::new(Layout::Pane('a')), split(), nested()] {
            for pane in ['a', 'b', 'z'] {
                let offered = offers(&arrangement, pane, false);
                let maximizes = arrangement.maximizes(&pane);
                assert_eq!(
                    labels(&offered).contains(&"Maximize".to_string()),
                    maximizes,
                    "{pane}"
                );
            }
        }
    }

    /// Every side a split or an edge is offered for is one a move would be
    /// carried out towards, and every side left out is one it would refuse: a
    /// row that does nothing is worse than no row at all.
    #[test]
    fn every_offer_is_one_the_move_would_carry_out() {
        for arrangement in [Arrangement::new(Layout::Pane('a')), split(), nested()] {
            for pane in ['a', 'b', 'c'] {
                for (alone, host) in [true, false]
                    .into_iter()
                    .flat_map(|alone| [(alone, Host::Desktop), (alone, Host::Browser)])
                {
                    let layout = arrangement.layout();
                    if !layout.contains(&pane) {
                        continue;
                    }
                    let standing = standing(&arrangement, &pane, alone);
                    let offered = offered::<char, Window>(&standing, &[], host);
                    let read = labels(&offered);
                    let re_homes = |to| re_homes(&to, Some(&pane), alone, layout, host);
                    for side in SIDES {
                        assert_eq!(
                            read.contains(&format!("Split({side:?})")),
                            re_homes(Destination::Beside { pane, side }),
                            "{pane} {alone} {host:?} split {side:?}"
                        );
                        let edge = read.contains(&format!("  {}", edge_name(side)));
                        assert_eq!(
                            edge,
                            re_homes(Destination::WindowEdge(side)),
                            "{pane} {alone} {host:?} edge {side:?}"
                        );
                    }
                    assert_eq!(
                        read.contains(&"NewWindow".to_string()),
                        re_homes(Destination::NewWindow),
                        "{pane} {alone} {host:?} new window"
                    );
                }
            }
        }
    }

    /// Every other open window is offered by the title it goes by, and a
    /// session holding none offers no such row at all.
    #[test]
    fn the_session_s_other_windows_are_offered_by_their_titles() {
        let arrangement = Arrangement::new(Layout::Pane('a'));
        let standing = standing(&arrangement, &'a', true);
        assert_eq!(
            labels(&offered(
                &standing,
                &windows(["1. b.profile.gz"]),
                Host::Desktop
            )),
            [
                "NewWindow",
                "Move to window",
                "  1. b.profile.gz",
                "KeysHelp",
                "Close",
            ]
        );
        assert_eq!(
            labels(&offered::<char, Window>(&standing, &[], Host::Desktop)),
            ["NewWindow", "KeysHelp", "Close"]
        );
    }

    /// A browser page holds one window, so a panel there offers no window of
    /// its own.
    #[test]
    fn a_browser_offers_no_new_window() {
        let arrangement = Arrangement::new(Layout::Pane('a'));
        let standing = standing(&arrangement, &'a', true);
        assert_eq!(
            labels(&offered::<char, Window>(&standing, &[], Host::Browser)),
            ["KeysHelp", "Close"]
        );
    }

    /// A move names the window it is to be carried out on, so the windows a
    /// session holds open are told apart by the offers and not by the titles
    /// alone: each row carries the window it was listed from.
    #[test]
    fn a_move_to_a_window_names_that_window() {
        let arrangement = Arrangement::new(Layout::Pane('a'));
        let standing = standing(&arrangement, &'a', true);
        let offered = offered(
            &standing,
            &windows(["second", "third", "fourth"]),
            Host::Desktop,
        );
        let listed = under(&offered, "Move to window");
        let named: Vec<&Place<Window>> = listed.iter().map(|under| &under.place).collect();
        assert_eq!(
            named,
            [
                &Place::Window("second"),
                &Place::Window("third"),
                &Place::Window("fourth"),
            ]
        );
    }

    /// Each side names the destination that side stands for, so a menu offering
    /// a split and an edge for the same side carries them out apart.
    #[test]
    fn every_offer_carries_out_what_its_row_reads() {
        for side in SIDES {
            assert_eq!(
                offer_deed::<char, Window>(Offer::Split(side), &'a'),
                Deed::Move(Destination::Beside { pane: 'a', side })
            );
            assert_eq!(
                place_deed::<char, Window>(Place::Edge(side)),
                Deed::Move(Destination::WindowEdge(side))
            );
        }
        assert_eq!(
            offer_deed::<char, Window>(Offer::NewWindow, &'a'),
            Deed::Move(Destination::NewWindow)
        );
        assert_eq!(
            place_deed::<char, Window>(Place::Window("second")),
            Deed::ToWindow("second")
        );
        assert_eq!(
            offer_deed::<char, Window>(Offer::EvenSplit, &'a'),
            Deed::EvenSplit
        );
        assert_eq!(
            offer_deed::<char, Window>(Offer::Maximize, &'a'),
            Deed::Maximize
        );
        assert_eq!(offer_deed::<char, Window>(Offer::Close, &'a'), Deed::Close);
    }
}
