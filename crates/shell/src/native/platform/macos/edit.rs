//! Edit commands and trusted key events for the page, delivered the way AppKit delivers them:
//! an action sent to the web view, and `NSEvent` key events sent through the window.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::sel;
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSWindow,
};
use objc2_foundation::{NSPoint, NSString};

use super::page::{Surface, call_async, object};
use crate::native::page::{Done, EditCommand, KeyChord, Modifiers, PageError};

fn platform(message: &str) -> PageError {
    PageError::Platform(message.to_owned())
}

/// Make the web view the window's first responder, so editing goes to the page's focus.
pub(super) fn focus<'a>(surface: &Surface<'a>) -> Result<&'a NSWindow, PageError> {
    let window = surface
        .window
        .ok_or_else(|| platform("The web view is not in a window."))?;
    let responder: &NSResponder = surface.view;
    if window.makeFirstResponder(Some(responder)) {
        Ok(window)
    } else {
        Err(platform("The page did not accept keyboard focus."))
    }
}

fn action(command: EditCommand) -> (Sel, &'static str) {
    match command {
        EditCommand::Cut => (sel!(cut:), "cut"),
        EditCommand::Copy => (sel!(copy:), "copy"),
        EditCommand::Paste => (sel!(paste:), "paste"),
        EditCommand::PasteAndMatchStyle => (sel!(pasteAsPlainText:), "pasteAndMatchStyle"),
        EditCommand::SelectAll => (sel!(selectAll:), "selectAll"),
        EditCommand::Undo => (sel!(undo:), "undo"),
        EditCommand::Redo => (sel!(redo:), "redo"),
        EditCommand::Delete => (sel!(delete:), "delete"),
    }
}

/// Send the action to the web view after focusing it.
fn perform(
    surface: &Surface<'_>,
    target: &AnyObject,
    selector: Sel,
    name: &str,
) -> Result<(), PageError> {
    focus(surface)?;
    let app = NSApplication::sharedApplication(surface.mtm);
    // SAFETY: the target answers to the selector, which takes the sender as its only argument.
    if unsafe { app.sendAction_to_from(selector, Some(target), None) } {
        Ok(())
    } else {
        Err(PageError::Platform(format!(
            "The page did not perform {name}."
        )))
    }
}

/// Send the command's action to the web view. Undo and redo fall back to the page's own
/// editor when the web view has no such action; any other command it does not answer is
/// `Unsupported`, never a silent no-op.
pub(super) fn edit(surface: &Surface<'_>, command: EditCommand, done: Done<()>) {
    let (selector, name) = action(command);
    let target = object(surface.view);
    // SAFETY: `respondsToSelector:` is understood by every Objective-C object.
    let responds: bool = unsafe { msg_send![target, respondsToSelector: selector] };
    if responds {
        return done(perform(surface, target, selector, name));
    }
    let editing = match command {
        EditCommand::Undo => "undo",
        EditCommand::Redo => "redo",
        _ => return done(Err(PageError::Unsupported(name))),
    };
    if let Err(error) = focus(surface) {
        return done(Err(error));
    }
    let script = format!("document.execCommand('{editing}'); return 'done';");
    call_async(surface, &script, move |outcome| done(outcome.map(|_| ())));
}

/// macOS virtual key code of a `KeyboardEvent.code`.
fn key_code(code: &str) -> Option<u16> {
    let value = match code {
        "KeyA" => 0,
        "KeyS" => 1,
        "KeyD" => 2,
        "KeyF" => 3,
        "KeyH" => 4,
        "KeyG" => 5,
        "KeyZ" => 6,
        "KeyX" => 7,
        "KeyC" => 8,
        "KeyV" => 9,
        "KeyB" => 11,
        "KeyQ" => 12,
        "KeyW" => 13,
        "KeyE" => 14,
        "KeyR" => 15,
        "KeyY" => 16,
        "KeyT" => 17,
        "KeyO" => 31,
        "KeyU" => 32,
        "KeyI" => 34,
        "KeyP" => 35,
        "KeyL" => 37,
        "KeyJ" => 38,
        "KeyK" => 40,
        "KeyN" => 45,
        "KeyM" => 46,
        "Digit1" => 18,
        "Digit2" => 19,
        "Digit3" => 20,
        "Digit4" => 21,
        "Digit5" => 23,
        "Digit6" => 22,
        "Digit7" => 26,
        "Digit8" => 28,
        "Digit9" => 25,
        "Digit0" => 29,
        "Equal" => 24,
        "Minus" => 27,
        "BracketRight" => 30,
        "BracketLeft" => 33,
        "Quote" => 39,
        "Semicolon" => 41,
        "Backslash" => 42,
        "Comma" => 43,
        "Slash" => 44,
        "Period" => 47,
        "Backquote" => 50,
        "Enter" => 36,
        "NumpadEnter" => 76,
        "Tab" => 48,
        "Space" => 49,
        "Backspace" => 51,
        "Escape" => 53,
        "Delete" => 117,
        "Home" => 115,
        "End" => 119,
        "PageUp" => 116,
        "PageDown" => 121,
        "ArrowLeft" => 123,
        "ArrowRight" => 124,
        "ArrowDown" => 125,
        "ArrowUp" => 126,
        "F1" => 122,
        "F2" => 120,
        "F3" => 99,
        "F4" => 118,
        "F5" => 96,
        "F6" => 97,
        "F7" => 98,
        "F8" => 100,
        "F9" => 101,
        "F10" => 109,
        "F11" => 103,
        "F12" => 111,
        _ => return None,
    };
    Some(value)
}

/// The `characters` and `charactersIgnoringModifiers` an `NSEvent` carries for a chord.
fn characters(chord: &KeyChord) -> Option<(String, String)> {
    let named = match chord.key.as_str() {
        "Enter" => Some('\r'),
        "Tab" => Some('\t'),
        "Escape" => Some('\u{1b}'),
        "Backspace" => Some('\u{7f}'),
        "Delete" => Some('\u{f728}'),
        "ArrowUp" => Some('\u{f700}'),
        "ArrowDown" => Some('\u{f701}'),
        "ArrowLeft" => Some('\u{f702}'),
        "ArrowRight" => Some('\u{f703}'),
        "Home" => Some('\u{f729}'),
        "End" => Some('\u{f72b}'),
        "PageUp" => Some('\u{f72c}'),
        "PageDown" => Some('\u{f72d}'),
        other => other
            .strip_prefix('F')
            .and_then(|number| number.parse::<u32>().ok())
            .filter(|number| (1..=12).contains(number))
            .and_then(|number| char::from_u32(0xf703 + number)),
    };
    if let Some(named) = named {
        return Some((named.to_string(), named.to_string()));
    }
    let mut letters = chord.key.chars();
    let letter = letters.next()?;
    if letters.next().is_some() {
        return None;
    }
    let plain = if chord.modifiers.shift {
        letter.to_uppercase().collect::<String>()
    } else {
        letter.to_lowercase().collect::<String>()
    };
    let typed = if chord.modifiers.ctrl && letter.is_ascii_alphabetic() {
        char::from(letter.to_ascii_lowercase() as u8 & 0x1f).to_string()
    } else {
        plain.clone()
    };
    Some((typed, plain))
}

fn flags(modifiers: Modifiers) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    if modifiers.shift {
        flags |= NSEventModifierFlags::Shift;
    }
    if modifiers.ctrl {
        flags |= NSEventModifierFlags::Control;
    }
    if modifiers.alt {
        flags |= NSEventModifierFlags::Option;
    }
    if modifiers.meta {
        flags |= NSEventModifierFlags::Command;
    }
    flags
}

/// Seconds since boot, the clock `NSEvent` timestamps use.
fn uptime() -> f64 {
    // SAFETY: `NSProcessInfo.processInfo` and `systemUptime` are plain Foundation getters.
    unsafe {
        let info: Retained<AnyObject> = msg_send![objc2::class!(NSProcessInfo), processInfo];
        msg_send![&*info, systemUptime]
    }
}

/// Press and release the chord through the window, so the page sees trusted key events.
pub(super) fn key(surface: &Surface<'_>, chord: &KeyChord) -> Result<(), PageError> {
    let code = key_code(&chord.code)
        .ok_or_else(|| PageError::Invalid(format!("unsupported key code {}", chord.code)))?;
    let (typed, plain) = characters(chord)
        .ok_or_else(|| PageError::Invalid(format!("unsupported key {}", chord.key)))?;
    let window = focus(surface)?;
    let (typed, plain) = (NSString::from_str(&typed), NSString::from_str(&plain));
    let (flags, number, time) = (flags(chord.modifiers), window.windowNumber(), uptime());
    for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint { x: 0.0, y: 0.0 },
            flags,
            time,
            number,
            None,
            &typed,
            &plain,
            false,
            code,
        )
        .ok_or_else(|| platform("AppKit could not build the key event."))?;
        window.sendEvent(&event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(key: &str, code: &str, modifiers: Modifiers) -> KeyChord {
        KeyChord {
            key: key.to_owned(),
            code: code.to_owned(),
            modifiers,
        }
    }

    #[test]
    fn letters_and_named_keys_have_virtual_key_codes() {
        assert_eq!(key_code("KeyE"), Some(14));
        assert_eq!(key_code("Enter"), Some(36));
        assert_eq!(key_code("F12"), Some(111));
        assert_eq!(key_code("Nope"), None);
    }

    #[test]
    fn control_letters_carry_the_control_character() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let pair = |text: &str| Some((text.to_owned(), text.to_owned()));
        let expected = Some(("\u{5}".to_owned(), "e".to_owned()));
        assert_eq!(characters(&chord("e", "KeyE", ctrl)), expected);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(characters(&chord("e", "KeyE", shift)), pair("E"));
    }

    #[test]
    fn named_keys_use_the_function_key_private_use_characters() {
        let plain = Modifiers::default();
        let pair = |text: &str| Some((text.to_owned(), text.to_owned()));
        assert_eq!(
            characters(&chord("ArrowUp", "ArrowUp", plain)),
            pair("\u{f700}")
        );
        assert_eq!(characters(&chord("F1", "F1", plain)), pair("\u{f704}"));
        assert_eq!(characters(&chord("Unidentified", "KeyA", plain)), None);
    }
}
