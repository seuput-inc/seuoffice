//! `webContents` of a view (the shared host `Contents`): capture, printToPDF,
//! zoom, focus and closing the window. Capture and PDF go through the Chrome
//! DevTools protocol of the view's browser (`Page.captureScreenshot`,
//! `Page.printToPDF`), as the prototype's PDF did.
//!
//! A CDP call blocks its caller until the browser answers; the browser
//! answers on the UI thread, which a core-thread caller does not hold.

mod owned;
pub(crate) use owned::Target as OwnedPrintTarget;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use shell_contract::hosts::Contents;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::runtime::Views;
use shell_contract::value::IpcValue;
use shell_contract::{ContentsHost, Hosts};
use tauri::Webview;
use tauri_runtime_cef::{DevToolsProtocol, WebviewCefExt, allocate_devtools_message_id};

use super::HostContext;
use super::window::Windows;
use crate::views::ViewTable;

/// a print of a large document can take a while
const CDP_TIMEOUT: Duration = Duration::from_secs(90);

type Answer = Result<(bool, Vec<u8>), String>;

/// CDP results waiting for their caller, by request id. The ids come from one
/// process-wide allocator, so one table serves every webview.
#[derive(Default)]
struct Pending(Mutex<HashMap<i32, PendingCall>>);

struct PendingCall {
    sender: mpsc::SyncSender<Answer>,
    owner: Option<owned::Owner>,
}

impl Pending {
    fn complete(&self, protocol: DevToolsProtocol) {
        if let DevToolsProtocol::MethodResult {
            message_id,
            success,
            result,
        } = protocol {
            let call = { self.0.lock().unwrap().remove(&message_id) };
            if let Some(call) = call {
                if let Some(owner) = call.owner {
                    let sent = owner.effect(|| call.sender.try_send(Ok((success, result))));
                    if let Err(error) = sent { let _ = call.sender.try_send(Err(error)); }
                } else { let _ = call.sender.try_send(Ok((success, result))); }
            }
        }
    }
}

static PENDING: LazyLock<Arc<Pending>> = LazyLock::new(Arc::default);

/// Labels of the webviews whose browser already hands its results to [`PENDING`]
static OBSERVED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// A webview that had CDP calls is gone: a later webview of the same label
/// is observed afresh. Labels of the shell's views are never reused; a
/// window made outside the view table (a print window) calls this when it closes.
#[allow(dead_code)] // called by batch M's print host
pub(crate) fn forget_webview(label: &str) {
    super::d2::hwp_host::keyboard::forget(label);
    OBSERVED.lock().unwrap().remove(label);
}

/// One Chrome DevTools protocol call on `webview`'s browser, answered with
/// the parsed `result`. Blocks until the browser answers, so not from the UI
/// thread. The webview's label must be unique among the webviews that use it.
pub(crate) fn cdp(webview: &Webview, method: &str, params: Value) -> Result<Value, String> {
    cdp_inner(webview, method, params, None)
}

pub(crate) fn cdp_owned(
    webview: &Webview, method: &str, params: Value,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Value, String> {
    let owner = owned::Owner { authority: owned::Authority::Pair { origin: origin.clone(), destination: destination.clone() }, cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target: target.clone() };
    owner.effect(|| ())?;
    cdp_inner(webview, method, params, Some(owner))
}

pub(crate) fn cdp_native_invocation(
    webview: &Webview, method: &str, params: Value,
    invocation: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Value, String> {
    cdp_native_invocation_many(webview, method, params, invocation, owner, &[], cancel, pending, actor, actor, target)
}

pub(crate) fn cdp_native_invocation_many(
    webview: &Webview, method: &str, params: Value,
    invocation: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Value, String> {
    let owner = owned::Owner {
        authority: owned::Authority::Native { invocation: invocation.clone(), owner: owner.clone(), participants: participants.to_vec(), stop: stop.clone() },
        cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target: target.clone(),
    };
    owner.effect(|| ())?;
    cdp_inner(webview, method, params, Some(owner))
}

pub(crate) fn cdp_existing_document(
    webview: &Webview, method: &str, params: Value,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget,
    additional: &[shell_contract::runtime::CancelToken],
) -> Result<Value, String> {
    let owner = owned::Owner {
        authority: owned::Authority::ExistingDocument { binding: binding.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() },
        cancel: cancel.clone(), pending: pending.clone(), actor: stop.clone(), target: target.clone(),
    };
    owner.effect(|| ())?;
    cdp_inner(webview, method, params, Some(owner))
}

pub(crate) fn cdp_bytes_existing_document(
    webview: &Webview, method: &str, params: Value,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget,
    additional: &[shell_contract::runtime::CancelToken],
) -> Result<Vec<u8>, String> {
    let owner = owned::Owner {
        authority: owned::Authority::ExistingDocument { binding: binding.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() },
        cancel: cancel.clone(), pending: pending.clone(), actor: stop.clone(), target: target.clone(),
    };
    owner.effect(|| ())?;
    let result = cdp_inner(webview, method, params, Some(owner.clone()))?;
    let data = result["data"].as_str().ok_or_else(|| format!("{method} returned no data"))?;
    let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
    owner.effect(|| bytes)
}

pub(crate) fn cdp_owned_many_extra(
    webview: &Webview, method: &str, params: Value, origin: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner], cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken, stop: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget, additional: &[shell_contract::runtime::CancelToken],
) -> Result<Value, String> {
    let authority = owned::Authority::OwnedMany { origin: origin.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() };
    cdp_extra(webview, method, params, authority, cancel, pending, stop, target)
}

pub(crate) fn cdp_native_many_extra(
    webview: &Webview, method: &str, params: Value, invocation: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner, participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget, additional: &[shell_contract::runtime::CancelToken],
) -> Result<Value, String> {
    let authority = owned::Authority::NativeManyExtra { invocation: invocation.clone(), owner: owner.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() };
    cdp_extra(webview, method, params, authority, cancel, pending, stop, target)
}

fn cdp_extra(webview: &Webview, method: &str, params: Value, authority: owned::Authority,
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget) -> Result<Value, String> {
    let owner = owned::Owner { authority, cancel: cancel.clone(), pending: pending.clone(), actor: stop.clone(), target: target.clone() };
    owner.effect(|| ())?;
    cdp_inner(webview, method, params, Some(owner))
}

pub(crate) fn cdp_bytes_owned_many_extra(
    webview: &Webview, method: &str, params: Value, origin: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner], cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken, stop: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget, additional: &[shell_contract::runtime::CancelToken],
) -> Result<Vec<u8>, String> {
    let authority = owned::Authority::OwnedMany { origin: origin.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() };
    cdp_bytes_extra(webview, method, params, authority, cancel, pending, stop, target)
}

pub(crate) fn cdp_bytes_native_many_extra(
    webview: &Webview, method: &str, params: Value, invocation: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner, participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget, additional: &[shell_contract::runtime::CancelToken],
) -> Result<Vec<u8>, String> {
    let authority = owned::Authority::NativeManyExtra { invocation: invocation.clone(), owner: owner.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: stop.clone() };
    cdp_bytes_extra(webview, method, params, authority, cancel, pending, stop, target)
}

fn cdp_bytes_extra(webview: &Webview, method: &str, params: Value, authority: owned::Authority,
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken, target: &OwnedPrintTarget) -> Result<Vec<u8>, String> {
    let owner = owned::Owner { authority, cancel: cancel.clone(), pending: pending.clone(), actor: stop.clone(), target: target.clone() };
    owner.effect(|| ())?;
    let result = cdp_inner(webview, method, params, Some(owner.clone()))?;
    let data = result["data"].as_str().ok_or_else(|| format!("{method} returned no data"))?;
    let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
    owner.effect(|| bytes)
}

fn cdp_inner(webview: &Webview, method: &str, params: Value, owner: Option<owned::Owner>) -> Result<Value, String> {
    let needs_observer = !OBSERVED.lock().map_err(|_| "CDP observer state poisoned")?.contains(webview.label());
    if needs_observer {
        let pending = PENDING.clone();
        let (registered, registration) = mpsc::sync_channel(1);
        let _registration_losses = if let Some(owner) = &owner {
            owner.losses()?.into_iter().map(|loss| {
                let registered = registered.clone();
                loss.on_cancel(move || { let _ = registered.try_send(Err("Print request expired.".to_owned())); })
            }).collect::<Vec<_>>()
        } else { Vec::new() };
        webview.on_dev_tools_protocol_async(
            move |protocol| pending.complete(protocol),
            move |result| { let _ = registered.try_send(result.map_err(|error| error.to_string())); },
        ).map_err(|error| error.to_string())?;
        registration.recv_timeout(CDP_TIMEOUT).map_err(|_| "CDP observer registration timed out".to_owned())??;
        if let Some(owner) = &owner { owner.effect(|| ())?; }
        OBSERVED.lock().map_err(|_| "CDP observer state poisoned")?.insert(webview.label().to_owned());
    }
    let id = allocate_devtools_message_id().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::sync_channel(1);
    PENDING.0.lock().unwrap().insert(id, PendingCall { sender: tx, owner: owner.clone() });
    let _subscriptions = match &owner {
        Some(owner) => match owner.subscribe(id) {
            Ok(subscriptions) => subscriptions,
            Err(error) => { PENDING.0.lock().unwrap().remove(&id); return Err(error); }
        },
        None => Vec::new(),
    };
    let message = json!({ "id": id, "method": method, "params": params }).to_string();
    let failed = PENDING.clone();
    let completion = move |result: Result<(), String>| {
        if let Err(error) = result
            && let Some(call) = failed.0.lock().unwrap().remove(&id)
        {
            let _ = call.sender.try_send(Err(error));
        }
    };
    let sent = if let Some(owner) = owner.clone() {
        webview.send_dev_tools_message_authorized(message.as_bytes(), Box::new(move |send| owner.effect(send)?), completion)
    } else {
        webview.send_dev_tools_message_async(message.as_bytes(), move |result| completion(result.map_err(|error| error.to_string())))
    };
    if let Err(error) = sent {
        PENDING.0.lock().unwrap().remove(&id);
        return Err(error.to_string());
    }
    let (success, result) = rx.recv_timeout(CDP_TIMEOUT).map_err(|_| {
        PENDING.0.lock().unwrap().remove(&id);
        format!("{method}: no answer in {}s", CDP_TIMEOUT.as_secs())
    })??;
    if let Some(owner) = &owner { owner.effect(|| ())?; }
    if !success {
        return Err(String::from_utf8_lossy(&result).into_owned());
    }
    serde_json::from_slice(&result).map_err(|e| e.to_string())
}

/// A CDP result's base64 `data` field (`Page.printToPDF`, `Page.captureScreenshot`)
pub(crate) fn cdp_bytes(webview: &Webview, method: &str, params: Value) -> Result<Vec<u8>, String> {
    let result = cdp(webview, method, params)?;
    let data = result["data"]
        .as_str()
        .ok_or_else(|| format!("{method} returned no data"))?;
    STANDARD.decode(data).map_err(|e| e.to_string())
}

pub(crate) fn cdp_bytes_owned(
    webview: &Webview, method: &str, params: Value,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Vec<u8>, String> {
    let result = cdp_owned(webview, method, params, origin, destination, cancel, pending, actor, target)?;
    let data = result["data"].as_str().ok_or_else(|| format!("{method} returned no data"))?;
    let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
    let owner = owned::Owner { authority: owned::Authority::Pair { origin: origin.clone(), destination: destination.clone() }, cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target: target.clone() };
    owner.effect(|| bytes)
}

pub(crate) fn cdp_bytes_native_invocation(
    webview: &Webview, method: &str, params: Value,
    invocation: &shell_contract::ipc::NativeInvocationTarget,
    registered: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Vec<u8>, String> {
    cdp_bytes_native_invocation_many(webview, method, params, invocation, registered, &[], cancel, pending, actor, actor, target)
}

pub(crate) fn cdp_bytes_native_invocation_many(
    webview: &Webview, method: &str, params: Value,
    invocation: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    actor: &shell_contract::runtime::CancelToken,
    stop: &shell_contract::runtime::CancelToken,
    target: &OwnedPrintTarget,
) -> Result<Vec<u8>, String> {
    let owner = owned::Owner {
        authority: owned::Authority::Native { invocation: invocation.clone(), owner: owner.clone(), participants: participants.to_vec(), stop: stop.clone() },
        cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target: target.clone(),
    };
    owner.effect(|| ())?;
    let result = cdp_inner(webview, method, params, Some(owner.clone()))?;
    let data = result["data"].as_str().ok_or_else(|| format!("{method} returned no data"))?;
    let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
    owner.effect(|| bytes)
}

struct ShellContents {
    table: Arc<ViewTable>,
    windows: Arc<Windows>,
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    let contents: ContentsHost = Arc::new(ShellContents {
        table: cx.views.clone(),
        windows: cx.windows.clone(),
    });
    hosts.insert(contents);
}

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(n)) if n.is_finite() => Some(*n),
        _ => None,
    }
}

/// Electron's named paper sizes, in inches
fn paper(name: &str) -> Option<(f64, f64)> {
    Some(match name {
        "A0" => (33.11, 46.81),
        "A1" => (23.39, 33.11),
        "A2" => (16.54, 23.39),
        "A3" => (11.69, 16.54),
        "A4" => (8.27, 11.69),
        "A5" => (5.83, 8.27),
        "A6" => (4.13, 5.83),
        "Legal" => (8.5, 14.0),
        "Letter" => (8.5, 11.0),
        "Tabloid" => (11.0, 17.0),
        _ => return None,
    })
}

/// `webContents.printToPDF(options)` as the parameters of `Page.printToPDF`. A
/// key Electron has and CDP names alike goes across; the page size and the
/// margins (inches in both) are spelled out. What `options` leaves out stays
/// at the browser's default.
pub(crate) fn pdf_params(options: &IpcValue) -> Value {
    let mut params = Map::new();
    for key in [
        "landscape",
        "displayHeaderFooter",
        "printBackground",
        "preferCSSPageSize",
        "generateTaggedPDF",
        "generateDocumentOutline",
    ] {
        if let Some(IpcValue::Bool(b)) = field(options, key) {
            params.insert(key.into(), json!(b));
        }
    }
    if let Some(scale) = number(field(options, "scale")) {
        params.insert("scale".into(), json!(scale));
    }
    for key in ["pageRanges", "headerTemplate", "footerTemplate"] {
        if let Some(IpcValue::String(s)) = field(options, key) {
            params.insert(key.into(), json!(s));
        }
    }
    match field(options, "pageSize") {
        Some(IpcValue::String(name)) => {
            if let Some((w, h)) = paper(name) {
                params.insert("paperWidth".into(), json!(w));
                params.insert("paperHeight".into(), json!(h));
            }
        }
        Some(size @ IpcValue::Object(_)) => {
            if let (Some(w), Some(h)) =
                (number(field(size, "width")), number(field(size, "height")))
            {
                params.insert("paperWidth".into(), json!(w));
                params.insert("paperHeight".into(), json!(h));
            }
        }
        _ => {}
    }
    if let Some(margins) = field(options, "margins") {
        for (key, cdp) in [
            ("top", "marginTop"),
            ("bottom", "marginBottom"),
            ("left", "marginLeft"),
            ("right", "marginRight"),
        ] {
            if let Some(m) = number(field(margins, key)) {
                params.insert(cdp.into(), json!(m));
            }
        }
    }
    Value::Object(params)
}

impl ShellContents {
    fn webview_of(&self, view: ViewId) -> Result<Webview, String> {
        self.windows
            .webview(view)
            .ok_or_else(|| format!("no view {}", view.0))
    }
}

impl Contents for ShellContents {
    fn window_of(&self, view: ViewId) -> Option<WindowId> {
        self.table.sender_of(view).map(|s| s.window)
    }

    fn capture_png(&self, view: ViewId) -> Result<Vec<u8>, String> {
        cdp_bytes(
            &self.webview_of(view)?,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        )
    }

    fn print_to_pdf_owned(&self, origin: &shell_contract::ipc::NativeRequestOwner, destination: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken, options: IpcValue) -> Result<Vec<u8>, String> {
        use shell_contract::runtime::CancelToken;
        let authority = destination.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>().ok_or("Print destination is not authenticated.")?;
        let native_origin = authority.origin.clone();
        let generation = destination.context.renderer().cloned().ok_or("Print renderer is unavailable.")?;
        let origin_loss = origin.loss_token().ok_or("Print origin lifetime is unavailable.")?;
        let destination_loss = destination.loss_token().ok_or("Print destination lifetime is unavailable.")?;
        let actor = CancelToken::default();
        struct ActorLifetime(CancelToken);
        impl Drop for ActorLifetime { fn drop(&mut self) { self.0.cancel(); } }
        let _actor_lifetime = ActorLifetime(actor.clone());
        let (send, receive) = std::sync::mpsc::sync_channel::<Result<OwnedPrintTarget, String>>(1);
        let subscriptions = [cancel.clone(), pending.clone(), origin_loss, destination_loss, generation.loss_token()].into_iter().map(|loss| {
            let actor = actor.clone(); let send = send.clone();
            loss.on_cancel(move || { actor.cancel(); let _ = send.try_send(Err("Print request expired.".into())); })
        }).collect::<Vec<_>>();
        // The view lookup obtains only a transport conduit. Native origin must
        // independently prove this is the captured browser/frame/document.
        let webview = self.webview_of(destination.view)?;
        let capture_actor = actor.clone();
        webview.with_cef_webview(move |native| {
            let target = if capture_actor.is_cancelled() {
                Err("Print request expired.".to_owned())
            } else {
                native_origin.capture_current_document(&native).map(|document| OwnedPrintTarget {
                    frame: native.frame_navigation_state().clone(), document, generation,
                }).ok_or_else(|| "The captured print document is no longer current.".to_owned())
            };
            let _ = send.try_send(target);
        }).map_err(|error| error.to_string())?;
        let target = receive.recv_timeout(CDP_TIMEOUT).map_err(|_| "Print document identity timed out.".to_owned())??;
        let result = cdp_bytes_owned(&webview, "Page.printToPDF", pdf_params(&options), origin, destination, cancel, pending, &actor, &target);
        actor.cancel();
        drop(subscriptions);
        result
    }

    fn print_to_pdf_owned_many_extra(
        &self, origin: &shell_contract::ipc::NativeRequestOwner, destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner], cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        let proof = destination.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .ok_or("Print destination is not authenticated.")?;
        let native_origin = proof.origin.clone();
        let actor = CancelToken::new();
        struct ActorLifetime(CancelToken);
        impl Drop for ActorLifetime { fn drop(&mut self) { self.0.cancel(); } }
        let _actor_lifetime = ActorLifetime(actor.clone());
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(destination.clone());
        captured.extend_from_slice(participants);
        let authority = owned::Authority::OwnedMany {
            origin: origin.clone(), participants: captured.clone(), additional: additional.to_vec(), stop: actor.clone(),
        };
        let generation = RendererGeneration::new();
        let (send, receive) = mpsc::sync_channel::<Result<OwnedPrintTarget, String>>(1);
        let subscriptions = authority.losses(cancel, pending, &actor, &generation)?.into_iter().map(|loss| {
            let actor = actor.clone(); let send = send.clone();
            loss.on_cancel(move || {
                actor.cancel();
                let _ = send.try_send(Err("Print request expired.".into()));
            })
        }).collect::<Vec<_>>();
        let webview = self.webview_of(destination.view)?;
        let captured_origin = origin.clone();
        let captured_cancel = cancel.clone();
        let captured_pending = pending.clone();
        let captured_additional = additional.to_vec();
        let captured_actor = actor.clone();
        let captured_generation = generation.clone();
        webview.with_cef_webview(move |native| {
            let target = native_origin.capture_current_document(&native).map(|document| OwnedPrintTarget {
                frame: native.frame_navigation_state().clone(), document, generation: captured_generation.clone(),
            });
            let result = target.and_then(|target| {
                crate::ipc::native_owner::with_owned_print_many_extra_effect(
                    &captured_origin, &captured, &captured_cancel, &captured_pending, &captured_actor,
                    &captured_generation, &captured_additional.iter().collect::<Vec<_>>(), || {
                        (target.frame.document().as_ref() == Some(&target.document)).then_some(target)
                    },
                ).flatten()
            }).ok_or_else(|| "The captured print document is no longer current.".to_owned());
            let _ = send.try_send(result);
        }).map_err(|error| error.to_string())?;
        let target = receive.recv_timeout(CDP_TIMEOUT).map_err(|_| "Print document identity timed out.".to_owned())??;
        let owner = owned::Owner { authority, cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target };
        owner.effect(|| ())?;
        let result = cdp_inner(&webview, "Page.printToPDF", pdf_params(&options), Some(owner.clone()))?;
        let data = result["data"].as_str().ok_or("Page.printToPDF returned no data")?;
        let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
        let result = owner.effect(|| bytes);
        actor.cancel();
        drop(subscriptions);
        result
    }

    fn print_to_pdf_native_invocation(
        &self, document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        let proof = owner.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .ok_or("Print destination is not authenticated.")?;
        let native_origin = proof.origin.clone();
        let actor = CancelToken::default();
        struct ActorLifetime(CancelToken);
        impl Drop for ActorLifetime { fn drop(&mut self) { self.0.cancel(); } }
        let _actor_lifetime = ActorLifetime(actor.clone());
        let authority = owned::Authority::Native {
            invocation: document.target().clone(), owner: owner.clone(), participants: participants.to_vec(), stop: actor.clone(),
        };
        let generation = RendererGeneration::new();
        let (send, receive) = mpsc::sync_channel::<Result<OwnedPrintTarget, String>>(1);
        let subscriptions = authority.losses(cancel, pending, &actor, &generation)?.into_iter().map(|loss| {
            let actor = actor.clone(); let send = send.clone();
            loss.on_cancel(move || {
                actor.cancel();
                let _ = send.try_send(Err("Print request expired.".into()));
            })
        }).collect::<Vec<_>>();
        let webview = self.webview_of(owner.view)?;
        let invocation = document.target().clone();
        let sender = owner.clone();
        let capture_participants = match &authority {
            owned::Authority::Native { participants, .. } => participants.clone(),
            owned::Authority::Pair { .. } | owned::Authority::ExistingDocument { .. } | owned::Authority::OwnedMany { .. } | owned::Authority::NativeManyExtra { .. } => unreachable!(),
        };
        let capture_cancel = cancel.clone();
        let capture_pending = pending.clone();
        let capture_actor = actor.clone();
        let capture_generation = generation.clone();
        webview.with_cef_webview(move |native| {
            let captured = native_origin.capture_current_document(&native).map(|document| OwnedPrintTarget {
                frame: native.frame_navigation_state().clone(), document, generation: capture_generation.clone(),
            });
            let result = captured.and_then(|target| {
                crate::ipc::native_owner::with_native_invocation_print_many_effect(
                    &invocation, &sender, &capture_participants, &capture_cancel, &capture_pending,
                    &capture_actor, &capture_actor, &capture_generation, || {
                        (target.frame.document().as_ref() == Some(&target.document)).then_some(target)
                    },
                ).flatten()
            }).ok_or_else(|| "The captured print document is no longer current.".to_owned());
            let _ = send.try_send(result);
        }).map_err(|error| error.to_string())?;
        let target = receive.recv_timeout(CDP_TIMEOUT).map_err(|_| "Print document identity timed out.".to_owned())??;
        let owner = owned::Owner { authority, cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target };
        owner.effect(|| ())?;
        let result = cdp_inner(&webview, "Page.printToPDF", pdf_params(&options), Some(owner.clone()))?;
        let data = result["data"].as_str().ok_or("Page.printToPDF returned no data")?;
        let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
        let result = owner.effect(|| bytes);
        actor.cancel();
        drop(subscriptions);
        result
    }

    fn print_to_pdf_existing_document(
        &self, binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken], options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        let captured_owner = binding.owner();
        let proof = captured_owner.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .ok_or("Print destination is not authenticated.")?;
        let native_origin = proof.origin.clone();
        let actor = CancelToken::new();
        struct ActorLifetime(CancelToken);
        impl Drop for ActorLifetime { fn drop(&mut self) { self.0.cancel(); } }
        let _actor_lifetime = ActorLifetime(actor.clone());
        let authority = owned::Authority::ExistingDocument {
            binding: binding.clone(), participants: participants.to_vec(), additional: additional.to_vec(), stop: actor.clone(),
        };
        let generation = RendererGeneration::new();
        let (send, receive) = mpsc::sync_channel::<Result<OwnedPrintTarget, String>>(1);
        let subscriptions = authority.losses(cancel, pending, &actor, &generation)?.into_iter().map(|loss| {
            let actor = actor.clone(); let send = send.clone();
            loss.on_cancel(move || {
                actor.cancel();
                let _ = send.try_send(Err("Print request expired.".into()));
            })
        }).collect::<Vec<_>>();
        let webview = self.webview_of(captured_owner.view)?;
        let captured_binding = binding.clone();
        let captured_participants = participants.to_vec();
        let captured_cancel = cancel.clone();
        let captured_pending = pending.clone();
        let captured_additional = additional.to_vec();
        let captured_actor = actor.clone();
        let captured_generation = generation.clone();
        webview.with_cef_webview(move |native| {
            let captured = native_origin.capture_current_document(&native).map(|document| OwnedPrintTarget {
                frame: native.frame_navigation_state().clone(), document, generation: captured_generation.clone(),
            });
            let result = captured.and_then(|target| {
                crate::ipc::native_owner::with_existing_document_print_effect(
                    &captured_binding, &captured_participants, &captured_cancel, &captured_pending, &captured_actor,
                    &captured_generation, &captured_additional.iter().collect::<Vec<_>>(), || {
                        (target.frame.document().as_ref() == Some(&target.document)).then_some(target)
                    },
                ).flatten()
            }).ok_or_else(|| "The captured print document is no longer current.".to_owned());
            let _ = send.try_send(result);
        }).map_err(|error| error.to_string())?;
        let target = receive.recv_timeout(CDP_TIMEOUT).map_err(|_| "Print document identity timed out.".to_owned())??;
        let owner = owned::Owner { authority, cancel: cancel.clone(), pending: pending.clone(), actor: actor.clone(), target };
        owner.effect(|| ())?;
        let result = cdp_inner(&webview, "Page.printToPDF", pdf_params(&options), Some(owner.clone()))?;
        let data = result["data"].as_str().ok_or("Page.printToPDF returned no data")?;
        let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
        let result = owner.effect(|| bytes);
        actor.cancel();
        drop(subscriptions);
        result
    }

    fn print_to_pdf(&self, view: ViewId, options: IpcValue) -> Result<Vec<u8>, String> {
        cdp_bytes(
            &self.webview_of(view)?,
            "Page.printToPDF",
            pdf_params(&options),
        )
    }

    /// `webContents.print` opens the system's print dialog. Tauri's `print`
    /// shows it and does not report how it ended, so a shown dialog is
    /// `true`; batch M's print host (`host/print.rs`) owns anything finer.
    fn print(&self, view: ViewId, options: IpcValue) -> Result<bool, String> {
        let webview = self
            .windows
            .webview(view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        // a named printer, no dialog: batch M's spooler (host/print.rs)
        if let Some(IpcValue::Bool(true)) = field(&options, "silent") {
            return super::print::print_silently_ipc(&webview, &options).map(|()| true);
        }
        webview.print().map_err(|e| e.to_string())?;
        Ok(true)
    }

    fn zoom_factor(&self, view: ViewId) -> f64 {
        self.table.zoom(view)
    }

    fn set_zoom_factor(&self, view: ViewId, factor: f64) {
        self.table.set_zoom(view, factor);
        if let Some(webview) = self.windows.webview(view)
            && let Err(error) = webview.set_zoom(factor)
        {
            eprintln!("shell: setZoomFactor on view {}: {error}", view.0);
        }
    }

    fn focus(&self, view: ViewId) {
        if let Some(webview) = self.windows.webview(view)
            && let Err(error) = webview.set_focus()
        {
            eprintln!("shell: focus view {}: {error}", view.0);
        }
    }

    fn close_window(&self, window: WindowId) {
        if let Err(error) = self.windows.close_window(window) {
            eprintln!("shell: close window {}: {error}", window.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(entries: Vec<(&str, IpcValue)>) -> IpcValue {
        IpcValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    }

    #[test]
    fn electron_options_become_cdp_parameters() {
        let options = object(vec![
            ("landscape", IpcValue::Bool(true)),
            ("printBackground", IpcValue::Bool(true)),
            ("pageSize", IpcValue::String("A4".into())),
            (
                "margins",
                object(vec![
                    ("top", IpcValue::Number(0.0)),
                    ("left", IpcValue::Number(0.5)),
                ]),
            ),
            ("pageRanges", IpcValue::String("1-2".into())),
            ("scale", IpcValue::Number(0.9)),
        ]);
        assert_eq!(
            pdf_params(&options),
            json!({
                "landscape": true,
                "printBackground": true,
                "scale": 0.9,
                "pageRanges": "1-2",
                "paperWidth": 8.27,
                "paperHeight": 11.69,
                "marginTop": 0.0,
                "marginLeft": 0.5,
            })
        );
        let custom = object(vec![(
            "pageSize",
            object(vec![
                ("width", IpcValue::Number(5.0)),
                ("height", IpcValue::Number(7.0)),
            ]),
        )]);
        assert_eq!(
            pdf_params(&custom),
            json!({ "paperWidth": 5.0, "paperHeight": 7.0 })
        );
        assert_eq!(pdf_params(&IpcValue::Undefined), json!({}));
    }
}
