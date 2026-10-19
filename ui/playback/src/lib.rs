//! A document's playback clock, which every view of the document follows, and
//! the bar that drives it.

mod bar;
mod clock;
mod note;

pub use bar::PlaybackBar;
pub use clock::{Alignment, BecameJoined, Clock, OnClock, Source, SourceId, Span, Speed, Timeline};
pub use note::unaligned_note;

use bviz_ui_components::KeyScope;
use gpui::{Pixels, actions, px};

actions!(
    bviz_playback,
    [
        /// Play or pause
        PlayPause,
        /// Step back
        StepBack,
        /// Step forward
        StepForward,
        /// Jump to start
        SeekStart,
        /// Jump to end
        SeekEnd,
        /// Play faster
        SpeedUp,
        /// Play slower
        SpeedDown,
    ]
);

/// The key context a window carries while the document it shows plays back,
/// so the playback keys reach a clock that is there to drive.
pub const KEY_CONTEXT: KeyScope = KeyScope::new("Playback", "Playback");

/// Size of the text the playback controls are set in.
const TEXT_SIZE: Pixels = px(12.);
