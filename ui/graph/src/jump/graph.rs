//! How the graph panel lands what its jump palette picks.

use super::{JumpEvent, JumpPalette, Stands};
use crate::derive::Drawn;
use crate::framing::{Frame, Held};
use crate::holder::Holder;
use crate::mode::LayoutMode;
use crate::model::{GraphModel, Shared};
use crate::reveal::{self, hidden_by};
use crate::{GraphPanel, chips, pins, why};
use bviz_graph_layout::Id;
use bviz_graph_layout::select::Visibility;
use gpui::{App, Context, Entity, FocusHandle, Window, px};
use rustc_hash::FxHashSet;
use std::sync::Arc;

/// Where the graph panel stood as its palette opened, and in which mode, so
/// that closing it unpicked returns there.
pub(crate) type OpenedAt = Option<(LayoutMode, Held)>;

/// The palette the graph panel stands over its drawing.
pub(crate) fn palette(
    model: Entity<GraphModel>,
    host: FocusHandle,
    chips: Entity<chips::ChipBar>,
    cx: &mut Context<GraphPanel>,
) -> Entity<JumpPalette> {
    let drawn = drawn_reader(cx);
    let panel = cx.weak_entity();
    let standing = Box::new(move |id, cx: &App| {
        let hidden = panel
            .upgrade()
            .and_then(|panel| hidden_by(panel.read(cx), id));
        hidden.map_or(Stands::Here, Stands::Hidden)
    });
    JumpPalette::new(model, Holder::Graph, host, chips, Some(drawn), standing, cx)
}

/// Reads the nodes the panel draws, for a control ranking them first.
pub(crate) fn drawn_reader(cx: &Context<GraphPanel>) -> chips::Drawn {
    let panel = cx.weak_entity();
    Box::new(move |cx: &App| {
        let drawn = (panel.upgrade()).map(|panel| drawn_ids(panel.read(cx)));
        Arc::new(drawn.unwrap_or_default())
    })
}

/// The nodes the drawing holds a box for at the zoom it is drawn at.
fn drawn_ids(view: &GraphPanel) -> FxHashSet<Id> {
    let drawn = view.drawn();
    let laid_out = drawn
        .as_ref()
        .and_then(|drawn| drawn.view(view.view_zoom(drawn)?));
    laid_out.map_or_else(FxHashSet::default, |laid_out| {
        laid_out.boxes.iter().map(|b| b.id).collect()
    })
}

/// Opens the palette over the drawing, or takes the focus back to it where it
/// is open, noting where the view stood as it opened.
pub(crate) fn open(view: &mut GraphPanel, window: &mut Window, cx: &mut Context<GraphPanel>) {
    let scope = super::Scope::default();
    if view
        .jump
        .update(cx, |palette, cx| palette.open(scope, window, cx))
    {
        view.jump_opened_at = view.framing.hold().map(|held| (view.mode, held));
    }
}

/// Lands what the palette asks of the panel.
pub(crate) fn landed(view: &mut GraphPanel, event: JumpEvent, cx: &mut Context<GraphPanel>) {
    if let JumpEvent::Preview(id) = event {
        preview(view, id, cx);
        return;
    }
    let opened_at = view.jump_opened_at.take();
    view.hovered = None;
    view.invalidate_canvas(cx);
    match event {
        JumpEvent::Preview(_) => {}
        JumpEvent::Go(id) => reveal::reveal(view, id, cx),
        JumpEvent::AskWhy(id) => why::ask_why(view, id, cx),
        JumpEvent::AlwaysShow(id) => {
            pins::set(view, id, Visibility::Shown, cx);
            reveal::reveal(view, id, cx);
        }
        JumpEvent::Closed => {
            if let Some((_, held)) = opened_at.filter(|&(mode, _)| mode == view.mode) {
                view.framing.return_to(held);
            }
        }
        JumpEvent::Dismissed => {}
    }
}

/// Moves the view onto `id`, or onto the innermost box drawn around it where
/// the drawing folds it away, and marks it as a hover would. Nothing is
/// selected or opened, so closing the palette unpicked leaves the drawing as
/// the palette found it.
fn preview(view: &mut GraphPanel, id: Id, cx: &mut Context<GraphPanel>) {
    let shown = view
        .model
        .read(cx)
        .loaded()
        .and_then(|loaded| drawn_holder(view, &loaded.shared, id));
    view.hovered = shown;
    let covered = view.jump.read(cx).painted().map_or(px(0.), |painted| {
        (painted.bottom() - view.framing.viewport().top()).max(px(0.))
    });
    if let Some(shown) = shown {
        view.framing
            .request_frame_below(Frame::Node(shown), covered);
    }
    view.invalidate_canvas(cx);
}

/// `id`, or the innermost node holding it, whichever the drawing draws
/// first.
fn drawn_holder(view: &GraphPanel, shared: &Shared, id: Id) -> Option<Id> {
    let drawn = view.drawn()?;
    let draws = |id: Id| match &drawn {
        Drawn::Map { map, .. } => map.placed(id).is_some(),
        Drawn::Layered { view, .. } => view.boxes.iter().any(|b| b.id == id),
    };
    std::iter::successors(Some(id), |id| shared.base.nodes.get(id.0 as usize)?.parent)
        .find(|&id| draws(id))
}

/// Closes the palette, leaving the view where it stands.
pub(crate) fn dismiss(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    if view.jump.update(cx, |palette, cx| palette.dismiss(cx)) {
        view.jump_opened_at = None;
        view.hovered = None;
        view.invalidate_canvas(cx);
    }
}

/// Closes the palette, which offered the nodes of a graph that has been
/// replaced, with the preview its rows put up. The palette may have closed
/// itself on the same replacement already, so the preview is put away
/// whether or not it was open.
pub(crate) fn reset(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    view.jump.update(cx, |palette, cx| palette.dismiss(cx));
    view.jump_opened_at = None;
    view.hovered = None;
}
