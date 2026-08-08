//! Microphone capture via cpal.
//!
//! - `record()` -- open stream, collect until the VAD endpoint / timeout, tear down.
//! - `record_while()` -- open stream, collect until a stop flag or `max_duration`, tear down.
//!
//! Both leave no capture threads running after they return.

use anyhow::{Context, Result, anyhow, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::dsp::{self, Endpoint, VadConfig, VadEvent, STT_RATE};

/// Configuration parameters for microphone recording.
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
    let host = cpal::default_host();
    let devices = host.input_devices().context(
        "failed to enumerate audio input devices: check that PipeWire/PulseAudio is running",
    )?;
    let names: Vec<String> = devices.map(|d| d.to_string()).collect();
    if names.is_empty() {
        bail!(
            "no audio input devices found: connect a microphone and check that PipeWire/PulseAudio is running"
        );
    }
    Ok(names)
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
    let host = cpal::default_host();
    let device = select_device(&host, cfg.device.as_deref())?;
    let dev_name = device.to_string();

    // Negotiate a stream config: cpal's heuristics rank f32 first and
    // standard rates (48 kHz, then 44.1 kHz) above exotic ones.
    let ranges: Vec<_> = device
        .supported_input_configs()
        .with_context(|| {
            format!("failed to query input formats of device '{dev_name}': is it still connected?")
        })?
        .collect();
    let range = ranges
        .iter()
        .max_by(|a, b| a.cmp_default_heuristics(b))
        .ok_or_else(|| {
            anyhow!(
                "device '{dev_name}' supports no input stream formats: pick another device (`dictate --list-devices`)"
            )
        })?;
    let chosen = range
        .try_with_standard_sample_rate()
        .unwrap_or_else(|| range.with_max_sample_rate());
    let format = chosen.sample_format();
    let stream_config = chosen.config();
    let channels = stream_config.channels as usize;
    let dev_rate = stream_config.sample_rate;
    log::info!("recording from '{dev_name}' at {dev_rate} Hz, {channels} channel(s), {format:?}");

    let (tx, rx) = mpsc::channel::<Msg>();
    let stream = match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::F64 => build_stream::<f64>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I8 => build_stream::<i8>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I24 => {
            build_stream::<cpal::I24>(&device, &stream_config, tx, &dev_name)
        }
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U8 => build_stream::<u8>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U24 => {
            build_stream::<cpal::U24>(&device, &stream_config, tx, &dev_name)
        }
        cpal::SampleFormat::U32 => build_stream::<u32>(&device, &stream_config, tx, &dev_name),
        other => bail!(
            "device '{dev_name}' only offers unsupported sample format {other:?}: pick another device (`dictate --list-devices`)"
        ),
    }?;
    stream.play().with_context(|| {
        format!("failed to start capture on device '{dev_name}': check microphone permissions")
    })?;

    let result = capture_loop(&rx, &stream, cfg, dev_rate, &dev_name);
    // Stream (and its capture thread) is torn down before we return.
    drop(stream);
    let (captured, speech_started) = result?;

    if captured.is_empty() || !speech_started {
        bail!(
            "no speech detected on device '{dev_name}': check the microphone is not muted and the right device is selected (`dictate --list-devices`)"
        );
    }

    let mut samples = dsp::resample(&captured, dev_rate, STT_RATE).with_context(|| {
        format!("failed to resample recording from {dev_rate} Hz to {STT_RATE} Hz")
    })?;
    let mut dc = dsp::DcBlock::new(STT_RATE);
    dc.process(&mut samples);
    dsp::normalize(&mut samples, cfg.target_rms, cfg.max_gain);
    trim_leading_silence(&mut samples, cfg.vad.speech_threshold);
    if samples.is_empty() {
        bail!(
            "no speech detected on device '{dev_name}': check the microphone is not muted and the right device is selected (`dictate --list-devices`)"
        );
    }
    Ok(samples)
}


/// Record while `stop` is clear. Ends on `stop`, `max_duration`, or capture
/// failure. If `discard` is set, all DSP is skipped and an empty `Vec` is
/// returned immediately (canceling transcription). Returns processed 16 kHz
/// mono samples, or an empty `Vec` when the hold produced no usable speech or
/// was canceled.
pub fn record_while(cfg: &RecordConfig, stop: &AtomicBool, discard: &AtomicBool) -> Result<Vec<f32>> {
    let host = cpal::default_host();
    let device = select_device(&host, cfg.device.as_deref())?;
    let dev_name = device.to_string();

    let ranges: Vec<_> = device
        .supported_input_configs()
        .with_context(|| {
            format!("failed to query input formats of device '{dev_name}': is it still connected?")
        })?
        .collect();
    let range = ranges
        .iter()
        .max_by(|a, b| a.cmp_default_heuristics(b))
        .ok_or_else(|| {
            anyhow!(
                "device '{dev_name}' supports no input stream formats: pick another device (`dictate --list-devices`)"
            )
        })?;
    let chosen = range
        .try_with_standard_sample_rate()
        .unwrap_or_else(|| range.with_max_sample_rate());
    let format = chosen.sample_format();
    let stream_config = chosen.config();
    let channels = stream_config.channels as usize;
    let dev_rate = stream_config.sample_rate;
    log::info!(
        "push-to-talk from '{dev_name}' at {dev_rate} Hz, {channels} channel(s), {format:?}"
    );

    let (tx, rx) = mpsc::channel::<Msg>();
    let stream = match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::F64 => build_stream::<f64>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I8 => build_stream::<i8>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::I24 => {
            build_stream::<cpal::I24>(&device, &stream_config, tx, &dev_name)
        }
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U8 => build_stream::<u8>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, tx, &dev_name),
        cpal::SampleFormat::U24 => {
            build_stream::<cpal::U24>(&device, &stream_config, tx, &dev_name)
        }
        cpal::SampleFormat::U32 => build_stream::<u32>(&device, &stream_config, tx, &dev_name),
        other => bail!(
            "device '{dev_name}' only offers unsupported sample format {other:?}: pick another device (`dictate --list-devices`)"
        ),
    }?;
    stream.play().with_context(|| {
        format!("failed to start capture on device '{dev_name}': check microphone permissions")
    })?;

    let result = capture_until_stop(&rx, &stream, cfg, dev_rate, &dev_name, stop);
    drop(stream);
    let captured = result?;
    // A cancelled utterance skips all DSP and returns empty immediately --
    // seconds of sinc resampling must not delay the cancel.
    if discard.load(Ordering::Relaxed) || captured.is_empty() {
        return Ok(Vec::new());
    }

    let mut samples = dsp::resample(&captured, dev_rate, STT_RATE).with_context(|| {
        format!("failed to resample recording from {dev_rate} Hz to {STT_RATE} Hz")
    })?;
    let mut dc = dsp::DcBlock::new(STT_RATE);
    dc.process(&mut samples);
    dsp::normalize(&mut samples, cfg.target_rms, cfg.max_gain);
    trim_leading_silence(&mut samples, cfg.vad.speech_threshold);
    Ok(samples)
}

/// Find the requested device, or the system default. Substring match is
/// case-insensitive; a miss lists every available device name.
fn select_device(host: &cpal::Host, needle: Option<&str>) -> Result<cpal::Device> {
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .context(
            "failed to enumerate audio input devices: check that PipeWire/PulseAudio is running",
        )?
        .collect();
    let names: Vec<String> = devices.iter().map(|d| d.to_string()).collect();
    match needle {
        None => {
            if let Some(d) = host.default_input_device() {
                return Ok(d);
            }
            if names.is_empty() {
                bail!(
                    "no audio input devices found: connect a microphone and check that PipeWire/PulseAudio is running"
                );
            }
            bail!(
                "no default input device: select one with `dictate --device` (available: {})",
                names.join(", ")
            );
        }
        Some(n) => {
            let lower = n.to_lowercase();
            devices
                .into_iter()
                .find(|d| d.to_string().to_lowercase().contains(&lower))
                .ok_or_else(|| {
                    anyhow!(
                        "no input device matching '{n}': available devices: {}",
                        names.join(", ")
                    )
                })
        }
    }
}

/// Audio pushed from the capture callback to the consumer loop.
enum Msg {
    /// Mono f32 frames at the device rate.
    Data(Vec<f32>),
    /// Fatal stream error reported by the backend.
    Error(String),
}

/// Channel count for the downmix, rejecting pathological device configs.
/// `chunks_exact(0)` panics, and inside the cpal callback that unwind
/// would cross the FFI boundary, so a 0-channel device must be refused
/// before the stream is built.
fn mono_channels(config: &cpal::StreamConfig, dev_name: &str) -> Result<usize> {
    if config.channels == 0 {
        bail!(
            "device '{dev_name}' reports an input stream with 0 channels: pick another device (`dictate --list-devices`)"
        );
    }
    Ok(config.channels as usize)
}

/// Open an input stream whose callback downmixes to mono f32 and forwards
/// frames over the channel. Generic over the device's native sample type.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Msg>,
    dev_name: &str,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let channels = mono_channels(config, dev_name)?;
    let data_tx = tx.clone();
    let err_tx = tx;
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| {
                let inv = 1.0 / channels as f32;
                let mono: Vec<f32> = data
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() * inv)
                    .collect();
                // A send failure means the consumer is gone; nothing to do.
                let _ = data_tx.send(Msg::Data(mono));
            },
            move |e| {
                let _ = err_tx.send(Msg::Error(e.to_string()));
            },
            None,
        )
        .with_context(|| {
            format!(
                "failed to open input stream on device '{dev_name}' — check microphone permissions and that no other application holds it"
            )
        })
}

/// Consume frames until the endpoint detector fires, the start timeout
/// fires, or `max_duration` elapses. On an endpoint, the trailing silence
/// that triggered it is trimmed from the buffer. Returns the captured frames
/// and whether the VAD ever confirmed speech.
fn capture_loop(
    rx: &mpsc::Receiver<Msg>,
    stream: &cpal::Stream,
    cfg: &RecordConfig,
    dev_rate: u32,
    dev_name: &str,
) -> Result<(Vec<f32>, bool)> {
    let chunk_frames = ((dev_rate as f64 * 0.03).round() as usize).max(1); // ~30 ms
    let max_frames = (cfg.max_duration.as_secs_f64() * dev_rate as f64) as usize;
    let mut endpoint = Endpoint::new(cfg.vad, dev_rate);
    let mut pending: Vec<f32> = Vec::new(); // frames not yet fed to the VAD
    let mut captured: Vec<f32> = Vec::new();
    // Wall-clock guard only: if the backend goes silent we still honor
    // max_duration (+ slack) instead of hanging forever.
    let deadline = capture_deadline(cfg.max_duration);

    'outer: loop {
        if captured.len() >= max_frames {
            break;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    log::warn!("device '{dev_name}' stopped delivering frames; ending capture");
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Msg::Error(e) => {
                bail!(
                    "capture stream on device '{dev_name}' failed: {e} — check the microphone connection and permissions"
                );
            }
            Msg::Data(mono) => pending.extend_from_slice(&mono),
        }
        while pending.len() >= chunk_frames {
            let event = endpoint.feed(&pending[..chunk_frames]);
            captured.extend_from_slice(&pending[..chunk_frames]);
            pending.drain(..chunk_frames);
            match event {
                VadEvent::Endpoint => {
                    // Drop the trailing silence that closed the utterance.
                    let trim = (cfg.vad.silence_ms as u64 * dev_rate as u64 / 1000) as usize;
                    captured.truncate(captured.len().saturating_sub(trim));
                    break 'outer;
                }
                VadEvent::StartTimeout => {
                    bail!(
                        "no speech detected within {}s on device '{dev_name}' — check the microphone is not muted and the right device is selected (`dictate --list-devices`)",
                        cfg.vad.start_timeout_secs
                    );
                }
                _ => {
                    if captured.len() >= max_frames {
                        break 'outer;
                    }
                }
            }
        }
    }
    // Ensure the callback cannot outlive this function's use of the channel.
    stream.pause().ok();
    Ok((captured, endpoint.speech_started()))
}


/// Collect frames until `stop` is set, `max_duration` elapses, or the
/// backend goes silent past the wall-clock guard. No VAD endpoint; the
/// hotkey release is the endpoint.
fn capture_until_stop(
    rx: &mpsc::Receiver<Msg>,
    stream: &cpal::Stream,
    cfg: &RecordConfig,
    dev_rate: u32,
    dev_name: &str,
    stop: &AtomicBool,
) -> Result<Vec<f32>> {
    let max_frames = (cfg.max_duration.as_secs_f64() * dev_rate as f64) as usize;
    let mut captured: Vec<f32> = Vec::new();
    let deadline = capture_deadline(cfg.max_duration);

    loop {
        if stop.load(Ordering::Relaxed) || captured.len() >= max_frames {
            break;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    log::warn!("device '{dev_name}' stopped delivering frames; ending capture");
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Msg::Error(e) => {
                bail!(
                    "capture stream on device '{dev_name}' failed: {e} — check the microphone connection and permissions"
                );
            }
            Msg::Data(mono) => {
                let room = max_frames.saturating_sub(captured.len());
                if room == 0 {
                    break;
                }
                if mono.len() > room {
                    captured.extend_from_slice(&mono[..room]);
                    break;
                }
                captured.extend_from_slice(&mono);
            }
        }
    }
    stream.pause().ok();
    Ok(captured)
}

/// Wall-clock deadline for a backend that stops delivering frames, or
/// `None` when `max_duration` is too large to add to `Instant::now()`
/// (a multi-century config): `Instant + Duration` panics on overflow, so
/// this uses checked arithmetic. `None` is safe — `max_frames` still
/// bounds the capture by frame count.
fn capture_deadline(max_duration: Duration) -> Option<Instant> {
    Instant::now().checked_add(max_duration.saturating_add(Duration::from_secs(2)))
}

/// Drop leading windows whose RMS stays under `threshold`.
fn trim_leading_silence(samples: &mut Vec<f32>, threshold: f32) {
    let hop = (STT_RATE as usize / 100).max(1); // 10 ms windows
    let mut cut = 0;
    while cut + hop <= samples.len() {
        let w = &samples[cut..cut + hop];
        let rms = (w
            .iter()
            .map(|s| {
                let val = if s.is_finite() { s.abs() } else { 0.0 };
                val * val
            })
            .sum::<f32>()
            / hop as f32)
            .sqrt();
        if rms >= threshold {
            break;
        }
        cut += hop;
    }
    if cut > 0 {
        samples.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    //! WHY: Audio device configuration, deadline calculation, and silence trimming
    //! must handle edge cases like 0-channel devices, duration overflow, and short buffers
    //! safely without panics or FFI unwind violations.
    use super::*;

    fn stream_config(channels: u16) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels,
            sample_rate: 48_000,
            buffer_size: cpal::BufferSize::Default,
        }
    }

    /// WHY: a device reporting 0 channels used to reach the capture
    /// callback, where chunks_exact(0) panics: an unwind across the cpal
    /// FFI boundary. The device must be refused with an actionable error
    /// before the stream is built.
    #[test]
    fn mono_channels_rejects_zero_channel_devices() {
        let err = mono_channels(&stream_config(0), "fake-dev")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("fake-dev") && err.contains("0 channels"),
            "error must name the device and the problem, got: {err}"
        );
        assert_eq!(mono_channels(&stream_config(1), "fake-dev").unwrap(), 1);
        assert_eq!(mono_channels(&stream_config(8), "fake-dev").unwrap(), 8);
    }

    /// WHY: max_duration comes from user TOML as u64 seconds. The old
    /// code did `Instant::now() + max_duration + slack`, which panics on
    /// overflow for absurd (multi-century) values. The deadline must be
    /// computed with checked arithmetic and degrade to None.
    #[test]
    fn capture_deadline_never_panics_on_absurd_durations() {
        // Sane values produce a deadline in the near future.
        let d = capture_deadline(Duration::from_secs(30)).unwrap();
        assert!(d > Instant::now());
        assert!(d <= Instant::now() + Duration::from_secs(33));
        // Absurd values must not panic; None means "rely on frame count".
        let _ = capture_deadline(Duration::from_secs(u64::MAX));
        let _ = capture_deadline(Duration::MAX);
    }

    /// WHY: trim_leading_silence walks 10 ms windows; buffers shorter
    /// than one window, empty buffers, and all-silence buffers are
    /// boundary cases that must not panic or corrupt the sample count.
    #[test]
    fn trim_leading_silence_boundary_cases() {
        // Empty input: no-op.
        let mut s: Vec<f32> = Vec::new();
        trim_leading_silence(&mut s, 0.01);
        assert!(s.is_empty());

        // Shorter than one 10 ms window (160 samples): untouched, since
        // no full window can be judged.
        let mut s = vec![0.0f32; 100];
        trim_leading_silence(&mut s, 0.01);
        assert_eq!(s.len(), 100);

        // All silence, exact multiple of the window: everything drains
        // (the caller then errors). A partial tail window is kept — only
        // full windows are judged.
        let mut s = vec![0.0f32; 480];
        trim_leading_silence(&mut s, 0.01);
        assert!(s.is_empty());
        let mut s = vec![0.0f32; 500];
        trim_leading_silence(&mut s, 0.01);
        assert_eq!(s.len(), 20, "the partial tail window is not a full window");

        // Leading silence followed by speech: exactly the silent prefix
        // (3 full windows) is removed, the loud tail is untouched.
        let mut s = vec![0.0f32; 480];
        s.extend_from_slice(&vec![0.5f32; 320]);
        trim_leading_silence(&mut s, 0.01);
        assert_eq!(s.len(), 320);
        assert!(s.iter().all(|&v| v == 0.5));
    }
    /// WHY: trim_leading_silence must treat non-finite samples (NaN/Inf) as
    /// silence (0.0 magnitude) rather than energy > threshold or causing panics/NaNs.
    #[test]
    fn trim_leading_silence_handles_nan_samples_safely() {
        let mut s = vec![f32::NAN; 480];
        s.extend_from_slice(&vec![0.5f32; 320]);
        trim_leading_silence(&mut s, 0.01);
        assert_eq!(s.len(), 320);
        assert!(s.iter().all(|&v| v == 0.5));
    }
}
