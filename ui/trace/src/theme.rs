//! Trace-specific color mapping: the roles the trace views paint with, and span
//! roles and counter series to fills, resolved from the shared palette.

use bviz_trace_layout::{SpanRole, Workload};
use bviz_ui_components::{LabelBoxStyle, LegendEntry, ScrollbarColors, Swatch};
use bviz_ui_theme::{DataColors, Theme, ThemeColors, hue_of, label_on, mix, over};
use gpui::{BorderStyle, Hsla, Rgba, hsla};

/// Saturation of a counter series' translucent fill.
const CATEGORY_SATURATION: f32 = 0.5;

/// Lightness of a counter series' translucent fill.
const CATEGORY_LIGHTNESS: f32 = 0.55;

/// Alpha of a counter series' filled area.
const COUNTER_AREA_ALPHA: f32 = 0.5;

/// A translucent fill for a counter series, keyed by its name like a category.
/// Counter series are unbounded, so they are spread over the whole wheel rather
/// than assigned one of the palette's slots.
pub(crate) fn counter_color(series: &str) -> Hsla {
    hsla(
        hue_of(series),
        CATEGORY_SATURATION,
        CATEGORY_LIGHTNESS,
        COUNTER_AREA_ALPHA,
    )
}

/// What a counter series' value under the crosshair is written on: a tag of
/// the series' own fill, so the label reads the same over the filled area and
/// over the bare chart beside it.
pub(crate) struct ReadoutTag {
    /// The series' fill as it stands on the canvas.
    pub(crate) fill: Hsla,
    pub(crate) label: Hsla,
}

/// How far an overhead fill is pulled toward the canvas, so Bazel's own machinery
/// sits behind the payload it serves. Bounded on both sides: pulled too little,
/// an overhead fill is still taken for the vivid fill of the next slot round the
/// wheel; pulled to halfway or past it, the overhead fills have converged far
/// enough on the canvas for the closest pair of them to stop reading as two
/// colors.
const OVERHEAD_FADE: f32 = 0.4;

/// How far the wash filling a remainder recedes toward the canvas from the
/// colour of its outline.
const REMAINDER_WASH_FADE: f32 = 0.6;

/// How far the fill of the rows a remainder reaches recedes toward the canvas
/// from the hueless data colour.
const REMAINDER_DEPTH_FADE: f32 = 0.7;

/// A remainder's paint: an outline around a wash of its own colour, where a
/// frame is one solid fill.
pub(crate) struct RemainderPaint {
    pub(crate) outline: Hsla,
    pub(crate) wash: Hsla,
    pub(crate) border: BorderStyle,
}

/// Every workload the reader is shown a name for, paired with the weight its
/// swatch is drawn in, payload first.
const LEGEND_ROLES: [(SpanRole, &str); 8] = [
    (
        SpanRole {
            workload: Workload::Subprocess,
            payload: true,
        },
        "action",
    ),
    (
        SpanRole {
            workload: Workload::Starlark,
            payload: true,
        },
        "starlark",
    ),
    (
        SpanRole {
            workload: Workload::Filesystem,
            payload: false,
        },
        "filesystem",
    ),
    (
        SpanRole {
            workload: Workload::RemoteCache,
            payload: false,
        },
        "remote/cache",
    ),
    (
        SpanRole {
            workload: Workload::Scheduling,
            payload: false,
        },
        "scheduling",
    ),
    (
        SpanRole {
            workload: Workload::Skyframe,
            payload: false,
        },
        "skyframe",
    ),
    (
        SpanRole {
            workload: Workload::Gc,
            payload: false,
        },
        "gc",
    ),
    (
        SpanRole {
            workload: Workload::Other,
            payload: false,
        },
        "other",
    ),
];

/// The legend's entries for the span-role colours, each swatch filled as the
/// flame boxes of its role are.
pub(crate) fn role_legend(palette: &Palette) -> impl Iterator<Item = LegendEntry> + use<> {
    let palette = *palette;
    LEGEND_ROLES
        .into_iter()
        .map(move |(role, name)| LegendEntry::new(Swatch::Fill(palette.role_fill(role)), name))
}

/// The colors the trace canvas paints with, owned so the `'static` paint closures
/// can capture them.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) border: Rgba,
    pub(crate) label: Rgba,
    /// Text on a control that is not the one in force, such as an unselected
    /// tab's label.
    pub(crate) muted_label: Rgba,
    pub(crate) background: Rgba,
    /// The sticky lane-header band fill, shared by the ruler band.
    pub(crate) header_band: Rgba,
    /// Faint vertical line dropped from each ruler tick through the lanes.
    pub(crate) gridline: Rgba,
    /// Accent for the build-phase boundary lines and their labels.
    pub(crate) marker: Rgba,
    pub(crate) scroll: ScrollbarColors,
    /// Fill for a row or control that marks the cursor by lifting off its surface.
    pub(crate) hover: Rgba,
    /// The line at a seam between two of the view's surfaces: the gutter's right
    /// edge and the counters divider.
    pub(crate) divider: Rgba,
    /// Opaque fill of a panel drawn over the chart.
    pub(crate) panel: Rgba,
    /// The hover crosshair line: a readout the cursor drags rather than a rule
    /// laid on the chart, so it ranks with secondary text and stands over the
    /// scenery it crosses.
    pub(crate) crosshair: Rgba,
    /// The rule at the document's playback time: where the whole document
    /// stands rather than where the cursor points, so it ranks with primary
    /// text where the crosshair ranks with secondary.
    pub(crate) playhead: Rgba,
    /// The edges and bars bounding what the reader has picked out.
    pub(crate) accent: Rgba,
    /// The wash over what the reader has picked out — a selected lane's gutter
    /// row, the ruler over a chosen time range.
    pub(crate) selection: Rgba,
    /// The slots the span workloads are colored from.
    data: DataColors,
    /// The theme's own roles, for handing to a shared component that resolves
    /// its colors from them; a trace view paints from this palette's roles
    /// rather than from these.
    pub(crate) colors: ThemeColors,
}

impl Palette {
    pub(crate) fn from_theme(theme: &Theme) -> Palette {
        let colors = &theme.colors;
        Palette {
            border: colors.border.into(),
            label: colors.text.into(),
            muted_label: colors.text_muted.into(),
            background: colors.background.into(),
            header_band: colors.surface.into(),
            gridline: colors.gridline.into(),
            marker: colors.highlight.into(),
            scroll: ScrollbarColors::from_colors(colors),
            hover: colors.element_hover.into(),
            divider: colors.border.into(),
            panel: colors.panel.into(),
            crosshair: colors.text_muted.into(),
            playhead: colors.text.into(),
            accent: colors.accent.into(),
            selection: colors.selection.into(),
            data: theme.data,
            colors: *colors,
        }
    }

    /// The tag of a counter series whose fill is `series`.
    pub(crate) fn readout_tag(&self, series: Hsla) -> ReadoutTag {
        let fill = over(series, self.background);
        ReadoutTag {
            fill: fill.into(),
            label: label_on(fill),
        }
    }

    /// Fill for a span, keyed by its [`SpanRole`]: the workload picks the data
    /// slot and the payload flag its weight. Both the flame chart and the flame
    /// graph paint with it, so a role is one color in both. The Other workload —
    /// including the flame graph's synthetic root — carries no hue.
    pub(crate) fn role_fill(&self, role: SpanRole) -> Hsla {
        let slot = match role.workload {
            Workload::Subprocess => self.data.orange,
            Workload::Starlark => self.data.green,
            Workload::Filesystem => self.data.cyan,
            Workload::RemoteCache => self.data.blue,
            Workload::Skyframe => self.data.purple,
            Workload::Scheduling => self.data.yellow,
            Workload::Gc => self.data.red,
            Workload::Other => return self.data.neutral,
        };
        if role.payload {
            slot
        } else {
            mix(slot, self.background, OVERHEAD_FADE).into()
        }
    }

    /// What a remainder whose members share `role`, or share none, is drawn in.
    /// The hueless colour outlines members of several roles, which the hue of
    /// no one of them describes, in dashes that tell it from the outline of
    /// members that are all of the Other workload.
    pub(crate) fn remainder(&self, role: Option<SpanRole>) -> RemainderPaint {
        let (outline, border) = match role {
            Some(role) => (self.role_fill(role), BorderStyle::Solid),
            None => (self.data.neutral, BorderStyle::Dashed),
        };
        RemainderPaint {
            outline,
            wash: mix(outline, self.background, REMAINDER_WASH_FADE).into(),
            border,
        }
    }

    /// Fill for the rows beneath a remainder that its members reach, which
    /// stand for no frame and so carry no role.
    pub(crate) fn remainder_depth_fill(&self) -> Hsla {
        mix(self.data.neutral, self.background, REMAINDER_DEPTH_FADE).into()
    }

    /// The style a labeled box paints with.
    pub(crate) fn label_box(&self) -> LabelBoxStyle {
        LabelBoxStyle {
            background: self.background,
            seam_mix: crate::SLICE_SEAM_MIX,
            label_min_px: crate::LABEL_MIN_PX,
            text_pad: crate::SLICE_TEXT_PAD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_theme::{
        MIN_DISTINCT_COLOR, MIN_EDGED_STEP, MIN_TEXT_CONTRAST, MIN_VISIBLE_STEP, built_in_themes,
        contrast_ratio, distance, level_step,
    };

    /// The most a hover fill may clear the surface under it, as a multiple of
    /// [`MIN_VISIBLE_STEP`]: hover is momentary feedback, so lifted further it
    /// reads as a surface in its own right and starts competing with the text on
    /// the row.
    const HOVER_HEADROOM: f32 = 3.0;

    /// The workloads that carry a hue, each of which [`Palette::role_fill`] paints
    /// in a vivid and a faded weight. Other is left out: it takes the hueless slot
    /// whichever weight it is asked for.
    const HUED_WORKLOADS: [Workload; 7] = [
        Workload::Subprocess,
        Workload::Starlark,
        Workload::Filesystem,
        Workload::RemoteCache,
        Workload::Skyframe,
        Workload::Scheduling,
        Workload::Gc,
    ];

    /// Every theme's palette, for a rule that has to hold whichever one a window
    /// paints with.
    fn palettes() -> Vec<Palette> {
        built_in_themes()
            .iter()
            .map(|theme| Palette::from_theme(theme))
            .collect()
    }

    /// The surfaces the trace stacks its rows and controls on.
    fn surfaces(palette: &Palette) -> [Rgba; 3] {
        [palette.background, palette.header_band, palette.panel]
    }

    /// Every fill [`Palette::role_fill`] can return, each once.
    fn role_fills(palette: &Palette) -> Vec<Hsla> {
        let mut fills = Vec::with_capacity(2 * HUED_WORKLOADS.len() + 1);
        for workload in HUED_WORKLOADS {
            for payload in [true, false] {
                fills.push(palette.role_fill(SpanRole { workload, payload }));
            }
        }
        fills.push(palette.role_fill(SpanRole {
            workload: Workload::Other,
            payload: false,
        }));
        fills
    }

    /// How far a fill stands off the canvas it is painted on.
    fn stand_off(palette: &Palette, role: SpanRole) -> f32 {
        distance(palette.role_fill(role), palette.background)
    }

    /// How many hues, evenly spaced round the wheel, stand for every counter
    /// series.
    const SERIES_HUES: u16 = 360;

    #[test]
    fn a_counter_readout_holds_text_contrast_on_its_tag_whatever_the_series() {
        for palette in palettes() {
            for hue in 0..SERIES_HUES {
                let series = hsla(
                    f32::from(hue) / f32::from(SERIES_HUES),
                    CATEGORY_SATURATION,
                    CATEGORY_LIGHTNESS,
                    COUNTER_AREA_ALPHA,
                );
                let tag = palette.readout_tag(series);
                let contrast = contrast_ratio(tag.label, tag.fill);
                assert!(contrast >= MIN_TEXT_CONTRAST, "hue {hue}: {contrast}");
            }
        }
    }

    /// A hover marks the row the cursor is on and hands it back on the way out.
    #[test]
    fn the_hover_lifts_off_a_surface_without_becoming_one() {
        for palette in palettes() {
            for surface in surfaces(&palette) {
                let lift = level_step(over(palette.hover, surface), surface);
                assert!(lift <= HOVER_HEADROOM * MIN_VISIBLE_STEP, "{lift} levels");
            }
        }
    }

    /// A row under the cursor and a row the reader has picked out say two
    /// different things about the same strip of screen, so the hover has to be a
    /// different color from the selection rather than a weaker version of it.
    #[test]
    fn the_hover_is_told_from_the_selection() {
        for palette in palettes() {
            for surface in surfaces(&palette) {
                let hovered = over(palette.hover, surface);
                let apart = distance(hovered, over(palette.selection, surface));
                assert!(apart >= MIN_DISTINCT_COLOR, "{apart}");
            }
        }
    }

    /// The gutter's right edge and the counters divider are drawn where the header
    /// band meets the chart canvas, so the line has to be seen against both of the
    /// surfaces it parts and not only against the one it is nearer in tone.
    #[test]
    fn the_divider_parts_the_two_surfaces_it_is_drawn_between() {
        for palette in palettes() {
            for surface in [palette.background, palette.header_band] {
                let step = level_step(over(palette.divider, surface), surface);
                assert!(step >= MIN_EDGED_STEP, "{step} levels");
            }
        }
    }

    /// A workload is read off the color of the box, so no two fills may be taken
    /// for each other and none may sink into the canvas the boxes are drawn on.
    #[test]
    fn every_span_role_is_told_from_the_others_and_from_the_canvas() {
        for palette in palettes() {
            let fills = role_fills(&palette);
            for (i, fill) in fills.iter().enumerate() {
                let over_canvas = distance(*fill, palette.background);
                assert!(over_canvas >= MIN_DISTINCT_COLOR, "{over_canvas}");
                for other in &fills[i + 1..] {
                    let apart = distance(*fill, *other);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{apart}");
                }
            }
        }
    }

    /// A remainder is told from a frame by being an outline around a wash, so
    /// the wash has to part from the outline, and from the canvas the outline
    /// stands on.
    #[test]
    fn a_remainders_wash_is_told_from_its_outline_and_from_the_canvas() {
        for palette in palettes() {
            let roles = HUED_WORKLOADS
                .into_iter()
                .chain([Workload::Other])
                .flat_map(|workload| [true, false].map(|payload| SpanRole { workload, payload }))
                .map(Some)
                .chain([None]);
            for role in roles {
                let RemainderPaint { outline, wash, .. } = palette.remainder(role);
                let from_outline = distance(wash, outline);
                assert!(
                    from_outline >= MIN_DISTINCT_COLOR,
                    "{role:?}: {from_outline}"
                );
                let from_canvas = level_step(wash, palette.background);
                assert!(
                    from_canvas >= MIN_EDGED_STEP,
                    "{role:?}: {from_canvas} levels"
                );
            }
        }
    }

    /// The rows a remainder reaches are drawn with no edge, and stand for no
    /// frame: the fill has to show on the canvas without being taken for the
    /// hueless fill of a frame of the Other workload.
    #[test]
    fn the_depth_beneath_a_remainder_is_told_from_the_canvas_and_from_a_frame() {
        for palette in palettes() {
            let depth = palette.remainder_depth_fill();
            let from_canvas = level_step(depth, palette.background);
            assert!(from_canvas >= MIN_VISIBLE_STEP, "{from_canvas} levels");
            let hueless = palette.role_fill(SpanRole {
                workload: Workload::Other,
                payload: false,
            });
            let from_frame = distance(depth, hueless);
            assert!(from_frame >= MIN_DISTINCT_COLOR, "{from_frame}");
        }
    }

    /// The flame views rank the build's own work over Bazel's machinery, which
    /// they can only say by every payload fill standing further off the canvas
    /// than every overhead one, taking each hued workload in both its weights.
    /// The hueless Other fill ranks with neither: it marks the spans that carry
    /// no workload at all.
    #[test]
    fn a_payload_fill_stands_further_off_the_canvas_than_any_overhead_fill() {
        for palette in palettes() {
            let (mut payload, mut overhead) = (f32::INFINITY, 0.0f32);
            for workload in HUED_WORKLOADS {
                payload = payload.min(stand_off(
                    &palette,
                    SpanRole {
                        workload,
                        payload: true,
                    },
                ));
                overhead = overhead.max(stand_off(
                    &palette,
                    SpanRole {
                        workload,
                        payload: false,
                    },
                ));
            }
            assert!(payload > overhead, "{payload} against {overhead}");
        }
    }
}
