//! End-to-end tests: espeak-ng speech → WAV → dictate binary → formatted text.
//!
//! Requirements (tests skip with a message when missing):
//! - espeak-ng in PATH (speech synthesis for fixtures)
//! - a ggml model at ~/.local/share/dictate/models/*.bin (or DICTATE_TEST_MODEL)
//!
//! Every transformation assertion is backed by two anchors so a test can
//! never pass vacuously:
//! - STT-fidelity anchors on the `--raw` transcript prove whisper actually
//!   heard the spoken command words and content. Without them, a garbled
//!   transcript would "pass" absence assertions (e.g. scratch that)
//!   without the feature ever firing.
//! - Command-word absence in the processed output proves the command was
//!   consumed. whisper inserts its own punctuation around spoken command
//!   words ("bank, comma,"), so asserting the presence of '.' or ','
//!   alone would pass even with the command table disabled.

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

/// SAFETY-CRITICAL: every binary invocation in this suite runs with an
/// explicit EMPTY config. Without this, a user who arms typing in their
/// real ~/.config/dictate/config.toml (type_output = true) would make
/// these tests inject real keystrokes into their live session. An empty
/// config also means unarmed, so no run here can ever reach xdotool.
fn hermetic_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("empty-config.toml");
    if !cfg.exists() {
        std::fs::write(&cfg, "").unwrap();
    }
    cfg
}

fn dictate(args: &[&str], wav: &Path, extra: &[String]) -> Output {
    let cfg = hermetic_config(wav.parent().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dictate"));
    cmd.arg(wav).arg("--config").arg(&cfg);
    for a in args {
        cmd.arg(a);
    }
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("run dictate")
}

/// Run the same WAV through the binary twice: once raw (text pipeline
/// bypassed) and once fully processed. Both outputs must succeed.
/// Returns (raw, processed) stdout, trailing newline (from println!)
/// trimmed so newline assertions see only transcript content.
fn raw_and_processed(model: &Path, wav: &Path, extra: &[String]) -> (String, String) {
    let m = &["--model", model.to_str().unwrap()];
    let raw_out = dictate(&[m[0], m[1], "--raw"], wav, extra);
    assert!(raw_out.status.success(), "raw run: {}", String::from_utf8_lossy(&raw_out.stderr));
    let out = dictate(m, wav, extra);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    (
        String::from_utf8(raw_out.stdout).unwrap().trim_end().to_string(),
        String::from_utf8(out.stdout).unwrap().trim_end().to_string(),
    )
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

/// Assert every STT-fidelity anchor appears in the raw transcript. If one
/// is missing, whisper garbled the speech and the transformation
/// assertions downstream would be vacuous — fail loudly instead.
macro_rules! anchor_raw {
    ($raw_lower:expr, $($needle:expr),+ $(,)?) => {
        $(
            assert!(
                $raw_lower.contains($needle),
                "STT garbled the fixture: raw transcript lacks {:?} — \
                 transformation assertions would be vacuous (raw: {:?})",
                $needle, $raw_lower
            );
        )+
    };
}

/// Assert every spoken command word was consumed by the pipeline. If the
/// command table stopped firing, the words would survive into the output.
macro_rules! commands_consumed {
    ($proc_lower:expr, $text:expr, $($word:expr),+ $(,)?) => {
        $(
            assert!(
                !$proc_lower.contains($word),
                "command {:?} did not fire (its words survive): {:?}",
                $word, $text
            );
        )+
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

    let (raw, text) = raw_and_processed(&model, &wav, &[]);
    let raw_lower = raw.to_lowercase();
    let lower = text.to_lowercase();

    // STT fidelity: whisper must have heard content and command words,
    // otherwise the pipeline assertions below prove nothing.
    anchor_raw!(raw_lower, "this is a test", "quick brown fox", "period", "new line", "question mark");
    // whisper emits a single line; a newline in the output can only come
    // from the `new line` command.
    assert!(!raw.contains('\n'), "raw transcript unexpectedly multi-line: {raw:?}");
    assert!(raw.contains('.'), "sanity: raw transcript has no punctuation at all: {raw:?}");
    // The pipeline must transform, not pass through.
    assert_ne!(raw, text, "pipeline left the transcript untouched: {text:?}");

    // Command transforms (observable regardless of STT wording).
    assert!(text.contains('.'), "period command did not fire: {text:?}");
    assert!(text.contains('\n'), "new line command did not fire: {text:?}");
    assert!(text.contains('?'), "question mark command did not fire: {text:?}");
    // The spoken command words themselves must be gone: whisper adds its
    // own '.' around "period", so symbol presence alone proves nothing.
    commands_consumed!(lower, text, "period", "new line", "question mark");
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

    let extra = &["--dictionary".into(), dict.to_string_lossy().into_owned()];
    let (raw, text) = raw_and_processed(&model, &wav, extra);
    let raw_lower = raw.to_lowercase();
    let lower = text.to_lowercase();

    // STT fidelity: whisper must have heard both clauses and the scratch
    // phrase. In particular "went to the store" must be audible in raw —
    // otherwise "store is gone from the output" passes vacuously even if
    // scratch that is broken.
    anchor_raw!(
        raw_lower,
        "went to the store", "scratch that", "went to the bank", "comma", "main street", "period"
    );
    assert_ne!(raw, text, "pipeline left the transcript untouched: {text:?}");

    // Scratch: the rescinded clause is gone, the kept clause survives.
    assert!(!lower.contains("store"), "scratch that did not delete: {text:?}");
    assert!(!lower.contains("scratch"), "scratch command words leaked: {text:?}");
    assert!(lower.contains("went to the bank"), "kept clause lost: {text:?}");
    // Dictionary: phrase override applied, replacement case exact.
    assert!(text.contains("Main Street"), "dictionary override missing: {text:?}");
    // Case-sensitive: the un-overridden lowercase phrase must be gone
    // (the lowercase haystack would always contain the replacement).
    assert!(!text.contains("main street"), "override did not apply (lowercase phrase survives): {text:?}");
    // Commands: symbols present AND spoken words consumed (whisper emits
    // its own ',' around "comma", so symbol presence alone is vacuous).
    assert!(text.contains(','), "comma command did not fire: {text:?}");
    commands_consumed!(lower, text, "comma", "period");
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

    let extra = &["--dictionary".into(), dict.to_string_lossy().into_owned()];
    let (raw, text) = raw_and_processed(&model, &wav, extra);
    let raw_lower = raw.to_lowercase();
    let lower = text.to_lowercase();

    anchor_raw!(
        raw_lower,
        "open quote", "close quote", "new paragraph", "exclamation mark", "hello world", "boss"
    );
    assert!(!raw.contains("\n\n"), "raw transcript unexpectedly has a blank line: {raw:?}");
    assert_ne!(raw, text, "pipeline left the transcript untouched: {text:?}");

    assert!(text.contains('"'), "quote commands did not fire: {text:?}");
    assert!(text.contains("\n\n"), "new paragraph command did not fire: {text:?}");
    assert!(text.contains('!'), "exclamation command did not fire: {text:?}");
    commands_consumed!(lower, text, "quote", "paragraph", "exclamation");
    // Dictionary name override: applied, and the original word is gone.
    assert!(text.contains("Mukund"), "name override missing: {text:?}");
    assert!(!lower.contains("boss"), "un-overridden word leaked: {text:?}");
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
    let wav = speak(&tmp, "t4.wav", "hello dictate system period");
    // No --model: must resolve from the default model directory.
    let out = dictate(&[], &wav, &[]);
    assert!(
        out.status.success(),
        "default model resolution failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The transcription actually ran: spoken content is in the output.
    let text = String::from_utf8(out.stdout).unwrap();
    let lower = text.to_lowercase();
    assert!(
        lower.contains("hello") && lower.contains("dictate"),
        "transcription empty or garbled: {text:?}"
    );
}

/// `--list-commands` must document every command's spoken form and its
/// result. Drift between the table and the docs is a hard failure: users
/// would say undocumented phrases or expect undocumented effects.
#[test]
fn typing_is_fail_closed_without_config_arming() {
    // SAFETY-CRITICAL: `--type` with an unarmed config must fail BEFORE
    // recording, model load, or any xdotool spawn. This test never types:
    // the guard fires first. No fixture or model needed — the error
    // precedes audio and transcription entirely.
    let tmp = std::env::temp_dir().join("dictate-e2e-blocker");
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg = hermetic_config(&tmp);
    let out = Command::new(env!("CARGO_BIN_EXE_dictate"))
        .arg("--type")
        .arg("--config")
        .arg(&cfg)
        .output()
        .expect("run dictate");
    assert!(!out.status.success(), "--type without arming must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("disarmed"), "error must name the blocker: {err}");
    assert!(err.contains("type_output"), "error must name the arming key: {err}");
    // The guard must fire fast — before whisper (never prints model load)
    // and before the microphone is opened.
    assert!(!err.contains("whisper"), "model loaded before the guard: {err}");
}

/// Real typing end-to-end (keystrokes landing in a window) is
/// intentionally NOT tested here: it must only ever run inside a
/// disposable microVM (e.g. Firecracker), never against a live desktop
/// session. The typing mechanism is covered by unit tests
/// (output::sanitize_for_typing, main::output_mode) and the fail-closed
/// guard above.

#[test]
fn list_commands_documents_every_command() {
    let out = Command::new(env!("CARGO_BIN_EXE_dictate"))
        .arg("--list-commands")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    // One documented line per command in the table.
    assert_eq!(
        text.lines().count(),
        16,
        "--list-commands should print one line per command:\n{text}"
    );
    // Every primary spoken form and its documented alternates.
    for needle in [
        "period", "full stop", "comma", "question mark", "exclamation mark", "colon",
        "semicolon", "ellipsis", "dot dot dot", "open quote", "close quote", "end quote",
        "unquote", "open paren", "close paren", "percent sign", "dollar sign", "new line",
        "new paragraph", "scratch that", "delete that",
    ] {
        assert!(text.contains(needle), "missing {needle} in --list-commands");
    }
    // Every line names its effect.
    for line in text.lines() {
        assert!(line.contains('→'), "doc line lacks an effect arrow: {line:?}");
    }
}
