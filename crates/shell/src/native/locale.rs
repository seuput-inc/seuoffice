//! The operating system's language list.
//!
//! Electron reported the language Chromium runs the app in (`app.getLocale()`, the first
//! `intl.accept_languages` entry). On the system webviews that language is the OS's own
//! preferred list, which this module reads.

/// The user's preferred languages as BCP 47 tags, most preferred first (`ko-KR`, `en-US`).
#[cfg(target_os = "macos")]
pub fn languages() -> Result<Vec<String>, String> {
    use objc2_foundation::NSLocale;

    let preferred = NSLocale::preferredLanguages().to_vec();
    Ok(preferred.iter().map(|language| language.to_string()).collect())
}

/// The user's preferred languages as BCP 47 tags, most preferred first (`ko-KR`, `en-US`).
#[cfg(windows)]
pub fn languages() -> Result<Vec<String>, String> {
    use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    use windows::core::PWSTR;

    let mut count = 0u32;
    let mut length = 0u32;
    // SAFETY: no buffer asks for the required length in UTF-16 units, terminators included.
    let sized =
        unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut length) };
    sized.map_err(|error| format!("reading the preferred UI languages: {error}"))?;
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: the buffer is `length` writable units, as the first call required.
    let read = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
    };
    read.map_err(|error| format!("reading the preferred UI languages: {error}"))?;
    // A multi-string: each tag ends with a NUL and the list with a second one.
    Ok(buffer
        .split(|unit| *unit == 0)
        .filter(|tag| !tag.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// The first preferred language, the app's locale.
pub fn first_language() -> Option<String> {
    match languages() {
        Ok(languages) => languages.into_iter().next(),
        Err(error) => {
            eprintln!("shell: the operating system's language is unavailable: {error}");
            None
        }
    }
}
