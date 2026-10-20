//! The icicle moving from the cell it is drawn from to another standing
//! over or under it.

use super::icicle::{self, Cell, Holds, Tree, VISIBLE_WIDTH};
use crate::camera::ease_out_cubic;
use bviz_time::Instant;
use gpui::{Bounds, Pixels, point, size};
use std::time::Duration;

/// How long the icicle takes to move from one focus to the next.
const DURATION: Duration = Duration::from_millis(200);

/// A cell as a frame draws it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placed {
    pub(crate) cell: Cell,
    /// Whether it is the top cell of the icicle it belongs to.
    pub(crate) on_top: bool,
}

impl Placed {
    /// The cells of an icicle at rest, laid out as [`icicle::lay_out`] lays
    /// them out.
    pub(crate) fn still(cells: Vec<Cell>) -> Vec<Self> {
        let placed = cells.into_iter().enumerate();
        placed
            .map(|(index, cell)| Placed {
                cell,
                on_top: index == 0,
            })
            .collect()
    }
}

/// Which of the two cells a transition moves between it moves towards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    /// From the higher to the lower.
    Down,
    Up,
}

/// The icicle moving between the one drawn from a cell and the one drawn
/// from a cell standing somewhere under it, which it does from `start` for
/// [`DURATION`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Transition {
    higher: Holds,
    /// The cells from the lower one up to the one standing directly under
    /// `higher`, the lower first; never empty.
    path: Vec<Holds>,
    way: Way,
    start: Instant,
}

impl Transition {
    /// The move from the icicle drawn from `from` to the one drawn from `to`
    /// in `tree`, beginning at `start`; none where the two are one cell or
    /// neither stands over the other.
    pub(crate) fn between(
        tree: &impl Tree,
        from: Holds,
        to: Holds,
        start: Instant,
    ) -> Option<Self> {
        if from == to {
            return None;
        }
        let (higher, path, way) = match path(tree, from, to) {
            Some(path) => (from, path, Way::Down),
            None => (to, path(tree, to, from)?, Way::Up),
        };
        Some(Self {
            higher,
            path,
            way,
            start,
        })
    }

    /// The cell the higher icicle is drawn from.
    pub(crate) fn higher(&self) -> Holds {
        self.higher
    }

    /// The cell the lower icicle is drawn from.
    pub(crate) fn lower(&self) -> Holds {
        self.path[0]
    }

    /// How far the move has come at `now` from the higher icicle towards the
    /// lower, eased: 0 at the higher, 1 at the lower.
    pub(crate) fn lowered(&self, now: Instant) -> f32 {
        let eased = ease_out_cubic(self.elapsed(now));
        match self.way {
            Way::Down => eased,
            Way::Up => 1. - eased,
        }
    }

    /// Whether the move has ended by `now`.
    pub(crate) fn ended(&self, now: Instant) -> bool {
        self.elapsed(now) >= 1.
    }

    /// The share of the move's length gone by at `now`.
    fn elapsed(&self, now: Instant) -> f32 {
        let gone = now.saturating_duration_since(self.start);
        (gone.as_secs_f32() / DURATION.as_secs_f32()).min(1.)
    }

    /// The cells drawn `lowered` of the way from the icicle drawn from the
    /// higher cell to the one drawn from the lower, each laid out as
    /// [`icicle::lay_out`] lays it out in `bounds`. One transform maps the
    /// two: it takes the place the higher icicle gives the lower cell
    /// `lowered` of the way to the top row, the higher icicle with it, and
    /// the lower icicle is laid out under it at the width it is drawn, so
    /// that it gathers what is too narrow to draw apart as either icicle
    /// does at rest. The lower icicle stands in for the cells the higher
    /// draws in its place, and what either lays out beyond `seen_in` is left
    /// out, which may reach above `bounds`. Where `tree` no longer draws the
    /// one under the other, the icicle the move makes for is drawn at rest.
    pub(crate) fn cells(
        &self,
        tree: &impl Tree,
        bounds: Bounds<Pixels>,
        seen_in: Bounds<Pixels>,
        row: Pixels,
        min_width: Pixels,
        lowered: f32,
    ) -> Vec<Placed> {
        let top = Bounds::new(bounds.origin, size(bounds.size.width, row));
        let higher = icicle::lay_out(tree, self.higher, bounds, row, min_width);
        let Some(anchor) = self.anchor(tree, &higher, bounds, row, min_width) else {
            let end = match self.way {
                Way::Down => self.path[0],
                Way::Up => self.higher,
            };
            return Placed::still(icicle::lay_out(tree, end, bounds, row, min_width));
        };
        let at = |from: Pixels, to: Pixels| from + (to - from) * lowered;
        let moved = Bounds::new(
            point(at(anchor.left(), top.left()), at(anchor.top(), top.top())),
            size(at(anchor.size.width, top.size.width), row),
        );
        let replaced = |rect: Bounds<Pixels>| {
            rect.top() >= anchor.top() - row / 2. && icicle::spans(anchor, rect)
        };
        let under = Bounds::new(moved.origin, size(moved.size.width, bounds.size.height));
        let lower = icicle::lay_out(tree, self.path[0], under, row, min_width);
        let mut cells = Vec::with_capacity(higher.len() + lower.len());
        let higher = Placed::still(higher).into_iter();
        let kept = higher.filter(|placed| !replaced(placed.cell.rect));
        cells.extend(kept.filter_map(|placed| carried(placed, anchor, moved, seen_in)));
        let lower = Placed::still(lower).into_iter();
        cells.extend(lower.filter_map(|placed| seen(placed, placed.cell.rect, seen_in)));
        cells
    }

    /// Where the `higher` icicle, laid out in `bounds`, places the lower
    /// cell, a row high: from the nearest cell drawn holding it, standing
    /// over it or gathering it, down through each cell between by its
    /// weight as [`icicle::lay_out`] places it, whether or not that is wide
    /// enough to draw.
    fn anchor(
        &self,
        tree: &impl Tree,
        higher: &[Cell],
        bounds: Bounds<Pixels>,
        row: Pixels,
        min_width: Pixels,
    ) -> Option<Bounds<Pixels>> {
        // Each cell over the next, the lower first and the higher last.
        let chain: Vec<Holds> = self.path.iter().copied().chain([self.higher]).collect();
        let rows = (bounds.size.height / row).floor() as usize;
        // How many cells at the chain's lower end stand too far below the
        // higher to be laid out.
        let unseen = chain.len().saturating_sub(rows);
        let nearest = chain
            .iter()
            .enumerate()
            .skip(unseen)
            .find_map(|(at, &holds)| {
                let drawn = higher.iter().find(|cell| {
                    cell.holds == holds
                        || matches!(cell.holds, Holds::Rest(_))
                            && holds.stands_under(cell.holds, tree)
                })?;
                let rect = match drawn.holds == holds {
                    true => drawn.rect,
                    false => within(
                        drawn.rect,
                        icicle::share_of(tree, *drawn, holds, min_width)?,
                    ),
                };
                Some((at, rect))
            });
        let top = Bounds::new(bounds.origin, size(bounds.size.width, row));
        let (at, mut rect) = nearest.unwrap_or((chain.len() - 1, top));
        for below in (0..at).rev() {
            let over = Cell {
                rect,
                holds: chain[below + 1],
            };
            rect = within(rect, icicle::share_of(tree, over, chain[below], min_width)?);
            rect.origin.y += row;
        }
        Some(rect)
    }
}

/// `rect` narrowed to the left edge and width of `share`.
fn within(rect: Bounds<Pixels>, (left, width): (Pixels, Pixels)) -> Bounds<Pixels> {
    Bounds::new(point(left, rect.top()), size(width, rect.size.height))
}

/// The cells from `lower` up to the one standing directly under `higher` in
/// `tree`, the lower first; none where `higher` stands over no such path.
fn path(tree: &impl Tree, higher: Holds, lower: Holds) -> Option<Vec<Holds>> {
    let mut path = Vec::new();
    let mut at = lower;
    loop {
        path.push(at);
        if at.stands_under(higher, tree) {
            return Some(path);
        }
        at = at.parent(tree)?;
    }
}

/// `placed` carried from where `from` lies to where `to` does, scaled across
/// as `to` is to `from` and shifted down as far, as [`seen`] in `bounds`.
fn carried(
    placed: Placed,
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
) -> Option<Placed> {
    let rect = placed.cell.rect;
    let scale = to.size.width / from.size.width;
    let left = to.left() + (rect.left() - from.left()) * scale;
    let top = rect.top() + (to.top() - from.top());
    let width = rect.size.width * scale;
    seen(
        placed,
        Bounds::new(point(left, top), size(width, rect.size.height)),
        bounds,
    )
}

/// `placed` drawn at `rect` cut to `bounds`; none where it falls outside them
/// or too narrow to see.
fn seen(placed: Placed, rect: Bounds<Pixels>, bounds: Bounds<Pixels>) -> Option<Placed> {
    let (left, right) = (
        rect.left().max(bounds.left()),
        rect.right().min(bounds.right()),
    );
    let (top, height) = (rect.top(), rect.size.height);
    let shown = top < bounds.bottom() && top + height > bounds.top();
    (shown && right - left >= VISIBLE_WIDTH).then(|| Placed {
        cell: Cell {
            rect: Bounds::new(point(left, top), size(right - left, height)),
            holds: placed.cell.holds,
        },
        on_top: placed.on_top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exclusive::icicle::tests::{Laid, ROW, Spec, area, leaf};
    use gpui::px;

    /// Two roots, the first holding a child of its own and that child one.
    fn tree() -> Laid {
        Laid::of(&[Spec(6, vec![Spec(5, vec![leaf(2)])]), leaf(2)])
    }

    fn still(tree: &Laid, top: Holds, bounds: Bounds<Pixels>) -> Vec<Placed> {
        Placed::still(icicle::lay_out(tree, top, bounds, ROW, px(1.)))
    }

    fn moving(
        transition: &Transition,
        tree: &Laid,
        bounds: Bounds<Pixels>,
        lowered: f32,
    ) -> Vec<Placed> {
        transition.cells(tree, bounds, bounds, ROW, px(1.), lowered)
    }

    /// Whether two cells hold the same and lie within a hair of each other.
    fn near(a: Cell, b: Cell) -> bool {
        let near = |a: Pixels, b: Pixels| (a - b).abs() < px(0.001);
        a.holds == b.holds
            && near(a.rect.left(), b.rect.left())
            && near(a.rect.right(), b.rect.right())
            && near(a.rect.top(), b.rect.top())
    }

    /// The same cells in the same order, each within a hair of the other.
    fn alike(drawn: &[Placed], expected: &[Placed]) -> bool {
        drawn.len() == expected.len()
            && drawn
                .iter()
                .zip(expected)
                .all(|(a, b)| a.on_top == b.on_top && near(a.cell, b.cell))
    }

    #[test]
    fn a_move_down_ends_on_the_lower_icicle_as_drawn_at_rest() {
        let (tree, bounds) = (tree(), area(100., 4.));
        let start = bviz_time::now();
        let down = Transition::between(&tree, Holds::Build, Holds::Node(0), start).unwrap();
        let ended = moving(&down, &tree, bounds, 1.);
        assert!(
            alike(&ended, &still(&tree, Holds::Node(0), bounds)),
            "{ended:#?}"
        );
    }

    /// The same cells, in any order, each within a hair of the other.
    fn same_cells(drawn: &[Placed], expected: &[Placed]) -> bool {
        drawn.len() == expected.len()
            && expected.iter().all(|placed| {
                let matched = drawn
                    .iter()
                    .find(|drawn| drawn.cell.holds == placed.cell.holds);
                matched.is_some_and(|drawn| near(drawn.cell, placed.cell))
            })
    }

    /// The narrowest cell drawn apart in the trees whose rest cells these
    /// tests draw.
    const MIN: Pixels = px(4.);

    /// Two roots, the first holding children too many to draw apart all
    /// under it at rest from the whole build, though not from itself.
    fn gathering() -> Laid {
        let first = Spec(50, vec![leaf(30), leaf(10), leaf(4), leaf(3), leaf(2)]);
        Laid::of(&[first, leaf(50)])
    }

    /// At the start of a move down the frame is the higher icicle as drawn at
    /// rest, the lower icicle's cells standing exactly where the higher drew
    /// its own and gathering the same rest cell; at the end of a move up it
    /// is the same.
    #[test]
    fn a_move_down_starts_from_the_higher_icicle_as_drawn_at_rest() {
        let (tree, bounds) = (gathering(), area(100., 4.));
        let at_rest = Placed::still(icicle::lay_out(&tree, Holds::Build, bounds, ROW, MIN));
        let rest = at_rest
            .iter()
            .filter(|placed| matches!(placed.cell.holds, Holds::Rest(_)));
        assert_eq!(rest.count(), 1, "{at_rest:#?}");
        let start = bviz_time::now();
        let down = Transition::between(&tree, Holds::Build, Holds::Node(0), start).unwrap();
        let started = down.cells(&tree, bounds, bounds, ROW, MIN, 0.);
        assert!(same_cells(&started, &at_rest), "{started:#?}");
        let up = Transition::between(&tree, Holds::Node(0), Holds::Build, start).unwrap();
        let ended = up.cells(
            &tree,
            bounds,
            bounds,
            ROW,
            MIN,
            up.lowered(start + DURATION),
        );
        assert!(same_cells(&ended, &at_rest), "{ended:#?}");
    }

    /// Going up is a move down run backwards: it starts on the lower icicle
    /// and ends on the higher.
    #[test]
    fn a_move_up_runs_from_the_lower_icicle_to_the_higher() {
        let tree = tree();
        let start = bviz_time::now();
        let up = Transition::between(&tree, Holds::Node(0), Holds::Build, start).unwrap();
        let down = Transition::between(&tree, Holds::Build, Holds::Node(0), start).unwrap();
        assert_eq!(up.lowered(start), 1.);
        assert_eq!(up.lowered(start + DURATION), 0.);
        assert_eq!(down.lowered(start), 0.);
        assert_eq!(down.lowered(start + DURATION), 1.);
        let bounds = area(100., 4.);
        assert_eq!(
            moving(&up, &tree, bounds, 0.5),
            moving(&down, &tree, bounds, 0.5)
        );
    }

    #[test]
    fn a_move_eases_out_and_ends_after_its_length() {
        let tree = tree();
        let start = bviz_time::now();
        let down = Transition::between(&tree, Holds::Build, Holds::Node(1), start).unwrap();
        let half = down.lowered(start + DURATION / 2);
        assert!(half > 0.5, "{half}");
        assert!(!down.ended(start + DURATION / 2));
        assert!(down.ended(start + DURATION));
        assert_eq!(down.lowered(start + DURATION * 2), 1.);
    }

    /// Mid-way the cell clicked spans between where it was and the top row,
    /// and the icicle drawn from it is fitted under it.
    #[test]
    fn mid_way_the_cell_clicked_grows_towards_the_top_row() {
        let (tree, bounds) = (tree(), area(100., 4.));
        let down =
            Transition::between(&tree, Holds::Build, Holds::Node(1), bviz_time::now()).unwrap();
        let cells = moving(&down, &tree, bounds, 0.5);
        let clicked = cells
            .iter()
            .find(|placed| placed.on_top && placed.cell.holds == Holds::Node(1));
        let rect = clicked.unwrap().cell.rect;
        // At rest the second root spans 75..100 of the second row.
        assert_eq!((rect.left(), rect.top()), (px(37.5), ROW * 0.5));
        assert_eq!(rect.right(), px(100.));
    }

    #[test]
    fn a_move_between_cells_neither_over_the_other_is_none() {
        let tree = tree();
        let now = bviz_time::now();
        assert!(Transition::between(&tree, Holds::Node(1), Holds::Node(2), now).is_none());
        assert!(Transition::between(&tree, Holds::Node(1), Holds::Node(1), now).is_none());
    }

    /// A cell too narrow to draw apart in the higher icicle moves from its
    /// share of the rest cell gathering it.
    #[test]
    fn a_cell_gathered_in_a_rest_cell_moves_from_its_share_of_it() {
        let tree = Laid::of(&[leaf(97), leaf(2), leaf(1)]);
        let bounds = area(100., 3.);
        let at_rest = Placed::still(icicle::lay_out(&tree, Holds::Build, bounds, ROW, MIN));
        let rest = at_rest
            .iter()
            .find(|placed| matches!(placed.cell.holds, Holds::Rest(_)))
            .unwrap()
            .cell
            .rect;
        let down =
            Transition::between(&tree, Holds::Build, Holds::Node(2), bviz_time::now()).unwrap();
        let started = down.cells(&tree, bounds, bounds, ROW, MIN, 0.);
        let lower_top = started
            .iter()
            .find(|placed| placed.on_top && placed.cell.holds == Holds::Node(2));
        let third = rest.size.width / 3.;
        let expected = Cell {
            rect: Bounds::new(
                point(rest.left() + third * 2., rest.top()),
                size(third, ROW),
            ),
            holds: Holds::Node(2),
        };
        assert!(near(lower_top.unwrap().cell, expected), "{started:#?}");
    }

    /// A cell the higher icicle leaves out, too narrow even for a rest cell,
    /// moves from its share of the cell over it rather than the whole of it,
    /// and so is not drawn at the start of the move, as at rest.
    #[test]
    fn a_cell_left_out_moves_from_its_share_of_the_cell_over_it() {
        let tree = Laid::of(&[Spec(1002, vec![leaf(1000), leaf(1)])]);
        let (bounds, min) = (area(100., 3.), px(5.));
        let at_rest = Placed::still(icicle::lay_out(&tree, Holds::Build, bounds, ROW, min));
        assert!(
            !at_rest
                .iter()
                .any(|placed| placed.cell.holds == Holds::Node(2))
        );
        let down =
            Transition::between(&tree, Holds::Build, Holds::Node(2), bviz_time::now()).unwrap();
        let higher: Vec<Cell> = at_rest.iter().map(|placed| placed.cell).collect();
        let anchor = down.anchor(&tree, &higher, bounds, ROW, min).unwrap();
        let per_point = px(100.) / 1002.;
        let expected = Cell {
            rect: Bounds::new(point(per_point * 1000., ROW * 2.), size(per_point, ROW)),
            holds: Holds::Node(2),
        };
        let anchored = Cell {
            rect: anchor,
            holds: Holds::Node(2),
        };
        assert!(near(anchored, expected), "{anchor:?}");
        let started = down.cells(&tree, bounds, bounds, ROW, min, 0.);
        assert!(same_cells(&started, &at_rest), "{started:#?}");
    }

    /// A run of roots on top that the whole build no longer gathers as one,
    /// as when it was opened at another width, moves from its share of the
    /// row of the roots.
    #[test]
    fn a_run_no_longer_gathered_moves_from_its_share_of_the_roots() {
        let tree = Laid::of(&[leaf(40), leaf(30), leaf(10), leaf(10), leaf(10)]);
        let min = px(20.);
        let narrow = icicle::lay_out(&tree, Holds::Build, area(50., 2.), ROW, min);
        let run = narrow.last().unwrap().holds;
        let Holds::Rest(gathered) = run else {
            panic!("{narrow:#?}");
        };
        assert_eq!(gathered.siblings(&tree), Some(&[1, 2, 3, 4][..]));
        let bounds = area(100., 2.);
        let higher = icicle::lay_out(&tree, Holds::Build, bounds, ROW, min);
        assert!(!higher.iter().any(|cell| cell.holds == run
            || matches!(cell.holds, Holds::Rest(_)) && run.stands_under(cell.holds, &tree)));
        let down = Transition::between(&tree, Holds::Build, run, bviz_time::now()).unwrap();
        let anchor = down.anchor(&tree, &higher, bounds, ROW, min).unwrap();
        assert_eq!(anchor, Bounds::new(point(px(40.), ROW), size(px(60.), ROW)));
    }

    /// A cell several rows below the focus moves from its own place, its
    /// path to the focus taken through every cell between.
    #[test]
    fn a_cell_rows_below_moves_from_its_own_place() {
        let (tree, bounds) = (tree(), area(100., 4.));
        let down =
            Transition::between(&tree, Holds::Build, Holds::Node(3), bviz_time::now()).unwrap();
        assert_eq!(down.path, [Holds::Node(3), Holds::Node(2), Holds::Node(0)]);
        let started = moving(&down, &tree, bounds, 0.);
        let lower_top = started
            .iter()
            .find(|placed| placed.on_top && placed.cell.holds == Holds::Node(3));
        assert_eq!(lower_top.unwrap().cell.rect.top(), ROW * 3.);
    }
}
