//! Non-UI terminal support: parsing Bazel Build Event Protocol output and
//! emulating a terminal to reproduce the final rendered state of a build.

pub use bazel_build_event_proto as bep;

use std::ops::RangeInclusive;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

mod columns;
mod display;
mod outcomes;
mod output;
mod replay;
mod screen;

/// History depth; large enough to retain every line of a build's output.
const SCROLLBACK: usize = 100_000;

/// Build an emulator of the given dimensions and feed it `bytes`, leaving the
/// grid holding the state those bytes produced.
fn build_term<D: Dimensions>(dimensions: &D, bytes: &[u8]) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: SCROLLBACK,
        ..Config::default()
    };
    let mut term = Term::new(config, dimensions, VoidListener);
    let mut processor: Processor = Processor::new();
    processor.advance(&mut term, bytes);
    term
}

/// Clone `lines` of the grid, each as its first `columns` cells, top to bottom.
fn snapshot_rows(grid: &Grid<Cell>, lines: RangeInclusive<i32>, columns: usize) -> Vec<Vec<Cell>> {
    lines
        .map(|line| {
            let row = &grid[Line(line)];
            (0..columns).map(|col| row[Column(col)].clone()).collect()
        })
        .collect()
}

pub use alacritty_terminal::grid::Scroll;
pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::selection::SelectionType;
pub use display::{CellPos, DisplaySource, SelectionSpan, Terminal, Url};
pub use output::{BuildOutput, ReadError, probe_invocation_id, read};
pub use replay::Replay;

/// The terminal cell and its styling vocabulary, re-exported from
/// `alacritty_terminal` so renderers can read cells while depending only on this
/// crate.
pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
