//! Native key input on Windows: `SendInput` to the foreground app window.
//!
//! The events enter the system input queue, so the window that is foreground gets them as it
//! gets the keyboard's: the WebView2 child with focus sees trusted `keydown`/`keypress`/`keyup`,
//! the Korean IME composes, and accelerators reach the window's menu. The window is brought to
//! the foreground first; Windows may refuse that to a process that did not just receive input,
//! which is reported as an error rather than typed into whatever is in front.
//!
//! Characters with a key on the current layout are sent as virtual keys; the others (Hangul in
//! `type`, say) as Unicode packets, which carry the text but no key code. The `ime` op always
//! sends virtual keys, from the letter's own VK, never through the layout tables.
//!
//! The Korean IME must be installed in the Windows image (the Korean language pack with its
//! keyboard); without it `ime` cannot run and says so.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shell_contract::runtime::Thread;
use tauri::Window;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::Ime::{
    IME_CMODE_HANGUL, IME_CONVERSION_MODE, IME_SENTENCE_MODE, ImmGetContext,
    ImmGetConversionStatus, ImmReleaseContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, KLF_NOTELLSHELL, LoadKeyboardLayoutW, MAPVK_VK_TO_VSC,
    MapVirtualKeyW, SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_ESCAPE, VK_F4, VK_HANGUL,
    VK_MENU, VK_RETURN, VK_SHIFT, VK_TAB, VkKeyScanW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, IsIconic,
    PostMessageW, SW_RESTORE, SetForegroundWindow, ShowWindow, WM_INPUTLANGCHANGEREQUEST,
};
use windows::core::w;

use super::scenario::{Shortcut, TypedKey};

/// between two typed characters
const KEY_GAP: Duration = Duration::from_millis(10);
/// after a layout or IME mode change: the system answers asynchronously
const SETTLE: Duration = Duration::from_millis(300);
const FOREGROUND_TRIES: u32 = 20;
const FOREGROUND_WAIT: Duration = Duration::from_millis(100);
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(5);
/// the language id of Korean, the low word of its keyboard layout handle
const KOREAN_LANGID: usize = 0x0412;

fn hwnd_of(window: &Window) -> Result<HWND, String> {
    let raw = window.hwnd().map_err(|error| error.to_string())?;
    // Tauri's `HWND` may come from another `windows` version; only the value crosses over.
    Ok(HWND(raw.0 as usize as *mut c_void))
}

/// The OS window handle, which `xcap` lists the window under
pub(super) fn window_id(_ui: &Arc<dyn Thread>, window: &Window) -> Result<u32, String> {
    Ok(hwnd_of(window)?.0 as usize as u32)
}

/// Makes the window the foreground window, or says it could not
fn foreground(hwnd: HWND) -> Result<(), String> {
    // SAFETY: both calls take a window handle of this process and change its state only.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    for _ in 0..FOREGROUND_TRIES {
        // SAFETY: reads the foreground window and asks for it to change.
        let done = unsafe {
            if GetForegroundWindow() == hwnd {
                true
            } else {
                let _ = SetForegroundWindow(hwnd);
                false
            }
        };
        if done {
            return Ok(());
        }
        sleep(FOREGROUND_WAIT);
    }
    Err("Windows did not let the app window become the foreground window.".into())
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    // SAFETY: a plain lookup of the scan code of a virtual key.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) } as u16;
    keyboard(
        vk,
        scan,
        if up {
            KEYEVENTF_KEYUP
        } else {
            KEYBD_EVENT_FLAGS(0)
        },
    )
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard(VIRTUAL_KEY(0), unit, flags)
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    // SAFETY: `inputs` is a valid slice of INPUT structures and the size is theirs.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(format!(
            "SendInput accepted {sent} of {} events (another process holds the input desktop, or UIPI blocks it)",
            inputs.len()
        ))
    }
}

/// One key down and up, with Shift held around it when asked
fn press(vk: VIRTUAL_KEY, shift: bool) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(4);
    if shift {
        inputs.push(key_input(VK_SHIFT, false));
    }
    inputs.push(key_input(vk, false));
    inputs.push(key_input(vk, true));
    if shift {
        inputs.push(key_input(VK_SHIFT, true));
    }
    inputs
}

/// The events that type `c`: its virtual key on the current layout, else Unicode packets
fn char_inputs(c: char) -> Vec<INPUT> {
    if c.is_ascii() {
        // SAFETY: a lookup in the calling thread's keyboard layout.
        let scan = unsafe { VkKeyScanW(u16::from(c as u8)) };
        let (vk, state) = ((scan & 0xff) as u16, ((scan >> 8) & 0xff) as u8);
        // no key, or one that needs Ctrl or Alt (AltGr) to type the character
        if scan != -1 && state & 0b110 == 0 {
            return press(VIRTUAL_KEY(vk), state & 1 != 0);
        }
    }
    let mut units = [0u16; 2];
    c.encode_utf16(&mut units)
        .iter()
        .flat_map(|unit| [unicode_input(*unit, false), unicode_input(*unit, true)])
        .collect()
}

fn typed_inputs(key: &TypedKey) -> Vec<INPUT> {
    match key {
        TypedKey::Return => press(VK_RETURN, false),
        TypedKey::Tab => press(VK_TAB, false),
        TypedKey::Escape => press(VK_ESCAPE, false),
        TypedKey::Backspace => press(VK_BACK, false),
        TypedKey::Char(c) => char_inputs(*c),
    }
}

/// Ctrl and the letter, or Alt+F4 for the quit that closes the window
fn shortcut_inputs(shortcut: Shortcut) -> Vec<INPUT> {
    if shortcut == Shortcut::Quit {
        return vec![
            key_input(VK_MENU, false),
            key_input(VK_F4, false),
            key_input(VK_F4, true),
            key_input(VK_MENU, true),
        ];
    }
    // the virtual key of a letter is its upper case ASCII code
    let letter = VIRTUAL_KEY(u16::from(shortcut.letter().to_ascii_uppercase() as u8));
    let mut inputs = vec![key_input(VK_CONTROL, false)];
    if shortcut.shift() {
        inputs.push(key_input(VK_SHIFT, false));
    }
    inputs.push(key_input(letter, false));
    inputs.push(key_input(letter, true));
    if shortcut.shift() {
        inputs.push(key_input(VK_SHIFT, true));
    }
    inputs.push(key_input(VK_CONTROL, true));
    inputs
}

pub(super) fn type_text(
    _ui: &Arc<dyn Thread>,
    window: &Window,
    keys: &[TypedKey],
) -> Result<(), String> {
    foreground(hwnd_of(window)?)?;
    for key in keys {
        send(&typed_inputs(key))?;
        sleep(KEY_GAP);
    }
    Ok(())
}

pub(super) fn shortcut(
    _ui: &Arc<dyn Thread>,
    window: &Window,
    shortcut: Shortcut,
) -> Result<(), String> {
    foreground(hwnd_of(window)?)?;
    send(&shortcut_inputs(shortcut))
}

/// The events of a raw key for the IME: the letter's own virtual key, Shift for a capital
fn ime_inputs(c: char) -> Result<Vec<INPUT>, String> {
    if !c.is_ascii_alphabetic() {
        return Err(format!("{c:?} is not a letter key"));
    }
    let vk = VIRTUAL_KEY(u16::from(c.to_ascii_uppercase() as u8));
    Ok(press(vk, c.is_ascii_uppercase()))
}

fn layout_language(layout: HKL) -> usize {
    layout.0 as usize & 0xffff
}

/// Asks the window's thread to switch to `layout` and waits until it did
fn request_layout(hwnd: HWND, thread: u32, layout: HKL) -> Result<(), String> {
    // SAFETY: posts the documented input language change request to a window of this process.
    unsafe {
        PostMessageW(
            Some(hwnd),
            WM_INPUTLANGCHANGEREQUEST,
            WPARAM(0),
            LPARAM(layout.0 as isize),
        )
    }
    .map_err(|error| format!("cannot request the keyboard layout: {error}"))?;
    let deadline = Instant::now() + LAYOUT_TIMEOUT;
    loop {
        // SAFETY: reads the keyboard layout of a thread id.
        let current = unsafe { GetKeyboardLayout(thread) };
        if layout_language(current) == layout_language(layout) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("the window's thread did not switch the keyboard layout".into());
        }
        sleep(Duration::from_millis(50));
    }
}

/// Whether the IME of the focused window is in Hangul mode; `None` when it cannot tell
fn hangul_mode() -> Option<bool> {
    // SAFETY: reads the foreground window and its thread's GUI state into a local structure.
    unsafe {
        let foreground = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(foreground, None);
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(thread, &mut info).ok()?;
        let focus = if info.hwndFocus.0.is_null() {
            foreground
        } else {
            info.hwndFocus
        };
        let context = ImmGetContext(focus);
        if context.0.is_null() {
            return None;
        }
        let mut conversion = IME_CONVERSION_MODE(0);
        let mut sentence = IME_SENTENCE_MODE(0);
        let known =
            ImmGetConversionStatus(context, Some(&mut conversion), Some(&mut sentence)).as_bool();
        let _ = ImmReleaseContext(focus, context);
        known.then_some(conversion.0 & IME_CMODE_HANGUL.0 != 0)
    }
}

/// One press of the Hangul key, which toggles the Korean IME between Hangul and alphanumeric
fn toggle_hangul() -> Result<(), String> {
    send(&press(VK_HANGUL, false))
}

/// Switches the window to the Korean keyboard layout, puts its IME in Hangul mode, types the
/// raw keys so the IME composes, then commits by toggling Hangul mode off and restores the
/// layout that was active
pub(super) fn ime_ko_2set(
    _ui: &Arc<dyn Thread>,
    window: &Window,
    keys: &str,
) -> Result<Value, String> {
    let hwnd = hwnd_of(window)?;
    foreground(hwnd)?;
    // SAFETY: reads the thread id and keyboard layout of this process's window.
    let (thread, previous) = unsafe {
        let thread = GetWindowThreadProcessId(hwnd, None);
        (thread, GetKeyboardLayout(thread))
    };
    // SAFETY: loads the Korean keyboard layout into this process without activating it here.
    let korean = unsafe { LoadKeyboardLayoutW(w!("00000412"), KLF_NOTELLSHELL) }.map_err(
        |error| format!("the Korean keyboard layout is not installed (Korean language pack): {error}"),
    )?;
    if layout_language(korean) != KOREAN_LANGID {
        return Err("Windows did not load the Korean keyboard layout".into());
    }
    request_layout(hwnd, thread, korean)?;
    sleep(SETTLE);
    // a freshly activated Korean IME may start in alphanumeric mode: toggle unless Hangul mode
    // is confirmed
    let started_in_hangul = hangul_mode();
    if started_in_hangul != Some(true) {
        toggle_hangul()?;
        sleep(SETTLE);
    }
    let typed = keys.chars().try_for_each(|c| {
        let result = send(&ime_inputs(c)?);
        sleep(KEY_GAP);
        result
    });
    sleep(SETTLE);
    // leaving Hangul mode commits the composition
    let committed = toggle_hangul();
    sleep(SETTLE);
    let restored = request_layout(hwnd, thread, previous);
    typed?;
    committed?;
    restored?;
    Ok(json!({
        "source": "ko-2set",
        "hangulModeBefore": started_in_hangul,
        "layout": format!("{:08x}", korean.0 as usize),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(inputs: &[INPUT]) -> Vec<(u16, bool)> {
        inputs
            .iter()
            .map(|input| {
                // SAFETY: every INPUT built here is a keyboard event.
                let ki = unsafe { input.Anonymous.ki };
                (ki.wVk.0, ki.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0)
            })
            .collect()
    }

    #[test]
    fn control_s_is_ctrl_down_s_down_s_up_ctrl_up() {
        assert_eq!(
            keys(&shortcut_inputs(Shortcut::Save)),
            [
                (VK_CONTROL.0, false),
                (u16::from(b'S'), false),
                (u16::from(b'S'), true),
                (VK_CONTROL.0, true),
            ]
        );
    }

    #[test]
    fn save_as_holds_shift_inside_ctrl() {
        let pressed = keys(&shortcut_inputs(Shortcut::SaveAs));
        assert_eq!(pressed.len(), 6);
        assert_eq!(pressed[1], (VK_SHIFT.0, false));
        assert_eq!(pressed[4], (VK_SHIFT.0, true));
    }

    #[test]
    fn quit_is_alt_f4() {
        assert_eq!(
            keys(&shortcut_inputs(Shortcut::Quit)),
            [
                (VK_MENU.0, false),
                (VK_F4.0, false),
                (VK_F4.0, true),
                (VK_MENU.0, true),
            ]
        );
    }

    #[test]
    fn ime_keys_are_letters_only() {
        assert_eq!(
            keys(&ime_inputs('g').unwrap()),
            [(u16::from(b'G'), false), (u16::from(b'G'), true)]
        );
        assert_eq!(ime_inputs('R').unwrap().len(), 4);
        assert!(ime_inputs('1').is_err());
    }

    #[test]
    fn hangul_text_falls_back_to_unicode_packets() {
        let inputs = char_inputs('\u{d55c}');
        assert_eq!(inputs.len(), 2);
        // SAFETY: built by `unicode_input`.
        let first = unsafe { inputs[0].Anonymous.ki };
        assert_eq!(first.wScan, 0xd55c);
        assert_eq!(first.wVk.0, 0);
    }
}
