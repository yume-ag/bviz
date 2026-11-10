use crate::GraphPanel;
use crate::chips::Unmarked;
use crate::derive::Shown;
use crate::neighborhood::{MarkingMemo, Neighborhood};
use crate::relation::Relation;
use crate::reveal::{hidden_by, standing_for};
use bviz_graph_layout::Id;
use gpui::{App, Context};
use std::rc::Rc;

/// The box standing for the document's selected node, whose related nodes
/// stay marked until another is selected, and which of its relations the
/// marking covers.
pub(crate) struct Selection {
    node: Option<Id>,
    relation: Relation,
    marked: MarkingMemo,
}

impl Selection {
    pub(crate) fn none() -> Self {
        Self {
            node: None,
            relation: Relation::default(),
            marked: MarkingMemo::default(),
        }
    }

    /// The box marked, which may hold the node selected rather than be it.
    pub(crate) fn node(&self) -> Option<Id> {
        self.node
    }

    pub(crate) fn relation(&self) -> Relation {
        self.relation
    }

    /// Marks from the box `node`, or from none.
    pub(crate) fn mark(&mut self, node: Option<Id>) {
        self.node = node;
    }

    pub(crate) fn set_relation(&mut self, relation: Relation) {
        self.relation = relation;
    }

    /// What the selection marks in `shown`. Empty while no box is marked,
    /// and while `shown` draws none for the box marked, as a map's coarser
    /// levels may not.
    pub(crate) fn marking(&mut self, shown: Shown<'_>) -> Rc<Neighborhood> {
        self.marked.marking(shown, self.node, self.relation)
    }
}

/// Selects `node`, or nothing for `None`, in the panel's document, and
/// marks the box standing for it.
pub(crate) fn select(view: &mut GraphPanel, node: Option<Id>, cx: &mut Context<GraphPanel>) {
    let by = cx.entity_id();
    view.model
        .update(cx, |model, cx| model.select(node, by, cx));
    remark(view, cx);
}

/// The node the panel's controls act on: the document's selected node, while
/// the panel marks a box for it, which may hold it rather than be it.
pub(crate) fn acted_on(view: &GraphPanel, cx: &App) -> Option<Id> {
    view.selection.node()?;
    view.model.read(cx).selected()
}

/// Marks the box standing for the document's selected node in the drawing as
/// it is, opening none, and no box where the panel hides that node, which the
/// query bar's status line then says.
pub(crate) fn remark(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let marking = view.model.read(cx).selected().map(|id| Marking {
        hidden: hidden_by(view, id).is_some(),
        standing: standing_for(view, id),
        settled: !view.derived.deriving(),
    });
    let (marked, unmarked) = marking.map_or((None, None), Marking::resolve);
    view.chips
        .update(cx, |bar, cx| bar.set_unmarked(unmarked, cx));
    if marked != view.selection.node() {
        view.selection.mark(marked);
        view.invalidate_canvas(cx);
    }
    // The chrome describes the node selected, which can change within the
    // box marked for it.
    cx.notify();
    crate::box_menu::remarked(view, cx);
}

/// What a panel's drawing makes of the node selected.
struct Marking {
    /// Whether the drawing hides it.
    hidden: bool,
    /// The box standing for it, which a hidden node may still have.
    standing: Option<Id>,
    /// Whether the drawing is the one the panel settles on, no derivation
    /// being in flight.
    settled: bool,
}

impl Marking {
    /// The box marked for the node, or why there is none; a drawing still
    /// being derived says nothing of a node it does not hold yet.
    fn resolve(self) -> (Option<Id>, Option<Unmarked>) {
        match self {
            Marking { hidden: true, .. } => (None, Some(Unmarked::Hidden)),
            Marking {
                standing: Some(standing),
                ..
            } => (Some(standing), None),
            Marking { settled, .. } => (None, settled.then_some(Unmarked::Absent)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::Drawn;
    use crate::relation::Direction;
    use bviz_graph_layout::{Box, Connector, EdgeKind, Kind, LaidOut};
    use std::sync::Arc;

    /// Any zoom: a layered drawing shows the one view at every zoom.
    const ZOOM: f32 = 1.0;

    fn leaf(id: u32) -> Box {
        Box {
            id: Id(id),
            label: String::new(),
            kind: Kind::Source,
            x: f64::from(id) * 200.0,
            y: 0.0,
            width: 100.0,
            height: 40.0,
            compound: false,
            group: false,
        }
    }

    fn shown(drawn: &Drawn) -> Shown<'_> {
        drawn.shown(ZOOM).expect("a layered view")
    }

    /// Two leaves, the first an input of the second.
    fn pair() -> Drawn {
        let view = LaidOut {
            boxes: vec![leaf(0), leaf(1)],
            connectors: vec![Connector {
                points: Vec::new(),
                kinds: vec![EdgeKind::Query],
                from: Id(0),
                to: Id(1),
                transition: None,
            }],
            full_edges: vec![(Id(0), Id(1))],
            ..Default::default()
        };
        Drawn::Layered {
            view: Arc::new(view),
            drawing: 0,
        }
    }

    #[test]
    fn a_hidden_selection_marks_nothing_and_says_so() {
        let marking = |hidden, standing, settled| {
            Marking {
                hidden,
                standing,
                settled,
            }
            .resolve()
        };
        let hidden = (None, Some(Unmarked::Hidden));
        assert_eq!(marking(true, Some(Id(1)), true), hidden);
        assert_eq!(marking(true, None, false), hidden);
        assert_eq!(marking(false, Some(Id(1)), true), (Some(Id(1)), None));
        assert_eq!(marking(false, None, true), (None, Some(Unmarked::Absent)));
        assert_eq!(marking(false, None, false), (None, None));
    }

    #[test]
    fn nothing_selected_marks_nothing() {
        let mut selection = Selection::none();
        assert!(selection.marking(shown(&pair())).boxes.is_empty());
    }

    /// The relation is the reader's standing answer to what a selection should
    /// mark, so it outlives the box it was set on and the next one is marked to
    /// the same reach without setting it again.
    #[test]
    fn the_relation_outlives_the_selection_it_was_set_on() {
        let mut selection = Selection::none();
        selection.mark(Some(Id(0)));
        selection.set_relation(Relation::Transitive(Direction::In));
        selection.mark(None);
        assert_eq!(selection.relation(), Relation::Transitive(Direction::In));
        selection.mark(Some(Id(1)));
        assert_eq!(selection.relation(), Relation::Transitive(Direction::In));
    }

    #[test]
    fn a_marking_covers_the_relation_it_was_asked_for() {
        let g = pair();
        let mut selection = Selection::none();
        selection.mark(Some(Id(0)));
        selection.set_relation(Relation::None);
        assert_eq!(selection.marking(shown(&g)).boxes.len(), 1);
        selection.set_relation(Relation::Direct(Direction::Out));
        assert_eq!(selection.marking(shown(&g)).boxes.len(), 2);
        selection.set_relation(Relation::Direct(Direction::In));
        assert_eq!(selection.marking(shown(&g)).boxes.len(), 1);
    }
}
