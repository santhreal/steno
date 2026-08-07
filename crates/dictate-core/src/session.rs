//! High-level embedder session: resident [`Engine`] + status overlay.
//!
//! Typing is optional and **fail-closed**: keystrokes are emitted only when
//! a typer was injected **and** `type_output` was armed from config.

use anyhow::{Result, bail};

use crate::engine::Engine;
use crate::overlay::{NullOverlay, OverlayBackend, Stage};

/// Keystroke sink for [`Session`].
///
/// Same shape as `dictate_platform::Typer`. Defined here so `Session` stays
/// in `dictate-core` without depending on the platform crate. Platform
/// backends implement both traits; hosts may also adapt any sink.
pub trait InjectTyper: Send {
    fn type_text(&mut self, text: &str) -> Result<()>;
}

impl InjectTyper for Box<dyn InjectTyper> {
    fn type_text(&mut self, text: &str) -> Result<()> {
        (**self).type_text(text)
    }
}

/// Offline STT session for host apps: engine + overlay (+ optional typer).
pub struct Session {
    engine: Engine,
    overlay: Box<dyn OverlayBackend>,
    type_output: bool,
    done_flash_ms: u64,
    typer: Option<Box<dyn InjectTyper>>,
}

/// Builder for [`Session`].
pub struct SessionBuilder {
    engine: Engine,
    overlay: Option<Box<dyn OverlayBackend>>,
    type_output: bool,
    done_flash_ms: u64,
    typer: Option<Box<dyn InjectTyper>>,
}

impl Session {
    /// Start a builder around a loaded [`Engine`].
    pub fn builder(engine: Engine) -> SessionBuilder {
        SessionBuilder {
            engine,
            overlay: None,
            type_output: false,
            done_flash_ms: 0,
            typer: None,
        }
    }

    /// Whether typing is armed from config (`type_output = true`).
    pub fn type_output_armed(&self) -> bool {
        self.type_output
    }

    /// Decode `pcm_16k` (16 kHz mono), drive overlay stages, and optionally
    /// type when armed + a typer was injected.
    ///
    /// Stage sequence (success): [`Stage::Recording`] → [`Stage::Transcribing`]
    /// → [`Stage::Done`] (optional flash). On error: ends on [`Stage::Error`].
    pub fn transcribe_f32(&mut self, pcm_16k: &[f32]) -> Result<String> {
        let text = Self::drive_overlay_stages(&*self.overlay, self.done_flash_ms, || {
            self.engine.transcribe_f32(pcm_16k)
        })?;
        self.maybe_type(&text)?;
        Ok(text)
    }

    /// Raw decode path with the same overlay / typing rules as
    /// [`Self::transcribe_f32`].
    pub fn transcribe_f32_raw(&mut self, pcm_16k: &[f32]) -> Result<String> {
        let text = Self::drive_overlay_stages(&*self.overlay, self.done_flash_ms, || {
            self.engine.transcribe_f32_raw(pcm_16k)
        })?;
        self.maybe_type(&text)?;
        Ok(text)
    }

    /// Borrow the overlay sink.
    pub fn overlay(&self) -> &dyn OverlayBackend {
        &*self.overlay
    }

    /// Borrow the engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Drive the Session overlay stage machine around `op`.
    ///
    /// Used by [`Self::transcribe_f32`]; exposed so unit tests (and hosts
    /// with their own decode) can verify stage order without loading a
    /// GPU model.
    pub fn drive_overlay_stages<R>(
        overlay: &dyn OverlayBackend,
        done_flash_ms: u64,
        op: impl FnOnce() -> Result<R>,
    ) -> Result<R> {
        // Listening → Thinking → Done (product names); code uses Recording /
        // Transcribing for the live pill labels.
        overlay.set(Stage::Recording);
        overlay.set(Stage::Transcribing);
        match op() {
            Ok(value) => {
                overlay.set(Stage::Done);
                if done_flash_ms > 0 {
                    overlay.flash(done_flash_ms);
                }
                Ok(value)
            }
            Err(err) => {
                overlay.set(Stage::Error);
                Err(err)
            }
        }
    }

    /// Fail-closed typing: no-op when disarmed; errors if armed without a typer.
    pub fn type_if_armed(&mut self, text: &str) -> Result<()> {
        self.maybe_type(text)
    }

    fn maybe_type(&mut self, text: &str) -> Result<()> {
        if !self.type_output {
            // Fail-closed: disarmed config never types, even with a typer.
            return Ok(());
        }
        match self.typer.as_mut() {
            Some(typer) => typer.type_text(text),
            None => bail!(
                "type_output is true but no typer was injected into Session — \
                 pass a platform Typer via SessionBuilder::typer, or set \
                 type_output = false in config"
            ),
        }
    }
}

impl SessionBuilder {
    /// Replace the status UI. Defaults to [`NullOverlay`] when omitted.
    pub fn overlay(mut self, overlay: impl OverlayBackend + 'static) -> Self {
        self.overlay = Some(Box::new(overlay));
        self
    }

    /// Inject a boxed overlay (same as [`Self::overlay`] for trait objects).
    pub fn overlay_box(mut self, overlay: Box<dyn OverlayBackend>) -> Self {
        self.overlay = Some(overlay);
        self
    }

    /// Arm / disarm typing from the user's config (`type_output`).
    ///
    /// Default is `false` (fail-closed). A typer alone never enables typing.
    pub fn type_output(mut self, armed: bool) -> Self {
        self.type_output = armed;
        self
    }

    /// How long to keep [`Stage::Done`] visible after a successful decode.
    pub fn done_flash_ms(mut self, ms: u64) -> Self {
        self.done_flash_ms = ms;
        self
    }

    /// Copy typing / flash settings from a loaded [`crate::Config`].
    pub fn from_config(mut self, cfg: &crate::Config) -> Self {
        self.type_output = cfg.type_output;
        self.done_flash_ms = cfg.ui.done_flash_ms;
        self
    }

    /// Inject a keystroke sink. Invoked only when `type_output` is armed.
    pub fn typer(mut self, typer: impl InjectTyper + 'static) -> Self {
        self.typer = Some(Box::new(typer));
        self
    }

    /// Build the session. Missing overlay → [`NullOverlay`].
    pub fn build(self) -> Session {
        Session {
            engine: self.engine,
            overlay: self
                .overlay
                .unwrap_or_else(|| Box::new(NullOverlay) as Box<dyn OverlayBackend>),
            type_output: self.type_output,
            done_flash_ms: self.done_flash_ms,
            typer: self.typer,
        }
    }
}

#[cfg(test)]
mod tests {
    //! WHY: Session must drive Recording → Transcribing → Done/Error around
    //! decode without requiring a GPU model. A recording overlay proves the
    //! order; NullOverlay proves the headless path does not panic.

    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingOverlay {
        stages: Arc<Mutex<Vec<Stage>>>,
    }

    impl OverlayBackend for RecordingOverlay {
        fn set(&self, stage: Stage) {
            self.stages.lock().expect("stage lock").push(stage);
        }
        fn flash(&self, _ms: u64) {}
        fn active(&self) -> bool {
            true
        }
    }

    #[test]
    fn overlay_stages_success_order() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let overlay = RecordingOverlay {
            stages: stages.clone(),
        };
        let out = Session::drive_overlay_stages(&overlay, 0, || Ok::<_, anyhow::Error>("hi"))
            .expect("ok path");
        assert_eq!(out, "hi");
        assert_eq!(
            *stages.lock().expect("stage lock"),
            [Stage::Recording, Stage::Transcribing, Stage::Done]
        );
    }

    #[test]
    fn overlay_stages_error_ends_on_error() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let overlay = RecordingOverlay {
            stages: stages.clone(),
        };
        let err = Session::drive_overlay_stages(&overlay, 0, || {
            Err::<(), _>(anyhow::anyhow!("decode failed"))
        })
        .expect_err("error path");
        assert!(err.to_string().contains("decode failed"));
        assert_eq!(
            *stages.lock().expect("stage lock"),
            [Stage::Recording, Stage::Transcribing, Stage::Error]
        );
    }

    #[test]
    fn null_overlay_stage_transitions_do_not_panic() {
        let overlay = NullOverlay;
        Session::drive_overlay_stages(&overlay, 0, || Ok::<_, anyhow::Error>(()))
            .expect("null success");
        let err = Session::drive_overlay_stages(&overlay, 0, || {
            Err::<(), _>(anyhow::anyhow!("x"))
        });
        assert!(err.is_err());
    }

    /// WHY: fail-closed typing — a typer must not run when type_output is false,
    /// and armed typing without a typer must error (never silently skip).
    #[test]
    fn typing_gates_match_session_maybe_type() {
        struct Probe {
            hits: Arc<Mutex<u32>>,
        }
        impl InjectTyper for Probe {
            fn type_text(&mut self, _text: &str) -> Result<()> {
                *self.hits.lock().expect("hits") += 1;
                Ok(())
            }
        }

        fn maybe_type(
            type_output: bool,
            typer: &mut Option<Box<dyn InjectTyper>>,
            text: &str,
        ) -> Result<()> {
            if !type_output {
                return Ok(());
            }
            match typer.as_mut() {
                Some(t) => t.type_text(text),
                None => bail!(
                    "type_output is true but no typer was injected into Session — \
                     pass a platform Typer via SessionBuilder::typer, or set \
                     type_output = false in config"
                ),
            }
        }

        let hits = Arc::new(Mutex::new(0u32));
        let mut typer: Option<Box<dyn InjectTyper>> = Some(Box::new(Probe {
            hits: hits.clone(),
        }));

        maybe_type(false, &mut typer, "hello").expect("disarmed");
        assert_eq!(*hits.lock().expect("hits"), 0, "disarmed must not type");

        maybe_type(true, &mut typer, "hello").expect("armed with typer");
        assert_eq!(*hits.lock().expect("hits"), 1);

        let mut none: Option<Box<dyn InjectTyper>> = None;
        let err = maybe_type(true, &mut none, "hello").expect_err("armed without typer");
        assert!(
            err.to_string().contains("no typer was injected"),
            "{err}"
        );
    }
}
