//! Native key input on macOS: synthesized `NSEvent` key events handed to `NSApplication`.
//!
//! `-[NSApplication sendEvent:]` is the path a hardware key takes after the window server: a
//! Command key press goes to the main menu's key equivalents first (the real accelerator of
//! Save, Close, Quit), anything else to the key window, whose first responder is the editor's
//! `WKWebView`. WebKit then handles the event as trusted input, through the active input
//! source, so the Korean IME composes. All of it runs on the UI thread.
//!
//! Characters outside the US layout (`type` with Hangul, say) carry their text with key code 0:
//! the page receives the text, but not a meaningful `KeyboardEvent.code`. Hangul is what the
//! `ime` op is for.

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSWindow};
use objc2_foundation::{NSPoint, NSString};
use serde_json::{Value, json};
use shell_contract::runtime::Thread;
use tauri::Window;

use super::scenario::{Shortcut, TypedKey};
use crate::host::on_ui;

/// between two typed characters, so WebKit and the input method keep up in order
const KEY_GAP: Duration = Duration::from_millis(10);
/// after an input source change or before it is committed: the system answers asynchronously
const SETTLE: Duration = Duration::from_millis(300);

/// One key press: the virtual key code, the characters it types and the modifiers held
struct Stroke {
    code: u16,
    typed: String,
    plain: String,
    flags: NSEventModifierFlags,
}

/// The ANSI (US) virtual key code of an unshifted character
fn base_code(c: char) -> Option<u16> {
    Some(match c {
        'a' => 0,
        's' => 1,
        'd' => 2,
        'f' => 3,
        'h' => 4,
        'g' => 5,
        'z' => 6,
        'x' => 7,
        'c' => 8,
        'v' => 9,
        'b' => 11,
        'q' => 12,
        'w' => 13,
        'e' => 14,
        'r' => 15,
        'y' => 16,
        't' => 17,
        '1' => 18,
        '2' => 19,
        '3' => 20,
        '4' => 21,
        '6' => 22,
        '5' => 23,
        '=' => 24,
        '9' => 25,
        '7' => 26,
        '-' => 27,
        '8' => 28,
        '0' => 29,
        ']' => 30,
        'o' => 31,
        'u' => 32,
        '[' => 33,
        'i' => 34,
        'p' => 35,
        'l' => 37,
        'j' => 38,
        '\'' => 39,
        'k' => 40,
        ';' => 41,
        '\\' => 42,
        ',' => 43,
        '/' => 44,
        'n' => 45,
        'm' => 46,
        '.' => 47,
        ' ' => 49,
        '`' => 50,
        _ => return None,
    })
}

/// The key a shifted symbol is on the US layout
fn shifted_base(c: char) -> Option<char> {
    Some(match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    })
}

/// The key code and Shift state that type `c` on the US layout
fn ansi_key(c: char) -> Option<(u16, bool)> {
    if let Some(base) = shifted_base(c) {
        return base_code(base).map(|code| (code, true));
    }
    if c.is_ascii_uppercase() {
        return base_code(c.to_ascii_lowercase()).map(|code| (code, true));
    }
    base_code(c).map(|code| (code, false))
}

fn named(code: u16, text: &str) -> Stroke {
    Stroke {
        code,
        typed: text.to_owned(),
        plain: text.to_owned(),
        flags: NSEventModifierFlags::empty(),
    }
}

fn stroke_of(key: &TypedKey) -> Stroke {
    match key {
        TypedKey::Return => named(36, "\r"),
        TypedKey::Tab => named(48, "\t"),
        TypedKey::Escape => named(53, "\u{1b}"),
        TypedKey::Backspace => named(51, "\u{7f}"),
        TypedKey::Char(c) => {
            let text = c.to_string();
            let (code, shift) = ansi_key(*c).unwrap_or((0, false));
            Stroke {
                code,
                typed: text.clone(),
                plain: text,
                flags: if shift {
                    NSEventModifierFlags::Shift
                } else {
                    NSEventModifierFlags::empty()
                },
            }
        }
    }
}

/// Command (and Shift) with the letter of the accelerator
fn shortcut_stroke(shortcut: Shortcut) -> Stroke {
    let letter = shortcut.letter();
    let text = if shortcut.shift() {
        letter.to_ascii_uppercase()
    } else {
        letter
    }
    .to_string();
    let mut flags = NSEventModifierFlags::Command;
    if shortcut.shift() {
        flags |= NSEventModifierFlags::Shift;
    }
    Stroke {
        code: base_code(letter).unwrap_or(0),
        typed: text.clone(),
        plain: text,
        flags,
    }
}

fn ns_window(window: &Window) -> Result<&NSWindow, String> {
    let pointer = window
        .ns_window()
        .map_err(|error| error.to_string())?
        .cast::<NSWindow>();
    // SAFETY: Tauri returns the live window it owns; the reference is used on the UI thread,
    // inside the job that asked for it.
    unsafe { pointer.as_ref() }.ok_or_else(|| "The window has no native window yet.".to_owned())
}

/// Seconds since boot, the clock `NSEvent` timestamps use
fn uptime() -> f64 {
    // SAFETY: `NSProcessInfo.processInfo` and `systemUptime` are plain Foundation getters.
    unsafe {
        let info: Retained<AnyObject> = msg_send![objc2::class!(NSProcessInfo), processInfo];
        msg_send![&*info, systemUptime]
    }
}

/// Makes `window` the key window of the active application
fn activate(mtm: MainThreadMarker, window: &NSWindow) {
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: `activateIgnoringOtherApps:` takes the one BOOL; sent to the shared application.
    unsafe {
        let _: () = msg_send![&*app, activateIgnoringOtherApps: Bool::new(true)];
    }
    if !window.isKeyWindow() {
        window.makeKeyAndOrderFront(None);
    }
}

/// Press and release one key through the application, on the UI thread
fn post(window: &Window, stroke: &Stroke) -> Result<(), String> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "AppKit key events can only be sent on the UI thread.".to_owned())?;
    let native = ns_window(window)?;
    activate(mtm, native);
    let app = NSApplication::sharedApplication(mtm);
    let (typed, plain) = (
        NSString::from_str(&stroke.typed),
        NSString::from_str(&stroke.plain),
    );
    let (number, time) = (native.windowNumber(), uptime());
    for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint { x: 0.0, y: 0.0 },
            stroke.flags,
            time,
            number,
            None,
            &typed,
            &plain,
            false,
            stroke.code,
        )
        .ok_or_else(|| "AppKit could not build the key event.".to_owned())?;
        app.sendEvent(&event);
    }
    Ok(())
}

fn send(ui: &Arc<dyn Thread>, window: &Window, stroke: Stroke) -> Result<(), String> {
    let window = window.clone();
    on_ui(ui, move || post(&window, &stroke))
}

/// The OS window number, which `xcap` lists the window under
pub(super) fn window_id(ui: &Arc<dyn Thread>, window: &Window) -> Result<u32, String> {
    let window = window.clone();
    on_ui(ui, move || {
        let number = ns_window(&window)?.windowNumber();
        u32::try_from(number).map_err(|_| "The window number is out of range.".to_owned())
    })
}

pub(super) fn type_text(
    ui: &Arc<dyn Thread>,
    window: &Window,
    keys: &[TypedKey],
) -> Result<(), String> {
    for key in keys {
        send(ui, window, stroke_of(key))?;
        sleep(KEY_GAP);
    }
    Ok(())
}

pub(super) fn shortcut(
    ui: &Arc<dyn Thread>,
    window: &Window,
    shortcut: Shortcut,
) -> Result<(), String> {
    send(ui, window, shortcut_stroke(shortcut))
}

/// Selects the Korean 2-Set input source, types the raw keys so the OS IME composes, commits by
/// switching back to the source that was active, and leaves that source selected
pub(super) fn ime_ko_2set(
    ui: &Arc<dyn Thread>,
    window: &Window,
    keys: &str,
) -> Result<Value, String> {
    let previous =
        on_ui(ui, tis::current_id).ok_or("cannot read the current keyboard input source")?;
    on_ui(ui, || tis::select(tis::KOREAN_2SET))?;
    sleep(SETTLE);
    let active = on_ui(ui, tis::current_id);
    let typed = if active.as_deref() == Some(tis::KOREAN_2SET) {
        keys.chars().try_for_each(|c| {
            let result = send(ui, window, stroke_of(&TypedKey::Char(c)));
            sleep(KEY_GAP);
            result
        })
    } else {
        Err(format!(
            "the Korean 2-Set input source did not become active (current: {active:?})"
        ))
    };
    sleep(SETTLE);
    // leaving the input source commits the composition into the page
    let back = previous.clone();
    let restored = on_ui(ui, move || tis::select(&back));
    typed?;
    restored?;
    sleep(SETTLE);
    Ok(json!({ "source": "ko-2set", "previous": previous }))
}

/// Text Input Sources (HIToolbox, through the Carbon framework): called on the UI thread only
mod tis {
    use std::ffi::{CStr, c_char, c_void};

    /// `com.apple.inputmethod.Korean.2SetKorean`
    pub(super) const KOREAN_2SET: &str = "com.apple.inputmethod.Korean.2SetKorean";

    type CfRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: CfRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
        fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32)
        -> u8;
        fn CFRelease(object: CfRef);
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISPropertyInputSourceID: CfRef;
        fn TISCreateInputSourceList(properties: CfRef, include_all_installed: u8) -> CfRef;
        fn TISCopyCurrentKeyboardInputSource() -> CfRef;
        fn TISGetInputSourceProperty(source: CfRef, key: CfRef) -> CfRef;
        fn TISEnableInputSource(source: CfRef) -> i32;
        fn TISSelectInputSource(source: CfRef) -> i32;
    }

    fn source_id(source: CfRef) -> Option<String> {
        // SAFETY: `source` is a live TISInputSourceRef; the property is a borrowed CFString.
        let value = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
        if value.is_null() {
            return None;
        }
        let mut buffer = [0 as c_char; 256];
        // SAFETY: the buffer is writable for its whole length, which is passed as its size.
        let copied =
            unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) };
        if copied == 0 {
            return None;
        }
        // SAFETY: a successful copy NUL-terminates the buffer.
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        Some(text.to_string_lossy().into_owned())
    }

    /// The identifier of the current keyboard input source
    pub(super) fn current_id() -> Option<String> {
        // SAFETY: a "Copy" function returns a retained reference, released below.
        let source = unsafe { TISCopyCurrentKeyboardInputSource() };
        if source.is_null() {
            return None;
        }
        let id = source_id(source);
        // SAFETY: balances the copy above.
        unsafe { CFRelease(source) };
        id
    }

    /// Enables (an enabled source stays so) and selects the installed source named `id`
    pub(super) fn select(id: &str) -> Result<(), String> {
        // SAFETY: a null filter lists every installed source; the list is released below.
        let list = unsafe { TISCreateInputSourceList(std::ptr::null(), 1) };
        if list.is_null() {
            return Err("the system listed no input sources".into());
        }
        // SAFETY: `list` is a live CFArray.
        let count = unsafe { CFArrayGetCount(list) };
        // a mode (`...Korean.2SetKorean`) belongs to an input method (`...Korean`) that must be
        // enabled before the mode can be selected
        let parent = id.rsplit_once('.').map_or("", |(parent, _)| parent);
        let (mut parent_status, mut korean) = (None, Vec::new());
        let mut outcome = Err(String::new());
        for index in 0..count {
            // SAFETY: `index` is below the count; the element is borrowed from the list.
            let source = unsafe { CFArrayGetValueAtIndex(list, index) };
            let Some(found) = source_id(source) else {
                continue;
            };
            if found.contains("Korean") {
                korean.push(found.clone());
            }
            if found == parent {
                // SAFETY: `source` is a live input source of the list.
                parent_status = Some(unsafe { TISEnableInputSource(source) });
            }
        }
        for index in 0..count {
            // SAFETY: as above.
            let source = unsafe { CFArrayGetValueAtIndex(list, index) };
            if source_id(source).as_deref() != Some(id) {
                continue;
            }
            // SAFETY: `source` is a live input source of the list.
            let (enabled, status) =
                unsafe { (TISEnableInputSource(source), TISSelectInputSource(source)) };
            outcome = if status == 0 {
                Ok(())
            } else {
                Err(format!(
                    "selecting {id} failed with OSStatus {status} (enabling it: {enabled}, \
                     enabling {parent}: {parent_status:?}; installed Korean sources: {korean:?})"
                ))
            };
            break;
        }
        if matches!(&outcome, Err(text) if text.is_empty()) {
            outcome = Err(format!(
                "the input source {id} is not installed (installed Korean sources: {korean:?})"
            ));
        }
        // SAFETY: balances the list created above.
        unsafe { CFRelease(list) };
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_codes_follow_the_us_layout() {
        assert_eq!(ansi_key('g'), Some((5, false)));
        assert_eq!(ansi_key('G'), Some((5, true)));
        assert_eq!(ansi_key('-'), Some((27, false)));
        assert_eq!(ansi_key('!'), Some((18, true)));
        assert_eq!(ansi_key(':'), Some((41, true)));
        assert_eq!(ansi_key(' '), Some((49, false)));
        assert_eq!(ansi_key('\u{d55c}'), None);
    }

    #[test]
    fn the_keys_of_the_korean_two_set_test_phrase_have_codes() {
        let codes: Vec<u16> = "gksrmf"
            .chars()
            .map(|c| ansi_key(c).expect("a US key").0)
            .collect();
        assert_eq!(codes, [5, 40, 1, 15, 46, 3]);
    }

    #[test]
    fn a_shortcut_is_command_plus_its_letter() {
        let save = shortcut_stroke(Shortcut::Save);
        assert_eq!((save.code, save.typed.as_str()), (1, "s"));
        assert_eq!(save.flags, NSEventModifierFlags::Command);
        let save_as = shortcut_stroke(Shortcut::SaveAs);
        assert_eq!(save_as.typed, "S");
        assert_eq!(
            save_as.flags,
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift
        );
        assert_eq!(shortcut_stroke(Shortcut::Quit).code, 12);
    }

    #[test]
    fn control_characters_use_their_own_keys() {
        assert_eq!(stroke_of(&TypedKey::Return).code, 36);
        assert_eq!(stroke_of(&TypedKey::Tab).typed, "\t");
        assert_eq!(stroke_of(&TypedKey::Escape).code, 53);
        assert_eq!(stroke_of(&TypedKey::Backspace).code, 51);
    }
}
