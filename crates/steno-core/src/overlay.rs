//! Overlay status types shared by the daemon, CLI, and embedders.
//!
//! Concrete window backends live in `steno-platform`. This module owns
//! the data (`Stage`) and the sink trait so `steno-core` never depends
//! on X11 / fontdue / tiny-skia.

/// Status UI sink for the daemon / embedders.
///
/// Method names match the concrete X11 pill API so call sites can move
/// to `Box<dyn OverlayBackend>` without renaming.
pub trait OverlayBackend: Send {
    /// Set the current status stage.
    fn set(&self, stage: Stage);
    /// Hold the final stage visible for `ms` milliseconds.
    fn flash(&self, ms: u64);
    /// True while the backend is live (fail-open UIs may return false).
    fn active(&self) -> bool;
}

/// Headless / test / embedder stand-in: every method is a no-op.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullOverlay;
impl NullOverlay {
    /// Construct a null overlay.
    pub fn new() -> Self {
        Self
    }
}


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


/// Status stage shown by the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Window unmapped (idle between utterances).
    Hidden,
    /// Live capture (shown as "Transcribing" with waveform + timer).
    Recording,
    /// Decode in flight (shown as "Processing" with spinner).
    Transcribing,
    /// Decode complete (shown as a checkmark).
    Done,
    /// Decode failed (shown as an X).
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

    #[test]
    fn null_overlay_new_constructor_and_backend_trait_methods() {
        // WHY: NullOverlay::new must produce a valid NullOverlay with active() == false
        // and safe no-op set/flash calls.
        let overlay = NullOverlay::new();
        assert_eq!(overlay, NullOverlay);
        assert!(!overlay.active());
        overlay.set(Stage::Recording);
        overlay.set(Stage::Done);
        overlay.flash(100);
    }

    #[test]
    fn fn_overlay_all_stages_and_boxed_dispatch() {
        // WHY: FnOverlay must forward all Stage variants (Hidden, Recording, Transcribing, Done, Error),
        // translate flash to Stage::Done, report active() == true, and work through Box<dyn OverlayBackend>.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec_clone = recorded.clone();
        let raw = FnOverlay(move |stage| {
            if let Ok(mut r) = rec_clone.lock() {
                r.push(stage);
            }
        });

        assert!(raw.active());
        let boxed: Box<dyn OverlayBackend> = Box::new(raw);
        assert!(boxed.active());

        for stage in [
            Stage::Hidden,
            Stage::Recording,
            Stage::Transcribing,
            Stage::Done,
            Stage::Error,
        ] {
            boxed.set(stage);
        }
        boxed.flash(250);

        if let Ok(r) = recorded.lock() {
            assert_eq!(
                *r,
                [
                    Stage::Hidden,
                    Stage::Recording,
                    Stage::Transcribing,
                    Stage::Done,
                    Stage::Error,
                    Stage::Done, // from flash(250)
                ]
            );
        }
    }
}
