//! Final formatting pass: the last stage of the text pipeline.
//!
//! - collapse runs of spaces (never newlines)
//! - no space before punctuation (,.;:!?%) or closing brackets/quotes;
//!   no space after opening brackets/quotes
//! - exactly one space after sentence punctuation (unless end of text)
//! - capitalize the first letter of the text and after ., !, ?, newline
//! - the pronoun "i" becomes "I"
//!
//! Idempotent: format(format(x)) == format(x).

pub fn format(input: &str) -> String {
    let _ = input;
    todo!()
}
