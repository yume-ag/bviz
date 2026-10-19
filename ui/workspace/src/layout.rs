use crate::Pane;
use gpui::{Axis, Entity};
use std::sync::atomic::{AtomicU64, Ordering};

/// The share of a split's extent no side may be squeezed below, so a divider
/// dragged to one end leaves the side it shrinks visible and its own divider
/// still within reach.
pub(crate) const MIN_PANE_SHARE: f32 = 0.05;

/// The share a split gives its leading side when nothing asks for another.
const EVEN_SHARE: f32 = 0.5;

/// The share of what a pane asked for that it keeps when
/// [`Layout::insert_beside`] lays another beside it, the two of them dividing
/// what it held between them.
pub(crate) const BESIDE_SHARE: f32 = 0.5;

/// The least a side of a split can ask for, so that the flexes of a split
/// always add up to something positive to divide a share out of.
const LEAST_FLEX: f32 = f32::MIN_POSITIVE;

/// What every side of an evened split asks for. A share is divided out of what
/// the sides ask for between them, so any one flex they all hold divides the
/// extent evenly and which one it is does not matter.
const EVEN_FLEX: f32 = 1.0;

/// Identity of one side of a split, minted where the side is laid out and kept
/// by it wherever the arrangement lays it out afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SideId(u64);

impl SideId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Identity of one divider: the side it is laid out against, which is the side
/// laid out just after it. A side keeps its identity as the sides around it come
/// and go, so a drag holds the same boundary for as long as that boundary is
/// laid out; and only [`Split::sides`] mints one, only against a side laid out
/// after another, so an id whose own boundary has gone names nothing anywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DividerId(SideId);

impl DividerId {
    pub fn as_u64(self) -> u64 {
        self.0.0
    }
}

/// Where a pane is put: beside another, or against an edge of the window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    /// The axis a split lays its sides out along to part them this way.
    pub fn axis(self) -> Axis {
        match self {
            Side::Left | Side::Right => Axis::Horizontal,
            Side::Top | Side::Bottom => Axis::Vertical,
        }
    }

    /// Whether what is put here is laid out ahead of what it is put beside.
    pub(crate) fn leads(self) -> bool {
        matches!(self, Side::Left | Side::Top)
    }
}

/// What an arrangement lays a pane put against one of its edges out at: the
/// share it takes of the room the dividers leave the sides of the split it
/// joins, and how many dividers that split then lays out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AtEdge {
    pub share: f32,
    pub dividers: usize,
}

/// What one side of a split asks for of the split's extent, against what its
/// siblings ask for. Only [`Flex::new`] mints one, so every flex is positive and
/// finite.
#[derive(Clone, Copy, Debug)]
struct Flex(f32);

impl Flex {
    /// `shares` of the extent, coerced into the positive finite range: a side
    /// asking for none of the extent, or for a share that is not a number, is
    /// asking for the least there is to ask for rather than for none.
    fn new(shares: f32) -> Self {
        Self(match shares.is_finite() && shares > LEAST_FLEX {
            true => shares,
            false => LEAST_FLEX,
        })
    }

    fn shares(self) -> f32 {
        self.0
    }
}

/// How a window's panes are arranged: panes, and the splits that lay several
/// arrangements out one after another along an axis with a draggable divider at
/// every boundary between them.
///
/// Every arrangement is in normal form: no side of a split is itself a split
/// along that split's own axis. One built or left as one is spliced in beside
/// its new siblings with its own flexes rescaled to the share it held, so every
/// divider parts sides of the one split that lays it out, and a pane closed
/// anywhere leaves the rest laid out as they were.
///
/// The leaf type is a parameter so the arrangement's algebra can be exercised on
/// plain values, away from the window real panes need.
#[derive(Clone, Debug)]
pub enum Layout<P = Entity<Pane>> {
    Pane(P),
    Split(Split<P>),
}

/// Two or more arrangements laid out one after another along an axis.
#[derive(Clone, Debug)]
pub struct Split<P = Entity<Pane>> {
    axis: Axis,
    children: Vec<Child<P>>,
}

/// One side of a split.
#[derive(Clone, Debug)]
struct Child<P> {
    id: SideId,
    layout: Layout<P>,
    flex: Flex,
    /// What this side asked for when its split was built, `None` for a side
    /// that joined the split afterwards. A split at rest lays its sides out at
    /// what they were built asking for where every side it holds was built with
    /// it, and evenly where any joined it since.
    built: Option<Flex>,
}

impl<P> Child<P> {
    /// A side of a split being built, asking for `flex`.
    fn built(layout: Layout<P>, flex: Flex) -> Self {
        Self {
            id: SideId::next(),
            layout,
            flex,
            built: Some(flex),
        }
    }

    /// A side joining a split already built, asking for `flex`.
    fn joined(layout: Layout<P>, flex: Flex) -> Self {
        Self {
            id: SideId::next(),
            layout,
            flex,
            built: None,
        }
    }
}

/// One side of a split, as the split lays it out.
pub struct SplitSide<'a, P = Entity<Pane>> {
    /// The divider laid out against this side, which every side but the one the
    /// split lays out first has.
    pub divider: Option<DividerId>,
    pub layout: &'a Layout<P>,
    /// The share of the split's extent this side is laid out at.
    pub share: f32,
}

impl<P> Split<P> {
    pub fn axis(&self) -> Axis {
        self.axis
    }

    /// The least share of the extent this split lays a side out at, which is
    /// [`MIN_PANE_SHARE`] until the split holds more sides than that floor
    /// leaves room for, and from there an even share of the split.
    fn floor(&self) -> f32 {
        MIN_PANE_SHARE.min(1.0 / self.children.len() as f32)
    }

    /// Each side in the order it is laid out. A side's share is what it asks for
    /// out of what every side asks for between them, floored at
    /// [`Split::floor`], and the floors come out of what the sides above them
    /// divide — so the shares add up to the whole extent however far a drag has
    /// pushed one side.
    pub fn sides(&self) -> Vec<SplitSide<'_, P>> {
        let flexes: Vec<f32> = self
            .children
            .iter()
            .map(|child| child.flex.shares())
            .collect();
        let floor = self.floor();
        let mut at_floor = vec![false; flexes.len()];
        // Holding a squeezed side at the floor leaves the sides above it less to
        // divide, which can squeeze another below the floor in turn. At least
        // one side always stays above it, so every pass but the last settles a
        // further side and the pass that settles none is the answer.
        let shares: Vec<f32> = loop {
            let mut free_flex = 0.0;
            let mut held = 0;
            for (flex, at_floor) in flexes.iter().zip(&at_floor) {
                match at_floor {
                    true => held += 1,
                    false => free_flex += flex,
                }
            }
            let scale = (1.0 - floor * held as f32) / free_flex;
            let mut settled = true;
            for (flex, at_floor) in flexes.iter().zip(at_floor.iter_mut()) {
                if !*at_floor && flex * scale < floor {
                    *at_floor = true;
                    settled = false;
                }
            }
            if settled {
                break flexes
                    .iter()
                    .zip(&at_floor)
                    .map(|(flex, at_floor)| match at_floor {
                        true => floor,
                        false => flex * scale,
                    })
                    .collect();
            }
        };
        self.children
            .iter()
            .zip(shares)
            .enumerate()
            .map(|(at, (child, share))| SplitSide {
                divider: (at > 0).then_some(DividerId(child.id)),
                layout: &child.layout,
                share,
            })
            .collect()
    }

    /// Where `divider` parts this split's sides: the boundary ahead of the side
    /// it is laid out against. `None` where this split lays out no such side, or
    /// lays it out first, with no boundary ahead of it.
    fn boundary_at(&self, divider: DividerId) -> Option<usize> {
        let DividerId(against) = divider;
        let at = self.children.iter().position(|child| child.id == against)?;
        at.checked_sub(1)
    }

    /// Puts the divider at `boundary` at `share` of the split's extent by
    /// re-dividing between the two sides it parts what those two hold, leaving
    /// every other side asking for exactly what it asked for.
    fn divide_at(&mut self, boundary: usize, share: f32) {
        let shares: Vec<f32> = self.sides().into_iter().map(|side| side.share).collect();
        let ahead: f32 = shares[..boundary].iter().sum();
        let between = shares[boundary] + shares[boundary + 1];
        let least = self.floor();
        let leading = (share - ahead).clamp(least, between - least);
        let parted =
            self.children[boundary].flex.shares() + self.children[boundary + 1].flex.shares();
        self.children[boundary].flex = Flex::new(parted * leading / between);
        self.children[boundary + 1].flex = Flex::new(parted * (between - leading) / between);
    }

    /// Leaves every side asking for as much as every other, so that the split
    /// lays them all out at the same share of its extent.
    fn even(&mut self) {
        for child in &mut self.children {
            child.flex = Flex::new(EVEN_FLEX);
        }
    }

    /// Lays every side out at what it asks for at rest.
    fn rest(&mut self) {
        let built: Option<Vec<Flex>> = self.children.iter().map(|child| child.built).collect();
        match built {
            Some(built) => {
                for (child, flex) in self.children.iter_mut().zip(built) {
                    child.flex = flex;
                }
            }
            None => self.even(),
        }
    }

    /// Brings the side at `at` into normal form. A spliced side asks for its
    /// share of what the side it replaces asked for, and was built asking for
    /// its share of what that side was built asking for.
    fn splice(&mut self, at: usize) {
        let child = self.children.remove(at);
        match child.layout {
            Layout::Split(inner) if inner.axis == self.axis => {
                let total: f32 = inner.children.iter().map(|side| side.flex.shares()).sum();
                let built_total: Option<f32> = inner
                    .children
                    .iter()
                    .map(|side| side.built.map(Flex::shares))
                    .sum();
                let spliced = inner.children.into_iter().map(|side| {
                    let built = match (child.built, side.built, built_total) {
                        (Some(whole), Some(part), Some(total)) => {
                            Some(Flex::new(whole.shares() * part.shares() / total))
                        }
                        _ => None,
                    };
                    Child {
                        flex: Flex::new(child.flex.shares() * side.flex.shares() / total),
                        built,
                        ..side
                    }
                });
                self.children.splice(at..at, spliced);
            }
            layout => self.children.insert(at, Child { layout, ..child }),
        }
    }
}

/// What removing a pane did to an arrangement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Removal {
    /// No pane here is that one.
    NotFound,
    /// The pane is gone and panes are left.
    Removed,
    /// The arrangement was that one pane, so nothing of it is left and whoever
    /// holds it is done with it.
    Emptied,
}

impl<P> Layout<P> {
    /// `first` and `second` either side of a divider along `axis`, the first
    /// side laid out at `ratio` of the extent they share.
    pub fn split_at(axis: Axis, ratio: f32, first: Layout<P>, second: Layout<P>) -> Self {
        Self::divided(axis, (first, ratio), vec![(second, 1.0 - ratio)])
    }

    /// `first` and then each of `rest` laid out one after another along `axis`,
    /// each at the flex of the extent it asks for. A lone arrangement is laid
    /// out in place of the split it would be the only side of.
    pub fn divided(axis: Axis, first: (Layout<P>, f32), rest: Vec<(Layout<P>, f32)>) -> Self {
        let flexed = |(layout, flex): (Layout<P>, f32)| Child::built(layout, Flex::new(flex));
        let mut children = Vec::with_capacity(1 + rest.len());
        children.push(flexed(first));
        children.extend(rest.into_iter().map(flexed));
        let mut split = Split { axis, children };
        // Back to front, so splicing a side in leaves the sides still to visit
        // where this loop expects them.
        for at in (0..split.children.len()).rev() {
            split.splice(at);
        }
        match split.children.len() {
            1 => split.children.remove(0).layout,
            _ => Layout::Split(split),
        }
    }

    /// The pane laid out first, which a window opens showing.
    pub fn first_pane(&self) -> &P {
        let mut layout = self;
        loop {
            match layout {
                Layout::Pane(pane) => return pane,
                Layout::Split(split) => layout = &split.children[0].layout,
            }
        }
    }

    /// What [`Layout::insert_at_edge`] lays a pane put against `side` of the
    /// whole arrangement out at: an even share of the split it joins, or half
    /// of the arrangement it is parted from.
    pub fn at_edge(&self, side: Side) -> AtEdge {
        match self {
            Layout::Split(split) if split.axis == side.axis() => AtEdge {
                share: 1.0 / (split.children.len() + 1) as f32,
                dividers: split.children.len(),
            },
            _ => AtEdge {
                share: EVEN_SHARE,
                dividers: 1,
            },
        }
    }

    /// The panes, in the order they are laid out.
    pub fn panes(&self) -> Panes<'_, P> {
        Panes {
            unvisited: vec![self],
        }
    }

    /// Hands the split the divider belongs to the share of its extent a drag
    /// has moved that divider to, reporting whether the arrangement holds it.
    pub fn move_divider(&mut self, divider: DividerId, share: f32) -> bool {
        match self.parted_by(divider) {
            Some((split, boundary)) => {
                split.divide_at(boundary, share);
                true
            }
            None => false,
        }
    }

    /// Gives every side of the split the divider belongs to the same share of
    /// that split's extent, reporting whether the arrangement holds it. The
    /// sides that are splits themselves go on dividing what they are given
    /// exactly as they did.
    pub fn even_split(&mut self, divider: DividerId) -> bool {
        match self.parted_by(divider) {
            Some((split, _)) => {
                split.even();
                true
            }
            None => false,
        }
    }

    /// Lays every side of the split the divider belongs to out at what it asks
    /// for at rest, reporting whether the arrangement holds it.
    pub fn rest_split(&mut self, divider: DividerId) -> bool {
        match self.parted_by(divider) {
            Some((split, _)) => {
                split.rest();
                true
            }
            None => false,
        }
    }

    /// The split `divider` belongs to, and the boundary between that split's
    /// sides it parts. `None` where the arrangement parts no sides by it.
    fn parted_by(&mut self, divider: DividerId) -> Option<(&mut Split<P>, usize)> {
        let Layout::Split(split) = self else {
            return None;
        };
        match split.boundary_at(divider) {
            Some(boundary) => Some((split, boundary)),
            None => split
                .children
                .iter_mut()
                .find_map(|child| child.layout.parted_by(divider)),
        }
    }
}

impl<P: PartialEq> Layout<P> {
    pub fn contains(&self, target: &P) -> bool {
        self.panes().any(|pane| pane == target)
    }

    fn is_pane(&self, target: &P) -> bool {
        matches!(self, Layout::Pane(pane) if pane == target)
    }

    /// A divider of the innermost split laying `target` out, which is how that
    /// split is named to [`Layout::even_split`]: every divider of a split names
    /// that same split, so which of them this is does not matter. `None` where
    /// no split lays `target` out, the arrangement being that pane alone.
    pub fn innermost_divider(&self, target: &P) -> Option<DividerId> {
        let Layout::Split(split) = self else {
            return None;
        };
        let side = split
            .children
            .iter()
            .find(|child| child.layout.contains(target))?;
        side.layout
            .innermost_divider(target)
            .or_else(|| split.children.get(1).map(|child| DividerId(child.id)))
    }

    /// Whether `target` is already laid out spanning `side` of the whole
    /// arrangement, which is the place [`Layout::insert_at_edge`] puts a pane:
    /// the arrangement is that pane, or its outermost split lays its sides out
    /// along that side's axis and lays that pane out at that end of them.
    pub fn spans_edge(&self, target: &P, side: Side) -> bool {
        match self {
            Layout::Pane(pane) => pane == target,
            Layout::Split(split) => {
                split.axis == side.axis()
                    && match side.leads() {
                        true => split.children.first(),
                        false => split.children.last(),
                    }
                    .is_some_and(|child| child.layout.is_pane(target))
            }
        }
    }
}

impl<P: Clone + PartialEq> Layout<P> {
    /// Puts `new` in a pane of its own on `side` of `target`, reporting whether
    /// the arrangement holds `target`. Where the split holding `target` already
    /// lays its sides out along that side's axis, `new` joins it as `target`'s
    /// neighbour and takes half of what `target` asked for, so every other side
    /// goes on asking for what it asked for; across that axis, `target` is
    /// parted from `new` by a split of two dividing evenly what `target` held.
    pub fn insert_beside(&mut self, target: &P, side: Side, new: P) -> bool {
        self.insert_beside_at(target, side, new, EVEN_SHARE)
    }

    /// [`insert_beside`](Self::insert_beside), a split of two parting
    /// `target` from `new` laying its leading side out at `leading` of the
    /// extent.
    pub(crate) fn insert_beside_at(
        &mut self,
        target: &P,
        side: Side,
        new: P,
        leading: f32,
    ) -> bool {
        let split = match self {
            Layout::Pane(pane) => {
                if pane != target {
                    return false;
                }
                let held = Layout::Pane(pane.clone());
                *self = Layout::parted(side, held, Layout::Pane(new), leading);
                return true;
            }
            Layout::Split(split) => split,
        };
        let Some(at) = split
            .children
            .iter()
            .position(|child| child.layout.contains(target))
        else {
            return false;
        };
        if split.axis != side.axis() || !split.children[at].layout.is_pane(target) {
            return split.children[at]
                .layout
                .insert_beside_at(target, side, new, leading);
        }
        let half = Flex::new(split.children[at].flex.shares() * BESIDE_SHARE);
        split.children[at].flex = half;
        let at = match side.leads() {
            true => at,
            false => at + 1,
        };
        split
            .children
            .insert(at, Child::joined(Layout::Pane(new), half));
        true
    }

    /// Puts `new` in a pane of its own spanning `side` of the whole
    /// arrangement. Where the arrangement is a split laying its sides out along
    /// that side's axis, `new` joins it asking for what its sides ask for
    /// between them on average, so that `new` takes an even share of the split
    /// and every other side goes on asking for what it asked for; across that
    /// axis, the whole arrangement is parted from `new` by a split of two.
    pub fn insert_at_edge(&mut self, side: Side, new: P) {
        self.insert_at_edge_at(side, new, EVEN_SHARE);
    }

    /// [`insert_at_edge`](Self::insert_at_edge), a split of two parting the
    /// whole arrangement from `new` laying its leading side out at `leading`
    /// of the extent.
    pub(crate) fn insert_at_edge_at(&mut self, side: Side, new: P, leading: f32) {
        if let Layout::Split(split) = self
            && split.axis == side.axis()
        {
            let asked: f32 = split.children.iter().map(|child| child.flex.shares()).sum();
            let even = Flex::new(asked / split.children.len() as f32);
            let at = match side.leads() {
                true => 0,
                false => split.children.len(),
            };
            split
                .children
                .insert(at, Child::joined(Layout::Pane(new), even));
            return;
        }
        let held = std::mem::replace(self, Layout::Pane(new.clone()));
        *self = Layout::parted(side, held, Layout::Pane(new), leading);
    }

    /// `held` and `new` either side of a divider, `new` on `side` of `held`,
    /// the leading one laid out at `leading` of what they are laid out in.
    fn parted(side: Side, held: Layout<P>, new: Layout<P>, leading: f32) -> Self {
        let (first, second) = match side.leads() {
            true => (new, held),
            false => (held, new),
        };
        Layout::split_at(side.axis(), leading, first, second)
    }

    /// Takes `target` out of the arrangement. What it asked for of its split
    /// goes to the sides beside it in proportion to what they ask for, and a
    /// split left with one side is replaced by that side — at every level the
    /// removal unwinds through.
    pub fn remove(&mut self, target: &P) -> Removal {
        let split = match self {
            Layout::Pane(pane) => {
                return match pane == target {
                    true => Removal::Emptied,
                    false => Removal::NotFound,
                };
            }
            Layout::Split(split) => split,
        };
        let mut removal = Removal::NotFound;
        let mut at = 0;
        for (index, child) in split.children.iter_mut().enumerate() {
            removal = child.layout.remove(target);
            if removal != Removal::NotFound {
                at = index;
                break;
            }
        }
        match removal {
            Removal::NotFound => return Removal::NotFound,
            Removal::Emptied => {
                split.children.remove(at);
            }
            Removal::Removed => split.splice(at),
        }
        if split.children.len() == 1 {
            *self = split.children.remove(0).layout;
        }
        Removal::Removed
    }
}

/// The panes of an arrangement, left to right and top to bottom.
pub struct Panes<'a, P> {
    unvisited: Vec<&'a Layout<P>>,
}

impl<'a, P> Iterator for Panes<'a, P> {
    type Item = &'a P;

    fn next(&mut self) -> Option<&'a P> {
        while let Some(layout) = self.unvisited.pop() {
            match layout {
                Layout::Pane(pane) => return Some(pane),
                Layout::Split(split) => self
                    .unvisited
                    .extend(split.children.iter().rev().map(|child| &child.layout)),
            }
        }
        None
    }
}

/// An arrangement written out: a pane as its name, a split as its sides in
/// parentheses, parted by `|` where it lays them out left to right and by `/`
/// where it lays them out one above the other.
#[cfg(test)]
pub(crate) fn shape<P: std::fmt::Display>(layout: &Layout<P>) -> String {
    match layout {
        Layout::Pane(pane) => pane.to_string(),
        Layout::Split(split) => {
            let between = match split.axis {
                Axis::Horizontal => "|",
                Axis::Vertical => "/",
            };
            let sides: Vec<String> = split
                .children
                .iter()
                .map(|child| shape(&child.layout))
                .collect();
            format!("({})", sides.join(between))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SHARE_ROUNDING, letter};

    /// A pane and the two more it is split off to the right of, all three laid
    /// out side by side.
    fn three_panes() -> Layout<char> {
        let mut layout = Layout::Pane('a');
        assert!(layout.insert_beside(&'a', Side::Right, 'b'));
        assert!(layout.insert_beside(&'b', Side::Right, 'c'));
        layout
    }

    /// The pane named by its index into the alphabet, 'a' first.
    fn pane(index: usize) -> Layout<char> {
        Layout::Pane(letter(index))
    }

    /// Panes 'a', 'b', ... one after another along `axis`, the one at each index
    /// asking for the flex given for it.
    fn along(axis: Axis, flexes: &[f32]) -> Layout<char> {
        let sided = |(index, flex): (usize, &f32)| (pane(index), *flex);
        let mut sides = flexes.iter().enumerate().map(sided);
        let first = sides.next().expect("a split holds a pane");
        Layout::divided(axis, first, sides.collect())
    }

    /// Panes 'a', 'b', ... side by side.
    fn row(flexes: &[f32]) -> Layout<char> {
        along(Axis::Horizontal, flexes)
    }

    /// Panes 'a', 'b', ... one above another.
    fn column(flexes: &[f32]) -> Layout<char> {
        along(Axis::Vertical, flexes)
    }

    /// `count` panes 'a', 'b', ... side by side, each asking for as much as the
    /// others.
    fn even_row(count: usize) -> Layout<char> {
        row(&vec![1.0; count])
    }

    #[test]
    fn a_pane_put_beside_a_lone_one_parts_it_from_it_by_a_split_of_two() {
        let mut layout = Layout::Pane('a');
        assert!(layout.insert_beside(&'a', Side::Bottom, 'b'));
        assert_eq!(shape(&layout), "(a/b)");
        assert_eq!(layout.panes().copied().collect::<Vec<_>>(), ['a', 'b']);
        assert_shares(root(&layout), &[0.5, 0.5]);
    }

    /// A pane put on the leading side of another is laid out ahead of it,
    /// whether it joins the split that pane is in or is parted from it by one.
    #[test]
    fn a_pane_put_on_a_leading_side_is_laid_out_ahead_of_what_it_is_put_beside() {
        let mut lone = Layout::Pane('a');
        assert!(lone.insert_beside(&'a', Side::Left, 'z'));
        assert_eq!(shape(&lone), "(z|a)");

        let mut row = three_panes();
        assert!(row.insert_beside(&'b', Side::Left, 'z'));
        assert_eq!(shape(&row), "(a|z|b|c)");
    }

    /// Joining a split takes half of what the pane it is put beside asked for,
    /// which is the only share that moves: every other side of the split is laid
    /// out where it was.
    #[test]
    fn a_pane_joining_its_neighbours_split_halves_that_neighbour_alone() {
        let mut layout = three_panes();
        assert_shares(root(&layout), &[0.5, 0.25, 0.25]);
        assert!(layout.insert_beside(&'a', Side::Right, 'd'));
        assert_eq!(shape(&layout), "(a|d|b|c)");
        assert_shares(root(&layout), &[0.25, 0.25, 0.25, 0.25]);
    }

    /// Across the axis of the split it is in, the pane is parted from the new
    /// one by a split of its own, which takes the place and the share it held.
    #[test]
    fn a_pane_put_across_its_splits_axis_is_parted_from_the_new_one_in_its_place() {
        let mut layout = three_panes();
        assert!(layout.insert_beside(&'b', Side::Bottom, 'd'));
        assert_eq!(shape(&layout), "(a|(b/d)|c)");
        assert_shares(root(&layout), &[0.5, 0.25, 0.25]);
    }

    /// A pane put against an edge the arrangement is already laid out along
    /// joins it as the side laid out first or last, at an even share of the
    /// extent — which is a share of the split, not half of it — and the sides it
    /// joins go on asking for exactly what they asked for, so what they held is
    /// divided between them as it was.
    #[test]
    fn a_pane_put_against_an_edge_of_a_split_along_it_takes_an_even_share() {
        let mut leading = row(&[1.0, 1.0, 2.0]);
        leading.insert_at_edge(Side::Left, 'z');
        assert_eq!(shape(&leading), "(z|a|b|c)");
        assert_shares(root(&leading), &[0.25, 0.1875, 0.1875, 0.375]);

        let mut trailing = column(&[1.0, 1.0, 2.0]);
        trailing.insert_at_edge(Side::Bottom, 'z');
        assert_eq!(shape(&trailing), "(a/b/c/z)");
        assert_shares(root(&trailing), &[0.1875, 0.1875, 0.375, 0.25]);
    }

    /// A pane put against an edge takes the share of the arrangement that
    /// [`Layout::at_edge`] reports, over a split laying out as many dividers as
    /// it reports — which together mark the region a drop there would fill
    /// before it is dropped.
    #[test]
    fn what_a_pane_put_against_an_edge_takes_is_told_ahead_of_the_move() {
        let mut leading = row(&[1.0, 1.0, 2.0]);
        let at = leading.at_edge(Side::Left);
        leading.insert_at_edge(Side::Left, 'z');
        assert_shares(root(&leading), &[at.share, 0.1875, 0.1875, 0.375]);
        assert_eq!(at.dividers, dividers(&leading));

        let mut across = row(&[1.0, 3.0]);
        let at = across.at_edge(Side::Top);
        across.insert_at_edge(Side::Top, 'z');
        assert_shares(root(&across), &[at.share, 1.0 - at.share]);
        assert_eq!(at.dividers, dividers(&across));

        let mut lone = Layout::Pane('a');
        let at = lone.at_edge(Side::Bottom);
        lone.insert_at_edge(Side::Bottom, 'z');
        assert_shares(root(&lone), &[1.0 - at.share, at.share]);
        assert_eq!(at.dividers, dividers(&lone));
    }

    /// A pane put beside another keeps [`BESIDE_SHARE`] of what that one asked
    /// for, which is what marks the region a drop there would fill before it is
    /// dropped.
    #[test]
    fn what_a_pane_put_beside_another_takes_is_the_share_told_ahead_of_the_move() {
        let mut row = row(&[1.0, 1.0]);
        assert!(row.insert_beside(&'a', Side::Right, 'z'));
        assert_shares(root(&row), &[0.5 * BESIDE_SHARE, 0.5 * BESIDE_SHARE, 0.5]);

        let mut across = Layout::Pane('a');
        assert!(across.insert_beside(&'a', Side::Bottom, 'z'));
        assert_shares(root(&across), &[1.0 - BESIDE_SHARE, BESIDE_SHARE]);
    }

    /// The pane a move would put against an edge is the pane already there when
    /// the arrangement is that pane alone, or lays it out at that end of a split
    /// along that edge's own axis.
    #[test]
    fn a_pane_spans_an_edge_where_a_pane_put_there_would_stand() {
        let lone = Layout::Pane('a');
        assert!(lone.spans_edge(&'a', Side::Left));
        assert!(lone.spans_edge(&'a', Side::Bottom));

        let row = row(&[1.0, 1.0, 1.0]);
        assert!(row.spans_edge(&'a', Side::Left));
        assert!(row.spans_edge(&'c', Side::Right));
        assert!(!row.spans_edge(&'b', Side::Left));
        assert!(!row.spans_edge(&'b', Side::Right));
    }

    /// A pane at the end of a split laid out across an edge reaches only part of
    /// that edge, and so does one the split at that end lays out among others.
    #[test]
    fn a_pane_the_arrangement_lays_out_across_an_edge_does_not_span_it() {
        let row = row(&[1.0, 1.0]);
        assert!(!row.spans_edge(&'a', Side::Top));
        assert!(!row.spans_edge(&'b', Side::Bottom));

        let mut nested = row;
        assert!(nested.insert_beside(&'b', Side::Bottom, 'c'));
        assert_eq!(shape(&nested), "(a|(b/c))");
        assert!(!nested.spans_edge(&'b', Side::Right));
        assert!(!nested.spans_edge(&'c', Side::Right));
        assert!(nested.spans_edge(&'a', Side::Left));
    }

    /// Across the axis the arrangement is laid out along, every pane of it is
    /// parted from the new one by the one divider, which halves the window.
    #[test]
    fn a_pane_put_against_an_edge_across_the_arrangement_parts_all_of_it() {
        let mut row = row(&[1.0, 3.0]);
        row.insert_at_edge(Side::Top, 'z');
        assert_eq!(shape(&row), "(z/(a|b))");
        assert_shares(root(&row), &[0.5, 0.5]);
        assert_shares(nested(root(&row), 1), &[0.25, 0.75]);
    }

    /// A lone pane is laid out along no axis, so a pane put against any edge of
    /// it is parted from it by a split of two.
    #[test]
    fn a_pane_put_against_an_edge_of_a_lone_one_parts_it_from_it() {
        let mut layout = Layout::Pane('a');
        layout.insert_at_edge(Side::Bottom, 'z');
        assert_eq!(shape(&layout), "(a/z)");
        assert_shares(root(&layout), &[0.5, 0.5]);
    }

    #[test]
    fn putting_a_pane_beside_one_no_arrangement_holds_changes_nothing() {
        let mut layout = three_panes();
        assert!(!layout.insert_beside(&'z', Side::Right, 'd'));
        assert_eq!(shape(&layout), "(a|b|c)");
    }

    /// A split built with a side laid out along its own axis lays that side's
    /// own sides out beside their new siblings, each asking for its share of
    /// what the side it came from asked for — so the proportions the sides were
    /// built with hold from the first frame, over the whole extent.
    #[test]
    fn a_side_built_as_a_split_along_its_own_axis_is_spliced_into_it() {
        let inner = Layout::divided(
            Axis::Horizontal,
            (Layout::Pane('b'), 1.0),
            vec![(Layout::Pane('c'), 3.0)],
        );
        let layout = Layout::divided(
            Axis::Horizontal,
            (Layout::Pane('a'), 1.0),
            vec![(inner, 1.0)],
        );
        assert_eq!(shape(&layout), "(a|b|c)");
        assert_shares(root(&layout), &[0.5, 0.125, 0.375]);
    }

    /// The normal form is a property of every arrangement that can be built and
    /// of every one a pane can be put into or taken out of, not of the ones a
    /// close happens to pass through: no split anywhere lays out a side that is
    /// a split along its own axis.
    #[test]
    fn no_arrangement_lays_out_a_side_split_along_its_own_axis() {
        let nested_row = Layout::split_at(
            Axis::Horizontal,
            0.25,
            Layout::split_at(Axis::Horizontal, 0.75, pane(0), pane(1)),
            pane(2),
        );
        let nested_column = Layout::divided(
            Axis::Vertical,
            (row(&[1.0, 2.0]), 1.0),
            vec![(
                Layout::divided(Axis::Vertical, (pane(2), 1.0), vec![(pane(3), 5.0)]),
                3.0,
            )],
        );
        for seed in [three_panes(), nested_row, nested_column, even_row(4)] {
            explore(&seed, 3);
        }
    }

    /// Asserts the normal form of `layout` and of every arrangement reached from
    /// it by putting a pane beside one of its panes or against one of its edges,
    /// or closing one of them, within `depth` further changes.
    fn explore(layout: &Layout<char>, depth: usize) {
        assert_normal_form(layout);
        let panes: Vec<char> = layout.panes().copied().collect();
        for target in &panes {
            let mut closed = layout.clone();
            closed.remove(target);
            assert_normal_form(&closed);
        }
        if depth == 0 {
            return;
        }
        // Every pane is named for its place in the alphabet, and a seed names
        // them from 'a' with none skipped, so the pane after the last is free.
        let new = letter(panes.len());
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            let mut edged = layout.clone();
            edged.insert_at_edge(side, new);
            explore(&edged, depth - 1);
            for target in &panes {
                let mut wider = layout.clone();
                assert!(wider.insert_beside(target, side, new));
                explore(&wider, depth - 1);
            }
        }
    }

    fn assert_normal_form(layout: &Layout<char>) {
        let Layout::Split(split) = layout else {
            return;
        };
        for child in &split.children {
            if let Layout::Split(inner) = &child.layout {
                assert_ne!(
                    inner.axis,
                    split.axis,
                    "{} lays out a side split along its own axis",
                    shape(layout)
                );
            }
            assert_normal_form(&child.layout);
        }
    }

    #[test]
    fn closing_either_side_of_a_split_leaves_the_other_in_its_place() {
        let mut first_closed = Layout::Pane('a');
        first_closed.insert_beside(&'a', Side::Right, 'b');
        assert_eq!(first_closed.remove(&'a'), Removal::Removed);
        assert_eq!(shape(&first_closed), "b");

        let mut second_closed = Layout::Pane('a');
        second_closed.insert_beside(&'a', Side::Right, 'b');
        assert_eq!(second_closed.remove(&'b'), Removal::Removed);
        assert_eq!(shape(&second_closed), "a");
    }

    /// What a closed pane asked for is not left behind as a gap: the sides
    /// beside it grow into it in proportion to what each of them asks for.
    #[test]
    fn what_a_closed_pane_asked_for_goes_to_the_sides_left_in_proportion() {
        let mut layout = row(&[0.2, 0.3, 0.5]);
        assert_shares(root(&layout), &[0.2, 0.3, 0.5]);
        assert_eq!(layout.remove(&'a'), Removal::Removed);
        assert_eq!(shape(&layout), "(b|c)");
        assert_shares(root(&layout), &[0.375, 0.625]);
    }

    #[test]
    fn a_split_left_with_one_side_is_replaced_by_it_at_every_depth() {
        let mut layout = Layout::Pane('a');
        layout.insert_beside(&'a', Side::Right, 'd');
        layout.insert_beside(&'a', Side::Bottom, 'c');
        layout.insert_beside(&'a', Side::Right, 'b');
        assert_eq!(shape(&layout), "(((a|b)/c)|d)");

        // The innermost split loses a side, and the side left takes its place
        // while the two splits above it stand.
        assert_eq!(layout.remove(&'b'), Removal::Removed);
        assert_eq!(shape(&layout), "((a/c)|d)");
        // Each further close collapses the split that held the pane, one level
        // further out each time.
        assert_eq!(layout.remove(&'c'), Removal::Removed);
        assert_eq!(shape(&layout), "(a|d)");
        assert_eq!(layout.remove(&'a'), Removal::Removed);
        assert_eq!(shape(&layout), "d");
    }

    /// A side left alone by a close lands in the split above it, and where that
    /// side is itself a split laid out the same way, its own sides are spliced
    /// in beside their new siblings: they divide between them the share the side
    /// they came from held, in the proportions they had.
    #[test]
    fn a_side_landing_in_a_split_along_its_own_axis_is_spliced_into_it() {
        let mut layout = Layout::split_at(
            Axis::Horizontal,
            0.5,
            Layout::Pane('a'),
            Layout::split_at(
                Axis::Vertical,
                0.5,
                Layout::Pane('b'),
                Layout::split_at(Axis::Horizontal, 0.25, Layout::Pane('c'), Layout::Pane('d')),
            ),
        );
        assert_eq!(shape(&layout), "(a|(b/(c|d)))");
        assert_eq!(layout.remove(&'b'), Removal::Removed);
        assert_eq!(shape(&layout), "(a|c|d)");
        assert_shares(root(&layout), &[0.5, 0.125, 0.375]);
    }

    #[test]
    fn closing_the_last_pane_empties_the_arrangement() {
        let mut layout = Layout::Pane('a');
        assert_eq!(layout.remove(&'a'), Removal::Emptied);
    }

    #[test]
    fn closing_a_pane_no_arrangement_holds_changes_nothing() {
        let mut layout = three_panes();
        assert_eq!(layout.remove(&'z'), Removal::NotFound);
        assert_eq!(shape(&layout), "(a|b|c)");
    }

    /// A divider parts two sides and re-divides what those two hold between
    /// them, so dragging it leaves every other side of the split laid out where
    /// it was.
    #[test]
    fn a_divider_re_divides_the_two_sides_it_parts_and_no_others() {
        let mut layout = even_row(4);
        assert!(layout.move_divider(divider_at(root(&layout), 1), 0.4));
        assert_shares(root(&layout), &[0.25, 0.15, 0.35, 0.25]);
    }

    /// What the two sides hold is re-divided at the share the divider is put at,
    /// whatever share each of them held of it — which a split of sides asking
    /// for the same as one another cannot tell from dividing the extent itself.
    #[test]
    fn a_divider_re_divides_what_the_sides_it_parts_hold_however_unevenly_they_hold_it() {
        let mut layout = row(&[1.0, 2.0, 1.0]);
        assert_shares(root(&layout), &[0.25, 0.5, 0.25]);
        assert!(layout.move_divider(divider_at(root(&layout), 1), 0.6));
        assert_shares(root(&layout), &[0.25, 0.35, 0.4]);
    }

    /// Dragged past either of the sides it parts, a divider leaves that side the
    /// least a pane is laid out at, measured over the whole split — so the pane
    /// stays visible and the divider stays within reach of it.
    #[test]
    fn a_divider_dragged_past_a_side_leaves_it_the_least_a_pane_takes() {
        let mut layout = even_row(4);
        let divider = divider_at(root(&layout), 1);

        assert!(layout.move_divider(divider, 5.0));
        assert_shares(
            root(&layout),
            &[0.25, 0.5 - MIN_PANE_SHARE, MIN_PANE_SHARE, 0.25],
        );
        assert!(layout.move_divider(divider, -5.0));
        assert_shares(
            root(&layout),
            &[0.25, MIN_PANE_SHARE, 0.5 - MIN_PANE_SHARE, 0.25],
        );
    }

    /// A divider belongs to the split that lays out the side it is against, at
    /// whichever depth and boundary that is, and re-divides that split alone.
    #[test]
    fn a_divider_is_moved_by_the_split_it_belongs_to() {
        let row = Layout::divided(
            Axis::Horizontal,
            (pane(1), 1.0),
            vec![(pane(2), 1.0), (pane(3), 1.0)],
        );
        let mut layout = Layout::divided(Axis::Vertical, (pane(0), 1.0), vec![(row, 1.0)]);
        assert_eq!(shape(&layout), "(a/(b|c|d))");
        let inner = divider_at(nested(root(&layout), 1), 1);
        assert!(layout.move_divider(inner, 0.75));
        assert_shares(root(&layout), &[0.5, 0.5]);
        assert_shares(nested(root(&layout), 1), &[1.0 / 3.0, 5.0 / 12.0, 0.25]);
    }

    /// A drag holds one divider, which is the boundary against one side — so the
    /// sides beside it closing leaves it parting the same two arrangements, and
    /// its own side closing leaves it naming nothing.
    #[test]
    fn a_divider_names_the_same_boundary_as_the_sides_around_it_come_and_go() {
        let mut layout = even_row(4);
        let divider = divider_at(root(&layout), 1);
        assert_eq!(layout.remove(&'a'), Removal::Removed);
        assert_eq!(shape(&layout), "(b|c|d)");

        // The divider is against 'c', which the close leaves at the first
        // boundary, so the drag re-divides what 'b' and 'c' hold.
        assert!(layout.move_divider(divider, 0.5));
        assert_shares(root(&layout), &[0.5, 1.0 / 6.0, 1.0 / 3.0]);

        assert_eq!(layout.remove(&'c'), Removal::Removed);
        assert!(!layout.move_divider(divider, 0.5));
    }

    /// However far the sides of a split have been dragged apart, evening it
    /// leaves every one of them at the same share of its extent.
    #[test]
    fn evening_a_split_lays_every_side_out_at_the_same_share() {
        let mut layout = row(&[1.0, 7.0, 2.0]);
        assert_shares(root(&layout), &[0.1, 0.7, 0.2]);
        assert!(layout.even_split(divider_at(root(&layout), 1)));
        assert_shares(root(&layout), &[1.0 / 3.0; 3]);
    }

    /// A divider belongs to the split that lays out the side it is against, at
    /// whatever depth that split is, and evens that split alone.
    #[test]
    fn a_split_is_evened_by_a_divider_of_its_own_at_any_depth() {
        let column = Layout::divided(Axis::Vertical, (pane(1), 1.0), vec![(pane(2), 3.0)]);
        let mut layout = Layout::divided(Axis::Horizontal, (pane(0), 1.0), vec![(column, 3.0)]);
        assert_eq!(shape(&layout), "(a|(b/c))");
        let inner = divider_at(nested(root(&layout), 1), 0);
        assert!(layout.even_split(inner));
        assert_shares(root(&layout), &[0.25, 0.75]);
        assert_shares(nested(root(&layout), 1), &[0.5, 0.5]);
    }

    /// Evening reaches the sides of one split: the splits laid out beside it and
    /// the splits it lays out go on dividing what they are given as they did.
    #[test]
    fn evening_a_split_leaves_the_splits_around_it_as_they_were() {
        let held = Layout::divided(Axis::Horizontal, (pane(2), 1.0), vec![(pane(3), 4.0)]);
        let evened = Layout::divided(Axis::Vertical, (pane(1), 1.0), vec![(held, 3.0)]);
        let beside = Layout::divided(Axis::Vertical, (pane(4), 1.0), vec![(pane(5), 4.0)]);
        let mut layout = Layout::divided(
            Axis::Horizontal,
            (pane(0), 1.0),
            vec![(evened, 7.0), (beside, 2.0)],
        );
        assert_eq!(shape(&layout), "(a|(b/(c|d))|(e/f))");
        assert!(layout.even_split(divider_at(nested(root(&layout), 1), 0)));
        assert_shares(root(&layout), &[0.1, 0.7, 0.2]);
        assert_shares(nested(root(&layout), 1), &[0.5, 0.5]);
        assert_shares(nested(nested(root(&layout), 1), 1), &[0.2, 0.8]);
        assert_shares(nested(root(&layout), 2), &[0.2, 0.8]);
    }

    #[test]
    fn evening_a_split_no_arrangement_holds_changes_nothing() {
        let mut layout = three_panes();
        let elsewhere = even_row(3);
        assert!(!layout.even_split(divider_at(root(&elsewhere), 1)));
        assert_shares(root(&layout), &[0.5, 0.25, 0.25]);
    }

    /// A split built at proportions of its own rests at them: however its
    /// divider has been dragged, or the split evened, putting it back to rest
    /// lays its sides out where it was built to.
    #[test]
    fn a_built_split_rests_at_the_proportions_it_was_built_at() {
        let mut layout = row(&[1.0, 2.0]);
        let divider = divider_at(root(&layout), 0);
        assert!(layout.move_divider(divider, 0.8));
        assert!(layout.rest_split(divider));
        assert_shares(root(&layout), &[1.0 / 3.0, 2.0 / 3.0]);
        assert!(layout.even_split(divider));
        assert_shares(root(&layout), &[0.5, 0.5]);
        assert!(layout.rest_split(divider));
        assert_shares(root(&layout), &[1.0 / 3.0, 2.0 / 3.0]);
    }

    /// A split made by putting a pane beside another rests evenly.
    #[test]
    fn a_split_parting_a_pane_from_one_put_beside_it_rests_evenly() {
        let mut layout = Layout::Pane('a');
        assert!(layout.insert_beside(&'a', Side::Right, 'b'));
        let divider = divider_at(root(&layout), 0);
        assert!(layout.move_divider(divider, 0.2));
        assert!(layout.rest_split(divider));
        assert_shares(root(&layout), &[0.5, 0.5]);
    }

    /// Asserts that `split`, once its first divider has been dragged and then
    /// put back to rest, lays its sides out at `expected` shares.
    fn assert_rests_at(
        layout: &mut Layout<char>,
        split: impl Fn(&Layout<char>) -> &Split<char>,
        expected: &[f32],
    ) {
        let divider = divider_at(split(layout), 0);
        assert!(layout.move_divider(divider, 0.9));
        assert!(layout.rest_split(divider));
        assert_shares(split(layout), expected);
    }

    /// A side joining a built split, put beside one of its sides or against
    /// the edge it spans, leaves the split resting evenly.
    #[test]
    fn a_split_a_side_joins_rests_evenly() {
        let mut beside = row(&[1.0, 2.0]);
        assert!(beside.insert_beside(&'b', Side::Right, 'z'));
        assert_eq!(shape(&beside), "(a|b|z)");
        assert_rests_at(&mut beside, root, &[1.0 / 3.0; 3]);

        let mut at_edge = row(&[1.0, 2.0]);
        at_edge.insert_at_edge(Side::Left, 'z');
        assert_eq!(shape(&at_edge), "(z|a|b)");
        assert_rests_at(&mut at_edge, root, &[1.0 / 3.0; 3]);
    }

    /// A side leaving a built split leaves the sides it had divide what it took
    /// in the proportions they were built at.
    #[test]
    fn a_split_a_side_leaves_rests_at_its_built_proportions() {
        let mut layout = row(&[1.0, 2.0, 3.0]);
        assert_eq!(layout.remove(&'b'), Removal::Removed);
        assert_eq!(shape(&layout), "(a|c)");
        assert_rests_at(&mut layout, root, &[0.25, 0.75]);
    }

    /// A pane split off below another and then closed again leaves the split
    /// resting where it was built to.
    #[test]
    fn a_split_a_joined_side_leaves_again_rests_as_built() {
        let mut layout = column(&[2.0, 1.0]);
        assert!(layout.insert_beside(&'b', Side::Bottom, 'z'));
        assert_eq!(shape(&layout), "(a/b/z)");
        assert_eq!(layout.remove(&'z'), Removal::Removed);
        assert_rests_at(&mut layout, root, &[2.0 / 3.0, 1.0 / 3.0]);
    }

    /// A split spliced into one along its own axis divides what it was built
    /// asking for between its sides in the proportions they were built at,
    /// whether it is spliced in as the arrangement is built or once a pane
    /// closing leaves it there.
    #[test]
    fn a_spliced_split_rests_at_the_shares_its_sides_were_built_at() {
        let mut built = Layout::divided(
            Axis::Horizontal,
            (row(&[1.0, 3.0]), 1.0),
            vec![(pane(2), 1.0)],
        );
        assert_eq!(shape(&built), "(a|b|c)");
        assert_rests_at(&mut built, root, &[0.125, 0.375, 0.5]);

        let inner = Layout::divided(
            Axis::Vertical,
            (pane(3), 1.0),
            vec![(row(&[1.0, 4.0]), 1.0)],
        );
        let mut closed = Layout::divided(Axis::Horizontal, (pane(2), 1.0), vec![(inner, 2.0)]);
        assert!(closed.move_divider(divider_at(root(&closed), 0), 0.5));
        assert_eq!(closed.remove(&'d'), Removal::Removed);
        assert_eq!(shape(&closed), "(c|a|b)");
        assert_rests_at(&mut closed, root, &[1.0 / 3.0, 2.0 / 15.0, 8.0 / 15.0]);
    }

    /// A pane put against an edge across the arrangement's axis is parted from
    /// all of it by a split that rests evenly, and the arrangement it parts
    /// goes on resting as it was built to.
    #[test]
    fn a_pane_put_against_an_edge_across_the_axis_rests_beside_it_evenly() {
        let mut layout = row(&[1.0, 2.0]);
        layout.insert_at_edge(Side::Bottom, 'z');
        assert_eq!(shape(&layout), "((a|b)/z)");
        assert_rests_at(&mut layout, root, &[0.5, 0.5]);
        assert_rests_at(
            &mut layout,
            |layout| nested(root(layout), 0),
            &[1.0 / 3.0, 2.0 / 3.0],
        );
    }

    /// A split keeps its own proportions at rest while the arrangements laid
    /// out at its sides change within them: a side that is a split losing a
    /// pane, or a pane parted from one put across the split's axis.
    #[test]
    fn a_split_whose_sides_keep_their_place_rests_as_built() {
        let column = |top, bottom| {
            Layout::divided(Axis::Vertical, (pane(top), 2.0), vec![(pane(bottom), 1.0)])
        };
        let built = || {
            Layout::divided(
                Axis::Horizontal,
                (column(0, 1), 1.0),
                vec![(column(2, 3), 2.0)],
            )
        };
        let as_built = [1.0 / 3.0, 2.0 / 3.0];

        let mut closed = built();
        assert_eq!(closed.remove(&'b'), Removal::Removed);
        assert_eq!(shape(&closed), "(a|(c/d))");
        assert_rests_at(&mut closed, root, &as_built);

        let mut across = built();
        assert!(across.insert_beside(&'c', Side::Bottom, 'z'));
        assert_eq!(shape(&across), "((a/b)|(c/z/d))");
        assert_rests_at(&mut across, root, &as_built);
    }

    /// The split a gesture on a pane evens is the innermost one laying that pane
    /// out, which is named by any divider that split lays out; a pane laid out
    /// in no split at all is parted from nothing and names none.
    #[test]
    fn a_pane_names_the_innermost_split_laying_it_out() {
        let mut layout = Layout::Pane('a');
        assert_eq!(layout.innermost_divider(&'a'), None);

        assert!(layout.insert_beside(&'a', Side::Right, 'b'));
        assert!(layout.insert_beside(&'b', Side::Bottom, 'c'));
        assert_eq!(shape(&layout), "(a|(b/c))");
        assert_eq!(layout.innermost_divider(&'z'), None);

        let outer = layout
            .innermost_divider(&'a')
            .expect("a split lays 'a' out");
        let inner = layout
            .innermost_divider(&'b')
            .expect("a split lays 'b' out");
        assert!(layout.move_divider(outer, 0.25));
        assert!(layout.move_divider(inner, 0.75));

        // 'b' names the split parting it from 'c', which is the only one evened.
        assert!(layout.even_split(inner));
        assert_shares(root(&layout), &[0.25, 0.75]);
        assert_shares(nested(root(&layout), 1), &[0.5, 0.5]);

        assert!(layout.even_split(outer));
        assert_shares(root(&layout), &[0.5, 0.5]);
    }

    #[test]
    fn moving_a_divider_no_arrangement_holds_changes_nothing() {
        let mut layout = three_panes();
        let elsewhere = even_row(3);
        assert!(!layout.move_divider(divider_at(root(&elsewhere), 1), 0.25));
        assert_shares(root(&layout), &[0.5, 0.25, 0.25]);
    }

    /// Holding one squeezed side at the floor can squeeze another below it, and
    /// that one another, so the shares settle only once a pass squeezes none —
    /// and every side is left at the floor or above it, taking the whole extent
    /// between them.
    #[test]
    fn sides_squeezed_by_the_floor_in_turn_settle_at_the_floor_together() {
        let layout = row(&[1.0, 5.5, 100.0]);
        assert_shares(
            root(&layout),
            &[MIN_PANE_SHARE, MIN_PANE_SHARE, 1.0 - 2.0 * MIN_PANE_SHARE],
        );
    }

    /// Past the count of sides the least share a pane takes leaves room for, the
    /// floor is an even share of the split instead, which every side can be laid
    /// out at at once.
    #[test]
    fn a_split_of_more_sides_than_the_least_share_allows_divides_evenly() {
        let count = (1.0 / MIN_PANE_SHARE) as usize + 5;
        let layout = even_row(count);
        assert_shares(root(&layout), &vec![1.0 / count as f32; count]);
    }

    /// Asserts `split` lays its sides out at `expected` shares of its extent,
    /// which between them are the whole of it: shares that add up to less leave
    /// the window room no pane is laid out in.
    fn assert_shares(split: &Split<char>, expected: &[f32]) {
        let shares: Vec<f32> = split.sides().into_iter().map(|side| side.share).collect();
        let even = shares.len() == expected.len()
            && shares
                .iter()
                .zip(expected)
                .all(|(share, want)| (share - want).abs() < SHARE_ROUNDING);
        assert!(even, "laid out at {shares:?}, not at {expected:?}");
        let whole: f32 = shares.iter().sum();
        assert!(
            (whole - 1.0).abs() < SHARE_ROUNDING,
            "{shares:?} take {whole} of the extent between them, not all of it"
        );
    }

    /// How many dividers the outermost split of `layout` parts its sides by.
    fn dividers(layout: &Layout<char>) -> usize {
        root(layout).children.len() - 1
    }

    /// The outermost split of `layout`.
    fn root(layout: &Layout<char>) -> &Split<char> {
        match layout {
            Layout::Pane(_) => panic!("the arrangement is a split"),
            Layout::Split(split) => split,
        }
    }

    /// The split `split` lays out at side `at`.
    fn nested(split: &Split<char>, at: usize) -> &Split<char> {
        match split.sides()[at].layout {
            Layout::Pane(_) => panic!("the side at {at} is a split"),
            Layout::Split(inner) => inner,
        }
    }

    /// The divider `split` lays out at `boundary`, the first boundary being the
    /// one between the first two sides.
    fn divider_at(split: &Split<char>, boundary: usize) -> DividerId {
        split.sides()[boundary + 1]
            .divider
            .expect("a side laid out after another is laid out against a divider")
    }
}
