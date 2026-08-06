//! Dictionary: user-defined phrase overrides loaded from TOML.
//!
//! File format (`~/.config/dictate/dictionary.toml` by default):
//! ```toml
//! [overrides]
//! "hypr whisper" = "hyprwhspr"
//! "mukund" = "Mukund"
//! "um" = ""            # empty replacement deletes the phrase
//! ```
//!
//! Matching is case-insensitive, whole-word, longest phrase first.
//! Replacement text is inserted literally (case as written).

use anyhow::Result;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Dictionary {
    // Sorted longest-phrase-first so greedy matching is stable.
    entries: Vec<(String, String)>,
}

impl Dictionary {
    /// `None` → empty dictionary. A missing explicit file is an error;
    /// a malformed one is an error naming the file.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let _ = path;
        todo!()
    }

    /// Also construct from an in-memory table (tests, defaults).
    pub fn from_entries<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let _ = entries;
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// Apply overrides to `input`. Whole-word matching: a phrase never
    /// matches inside a larger word.
    pub fn apply(&self, input: &str) -> String {
        let _ = input;
        todo!()
    }
}
