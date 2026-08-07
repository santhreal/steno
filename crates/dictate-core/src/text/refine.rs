//! Post-format ASR cleanup (offline, rule-based).
//!
//! Runs as the **last** stage of [`super::TextPipeline`] on the already
//! formatted string (verbatim markers are already stripped by format).
//! Rules are deliberately small and case-preserving so dictionary brand
//! replacements that survived formatting keep their casing.
//!
//! Limitation: after markers are stripped, refine cannot tell a brand
//! token from ordinary text. Rules never re-case tokens (duplicate-word
//! collapse keeps the first spelling; phrase maps use fixed literals).
//! Tokens with internal capitals or short all-lowercase brands emitted
//! by format are left alone by case-transform rules — there are none.
//!
//! Full LLM GEC is a future [`RefineBackend`] only; this module stays
//! offline and ships [`RuleRefine`] as the default.

use std::borrow::Cow;

/// Pluggable post-STT refinement. Implementations must be pure and offline.
pub trait RefineBackend: Send + Sync {
    fn refine(&self, text: &str) -> String;
}

/// Identity backend used when `[refine] enabled = false`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullRefine;

impl RefineBackend for NullRefine {
    fn refine(&self, text: &str) -> String {
        text.to_string()
    }
}

/// Default offline rule backend (`backend = "rules"`).
#[derive(Debug, Default, Clone, Copy)]
pub struct RuleRefine;

impl RefineBackend for RuleRefine {
    fn refine(&self, text: &str) -> String {
        rule_refine(text)
    }
}

/// `[refine]` section: enable/disable and backend name.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefineConfig {
    /// When false, the pipeline uses [`NullRefine`].
    pub enabled: bool,
    /// `"rules"` selects [`RuleRefine`]. Unknown names warn and fall back
    /// to rules (fail soft on the cleanup pass, not closed).
    pub backend: String,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "rules".to_string(),
        }
    }
}

impl RefineConfig {
    /// Build the configured backend. Disabled → [`NullRefine`]; unknown
    /// backend name → warn + [`RuleRefine`].
    pub fn make_backend(&self) -> Box<dyn RefineBackend> {
        if !self.enabled {
            return Box::new(NullRefine);
        }
        match self.backend.as_str() {
            "rules" => Box::new(RuleRefine),
            other => {
                log::warn!(
                    "unknown refine backend {other:?}; using \"rules\". \
                     Set backend = \"rules\" or enabled = false"
                );
                Box::new(RuleRefine)
            }
        }
    }
}

/// Tiny table of spaced ASR contractions → solid forms.
/// Matched case-insensitively as whole consecutive words; replacement
/// casing follows the first word of the match when that word was
/// capitalized, otherwise the table literal.
const SPACED_CONTRACTIONS: &[(&[&str], &str)] = &[
    (&["can", "not"], "cannot"),
    (&["will", "not"], "won't"),
    (&["do", "not"], "don't"),
    (&["does", "not"], "doesn't"),
    (&["did", "not"], "didn't"),
    (&["is", "not"], "isn't"),
    (&["are", "not"], "aren't"),
    (&["was", "not"], "wasn't"),
    (&["were", "not"], "weren't"),
    (&["have", "not"], "haven't"),
    (&["has", "not"], "hasn't"),
    (&["had", "not"], "hadn't"),
    (&["would", "not"], "wouldn't"),
    (&["could", "not"], "couldn't"),
    (&["should", "not"], "shouldn't"),
];

/// Frequent ASR garbling → intended text. Keep tiny and documented.
/// Matched case-insensitively as consecutive whole words; replacement is
/// inserted literally (fixed casing from the table).
const COMMON_ASR_FIXES: &[(&[&str], &str)] = &[
    // Repeated filler / stutter patterns ASR often doubles.
    (&["gotta", "gotta"], "gotta"),
    (&["kind", "of", "of"], "kind of"),
    (&["sort", "of", "of"], "sort of"),
    (&["a", "lot", "of", "of"], "a lot of"),
];

fn is_orphan_punct_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    tok.chars().all(is_drop_punct)
}

fn is_drop_punct(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | ':' | '!' | '?' | '%' | '…' | '"' | '\'' | '(' | ')' | '[' | ']' | '{'
            | '}'
    )
}

fn is_closing_punct(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | ':' | '!' | '?' | '%' | '…' | ')' | ']' | '}'
    )
}

/// Apply the full rule set to formatted transcript text.
pub fn rule_refine(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // Preserve paragraph structure: refine each line independently so
    // newlines from "new line" / "new paragraph" survive.
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&rule_refine_line(line));
    }
    out
}

fn rule_refine_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut words: Vec<Cow<'_, str>> = trimmed.split_whitespace().map(Cow::Borrowed).collect();

    apply_phrase_map(&mut words, COMMON_ASR_FIXES, true);
    apply_phrase_map(&mut words, SPACED_CONTRACTIONS, false);
    collapse_duplicate_words(&mut words);
    // Drop leading orphan punct tokens (nothing to attach to). Trailing /
    // mid-stream punct is reattached by strip_space_before_punct.
    while words.first().is_some_and(|w| is_orphan_punct_token(w)) {
        words.remove(0);
    }

    if words.is_empty() {
        return String::new();
    }

    // Punctuation-only residue (e.g. ", . !") → empty.
    if words.iter().all(|w| is_orphan_punct_token(w)) {
        return String::new();
    }

    let joined = words.join(" ");
    strip_space_before_punct(&joined)
}

/// Longest-first phrase replacement over whitespace tokens.
fn apply_phrase_map(words: &mut Vec<Cow<'_, str>>, table: &[(&[&str], &str)], literal_case: bool) {
    if words.is_empty() || table.is_empty() {
        return;
    }
    let mut i = 0;
    while i < words.len() {
        let mut matched: Option<(usize, String)> = None;
        for &(phrase, repl) in table {
            let n = phrase.len();
            if n == 0 || i + n > words.len() {
                continue;
            }
            let ok = phrase
                .iter()
                .zip(words[i..i + n].iter())
                .all(|(p, w)| w.eq_ignore_ascii_case(p));
            if !ok {
                continue;
            }
            if matched.as_ref().is_some_and(|(m, _)| *m >= n) {
                continue;
            }
            let replacement = if literal_case {
                repl.to_string()
            } else {
                match_contraction_case(&words[i], repl)
            };
            matched = Some((n, replacement));
        }
        if let Some((n, replacement)) = matched {
            words.splice(i..i + n, std::iter::once(Cow::Owned(replacement)));
            i += 1;
        } else {
            i += 1;
        }
    }
}

fn match_contraction_case(first: &str, repl: &str) -> String {
    let Some(fc) = first.chars().next() else {
        return repl.to_string();
    };
    if fc.is_uppercase() {
        let mut out = repl.to_string();
        if let Some(r) = out.get_mut(0..1) {
            r.make_ascii_uppercase();
        }
        out
    } else {
        repl.to_string()
    }
}

/// Collapse consecutive duplicate words case-insensitively, keeping the
/// first token's casing.
fn collapse_duplicate_words(words: &mut Vec<Cow<'_, str>>) {
    if words.len() < 2 {
        return;
    }
    let mut out: Vec<Cow<'_, str>> = Vec::with_capacity(words.len());
    for w in words.drain(..) {
        if out
            .last()
            .is_some_and(|prev| prev.eq_ignore_ascii_case(&w))
        {
            continue;
        }
        out.push(w);
    }
    *words = out;
}


/// Remove spaces immediately before closing/sentence punctuation, then
/// collapse any residual multi-space runs.
fn strip_space_before_punct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            if chars.peek().copied().is_some_and(is_closing_punct) {
                continue;
            }
            if out.ends_with(' ') {
                continue;
            }
            out.push(' ');
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    //! WHY: RuleRefine must clean high-value ASR glitches without fighting
    //! format capitalization or inventing case changes on brand-like tokens.

    use super::*;

    #[test]
    fn collapses_consecutive_duplicate_words() {
        assert_eq!(rule_refine("the the cat"), "the cat");
        assert_eq!(rule_refine("Hello hello world"), "Hello world");
        assert_eq!(rule_refine("the The THE cat"), "the cat");
    }

    #[test]
    fn empty_and_punctuation_only_stable() {
        assert_eq!(rule_refine(""), "");
        assert_eq!(rule_refine("   "), "");
        assert_eq!(rule_refine("."), "");
        assert_eq!(rule_refine(" , . ! ? "), "");
        assert_eq!(NullRefine.refine(""), "");
        assert_eq!(NullRefine.refine(" , "), " , ");
    }

    #[test]
    fn null_refine_leaves_text_unchanged() {
        let n = NullRefine;
        assert_eq!(n.refine("the the cat"), "the the cat");
        assert_eq!(n.refine("Hello,  world"), "Hello,  world");
    }

    #[test]
    fn strips_space_before_punct_and_collapses_spaces() {
        assert_eq!(rule_refine("Hello , world"), "Hello, world");
        assert_eq!(rule_refine("Wait  a   minute"), "Wait a minute");
        assert_eq!(rule_refine("Done ."), "Done.");
    }

    #[test]
    fn spaced_contractions() {
        assert_eq!(rule_refine("I can not go"), "I cannot go");
        assert_eq!(rule_refine("we do not know"), "we don't know");
        assert_eq!(rule_refine("Can not stop"), "Cannot stop");
    }

    #[test]
    fn drops_orphan_punctuation_tokens() {
        // Leading orphans have nothing to attach to.
        assert_eq!(rule_refine(". hello"), "hello");
        assert_eq!(rule_refine(", yes"), "yes");
        // Mid/trailing punct reattaches instead of vanishing.
        assert_eq!(rule_refine("hello . world"), "hello. world");
        assert_eq!(rule_refine("yes , no"), "yes, no");
    }

    #[test]
    fn preserves_newlines() {
        assert_eq!(rule_refine("the the cat\nhello hello"), "the cat\nhello");
    }

    #[test]
    fn does_not_recase_brand_like_tokens() {
        assert_eq!(rule_refine("use veyyon today"), "use veyyon today");
        assert_eq!(rule_refine("OpenAI OpenAI rocks"), "OpenAI rocks");
    }

    #[test]
    fn refine_config_disabled_yields_null() {
        let cfg = RefineConfig {
            enabled: false,
            backend: "rules".into(),
        };
        let b = cfg.make_backend();
        assert_eq!(b.refine("the the cat"), "the the cat");
    }

    #[test]
    fn refine_config_default_is_rules() {
        let cfg = RefineConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, "rules");
        let b = cfg.make_backend();
        assert_eq!(b.refine("the the cat"), "the cat");
    }

    #[test]
    fn unknown_backend_falls_back_to_rules() {
        let cfg = RefineConfig {
            enabled: true,
            backend: "llm".into(),
        };
        let b = cfg.make_backend();
        assert_eq!(b.refine("Hello hello"), "Hello");
    }

    #[test]
    fn rule_refine_via_trait() {
        assert_eq!(RuleRefine.refine("the the cat"), "the cat");
    }

    #[test]
    fn preserves_space_after_closing_quotes() {
        assert_eq!(rule_refine("Is \"veyyon\" it?"), "Is \"veyyon\" it?");
    }

    #[test]
    fn refine_config_deserializes_from_toml() {
        let cfg: RefineConfig =
            toml::from_str("enabled = false\nbackend = \"rules\"\n").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, "rules");
        assert_eq!(cfg.make_backend().refine("the the"), "the the");
    }
}
