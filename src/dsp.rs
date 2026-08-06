//! Sample-domain DSP: WAV reading, resampling, DC blocking, gain
//! normalization, and energy-based endpoint (voice activity) detection.
//!
//! Everything here is pure logic over `&[f32]` — no device access (that is
//! `audio.rs`) — so every piece is unit-testable without a microphone.

use anyhow::{Context, Result, bail};
use rubato::Resampler;
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
    let reader = hound::WavReader::open(path).with_context(|| {
        format!(
            "failed to open WAV file '{}' — check the path exists and is a valid WAV",
            path.display()
        )
    })?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    if channels == 0 {
        bail!(
            "WAV file '{}' has 0 channels — re-export it with at least one channel",
            path.display()
        );
    }
    if spec.sample_rate == 0 {
        bail!(
            "WAV file '{}' has a sample rate of 0 Hz — re-export it with a valid sample rate",
            path.display()
        );
    }

    let mut interleaved: Vec<f32> = Vec::new();
    match spec.sample_format {
        hound::SampleFormat::Float => {
            if spec.bits_per_sample != 32 {
                bail!(
                    "WAV file '{}' has an unsupported float bit depth of {} — re-export as 32-bit float or PCM",
                    path.display(),
                    spec.bits_per_sample
                );
            }
            for s in reader.into_samples::<f32>() {
                let v = s.with_context(|| {
                    format!("WAV file '{}' contains a corrupt sample", path.display())
                })?;
                interleaved.push(v.clamp(-1.0, 1.0));
            }
        }
        hound::SampleFormat::Int => {
            // Full-scale magnitude for signed PCM of this depth.
            let scale = match spec.bits_per_sample {
                1..=8 => (1u32 << 7) as f32,
                9..=16 => (1u32 << 15) as f32,
                17..=32 => (1u64 << (spec.bits_per_sample - 1)) as f32,
                _ => {
                    bail!(
                        "WAV file '{}' has an unsupported bit depth of {} — re-export as 8/16/24/32-bit PCM",
                        path.display(),
                        spec.bits_per_sample
                    );
                }
            };
            if spec.bits_per_sample <= 8 {
                for s in reader.into_samples::<i8>() {
                    let v = s.with_context(|| {
                        format!("WAV file '{}' contains a corrupt sample", path.display())
                    })?;
                    interleaved.push(v as f32 / scale);
                }
            } else if spec.bits_per_sample <= 16 {
                for s in reader.into_samples::<i16>() {
                    let v = s.with_context(|| {
                        format!("WAV file '{}' contains a corrupt sample", path.display())
                    })?;
                    interleaved.push(v as f32 / scale);
                }
            } else {
                // hound carries 24- and 32-bit PCM in i32 samples.
                for s in reader.into_samples::<i32>() {
                    let v = s.with_context(|| {
                        format!("WAV file '{}' contains a corrupt sample", path.display())
                    })?;
                    interleaved.push(v as f32 / scale);
                }
            }
        }
    }

    let inv = 1.0 / channels as f32;
    let mono: Vec<f32> = interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() * inv)
        .collect();
    Ok((mono, spec.sample_rate))
}

/// Resample `input` from `from_rate` to `to_rate` with a sinc resampler
/// (rubato). A matching rate must not run the resampler.
pub fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>> {
    if from_rate == 0 || to_rate == 0 {
        bail!(
            "cannot resample from {from_rate} Hz to {to_rate} Hz — sample rates must be greater than 0"
        );
    }
    if from_rate == to_rate {
        return Ok(input.to_vec());
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let params = rubato::SincInterpolationParameters::new(256, rubato::WindowFunction::BlackmanHarris2);
    let mut resampler = rubato::Async::<f32>::new_sinc(
        to_rate as f64 / from_rate as f64,
        1.0, // fixed ratio, no runtime adjustment
        &params,
        1024, // input frames per processing chunk
        1,
        rubato::FixedAsync::Input,
    )
    .with_context(|| format!("failed to build sinc resampler for {from_rate} Hz -> {to_rate} Hz"))?;

    let buf_in = rubato::audioadapter_buffers::direct::InterleavedSlice::new(input, 1, input.len())
        .context("failed to wrap input for resampling")?;
    let out = resampler
        .process_all(&buf_in, input.len(), None)
        .with_context(|| format!("failed to resample audio from {from_rate} Hz to {to_rate} Hz"))?;
    Ok(out.take_data())
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
        let r = if rate == 0 {
            0.0
        } else {
            (-2.0 * std::f32::consts::PI * 5.0 / rate as f32).exp()
        };
        Self { x1: 0.0, y1: 0.0, r }
    }
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let y = *s - self.x1 + self.r * self.y1;
            self.x1 = *s;
            self.y1 = y;
            *s = y;
        }
    }
}

/// Scale `samples` so the whole buffer sits at `target_rms`, with gain
/// clamped to [1/max_gain, max_gain] and hard-limiting any sample that
/// would exceed [-1.0, 1.0] after gain. Silent buffers stay silent.
pub fn normalize(samples: &mut [f32], target_rms: f32, max_gain: f32) {
    if samples.is_empty() {
        return;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    if !rms.is_finite() || rms == 0.0 {
        return;
    }
    let gain = (target_rms / rms).clamp(1.0 / max_gain, max_gain);
    for s in samples.iter_mut() {
        *s = (*s * gain).clamp(-1.0, 1.0);
    }
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
    cfg: VadConfig,
    rate: u32,
    fed_ms: f64,
    speech_ms: f64,
    trailing_silence_ms: f64,
    started: bool,
    latched: Option<VadEvent>,
}

impl Endpoint {
    pub fn new(cfg: VadConfig, rate: u32) -> Self {
        Self {
            cfg,
            rate,
            fed_ms: 0.0,
            speech_ms: 0.0,
            trailing_silence_ms: 0.0,
            started: false,
            latched: None,
        }
    }

    pub fn feed(&mut self, chunk: &[f32]) -> VadEvent {
        if let Some(e) = self.latched {
            return e;
        }
        if chunk.is_empty() || self.rate == 0 {
            return if self.started {
                VadEvent::InSpeech
            } else {
                VadEvent::WaitingForSpeech
            };
        }
        let dur_ms = chunk.len() as f64 * 1000.0 / self.rate as f64;
        self.fed_ms += dur_ms;
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        let is_speech = rms >= self.cfg.speech_threshold;

        if is_speech {
            self.speech_ms += dur_ms;
            self.trailing_silence_ms = 0.0;
            if self.speech_ms >= self.cfg.min_speech_ms as f64 {
                self.started = true;
            }
        } else if self.started {
            self.trailing_silence_ms += dur_ms;
            if self.trailing_silence_ms >= self.cfg.silence_ms as f64 {
                self.latched = Some(VadEvent::Endpoint);
                return VadEvent::Endpoint;
            }
        }

        if !self.started && self.fed_ms >= self.cfg.start_timeout_secs as f64 * 1000.0 {
            self.latched = Some(VadEvent::StartTimeout);
            return VadEvent::StartTimeout;
        }

        if self.started {
            VadEvent::InSpeech
        } else {
            VadEvent::WaitingForSpeech
        }
    }

    /// True once at least `min_speech_ms` of speech has been seen.
    pub fn speech_started(&self) -> bool {
        self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine(freq: f32, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Unique temp path per test; no tempfile crate needed.
    fn temp_wav(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dictate-test-{}-{}.wav", std::process::id(), name))
    }

    // ---- read_wav ----

    #[test]
    fn read_wav_16bit_mono_scales_to_unit_range() {
        let path = temp_wav("16mono");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for v in [0i16, 16384, -16384, 32767, -32768] {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }
        let (samples, rate) = read_wav(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(rate, 16_000);
        let expected = [0.0, 0.5, -0.5, 32767.0 / 32768.0, -1.0];
        assert_eq!(samples.len(), expected.len());
        for (a, e) in samples.iter().zip(expected) {
            assert!((a - e).abs() < 1e-6, "got {a}, want {e}");
        }
    }

    #[test]
    fn read_wav_24bit_stereo_downmixes_to_mono() {
        let path = temp_wav("24stereo");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        // 24-bit full scale is 2^23 = 8388608; hound carries it in i32.
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            // left = +half scale, right = -half scale -> mono mean 0
            w.write_sample(4_194_304i32).unwrap();
            w.write_sample(-4_194_304i32).unwrap();
            // left = full scale, right = full scale -> mono 8388607/8388608
            w.write_sample(8_388_607i32).unwrap();
            w.write_sample(8_388_607i32).unwrap();
            w.finalize().unwrap();
        }
        let (samples, rate) = read_wav(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(rate, 44_100);
        assert_eq!(samples.len(), 2, "stereo must downmix to one sample per frame");
        assert!(samples[0].abs() < 1e-6, "opposite channels must cancel, got {}", samples[0]);
        let want = 8_388_607.0 / 8_388_608.0;
        assert!((samples[1] - want).abs() < 1e-6, "got {}, want {want}", samples[1]);
    }

    #[test]
    fn read_wav_32bit_float_passthrough() {
        let path = temp_wav("32float");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let input = [0.0f32, 0.25, -0.75, 1.0, -1.0];
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for v in input {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }
        let (samples, rate) = read_wav(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(samples.len(), input.len());
        for (a, e) in samples.iter().zip(input) {
            assert!((a - e).abs() < 1e-7);
        }
    }

    #[test]
    fn read_wav_missing_file_names_path() {
        let path = temp_wav("does-not-exist");
        let err = read_wav(&path).unwrap_err().to_string();
        assert!(
            err.contains("does-not-exist"),
            "error must name the path, got: {err}"
        );
    }

    // ---- resample ----

    /// Dominant frequency estimate via zero crossings over the middle of the
    /// signal (skip edges, where the sinc filter has transients).
    fn zero_crossing_freq(samples: &[f32], rate: u32) -> f32 {
        let start = samples.len() / 4;
        let end = samples.len() * 3 / 4;
        let mut crossings = 0usize;
        for w in samples[start..end].windows(2) {
            if (w[0] < 0.0) != (w[1] < 0.0) {
                crossings += 1;
            }
        }
        let secs = (end - start) as f32 / rate as f32;
        crossings as f32 / 2.0 / secs
    }

    #[test]
    fn resample_22050_to_16000_preserves_sine_frequency() {
        let input = sine(440.0, 22_050, 22_050, 0.8);
        let out = resample(&input, 22_050, WHISPER_RATE).unwrap();
        assert!(
            (out.len() as i64 - 16_000).abs() <= 4,
            "1s at 22050 Hz should become ~16000 samples, got {}",
            out.len()
        );
        let f = zero_crossing_freq(&out, WHISPER_RATE);
        assert!(
            (430.0..=450.0).contains(&f),
            "440 Hz sine resampled to {f} Hz dominant frequency"
        );
        // Resampled signal must still carry real energy, not be smoothed away.
        assert!(rms(&out[out.len() / 4..out.len() * 3 / 4]) > 0.3);
    }

    #[test]
    fn resample_matching_rate_is_exact_passthrough() {
        let input = sine(440.0, 16_000, 1600, 0.5);
        let out = resample(&input, 16_000, 16_000).unwrap();
        assert_eq!(out, input, "matching rates must not run the resampler");
    }

    #[test]
    fn resample_empty_input_returns_empty() {
        let out = resample(&[], 44_100, 16_000).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn resample_zero_rate_errors_instead_of_panicking() {
        assert!(resample(&[0.1, 0.2], 0, 16_000).is_err());
        assert!(resample(&[0.1, 0.2], 16_000, 0).is_err());
    }

    // ---- DcBlock ----

    #[test]
    fn dcblock_removes_dc_offset() {
        let rate = 16_000;
        let mut signal = sine(440.0, rate, rate as usize, 0.5);
        for s in signal.iter_mut() {
            *s += 0.3; // DC offset
        }
        let mut dc = DcBlock::new(rate);
        dc.process(&mut signal);
        // Skip the filter's settling region; the tail must center on zero.
        let tail = &signal[rate as usize / 2..];
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(mean.abs() < 0.01, "DC offset not removed, tail mean {mean}");
        // The AC component must survive: DC block is not a mute.
        assert!(rms(tail) > 0.2, "signal energy lost, tail rms {}", rms(tail));
    }

    // ---- normalize ----

    #[test]
    fn normalize_boosts_quiet_signal_to_target() {
        let mut signal = sine(440.0, 16_000, 16_000, 0.02); // rms ~0.0141
        normalize(&mut signal, 0.1, 8.0); // needs gain ~7.07, under the cap
        let r = rms(&signal);
        assert!(
            (r - 0.1).abs() < 0.005,
            "quiet signal should land at target RMS 0.1, got {r}"
        );
    }

    #[test]
    fn normalize_respects_gain_cap_on_near_silence() {
        let mut signal = sine(440.0, 16_000, 16_000, 0.001); // rms ~0.000707
        normalize(&mut signal, 0.1, 8.0); // would need ~141x, capped at 8x
        let peak = signal.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.008).abs() < 0.0005,
            "gain must be capped at 8x: peak {peak}, want ~0.008"
        );
    }

    #[test]
    fn normalize_attenuates_loud_signal_and_never_clips() {
        // Peak-heavy signal: gain > 1 would push the peaks past 1.0.
        let mut signal = vec![0.9f32, -0.9, 0.1, -0.1];
        normalize(&mut signal, 0.9, 10.0); // target RMS 0.9 -> gain ~1.41
        assert!(
            signal.iter().all(|s| s.abs() <= 1.0),
            "samples must be hard-limited to [-1, 1]: {signal:?}"
        );
        assert_eq!(signal[0], 1.0, "overshooting peak must be limited to 1.0");
        assert_eq!(signal[1], -1.0);
    }

    #[test]
    fn normalize_attenuates_when_above_target() {
        let mut signal = sine(440.0, 16_000, 16_000, 0.8); // rms ~0.566
        normalize(&mut signal, 0.1, 8.0); // gain ~0.177, inside [1/8, 8]
        let r = rms(&signal);
        assert!(
            (r - 0.1).abs() < 0.005,
            "loud signal should land at target RMS 0.1, got {r}"
        );
    }

    #[test]
    fn normalize_silence_stays_silent() {
        let mut signal = vec![0.0f32; 1024];
        normalize(&mut signal, 0.1, 8.0);
        assert!(signal.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn normalize_empty_input_does_not_panic() {
        normalize(&mut [], 0.1, 8.0);
    }

    // ---- Endpoint ----

    fn test_vad() -> VadConfig {
        VadConfig {
            silence_ms: 300,
            min_speech_ms: 100,
            start_timeout_secs: 2,
            speech_threshold: 0.05,
        }
    }

    const CHUNK: usize = 480; // 30 ms at 16 kHz

    fn silence() -> Vec<f32> {
        vec![0.0; CHUNK]
    }

    fn speech() -> Vec<f32> {
        sine(200.0, 16_000, CHUNK, 0.5) // rms ~0.35, well above threshold
    }

    #[test]
    fn endpoint_follows_silence_speech_silence_script() {
        let mut ep = Endpoint::new(test_vad(), 16_000);

        // 5 chunks of silence: still waiting.
        for _ in 0..5 {
            assert_eq!(ep.feed(&silence()), VadEvent::WaitingForSpeech);
            assert!(!ep.speech_started());
        }
        // 4 chunks (120 ms) of speech: crosses min_speech_ms = 100.
        assert_eq!(ep.feed(&speech()), VadEvent::WaitingForSpeech); // 30 ms
        assert!(!ep.speech_started());
        assert_eq!(ep.feed(&speech()), VadEvent::WaitingForSpeech); // 60 ms
        assert_eq!(ep.feed(&speech()), VadEvent::WaitingForSpeech); // 90 ms
        assert_eq!(ep.feed(&speech()), VadEvent::InSpeech); // 120 ms
        assert!(ep.speech_started());
        // 9 chunks (270 ms) of trailing silence: inside the 300 ms hangover.
        for _ in 0..9 {
            assert_eq!(ep.feed(&silence()), VadEvent::InSpeech);
        }
        // The 10th chunk reaches exactly 300 ms of trailing silence.
        assert_eq!(ep.feed(&silence()), VadEvent::Endpoint);
        // Terminal events latch.
        assert_eq!(ep.feed(&speech()), VadEvent::Endpoint);
    }

    #[test]
    fn endpoint_short_blip_never_starts_speech() {
        let mut ep = Endpoint::new(test_vad(), 16_000);
        // 2 chunks (60 ms) of "speech" — below the 100 ms minimum.
        assert_eq!(ep.feed(&speech()), VadEvent::WaitingForSpeech);
        assert_eq!(ep.feed(&speech()), VadEvent::WaitingForSpeech);
        assert!(!ep.speech_started());
        // Followed by silence, still waiting (blip must not count as an utterance).
        assert_eq!(ep.feed(&silence()), VadEvent::WaitingForSpeech);
        assert!(!ep.speech_started());
    }

    #[test]
    fn endpoint_start_timeout_fires_on_pure_silence() {
        let mut ep = Endpoint::new(test_vad(), 16_000);
        // 2 s timeout / 30 ms chunks: fires on the 67th chunk.
        for i in 0..66 {
            assert_eq!(ep.feed(&silence()), VadEvent::WaitingForSpeech, "chunk {i}");
        }
        assert_eq!(ep.feed(&silence()), VadEvent::StartTimeout);
        assert_eq!(ep.feed(&speech()), VadEvent::StartTimeout, "must latch");
        assert!(!ep.speech_started());
    }

    #[test]
    fn endpoint_trailing_silence_boundary_is_exact() {
        let cfg = test_vad(); // silence_ms = 300
        let mut ep = Endpoint::new(cfg, 16_000);
        for _ in 0..4 {
            ep.feed(&speech());
        }
        assert!(ep.speech_started());
        // One chunk short of the boundary: 270 ms < 300 ms.
        for _ in 0..9 {
            assert_eq!(ep.feed(&silence()), VadEvent::InSpeech);
        }
        // Exactly 300 ms of trailing silence -> Endpoint.
        assert_eq!(ep.feed(&silence()), VadEvent::Endpoint);
    }

    #[test]
    fn endpoint_speech_interrupts_trailing_silence() {
        let mut ep = Endpoint::new(test_vad(), 16_000);
        for _ in 0..4 {
            ep.feed(&speech());
        }
        for _ in 0..9 {
            ep.feed(&silence()); // 270 ms of trailing silence
        }
        // Speech resumes: hangover resets, no endpoint may fire.
        assert_eq!(ep.feed(&speech()), VadEvent::InSpeech);
        // A full 300 ms of fresh silence is now required.
        for _ in 0..9 {
            assert_eq!(ep.feed(&silence()), VadEvent::InSpeech);
        }
        assert_eq!(ep.feed(&silence()), VadEvent::Endpoint);
    }
}
