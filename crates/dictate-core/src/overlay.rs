//! Overlay status types shared by the daemon, CLI, and embedders.
//!
//! Concrete window backends live in `dictate-platform`. This module owns
//! the data (`Stage`) and the sink trait so `dictate-core` never depends
//! on X11 / fontdue / tiny-skia.

/// Status UI sink for the daemon / embedders.
///
/// Method names match the concrete X11 pill API so call sites can move
/// to `Box<dyn OverlayBackend>` without renaming.
pub trait OverlayBackend: Send {
    fn set(&self, stage: Stage);
    fn flash(&self, ms: u64);
    /// True while the backend is live (fail-open UIs may return false).
    fn active(&self) -> bool;
}

/// Headless / test / embedder stand-in: every method is a no-op.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullOverlay;

impl OverlayBackend for NullOverlay {
    fn set(&self, _stage: Stage) {}

    fn flash(&self, _ms: u64) {}

    fn active(&self) -> bool {
        false
    }
}

impl OverlayBackend for Box<dyn OverlayBackend> {
    fn set(&self, stage: Stage) {
        (**self).set(stage)
    }
    fn flash(&self, ms: u64) {
        (**self).flash(ms)
    }
    fn active(&self) -> bool {
        (**self).active()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Window unmapped — idle between utterances.
    Hidden,
    /// Live capture (shown as "Transcribing" with waveform + timer).
    Recording,
    /// Decode in flight (shown as "Processing" with spinner).
    Transcribing,
    Done,
    Error,
}
