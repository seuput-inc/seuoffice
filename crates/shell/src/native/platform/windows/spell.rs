//! `SpellOps` over the Windows spell checking API (`ISpellCheckerFactory`).
//!
//! WebView2 has no spell-check surface the shell can reach: its context menu carries the
//! suggestion items without their text, and it has no user dictionary or language setting.
//! The system checker is what the shell can ask, and the user dictionary it keeps is shared
//! by every app and survives restarts, as the macOS checker's is.
//!
//! The checker follows the language WebView2 itself runs in: wry creates the environment in
//! the user's UI language, so that is the language asked here.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Globalization::{
    GetUserDefaultUILanguage, ISpellChecker, ISpellChecker2, ISpellCheckerFactory,
    LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME, SpellCheckerFactory,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::core::{HSTRING, Interface, PWSTR};

use super::Platform;
use crate::native::platform::SpellOps;

/// More than the menu shows, so a duplicate or empty answer cannot empty it.
const MAX_SUGGESTIONS: usize = 10;

thread_local! {
    /// COM objects stay in the apartment that made them: the checker is made on, and only
    /// used from, the UI thread. Creating one loads the dictionary, so it is kept.
    static CHECKER: RefCell<Option<ISpellChecker>> = const { RefCell::new(None) };
}

/// `tag` and the languages it falls back to, most specific first: `zh-Hant-TW`, `zh-Hant`,
/// `zh`.
fn candidates(tag: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = tag;
    while !rest.is_empty() {
        found.push(rest);
        match rest.rfind('-') {
            Some(at) => rest = &rest[..at],
            None => break,
        }
    }
    found
}

/// A word the checker can be asked about: trimmed, and free of the NUL a wide string cannot
/// carry.
fn word_of(text: &str) -> Option<HSTRING> {
    let text = text.trim();
    (!text.is_empty() && !text.contains('\0')).then(|| HSTRING::from(text))
}

fn failed(what: &str, error: windows::core::Error) -> String {
    format!("The Windows spell checker {what}: {error}")
}

/// The language tag WebView2 was created with (wry: the user's UI language).
fn ui_language() -> Result<String, String> {
    let mut name = [0u16; MAX_LOCALE_NAME as usize];
    // SAFETY: both calls only read the OS language and write into `name`.
    let written = unsafe {
        let language = GetUserDefaultUILanguage();
        LCIDToLocaleName(
            u32::from(language),
            Some(&mut name),
            LOCALE_ALLOW_NEUTRAL_NAMES,
        )
    };
    // The count includes the terminating NUL.
    let length = usize::try_from(written)
        .ok()
        .and_then(|count| count.checked_sub(1))
        .filter(|length| *length > 0)
        .ok_or("The Windows spell checker could not read the UI language.")?;
    Ok(String::from_utf16_lossy(&name[..length]))
}

fn create() -> Result<ISpellChecker, String> {
    // SAFETY: a plain in-process COM activation on the UI thread, which WebView2 already
    // initialized as an apartment.
    let factory: ISpellCheckerFactory =
        unsafe { CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| failed("is unavailable", error))?;
    let tag = ui_language()?;
    for candidate in candidates(&tag) {
        let language = HSTRING::from(candidate);
        // SAFETY: `language` outlives the call.
        let supported = unsafe { factory.IsSupported(&language) }
            .map_err(|error| failed("could not list its languages", error))?;
        if supported.as_bool() {
            // SAFETY: as above.
            return unsafe { factory.CreateSpellChecker(&language) }
                .map_err(|error| failed("could not load its dictionary", error));
        }
    }
    Err(format!(
        "The Windows spell checker has no dictionary for {tag}."
    ))
}

fn with_checker<T>(run: impl FnOnce(&ISpellChecker) -> Result<T, String>) -> Result<T, String> {
    CHECKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(create()?);
        }
        match slot.as_ref() {
            Some(checker) => run(checker),
            None => Err("The Windows spell checker is not loaded.".to_owned()),
        }
    })
}

impl SpellOps for Platform {
    fn suggestions(word: &str) -> Result<Vec<String>, String> {
        let Some(word) = word_of(word) else {
            return Ok(Vec::new());
        };
        with_checker(|checker| {
            // SAFETY: `word` outlives the call; every string the enumerator hands out is
            // owned by the caller and freed right after it is copied.
            unsafe {
                let list = checker
                    .Suggest(&word)
                    .map_err(|error| failed("could not suggest", error))?;
                let mut found: Vec<String> = Vec::new();
                while found.len() < MAX_SUGGESTIONS {
                    let mut slot = [PWSTR::null()];
                    let mut fetched = 0u32;
                    list.Next(&mut slot, Some(&mut fetched as *mut u32))
                        .ok()
                        .map_err(|error| failed("could not read its suggestions", error))?;
                    if fetched == 0 {
                        break;
                    }
                    let text = slot[0].to_string();
                    CoTaskMemFree(Some(slot[0].0 as *const c_void));
                    if let Ok(text) = text
                        && !text.is_empty()
                        && !found.contains(&text)
                    {
                        found.push(text);
                    }
                }
                Ok(found)
            }
        })
    }

    fn is_misspelled(word: &str) -> Result<bool, String> {
        let Some(word) = word_of(word) else {
            return Ok(false);
        };
        with_checker(|checker| {
            // SAFETY: `word` outlives the call. `Next` answers S_OK with an error, or S_FALSE
            // with none and a null slot.
            unsafe {
                let errors = checker
                    .Check(&word)
                    .map_err(|error| failed("could not check the word", error))?;
                let mut first = None;
                errors
                    .Next(&mut first)
                    .ok()
                    .map_err(|error| failed("could not read its findings", error))?;
                Ok(first.is_some())
            }
        })
    }

    fn learn(word: &str) -> Result<bool, String> {
        let word = word_of(word).ok_or("A word to learn must not be empty.")?;
        with_checker(|checker| {
            // SAFETY: `word` outlives the call.
            unsafe { checker.Add(&word) }
                .map_err(|error| failed("did not learn the word", error))?;
            Ok(true)
        })
    }

    fn unlearn(word: &str) -> Result<bool, String> {
        let word = word_of(word).ok_or("A word to unlearn must not be empty.")?;
        with_checker(|checker| {
            let checker = checker
                .cast::<ISpellChecker2>()
                .map_err(|error| failed("cannot drop words on this Windows", error))?;
            // SAFETY: `word` outlives the call.
            unsafe { checker.Remove(&word) }
                .map_err(|error| failed("did not drop the word", error))?;
            Ok(true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_falls_back_one_subtag_at_a_time() {
        assert_eq!(candidates("zh-Hant-TW"), ["zh-Hant-TW", "zh-Hant", "zh"]);
        assert_eq!(candidates("ko-KR"), ["ko-KR", "ko"]);
        assert_eq!(candidates("en"), ["en"]);
        assert!(candidates("").is_empty());
    }

    #[test]
    fn a_word_is_trimmed_and_refused_when_empty_or_carrying_nul() {
        assert_eq!(word_of("  teh \n"), Some(HSTRING::from("teh")));
        assert!(word_of("   ").is_none());
        assert!(word_of("te\0h").is_none());
    }

    #[test]
    fn a_learned_word_is_no_longer_misspelled_until_it_is_dropped() {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

        // SAFETY: the test thread becomes the apartment the checker is made in.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let word = "qzxwvkjseuoffice";
        match Platform::is_misspelled(word) {
            // an image without the checker or the UI language's dictionary says so
            Err(error) => assert!(error.contains("spell checker"), "{error}"),
            Ok(first) => {
                assert!(first);
                assert!(Platform::learn(word).unwrap());
                let learned = Platform::is_misspelled(word);
                let dropped = Platform::unlearn(word);
                assert!(!learned.unwrap());
                assert!(dropped.unwrap());
                assert!(Platform::is_misspelled(word).unwrap());
            }
        }
    }
}
