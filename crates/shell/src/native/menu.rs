//! What the application and context menus need from the native layer.
//!
//! - [`FocusCapture`]: the registered document of the view that has the keyboard focus when
//!   an application menu item is activated. Replaces `capture_native_menu_activation`.
//! - [`ipc_context_menu`]: the Rust ingress of the bridge's `contextmenu` forwarder. The
//!   page's right click is cancelled by the bridge and posted here with the document nonce;
//!   [`install`] receives it as a [`ContextMenuRequest`] with the spelling lookup done.
//!   Replaces `set_context_menu_handler`. Child frames carry no bridge, so a right click in
//!   one never reaches this ingress.
//! - edit commands, the cache-ignoring reload, the system spell checker and the
//!   misspelling replacement, which the menus run on a view.

use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};
use tauri::ipc::{InvokeBody, Request};
use tauri::{AppHandle, Manager, Webview};

use super::browser::BrowserIdentity;
use super::defer::is_ui_thread;
use super::document::DocumentContext;
use super::events::{NativeEvent, NativeEventKind, ViewEvent};
use super::ext::WebviewExt;
use super::page::{Done, EditCommand};
use super::platform::{Current, PageDriver, SpellOps};
use super::window::native_app_key_window_handle;
use super::{Native, redeem};

/// The view that last took the keyboard focus. Menus and popups can steal the OS focus from
/// the view without changing what the user is working in, so a blur does not clear it.
static LAST_FOCUSED: Mutex<Option<BrowserIdentity>> = Mutex::new(None);

/// Feed every browser's events here (from the sink that already sees them): the focus
/// events say which view of a window with several views is the one in use.
pub fn note_view_event(event: &NativeEvent) {
    if matches!(event.kind, NativeEventKind::View(ViewEvent::Focus)) {
        *LAST_FOCUSED
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(event.browser.clone());
    }
}

/// The document an application menu item was activated on.
#[derive(Clone)]
pub struct FocusCapture {
    origin: DocumentContext,
}

impl FocusCapture {
    /// Read the focused view's registered document. Call on the UI thread, inside the menu
    /// event. Fails closed: with several candidate views and no focus event for any, it names
    /// none.
    pub fn capture(app: &AppHandle) -> Result<Self, String> {
        if !is_ui_thread() {
            return Err("A menu focus capture must run on the UI thread.".into());
        }
        let native = app
            .try_state::<Native>()
            .ok_or("The native module is not installed.")?;
        let key = native_app_key_window_handle(app)?
            .ok_or("No application window has the keyboard focus.")?;
        let in_key_window = |label: &str| {
            app.get_webview(label)
                .is_some_and(|webview| webview.window().label() == key.label())
        };
        let tracked = {
            let mut last = LAST_FOCUSED
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if last.as_ref().is_some_and(|browser| !browser.is_open()) {
                *last = None;
            }
            last.clone()
                .filter(|browser| in_key_window(browser.label()))
        };
        let browser = match tracked {
            Some(browser) => browser,
            None => {
                let mut candidates = app
                    .webviews()
                    .into_keys()
                    .filter(|label| in_key_window(label))
                    .filter_map(|label| native.browser(&label))
                    .filter(|browser| browser.registered_context().is_some());
                match (candidates.next(), candidates.next()) {
                    (Some(only), None) => only,
                    (None, _) => {
                        return Err("The focused window has no registered document.".into());
                    }
                    _ => return Err("The focused view of the window is unknown.".into()),
                }
            }
        };
        let origin = browser
            .registered_context()
            .ok_or("The focused view has no registered document.")?;
        Ok(Self { origin })
    }

    /// The registered document the activation happened on.
    pub fn origin(&self) -> &DocumentContext {
        &self.origin
    }

    /// False once that document was revoked.
    pub fn is_live(&self) -> bool {
        self.origin.is_live()
    }
}

/// Which editing commands the field under the click allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditFlags {
    pub can_cut: bool,
    pub can_copy: bool,
    pub can_paste: bool,
    pub can_select_all: bool,
}

/// A right click as the menu template reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextMenuParams {
    pub is_editable: bool,
    pub selection_text: String,
    pub misspelled_word: String,
    pub dictionary_suggestions: Vec<String>,
    pub edit_flags: EditFlags,
    pub media_type: Option<String>,
    pub src_url: Option<String>,
    /// The click in CSS pixels of the page's viewport.
    pub x: f64,
    pub y: f64,
}

/// A right click of an authenticated document, with its spelling looked up.
pub struct ContextMenuRequest {
    /// The webview the click arrived on (routing only).
    pub label: String,
    /// The document the bridge registered; the receiver still authorizes it against its view.
    pub origin: DocumentContext,
    pub params: ContextMenuParams,
}

type Handler = Box<dyn Fn(ContextMenuRequest) + Send + Sync>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Receive every authenticated right click, on the UI thread. Once per process.
pub fn install(handler: impl Fn(ContextMenuRequest) + Send + Sync + 'static) -> Result<(), String> {
    HANDLER
        .set(Box::new(handler))
        .map_err(|_| "The context menu handler is already installed.".to_owned())
}

/// The longest selection the menu reads, as Chromium's own context menu params cut it.
const SELECTION_MAX_CHARS: usize = 1024;
const SRC_URL_MAX_CHARS: usize = 8192;
/// A misspelled-word candidate is one word.
const WORD_MAX_CHARS: usize = 64;
const MAX_SUGGESTIONS: usize = 5;

/// The bridge's `contextmenu` forwarder posts `{ isEditable, selectionText, srcUrl, mediaType,
/// x, y, editFlags: { canCut, canCopy, canPaste, canSelectAll }, word }` as JSON with the
/// document nonce header. `word` is the word under the click in an editable field, if any;
/// the click is in CSS pixels of the viewport.
#[tauri::command]
pub async fn ipc_context_menu(webview: Webview, request: Request<'_>) -> Result<(), String> {
    let origin = redeem(&webview, request.headers()).ok_or(crate::ipc::NOT_READY)?;
    let payload = match request.body() {
        InvokeBody::Json(value) => Payload::parse(value)?,
        InvokeBody::Raw(bytes) => {
            let value = serde_json::from_slice(bytes)
                .map_err(|error| format!("Invalid context menu: {error}"))?;
            Payload::parse(&value)?
        }
    };
    if HANDLER.get().is_none() {
        return Err("The context menu is not installed.".into());
    }
    let label = webview.label().to_owned();
    // The system spell checker belongs to the UI thread.
    webview
        .app_handle()
        .run_on_main_thread(move || {
            let spelling = payload.spelling();
            let params = payload.into_params(spelling);
            if let Some(handler) = HANDLER.get() {
                handler(ContextMenuRequest {
                    label,
                    origin,
                    params,
                });
            }
        })
        .map_err(|error| error.to_string())
}

/// The click as the page sent it, validated.
struct Payload {
    is_editable: bool,
    selection_text: String,
    src_url: Option<String>,
    media_type: Option<String>,
    x: f64,
    y: f64,
    edit_flags: EditFlags,
    word: Option<String>,
}

impl Payload {
    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("Invalid context menu: not an object.")?;
        let flags = match object.get("editFlags") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(flags)) => flags.clone(),
            Some(_) => return Err("Invalid context menu: editFlags is not an object.".into()),
        };
        let selection_text = text(object, "selectionText")?.unwrap_or_default();
        Ok(Self {
            is_editable: flag(object, "isEditable")?,
            selection_text: selection_text.chars().take(SELECTION_MAX_CHARS).collect(),
            src_url: bounded(text(object, "srcUrl")?, "srcUrl", SRC_URL_MAX_CHARS)?,
            media_type: text(object, "mediaType")?,
            x: coordinate(object, "x")?,
            y: coordinate(object, "y")?,
            edit_flags: EditFlags {
                can_cut: flag(&flags, "canCut")?,
                can_copy: flag(&flags, "canCopy")?,
                can_paste: flag(&flags, "canPaste")?,
                can_select_all: flag(&flags, "canSelectAll")?,
            },
            word: text(object, "word")?.filter(|word| {
                word.chars().count() <= WORD_MAX_CHARS && !word.chars().any(char::is_whitespace)
            }),
        })
    }

    /// The word under the click when it is misspelled, with the checker's replacements.
    fn spelling(&self) -> Option<(String, Vec<String>)> {
        let word = self
            .word
            .as_deref()
            .filter(|word| self.is_editable && !word.is_empty())?;
        match Current::is_misspelled(word) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                eprintln!("shell: the spell checker: {error}");
                return None;
            }
        }
        match Current::suggestions(word) {
            Ok(mut suggestions) => {
                suggestions.truncate(MAX_SUGGESTIONS);
                Some((word.to_owned(), suggestions))
            }
            Err(error) => {
                eprintln!("shell: the spell checker: {error}");
                None
            }
        }
    }

    fn into_params(self, spelling: Option<(String, Vec<String>)>) -> ContextMenuParams {
        let (misspelled_word, dictionary_suggestions) = spelling.unwrap_or_default();
        ContextMenuParams {
            is_editable: self.is_editable,
            selection_text: self.selection_text,
            misspelled_word,
            dictionary_suggestions,
            edit_flags: self.edit_flags,
            media_type: self.media_type,
            src_url: self.src_url,
            x: self.x,
            y: self.y,
        }
    }
}

fn flag(object: &Map<String, Value>, name: &str) -> Result<bool, String> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("Invalid context menu: {name} is not a boolean.")),
    }
}

fn text(object: &Map<String, Value>, name: &str) -> Result<Option<String>, String> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("Invalid context menu: {name} is not a string.")),
    }
}

fn bounded(value: Option<String>, name: &str, max: usize) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.chars().count() > max => {
            Err(format!("Invalid context menu: {name} is too long."))
        }
        other => Ok(other),
    }
}

fn coordinate(object: &Map<String, Value>, name: &str) -> Result<f64, String> {
    object
        .get(name)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("Invalid context menu: {name} is not a number."))
}

/// Run an editing command on the view's focused frame. A failure is reported, not returned:
/// the menu has nobody to hand it to.
pub fn edit(webview: &Webview, command: EditCommand) {
    match webview.page() {
        Ok(page) => page.edit(command, report(format!("{command:?}"))),
        Err(error) => eprintln!("shell: {command:?}: {error}"),
    }
}

fn report<T: Send + 'static>(what: String) -> Done<T> {
    Box::new(move |result| {
        if let Err(error) = result {
            eprintln!("shell: {what}: {error}");
        }
    })
}

/// Reload the page; with `ignore_cache` the cache is bypassed (macOS `reloadFromOrigin`,
/// Windows CDP `Page.reload`). Call on the UI thread.
pub fn reload(webview: &Webview, ignore_cache: bool) -> Result<(), String> {
    if ignore_cache {
        Current::reload_ignoring_cache(webview)
    } else {
        webview.reload().map_err(|error| error.to_string())
    }
}

/// Add a word to the system user dictionary, shared by every view and kept across runs.
/// `Ok(false)` where the platform has no dictionary to add to (Windows). UI thread.
pub fn learn_word(word: &str) -> Result<bool, String> {
    Current::learn(word)
}

/// Take a word out of the system user dictionary. `Ok(false)` where the platform cannot.
pub fn unlearn_word(word: &str) -> Result<bool, String> {
    Current::unlearn(word)
}

/// The script that replaces the misspelled word of a right click, run in the page: the word
/// is replaced only where it is still the selection or the word at the click, so a page that
/// changed meanwhile is never edited at a stale place. It answers `true` once replaced.
pub fn replace_misspelling_script(misspelled: &str, replacement: &str, x: f64, y: f64) -> String {
    format!(
        "(() => {{
  const was = {was};
  const now = {now};
  const wordAt = (text, at) => {{
    for (const m of text.matchAll(/[\\p{{L}}\\p{{N}}'\u{2019}]+/gu)) {{
      if (m.index <= at && at <= m.index + m[0].length) return [m.index, m.index + m[0].length];
    }}
    return null;
  }};
  const field = document.activeElement;
  if (field && (field.tagName === 'TEXTAREA' || field.tagName === 'INPUT')
      && typeof field.selectionStart === 'number') {{
    let [from, to] = [field.selectionStart, field.selectionEnd];
    if (from === to) {{
      const span = wordAt(field.value, from);
      if (!span) return false;
      [from, to] = span;
    }}
    if (field.value.slice(from, to) !== was) return false;
    field.setSelectionRange(from, to);
    return document.execCommand('insertText', false, now);
  }}
  const selection = getSelection();
  if (!selection) return false;
  if (selection.isCollapsed || selection.toString() !== was) {{
    const range = document.caretRangeFromPoint({x}, {y});
    const node = range && range.startContainer;
    if (!node || node.nodeType !== Node.TEXT_NODE) return false;
    const span = wordAt(node.data, range.startOffset);
    if (!span || node.data.slice(span[0], span[1]) !== was) return false;
    selection.setBaseAndExtent(node, span[0], node, span[1]);
  }}
  return document.execCommand('insertText', false, now);
}})()",
        was = Value::String(misspelled.to_owned()),
        now = Value::String(replacement.to_owned()),
    )
}

/// Replace the misspelled word of the right click on the view's page.
pub fn replace_misspelling(webview: &Webview, misspelled: &str, replacement: &str, x: f64, y: f64) {
    let page = match webview.page() {
        Ok(page) => page,
        Err(error) => return eprintln!("shell: replacing a misspelling: {error}"),
    };
    let script = replace_misspelling_script(misspelled, replacement, x, y);
    page.evaluate(
        &script,
        false,
        None,
        Box::new(|result| match result {
            Ok(Value::Bool(true)) => {}
            Ok(_) => eprintln!("shell: the misspelled word is no longer where it was clicked"),
            Err(error) => eprintln!("shell: replacing a misspelling: {error}"),
        }),
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn click() -> Value {
        json!({
            "isEditable": true,
            "selectionText": "teh",
            "srcUrl": null,
            "mediaType": "none",
            "x": 12.5,
            "y": 40,
            "editFlags": { "canCut": true, "canPaste": true },
            "word": "teh",
        })
    }

    #[test]
    fn a_click_keeps_what_the_menu_reads() {
        let payload = Payload::parse(&click()).unwrap();
        let params = payload.into_params(Some(("teh".into(), vec!["the".into()])));
        assert!(params.is_editable);
        assert_eq!(params.selection_text, "teh");
        assert_eq!(params.misspelled_word, "teh");
        assert_eq!(params.dictionary_suggestions, ["the"]);
        assert_eq!(params.media_type.as_deref(), Some("none"));
        assert_eq!(params.src_url, None);
        assert_eq!((params.x, params.y), (12.5, 40.0));
        assert_eq!(
            params.edit_flags,
            EditFlags {
                can_cut: true,
                can_copy: false,
                can_paste: true,
                can_select_all: false
            }
        );
    }

    #[test]
    fn a_click_without_spelling_has_no_misspelled_word() {
        let params = Payload::parse(&click()).unwrap().into_params(None);
        assert_eq!(params.misspelled_word, "");
        assert!(params.dictionary_suggestions.is_empty());
    }

    #[test]
    fn a_click_with_a_wrong_type_or_no_position_is_refused() {
        let mut value = click();
        value["isEditable"] = json!("yes");
        assert!(Payload::parse(&value).is_err());
        let mut value = click();
        value.as_object_mut().unwrap().remove("y");
        assert!(Payload::parse(&value).is_err());
        let mut value = click();
        value["x"] = json!("3");
        assert!(Payload::parse(&value).is_err());
        assert!(Payload::parse(&json!([1])).is_err());
    }

    #[test]
    fn long_selections_are_cut_and_long_urls_are_refused() {
        let mut value = click();
        value["selectionText"] = json!("a".repeat(SELECTION_MAX_CHARS + 50));
        let payload = Payload::parse(&value).unwrap();
        assert_eq!(payload.selection_text.chars().count(), SELECTION_MAX_CHARS);
        let mut value = click();
        value["srcUrl"] = json!("u".repeat(SRC_URL_MAX_CHARS + 1));
        assert!(Payload::parse(&value).is_err());
    }

    #[test]
    fn only_a_single_short_word_is_a_misspelling_candidate() {
        let mut value = click();
        value["word"] = json!("two words");
        assert_eq!(Payload::parse(&value).unwrap().word, None);
        value["word"] = json!("w".repeat(WORD_MAX_CHARS + 1));
        assert_eq!(Payload::parse(&value).unwrap().word, None);
        value["word"] = json!("don't");
        assert_eq!(
            Payload::parse(&value).unwrap().word.as_deref(),
            Some("don't")
        );
    }

    #[test]
    fn the_replacement_script_quotes_both_words_and_carries_the_click() {
        let script = replace_misspelling_script("it's \"bad\"", "line\nbreak", 12.5, 40.0);
        assert!(script.contains("const was = \"it's \\\"bad\\\"\";"));
        assert!(script.contains("const now = \"line\\nbreak\";"));
        assert!(script.contains("document.caretRangeFromPoint(12.5, 40)"));
        assert!(script.starts_with("(() => {"));
        assert!(script.ends_with("})()"));
    }
}
