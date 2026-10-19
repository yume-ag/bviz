//! Emulating a terminal to recover the final on-screen state of a build.
//!
//! With `--curses`, Bazel's console output is a stream of ANSI control sequences
//! that repaint a progress display in place. Feeding it through a terminal
//! emulator at the width Bazel used resolves those sequences into a flat
//! character grid: the build output as it finally appeared.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::columns::Columns;
use crate::output::BuildOutput;
use crate::{build_term, snapshot_rows};

/// Rows in the internal emulator. The build event stream carries the terminal
/// width but not its height, and Bazel's progress repaints move the cursor only
/// within the block they just wrote, so any height exceeding that block
/// reproduces the output; this is generously larger. Content above the screen
/// scrolls into history and is preserved by `scrolling_history`.
const INTERNAL_ROWS: usize = 100;

/// The final terminal state: rows from the top of scrollback history down to the
/// last non-blank screen line. Cells are `alacritty_terminal`'s, carrying their
/// character, colors, and style flags.
pub struct Screen {
    pub rows: Vec<Vec<Cell>>,
}

/// Terminal dimensions for the emulator. Height is fixed; see [`INTERNAL_ROWS`].
struct Size {
    columns: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        INTERNAL_ROWS
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

impl Screen {
    /// Replay a build's captured output through a terminal emulator and snapshot
    /// the resulting grid.
    pub fn from_output(output: &BuildOutput) -> Screen {
        Screen::from_bytes(&output.bytes, output.columns)
    }

    /// Replay a prefix of a build's captured output through a terminal emulator and
    /// snapshot the resulting grid: the terminal as it stood after those bytes. A
    /// playback frame is such a prefix.
    pub(crate) fn from_bytes(bytes: &[u8], columns: Columns) -> Screen {
        let columns = columns.get();
        let term = build_term(&Size { columns }, &lf_to_crlf(bytes));

        let grid = term.grid();
        let mut rows = snapshot_rows(
            grid,
            grid.topmost_line().0..=grid.bottommost_line().0,
            columns,
        );

        while rows.last().is_some_and(|row| is_blank(row)) {
            rows.pop();
        }
        Screen { rows }
    }

    /// The plain text of the screen, one line per row with trailing blanks trimmed.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        rows_to_text(&self.rows)
    }

    /// Serialize the screen to a clean ANSI byte stream: one logical line per row
    /// (rows alacritty wrapped are rejoined), trailing blanks trimmed, with SGR
    /// codes for each cell's color and style. Replaying this into a terminal of
    /// any width re-wraps it to that width, free of the original curses control.
    pub(crate) fn to_ansi(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let wrapped = emit_row(&mut out, row);
            // A non-wrapped row ends its logical line with a reset and CRLF (the
            // carriage return returns the cursor to column zero); a wrapped row
            // flows into the next without a break. The final line gets no trailing
            // newline, so the cursor stays on the last row of content rather than
            // scrolling it into history.
            if !wrapped && i + 1 < self.rows.len() {
                out.extend_from_slice(b"\x1b[0m\r\n");
            }
        }
        out
    }
}

/// Emit one row as SGR-styled cells, returning whether it wraps into the next.
/// A wrapped row emits every cell; a trimmed row stops after its last non-blank
/// cell. Cells sharing fg/bg/flags form a run preceded by one SGR escape. A wide
/// character's trailing spacer carries no character of its own and is skipped;
/// replaying the wide character recreates it.
fn emit_row(out: &mut Vec<u8>, row: &[Cell]) -> bool {
    let wrapped = row
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
    let end = if wrapped {
        row.len()
    } else {
        row.iter()
            .rposition(|cell| !is_blank_cell(cell))
            .map_or(0, |i| i + 1)
    };
    let mut col = 0;
    while col < end {
        let start = col;
        let key = (row[col].fg, row[col].bg, row[col].flags);
        while col < end && (row[col].fg, row[col].bg, row[col].flags) == key {
            col += 1;
        }
        emit_sgr(out, key.0, key.1, key.2);
        for cell in &row[start..col] {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        }
    }
    wrapped
}

/// Append the SGR escape that reproduces a cell's color and style, resetting
/// first so runs do not bleed.
fn emit_sgr(out: &mut Vec<u8>, fg: Color, bg: Color, flags: Flags) {
    let mut codes: Vec<String> = vec!["0".into()];
    color_codes(&mut codes, fg, 3);
    color_codes(&mut codes, bg, 4);
    if flags.contains(Flags::BOLD) {
        codes.push("1".into());
    }
    if flags.contains(Flags::DIM) {
        codes.push("2".into());
    }
    if flags.contains(Flags::ITALIC) {
        codes.push("3".into());
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        codes.push("4".into());
    }
    if flags.contains(Flags::INVERSE) {
        codes.push("7".into());
    }
    if flags.contains(Flags::HIDDEN) {
        codes.push("8".into());
    }
    if flags.contains(Flags::STRIKEOUT) {
        codes.push("9".into());
    }
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(codes.join(";").as_bytes());
    out.push(b'm');
}

/// Append the SGR parameter for a color on the given layer (3 = foreground,
/// 4 = background). Default colors emit nothing, relying on the leading reset.
fn color_codes(codes: &mut Vec<String>, color: Color, layer: u8) {
    match color {
        Color::Named(named) => {
            let idx = named as usize;
            if idx < 8 {
                codes.push(format!("{}{}", layer, idx));
            } else if idx < 16 {
                let base = if layer == 3 { 90 } else { 100 };
                codes.push((base + idx - 8).to_string());
            }
        }
        Color::Indexed(i) => codes.push(format!("{}8;5;{}", layer, i)),
        Color::Spec(Rgb { r, g, b }) => codes.push(format!("{}8;2;{};{};{}", layer, r, g, b)),
    }
}

/// Translate bare line feeds to CRLF.
///
/// Bazel writes progress repaints assuming a `\n` returns the cursor to column
/// zero, which on a real terminal is the tty's `ONLCR` translation, applied
/// after the bytes the build event stream captures. Without it, alacritty keeps
/// the column on a line feed, so each progress line starts mid-row and the
/// repaints drift down the screen instead of overwriting in place.
fn lf_to_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// Rows rendered as text: each row's characters joined and its trailing blanks
/// trimmed, rows joined by newline. Rows that are entirely blank become empty
/// lines; trimming trailing empty lines is left to the caller.
#[cfg(test)]
pub(crate) fn rows_to_text(rows: &[Vec<Cell>]) -> String {
    rows.iter()
        .map(|row| row.iter().map(|cell| cell.c).collect::<String>())
        .map(|line| line.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A cell is blank if it is a space on the default background.
fn is_blank_cell(cell: &Cell) -> bool {
    cell.c == ' ' && cell.bg == Color::Named(NamedColor::Background)
}

/// A row is blank if every cell is blank.
fn is_blank(row: &[Cell]) -> bool {
    row.iter().all(is_blank_cell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::read_slice;

    const SAMPLE: &[u8] = include_bytes!("../testdata/sample.bep");

    #[test]
    fn renders_final_build_output() {
        let output = read_slice(SAMPLE)
            .expect("parse sample stream")
            .into_inner();
        let screen = Screen::from_output(&output);
        // The emulator was sized to the width the stream reported, so every
        // retained row spans it.
        assert!(screen.rows.iter().all(|row| row.len() == 100));

        // The build finished, so its concluding line survives into the grid.
        let text = screen.text();
        assert!(
            text.contains("Build completed successfully"),
            "final screen:\n{text}"
        );

        // The progress display is repainted in place, not appended: the dozens
        // of intermediate "Analyzing:" updates collapse to at most the final one.
        let analyzing = text
            .lines()
            .filter(|line| line.contains("Analyzing:"))
            .count();
        assert!(analyzing <= 1, "progress did not collapse:\n{text}");
    }

    #[test]
    fn collapses_in_place_repaints() {
        // Bazel's repaint idiom: a line, a line feed, then CR + cursor-up +
        // clear-line before the next. These must overwrite onto one row.
        let output = BuildOutput::from_bytes(
            20,
            b"first\n\r\x1b[1A\x1b[Ksecond\n\r\x1b[1A\x1b[Kthird\n".to_vec(),
        );
        let screen = Screen::from_output(&output);
        assert_eq!(screen.text(), "third");
    }

    #[test]
    fn golden_window_shape() {
        // A miniature build window exercising the layout that matters: literal
        // indentation that must be preserved, and a two-line progress block that
        // is repainted twice in place and finally replaced by a colored result.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"Build:\n  //app:bin\n  //app:test\n");
        bytes.extend_from_slice(b"Running 0/2\n   local\n");
        bytes.extend_from_slice(b"\r\x1b[1A\x1b[K\r\x1b[1A\x1b[K");
        bytes.extend_from_slice(b"Running 2/2\n   done\n");
        bytes.extend_from_slice(b"\r\x1b[1A\x1b[K\r\x1b[1A\x1b[K");
        bytes.extend_from_slice(b"\x1b[32mPASS\x1b[0m all\n");
        let screen = Screen::from_output(&BuildOutput::from_bytes(40, bytes));

        // Shape: the indents survive and the progress block collapses to PASS.
        assert_eq!(screen.text(), "Build:\n  //app:bin\n  //app:test\nPASS all");

        // Color: the result line carries the green set by the SGR sequence.
        let pass = &screen.rows[3];
        assert_eq!(pass[0].c, 'P');
        assert_eq!(pass[0].fg, Color::Named(NamedColor::Green));
    }
}
