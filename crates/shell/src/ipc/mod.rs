//! The shell's IPC boundary (the stage 3 shell contract, section 2): three
//! Tauri commands and one synchronous scheme, however many channels there are.
//!
//! - `ipc_invoke`: the raw body is the frame `{id, channel, args}`; it is
//!   posted to the core thread and dispatched there. The command answers
//!   "received"; the handler's answer leaves on the view's output stream as
//!   `{reply: id, value | error}`.
//! - `ipc_send`: the frame `{channel, args}`, no answer.
//! - `ipc_listen`: the bridge hands over the view's output stream (a Tauri
//!   `ipc::Channel`) when a page starts.
//! - `seuoffice-ipc` (see [`sync`]): `sendSync`, answered on the core thread.
//!
//! Each command redeems a one-use native attestation and retains the exact
//! browser/document and creation-bound renderer. Labels select a registered
//! view; they never authenticate a caller or rebind a replaced document.

pub(crate) mod native_owner;
pub mod outlet;
pub mod sync;

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use shell_contract::error::IpcError;
use shell_contract::ids::{RequestId, ViewId};
use shell_contract::ipc::{ChannelKind, NativeContext, NativeRequestOwner, Reply, Sender};
use shell_contract::runtime::Thread;
use shell_contract::value::IpcValue;
use shell_core::dispatch::Dispatcher;
use shell_core::outlet::reply_frame;
use shell_core::wire::{decode_list, unframe};
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::{Manager, State, Webview};
use tauri_runtime_cef::WebviewCefExt;

use crate::measure::Measure;
use crate::trace::trace;
use crate::views::ViewTable;
use outlet::TauriOutlet;

thread_local! {
    /// The dispatcher lives on the core thread, where the handlers' `Rc` state is
    static DISPATCHER: RefCell<Option<Dispatcher>> = const { RefCell::new(None) };
}

/// Installs the dispatcher on the core thread, from the job that built the table
pub fn install_dispatcher(dispatcher: Dispatcher) {
    DISPATCHER.with(|d| *d.borrow_mut() = Some(dispatcher));
}

/// Cancels a view's pending calls; on the core thread
pub fn view_destroyed(view: ViewId) {
    DISPATCHER.with(|d| {
        if let Some(d) = d.borrow().as_ref() {
            d.view_destroyed(view);
        }
    });
}

/// Cancels calls belonging to the replaced renderer, on the core thread.
pub fn renderer_replaced(view: ViewId, old: shell_contract::runtime::RendererGeneration) {
    DISPATCHER.with(|d| {
        if let Some(d) = d.borrow().as_ref() {
            d.renderer_replaced(view, &old);
        }
    });
}

/// One message to its handler, on the core thread. `deliver` gets the answer
/// once, from whichever thread the handler answers on.
pub(crate) fn dispatch(
    sender: Sender,
    kind: ChannelKind,
    request: Option<RequestId>,
    native: Option<NativeContext>,
    channel: String,
    args: Vec<IpcValue>,
    deliver: Box<dyn FnOnce(Reply) + Send>,
) {
    if sender.surface == shell_contract::Surface::Slides &&
        !native.as_ref().and_then(|context| context.downcast_ref::<crate::views::SlidesViewLease>()).is_some_and(|lease| lease.is_active()) {
        return deliver(Err(IpcError { name: Some("Error".into()), message: "The presentation view is no longer available.".into() }));
    }
    DISPATCHER.with(|d| match d.borrow().as_ref() {
        Some(d) => d.dispatch(sender, kind, request, native, &channel, args, deliver),
        // the table is built before the first window opens; a message before it is a bug
        None => deliver(Err(IpcError {
            name: Some("Error".into()),
            message: "the shell has not finished starting".into(),
        })),
    });
}

/// What the commands and the scheme handler share; managed by Tauri
pub struct Ipc {
    pub core: Arc<dyn Thread>,
    pub views: Arc<ViewTable>,
    pub outlet: Arc<TauriOutlet>,
    /// set for `--measure-dialog`: the test channels of [`crate::measure`]
    pub measure: Option<Arc<Measure>>,
}


impl Ipc {
    /// A command or a sync request reached the shell: the measurement's
    /// watchdog tells "nothing arrived" from "something stopped later"
    pub(crate) fn note_call(&self) {
        if let Some(measure) = &self.measure {
            measure.note_call();
        }
    }

    pub(crate) fn call_sender_label(&self, label: &str) -> Result<(Sender, Option<NativeContext>), String> {
        self.views.sender_of_label(label)
            .ok_or_else(|| format!("the webview '{label}' has no bridge to the shell"))?;
        Err("Renderer document authentication is not ready.".into())
    }
}

struct Call {
    id: Option<u64>,
    channel: String,
    args: Vec<IpcValue>,
}


fn parse_bytes(bytes: &[u8], with_id: bool) -> Result<Call, String> {
    let (header, buffers) = unframe(bytes).map_err(|e| e.to_string())?;
    let channel = header
        .get("channel")
        .and_then(Value::as_str)
        .ok_or("the frame has no channel")?
        .to_owned();
    let id = if with_id {
        Some(
            header
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("the frame has no id")?,
        )
    } else {
        None
    };
    let args = decode_list(header.get("args").unwrap_or(&Value::Null), &buffers)
        .map_err(|e| e.to_string())?;
    Ok(Call { id, channel, args })
}

fn with_verified<T>(owner: &NativeRequestOwner, action: impl FnOnce() -> T) -> Option<T> {
    let origin = &owner.context.authority_ref::<native_owner::NativeOwnerAuthority>()?.origin;
    let renderer = owner.context.renderer()?;
    let loss = owner.loss_token()?;
    origin.with_live(|| renderer.with_active(|| loss.with_active(action))).flatten().flatten()
}
fn with_verified_reply<T>(
    owner: &NativeRequestOwner, cancel: Option<&shell_contract::runtime::CancelToken>, action: impl FnOnce() -> T,
) -> Option<T> {
    let Some(cancel) = cancel else { return with_verified(owner, action); };
    let origin = &owner.context.authority_ref::<native_owner::NativeOwnerAuthority>()?.origin;
    let renderer = owner.context.renderer()?;
    let loss = owner.loss_token()?;
    let mut plan = shell_contract::runtime::CancelTokenGatePlan::new(&[&loss, cancel]);
    origin.with_live(|| renderer.with_active(|| plan.with_active(action))).flatten().flatten()
}
struct ReplyAuthorityState {
    owner: NativeRequestOwner,
    transferred: bool,
    cancel: Option<shell_contract::runtime::CancelToken>,
    delivered: bool,
}

/// One authenticated invoke, never a renderer-supplied transfer capability.
pub(crate) struct ReplyAuthority {
    request: RequestId,
    state: Mutex<ReplyAuthorityState>,
}

fn same_reply_owner(a: &NativeRequestOwner, b: &NativeRequestOwner) -> bool {
    a.view == b.view && Arc::ptr_eq(&a.context.as_arc(), &b.context.as_arc())
        && a.loss_token().zip(b.loss_token()).is_some_and(|(a, b)| a.same_token(&b))
}

impl ReplyAuthority {
    fn new(request: RequestId, owner: NativeRequestOwner) -> Self {
        Self { request, state: Mutex::new(ReplyAuthorityState { owner, transferred: false, delivered: false, cancel: None }) }
    }

    /// Called only within the exact saved Docs media admission. The operation
    /// must be a short state commit and must not call application code.
    pub(crate) fn with_media_transfer<T>(
        &self, actual: &NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken,
        successor: NativeRequestOwner, commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let marker = actual.context.request_authority::<Self>().ok_or("The native request reply authority is unavailable")?;
        if !std::ptr::eq(Arc::as_ptr(&marker), self) || cancel.is_cancelled()
            || actual.dispatcher_cancellation_for(cancel).is_none() {
            return Err("The native reply transfer does not match the request".into());
        }
        let mut state = self.state.lock().map_err(|_| "The native request reply authority is unavailable")?;
        if state.delivered || state.transferred || !same_reply_owner(&state.owner, actual) {
            return Err("The native request reply authority was replaced".into());
        }
        let old_origin = actual.context.authority_ref::<native_owner::NativeOwnerAuthority>()
            .ok_or("The native request origin is unavailable")?;
        let next_origin = successor.context.authority_ref::<native_owner::NativeOwnerAuthority>()
            .ok_or("The native reply successor is unavailable")?;
        if actual.view != successor.view || old_origin.origin != next_origin.origin
            || !actual.context.renderer().zip(successor.context.renderer())
                .is_some_and(|(old, next)| old.same_generation(next)) {
            return Err("The native reply successor does not match the request".into());
        }
        let value = commit()?;
        state.owner = NativeRequestOwner { view: successor.view, context: successor.context.without_request_authority() };
        state.cancel = Some(cancel.clone());
        state.transferred = true;
        Ok(value)
    }
}


fn verified_reply(ipc: &Ipc, authority: Arc<ReplyAuthority>, id: RequestId, channel: String) -> Box<dyn FnOnce(Reply) + Send> {
    let outlet = ipc.outlet.clone();
    Box::new(move |reply| {
        let mut frame = Some(reply_frame(id, &channel, &reply));
        for _ in 0..2 {
            let (owner, transferred, cancel) = match authority.state.lock() {
                Ok(state) if !state.delivered && authority.request == id => (state.owner.clone(), state.transferred, state.cancel.clone()),
                _ => return,
            };
            let mut accepted = false;
            let mut queued = None;
            with_verified_reply(&owner, cancel.as_ref(), || {
                let Ok(mut state) = authority.state.lock() else { return; };
                if state.delivered || state.transferred != transferred || !same_reply_owner(&state.owner, &owner) { return; }
                state.delivered = true;
                accepted = true;
                if let (Some(renderer), Some(frame)) = (owner.context.renderer(), frame.take()) {
                    queued = outlet.enqueue_generation_fenced(owner.view, renderer, frame).ok().flatten();
                }
            });
            if accepted {
                if let Some(ticket) = queued { let _ = ticket.drain(); }
                return;
            }
            let changed = authority.state.lock().ok()
                .is_some_and(|state| !state.delivered && (state.transferred != transferred || !same_reply_owner(&state.owner, &owner)));
            if !changed { return; }
        }
    })
}

/// Accepts only a sender and owner bound by authenticated native ingress.
pub(crate) fn invoke_verified(ipc: &Ipc, sender: Sender, mut owner: NativeRequestOwner, bytes: &[u8]) -> Result<(), String> {
    if sender.view != owner.view || with_verified(&owner, || ()).is_none() {
        return Err("Renderer document is no longer available.".into());
    }
    let Call { id, channel, args } = parse_bytes(bytes, true)?;
    let id = RequestId(id.ok_or("the frame has no id")?);
    ipc.note_call();
    let authority = Arc::new(ReplyAuthority::new(id, owner.clone()));
    owner.context = owner.context.with_request_authority(authority.clone());
    let deliver = verified_reply(ipc, authority, id, channel.clone());
    if let Some(measure) = &ipc.measure && measure.handles(&channel) {
        measure.invoke(ipc, sender.view, id, &channel, args, deliver);
        return Ok(());
    }
    ipc.core.post(Box::new(move || {
        if with_verified(&owner, || ()).is_none() { return; }
        dispatch(sender, ChannelKind::Invoke, Some(id), Some(owner.context), channel, args, deliver);
    }));
    Ok(())
}

/// Dispatch an authenticated send without rebinding its native owner.
pub(crate) fn send_verified(ipc: &Ipc, sender: Sender, owner: NativeRequestOwner, bytes: &[u8]) -> Result<(), String> {
    if sender.view != owner.view || with_verified(&owner, || ()).is_none() {
        return Err("Renderer document is no longer available.".into());
    }
    let Call { channel, args, .. } = parse_bytes(bytes, false)?;
    ipc.note_call();
    ipc.core.post(Box::new(move || {
        if with_verified(&owner, || ()).is_none() { return; }
        let name = channel.clone();
        dispatch(sender, ChannelKind::Send, None, Some(owner.context), channel, args, Box::new(move |reply| {
            if let Err(error) = reply { eprintln!("shell: send '{name}': {error}"); }
        }));
    }));
    Ok(())
}

/// Attach the stream only while the captured origin and generation remain live.
pub(crate) fn listen_verified(ipc: &Ipc, sender: Sender, owner: NativeRequestOwner, channel: Channel<InvokeResponseBody>) -> Result<(), String> {
    if sender.view != owner.view { return Err("Renderer document is no longer available.".into()); }
    let renderer = owner.context.renderer().ok_or("Renderer document is no longer available.")?;
    let channel = Arc::new(channel);
    let retained = channel.clone();
    let (_, drain) = with_verified(&owner, || ipc.outlet.attach_generation_fenced(sender.view, channel, renderer))
        .ok_or_else(|| "Renderer document is no longer available.".to_owned())??;
    drop(retained);
    if let Some(drain) = drain { drain.drain()?; }
    ipc.note_call();
    Ok(())
}

fn verified_sender(
    ipc: &Ipc,
    label: &str,
    origin: &tauri_runtime_cef::NativeContextToken,
) -> Result<(Sender, NativeRequestOwner, crate::views::NativeReservedViewState), String> {
    let (sender, renderer, reserved) = ipc.views.authorize_verified_native_origin(label, origin)
        .ok_or("Renderer document authentication is not ready.")?;
    let owner = native_owner::capture_owner(&ipc.views, sender.view, &renderer)?;
    let authority = owner.context.authority_ref::<native_owner::NativeOwnerAuthority>()
        .ok_or("Renderer document authentication is not ready.")?;
    if authority.origin != *origin {
        return Err("Renderer document is no longer available.".into());
    }
    Ok((sender, owner, reserved))
}

fn attested_sender(
    ipc: &Ipc, webview: &Webview, request: &Request<'_>,
) -> Result<(Sender, NativeRequestOwner), String> {
    let origin = tauri_runtime_cef::redeem_native_context(request.headers())
        .ok_or("Renderer document authentication is not ready.")?;
    let (sender, owner, _) = verified_sender(ipc, webview.label(), &origin)?;
    Ok((sender, owner))
}

/// Called once on the actual constructed webview, before its document loads.
/// The native producer proves each context against this captured creation epoch.
pub(crate) fn bind_native_webview(
    webview: &Webview, view: ViewId, creation: shell_contract::runtime::RendererGeneration,
) -> Result<(), String> {
    let ipc = webview.app_handle().try_state::<Ipc>()
        .ok_or("The shell IPC service is not installed.")?;
    let views = ipc.views.clone();
    let app = webview.app_handle().clone();
    let label = webview.label().to_owned();
    let captured_webview = webview.clone();
    webview.bind_native_context_ipc(
        move |origin| views.bind_verified_native_origin(view, &creation, origin),
        move |mut request| {
            let Some(ipc) = app.try_state::<Ipc>() else { request.respond(None); return };
            let Ok((sender, owner, reserved)) = verified_sender(&ipc, &label, &request.origin) else {
                request.respond(None);
                return;
            };
            if sender.view != view { request.respond(None); return; }
            let channel = std::mem::take(&mut request.channel);
            let bytes = std::mem::take(&mut request.body);
            sync::handle_verified(&ipc, sender, owner, reserved, captured_webview.clone(), channel, &bytes,
                Box::new(move |bytes| request.respond(Some(bytes))));
        },
    ).map_err(|error| error.to_string())
}

struct ExactNativeChannel {
    sink: tauri_runtime_cef::NativeChannelQueue,
    callback: u32,
    index: std::sync::Mutex<u64>,
}

impl ExactNativeChannel {
    fn send(&self, body: InvokeResponseBody) -> Result<(), String> {
        let mut index = self.index.lock().map_err(|_| "Native channel state was poisoned.")?;
        let next = index.checked_add(1).ok_or("Native channel sequence was exhausted.")?;
        let (raw, bytes) = match body {
            InvokeResponseBody::Raw(bytes) => (true, bytes),
            InvokeResponseBody::Json(json) => (false, json.into_bytes()),
        };
        self.sink.enqueue(self.callback, *index, raw, bytes, false)?;
        *index = next;
        Ok(())
    }
}

impl Drop for ExactNativeChannel {
    fn drop(&mut self) {
        if let Ok(index) = self.index.get_mut() {
            let _ = self.sink.enqueue(self.callback, *index, false, Vec::new(), true);
        }
    }
}

fn native_channel(owner: &NativeRequestOwner, id: &str) -> Result<Channel<InvokeResponseBody>, String> {
    use std::str::FromStr;
    use tauri::ipc::JavaScriptChannelId;
    JavaScriptChannelId::from_str(id).map_err(str::to_owned)?;
    let callback = id.strip_prefix("__CHANNEL__:")
        .and_then(|id| id.parse::<u32>().ok()).ok_or("Invalid native channel ID.")?;
    let origin = &owner.context.authority_ref::<native_owner::NativeOwnerAuthority>()
        .ok_or("Renderer document authentication is not ready.")?.origin;
    let sink = origin.reply_sink().and_then(|sink| sink.channel_queue())
        .ok_or("Native reply transport is not ready.")?;
    let channel = ExactNativeChannel { sink, callback, index: std::sync::Mutex::new(0) };
    Ok(Channel::new(move |body| {
        channel.send(body).map_err(|error| std::io::Error::other(error).into())
    }))
}

#[tauri::command]
pub async fn ipc_invoke(
    webview: Webview,
    request: Request<'_>,
    ipc: State<'_, Ipc>,
) -> Result<(), String> {
    let (sender, owner) = attested_sender(&ipc, &webview, &request)
        .inspect_err(|e| trace!("ipc", "invoke refused: {e}"))?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("the shell's IPC commands take a raw body".into());
    };
    invoke_verified(&ipc, sender, owner, bytes)
}

#[tauri::command]
pub async fn ipc_send(
    webview: Webview,
    request: Request<'_>,
    ipc: State<'_, Ipc>,
) -> Result<(), String> {
    let (sender, owner) = attested_sender(&ipc, &webview, &request)
        .inspect_err(|e| trace!("ipc", "send refused: {e}"))?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("the shell's IPC commands take a raw body".into());
    };
    send_verified(&ipc, sender, owner, bytes)
}

#[tauri::command]
pub async fn ipc_listen(
    webview: Webview,
    request: Request<'_>,
    on_frame: String,
    ipc: State<'_, Ipc>,
) -> Result<(), String> {
    let (sender, owner) = attested_sender(&ipc, &webview, &request)
        .inspect_err(|e| trace!("ipc", "listen refused: {e}"))?;
    let channel = native_channel(&owner, &on_frame)?;
    listen_verified(&ipc, sender, owner, channel)
}

#[cfg(test)]
mod reply_transfer_tests {
    use super::*;
    use shell_contract::runtime::{CancelToken, RendererGeneration};

    fn request() -> (Arc<ReplyAuthority>, NativeRequestOwner, CancelToken) {
        let renderer = RendererGeneration::new();
        let owner = NativeRequestOwner { view: ViewId(7),
            context: NativeContext::with_owner_loss((), renderer, CancelToken::default()) };
        let authority = Arc::new(ReplyAuthority::new(RequestId(11), owner.clone()));
        let cancel = CancelToken::default();
        let actual = NativeRequestOwner { view: owner.view,
            context: owner.context.with_request_authority(authority.clone())
                .bind_dispatcher_cancellation(owner.view, cancel.clone(), CancelToken::default()) };
        (authority, actual, cancel)
    }

    #[test]
    fn request_transfer_rejects_foreign_request_cancel_and_unattested_origin() {
        let (authority, actual, cancel) = request();
        let (other, _, _) = request();
        let wrong_request = NativeRequestOwner { view: actual.view,
            context: actual.context.clone().with_request_authority(other) };
        let mut committed = false;
        assert!(authority.with_media_transfer(&wrong_request, &cancel, actual.clone(), || {
            committed = true; Ok(())
        }).is_err());
        assert!(authority.with_media_transfer(&actual, &CancelToken::default(), actual.clone(), || {
            committed = true; Ok(())
        }).is_err());
        assert!(authority.with_media_transfer(&actual, &cancel, actual.clone(), || {
            committed = true; Ok(())
        }).is_err());
        assert!(!committed);
        cancel.cancel();
        assert!(authority.with_media_transfer(&actual, &cancel, actual.clone(), || {
            committed = true; Ok(())
        }).is_err());
        assert!(!committed);
    }

    #[test]
    fn consumed_request_cannot_transfer_again() {
        let (authority, actual, cancel) = request();
        authority.state.lock().unwrap().transferred = true;
        let error = authority.with_media_transfer(&actual, &cancel, actual.clone(), || Ok(())).unwrap_err();
        assert_eq!(error, "The native request reply authority was replaced");
    }
}
