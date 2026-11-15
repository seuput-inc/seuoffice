//! Regional settings, independent of the application's display language.

#[cfg(target_os = "macos")]
pub(crate) fn system_locale() -> Result<String, String> {
    use objc2_foundation::NSLocale;
    let locale = NSLocale::currentLocale();
    let identifier = locale.localeIdentifier();
    let language = NSLocale::canonicalLanguageIdentifierFromString(&identifier).to_string();
    if language.is_empty() {
        Err("the operating system returned an empty regional locale".to_owned())
    } else {
        Ok(language)
    }
}

#[cfg(windows)]
pub(crate) fn system_locale() -> Result<String, String> {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buffer = [0u16; 85];
    // SAFETY: the API receives the full writable slice and its checked length.
    let length = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if length <= 1 {
        return Err(format!("reading the regional locale: {}", std::io::Error::last_os_error()));
    }
    String::from_utf16(&buffer[..length as usize - 1]).map_err(|e| format!("invalid regional locale: {e}"))
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn system_locale() -> Result<String, String> {
    let locale = ["LC_ALL", "LC_TIME", "LANG"].into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "C".to_owned());
    if locale == "C" || locale == "POSIX" || locale.starts_with("C.") {
        return Ok("en-US".to_owned());
    }
    // The consumer chooses Gregorian dates from the region, not POSIX
    // calendar, encoding or script modifiers.
    Ok(locale.split('@').next().unwrap_or(&locale)
        .split('.').next().unwrap_or(&locale).replace('_', "-"))
}
