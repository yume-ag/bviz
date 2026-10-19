use crate::{
    Clock, PlayPause, Speed, SpeedDown, SpeedUp, StepBack, StepForward, TEXT_SIZE, Timeline,
};
use bviz_invocation::EpochMicros;
use bviz_ui_components::{
    Button, Dropdown, DropdownEvent, ScrubberColors, ScrubberState, TextTooltip, TruncatingLabel,
    scrubber,
};
use bviz_ui_theme::ThemeColors;
use gpui::{
    Action, AnyElement, App, Context, Entity, IntoElement, Pixels, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use std::time::Duration;

/// Height of the band the bar draws.
const BAR_HEIGHT: Pixels = px(40.);

/// How wide the document's name may grow before it is cut, so a long one
/// leaves the scrubber room.
const NAME_MAX_WIDTH: Pixels = px(240.);

/// Width held for the time readout, wide enough for the longest it reads, so
/// the scrubber does not shift as the time changes.
const READOUT_WIDTH: Pixels = px(128.);

const SECONDS_PER_MINUTE: u64 = 60;

/// What the readout leads an approximate time with.
const APPROXIMATE_MARK: &str = "~";

/// What the readout's hint says of an approximate time.
const APPROXIMATE_HINT: &str = "Approximate time";

/// The transport driving a document's [`Clock`], labelled with the document's
/// name. Draws nothing while the clock has no time.
pub struct PlaybackBar {
    name: SharedString,
    clock: Entity<Clock>,
    /// Shows the clock's speed, and sets it when the user picks another.
    speed: Entity<Dropdown>,
    scrubber: ScrubberState,
    _clock_changed: Subscription,
    _speed_chosen: Subscription,
}

impl PlaybackBar {
    pub fn new(name: SharedString, clock: Entity<Clock>, cx: &mut Context<Self>) -> Self {
        let current = speed_of(&clock, cx);
        let labels = Speed::all().map(|speed| format!("{}\u{d7}", speed.factor()));
        let speed = cx.new(|cx| {
            Dropdown::new("playback-speed", labels, current.index(), cx)
                .tooltip("Playback speed")
                .keys_for_focused(&SpeedDown)
                .keys_for_focused(&SpeedUp)
        });
        let clock_changed = cx.observe(&clock, |bar, clock, cx| {
            let index = speed_of(&clock, cx).index();
            bar.speed
                .update(cx, |dropdown, cx| dropdown.set_selected(index, cx));
            cx.notify();
        });
        let speed_chosen = cx.subscribe(&speed, |bar, _, event: &DropdownEvent, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(speed) = Speed::from_index(*index) {
                bar.clock.update(cx, |clock, cx| clock.set_speed(speed, cx));
            }
        });
        Self {
            name,
            clock,
            speed,
            scrubber: ScrubberState::default(),
            _clock_changed: clock_changed,
            _speed_chosen: speed_chosen,
        }
    }

    pub fn clock(&self) -> &Entity<Clock> {
        &self.clock
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// Labels the bar with `name`, for a document that goes by another.
    pub fn rename(&mut self, name: SharedString, cx: &mut Context<Self>) {
        self.name = name;
        cx.notify();
    }

    /// Seeks to `fraction` of the way through the range. Grabbing the scrubber
    /// pauses; the drag that follows only seeks, so a play toggled from the
    /// keyboard mid-drag stands.
    fn scrub_to(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let held = self.scrubber.held();
        self.clock.update(cx, |clock, cx| {
            let Some(range) = clock.timeline().map(Timeline::range) else {
                return;
            };
            if !held {
                clock.pause(cx);
            }
            clock.seek(range.at_fraction(fraction), cx);
        });
    }

    fn transport(&self, playing: bool) -> [Button; 3] {
        let drive = |id: &'static str,
                     icon: &'static str,
                     action: &dyn Action,
                     gesture: fn(&mut Clock, &mut Context<Clock>)| {
            let clock = self.clock.clone();
            Button::new(id, move |_, _, cx| clock.update(cx, gesture))
                .ghost()
                .icon(icon)
                .keys_for_focused(action)
        };
        let icon = if playing { "pause" } else { "play" };
        [
            drive("playback-step-back", "skip-back", &StepBack, |clock, cx| {
                clock.step(false, cx);
            }),
            drive("playback-play-pause", icon, &PlayPause, Clock::play_pause),
            drive(
                "playback-step-forward",
                "skip-forward",
                &StepForward,
                |clock, cx| {
                    clock.step(true, cx);
                },
            ),
        ]
    }

    fn scrubber(&self, fraction: f32, colors: &ThemeColors, cx: &mut Context<Self>) -> AnyElement {
        // The played portion and the knob mark a position within the span the
        // track carries, so they take the text they are read among.
        let fills = ScrubberColors {
            track: colors.element_hover.into(),
            played: colors.text.into(),
            knob: colors.text.into(),
        };
        scrubber(
            "playback-scrubber",
            fraction,
            fills,
            |bar: &mut Self| &mut bar.scrubber,
            cx.entity(),
            |bar, fraction, cx| bar.scrub_to(fraction, cx),
        )
        .into_any_element()
    }
}

impl Render for PlaybackBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let clock = self.clock.read(cx);
        let Some(timeline) = clock.timeline() else {
            return div().into_any_element();
        };
        Clock::keep_pace(Some(&self.clock), window, cx);
        let now = timeline.now();
        let fraction = timeline.range().fraction_at(now);
        let readout = readout(timeline, now);
        let approximate = timeline.approximate();
        let playing = timeline.is_playing();
        let colors = bviz_ui_theme::theme(window, cx).colors;
        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .h(BAR_HEIGHT)
            .w_full()
            .bg(colors.surface)
            .border_t_1()
            .border_color(colors.border)
            .font_family(bviz_ui_assets::UI)
            .text_color(colors.text)
            .child(
                TruncatingLabel::new("playback-name", self.name.clone())
                    .max_w(NAME_MAX_WIDTH)
                    .flex_none()
                    .text_size(TEXT_SIZE)
                    .text_color(colors.text_muted),
            )
            .children(self.transport(playing))
            .child(self.scrubber(fraction, &colors, cx))
            .child(
                div()
                    .id("playback-readout")
                    .w(READOUT_WIDTH)
                    .flex_none()
                    .text_size(TEXT_SIZE)
                    .when(approximate, |readout| {
                        let hint = TextTooltip::builder(APPROXIMATE_HINT.into(), colors.tooltip());
                        readout.tooltip(hint)
                    })
                    .child(readout),
            )
            .child(self.speed.clone())
            .into_any_element()
    }
}

fn speed_of(clock: &Entity<Clock>, cx: &App) -> Speed {
    clock
        .read(cx)
        .timeline()
        .map_or(Speed::default(), Timeline::speed)
}

/// `now` and the range's length, each as time since the range's start; the
/// first marked approximate where the timeline's times are.
fn readout(timeline: &Timeline, now: EpochMicros) -> SharedString {
    let range = timeline.range();
    let elapsed = minutes_seconds(range.elapsed_micros(now));
    let total = minutes_seconds(range.length_micros());
    let approximate = if timeline.approximate() {
        APPROXIMATE_MARK
    } else {
        ""
    };
    format!("{approximate}{elapsed} / {total}").into()
}

/// `micros` as `M:SS`, to the nearest second.
fn minutes_seconds(micros: i64) -> String {
    let seconds = Duration::from_micros(micros.max(0).unsigned_abs())
        .as_secs_f64()
        .round() as u64;
    format!(
        "{}:{:02}",
        seconds / SECONDS_PER_MINUTE,
        seconds % SECONDS_PER_MINUTE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_reads_as_minutes_and_seconds() {
        assert_eq!(minutes_seconds(0), "0:00");
        assert_eq!(minutes_seconds(59_600_000), "1:00");
        assert_eq!(minutes_seconds(754_000_000), "12:34");
        assert_eq!(minutes_seconds(-5), "0:00");
    }
}
