//! Application assets — the Inter and JetBrains Mono fonts and the Phosphor icon
//! set — embedded into the binary at build time. The files are downloaded by
//! Bazel `http_archive`s and copied into `OUT_DIR` by `build.rs`, then baked in
//! with `include_bytes!`, so there is nothing to load from disk at runtime. It
//! also resolves text metrics and shapes single-run lines from those fonts.

use gpui::{
    App, AssetSource, Font, Hsla, Pixels, Result, ShapedLine, SharedString, Svg, TextRun, Window,
    svg,
};
use std::borrow::Cow;

/// UI font family.
pub const UI: &str = "Inter";
/// Monospace font family.
pub const MONO: &str = "JetBrains Mono";

/// The font, size, and line height of the window's current text style.
pub struct TextMetrics {
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: Pixels,
}

/// Resolve [`TextMetrics`] from the window's inherited text style, so a custom
/// element renders in the family/size cascaded to it.
pub fn text_metrics(window: &Window) -> TextMetrics {
    let style = window.text_style();
    TextMetrics {
        font: style.font(),
        font_size: style.font_size.to_pixels(window.rem_size()),
        line_height: window.line_height(),
    }
}

/// Shape one run of `text` in a single font, size, and color, with no
/// decorations, for measuring (e.g. a monospace cell width) or painting (e.g. a
/// box label).
pub fn shape_line(
    window: &mut Window,
    text: impl Into<SharedString>,
    font: Font,
    font_size: Pixels,
    color: Hsla,
) -> ShapedLine {
    let text = text.into();
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text, font_size, &[run], None)
}

macro_rules! embedded {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name))
    };
}

const FONTS: &[&[u8]] = &[
    embedded!("Inter-Regular.ttf"),
    embedded!("Inter-Bold.ttf"),
    embedded!("Inter-Italic.ttf"),
    embedded!("Inter-BoldItalic.ttf"),
    embedded!("JetBrainsMono-Regular.ttf"),
    embedded!("JetBrainsMono-Bold.ttf"),
    embedded!("JetBrainsMono-Italic.ttf"),
    embedded!("JetBrainsMono-BoldItalic.ttf"),
];

/// Register the embedded fonts with gpui's text system.
pub fn load_fonts(cx: &App) {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    cx.text_system().add_fonts(fonts).expect("register fonts");
}

fn icon_svg(file: &str) -> Option<&'static [u8]> {
    Some(match file {
        "caret-double-down.svg" => embedded!("caret-double-down.svg"),
        "caret-double-up.svg" => embedded!("caret-double-up.svg"),
        "caret-down.svg" => embedded!("caret-down.svg"),
        "caret-up.svg" => embedded!("caret-up.svg"),
        "check.svg" => embedded!("check.svg"),
        "circle-notch.svg" => embedded!("circle-notch.svg"),
        "minus.svg" => embedded!("minus.svg"),
        "pause.svg" => embedded!("pause.svg"),
        "play.svg" => embedded!("play.svg"),
        "skip-back.svg" => embedded!("skip-back.svg"),
        "skip-forward.svg" => embedded!("skip-forward.svg"),
        "square.svg" => embedded!("square.svg"),
        "x.svg" => embedded!("x.svg"),
        _ => return None,
    })
}

/// Serves the embedded Phosphor icons to gpui's `svg()` element.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let file = path.rsplit('/').next().unwrap_or(path);
        Ok(icon_svg(file).map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// A regular-weight Phosphor icon, tinted by the element's text color.
pub fn icon(name: &str) -> Svg {
    svg().path(format!("assets/regular/{name}.svg"))
}
