//! Dictionary: user-defined phrase overrides.
//!
//! Primary source is `[dict.overrides]` in `config.toml` (built via
//! [`Dictionary::from_entries`] / [`Dictionary::from_map`]). A standalone
//! legacy file is still parseable by [`Dictionary::load`] /
//! [`Dictionary::load_overrides`] for one-shot migration:
//! ```toml
//! [overrides]
//! "hypr whisper" = "hyprwhspr"
//! "mukund" = "Mukund"
//! "um" = ""            # empty replacement deletes the phrase
//! ```
//!
//! Matching is case-insensitive, whole-word, longest phrase first.
//! Replacement text is inserted literally (case as written): `apply_marked`
//! wraps each replacement in verbatim markers so the formatter never
//! re-cases it, and [`Dictionary::apply`] strips them for plain-text callers.
//!
//! An override whose phrase collides with a voice command phrase (say
//! "period") is dead: commands run first (see `mod.rs`) and consume the
//! spoken words before the dictionary sees them. Entries with an empty
//! or whitespace-only phrase are dropped at load; they could never match
//! a real token and would otherwise match everywhere without consuming
//! input. Entries whose phrase is or ends in punctuation ("...", "e.g.")
//! silently never match, because the tokenizer splits edge punctuation
//! off words; write such overrides without the punctuation.

use super::commands::{Tok, match_at, tokenize};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// In-memory phrase replacement dictionary.
#[derive(Debug, Default, Clone)]
pub struct Dictionary {
    // Sorted longest-phrase-first so greedy matching is stable.
    entries: Vec<(String, String)>,
}

/// On-disk shape: a required `[overrides]` table of phrase → replacement.
#[derive(Deserialize)]
struct DictFile {
    overrides: HashMap<String, String>,
}

impl Dictionary {
    /// `None` → empty dictionary. A missing explicit file is an error;
    /// a malformed one is an error naming the file.
    ///
    /// Standalone `[overrides]` TOML file entry (legacy dictionary.toml).
    /// Runtime callers build from `Config.dict.overrides` via
    /// [`Self::from_entries`] / [`Self::from_map`]; migration uses this plus
    /// [`Self::load_overrides`].
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        Ok(Self::from_entries(Self::load_overrides(path)?))
    }

    /// Parse a standalone `[overrides]` TOML file (legacy dictionary.toml).
    pub fn load_overrides(path: &Path) -> Result<HashMap<String, String>> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read dictionary file '{}'; fix the path or create the file",
                path.display()
            )
        })?;
        let parsed: DictFile = toml::from_str(&text).with_context(|| {
            format!(
                "invalid dictionary file '{}'; expected an [overrides] table of phrase = \"replacement\" entries",
                path.display()
            )
        })?;
        Ok(parsed.overrides)
    }

    /// Construct from an in-memory `HashMap` (config `[dict.overrides]`).
    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self::from_entries(map)
    }

    /// Also construct from an in-memory table (tests, defaults).
    pub fn from_entries<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut entries: Vec<(String, String)> = entries
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        // Drop phrases with no words (empty or whitespace-only keys,
        // both legal in TOML). A zero-word phrase matches at every word
        // without consuming a token, looping `apply` forever.
        entries.retain(|(k, _)| k.split_whitespace().next().is_some());
        // Longest phrase first (by word count) so greedy matching never
        // lets a short phrase shadow a longer one; tie-break on the
        // phrase text so ordering stays deterministic.
        entries.sort_by(|a, b| {
            b.0.split_whitespace()
                .count()
                .cmp(&a.0.split_whitespace().count())
                .then_with(|| a.0.cmp(&b.0))
        });
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }


    /// Entries as a map (order not significant). Used when Config stores
    /// overrides after a legacy [`Self::load`].
    pub fn to_map(&self) -> HashMap<String, String> {
        self.entries.iter().cloned().collect()
    }

    /// Apply overrides to `input`. Whole-word matching: a phrase never
    /// matches inside a larger word.
    pub fn apply(&self, input: &str) -> String {
        super::format::strip_verbatim(&self.apply_marked(input))
    }

    /// [`apply`](Self::apply) plus a verbatim marker pair around every
    /// inserted replacement, so the formatter can protect the replacement's
    /// case ("veyyon" must not become "Veyyon" at a sentence start).
    /// Callers that do not run the formatter must strip the markers
    /// (`format::strip_verbatim`); `TextPipeline` handles both paths.
    pub(super) fn apply_marked(&self, input: &str) -> String {
        if self.is_empty() {
            return input.to_string();
        }
        // Pre-split and lowercase every phrase once, longest first.
        let phrases: Vec<(Vec<&str>, &str)> = self
            .entries
            .iter()
            .map(|(p, r)| (p.split_whitespace().collect(), r.as_str()))
            .collect();

        let toks = tokenize(input);
        let mut out = String::with_capacity(input.len());
        let mut first = true;
        let emit = |out: &mut String, text: &str, first: &mut bool| {
            // An empty replacement deletes the phrase; emitting nothing
            // and joining survivors with single spaces collapses the
            // gap it leaves.
            if !text.is_empty() {
                if !*first {
                    out.push(' ');
                }
                out.push_str(text);
                *first = false;
            }
        };
        let mut i = 0;
        while i < toks.len() {
            match toks[i] {
                Tok::Newline => {
                    out.push('\n');
                    first = true;
                    i += 1;
                }
                Tok::Punct(c) => {
                    let mut buf = [0u8; 4];
                    let c = c.encode_utf8(&mut buf).to_owned();
                    emit(&mut out, &c, &mut first);
                    i += 1;
                }
                Tok::Word(w) => {
                    let mut found: Option<(&str, usize)> = None;
                    for (pw, r) in &phrases {
                        if let Some(end) = match_at(&toks, i, pw) {
                            found = Some((r, end));
                            break;
                        }
                    }
                    match found {
                        // Unlike commands, a replacement never absorbs
                        // punctuation after the phrase. Mark it verbatim so
                        // the formatter leaves its case alone.
                        Some((r, end)) => {
                            if !r.is_empty() {
                                if !first {
                                    out.push(' ');
                                }
                                out.push(super::VERBATIM_START);
                                out.push_str(r);
                                out.push(super::VERBATIM_END);
                                first = false;
                            }
                            i = end;
                        }
                        None => {
                            emit(&mut out, w, &mut first);
                            i += 1;
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! WHY: User dictionary phrase overrides, TOML configuration loading, and multi-word term
    //! replacements must preserve text formatting while accurately substituting target phrases.
    use super::*;
    use std::io::Write;

    /// Write `contents` to a unique temp file and return its path.
    fn temp_toml(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "steno-dict-test-{}-{}.toml",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn load_none_is_empty() {
        let d = Dictionary::load(None).unwrap();
        assert!(d.is_empty());
        // Empty dictionary returns input byte-for-byte.
        assert_eq!(d.apply("leave  me\nalone"), "leave  me\nalone");
    }

    #[test]
    fn load_missing_file_errors_naming_path() {
        let path = std::env::temp_dir().join("steno-dict-test-definitely-missing.toml");
        let err = Dictionary::load(Some(&path)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(path.to_str().unwrap()),
            "error must name path: {msg}"
        );
    }

    #[test]
    fn load_valid_file() {
        let path = temp_toml(
            "valid",
            "[overrides]\n\"hypr whisper\" = \"hyprwhspr\"\n\"mukund\" = \"Mukund\"\n\"um\" = \"\"\n",
        );
        let d = Dictionary::load(Some(&path)).unwrap();
        assert!(!d.is_empty());
        assert_eq!(d.apply("i use hypr whisper daily"), "i use hyprwhspr daily");
        assert_eq!(d.apply("say mukund said so"), "say Mukund said so");
        assert_eq!(d.apply("well um okay"), "well okay");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_malformed_toml_errors_naming_file() {
        let path = temp_toml("malformed", "[overrides\nnot toml");
        let err = Dictionary::load(Some(&path)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(path.to_str().unwrap()),
            "error must name file: {msg}"
        );
        // The toml parser's line/column must survive the added context so
        // the user can find the broken entry.
        assert!(msg.contains("line"), "error must locate the line: {msg}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_missing_overrides_table_errors() {
        let path = temp_toml("no-overrides", "[other]\nx = 1\n");
        let err = Dictionary::load(Some(&path)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("overrides"),
            "error must mention overrides: {msg}"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn phrase_override_applies() {
        let d = Dictionary::from_entries([("new york", "New York")]);
        assert_eq!(d.apply("i live in new york"), "i live in New York");
    }

    #[test]
    fn empty_replacement_deletes_and_collapses_spaces() {
        let d = Dictionary::from_entries([("um", "")]);
        assert_eq!(d.apply("well um okay"), "well okay");
        // Deletion at the edges leaves no stray spaces.
        assert_eq!(d.apply("um okay"), "okay");
        assert_eq!(d.apply("okay um"), "okay");
    }

    #[test]
    fn longest_phrase_wins_over_overlapping_shorter() {
        let d = Dictionary::from_entries([("new york", "NY"), ("new york city", "NYC")]);
        assert_eq!(d.apply("new york city"), "NYC");
        assert_eq!(d.apply("new york state"), "NY state");
        assert_eq!(
            d.apply("i love new york city and new york"),
            "i love NYC and NY"
        );
    }

    #[test]
    fn matching_is_case_insensitive_replacement_case_preserved() {
        let d = Dictionary::from_entries([("mukund", "Mukund")]);
        assert_eq!(d.apply("MUKUND spoke"), "Mukund spoke");
        assert_eq!(d.apply("Mukund spoke"), "Mukund spoke");
    }

    #[test]
    fn matching_is_whole_word() {
        let d = Dictionary::from_entries([("cat", "dog")]);
        // "cat" must not rewrite inside "catch" or "bobcat".
        assert_eq!(d.apply("catch the cat"), "catch the dog");
        assert_eq!(d.apply("bobcat"), "bobcat");
    }

    #[test]
    fn phrases_do_not_span_newlines_and_newlines_survive() {
        let d = Dictionary::from_entries([("new york", "NY")]);
        assert_eq!(d.apply("hello\nnew york"), "hello\nNY");
        assert_eq!(d.apply("new\n\nyork"), "new\n\nyork");
    }

    #[test]
    fn from_entries_is_empty_reflects_table() {
        assert!(Dictionary::from_entries(Vec::<(String, String)>::new()).is_empty());
        assert!(!Dictionary::from_entries([("a", "b")]).is_empty());
    }

    /// Regression: empty and whitespace-only keys (both legal TOML) used
    /// to make `apply` loop forever: a zero-word phrase matches at every
    /// word position without consuming a token. They are dropped at
    /// construction, so `is_empty` reflects the usable table.
    #[test]
    fn empty_and_whitespace_keys_are_dropped() {
        assert!(Dictionary::from_entries([("", "x")]).is_empty());
        assert!(Dictionary::from_entries([("  \t ", "x")]).is_empty());
        let d = Dictionary::from_entries([("", "boom"), ("a", "b")]);
        assert_eq!(d.apply("a cat"), "b cat");
    }

    /// A TOML file with an empty quoted key loads, and the entry is
    /// ignored rather than hanging the first `apply` call.
    #[test]
    fn load_toml_with_empty_key_does_not_hang() {
        let path = temp_toml("empty-key", "[overrides]\n\"\" = \"boom\"\n\"a\" = \"b\"\n");
        let d = Dictionary::load(Some(&path)).unwrap();
        assert_eq!(d.apply("a cat"), "b cat");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn single_character_and_three_way_overlap() {
        let d = Dictionary::from_entries([("a", "A!")]);
        assert_eq!(d.apply("a cat sat"), "A! cat sat");
        let d =
            Dictionary::from_entries([("new york", "NY"), ("new york city", "NYC"), ("york", "Y")]);
        assert_eq!(d.apply("a trip to new york city"), "a trip to NYC");
        assert_eq!(d.apply("new york state"), "NY state");
        // "york" fires only where no longer phrase starts earlier.
        assert_eq!(d.apply("yorkshire york"), "yorkshire Y");
    }

    /// Commands run before the dictionary (fixed pipeline order), so an
    /// override whose phrase collides with a command phrase never sees
    /// the spoken words: the command consumes them first. Pins the
    /// documented winner via the real pipeline.
    #[test]
    fn command_phrases_win_over_colliding_dictionary_entries() {
        let dict = Dictionary::from_entries([
            ("period", "PERIOD"),
            ("scratch that", "KEPT"),
            ("new york", "NY"),
        ]);
        let pipe = crate::text::TextPipeline::new(crate::text::TextConfig::default(), dict);
        // "period" fired as a command; the override is dead.
        assert_eq!(
            pipe.process_stream("wait period", Default::default()).0,
            "Wait."
        );
        // "scratch that" fired as a command, not replaced with KEPT.
        assert_eq!(
            pipe.process_stream("oops scratch that", Default::default())
                .0,
            ""
        );
        // Non-colliding overrides still apply to command output.
        assert_eq!(
            pipe.process_stream("new york period", Default::default()).0,
            "NY."
        );
    }

    #[test]
    fn transcript_punctuation_does_not_block_matching() {
        let d = Dictionary::from_entries([("new york", "New York")]);
        // Trailing punctuation is kept, not absorbed by the replacement.
        assert_eq!(d.apply("i love new york."), "i love New York .");
        let d = Dictionary::from_entries([("um", "")]);
        assert_eq!(d.apply("well, um, okay"), "well , , okay");
    }

    /// `apply_marked` wraps every inserted replacement in the verbatim
    /// marker pair; `apply` strips them. This marker contract is what
    /// lets the formatter protect replacement case.
    #[test]
    fn apply_marked_wraps_replacements_and_apply_strips() {
        let d = Dictionary::from_entries([("vayon", "veyyon")]);
        assert_eq!(
            d.apply_marked("say vayon"),
            "say \u{E000}veyyon\u{E001}"
        );
        assert_eq!(d.apply("say vayon"), "say veyyon");
        // Non-replaced text and deletions carry no markers.
        let d = Dictionary::from_entries([("um", "")]);
        assert_eq!(d.apply_marked("well um okay"), "well okay");
    }

    /// Regression ("Vayon" bug): the recognizer hears the brand
    /// "veyyon" as "Vayon". An entry for the misspelling must match case-insensitively
    /// and insert the brand's exact lowercase form, even at a sentence
    /// start, where the formatter used to re-capitalize the replacement
    /// to "Veyyon".
    #[test]
    fn misspelling_override_keeps_lowercase_brand_at_sentence_start() {
        let dict = Dictionary::from_entries([("vayon", "veyyon")]);
        let pipe = crate::text::TextPipeline::new(crate::text::TextConfig::default(), dict);
        assert_eq!(
            pipe.process_stream("vayon is great", Default::default()).0,
            "veyyon is great"
        );
        // Whisper's own capitalization of the misspelling also matches.
        assert_eq!(
            pipe.process_stream("Vayon is great", Default::default()).0,
            "veyyon is great"
        );
        // Sentence capitalization still applies to ordinary words, and a
        // mid-sentence replacement is untouched either way.
        assert_eq!(
            pipe.process_stream("i like vayon", Default::default()).0,
            "I like veyyon"
        );
        // After a voice command's punctuation, same protection.
        assert_eq!(
            pipe.process_stream("vayon period", Default::default()).0,
            "veyyon."
        );
    }

    /// The misspelling must also match with transcript punctuation glued
    /// to it ("Vayon," / "Vayon."): the tokenizer splits edge
    /// punctuation off before matching.
    #[test]
    fn misspelling_override_matches_next_to_punctuation() {
        let dict = Dictionary::from_entries([("vayon", "veyyon")]);
        // Dict level: punctuation is split off and survives the rewrite.
        assert_eq!(dict.apply("Vayon, really."), "veyyon , really .");
        // Pipeline level: the formatter re-attaches it.
        let pipe = crate::text::TextPipeline::new(crate::text::TextConfig::default(), dict);
        assert_eq!(
            pipe.process_stream("Vayon, really.", Default::default()).0,
            "veyyon, really."
        );
        assert_eq!(
            pipe.process_stream("is \"vayon\" it?", Default::default()).0,
            "Is \"veyyon\" it?"
        );
    }

    /// Longest phrase first: the two-word misspelling beats its one-word
    /// prefix wherever both could start.
    #[test]
    fn longest_misspelling_phrase_wins() {
        let d = Dictionary::from_entries([("vay", "V"), ("vay on", "veyyon")]);
        assert_eq!(d.apply("vay on fire"), "veyyon fire");
        assert_eq!(d.apply("Vay on fire"), "veyyon fire");
        assert_eq!(d.apply("vay fire"), "V fire");
    }

    /// With formatting disabled the pipeline must still strip the
    /// verbatim markers; they are an internal contract, never output.
    #[test]
    fn verbatim_markers_never_reach_output_when_formatting_disabled() {
        let dict = Dictionary::from_entries([("vayon", "veyyon")]);
        let cfg = crate::text::TextConfig {
            commands: false,
            format: false,
        };
        let pipe = crate::text::TextPipeline::new(cfg, dict);
        assert_eq!(
            pipe.process_stream("vayon is great", Default::default()).0,
            "veyyon is great"
        );
    }
}
