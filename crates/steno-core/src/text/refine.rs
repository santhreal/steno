//! Post-STT ASR cleanup (offline, rule-based).
//!
//! Runs as the **second** stage of [`super::TextPipeline`] (after voice
//! commands, before formatting). Rules are deliberately small and
//! case-preserving so dictionary brand replacements keep their casing.
//!
//! Limitation: refine cannot tell a brand token from ordinary text.
//! Rules never re-case tokens (duplicate-word collapse keeps the first
//! spelling; phrase maps follow the first matched token's capitalization
//! when the replacement is not a forced literal). Tokens with internal
//! capitals or short all-lowercase brands are left alone by case-transform
//! rules: there are none that rewrite token case beyond first-letter carry
//! for phrase hits.
//!
//! ## Honest limits
//!
//! [`RuleRefine`] only fixes high-precision, local ASR/grammar glitches:
//! spaced / split contractions, common ASR phrase maps (homophones with
//! tight context, doubled prepositions, frequent mishears), a small
//! subject-verb map, a/an before clear vowel/consonant starts (plus a few
//! silent-h / yoo-u / x- edges), duplicate words/short clauses, one
//! leading filler, and optional trailing discourse fillers. It **cannot**
//! repair severe STT garble such as acoustic hallucinations
//! (`"chromax"` → intended grammar/word). Those belong to the dictionary
//! (known phrases) and a better acoustic model. Real grammatical error
//! correction (GEC) is a future [`RefineBackend`] plug only; this module
//! ships [`RuleRefine`] as the default with pure offline rules and no LLM or
//! LanguageTool dependency.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use super::Dictionary;
#[cfg(feature = "llm")]
use super::llm_refine::LlmRefine;

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
#[derive(Debug, Default, Clone)]
pub struct RuleRefine {
    pub dictionary: Dictionary,
}

impl RuleRefine {
    pub fn new(dictionary: Dictionary) -> Self {
        Self { dictionary }
    }

    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self {
            dictionary: Dictionary::from_map(map),
        }
    }
}

impl RefineBackend for RuleRefine {
    fn refine(&self, text: &str) -> String {
        rule_refine_with_dict(text, &self.dictionary)
    }
}

/// `[refine]` section: enable/disable, backend name, and dictionary overrides.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefineConfig {
    /// When false, the pipeline uses [`NullRefine`].
    pub enabled: bool,
    /// `"rules"` selects [`RuleRefine`]. `"llm"` selects [`LlmRefine`]
    /// (requires the `llm` cargo feature). Unknown names warn and fall
    /// back to rules.
    pub backend: String,
    /// Custom dictionary/vocabulary overrides (phrase -> replacement).
    #[serde(alias = "overrides")]
    pub dictionary: HashMap<String, String>,
    /// LLM backend configuration (`[refine.llm]`). Used when
    /// `backend = "llm"`.
    #[serde(default)]
    pub llm: LlmRefineConfig,
}


/// LLM refine backend configuration (`[refine.llm]`).
///
/// Used when `refine.backend = "llm"`. The model is a GGUF file loaded
/// via llama-cpp-2. GPU offload is controlled by `n_gpu_layers`:
/// 0 = CPU only, >0 = offload that many layers to GPU, -1 = all layers.
///
/// Requires the `llm` cargo feature (or `llm-cuda` / `llm-vulkan` /
/// `llm-metal` for GPU acceleration).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmRefineConfig {
    /// Path to a GGUF model file (e.g. Qwen3-0.6B-Q4_K_M.gguf).
    pub model_path: Option<PathBuf>,
    /// Number of layers to offload to GPU. 0 = CPU only, -1 = all layers.
    /// Default: -1 (all layers to GPU when a GPU backend is compiled in;
    /// llama.cpp falls back to CPU when no GPU is available).
    pub n_gpu_layers: i32,
    /// CPU threads for prompt processing and CPU-only inference.
    /// Default: 4.
    pub n_threads: u32,
    /// Maximum tokens to generate in the correction response.
    /// Default: 512 (enough for any single utterance).
    pub max_tokens: u32,
    /// LLM context window size in tokens. The prompt (system + user +
    /// chat template overhead) plus `max_tokens` must fit within this.
    /// Default: 4096. Lower values reduce VRAM; raise it only if
    /// truncation warnings appear in the log.
    pub n_ctx: u32,
    /// Sampling temperature. Lower = more deterministic.
    /// Default: 0.1 (conservative corrections).
    pub temperature: f32,
    /// Custom system prompt. If empty, a built-in prompt is used that
    /// instructs the model to fix grammar, punctuation, and apply
    /// dictionary substitutions without changing meaning.
    pub prompt: String,
    /// Prepend `/no_think` to the user message to suppress reasoning
    /// output in Qwen3-family models. Has no effect on other models.
    /// Default: false. Set to true when using a Qwen3 model.
    pub no_think: bool,
}

impl Default for LlmRefineConfig {
    fn default() -> Self {
        Self {
            model_path: None,
            n_gpu_layers: -1,
            n_threads: 4,
            max_tokens: 512,
            n_ctx: 4096,
            temperature: 0.1,
            prompt: String::new(),
            no_think: false,
        }
    }
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "rules".to_string(),
            dictionary: HashMap::new(),
            llm: LlmRefineConfig::default(),
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
            "rules" => Box::new(RuleRefine::from_map(self.dictionary.clone())),
            "llm" => {
                #[cfg(feature = "llm")]
                {
                    match LlmRefine::new(&self.llm, &self.dictionary) {
                        Ok(backend) => Box::new(backend),
                        Err(e) => {
                            log::error!(
                                "LLM refine backend failed to load: {e:#}. \
                                 Falling back to rules."
                            );
                            Box::new(RuleRefine::from_map(self.dictionary.clone()))
                        }
                    }
                }
                #[cfg(not(feature = "llm"))]
                {
                    log::error!(
                        "refine backend = \"llm\" but the `llm` cargo feature is not enabled. \
                         Rebuild with --features llm (CPU) or --features llm-cuda / \
                         llm-vulkan / llm-metal (GPU). Falling back to rules."
                    );
                    Box::new(RuleRefine::from_map(self.dictionary.clone()))
                }
            }
            other => {
                log::warn!(
                    "unknown refine backend {other:?}; using \"rules\". \
                     Set backend = \"rules\", \"llm\", or enabled = false"
                );
                Box::new(RuleRefine::from_map(self.dictionary.clone()))
            }
        }
    }
}

/// Spaced / split ASR contractions and safe informal spoken forms →
/// solid forms. Matched case-insensitively as whole consecutive words;
/// replacement casing follows the first word of the match when that word
/// was capitalized, otherwise the table literal.
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
    // Split apostrophe forms ASR often emits as separate tokens.
    (&["i", "m"], "I'm"),
    (&["i", "ve"], "I've"),
    (&["i", "ll"], "I'll"),
    (&["i", "d"], "I'd"),
    (&["you", "re"], "you're"),
    (&["you", "ve"], "you've"),
    (&["you", "ll"], "you'll"),
    (&["we", "re"], "we're"),
    (&["we", "ve"], "we've"),
    (&["we", "ll"], "we'll"),
    (&["they", "re"], "they're"),
    (&["they", "ve"], "they've"),
    (&["they", "ll"], "they'll"),
    (&["it", "s"], "it's"),
    (&["that", "s"], "that's"),
    (&["what", "s"], "what's"),
    (&["who", "s"], "who's"),
    (&["there", "s"], "there's"),
    (&["here", "s"], "here's"),
    (&["let", "s"], "let's"),
    (&["is", "n", "t"], "isn't"),
    (&["are", "n", "t"], "aren't"),
    (&["was", "n", "t"], "wasn't"),
    (&["were", "n", "t"], "weren't"),
    (&["do", "n", "t"], "don't"),
    (&["does", "n", "t"], "doesn't"),
    (&["did", "n", "t"], "didn't"),
    (&["can", "t"], "can't"),
    (&["won", "t"], "won't"),
    (&["would", "n", "t"], "wouldn't"),
    (&["could", "n", "t"], "couldn't"),
    (&["should", "n", "t"], "shouldn't"),
    // Informal spoken forms sometimes spaced by ASR.
    (&["gon", "na"], "gonna"),
    (&["wan", "na"], "wanna"),
    (&["got", "ta"], "gotta"),
    (&["lem", "me"], "lemme"),
    (&["giv", "me"], "gimme"),
    (&["dun", "no"], "dunno"),
    (&["y", "all"], "y'all"),
    (&["ya", "ll"], "y'all"),
];

/// Frequent ASR garbling → intended text. High-precision only; documented
/// families stay narrow so brand/dictionary tokens are not reinvented.
/// Matched case-insensitively as consecutive whole words; replacement
/// casing follows the first matched token (not a forced lowercase literal).
const COMMON_ASR_FIXES: &[(&[&str], &str)] = &[
    // Repeated filler / stutter patterns ASR often doubles.
    (&["gotta", "gotta"], "gotta"),
    (&["gonna", "gonna"], "gonna"),
    (&["wanna", "wanna"], "wanna"),
    (&["kind", "of", "of"], "kind of"),
    (&["sort", "of", "of"], "sort of"),
    (&["a", "lot", "of", "of"], "a lot of"),
    (&["out", "of", "of"], "out of"),
    (&["in", "order", "to", "to"], "in order to"),
    (&["going", "to", "to"], "going to"),
    (&["have", "to", "to"], "have to"),
    (&["need", "to", "to"], "need to"),
    (&["want", "to", "to"], "want to"),
    (&["try", "to", "to"], "try to"),
    (&["as", "well", "as", "as"], "as well as"),
    (&["each", "other", "other"], "each other"),
    (&["or", "not", "not"], "or not"),
    // Modal "of" → "have" (spoken "should've" misheard as "should of").
    (&["should", "of"], "should have"),
    (&["could", "of"], "could have"),
    (&["would", "of"], "would have"),
    (&["might", "of"], "might have"),
    (&["must", "of"], "must have"),
    (&["ought", "to", "of"], "ought to have"),
    // Mixed duplicated articles (identical pairs already collapse via
    // `collapse_duplicate_words`, including "i i").
    (&["the", "a"], "the"),
    (&["a", "the"], "the"),
    (&["the", "an"], "the"),
    (&["an", "the"], "the"),
    (&["a", "an"], "an"),
    (&["an", "a"], "an"),
    // Homophone their/there/they're — tight contexts only.
    (&["their", "is"], "there is"),
    (&["their", "are"], "there are"),
    (&["their", "was"], "there was"),
    (&["their", "were"], "there were"),
    (&["there", "going", "to"], "they're going to"),
    (&["your", "going", "to"], "you're going to"),
    (&["your", "gonna"], "you're gonna"),
    (&["your", "welcome"], "you're welcome"),
    // its/it's before a determiner (possessive "its cat" stays).
    (&["its", "a"], "it's a"),
    (&["its", "an"], "it's an"),
    (&["its", "the"], "it's the"),
    // Stuttered existential / wh-clefts.
    (&["there", "is", "is"], "there is"),
    (&["there's", "is"], "there's"),
    (&["there", "are", "are"], "there are"),
    (&["who", "is", "is"], "who is"),
    (&["what's", "is"], "what's"),
    (&["that's", "is"], "that's"),
    // Common fused / misheard spoken forms (single-token safe literals).
    (&["alot"], "a lot"),
    (&["aswell"], "as well"),
    (&["incase"], "in case"),
    (&["supposably"], "supposedly"),
    (&["irregardless"], "regardless"),
    (&["all", "of", "the", "sudden"], "all of a sudden"),
    (&["for", "all", "intensive", "purposes"], "for all intents and purposes"),
    (&["ex", "specially"], "especially"),
];

/// Tiny high-precision subject-verb phrase maps. Not a grammar engine:
/// only fixed spoken pairs that ASR produces with high confidence.
const SUBJECT_VERB_FIXES: &[(&[&str], &str)] = &[
    (&["he", "don't"], "he doesn't"),
    (&["she", "don't"], "she doesn't"),
    (&["it", "don't"], "it doesn't"),
    (&["he", "doesn't", "doesn't"], "he doesn't"),
    (&["she", "doesn't", "doesn't"], "she doesn't"),
    (&["it", "doesn't", "doesn't"], "it doesn't"),
    (&["i", "is"], "i am"),
    (&["you", "is"], "you are"),
    (&["we", "is"], "we are"),
    (&["they", "is"], "they are"),
    (&["he", "are"], "he is"),
    (&["she", "are"], "she is"),
    (&["it", "are"], "it is"),
    (&["i", "are"], "i am"),
    (&["we", "was"], "we were"),
    (&["they", "was"], "they were"),
    (&["you", "was"], "you were"),
    (&["this", "are"], "this is"),
    (&["that", "are"], "that is"),
    (&["these", "is"], "these are"),
    (&["those", "is"], "those are"),
    (&["there", "is", "many"], "there are many"),
    (&["there", "is", "several"], "there are several"),
    (&["there", "is", "few"], "there are few"),
];

/// Leading utterance fillers stripped once. `um`/`uh` stay with the
/// dictionary: refine does not delete them mid-stream or compete with
/// `[overrides] "um" = ""`.
const LEADING_FILLERS: &[&str] = &[
    "well", "so", "okay", "ok", "alright", "anyway", "basically", "like",
];

/// Trailing discourse fillers stripped once when content remains.
/// Multi-word only; never touches dictionary-owned `um`/`uh`.
const TRAILING_FILLER_PHRASES: &[&[&str]] = &[
    &["you", "know"],
    &["i", "mean"],
    &["you", "see"],
    &["i", "guess"],
    &["i", "suppose"],
    &["or", "something"],
    &["or", "whatever"],
    &["and", "stuff"],
    &["and", "everything"],
];

/// Silent-h words that take "an" (ASCII spelling, not a phoneme model).
const AN_SILENT_H: &[&str] = &[
    "hour", "hours", "hourly", "honest", "honestly", "honor", "honors", "honored",
    "honoring", "honour", "honours", "honoured", "honouring", "heir", "heirs",
];

/// u-/eu- words with initial "yoo" glide that take "a", not "an".
const A_YOO_U: &[&str] = &[
    "unique", "university", "universal", "uniform", "united", "useful", "usual",
    "usually", "euro", "european", "europe",
];

/// "lets" → "let's …" only before common imperatives, and never right after
/// a subject pronoun (so "he lets go" stays a real verb phrase).
const LETS_IMPERATIVES: &[&str] = &["go", "see", "try", "start", "do"];
const LETS_SUBJECT_BLOCKERS: &[&str] = &[
    "he", "she", "it", "who", "that", "which", "what",
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

/// Apply the full rule set to transcript text.
pub fn rule_refine(text: &str) -> String {
    rule_refine_with_dict(text, &Dictionary::default())
}

/// Apply the full rule set including dictionary phrase overrides to transcript text.
pub fn rule_refine_with_dict(text: &str, dict: &Dictionary) -> String {
    if text.is_empty() {
        return String::new();
    }

    let text = if !dict.is_empty() {
        dict.apply(text)
    } else {
        text.to_string()
    };

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
    fix_lets_imperatives(&mut words);
    fix_indefinite_article(&mut words);
    apply_phrase_map(&mut words, SPACED_CONTRACTIONS, false);
    collapse_duplicate_words(&mut words);
    collapse_repeated_short_clauses(&mut words);
    strip_leading_filler_once(&mut words);
    strip_trailing_filler_once(&mut words);
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


/// Fix utterance "lets `imperative`" → "let's `imperative`" unless the
/// preceding token is a subject pronoun ("he lets go" must stay).
fn fix_lets_imperatives(words: &mut Vec<Cow<'_, str>>) {
    if words.len() < 2 {
        return;
    }
    let mut i = 0;
    while i + 1 < words.len() {
        if words[i].eq_ignore_ascii_case("lets")
            && LETS_IMPERATIVES
                .iter()
                .any(|imp| words[i + 1].eq_ignore_ascii_case(imp))
        {
            let blocked = i > 0
                && LETS_SUBJECT_BLOCKERS
                    .iter()
                    .any(|s| words[i - 1].eq_ignore_ascii_case(s));
            if !blocked {
                let repl = format!("let's {}", words[i + 1].as_ref().to_ascii_lowercase());
                let replacement = match_contraction_case(&words[i], &repl);
                words.splice(i..i + 2, std::iter::once(Cow::Owned(replacement)));
                i += 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Cheap, safe a/an repair before a following alphabetic word.
///
/// ASCII vowel/consonant starts plus a few high-precision edges:
/// silent-h allowlist (`hour`/`honest`/…), yoo-glide u/eu words
/// (`unique`/`university`/…), and `x…` (spoken "ex"). Still skips bare
/// `u…`/`one`/`once` and unmarked `h…`. Not a phoneme model.
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
        // Silent-h allowlist before generic vowel logic.
        if AN_SILENT_H.iter().any(|w| next_lower == *w) {
            return Some(match_contraction_case(article, "an"));
        }
        // x… is spoken "ex…".
        if first == 'x' {
            return Some(match_contraction_case(article, "an"));
        }
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
        // Known yoo-glide u/eu words take "a".
        if A_YOO_U.iter().any(|w| next_lower == *w) {
            return Some(match_contraction_case(article, "a"));
        }
        // Consonant start; skip h- (hour/honest) and vowels entirely.
        // x… stays "an".
        if first.is_ascii_alphabetic()
            && !matches!(first, 'a' | 'e' | 'i' | 'o' | 'u' | 'h' | 'x')
        {
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

/// Strip at most one trailing discourse-filler phrase when enough content
/// remains (at least two tokens). Does not touch `um`/`uh` (dictionary-owned)
/// and will not reduce `do you know` to a bare `do`.
fn strip_trailing_filler_once(words: &mut Vec<Cow<'_, str>>) {
    if words.len() < 4 {
        return;
    }
    let mut best_n = 0usize;
    for phrase in TRAILING_FILLER_PHRASES {
        let n = phrase.len();
        // Keep ≥2 content tokens so real questions like "do you know" stay.
        if n == 0 || words.len() < n + 2 || n <= best_n {
            continue;
        }
        let start = words.len() - n;
        let ok = phrase
            .iter()
            .zip(words[start..].iter())
            .all(|(p, w)| w.eq_ignore_ascii_case(p));
        if ok {
            best_n = n;
        }
    }
    if best_n > 0 {
        words.truncate(words.len() - best_n);
        while words.last().is_some_and(|w| is_orphan_punct_token(w)) {
            words.pop();
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
            dictionary: HashMap::new(),
            llm: LlmRefineConfig::default(),
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
            dictionary: HashMap::new(),
            llm: LlmRefineConfig::default(),
        };
        let b = cfg.make_backend();
        assert_eq!(b.refine("Hello hello"), "Hello");
    }

    #[test]
    fn rule_refine_via_trait() {
        assert_eq!(RuleRefine::default().refine("the the cat"), "the cat");
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
    fn refine_config_deserializes_dictionary_table() {
        let toml_str = "enabled = true\nbackend = \"rules\"\n\n[dictionary]\nvayon = \"veyyon\"\n";
        let cfg: RefineConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.dictionary.get("vayon").map(String::as_str), Some("veyyon"));
        let backend = cfg.make_backend();
        assert_eq!(backend.refine("hello vayon world"), "hello veyyon world");
    }

    #[test]
    fn refine_config_deserializes_overrides_alias() {
        let toml_str = "enabled = true\nbackend = \"rules\"\n\n[overrides]\nvayon = \"veyyon\"\n";
        let cfg: RefineConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.dictionary.get("vayon").map(String::as_str), Some("veyyon"));
    }

    #[test]
    fn rule_refine_applies_dictionary_rules() {
        let mut map = HashMap::new();
        map.insert("chromax".into(), "Chromax".into());
        let refine = RuleRefine::from_map(map);
        assert_eq!(refine.refine("open chromax now"), "open Chromax now");
    }

    #[test]
    fn modal_of_becomes_have() {
        assert_eq!(rule_refine("I should of gone"), "I should have gone");
        assert_eq!(rule_refine("Could of been"), "Could have been");
        assert_eq!(rule_refine("we would of tried"), "we would have tried");
        assert_eq!(rule_refine("they might of left"), "they might have left");
        assert_eq!(rule_refine("you must of known"), "you must have known");
        assert_eq!(rule_refine("you ought to of asked"), "you ought to have asked");
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
        assert_eq!(rule_refine("their was a chance"), "there was a chance");
        assert_eq!(rule_refine("their were problems"), "there were problems");
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

    #[test]
    fn doubled_prepositions_and_common_mishears() {
        // WHY: ASR often doubles the trailing preposition after fixed
        // phrases; fused mishears need exact-token maps, not fuzzy GEC.
        let cases = [
            ("out of of time", "out of time"),
            ("going to to leave", "going to leave"),
            ("need to to finish", "need to finish"),
            ("as well as as that", "as well as that"),
            ("each other other day", "each other day"),
            ("gonna gonna try", "gonna try"),
            ("wanna wanna see", "wanna see"),
            ("alot of work", "a lot of work"),
            ("do it aswell", "do it as well"),
            ("incase it rains", "in case it rains"),
            ("supposably ready", "supposedly ready"),
            ("irregardless of that", "regardless of that"),
            ("all of the sudden", "all of a sudden"),
            (
                "for all intensive purposes",
                "for all intents and purposes",
            ),
            ("ex specially now", "especially now"),
            ("ought to of asked", "ought to have asked"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // Near-misses: legitimate "of"/"to" sequences stay.
        assert_eq!(rule_refine("out of time"), "out of time");
        assert_eq!(rule_refine("going to leave"), "going to leave");
        assert_eq!(rule_refine("specially now"), "specially now");
    }

    #[test]
    fn homophone_tight_context_maps() {
        // WHY: only flip their/your/its/lets when the following token makes
        // the spoken form overwhelmingly likely.
        let cases = [
            ("there going to leave", "they're going to leave"),
            ("your going to see it", "you're going to see it"),
            ("Your gonna like this", "You're gonna like this"),
            ("your welcome", "you're welcome"),
            ("its a bug", "it's a bug"),
            ("its an idea", "it's an idea"),
            ("Its the plan", "It's the plan"),
            ("lets go now", "let's go now"),
            ("Lets see", "Let's see"),
            ("lets try again", "let's try again"),
            ("who is is next", "who is next"),
            ("what's is wrong", "what's wrong"),
            ("that's is fine", "that's fine"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // Near-misses: possessives / bare verb "lets" stay.
        assert_eq!(rule_refine("your cat sat"), "your cat sat");
        assert_eq!(rule_refine("its whiskers"), "its whiskers");
        assert_eq!(rule_refine("he lets go"), "he lets go");
        assert_eq!(rule_refine("there are cats"), "there are cats");
    }

    #[test]
    fn subject_verb_agreement_extensions() {
        // WHY: expand the tiny map with high-confidence spoken pairs only.
        let cases = [
            ("we was ready", "we were ready"),
            ("they was late", "they were late"),
            ("you was there", "you were there"),
            ("this are fine", "this is fine"),
            ("that are wrong", "that is wrong"),
            ("these is ready", "these are ready"),
            ("those is broken", "those are broken"),
            ("there is many bugs", "there are many bugs"),
            ("there is several options", "there are several options"),
            ("there is few left", "there are few left"),
            ("he doesn't doesn't know", "he doesn't know"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // Near-miss: do not rewrite plural nouns outside the map.
        assert_eq!(rule_refine("the dogs is loud"), "the dogs is loud");
        assert_eq!(rule_refine("there is one bug"), "there is one bug");
    }

    #[test]
    fn a_an_edge_cases_silent_h_yoo_x() {
        // WHY: extend a/an with allowlists, not a phoneme model.
        let cases = [
            ("a hour ago", "an hour ago"),
            ("a honest answer", "an honest answer"),
            ("A heir apparent", "An heir apparent"),
            ("an unique idea", "a unique idea"),
            ("an university town", "a university town"),
            ("an european plan", "a european plan"),
            ("a xray scan", "an xray scan"),
            ("a xylophone", "an xylophone"),
            ("an book", "a book"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // Near-misses: unmarked h-/u- stay; already-correct forms stay.
        assert_eq!(rule_refine("a house"), "a house");
        assert_eq!(rule_refine("an hour"), "an hour");
        assert_eq!(rule_refine("a unique idea"), "a unique idea");
        assert_eq!(rule_refine("a umbrella"), "a umbrella"); // still skipped (u…)
        assert_eq!(rule_refine("an xray"), "an xray");
    }

    #[test]
    fn spaced_split_and_informal_contractions() {
        // WHY: ASR often drops apostrophes or spaces informal reductions.
        let cases = [
            ("i m ready", "I'm ready"),
            ("you re late", "you're late"),
            ("they ve left", "they've left"),
            ("we ll see", "we'll see"),
            ("it s fine", "it's fine"),
            ("that s all", "that's all"),
            ("what s next", "what's next"),
            ("let s go", "let's go"),
            ("do n t stop", "don't stop"),
            ("can t wait", "can't wait"),
            ("won t work", "won't work"),
            ("is n t ready", "isn't ready"),
            ("gon na leave", "gonna leave"),
            ("wan na try", "wanna try"),
            ("got ta go", "gotta go"),
            ("lem me see", "lemme see"),
            ("giv me that", "gimme that"),
            ("dun no why", "dunno why"),
            ("y all ready", "y'all ready"),
            ("I m here", "I'm here"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // Near-miss: already-solid contractions / unrelated letters stay.
        assert_eq!(rule_refine("I'm ready"), "I'm ready");
        assert_eq!(rule_refine("call m later"), "call m later");
        assert_eq!(rule_refine("see ya soon"), "see ya soon");
    }

    #[test]
    fn trailing_filler_cleanup_not_um_uh() {
        // WHY: trailing discourse tags are optional cleanup; never fight
        // dictionary-owned um/uh or wipe the whole utterance.
        let cases = [
            ("we should leave you know", "we should leave"),
            ("that works i mean", "that works"),
            ("look closer you see", "look closer"),
            ("maybe later i guess", "maybe later"),
            ("try again i suppose", "try again"),
            ("bring a charger or something", "bring a charger"),
            ("call support or whatever", "call support"),
            ("packed snacks and stuff", "packed snacks"),
            ("said hello and everything", "said hello"),
        ];
        for (input, expected) in cases {
            assert_eq!(rule_refine(input), expected, "input={input:?}");
        }
        // um/uh untouched; mid-stream discourse phrases stay; lone phrase stays.
        assert_eq!(rule_refine("um I agree"), "um I agree");
        assert_eq!(rule_refine("uh maybe later"), "uh maybe later");
        assert_eq!(rule_refine("I mean yes"), "I mean yes");
        assert_eq!(rule_refine("you know"), "you know");
        assert_eq!(rule_refine("do you know"), "do you know");
    }
}
