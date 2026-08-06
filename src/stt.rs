//! Speech-to-text via sherpa-onnx (Parakeet TDT) on CUDA. One
//! `Transcriber` per model; the daemon keeps it resident in VRAM for its
//! whole lifetime, so per-utterance decode is a hot GPU call.
//!
//! The model is a DIRECTORY with encoder/decoder/joiner ONNX files plus
//! tokens.txt (e.g. sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8).

use anyhow::{Context, Result, anyhow, bail, ensure};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};
use std::path::{Path, PathBuf};

use crate::dsp::STT_RATE;

pub struct Transcriber {
    recognizer: OfflineRecognizer,
}

impl Transcriber {
    /// Load a sherpa-onnx transducer model directory onto the GPU.
    /// Fails closed: a CUDA build that cannot init the GPU errors here
    /// rather than silently decoding on CPU for the daemon's lifetime.
    pub fn load(model_dir: &Path, n_threads: u32) -> Result<Self> {
        ensure!(
            (1..=(i32::MAX as u32)).contains(&n_threads),
            "invalid n_threads = {n_threads} — set it between 1 and {}",
            i32::MAX
        );
        let files = model_files(model_dir)?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(path_str(&files.encoder)?);
        config.model_config.transducer.decoder = Some(path_str(&files.decoder)?);
        config.model_config.transducer.joiner = Some(path_str(&files.joiner)?);
        config.model_config.tokens = Some(path_str(&files.tokens)?);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.provider = Some("cuda".into());
        config.model_config.num_threads = n_threads as i32;

        let recognizer = OfflineRecognizer::create(&config).ok_or_else(|| {
            anyhow!(
                "sherpa-onnx failed to load the model from {} — check that the CUDA build is \
                 installed (SHERPA_ONNX_LIB_DIR), the GPU is free, and the model files are intact",
                model_dir.display()
            )
        })?;
        Ok(Self { recognizer })
    }

    /// Transcribe 16 kHz mono f32 samples. Parakeet decodes an utterance
    /// in one shot (there are no partial segments), so `sink` is invoked
    /// exactly once with the raw transcript when it is non-empty.
    pub fn transcribe_streaming(
        &self,
        samples: &[f32],
        mut sink: impl FnMut(&str) + 'static,
    ) -> Result<()> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(STT_RATE as i32, samples);
        self.recognizer.decode(&stream);
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
         ~/.local/share/dictate/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8 \
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
    //! left in this module — a whisper ggml file or a half-extracted
    //! archive must fail with a corrective message, not an ONNX error.
    use super::*;
    use std::fs;

    #[test]
    fn ggml_file_is_rejected_with_guidance() {
        let dir = std::env::temp_dir().join("dictate-stt-test-ggml");
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("ggml-base.en.bin");
        fs::write(&bin, b"not a dir").unwrap();
        let err = model_files(&bin).unwrap_err().to_string();
        assert!(err.contains("no longer supported"), "got: {err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incomplete_model_dir_names_the_missing_file() {
        let dir = std::env::temp_dir().join("dictate-stt-test-partial");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        fs::write(dir.join("encoder.int8.onnx"), b"x").unwrap();
        let err = model_files(&dir).unwrap_err().to_string();
        assert!(err.contains("decoder.int8.onnx"), "got: {err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn int8_and_float_names_both_accepted() {
        let dir = std::env::temp_dir().join("dictate-stt-test-float");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        fs::write(dir.join("encoder.onnx"), b"x").unwrap();
        fs::write(dir.join("decoder.onnx"), b"x").unwrap();
        fs::write(dir.join("joiner.onnx"), b"x").unwrap();
        let files = model_files(&dir).unwrap();
        assert!(files.encoder.ends_with("encoder.onnx"));
        fs::remove_dir_all(&dir).ok();
    }
}
