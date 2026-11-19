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
//! Each command redeems the document nonce its bridge registered (`ipc_register`, header
//! `x-seuoffice-doc`) and retains the exact browser/document and creation-bound renderer.
//! Labels select a registered view; they never authenticate a caller or rebind a replaced
//! document.

pub(crate) mod native_owner;
mod native_record;
pub mod outlet;
pub mod sync;

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::native::{self, BridgeMode, BrowserIdentity, DocumentContext, WebviewExt};
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

/// A dispatch that holds the core thread this long is traced as such
const SLOW_DISPATCH: std::time::Duration = std::time::Duration::from_millis(250);

/// The channel the core thread is dispatching right now and since when: a stalled run reads it
/// (`in_flight`) to say which handler holds the core thread
static IN_FLIGHT: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);

/// One dispatch in progress; the record ends with it, also when the handler panics
struct InFlight {
    started: std::time::Instant,
}

impl InFlight {
    fn begin(channel: &str) -> Self {
        let started = std::time::Instant::now();
        *IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()) = Some((channel.to_owned(), started));
        Self { started }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        *IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The dispatch the core thread is inside of, with how long it has been there
pub(crate) fn in_flight() -> Option<String> {
    let held = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    let (channel, since) = held.as_ref()?;
    Some(format!(
        "'{channel}' for {} ms",
        since.elapsed().as_millis()
    ))
}

/// One message to its handler, on the core thread. `deliver` gets the answer
/// once, from whichever thread the handler answers on. A `--e2e` run records the
/// native-only channels here (native_record.rs).
pub(crate) fn dispatch(
    sender: Sender,
    kind: ChannelKind,
    request: Option<RequestId>,
    native: Option<NativeContext>,
    channel: String,
    args: Vec<IpcValue>,
    deliver: Box<dyn FnOnce(Reply) + Send>,
) {
    let call = native_record::Call {
        sender,
        kind,
        request,
        native: native.is_some(),
        channel: &channel,
    };
    let Some((args, deliver)) = native_record::observe(&call, args, deliver) else {
        return;
    };
    if sender.surface == shell_contract::Surface::Slides
        && !native
            .as_ref()
            .and_then(|context| context.downcast_ref::<crate::views::SlidesViewLease>())
            .is_some_and(|lease| lease.is_active())
    {
        return deliver(Err(IpcError {
            name: Some("Error".into()),
            message: "The presentation view is no longer available.".into(),
        }));
    }
    trace!("ipc", "view {} '{channel}'", sender.view.0);
    let in_flight = crate::trace::enabled().then(|| InFlight::begin(&channel));
    DISPATCHER.with(|d| match d.borrow().as_ref() {
        Some(d) => d.dispatch(sender, kind, request, native, &channel, args, deliver),
        // the table is built before the first window opens; a message before it is a bug
        None => deliver(Err(IpcError {
            name: Some("Error".into()),
            message: "the shell has not finished starting".into(),
        })),
    });
    if let Some(in_flight) = in_flight
        && in_flight.started.elapsed() >= SLOW_DISPATCH
    {
        trace!(
            "core",
            "'{channel}' held the core thread for {} ms",
            in_flight.started.elapsed().as_millis()
        );
    }
}

/// What the commands and the scheme handler share; managed by Tauri
#[derive(Clone)]
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
    let origin = &owner
        .context
        .authority_ref::<native_owner::NativeOwnerAuthority>()?
        .origin;
    let renderer = owner.context.renderer()?;
    let loss = owner.loss_token()?;
    origin
        .with_live(|| renderer.with_active(|| loss.with_active(action)))
        .flatten()
        .flatten()
}
fn with_verified_reply<T>(
    owner: &NativeRequestOwner,
    cancel: Option<&shell_contract::runtime::CancelToken>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let Some(cancel) = cancel else {
        return with_verified(owner, action);
    };
    let origin = &owner
        .context
        .authority_ref::<native_owner::NativeOwnerAuthority>()?
        .origin;
    let renderer = owner.context.renderer()?;
    let loss = owner.loss_token()?;
    let mut plan = shell_contract::runtime::CancelTokenGatePlan::new(&[&loss, cancel]);
    origin
        .with_live(|| renderer.with_active(|| plan.with_active(action)))
        .flatten()
        .flatten()
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
    a.view == b.view
        && Arc::ptr_eq(&a.context.as_arc(), &b.context.as_arc())
        && a.loss_token()
            .zip(b.loss_token())
            .is_some_and(|(a, b)| a.same_token(&b))
}

impl ReplyAuthority {
    fn new(request: RequestId, owner: NativeRequestOwner) -> Self {
        Self {
            request,
            state: Mutex::new(ReplyAuthorityState {
                owner,
                transferred: false,
                delivered: false,
                cancel: None,
            }),
        }
    }

    /// Called only within the exact saved Docs media admission. The operation
    /// must be a short state commit and must not call application code.
    pub(crate) fn with_media_transfer<T>(
        &self,
        actual: &NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        successor: NativeRequestOwner,
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let marker = actual
            .context
            .request_authority::<Self>()
            .ok_or("The native request reply authority is unavailable")?;
        if !std::ptr::eq(Arc::as_ptr(&marker), self)
            || cancel.is_cancelled()
            || actual.dispatcher_cancellation_for(cancel).is_none()
        {
            return Err("The native reply transfer does not match the request".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "The native request reply authority is unavailable")?;
        if state.delivered || state.transferred || !same_reply_owner(&state.owner, actual) {
            return Err("The native request reply authority was replaced".into());
        }
        let old_origin = actual
            .context
            .authority_ref::<native_owner::NativeOwnerAuthority>()
            .ok_or("The native request origin is unavailable")?;
        let next_origin = successor
            .context
            .authority_ref::<native_owner::NativeOwnerAuthority>()
            .ok_or("The native reply successor is unavailable")?;
        if actual.view != successor.view
            || old_origin.origin != next_origin.origin
            || !actual
                .context
                .renderer()
                .zip(successor.context.renderer())
                .is_some_and(|(old, next)| old.same_generation(next))
        {
            return Err("The native reply successor does not match the request".into());
        }
        let value = commit()?;
        state.owner = NativeRequestOwner {
            view: successor.view,
            context: successor.context.without_request_authority(),
        };
        state.cancel = Some(cancel.clone());
        state.transferred = true;
        Ok(value)
    }

    /// Who the answer is owed to, until it was delivered.
    fn undelivered(&self) -> Option<ReplyClaim> {
        let state = self.state.lock().ok()?;
        (!state.delivered).then(|| (state.owner.clone(), state.transferred, state.cancel.clone()))
    }
}

/// What an answer is delivered against: the owner of its request, whether the answer was
/// transferred to the owner of a document its call loaded, and the cancellation such a
/// transfer stays bound to.
type ReplyClaim = (NativeRequestOwner, bool, Option<shell_contract::runtime::CancelToken>);

/// Hands `frame` to the page inside the gates of `under`, if the request is still as `claim`
/// read it. `true`: this was the one delivery of the request's answer.
fn deliver_reply(
    outlet: &TauriOutlet,
    authority: &ReplyAuthority,
    (owner, transferred, cancel): &ReplyClaim,
    under: &NativeRequestOwner,
    frame: &mut Option<Vec<u8>>,
    channel: &str,
) -> bool {
    let mut accepted = false;
    let mut queued = None;
    with_verified_reply(under, cancel.as_ref(), || {
        let Ok(mut state) = authority.state.lock() else {
            return;
        };
        if state.delivered
            || state.transferred != *transferred
            || !same_reply_owner(&state.owner, owner)
        {
            return;
        }
        state.delivered = true;
        accepted = true;
        if let (Some(renderer), Some(frame)) = (under.context.renderer(), frame.take()) {
            queued = match outlet.enqueue_generation_fenced(under.view, renderer, frame) {
                Ok(ticket) => ticket,
                Err(_) => {
                    trace!("refused", "the reply of '{channel}' could not be queued");
                    None
                }
            };
        }
    });
    if let Some(ticket) = queued {
        let _ = ticket.drain();
    }
    accepted
}

fn verified_reply(
    ipc: &Ipc,
    authority: Arc<ReplyAuthority>,
    id: RequestId,
    channel: String,
) -> Box<dyn FnOnce(Reply) + Send> {
    let ipc = ipc.clone();
    Box::new(move |reply| {
        if authority.request != id {
            return;
        }
        let mut frame = Some(reply_frame(id, &channel, &reply));
        for _ in 0..2 {
            let Some(claim) = authority.undelivered() else {
                return;
            };
            if deliver_reply(&ipc.outlet, &authority, &claim, &claim.0, &mut frame, &channel) {
                return;
            }
            let changed = authority.undelivered().is_some_and(|(owner, transferred, _)| {
                transferred != claim.1 || !same_reply_owner(&owner, &claim.0)
            });
            if changed {
                continue;
            }
            // The owner is lost. Whoever answers may hold gates of its own here, so the owner
            // the page's document has now is looked for in a job of the core thread, which
            // starts with none.
            let Some(frame) = frame.take() else {
                return;
            };
            let core = ipc.core.clone();
            core.post(Box::new(move || late_reply(&ipc, &authority, frame, &channel)));
            return;
        }
    })
}

/// The answer of a call whose owner was lost after its dispatch, on the core thread. A page
/// that loads a document takes the owner from the calls it has in progress and still waits for
/// their answers (S121, macOS `docs-edit`: `app:font-family-info`). The owner its native
/// document has now then admits the answer in the lost one's place: same view, same renderer,
/// same native document ([`native_owner::succeeding_owner`]), and the cancellation of a
/// transferred answer still applies. A page that is gone has no such owner and gets nothing.
/// Such an answer leaves after whatever the core thread sent the page in between.
fn late_reply(ipc: &Ipc, authority: &ReplyAuthority, frame: Vec<u8>, channel: &str) {
    let Some(claim) = authority.undelivered() else {
        return;
    };
    let view = claim.0.view.0;
    let mut frame = Some(frame);
    let current = native_owner::succeeding_owner(&ipc.views, &claim.0);
    let delivered = current.as_ref().is_some_and(|current| {
        deliver_reply(&ipc.outlet, authority, &claim, current, &mut frame, channel)
    });
    if delivered {
        trace!(
            "owner",
            "the reply of '{channel}' reaches view {view} under the owner its document has now"
        );
    } else {
        trace!(
            "refused",
            "the reply of '{channel}' of view {view} was dropped: its document is gone"
        );
    }
}

/// The owner a posted call is dispatched under: its own while that still verifies. Between its
/// arrival and its turn on the core thread the page may have loaded a document, which takes the
/// owner from every other call the page has on its way, though the page still waits for their
/// answers. The owner its native document has now then stands for the lost one
/// ([`native_owner::succeeding_owner`]). `None`: the document is gone, with nobody to answer.
fn posted_owner(
    views: &ViewTable,
    owner: NativeRequestOwner,
    kind: &str,
    channel: &str,
) -> Option<NativeRequestOwner> {
    if with_verified(&owner, || ()).is_some() {
        return Some(owner);
    }
    let current = native_owner::succeeding_owner(views, &owner)
        .filter(|current| with_verified(current, || ()).is_some());
    if current.is_some() {
        trace!(
            "owner",
            "the {kind} '{channel}' of view {} goes on under the owner its document has now",
            owner.view.0
        );
    } else {
        trace!(
            "refused",
            "the {kind} '{channel}' of view {} was not dispatched: its document is gone",
            owner.view.0
        );
    }
    current
}

/// Accepts only a sender and owner bound by authenticated native ingress.
pub(crate) fn invoke_verified(
    ipc: &Ipc,
    sender: Sender,
    owner: NativeRequestOwner,
    bytes: &[u8],
) -> Result<(), String> {
    if sender.view != owner.view || with_verified(&owner, || ()).is_none() {
        trace!(
            "refused",
            "the owner of view {} did not verify for an invoke (same view: {})",
            sender.view.0,
            sender.view == owner.view
        );
        return Err("Renderer document is no longer available.".into());
    }
    let Call { id, channel, args } = parse_bytes(bytes, true)?;
    let id = RequestId(id.ok_or("the frame has no id")?);
    ipc.note_call();
    if let Some(measure) = &ipc.measure
        && measure.handles(&channel)
    {
        let authority = Arc::new(ReplyAuthority::new(id, owner));
        let deliver = verified_reply(ipc, authority, id, channel.clone());
        measure.invoke(ipc, sender.view, id, &channel, args, deliver);
        return Ok(());
    }
    let shell = ipc.clone();
    ipc.core.post(Box::new(move || {
        let Some(mut owner) = posted_owner(&shell.views, owner, "invoke", &channel) else {
            return;
        };
        let authority = Arc::new(ReplyAuthority::new(id, owner.clone()));
        owner.context = owner.context.with_request_authority(authority.clone());
        let deliver = verified_reply(&shell, authority, id, channel.clone());
        dispatch(
            sender,
            ChannelKind::Invoke,
            Some(id),
            Some(owner.context),
            channel,
            args,
            deliver,
        );
    }));
    Ok(())
}

/// Dispatch an authenticated send without rebinding its native owner.
pub(crate) fn send_verified(
    ipc: &Ipc,
    sender: Sender,
    owner: NativeRequestOwner,
    bytes: &[u8],
) -> Result<(), String> {
    if sender.view != owner.view || with_verified(&owner, || ()).is_none() {
        trace!(
            "refused",
            "the owner of view {} did not verify for a send (same view: {})",
            sender.view.0,
            sender.view == owner.view
        );
        return Err("Renderer document is no longer available.".into());
    }
    let Call { channel, args, .. } = parse_bytes(bytes, false)?;
    ipc.note_call();
    let views = ipc.views.clone();
    ipc.core.post(Box::new(move || {
        let Some(owner) = posted_owner(&views, owner, "send", &channel) else {
            return;
        };
        let name = channel.clone();
        dispatch(
            sender,
            ChannelKind::Send,
            None,
            Some(owner.context),
            channel,
            args,
            Box::new(move |reply| {
                if let Err(error) = reply {
                    eprintln!("shell: send '{name}': {error}");
                }
            }),
        );
    }));
    Ok(())
}

/// Attach the stream only while the captured origin and generation remain live.
pub(crate) fn listen_verified(
    ipc: &Ipc,
    sender: Sender,
    owner: NativeRequestOwner,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    if sender.view != owner.view {
        return Err("Renderer document is no longer available.".into());
    }
    let renderer = owner
        .context
        .renderer()
        .ok_or("Renderer document is no longer available.")?;
    let channel = Arc::new(channel);
    let retained = channel.clone();
    let (_, drain) = with_verified(&owner, || {
        ipc.outlet
            .attach_generation_fenced(sender.view, channel, renderer)
    })
    .ok_or_else(|| "Renderer document is no longer available.".to_owned())??;
    drop(retained);
    if let Some(drain) = drain {
        drain.drain()?;
    }
    ipc.note_call();
    Ok(())
}

pub(super) fn verified_sender(
    ipc: &Ipc,
    label: &str,
    origin: &DocumentContext,
) -> Result<
    (
        Sender,
        NativeRequestOwner,
        crate::views::NativeReservedViewState,
    ),
    String,
> {
    let (sender, renderer, reserved) = ipc
        .views
        .authorize_verified_native_origin(label, origin)
        .ok_or(NOT_READY)?;
    let owner = native_owner::capture_owner(&ipc.views, sender.view, &renderer)?;
    let authority = owner
        .context
        .authority_ref::<native_owner::NativeOwnerAuthority>()
        .ok_or(NOT_READY)?;
    if authority.origin != *origin {
        trace!(
            "refused",
            "the owner of view {} is bound to another document than the caller's", sender.view.0
        );
        return Err("Renderer document is no longer available.".into());
    }
    Ok((sender, owner, reserved))
}

fn attested_sender(
    ipc: &Ipc,
    webview: &Webview,
    request: &Request<'_>,
) -> Result<(Sender, NativeRequestOwner), String> {
    let origin = native::redeem(webview, request.headers()).ok_or(NOT_READY)?;
    let (sender, owner, _) = verified_sender(ipc, webview.label(), &origin)?;
    Ok((sender, owner))
}

/// Refusal text of every call whose document is not (yet) bound to its view. The bridge
/// retries a call refused with exactly this text, which Rust answers before reading any state.
pub(crate) const NOT_READY: &str = "Renderer document authentication is not ready.";
/// `ipc_register` before the document was committed; the bridge retries the registration.
pub(crate) const NOT_COMMITTED: &str = "Renderer document is not committed yet.";

fn register_refusal(error: native::RegisterError) -> String {
    match error {
        native::RegisterError::NoDocument => NOT_COMMITTED.to_owned(),
        native::RegisterError::BridgeDisabled => "Renderer document has no bridge.".to_owned(),
        native::RegisterError::Revoked | native::RegisterError::Conflict => {
            "Renderer document is no longer available.".to_owned()
        }
    }
}

pub(crate) fn bridge_name(mode: BridgeMode) -> &'static str {
    match mode {
        BridgeMode::None => "none",
        BridgeMode::Invoke => "invoke",
        BridgeMode::Preload => "preload",
    }
}

/// The browser a request arrived on and the nonce it carries.
fn nonce_browser(
    webview: &Webview,
    request: &Request<'_>,
) -> Result<(BrowserIdentity, [u8; native::ingress::NONCE_LEN]), String> {
    let nonce = native::ingress::nonce_of(request.headers()).ok_or(NOT_READY)?;
    let browser = webview.native_browser().map_err(|_| NOT_READY.to_owned())?;
    Ok((browser, nonce))
}

/// Bind the nonce to the browser's committed document, once (the bridge's first call). The
/// answer is the public id of the document (the argument of `__seuofficeCheck`) and what its
/// bridge may do.
pub(crate) fn register_document(
    browser: &BrowserIdentity,
    nonce: [u8; native::ingress::NONCE_LEN],
) -> Result<Value, String> {
    let context = browser.register_document(nonce).map_err(register_refusal)?;
    Ok(serde_json::json!({
        "document": context.identity().id().get(),
        "bridge": bridge_name(browser.bridge_mode()),
    }))
}

/// Called once on the actual constructed webview, before its document loads. Every document
/// the browser registers is bound against this captured creation epoch.
pub(crate) fn bind_native_webview(
    webview: &Webview,
    view: ViewId,
    creation: shell_contract::runtime::RendererGeneration,
) -> Result<(), String> {
    let ipc = webview
        .app_handle()
        .try_state::<Ipc>()
        .ok_or("The shell IPC service is not installed.")?;
    let views = ipc.views.clone();
    webview.native_browser()?.bind_registration(move |origin| {
        // A refused binding leaves the document without an owner: its calls stay refused.
        views.bind_verified_native_origin(view, &creation, origin);
    })
}

struct ExactNativeChannel {
    sink: native::ChannelQueue,
    callback: u32,
    index: std::sync::Mutex<u64>,
}

impl ExactNativeChannel {
    fn send(&self, body: InvokeResponseBody) -> Result<(), String> {
        let mut index = self
            .index
            .lock()
            .map_err(|_| "Native channel state was poisoned.")?;
        let next = index
            .checked_add(1)
            .ok_or("Native channel sequence was exhausted.")?;
        let (raw, bytes) = match body {
            InvokeResponseBody::Raw(bytes) => (true, bytes),
            InvokeResponseBody::Json(json) => (false, json.into_bytes()),
        };
        self.sink
            .enqueue(self.callback, *index, raw, bytes, false)?;
        *index = next;
        Ok(())
    }
}

impl Drop for ExactNativeChannel {
    fn drop(&mut self) {
        if let Ok(index) = self.index.get_mut() {
            let _ = self
                .sink
                .enqueue(self.callback, *index, false, Vec::new(), true);
        }
    }
}

fn native_channel(
    owner: &NativeRequestOwner,
    id: &str,
) -> Result<Channel<InvokeResponseBody>, String> {
    use std::str::FromStr;
    use tauri::ipc::JavaScriptChannelId;
    JavaScriptChannelId::from_str(id).map_err(str::to_owned)?;
    let callback = id
        .strip_prefix("__CHANNEL__:")
        .and_then(|id| id.parse::<u32>().ok())
        .ok_or("Invalid native channel ID.")?;
    let origin = &owner
        .context
        .authority_ref::<native_owner::NativeOwnerAuthority>()
        .ok_or("Renderer document authentication is not ready.")?
        .origin;
    let sink = origin
        .reply_sink()
        .and_then(|sink| sink.channel_queue())
        .ok_or("Native reply transport is not ready.")?;
    let channel = ExactNativeChannel {
        sink,
        callback,
        index: std::sync::Mutex::new(0),
    };
    Ok(Channel::new(move |body| {
        channel
            .send(body)
            .map_err(|error| std::io::Error::other(error).into())
    }))
}

/// The bridge's first call: bind its nonce to the committed document of this webview's browser.
#[tauri::command]
pub async fn ipc_register(webview: Webview, request: Request<'_>) -> Result<Value, String> {
    trace!("ipc", "'{}' registers its document", webview.label());
    let (browser, nonce) = nonce_browser(&webview, &request)
        .inspect_err(|e| trace!("ipc", "register refused: {e}"))?;
    register_document(&browser, nonce).inspect_err(|e| trace!("ipc", "register refused: {e}"))
}

/// The bridge's second call: the registered document has loaded and its fonts are ready.
#[tauri::command]
pub async fn ipc_render_ready(webview: Webview, request: Request<'_>) -> Result<(), String> {
    trace!("ipc", "'{}' reports render-ready", webview.label());
    let (browser, nonce) = nonce_browser(&webview, &request)
        .inspect_err(|e| trace!("ipc", "render-ready refused: {e}"))?;
    browser
        .render_ready(&nonce)
        .map(|_| ())
        .map_err(register_refusal)
}

/// A refused call, on a tag the trace does not thin out: which channel of which view was
/// refused and why. The `ipc` tag shows one line in a hundred after the first forty.
fn trace_refused(kind: &str, label: &str, bytes: Option<&[u8]>, error: &str) {
    trace!("refused", "{kind} of '{label}'{}: {error}", {
        let channel = bytes.and_then(|bytes| parse_bytes(bytes, false).ok());
        channel.map_or(String::new(), |call| format!(" '{}'", call.channel))
    });
}

#[tauri::command]
pub async fn ipc_invoke(
    webview: Webview,
    request: Request<'_>,
    ipc: State<'_, Ipc>,
) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("the shell's IPC commands take a raw body".into());
    };
    let bytes: &[u8] = bytes;
    let (sender, owner) = attested_sender(&ipc, &webview, &request)
        .inspect_err(|e| trace_refused("invoke", webview.label(), Some(bytes), e))?;
    invoke_verified(&ipc, sender, owner, bytes)
        .inspect_err(|e| trace_refused("invoke", webview.label(), Some(bytes), e))
}

#[tauri::command]
pub async fn ipc_send(
    webview: Webview,
    request: Request<'_>,
    ipc: State<'_, Ipc>,
) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("the shell's IPC commands take a raw body".into());
    };
    let bytes: &[u8] = bytes;
    let (sender, owner) = attested_sender(&ipc, &webview, &request)
        .inspect_err(|e| trace_refused("send", webview.label(), Some(bytes), e))?;
    send_verified(&ipc, sender, owner, bytes)
        .inspect_err(|e| trace_refused("send", webview.label(), Some(bytes), e))
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
        let owner = NativeRequestOwner {
            view: ViewId(7),
            context: NativeContext::with_owner_loss((), renderer, CancelToken::default()),
        };
        let authority = Arc::new(ReplyAuthority::new(RequestId(11), owner.clone()));
        let cancel = CancelToken::default();
        let actual = NativeRequestOwner {
            view: owner.view,
            context: owner
                .context
                .with_request_authority(authority.clone())
                .bind_dispatcher_cancellation(owner.view, cancel.clone(), CancelToken::default()),
        };
        (authority, actual, cancel)
    }

    #[test]
    fn request_transfer_rejects_foreign_request_cancel_and_unattested_origin() {
        let (authority, actual, cancel) = request();
        let (other, _, _) = request();
        let wrong_request = NativeRequestOwner {
            view: actual.view,
            context: actual.context.clone().with_request_authority(other),
        };
        let mut committed = false;
        assert!(
            authority
                .with_media_transfer(&wrong_request, &cancel, actual.clone(), || {
                    committed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(
            authority
                .with_media_transfer(&actual, &CancelToken::default(), actual.clone(), || {
                    committed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(
            authority
                .with_media_transfer(&actual, &cancel, actual.clone(), || {
                    committed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!committed);
        cancel.cancel();
        assert!(
            authority
                .with_media_transfer(&actual, &cancel, actual.clone(), || {
                    committed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!committed);
    }

    #[test]
    fn consumed_request_cannot_transfer_again() {
        let (authority, actual, cancel) = request();
        authority.state.lock().unwrap().transferred = true;
        let error = authority
            .with_media_transfer(&actual, &cancel, actual.clone(), || Ok(()))
            .unwrap_err();
        assert_eq!(error, "The native request reply authority was replaced");
    }
}

#[cfg(test)]
mod posted_call_tests {
    use super::*;
    use crate::native::browser::testing::{TEST_NONCE, registered_context, url};
    use main_core::docs::byte_handoff::MediaViewLease;
    use shell_contract::ipc::{CallContext, Handler, Registry, Responder};
    use shell_contract::runtime::RendererGeneration;
    use shell_core::table::Table;
    use shell_core::wire::{Encoder, frame};
    use std::rc::Rc;

    struct Inline;

    impl Thread for Inline {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }
        fn is_current(&self) -> bool {
            true
        }
    }

    /// The core thread of a test: what is posted to it waits until the test lets it run.
    #[derive(Default)]
    struct Waiting(Mutex<Vec<Box<dyn FnOnce() + Send>>>);

    impl Thread for Waiting {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            self.0.lock().unwrap().push(job);
        }
        fn is_current(&self) -> bool {
            true
        }
    }

    /// A Docs page of a real view table, bound to a real registered native document, with its
    /// output stream open. `answered` lists the requests the page was answered.
    struct DocsPage {
        ipc: Ipc,
        core: Arc<Waiting>,
        sender: Sender,
        created: RendererGeneration,
        lease: MediaViewLease,
        browser: BrowserIdentity,
        answered: Arc<Mutex<Vec<u64>>>,
    }

    impl DocsPage {
        fn open(label: &str) -> Self {
            let outlet = Arc::new(TauriOutlet::default());
            let views = Arc::new(ViewTable::new(outlet.clone(), Arc::new(Inline)));
            let window = shell_contract::ids::WindowId(1);
            let surface = shell_contract::Surface::Docs;
            let _lifecycle = views.add_window(window, label.into());
            let (view, created) = views
                .register_view(views.allocate_view().unwrap(), window, surface)
                .unwrap();
            let lease = MediaViewLease::new(view.0).unwrap();
            views.set_docs_media_lease(view, lease.clone()).unwrap();
            let (browser, origin) = registered_context(label);
            assert!(views.bind_verified_native_origin(view, &created, &origin));
            let answered = Arc::new(Mutex::new(Vec::new()));
            let seen = answered.clone();
            let stream = Channel::new(move |body| {
                if let InvokeResponseBody::Raw(bytes) = body
                    && let Ok((header, _)) = unframe(&bytes)
                    && let Some(id) = header.get("reply").and_then(Value::as_u64)
                {
                    seen.lock().unwrap().push(id);
                }
                Ok(())
            });
            outlet.attach(view, stream, created.clone()).unwrap();
            let core = Arc::new(Waiting::default());
            Self {
                ipc: Ipc {
                    core: core.clone(),
                    views,
                    outlet,
                    measure: None,
                },
                core,
                sender: Sender {
                    view,
                    window,
                    surface,
                },
                created,
                lease,
                browser,
                answered,
            }
        }

        /// The page invokes a channel: the call arrives and waits for the core thread.
        fn invokes(&self, id: u64) {
            self.invokes_channel(id, "docs:consume-new-blank");
        }

        fn invokes_channel(&self, id: u64, channel: &str) {
            let (views, view) = (&self.ipc.views, self.sender.view);
            let owner = native_owner::capture_owner(views, view, &self.created).unwrap();
            let mut encoder = Encoder::default();
            let args = encoder.values(&[]);
            let header = serde_json::json!({ "id": id, "channel": channel, "args": args });
            let bytes = frame(&header, &encoder.buffers);
            invoke_verified(&self.ipc, self.sender, owner, &bytes).unwrap();
        }

        /// The page invokes a channel whose handler keeps its responder, as one that answers
        /// when a worker is done does. The call is dispatched; its answer is the test's to give.
        fn invokes_a_slow_channel(&self, id: u64) -> Responder {
            const SLOW: &str = "app:font-family-info";
            struct Keeps(Rc<RefCell<Option<Responder>>>);
            impl Handler for Keeps {
                fn call(&self, _: &CallContext, _: Vec<IpcValue>, reply: Responder) {
                    *self.0.borrow_mut() = Some(reply);
                }
            }
            let kept = Rc::new(RefCell::new(None));
            let mut table = Table::default();
            let batch = shell_core::table::spec_of(SLOW).unwrap().batch;
            let handler = Rc::new(Keeps(kept.clone()));
            table.register(batch, SLOW, ChannelKind::Invoke, handler).unwrap();
            install_dispatcher(Dispatcher::new(table));
            self.invokes_channel(id, SLOW);
            self.core_thread_runs();
            // a later test of this thread starts without a table again
            DISPATCHER.with(|dispatcher| *dispatcher.borrow_mut() = None);
            let answer = kept.borrow_mut().take();
            answer.expect("the call was dispatched")
        }

        fn core_thread_runs(&self) {
            let jobs = std::mem::take(&mut *self.core.0.lock().unwrap());
            for job in jobs {
                job();
            }
        }

        fn answered(&self) -> Vec<u64> {
            self.answered.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_call_that_waits_while_its_page_loads_a_document_is_still_answered() {
        let page = DocsPage::open("posted-call-document-load");
        page.invokes(7);
        // the open ahead of it finishes first: the page shows another document now
        let next = page.lease.stage_document().unwrap();
        page.lease.commit_document(&next).unwrap();
        page.core_thread_runs();
        assert_eq!(page.answered(), [7]);
    }

    #[test]
    fn a_call_whose_page_is_gone_is_not_dispatched() {
        // the page left its document, and a successor took the view at the very same address
        let page = DocsPage::open("posted-call-navigation");
        page.invokes(8);
        let address = url("https://example.test/a");
        assert!(page.browser.core.admit_navigation(&address));
        page.browser.core.commit(&address);
        let successor = page.browser.core.register_document(TEST_NONCE).unwrap();
        assert!(page.ipc.views.bind_verified_native_origin(
            page.sender.view,
            &page.created,
            &successor
        ));
        page.core_thread_runs();
        assert!(page.answered().is_empty());

        // the view ended its document session
        let page = DocsPage::open("posted-call-closed");
        page.invokes(9);
        page.lease.revoke();
        page.core_thread_runs();
        assert!(page.answered().is_empty());
    }

    #[test]
    fn an_answer_that_comes_after_its_page_loaded_a_document_still_reaches_the_page() {
        // with nothing in between, the answer arrives
        let page = DocsPage::open("late-answer-at-once");
        let answer = page.invokes_a_slow_channel(20);
        assert!(page.answered().is_empty());
        answer.reply(Ok(IpcValue::Null));
        page.core_thread_runs();
        assert_eq!(page.answered(), [20]);

        // the page loads a document while the handler works, as the open of its tab does
        let page = DocsPage::open("late-answer-document-load");
        let answer = page.invokes_a_slow_channel(21);
        let next = page.lease.stage_document().unwrap();
        page.lease.commit_document(&next).unwrap();
        answer.reply(Ok(IpcValue::Null));
        page.core_thread_runs();
        assert_eq!(page.answered(), [21]);
    }

    #[test]
    fn an_answer_to_a_page_that_is_gone_reaches_nobody() {
        // the page left its document, and a successor took the view at the very same address
        let page = DocsPage::open("late-answer-navigation");
        let answer = page.invokes_a_slow_channel(22);
        let address = url("https://example.test/a");
        assert!(page.browser.core.admit_navigation(&address));
        page.browser.core.commit(&address);
        let successor = page.browser.core.register_document(TEST_NONCE).unwrap();
        assert!(page.ipc.views.bind_verified_native_origin(
            page.sender.view,
            &page.created,
            &successor
        ));
        answer.reply(Ok(IpcValue::Null));
        page.core_thread_runs();
        assert!(page.answered().is_empty());

        // the view ended its document session
        let page = DocsPage::open("late-answer-closed");
        let answer = page.invokes_a_slow_channel(23);
        page.lease.revoke();
        answer.reply(Ok(IpcValue::Null));
        page.core_thread_runs();
        assert!(page.answered().is_empty());
    }
}
