//! Microphone capture via cpal. One `record()` call: open stream, collect
//! until the endpoint detector fires or `max_duration` elapses, tear down.
//! No daemon, no background threads left running.

use anyhow::Result;
use std::time::Duration;

use crate::dsp::VadConfig;

#[derive(Debug, Clone)]
pub struct RecordConfig {
    /// Substring match on the input device name; `None` = system default.
    pub device: Option<String>,
    pub max_duration: Duration,
    pub vad: VadConfig,
    pub target_rms: f32,
    pub max_gain: f32,
}

/// Names of all input devices, for `dictate --list-devices`.
pub fn list_input_devices() -> Result<Vec<String>> {
    todo!()
}

/// Record one utterance from the microphone.
///
/// Returns 16 kHz mono f32 that has been DC-blocked and gain-normalized,
/// with leading silence trimmed. Blocks until:
/// - the endpoint detector fires (`vad`), or
/// - `max_duration` elapses (returns what was captured), or
/// - the start timeout fires (error: no speech detected).
///
/// Errors must name the device and say what to do (e.g. list devices,
/// check microphone permissions/mute).
pub fn record(cfg: &RecordConfig) -> Result<Vec<f32>> {
    let _ = cfg;
    todo!()
}
