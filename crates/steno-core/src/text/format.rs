//! Final formatting pass: the last stage of the text pipeline.
//!
//! - collapse runs of spaces (never newlines)
//! - no space before punctuation (,.;:!?%) or closing brackets/quotes;
//!   no space after opening brackets/quotes
//! - exactly one space after sentence punctuation (unless end of text)
//! - capitalize the first letter of the text and after ., !, ?, newline
//! - the pronoun "i" becomes "I"
//! - duplicate pause punctuation (, ; : ! ? %) collapses to one (the
//!   recognizer inserts its own punctuation around spoken commands: "bank,
//!   comma," must not become "bank,,"). Runs of '.' are kept ("..."),
//!   intentional stutters like "!!" are lost.
//!
//! Idempotent: format(format(x)) == format(x).
//!
//! Resolved ambiguities:
//! - Double quotes toggle open/close state. A single quote does the
//!   same, but only when it is not between two word characters, so
//!   apostrophes ("don't") are never mangled.
//! - "."/"!"/"?" only starts a new sentence (capitalization) when
//!   followed by whitespace or end of text, so "e.g.", "3.14", and
//!   "example.com" are left alone. Spaces are normalized, never
//!   inserted, so decimals are never split.

/// Characters that never take a space before them.
fn is_closing(c: char) -> bool {
    matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | '%' | ')' | ']' | '}')
}

/// Formatter state carried across streamed chunks. Quote and bracket
/// state must survive segment boundaries: otherwise a closing quote that
/// lands in the NEXT chunk is misread as an opening one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FmtState {
    /// Next alphabetic char starts a sentence (begin forced uppercase).
    pub capitalize_next: bool,
/// Inside a double-quote pair.
pub in_dquote: bool,
/// Inside a single-quote pair.
pub in_squote: bool,
    /// Last emitted char was an opener (`(`, `[`, `{`, or opening quote):
    /// suppresses the space before the next char.
    pub last_open: bool,
}

impl Default for FmtState {
    /// Fresh text: start of a sentence, outside any quote.
    fn default() -> Self {
        Self {
            capitalize_next: true,
            in_dquote: false,
            in_squote: false,
            last_open: false,
        }
    }
}

/// Streaming entry point: pass [`FmtState::default`] for the first chunk,
/// then feed each returned state into the next call.
pub fn format_with(input: &str, state: FmtState) -> (String, FmtState) {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut pending_space = false;
    let FmtState {
        mut capitalize_next,
        mut in_dquote,
        mut in_squote,
        mut last_open,
    } = state;

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => {
                pending_space = true;
                continue;
            }
            '\n' => {
                // Newlines pass through untouched (blank lines survive);
                // any pending space before one is dropped.
                out.push('\n');
                pending_space = false;
                last_open = false;
                capitalize_next = true;
                continue;
            }
            _ => {}
        }

        // The standalone pronoun "i" is always capitalized. A pending
        // (not yet flushed) space already ends the previous word.
        let word_before = !pending_space && out.chars().last().is_some_and(char::is_alphanumeric);
        let word_after = chars.peek().is_some_and(|n| n.is_alphanumeric());
        let c = if c == 'i' && !word_before && !word_after {
            'I'
        } else {
            c
        };
        // A single quote between word characters is an apostrophe, not
        // a quote delimiter.
        let squote = c == '\'' && !(word_before && word_after);

        let closing = is_closing(c) || (c == '"' && in_dquote) || (squote && in_squote);

        if pending_space {
            pending_space = false;
            let at_line_start = out.is_empty() || out.ends_with('\n');
            if !at_line_start && !closing && !last_open {
                out.push(' ');
            }
        }

        // Collapse duplicate pause punctuation (see module docs).
        // Dots are excluded: "..." is meaningful.
        if matches!(c, ',' | ';' | ':' | '!' | '?' | '%') && out.ends_with(c) {
            continue;
        }

        if capitalize_next && c.is_alphabetic() {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }

        if c.is_alphanumeric() {
            capitalize_next = false;
        }
        match c {
            // A sentence boundary only counts when something separates it
            // from the next word; that keeps abbreviations and decimals
            // intact.
            '.' | '!' | '?' => {
                capitalize_next = chars.peek().is_none_or(|n| n.is_whitespace());
            }
            '"' => in_dquote = !in_dquote,
            _ if squote => in_squote = !in_squote,
            _ => {}
        }
        last_open =
            matches!(c, '(' | '[' | '{') || (c == '"' && in_dquote) || (squote && in_squote);
    }
    // A trailing newline also means the next chunk starts a sentence.
    if out.ends_with('\n') {
        capitalize_next = true;
    }
    (
        out,
        FmtState {
            capitalize_next,
            in_dquote,
            in_squote,
            last_open,
        },
    )
}

#[cfg(test)]
mod tests {
    //! WHY: Streaming text formatting (`FmtState`) must correctly manage sentence capitalization,
    //! pronoun handling, quote tracking, and spacing across audio segment boundaries.
    use super::{FmtState, format_with};

    /// One-shot convenience for tests: format a whole string at once.
    fn format(input: &str) -> String {
        format_with(input, FmtState::default()).0
    }

    #[test]
    fn stream_state_carries_across_chunks() {
        // Mid-sentence continuation: no forced capital, state stays false.
        let (a, st) = format_with("the quick", FmtState::default());
        assert_eq!(a, "The quick");
        assert!(!st.capitalize_next);
        let (b, st) = format_with("brown fox.", st);
        assert_eq!(b, "brown fox.");
        assert!(
            st.capitalize_next,
            "sentence-final period must set next-chunk capital"
        );
        let (c, _) = format_with("done here", st);
        assert_eq!(c, "Done here");
    }

    #[test]
    fn stream_pronoun_and_newline_state() {
        // The pronoun rule applies mid-sentence too.
        let mid = FmtState {
            capitalize_next: false,
            ..FmtState::default()
        };
        let (a, _) = format_with("then i left", mid);
        assert_eq!(a, "then I left");
        // A trailing newline means the next chunk starts a sentence.
        let (_, st) = format_with("first line\n", mid);
        assert!(st.capitalize_next);
    }

    #[test]
    fn stream_quote_state_survives_segment_boundary() {
        // Regression: a quote opened in one chunk must close in the next.
        // Before FmtState carried quote state, the closing quote was
        // misread as an opening one, mangling spacing.
        let (a, st) = format_with("say \"hello", FmtState::default());
        assert_eq!(a, "Say \"hello");
        assert!(st.in_dquote);
        let (b, st) = format_with(" world\" now", st);
        // The leading space is stripped (the Emitter's joiner re-adds it).
        assert_eq!(b, "world\" now");
        assert!(!st.in_dquote);
        assert_eq!(format("say \"hello world\" now"), format!("{a} {b}"));

        // Single quotes: same boundary behavior.
        let (a, st) = format_with("the ' red", FmtState::default());
        assert!(st.in_squote);
        let (b, _) = format_with(" one ' here", st);
        assert_eq!(format!("{a} {b}"), format("the ' red one ' here"));
    }

    #[test]
    fn collapses_space_runs_but_not_newlines() {
        assert_eq!(format("a  b   c"), "A b c");
        assert_eq!(format("a\t \tb"), "A b");
        assert_eq!(format("a\n\nb"), "A\n\nB");
        // Spaces around newlines are dropped, never kept.
        assert_eq!(format("a  \n  b"), "A\nB");
    }

    #[test]
    fn no_space_before_punctuation_or_closers() {
        assert_eq!(format("hello , world ."), "Hello, world.");
        assert_eq!(
            format("wait ; really : yes ! no ?"),
            "Wait; really: yes! No?"
        );
        assert_eq!(format("fifty %"), "Fifty%");
        assert_eq!(format("( hi ) [ there ] { you }"), "(Hi) [there] {you}");
    }

    #[test]
    fn no_space_after_openers_or_inside_quotes() {
        assert_eq!(format("( hello"), "(Hello");
        assert_eq!(format("say \" hi \" now"), "Say \"hi\" now");
        assert_eq!(format("\" quoted \" word"), "\"Quoted\" word");
    }

    #[test]
    fn apostrophes_are_not_quotes() {
        assert_eq!(
            format("i don't think it's ' odd '"),
            "I don't think it's 'odd'"
        );
        assert_eq!(format("rock 'n' roll"), "Rock 'n' roll");
    }

    #[test]
    fn one_space_after_sentence_punctuation() {
        assert_eq!(format("hi.  there"), "Hi. There");
        assert_eq!(format("wow !\namazing ?  yes"), "Wow!\nAmazing? Yes");
        // No space is inserted where none existed: decimals and
        // abbreviations survive, and there is no capitalization either.
        assert_eq!(format("pi is 3.14"), "Pi is 3.14");
        assert_eq!(format("use e.g. this"), "Use e.g. This");
        assert_eq!(format("see example.com now"), "See example.com now");
    }

    #[test]
    fn capitalizes_first_letter_and_after_sentence_boundaries() {
        assert_eq!(format("hello world"), "Hello world");
        assert_eq!(format("wow! amazing? yes. done"), "Wow! Amazing? Yes. Done");
        assert_eq!(format("one line\nnext line"), "One line\nNext line");
        // Capitalization reaches through opening brackets and quotes.
        assert_eq!(
            format("hello. \"world\" ok. (next)"),
            "Hello. \"World\" ok. (Next)"
        );
    }

    #[test]
    fn pronoun_i_becomes_capital() {
        assert_eq!(format("i think i am"), "I think I am");
        assert_eq!(format("am i right"), "Am I right");
        assert_eq!(format("i'm sure"), "I'm sure");
        // "i" inside a larger word is untouched.
        assert_eq!(format("this is fine, hi"), "This is fine, hi");
    }

    #[test]
    fn empty_whitespace_and_punctuation_only() {
        assert_eq!(format(""), "");
        assert_eq!(format("   \t \n  "), "\n");
        assert_eq!(format("..."), "...");
        assert_eq!(format(" ! ? "), "!?");
    }

    #[test]
    fn paragraph_blank_lines_preserved() {
        assert_eq!(
            format("first para.\n\n\nsecond para."),
            "First para.\n\n\nSecond para."
        );
    }

    #[test]
    fn combined_messy_input() {
        let messy =
            "  hello   world .  i said \" hi there \" ( it works ) .\n\nnext  line !  ok ?  ";
        let expected = "Hello world. I said \"hi there\" (it works).\n\nNext line! Ok?";
        assert_eq!(format(messy), expected);
    }

    #[test]
    fn duplicate_pause_punctuation_collapses() {
        // The real case: the recognizer writes "bank, comma," around the
        // spoken command; the command supplies "," and the transcript one must go.
        assert_eq!(format("bank , , main street"), "Bank, main street");
        assert_eq!(format("yes ! ! no ? ?"), "Yes! No?");
        // Runs of dots survive (ellipsis intent); the sentence-boundary
        // rule still applies after the final dot.
        assert_eq!(format("wait ... ok"), "Wait... Ok");
        assert_eq!(format("..."), "...");
    }

    #[test]
    fn unbalanced_quotes_and_mid_quote_endings() {
        assert_eq!(format("say \"hi"), "Say \"hi");
        assert_eq!(format("she said \"stop"), "She said \"stop");
        assert_eq!(format("\"oops"), "\"Oops");
    }

    #[test]
    fn quotes_adjacent_to_newlines() {
        assert_eq!(format("a\n\" b\"\nc"), "A\n\"B\"\nC");
    }

    #[test]
    fn pronoun_i_at_every_boundary() {
        assert_eq!(format("i i, i. i\ni"), "I I, I. I\nI");
        assert_eq!(format("\"i\" said (i)"), "\"I\" said (I)");
    }

    #[test]
    fn unicode_passthrough() {
        // Emoji are not alphanumerics; they pass through without spacing
        // or capitalization side effects. CJK needs no spaces.
        assert_eq!(format("hello 👋 world"), "Hello 👋 world");
        assert_eq!(format("你好世界"), "你好世界");
        assert_eq!(format("你好 世界"), "你好 世界");
        assert_eq!(format("café — résumé"), "Café — résumé");
    }

    #[test]
    fn idempotent_on_adversarial_inputs() {
        let cases = [
            "say \"hi",
            "a\n\" b\"\nc",
            "i i, i. i\ni",
            "hello 👋 world … next",
            "\"'\"",
            "你好世界 3.14 e.g. wait ... ok",
            "! ,, ;; ?? %%",
        ];
        for x in cases {
            let once = format(x);
            assert_eq!(format(&once), once, "not idempotent for {x:?}");
        }
        // Long input stays correct and idempotent.
        let long = "hello world, i said \"quote me\". ".repeat(5_000);
        let once = format(&long);
        assert_eq!(format(&once), once);
    }

    #[test]
    fn idempotent_on_messy_input() {
        let messy = "  hello   world .  i said \" hi there \" ( it works ) .\n\nnext  line !  e.g. 3.14 ok ?  ";
        let once = format(messy);
        assert_eq!(format(&once), once, "format must be idempotent");
    }
}
