//! Defined voice commands and what they do. Matching is whole-word,
//! case-insensitive, longest phrase first. Applied before the dictionary.

/// What a matched command emits or does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Insert this literal text (punctuation, symbols).
    Insert(&'static str),
    /// One newline.
    Newline,
    /// Paragraph break (two newlines).
    Paragraph,
    /// Delete everything written since the previous sentence boundary
    /// (or the whole utterance when there is none).
    Scratch,
}

/// One voice command: the spoken phrase(s), the action, and user-facing
/// documentation shown by `dictate --list-commands`.
#[derive(Debug, Clone, Copy)]
pub struct VoiceCommand {
    /// Alternative spoken forms, each a single phrase (may be multi-word).
    pub phrases: &'static [&'static str],
    pub action: Action,
    pub doc: &'static str,
}

/// The full command table. Keep it small: every entry is a footgun when a
/// user actually wants to say the words literally.
pub const COMMANDS: &[VoiceCommand] = &[
    VoiceCommand {
        phrases: &["period", "full stop"],
        action: Action::Insert("."),
        doc: "period / full stop → .",
    },
    VoiceCommand {
        phrases: &["comma"],
        action: Action::Insert(","),
        doc: "comma → ,",
    },
    VoiceCommand {
        phrases: &["question mark"],
        action: Action::Insert("?"),
        doc: "question mark → ?",
    },
    VoiceCommand {
        phrases: &["exclamation mark", "exclamation point"],
        action: Action::Insert("!"),
        doc: "exclamation mark / point → !",
    },
    VoiceCommand {
        phrases: &["colon"],
        action: Action::Insert(":"),
        doc: "colon → :",
    },
    VoiceCommand {
        phrases: &["semicolon", "semi colon"],
        action: Action::Insert(";"),
        doc: "semicolon → ;",
    },
    VoiceCommand {
        phrases: &["ellipsis", "dot dot dot"],
        action: Action::Insert("…"),
        doc: "ellipsis / dot dot dot → …",
    },
    VoiceCommand {
        phrases: &["open quote"],
        action: Action::Insert("\""),
        doc: "open quote → \"",
    },
    VoiceCommand {
        phrases: &["close quote", "end quote", "unquote"],
        action: Action::Insert("\""),
        doc: "close quote / end quote / unquote → \"",
    },
    VoiceCommand {
        phrases: &["open paren", "open parenthesis", "open bracket"],
        action: Action::Insert("("),
        doc: "open paren → (",
    },
    VoiceCommand {
        phrases: &["close paren", "close parenthesis", "close bracket"],
        action: Action::Insert(")"),
        doc: "close paren → )",
    },
    VoiceCommand {
        phrases: &["percent sign"],
        action: Action::Insert("%"),
        doc: "percent sign → %",
    },
    VoiceCommand {
        phrases: &["dollar sign"],
        action: Action::Insert("$"),
        doc: "dollar sign → $",
    },
    VoiceCommand {
        phrases: &["new line"],
        action: Action::Newline,
        doc: "new line → line break",
    },
    VoiceCommand {
        phrases: &["new paragraph"],
        action: Action::Paragraph,
        doc: "new paragraph → blank line",
    },
    VoiceCommand {
        phrases: &["scratch that", "delete that", "strike that"],
        action: Action::Scratch,
        doc: "scratch that / delete that → delete back to the last sentence boundary",
    },
];

/// One input token. Whisper attaches sentence punctuation to the
/// neighboring word ("mark."); edge punctuation is split off into its
/// own token so whole-word matching stays honest on real transcripts.
/// Interior punctuation (apostrophes, hyphens, decimal points) stays
/// inside the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tok<'a> {
    Word(&'a str),
    Punct(char),
    Newline,
}

/// Split raw transcript text into tokens. Words keep their original
/// case; newlines become hard tokens so line structure survives and
/// Scratch can use a newline as a sentence boundary.
pub(super) fn tokenize(input: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    for (n, line) in input.split('\n').enumerate() {
        if n > 0 {
            toks.push(Tok::Newline);
        }
        for chunk in line.split_whitespace() {
            let lead = chunk.len() - chunk.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
            for c in chunk[..lead].chars() {
                toks.push(Tok::Punct(c));
            }
            let rest = &chunk[lead..];
            let core_len = rest.trim_end_matches(|c: char| !c.is_alphanumeric()).len();
            let (core, tail) = rest.split_at(core_len);
            if !core.is_empty() {
                toks.push(Tok::Word(core));
            }
            for c in tail.chars() {
                toks.push(Tok::Punct(c));
            }
        }
    }
    toks
}

/// Match the `words` of a phrase against consecutive word tokens
/// starting at token index `i` (which must be a word). Punctuation
/// between words is ignored; a newline never participates. Returns the
/// token index one past the last matched word.
#[must_use]
pub(super) fn match_at(toks: &[Tok<'_>], i: usize, words: &[&str]) -> Option<usize> {
    let mut ti = i;
    for (k, pw) in words.iter().enumerate() {
        if k > 0 {
            while matches!(toks.get(ti), Some(Tok::Punct(_))) {
                ti += 1;
            }
        }
        match toks.get(ti) {
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case(pw) => ti += 1,
            _ => return None,
        }
    }
    Some(ti)
}

/// Apply the command table to raw transcript text.
///
/// Algorithm: tokenize into words, match command phrases greedily
/// longest-first at each position, emit words/actions into an output
/// buffer. `Scratch` truncates the output back past the last
/// sentence-final punctuation ('.', '!', '?', newline) if any, else
/// clears it. Spacing between words is single-space; the formatter fixes
/// punctuation spacing later.
pub fn apply(input: &str) -> String {
    // Every spoken form, pre-split into words, longest first so
    // greedy matching is stable. The table is tiny, so a per-call build
    // is cheaper than a lazy static.
    let mut phrases: Vec<(Vec<&str>, Action)> = Vec::new();
    for cmd in COMMANDS {
        for phrase in cmd.phrases {
            phrases.push((phrase.split_whitespace().collect(), cmd.action));
        }
    }
    phrases.sort_by_key(|(words, _)| std::cmp::Reverse(words.len()));

    let toks = tokenize(input);

    fn push_word(out: &mut String, w: &str) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(w);
    }

    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < toks.len() {
        // Match a command phrase at this position, longest first.
        let mut matched: Option<(usize, Action)> = None;
        if let Tok::Word(_) = toks[i] {
            for (words, action) in &phrases {
                if let Some(end) = match_at(&toks, i, words) {
                    matched = Some((end, *action));
                    break;
                }
            }
        }

        match matched {
            Some((mut end, action)) => {
                // A matched command supplies its own punctuation; absorb
                // any transcript punctuation immediately after it so
                // "question mark." does not leave a stray "." behind.
                while matches!(toks.get(end), Some(Tok::Punct(_))) {
                    end += 1;
                }
                match action {
                    Action::Insert(s) => push_word(&mut out, s),
                    Action::Newline => out.push('\n'),
                    Action::Paragraph => {
                        // A paragraph break supersedes any pending line
                        // breaks: normalize to exactly two newlines.
                        while out.ends_with('\n') {
                            out.pop();
                        }
                        if !out.is_empty() {
                            out.push_str("\n\n");
                        }
                    }
                    Action::Scratch => {
                        // Keep the boundary punctuation itself; delete
                        // everything written after it.
                        match out.rfind(['.', '!', '?', '\n']) {
                            Some(idx) => out.truncate(idx + 1),
                            None => out.clear(),
                        }
                    }
                }
                i = end;
            }
            None => {
                match toks[i] {
                    Tok::Word(w) => push_word(&mut out, w),
                    Tok::Punct(c) => {
                        let mut buf = [0u8; 4];
                        push_word(&mut out, c.encode_utf8(&mut buf));
                    }
                    Tok::Newline => out.push('\n'),
                }
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every documented phrase must actually trigger its action, and the
    /// user-facing doc must name the primary spoken form. This guards the
    /// `--list-commands` output from drifting away from real behavior.
    #[test]
    fn every_command_fires() {
        assert!(!COMMANDS.is_empty());
        for cmd in COMMANDS {
            assert!(!cmd.doc.is_empty(), "command missing doc");
            assert!(
                cmd.doc.contains(cmd.phrases[0]),
                "doc {:?} does not name primary phrase {:?}",
                cmd.doc,
                cmd.phrases[0]
            );
            for phrase in cmd.phrases {
                match cmd.action {
                    Action::Insert(s) => assert_eq!(apply(phrase), s, "phrase {phrase:?}"),
                    Action::Newline => assert_eq!(apply(phrase), "\n", "phrase {phrase:?}"),
                    Action::Paragraph => assert_eq!(apply(phrase), "", "phrase {phrase:?} at start emits nothing before text"),
                    Action::Scratch => assert_eq!(
                        apply(&format!("keep this. delete this {phrase}")),
                        "keep this .",
                        "phrase {phrase:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn paragraph_after_text_emits_two_newlines() {
        assert_eq!(apply("first new paragraph second"), "first\n\nsecond");
        // A paragraph break supersedes a preceding line break.
        assert_eq!(apply("a new line new paragraph b"), "a\n\nb");
    }

    #[test]
    fn matching_is_case_insensitive_but_words_keep_case() {
        assert_eq!(apply("PERIOD"), ".");
        assert_eq!(apply("Hello WORLD period"), "Hello WORLD .");
        assert_eq!(apply("New LINE"), "\n");
    }

    #[test]
    fn matching_is_whole_word() {
        // "period" inside "periodic" must not fire.
        assert_eq!(apply("periodic table"), "periodic table");
        // "comma" inside "command" must not fire.
        assert_eq!(apply("command line"), "command line");
        // "colon" inside "colonial" must not fire.
        assert_eq!(apply("colonial era"), "colonial era");
    }

    #[test]
    fn multi_word_phrases_match_across_words_only() {
        assert_eq!(apply("full stop"), ".");
        // Neither word alone is a command.
        assert_eq!(apply("full"), "full");
        assert_eq!(apply("stop"), "stop");
        // Words must be adjacent.
        assert_eq!(apply("full of stop"), "full of stop");
    }

    #[test]
    fn longest_phrase_wins_at_each_position() {
        // "open parenthesis" must win over any shorter prefix reading.
        assert_eq!(apply("open parenthesis"), "(");
        assert_eq!(apply("open paren"), "(");
        // "exclamation mark" is two words, not "exclamation" + "mark".
        assert_eq!(apply("exclamation mark"), "!");
        assert_eq!(apply("dot dot dot"), "…");
    }

    #[test]
    fn inserts_are_single_spaced_for_the_formatter() {
        assert_eq!(apply("hello comma world period"), "hello , world .");
        assert_eq!(apply("open quote hi close quote"), "\" hi \"");
        assert_eq!(apply("fifty percent sign"), "fifty %");
        assert_eq!(apply("five dollar sign"), "five $");
        assert_eq!(apply("colon semicolon"), ": ;");
    }

    #[test]
    fn newline_command_inserts_line_break() {
        assert_eq!(apply("hello new line world"), "hello\nworld");
    }

    #[test]
    fn scratch_with_prior_sentence_boundary_keeps_boundary() {
        assert_eq!(
            apply("hello world period this is bad scratch that"),
            "hello world ."
        );
        // Transcript punctuation is split into its own token; the
        // formatter reattaches it later.
        assert_eq!(
            apply("good sentence! bad words delete that"),
            "good sentence !"
        );
        assert_eq!(apply("sure? no strike that"), "sure ?");
    }

    #[test]
    fn scratch_without_boundary_clears_everything() {
        assert_eq!(apply("one two three scratch that"), "");
    }

    #[test]
    fn scratch_at_start_is_a_no_op_on_empty_output() {
        assert_eq!(apply("scratch that"), "");
        assert_eq!(apply("scratch that hello"), "hello");
    }

    #[test]
    fn scratch_uses_newline_as_boundary() {
        assert_eq!(
            apply("first line new line second line scratch that"),
            "first line\n"
        );
    }

    #[test]
    fn empty_and_whitespace_input() {
        assert_eq!(apply(""), "");
        assert_eq!(apply("   \t  "), "");
    }

    #[test]
    fn transcript_punctuation_does_not_block_matching() {
        // Whisper attaches sentence punctuation to the last word.
        assert_eq!(apply("lazy dog question mark."), "lazy dog ?");
        assert_eq!(apply("hello world period,"), "hello world .");
        // Punctuation inside a phrase span does not block it either.
        assert_eq!(apply("question, mark"), "?");
        // Unmatched words keep their punctuation (formatter reattaches).
        assert_eq!(apply("well, okay"), "well , okay");
    }

    #[test]
    fn interior_punctuation_stays_in_words() {
        assert_eq!(apply("don't stop"), "don't stop");
        assert_eq!(apply("3.14 period"), "3.14 .");
        assert_eq!(apply("e.g. this"), "e.g . this");
    }
}
