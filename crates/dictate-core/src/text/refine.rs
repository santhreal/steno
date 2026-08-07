//! Post-format ASR cleanup (offline, rule-based).
//!
//! Runs as the **last** stage of [`super::TextPipeline`] on the already
//! formatted string (verbatim markers are already stripped by format).
//! Rules are deliberately small and case-preserving so dictionary brand
//! replacements that survived formatting keep their casing.
//!
//! Limitation: after markers are stripped, refine cannot tell a brand
//! token from ordinary text. Rules never re-case tokens (duplicate-word
//! collapse keeps the first spelling; phrase maps follow the first
//! matched token's capitalization when the replacement is not a forced
//! literal). Tokens with internal capitals or short all-lowercase brands
//! emitted by format are left alone by case-transform rules — there are
//! none that rewrite token case beyond first-letter carry for phrase hits.
//!
//! ## Honest limits
//!
//! [`RuleRefine`] only fixes high-precision, local ASR/grammar glitches
//! (spaced contractions, a handful of phrase maps, a/an before clear
//! vowel/consonant starts, duplicate words/short clauses, one leading
//! filler). It **cannot** repair severe STT garble such as acoustic
//! hallucinations (`"chromax"` → intended grammar/word). Those belong to
//! the dictionary (known phrases) and a better acoustic model. Real
//! grammatical error correction (GEC) is a future [`RefineBackend`] plug
//! only; this module stays pure offline rules with no LLM or LanguageTool
//! dependency.
//!
//! Full LLM GEC is therefore a future [`RefineBackend`] only; this module
//! ships [`RuleRefine`] as the default.

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
/// Matched case-insensitively as consecutive whole words; replacement
/// casing follows the first matched token (not a forced lowercase literal).
const COMMON_ASR_FIXES: &[(&[&str], &str)] = &[
    // Repeated filler / stutter patterns ASR often doubles.
    (&["gotta", "gotta"], "gotta"),
    (&["kind", "of", "of"], "kind of"),
    (&["sort", "of", "of"], "sort of"),
    (&["a", "lot", "of", "of"], "a lot of"),
    // Modal "of" → "have" (spoken "should've" misheard as "should of").
    (&["should", "of"], "should have"),
    (&["could", "of"], "could have"),
    (&["would", "of"], "would have"),
    (&["might", "of"], "might have"),
    (&["must", "of"], "must have"),
    // Mixed duplicated articles (identical pairs already collapse via
    // `collapse_duplicate_words`, including "i i").
    (&["the", "a"], "the"),
    (&["a", "the"], "the"),
    (&["the", "an"], "the"),
    (&["an", "the"], "the"),
    (&["a", "an"], "an"),
    (&["an", "a"], "an"),
    // Homophone their/there — only before is/are (high confidence).
    (&["their", "is"], "there is"),
    (&["their", "are"], "there are"),
    // Stuttered existential.
    (&["there", "is", "is"], "there is"),
    (&["there's", "is"], "there's"),
    (&["there", "are", "are"], "there are"),
];

/// Tiny high-precision subject–verb phrase maps. Not a grammar engine:
/// only fixed spoken pairs that ASR produces with high confidence.
const SUBJECT_VERB_FIXES: &[(&[&str], &str)] = &[
    (&["he", "don't"], "he doesn't"),
    (&["she", "don't"], "she doesn't"),
    (&["it", "don't"], "it doesn't"),
    (&["i", "is"], "i am"),
    (&["you", "is"], "you are"),
    (&["we", "is"], "we are"),
    (&["they", "is"], "they are"),
    (&["he", "are"], "he is"),
    (&["she", "are"], "she is"),
    (&["it", "are"], "it is"),
    (&["i", "are"], "i am"),
];

/// Leading utterance fillers stripped once. `um`/`uh` stay with the
/// dictionary — refine does not delete them mid-stream or compete with
/// `[overrides] "um" = ""`.
const LEADING_FILLERS: &[&str] = &[
    "well", "so", "okay", "ok", "alright", "anyway", "basically", "like",
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

    apply_phrase_map(&mut words, COMMON_ASR_FIXES, false);
    apply_phrase_map(&mut words, SUBJECT_VERB_FIXES, false);
    fix_indefinite_article(&mut words);
    apply_phrase_map(&mut words, SPACED_CONTRACTIONS, false);
    collapse_duplicate_words(&mut words);
    collapse_repeated_short_clauses(&mut words);
    strip_leading_filler_once(&mut words);
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

/// Cheap, safe a/an repair before a following alphabetic word.
///
/// Only ASCII vowel/consonant starts. Skips `u…` and `one`/`once` (a-one
/// is correct) and skips all `h…` targets for `an→a` (hour/honest).
/// Not a phoneme model — ambiguous cases are left alone.
fn fix_indefinite_article(words: &mut [Cow<'_, str>]) {
    if words.len() < 2 {
        return;
    }
    let mut i = 0;
    while i + 1 < words.len() {
        let article = words[i].as_ref();
        let next = words[i + 1].as_ref();
        if let Some(fixed) = a_an_fix(article, next) {
            words[i] = Cow::Owned(fixed);
        }
        i += 1;
    }
}

fn a_an_fix(article: &str, next: &str) -> Option<String> {
    let next_alpha: String = next
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if next_alpha.is_empty() {
        return None;
    }
    let first = next_alpha.chars().next()?.to_ascii_lowercase();
    let next_lower = next_alpha.to_ascii_lowercase();

    if article.eq_ignore_ascii_case("a") {
        // Vowel start, excluding u- (university) and one/once.
        if matches!(first, 'a' | 'e' | 'i' | 'o')
            && next_lower != "one"
            && next_lower != "once"
        {
            return Some(match_contraction_case(article, "an"));
        }
        return None;
    }
    if article.eq_ignore_ascii_case("an") {
        // Consonant start; skip h- (hour/honest) and vowels entirely.
        if first.is_ascii_alphabetic() && !matches!(first, 'a' | 'e' | 'i' | 'o' | 'u' | 'h') {
            return Some(match_contraction_case(article, "a"));
        }
        return None;
    }
    None
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

/// Collapse identical consecutive short clauses (2..=4 words), keeping
/// the first copy's casing. Single-word repeats are already handled by
/// [`collapse_duplicate_words`].
fn collapse_repeated_short_clauses(words: &mut Vec<Cow<'_, str>>) {
    if words.len() < 4 {
        return;
    }
    let mut i = 0;
    while i < words.len() {
        let mut collapsed = false;
        for len in (2..=4).rev() {
            if i + 2 * len > words.len() {
                continue;
            }
            let same = (0..len).all(|k| words[i + k].eq_ignore_ascii_case(&words[i + len + k]));
            if same {
                words.drain(i + len..i + 2 * len);
                collapsed = true;
                break;
            }
        }
        if !collapsed {
            i += 1;
        }
    }
}

/// Strip at most one leading filler word when content remains after it.
/// Does not touch `um`/`uh` (dictionary-owned).
fn strip_leading_filler_once(words: &mut Vec<Cow<'_, str>>) {
    if words.len() < 2 {
        return;
    }
    if words
        .first()
        .is_some_and(|w| LEADING_FILLERS.iter().any(|f| w.eq_ignore_ascii_case(f)))
    {
        words.remove(0);
        while words.first().is_some_and(|w| is_orphan_punct_token(w)) {
            words.remove(0);
        }
    }
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
    //! Each new rule has a positive mutation assert and a near-miss that
    //! must stay unchanged (proves the rule is narrow, not a grammar engine).

    use super::*;

    #[test]
    fn collapses_consecutive_duplicate_words() {
        assert_eq!(rule_refine("the the cat"), "the cat");
        assert_eq!(rule_refine("Hello hello world"), "Hello world");
        assert_eq!(rule_refine("the The THE cat"), "the cat");
        // "i i" is the same mechanism — no special case required.
        assert_eq!(rule_refine("i i think"), "i think");
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

    #[test]
    fn modal_of_becomes_have() {
        assert_eq!(rule_refine("I should of gone"), "I should have gone");
        assert_eq!(rule_refine("Could of been"), "Could have been");
        assert_eq!(rule_refine("we would of tried"), "we would have tried");
        assert_eq!(rule_refine("they might of left"), "they might have left");
        assert_eq!(rule_refine("you must of known"), "you must have known");
        // Near-miss: real "of" after a non-modal stays.
        assert_eq!(rule_refine("think of leaving"), "think of leaving");
        assert_eq!(rule_refine("a pair of shoes"), "a pair of shoes");
    }

    #[test]
    fn mixed_duplicate_articles() {
        assert_eq!(rule_refine("the a cat"), "the cat");
        assert_eq!(rule_refine("a the cat"), "the cat");
        assert_eq!(rule_refine("an a apple"), "an apple");
        assert_eq!(rule_refine("a an idea"), "an idea");
        assert_eq!(rule_refine("the an end"), "the end");
        // Near-miss: legitimate adjacent determiners across real words.
        assert_eq!(rule_refine("give the award"), "give the award");
    }

    #[test]
    fn their_is_are_high_confidence_only() {
        assert_eq!(rule_refine("their is a bug"), "there is a bug");
        assert_eq!(rule_refine("Their are options"), "There are options");
        assert_eq!(rule_refine("there is is a way"), "there is a way");
        assert_eq!(rule_refine("there's is time"), "there's time");
        // Near-miss: possessive "their" before a noun must not flip.
        assert_eq!(rule_refine("their cat sat"), "their cat sat");
        assert_eq!(rule_refine("see their idea"), "see their idea");
    }

    #[test]
    fn subject_verb_tiny_phrase_maps() {
        assert_eq!(rule_refine("he don't know"), "he doesn't know");
        assert_eq!(rule_refine("She don't care"), "She doesn't care");
        assert_eq!(rule_refine("it don't work"), "it doesn't work");
        assert_eq!(rule_refine("they is ready"), "they are ready");
        assert_eq!(rule_refine("I is here"), "I am here");
        assert_eq!(rule_refine("you is late"), "you are late");
        assert_eq!(rule_refine("he are gone"), "he is gone");
        // Near-miss: do not invent agreement outside the tiny map.
        assert_eq!(rule_refine("the cats is ready"), "the cats is ready");
        assert_eq!(rule_refine("data are clear"), "data are clear");
    }

    #[test]
    fn a_an_cheap_safe_vowel_consonant() {
        assert_eq!(rule_refine("a apple"), "an apple");
        assert_eq!(rule_refine("A orange"), "An orange");
        assert_eq!(rule_refine("an book"), "a book");
        assert_eq!(rule_refine("An dog"), "A dog");
        // Near-misses left alone (not a phoneme model).
        assert_eq!(rule_refine("a university"), "a university");
        assert_eq!(rule_refine("a one"), "a one");
        assert_eq!(rule_refine("an hour"), "an hour");
        assert_eq!(rule_refine("a cat"), "a cat");
    }

    #[test]
    fn collapses_repeated_short_clauses() {
        assert_eq!(
            rule_refine("going home going home now"),
            "going home now"
        );
        assert_eq!(rule_refine("I think I think so"), "I think so");
        assert_eq!(
            rule_refine("see the cat see the cat"),
            "see the cat"
        );
        // Triple consecutive short clause collapses to one.
        assert_eq!(
            rule_refine("I mean I mean I mean yes"),
            "I mean yes"
        );
        // Near-miss: overlapping but non-identical spans stay.
        assert_eq!(
            rule_refine("going home going back"),
            "going home going back"
        );
    }

    #[test]
    fn strips_one_leading_filler_not_um_uh() {
        assert_eq!(rule_refine("well I agree"), "I agree");
        assert_eq!(rule_refine("So we start"), "we start");
        assert_eq!(rule_refine("okay , let's go"), "let's go");
        assert_eq!(rule_refine("basically this works"), "this works");
        // Only once — second filler remains.
        assert_eq!(rule_refine("well so maybe"), "so maybe");
        // um/uh: do not fight the dictionary; leave them alone here.
        assert_eq!(rule_refine("um I agree"), "um I agree");
        assert_eq!(rule_refine("uh maybe later"), "uh maybe later");
        // Near-miss: mid-stream filler is not stripped.
        assert_eq!(rule_refine("I well know"), "I well know");
        // Lone filler must not wipe the line.
        assert_eq!(rule_refine("well"), "well");
    }

    #[test]
    fn common_asr_stutter_and_severe_garble_untouched() {
        assert_eq!(rule_refine("gotta gotta go"), "gotta go");
        assert_eq!(rule_refine("kind of of weird"), "kind of weird");
        // Severe acoustic garble is out of scope for RuleRefine.
        assert_eq!(rule_refine("chromax grammar"), "chromax grammar");
        assert_eq!(rule_refine("open the chromax"), "open the chromax");
    }
}
