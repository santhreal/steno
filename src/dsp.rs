//! Sample-domain DSP: WAV reading, resampling, DC blocking, gain
//! normalization, and energy-based endpoint (voice activity) detection.
//!
//! Everything here is pure logic over `&[f32]` — no device access (that is
//! `audio.rs`) — so every piece is unit-testable without a microphone.

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

/// whisper.cpp's required input rate.
pub const WHISPER_RATE: u32 = 16_000;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DspConfig {
    /// Normalize recordings toward this RMS (0..1). Quiet mics are the top
    /// cause of bad whisper decodes.
    pub target_rms: f32,
    /// Never boost more than this factor, so silence is not amplified into
    /// noise.
    pub max_gain: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            target_rms: 0.1,
            max_gain: 8.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    /// Stop after this much continuous trailing silence.
    pub silence_ms: u32,
    /// Ignore "speech" bursts shorter than this (clicks, bumps).
    pub min_speech_ms: u32,
    /// Give up waiting for speech to start after this long.
    pub start_timeout_secs: u64,
    /// RMS of one analysis window that counts as speech (0..1).
    pub speech_threshold: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            silence_ms: 900,
            min_speech_ms: 250,
            start_timeout_secs: 10,
            speech_threshold: 0.01,
        }
    }
}

/// Read a WAV file, mix all channels down to mono, convert to f32 in
/// [-1.0, 1.0] regardless of source bit depth. Returns (samples, rate).
/// Errors must name the path and the offending property.
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let _ = path;
    todo!()
}

/// Resample `input` from `from_rate` to `to_rate` with a sinc resampler
/// (rubato). A matching rate must not run the resampler.
pub fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>> {
    let _ = (input, from_rate, to_rate);
    todo!()
}

/// First-order DC-blocking high-pass: y[n] = x[n] - x[n-1] + r * y[n-1],
/// r tuned so the corner sits near 5 Hz. Removes mic DC offset and rumble.
#[derive(Debug, Clone)]
pub struct DcBlock {
    x1: f32,
    y1: f32,
    r: f32,
}

impl DcBlock {
    pub fn new(rate: u32) -> Self {
        let _ = rate;
        todo!()
    }
    pub fn process(&mut self, samples: &mut [f32]) {
        let _ = samples;
        todo!()
    }
}

/// Scale `samples` so the whole buffer sits at `target_rms`, with gain
/// clamped to [1/max_gain, max_gain] and hard-limiting any sample that
/// would exceed [-1.0, 1.0] after gain. Silent buffers stay silent.
pub fn normalize(samples: &mut [f32], target_rms: f32, max_gain: f32) {
    let _ = (samples, target_rms, max_gain);
    todo!()
}

/// What `Endpoint::feed` concluded about the chunk it just saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// No speech detected yet, still inside the start timeout.
    WaitingForSpeech,
    /// Speech is (or recently was) present.
    InSpeech,
    /// `silence_ms` of silence after at least `min_speech_ms` of speech:
    /// the utterance is over.
    Endpoint,
    /// No speech within `start_timeout_secs` of feeding.
    StartTimeout,
}

/// Energy VAD with hangover. Feed equal-size chunks (~30 ms of mono audio).
/// Tracks total fed duration itself; no clocks needed.
pub struct Endpoint {
    // agent-defined state
}

impl Endpoint {
    pub fn new(cfg: VadConfig, rate: u32) -> Self {
        let _ = (cfg, rate);
        todo!()
    }
    pub fn feed(&mut self, chunk: &[f32]) -> VadEvent {
        let _ = chunk;
        todo!()
    }
    /// True once at least `min_speech_ms` of speech has been seen.
    pub fn speech_started(&self) -> bool {
        todo!()
    }
}
