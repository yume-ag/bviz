use crate::GraphPanel;
use bviz_graph_layout::Id;
use bviz_time::Instant;
use gpui::Context;
use std::time::Duration;

/// How long the just-toggled box stays emphasized before the highlight fades.
const HIGHLIGHT_LINGER: Duration = Duration::from_millis(1100);

/// Repaint interval while the emphasis fades, short enough that the fade reads
/// as continuous.
const FADE_TICK: Duration = Duration::from_millis(33);

/// A toggled box, held until its emphasis has faded.
struct Toggled {
    id: Id,
    /// When the highlight began fading; `None` until the relayout lands, so the
    /// linger runs from the moment the box becomes visible and a relayout slower
    /// than the linger cannot surface an already-faded box.
    fading_since: Option<Instant>,
}

/// The emphasis on the just-toggled box: the box a relayout frames and
/// highlights, and the fade that ends it.
pub(crate) struct Emphasis {
    current: Option<Toggled>,
    /// Identifies the fade a timer drives, so a timer whose generation has been
    /// superseded retires instead of clearing a fresher emphasis.
    generation: u64,
}

/// What a fade timer's tick found.
enum Fade {
    /// A later toggle owns the emphasis, leaving this timer nothing to drive.
    Superseded,
    Fading,
    Faded,
}

impl Emphasis {
    pub(crate) fn none() -> Self {
        Self {
            current: None,
            generation: 0,
        }
    }

    /// Emphasizes the box `id`.
    pub(crate) fn toggled(&mut self, id: Id) {
        self.current = Some(Toggled {
            id,
            fading_since: None,
        });
        self.generation = self.generation.wrapping_add(1);
    }

    /// Drops the emphasis, retiring any fade timer driving it.
    pub(crate) fn clear(&mut self) {
        self.current = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// The emphasized box and the alpha its highlight paints at, falling linearly
    /// to zero across `HIGHLIGHT_LINGER` once the fade begins.
    pub(crate) fn highlight(&self) -> Option<(Id, f32)> {
        let toggled = self.current.as_ref()?;
        let alpha = match toggled.fading_since {
            Some(since) => (1.0 - since.elapsed().as_secs_f32() / HIGHLIGHT_LINGER.as_secs_f32())
                .clamp(0.0, 1.0),
            None => 0.0,
        };
        Some((toggled.id, alpha))
    }

    /// The box a settling relayout keeps in view, and the only one it chases.
    /// What surrounded the box before the relayout is deliberately left out: a
    /// toggle spreads those boxes apart, and framing them would drag the zoom
    /// out after them every time one is opened.
    pub(crate) fn framed(&self) -> Option<Id> {
        Some(self.current.as_ref()?.id)
    }

    /// Starts the fade of the emphasized box, yielding the generation its timer
    /// quotes back to [`Emphasis::tick`]; `None` when no box is emphasized.
    fn begin(&mut self) -> Option<u64> {
        self.current.as_mut()?.fading_since = Some(bviz_time::now());
        Some(self.generation)
    }

    fn tick(&mut self, generation: u64) -> Fade {
        if self.generation != generation {
            return Fade::Superseded;
        }
        match &self.current {
            Some(Toggled {
                fading_since: Some(since),
                ..
            }) if since.elapsed() < HIGHLIGHT_LINGER => Fade::Fading,
            _ => {
                self.current = None;
                Fade::Faded
            }
        }
    }
}

/// Fades the emphasis out, repainting as it goes and clearing it at the end.
/// Called as the relayout lands.
pub(crate) fn begin_fade(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let Some(generation) = view.emphasis.begin() else {
        return;
    };
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(FADE_TICK).await;
            let done = this.update(cx, |this, cx| match this.emphasis.tick(generation) {
                Fade::Superseded => true,
                Fade::Fading => {
                    this.invalidate_canvas_only(cx);
                    false
                }
                Fade::Faded => {
                    this.invalidate_canvas_only(cx);
                    true
                }
            });
            if done.unwrap_or(true) {
                break;
            }
        }
    })
    .detach();
}
