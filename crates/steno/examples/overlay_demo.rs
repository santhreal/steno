//! Drive the real overlay through every state for visual/X testing.
//! Dev tool only: `cargo run --release --example overlay_demo`.
//! Cycles Recording → Transcribing → Done → Error with pauses so the
//! window can be screenshotted, detected, and click-tested.

use std::thread::sleep;
use std::time::Duration;

use steno_core::config::UiConfig;
use steno_platform::{Overlay, Stage};

fn main() {
    let ov = Overlay::start(&UiConfig::default());
    if !ov.active() {
        eprintln!("overlay failed to start (no DISPLAY/ARGB/font)");
        std::process::exit(1);
    }
    for (stage, ms) in [
        (Stage::Recording, 4000u64),
        (Stage::Transcribing, 3000),
        (Stage::Done, 2000),
        (Stage::Error, 2000),
        (Stage::Hidden, 500),
    ] {
        ov.set(stage);
        sleep(Duration::from_millis(ms));
    }
}
