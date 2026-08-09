//! Speech-to-text via sherpa-onnx (Parakeet TDT). One `Transcriber` per
//! model; the daemon keeps it resident for its whole lifetime. Provider is
//! `"cuda"` (default), `"cpu"`, or `"metal"` (macOS) -- chosen by config,
//! never silently swapped.
//!
//! The model is a DIRECTORY with encoder/decoder/joiner ONNX files plus
//! tokens.txt (e.g. sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8).

use anyhow::{Context, Result, anyhow, bail, ensure};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::dsp::STT_RATE;

/// Resident sherpa-onnx Parakeet TDT speech-to-text recognizer.
pub struct Transcriber {
    /// Serialize decode: API thread + Caps Lock path share one model; the
    /// sherpa binding marks Sync but concurrent CUDA decode is still racy.
    recognizer: Mutex<OfflineRecognizer>,
}

impl Transcriber {
    /// Load a sherpa-onnx transducer model directory with the given
    /// execution `provider` (`"cuda"`, `"cpu"`, or `"metal"`).
    ///
    /// Fails closed: if the provider cannot init, this errors with a
    /// corrective hint: it never silently falls back.
    pub fn load(model_dir: &Path, n_threads: u32, provider: &str) -> Result<Self> {
        ensure!(
            (1..=(i32::MAX as u32)).contains(&n_threads),
            "invalid n_threads = {n_threads} — set it between 1 and {}",
            i32::MAX
        );
        ensure!(
            matches!(provider, "cuda" | "cpu" | "metal"),
            "invalid provider = {provider:?} — set it to \"cuda\", \"cpu\", or \"metal\" in config.toml"
        );
        let files = model_files(model_dir)?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(path_str(&files.encoder)?);
        config.model_config.transducer.decoder = Some(path_str(&files.decoder)?);
        config.model_config.transducer.joiner = Some(path_str(&files.joiner)?);
        config.model_config.tokens = Some(path_str(&files.tokens)?);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.provider = Some(provider.to_string());
        config.model_config.num_threads = n_threads as i32;

        let recognizer = OfflineRecognizer::create(&config).ok_or_else(|| {
            match provider {
                "cuda" => anyhow!(
                    "sherpa-onnx failed to load the model from {} with provider = \"cuda\" — \
                     check that the CUDA build is installed (SHERPA_ONNX_LIB_DIR), the GPU is free, \
                     and the model files are intact. For CI/headless hosts without NVIDIA, set \
                     provider = \"cpu\" in ~/.config/steno/config.toml (no silent fallback)",
                    model_dir.display()
                ),
                "metal" => anyhow!(
                    "sherpa-onnx failed to load the model from {} with provider = \"metal\" — \
                     check that the Metal build is installed (SHERPA_ONNX_LIB_DIR), the GPU is free, \
                     and the model files are intact. For hosts without Metal, set \
                     provider = \"cpu\" in ~/.config/steno/config.toml (no silent fallback)",
                    model_dir.display()
                ),
                _ => anyhow!(
                    "sherpa-onnx failed to load the model from {} with provider = \"cpu\" — \
                     check that sherpa-onnx is installed (SHERPA_ONNX_LIB_DIR) and the model \
                     files are intact",
                    model_dir.display()
                ),
            }
        })?;
        Ok(Self { recognizer: Mutex::new(recognizer) })
    }

    /// Transcribe 16 kHz mono f32 samples. Parakeet decodes an utterance
    /// in one shot (there are no partial segments), so `sink` is invoked
    /// exactly once with the raw transcript when it is non-empty.
    /// The callback requires a `'static` lifetime.
    pub fn transcribe_streaming(
        &self,
        samples: &[f32],
        mut sink: impl FnMut(&str) + 'static,
    ) -> Result<()> {
        let recognizer = self
            .recognizer
            .lock()
            .map_err(|_| anyhow!("transcriber lock poisoned — restart the daemon"))?;
        let stream = recognizer.create_stream();
        stream.accept_waveform(STT_RATE as i32, samples);
        recognizer.decode(&stream);
        let result = stream
            .get_result()
            .ok_or_else(|| anyhow!("sherpa-onnx returned no result for a decoded stream"))?;
        let text = result.text.trim();
        if !text.is_empty() {
            sink(text);
        }
        Ok(())
    }
}
impl Transcriber {
    pub fn dummy() -> Self {
        let recognizer: sherpa_onnx::OfflineRecognizer = unsafe { std::mem::zeroed() };
        Self {
            recognizer: Mutex::new(recognizer),
        }
    }
}


#[derive(Debug)]
struct ModelFiles {
    encoder: PathBuf,
    decoder: PathBuf,
    joiner: PathBuf,
    tokens: PathBuf,
}

/// Locate the four required files inside a model directory. The int8
/// downloads name them `*.int8.onnx`; accept the float names too.
fn model_files(dir: &Path) -> Result<ModelFiles> {
    ensure!(
        dir.is_dir(),
        "model path '{}' is not a sherpa-onnx model directory — point model_path at e.g. \
         ~/.local/share/steno/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8 \
         (whisper ggml .bin files are no longer supported)",
        dir.display()
    );
    let pick = |stem: &str| -> Result<PathBuf> {
        for name in [
            format!("{stem}.int8.onnx"),
            format!("{stem}.onnx"),
            format!("{stem}.fp16.onnx"),
        ] {
            let p = dir.join(&name);
            if p.is_file() {
                return Ok(p);
            }
        }
        bail!(
            "model directory '{}' is missing {stem}.int8.onnx — re-extract the model archive \
             (see README: Models)",
            dir.display()
        );
    };
    let tokens = dir.join("tokens.txt");
    ensure!(
        tokens.is_file(),
        "model directory '{}' is missing tokens.txt — re-extract the model archive",
        dir.display()
    );
    Ok(ModelFiles {
        encoder: pick("encoder")?,
        decoder: pick("decoder")?,
        joiner: pick("joiner")?,
        tokens,
    })
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .with_context(|| format!("model path '{}' is not valid UTF-8", p.display()))
}

#[cfg(test)]
mod tests {
    //! WHY: model-dir validation is the only user-facing failure logic
    //! left in this module: a whisper ggml file or a half-extracted
    //! archive must fail with a corrective message, not an ONNX error.
    use super::*;
    use std::fs;

    #[test]
    fn ggml_file_is_rejected_with_guidance() {
        let dir = std::env::temp_dir().join("steno-stt-test-ggml");
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("ggml-base.en.bin");
        fs::write(&bin, b"not a dir").unwrap();
        let err = model_files(&bin).unwrap_err().to_string();
        assert!(err.contains("no longer supported"), "got: {err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incomplete_model_dir_names_the_missing_file() {
        let dir = std::env::temp_dir().join("steno-stt-test-partial");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        fs::write(dir.join("encoder.int8.onnx"), b"x").unwrap();
        let err = model_files(&dir).unwrap_err().to_string();
        assert!(err.contains("decoder.int8.onnx"), "got: {err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn int8_and_float_names_both_accepted() {
        let dir = std::env::temp_dir().join("steno-stt-test-float");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        fs::write(dir.join("encoder.onnx"), b"x").unwrap();
        fs::write(dir.join("decoder.onnx"), b"x").unwrap();
        fs::write(dir.join("joiner.onnx"), b"x").unwrap();
        let files = model_files(&dir).unwrap();
        assert!(files.encoder.ends_with("encoder.onnx"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_unknown_provider_before_model_io() {
        // WHY: provider typos must fail with a config hint, not an ONNX /
        // CUDA init error. Use a missing path so we never touch the GPU.
        let missing = std::env::temp_dir().join("steno-stt-missing-model-dir");
        let err = Transcriber::load(&missing, 1, "gpu")
            .err()
            .expect("unknown provider must error")
            .to_string();
        assert!(err.contains("provider"), "got: {err}");
        assert!(err.contains("gpu"), "got: {err}");
        assert!(err.contains("cuda") && err.contains("cpu") && err.contains("metal"), "got: {err}");
    }

    #[test]
    fn load_accepts_cpu_cuda_and_metal_provider_strings() {
        // WHY: signature must accept both allowed providers. Incomplete
        // model dirs fail at model_files — never reaches GPU decode.
        let dir = std::env::temp_dir().join("steno-stt-test-provider-partial");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        for provider in ["cuda", "cpu", "metal"] {
            let err = Transcriber::load(&dir, 1, provider)
                .err()
                .expect("incomplete model must error")
                .to_string();
            assert!(
                err.contains("encoder") || err.contains("decoder") || err.contains("joiner"),
                "provider={provider} should reach model_files: {err}"
            );
            assert!(!err.contains("invalid provider"), "got: {err}");
        }
        fs::remove_dir_all(&dir).ok();
    }
}
