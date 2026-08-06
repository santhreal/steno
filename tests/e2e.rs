//! End-to-end tests: espeak-ng speech → WAV → dictate binary → formatted text.
//!
//! Requirements (tests skip with a message when missing):
//! - espeak-ng in PATH (speech synthesis for fixtures)
//! - a ggml model at ~/.local/share/dictate/models/*.bin (or DICTATE_TEST_MODEL)
//!
//! Assertions target the pipeline's observable contract (commands firing,
//! dictionary overrides, formatting), with tolerant matching on the exact
//! transcription: STT wording may vary, transformations must not.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn test_model() -> Option<PathBuf> {
    if let Ok(m) = std::env::var("DICTATE_TEST_MODEL") {
        return Some(PathBuf::from(m));
    }
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".local/share/dictate/models");
    let mut bins: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "bin"))
        .collect();
    // Prefer the largest model: the best available STT quality.
    bins.sort_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
    bins.into_iter().next_back()
}

fn speak(dir: &Path, name: &str, text: &str) -> PathBuf {
    let wav = dir.join(name);
    let status = Command::new("espeak-ng")
        .args(["-s", "110", "-w"])
        .arg(&wav)
        .arg(text)
        .status()
        .expect("spawn espeak-ng");
    assert!(status.success());
    wav
}

fn dictate(args: &[&str], wav: &Path, extra: &[String]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dictate"));
    cmd.arg(wav);
    for a in args {
        cmd.arg(a);
    }
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("run dictate")
}

/// Skipped tests print why; a requirement regression is visible, not silent.
macro_rules! require {
    ($cond:expr, $why:expr) => {
        if !$cond {
            eprintln!("SKIP: {}", $why);
            return;
        }
    };
}

#[test]
fn e2e_commands_and_formatting() {
    require!(have("espeak-ng"), "espeak-ng not installed");
    let Some(model) = test_model() else {
        eprintln!("SKIP: no model in ~/.local/share/dictate/models");
        return;
    };
    let tmp = std::env::temp_dir().join("dictate-e2e-1");
    std::fs::create_dir_all(&tmp).unwrap();
    let wav = speak(
        &tmp,
        "t1.wav",
        "this is a test of the dictate system period new line the quick brown fox jumps over the lazy dog question mark",
    );

    let out = dictate(&["--model", model.to_str().unwrap()], &wav, &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let lower = text.to_lowercase();
    // Command transforms (observable regardless of STT wording).
    assert!(text.contains('.'), "period command did not fire: {text:?}");
    assert!(text.contains('\n'), "new line command did not fire: {text:?}");
    assert!(text.contains('?'), "question mark command did not fire: {text:?}");
    // Content survived the pipeline.
    assert!(lower.contains("quick brown fox"), "content lost: {text:?}");
    // Formatter: first letter capitalized, no space before punctuation.
    assert!(text.chars().next().unwrap().is_uppercase(), "no leading capital: {text:?}");
    assert!(!text.contains(" .") && !text.contains(" ?"), "space before punctuation: {text:?}");
}

#[test]
fn e2e_scratch_and_dictionary() {
    require!(have("espeak-ng"), "espeak-ng not installed");
    let Some(model) = test_model() else {
        eprintln!("SKIP: no model in ~/.local/share/dictate/models");
        return;
    };
    let tmp = std::env::temp_dir().join("dictate-e2e-2");
    std::fs::create_dir_all(&tmp).unwrap();
    let dict = tmp.join("dict.toml");
    std::fs::write(&dict, "[overrides]\n\"main street\" = \"Main Street\"\n\"um\" = \"\"\n").unwrap();
    let wav = speak(
        &tmp,
        "t2.wav",
        "um i went to the store scratch that i went to the bank comma main street period",
    );

    let out = dictate(
        &["--model", model.to_str().unwrap()],
        &wav,
        &["--dictionary".into(), dict.to_string_lossy().into_owned()],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let lower = text.to_lowercase();
    assert!(!lower.contains("went to the store"), "scratch that did not delete: {text:?}");
    assert!(lower.contains("went to the bank"), "kept clause lost: {text:?}");
    assert!(text.contains("Main Street"), "dictionary override missing: {text:?}");
    assert!(text.contains(','), "comma command did not fire: {text:?}");
    assert!(!text.contains(",,"), "duplicate punctuation leaked: {text:?}");
}

#[test]
fn e2e_quotes_paragraph_and_names() {
    require!(have("espeak-ng"), "espeak-ng not installed");
    let Some(model) = test_model() else {
        eprintln!("SKIP: no model in ~/.local/share/dictate/models");
        return;
    };
    let tmp = std::env::temp_dir().join("dictate-e2e-3");
    std::fs::create_dir_all(&tmp).unwrap();
    let wav = speak(
        &tmp,
        "t3.wav",
        "open quote hello world close quote new paragraph this is the boss speaking exclamation mark",
    );
    let dict = tmp.join("dict.toml");
    std::fs::write(&dict, "[overrides]\n\"boss\" = \"Mukund\"\n").unwrap();

    let out = dictate(
        &["--model", model.to_str().unwrap()],
        &wav,
        &["--dictionary".into(), dict.to_string_lossy().into_owned()],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains('"'), "quote commands did not fire: {text:?}");
    assert!(text.contains("\n\n"), "new paragraph command did not fire: {text:?}");
    assert!(text.contains('!'), "exclamation command did not fire: {text:?}");
    assert!(text.contains("Mukund"), "name override missing: {text:?}");
}

#[test]
fn e2e_model_resolution_from_default_dir() {
    let Some(_) = test_model() else {
        eprintln!("SKIP: no model in ~/.local/share/dictate/models");
        return;
    };
    require!(have("espeak-ng"), "espeak-ng not installed");
    let tmp = std::env::temp_dir().join("dictate-e2e-4");
    std::fs::create_dir_all(&tmp).unwrap();
    let wav = speak(&tmp, "t4.wav", "hello dictate period");
    // No --model: must resolve from the default model directory.
    let out = dictate(&[], &wav, &[]);
    assert!(
        out.status.success(),
        "default model resolution failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn list_commands_documents_every_command() {
    let out = Command::new(env!("CARGO_BIN_EXE_dictate"))
        .arg("--list-commands")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in ["period", "scratch that", "new paragraph", "question mark"] {
        assert!(text.contains(needle), "missing {needle} in --list-commands");
    }
}
