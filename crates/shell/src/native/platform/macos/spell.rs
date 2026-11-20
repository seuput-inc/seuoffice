//! `SpellOps` over the system spell checker. The checker is shared by the whole process, so
//! a word learned here is learned for every web view and survives restarts.

use objc2::rc::Retained;
use objc2_app_kit::NSSpellChecker;
use objc2_foundation::{NSRange, NSString};

use super::Platform;
use crate::native::platform::SpellOps;

/// `NSNotFound`: the checker's answer when the string has no misspelled word.
const NOT_FOUND: usize = isize::MAX as usize;

fn whole(word: &NSString) -> NSRange {
    NSRange {
        location: 0,
        length: word.length(),
    }
}

fn word_of(text: &str) -> Option<Retained<NSString>> {
    let text = text.trim();
    (!text.is_empty()).then(|| NSString::from_str(text))
}

impl SpellOps for Platform {
    fn suggestions(word: &str) -> Result<Vec<String>, String> {
        let Some(word) = word_of(word) else {
            return Ok(Vec::new());
        };
        let checker = NSSpellChecker::sharedSpellChecker();
        let guesses = checker.guessesForWordRange_inString_language_inSpellDocumentWithTag(
            whole(&word),
            &word,
            None,
            0,
        );
        Ok(guesses
            .map(|guesses| {
                guesses
                    .to_vec()
                    .iter()
                    .map(|guess| guess.to_string())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn is_misspelled(word: &str) -> Result<bool, String> {
        let Some(word) = word_of(word) else {
            return Ok(false);
        };
        let found = NSSpellChecker::sharedSpellChecker().checkSpellingOfString_startingAt(&word, 0);
        Ok(found.location != NOT_FOUND)
    }

    fn learn(word: &str) -> Result<bool, String> {
        let word = word_of(word).ok_or("A word to learn must not be empty.")?;
        NSSpellChecker::sharedSpellChecker().learnWord(&word);
        Ok(true)
    }

    fn unlearn(word: &str) -> Result<bool, String> {
        let word = word_of(word).ok_or("A word to unlearn must not be empty.")?;
        NSSpellChecker::sharedSpellChecker().unlearnWord(&word);
        Ok(true)
    }
}
