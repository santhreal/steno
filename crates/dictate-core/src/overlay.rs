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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullOverlay;

impl OverlayBackend for NullOverlay {
    fn set(&self, _stage: Stage) {}

    fn flash(&self, _ms: u64) {}

    fn active(&self) -> bool {
        false
    }
}
/// Closure-backed overlay for testing and embedding.
pub struct FnOverlay<F: Fn(Stage) + Send + Sync>(pub F);

impl<F: Fn(Stage) + Send + Sync> OverlayBackend for FnOverlay<F> {
    fn set(&self, stage: Stage) {
        (self.0)(stage);
    }

    fn flash(&self, _ms: u64) {
        (self.0)(Stage::Done);
    }

    fn active(&self) -> bool {
        true
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
    /// Window unmapped (idle between utterances).
    Hidden,
    /// Live capture (shown as "Transcribing" with waveform + timer).
    Recording,
    /// Decode in flight (shown as "Processing" with spinner).
    Transcribing,
    Done,
    Error,
}

#[cfg(test)]
mod tests {
    //! WHY: Overlay implementations (`FnOverlay`, `NullOverlay`) must correctly report active status
    //! and state transitions across recording, transcribing, done, and error stages.
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn fn_overlay_records_stages_and_flash() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let stages_clone = stages.clone();
        let overlay = FnOverlay(move |stage| {
            if let Ok(mut s) = stages_clone.lock() {
                s.push(stage);
            }
        });

        assert!(overlay.active());
        overlay.set(Stage::Recording);
        overlay.set(Stage::Transcribing);
        overlay.flash(180);

        if let Ok(s) = stages.lock() {
            assert_eq!(*s, [Stage::Recording, Stage::Transcribing, Stage::Done]);
        } else {
            panic!("mutex poisoned");
        }
    }
}
