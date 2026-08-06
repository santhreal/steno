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

/// Apply the command table to raw transcript text.
///
/// Algorithm: tokenize into words, match command phrases greedily
/// longest-first at each position, emit words/actions into an output
/// buffer. `Scratch` truncates the output back past the last
/// sentence-final punctuation ('.', '!', '?', newline) if any, else
/// clears it. Spacing between words is single-space; the formatter fixes
/// punctuation spacing later.
pub fn apply(input: &str) -> String {
    let _ = input;
    todo!()
}
