//! A sum of spawn time drawn as a bar of its phases.

use crate::format::{exact_duration, share};
use crate::palette::Palette;
use bviz_execlog::{Phase, PhaseTiming};
use bviz_ui_components::TextTooltip;
use bviz_ui_spawn_phase::phase_name;
use gpui::{Div, Pixels, div, prelude::*, px, relative};
use std::time::Duration;

const BAR_HEIGHT: Pixels = px(14.);
/// The canvas showing between two segments of a bar, so neighbouring phases
/// part by more than their colours.
const SEGMENT_GAP: Pixels = px(2.);
/// The narrowest a segment is drawn, so even a sliver of a phase can be
/// hovered for its time.
const MIN_SEGMENT_WIDTH: Pixels = px(4.);

/// The phases `timing` spent time in, in the order a bar draws them; a sum's
/// holds every phase any of its bars draws.
pub(crate) fn spent_in(timing: &PhaseTiming) -> impl Iterator<Item = Phase> + '_ {
    Phase::ALL
        .into_iter()
        .filter(|&phase| !timing.phase(phase).is_zero())
}

/// `timing` as a bar of its phases, as long against the bar for `longest` as
/// its total is, each phase hovered for its exact duration and its share of
/// the total.
///
/// Where the reported phases overlap to sum past the total, the segments
/// divide the bar by that sum, and the tooltip says so.
///
/// The phases of a runner that reports only execution are drawn as one
/// unbroken bar, apart from a breakdown whose other phases took no time.
pub(crate) fn phase_bar(timing: &PhaseTiming, longest: Duration, palette: &Palette) -> Div {
    let total = timing.total();
    let summed: Duration = Phase::ALL.into_iter().map(|p| timing.phase(p)).sum();
    let span = total.max(summed);
    let execution_only = timing.reports_execution_only();
    let segments = spent_in(timing).map(|phase| {
        let time = timing.phase(phase);
        let mut tip = format!(
            "{} · {} · {} of {}",
            phase_name(phase),
            exact_duration(time),
            share(time, total),
            exact_duration(total),
        );
        if summed > total {
            tip.push_str(&format!(
                "\nphases overlap · sum {}",
                exact_duration(summed)
            ));
        }
        let segment = div()
            .id(("phase", phase.index()))
            .h_full()
            .min_w(MIN_SEGMENT_WIDTH)
            .w(relative(fraction(time, span)));
        let segment = if execution_only && phase == Phase::Execute {
            tip.push_str("\nonly phase reported");
            segment
                .bg(palette.unbroken)
                .border_2()
                .border_dashed()
                .border_color(palette.phase(phase))
        } else {
            segment.bg(palette.phase(phase))
        };
        segment.tooltip(TextTooltip::builder(tip.into(), palette.colors.tooltip()))
    });
    div()
        .flex()
        .flex_row()
        .gap(SEGMENT_GAP)
        .h(BAR_HEIGHT)
        .w(relative(fraction(total, longest)))
        .children(segments)
}

/// `part` of `whole`, as a length relative to the length `whole` takes.
fn fraction(part: Duration, whole: Duration) -> f32 {
    if whole.is_zero() {
        0.0
    } else {
        (part.as_secs_f64() / whole.as_secs_f64()) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_phases_spent_in_are_listed_in_bar_order() {
        let mut reported = [Duration::ZERO; bviz_invocation::spawn::REPORTED_PHASES];
        reported[Phase::Fetch.index()] = Duration::from_millis(3);
        reported[Phase::Queue.index()] = Duration::from_millis(2);
        let timing = PhaseTiming::new(Duration::from_millis(5), reported);
        let phases: Vec<Phase> = spent_in(&timing).collect();
        assert_eq!(phases, [Phase::Queue, Phase::Fetch]);
    }
}
