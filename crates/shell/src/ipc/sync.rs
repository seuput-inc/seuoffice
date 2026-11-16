//! The scheme `seuoffice-ipc` (the stage 3 shell contract, section 2). It serves three things
//! to the document bridge, each authenticated by the document nonce in the `x-seuoffice-doc`
//! header (never by the webview label alone):
//!
//! - `ipcRenderer.sendSync`: a synchronous XHR `POST <scheme>/<channel>`, body the frame
//!   `{args}`, answer the frame `{value}` or `{error}`. Only documents whose browser was built
//!   with the preload bridge may call it. The scheme is `seuoffice-ipc://localhost/` on macOS
//!   and `http://seuoffice-ipc.localhost/` on Windows; the bridge builds the URL with Tauri's
//!   `convertFileSrc`.
//! - `GET outbox/<documentId>`: the frames queued in the document's reply sink since the last
//!   pull (see [`encode_frames`]). The sink wakes the bridge with `__seuofficeWake`; replies
//!   never travel through `evaluateJavaScript`.
//! - `POST seuoffice:register`: registers the nonce synchronously, for a `sendSync` that runs
//!   before the bridge's own asynchronous `ipc_register` completed.
//!
//! Besides the table's one `sendSync` channel the scheme answers three reserved channels that
//! are not in the table, from the sender's view record: the drop paths of
//! `webUtils.getPathForFile` and the zoom factor of `webFrame`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shell_contract::error::IpcError;
use shell_contract::ipc::{ChannelKind, NativeRequestOwner, Reply, Sender};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use shell_contract::value::IpcValue;
use shell_core::wire::{Encoder, decode_list, frame, unframe};
use tauri::http::{self, HeaderMap, Method, StatusCode};
use tauri::{Manager, UriSchemeContext, UriSchemeResponder, Wry};

use super::{Ipc, NOT_READY, dispatch, register_document, verified_sender};
use crate::ipc::native_owner::NativeOwnerAuthority;
use crate::native::{self, BridgeMode, Native, ReplyFrame, WebviewExt};
use crate::trace::trace;
use crate::views::NativeReservedViewState;

pub const SCHEME: &str = "seuoffice-ipc";

/// `webUtils.getPathForFile`: the paths of the sender's latest drop, in order
const DROPPED_PATHS: &str = "webutils:dropped-paths";
const SET_ZOOM_FACTOR: &str = "webframe:set-zoom-factor";
const GET_ZOOM_FACTOR: &str = "webframe:get-zoom-factor";
const REGISTER: &str = "seuoffice:register";
const OUTBOX: &str = "outbox/";

fn response(
    status: StatusCode,
    body: impl Into<Cow<'static, [u8]>>,
) -> http::Response<Cow<'static, [u8]>> {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        // the page's origin is another host of `.localhost`: a cross-origin XHR
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(http::header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
        .header(http::header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .body(body.into())
        .expect("a response of fixed parts")
}

fn answer_frame(reply: &Reply) -> Vec<u8> {
    let mut e = Encoder::default();
    let header = match reply {
        Ok(value) => json!({ "value": e.value(value) }),
        // `sendSync` has no "Error invoking remote method" sentence: Electron's
        // listener threw, and the bridge rethrows the text
        Err(error) => json!({ "error": error.to_string() }),
    };
    frame(&header, &e.buffers)
}

fn decode_args(bytes: &[u8]) -> Result<Vec<IpcValue>, shell_core::wire::WireError> {
    let (header, buffers) = unframe(bytes)?;
    decode_list(header.get("args").unwrap_or(&Value::Null), &buffers)
}

struct SyncCompletion {
    reply: Mutex<Option<Box<dyn FnOnce(Vec<u8>) + Send>>>,
    _loss: Mutex<Option<CancelSubscription>>,
}

impl SyncCompletion {
    fn new(loss: &CancelToken, reply: Box<dyn FnOnce(Vec<u8>) + Send>) -> Arc<Self> {
        let completion = Arc::new(Self { reply: Mutex::new(Some(reply)), _loss: Mutex::new(None) });
        let weak = Arc::downgrade(&completion);
        let subscription = loss.on_cancel(move || {
            if let Some(completion) = weak.upgrade() {
                completion.finish(Err(IpcError { name: None, message: "Renderer document is no longer available.".into() }));
            }
        });
        *completion._loss.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription);
        completion
    }

    fn finish(&self, reply: Reply) {
        let deliver = self.reply.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(deliver) = deliver { deliver(answer_frame(&reply)); }
    }
}

fn revoked() -> IpcError {
    IpcError { name: None, message: "Renderer document is no longer available.".into() }
}

/// The caller supplies one already authenticated owner and its captured view objects.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_verified(
    ipc: &Ipc,
    sender: Sender,
    owner: NativeRequestOwner,
    reserved_state: NativeReservedViewState,
    webview: tauri::Webview,
    channel: String,
    bytes: &[u8],
    reply: Box<dyn FnOnce(Vec<u8>) + Send>,
) {
    let Some(loss) = owner.loss_token() else { reply(answer_frame(&Err(revoked()))); return };
    let completion = SyncCompletion::new(&loss, reply);
    let Some(origin) = owner.context.authority_ref::<NativeOwnerAuthority>().map(|authority| authority.origin.clone()) else {
        completion.finish(Err(revoked()));
        return;
    };
    let Some(renderer) = owner.context.renderer().cloned() else { completion.finish(Err(revoked())); return };
    if owner.view != sender.view { completion.finish(Err(revoked())); return; }
    let args = match decode_args(bytes) {
        Ok(args) => args,
        Err(error) => { completion.finish(Err(IpcError { name: None, message: error.to_string() })); return; }
    };
    ipc.note_call();
    if matches!(channel.as_str(), DROPPED_PATHS | SET_ZOOM_FACTOR | GET_ZOOM_FACTOR) {
        // the webview must still be the browser this document was registered on
        let current = webview.native_browser()
            .is_ok_and(|browser| origin.browser().is_same_browser(&browser));
        let mut zoom = None;
        let reply = if current {
            origin.with_live(|| renderer.with_active(|| loss.with_active(|| {
                match channel.as_str() {
                    DROPPED_PATHS => Ok(IpcValue::Array(reserved_state.dropped().into_iter()
                        .map(|path| IpcValue::String(path.to_string_lossy().into_owned())).collect())),
                    GET_ZOOM_FACTOR => Ok(IpcValue::Number(reserved_state.zoom())),
                    SET_ZOOM_FACTOR => {
                        if let Some(IpcValue::Number(factor)) = args.first()
                            && factor.is_finite() && *factor > 0.0 {
                            reserved_state.set_zoom(*factor);
                            zoom = Some(*factor);
                        }
                        Ok(IpcValue::Undefined)
                    }
                    _ => Err(revoked()),
                }
            }))).flatten().flatten().unwrap_or_else(|| Err(revoked()))
        } else {
            Err(revoked())
        };
        // the page zoom is applied after every authority gate ended
        let reply = match (zoom, reply) {
            (Some(factor), Ok(value)) => webview.set_zoom(factor).map(|()| value)
                .map_err(|error| IpcError { name: None, message: error.to_string() }),
            (_, reply) => reply,
        };
        completion.finish(reply);
        return;
    }
    ipc.core.post(Box::new(move || {
        let admitted = origin.with_live(|| renderer.with_active(|| loss.with_active(|| ()))).flatten().flatten().is_some();
        if !admitted { completion.finish(Err(revoked())); return; }
        let callback = completion.clone();
        dispatch(sender, ChannelKind::SendSync, None, Some(owner.context), channel, args, Box::new(move |reply| {
            let current = origin.with_live(|| renderer.with_active(|| loss.with_active(|| ()))).flatten().flatten().is_some();
            callback.finish(if current { reply } else { Err(revoked()) });
        }));
    }));
}

fn text_error(status: StatusCode, text: &str) -> http::Response<Cow<'static, [u8]>> {
    response(
        status,
        answer_frame(&Err(IpcError {
            name: None,
            message: text.into(),
        })),
    )
}

const FRAME_HEADER: usize = 17;

/// The outbox wire format the bridge parses (`invoke-system.js`, `deliverFrames`), per frame,
/// little endian: `u32 callback`, `u8 flags` (1 channel message, 2 raw body, 4 channel end),
/// `u64 index`, `u32 length`, then `length` body bytes. A plain reply has no channel flag and
/// index 0.
pub(crate) fn encode_frames(frames: &[ReplyFrame]) -> Vec<u8> {
    let total = frames.iter().map(|frame| FRAME_HEADER + frame.body.len()).sum();
    let mut out = Vec::with_capacity(total);
    for frame in frames {
        let (mut flags, index) = match frame.channel {
            None => (0u8, 0u64),
            Some((index, end)) => (1 | if end { 4 } else { 0 }, index),
        };
        if frame.raw {
            flags |= 2;
        }
        let length = u32::try_from(frame.body.len()).expect("a queued frame is far below 4 GiB");
        out.extend_from_slice(&frame.callback.to_le_bytes());
        out.push(flags);
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&frame.body);
    }
    out
}

/// `seuoffice:register`: the nonce of a document whose asynchronous registration is in flight.
fn register_sync(native: &Native, label: &str, headers: &HeaderMap) -> http::Response<Cow<'static, [u8]>> {
    let Some(nonce) = native::ingress::nonce_of(headers) else {
        return text_error(StatusCode::FORBIDDEN, NOT_READY);
    };
    let Some(browser) = native.browser(label) else {
        return text_error(StatusCode::FORBIDDEN, NOT_READY);
    };
    if browser.bridge_mode() != BridgeMode::Preload {
        return text_error(StatusCode::FORBIDDEN, "this webview has no synchronous bridge");
    }
    match register_document(&browser, nonce) {
        Ok(registered) => response(StatusCode::OK, registered.to_string().into_bytes()),
        Err(error) => text_error(StatusCode::CONFLICT, &error),
    }
}

/// `outbox/<documentId>`: take the frames queued for the document the nonce names.
fn outbox(native: &Native, label: &str, headers: &HeaderMap, id: &str) -> http::Response<Cow<'static, [u8]>> {
    let Some(context) = native::ingress::redeem(native, label, headers) else {
        return text_error(StatusCode::FORBIDDEN, NOT_READY);
    };
    if id.parse::<u64>().ok() != Some(context.identity().id().get()) {
        return text_error(StatusCode::CONFLICT, "Renderer document is no longer available.");
    }
    let Some(sink) = context.reply_sink() else {
        return text_error(StatusCode::FORBIDDEN, "Renderer document is no longer available.");
    };
    response(StatusCode::OK, encode_frames(&sink.drain()))
}

pub fn handle(
    ctx: UriSchemeContext<'_, Wry>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    if request.method() == Method::OPTIONS {
        responder.respond(response(StatusCode::NO_CONTENT, Vec::new()));
        return;
    }
    let app = ctx.app_handle();
    let label = ctx.webview_label();
    let Some(natives) = app.try_state::<Native>() else {
        responder.respond(text_error(StatusCode::FORBIDDEN, "the native module is not installed"));
        return;
    };
    let channel =
        percent_encoding::percent_decode_str(request.uri().path().trim_start_matches('/'))
            .decode_utf8_lossy()
            .into_owned();
    if channel == REGISTER {
        responder.respond(register_sync(&natives, label, request.headers()));
        return;
    }
    if let Some(id) = channel.strip_prefix(OUTBOX) {
        responder.respond(outbox(&natives, label, request.headers(), id));
        return;
    }
    // Everything else needs the registered document and the preload bridge, and is refused
    // before any state is read.
    let refuse = |text: &str| {
        trace!("sync", "refused: '{label}': {text}");
        text_error(StatusCode::FORBIDDEN, text)
    };
    let Some(context) = native::ingress::redeem(&natives, label, request.headers()) else {
        responder.respond(refuse(NOT_READY));
        return;
    };
    if context.browser().bridge_mode() != BridgeMode::Preload {
        responder.respond(refuse("this webview has no synchronous bridge"));
        return;
    }
    let (Some(ipc), Some(webview)) = (app.try_state::<Ipc>(), app.get_webview(label)) else {
        responder.respond(refuse(NOT_READY));
        return;
    };
    let Ok((sender, owner, reserved)) = verified_sender(&ipc, label, &context) else {
        responder.respond(refuse(NOT_READY));
        return;
    };
    trace!("sync", "view {}: '{channel}' ({} bytes)", sender.view.0, request.body().len());
    handle_verified(
        &ipc, sender, owner, reserved, webview, channel, request.body(),
        Box::new(move |bytes| responder.respond(response(StatusCode::OK, bytes))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn cancellation_and_response_settle_only_once() {
        let loss = CancelToken::default();
        let (tx, rx) = mpsc::channel();
        let completion = SyncCompletion::new(&loss, Box::new(move |frame| { tx.send(frame).unwrap(); }));
        loss.cancel();
        completion.finish(Ok(IpcValue::Number(5.0)));
        let (header, _) = unframe(&rx.recv().unwrap()).unwrap();
        assert_eq!(header["error"], "Renderer document is no longer available.");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn completed_response_is_not_replaced_by_later_cancellation() {
        let loss = CancelToken::default();
        let (tx, rx) = mpsc::channel();
        let completion = SyncCompletion::new(&loss, Box::new(move |frame| { tx.send(frame).unwrap(); }));
        completion.finish(Ok(IpcValue::Number(5.0)));
        loss.cancel();
        let (header, _) = unframe(&rx.recv().unwrap()).unwrap();
        assert_eq!(header["value"], 5.0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn outbox_frames_carry_callback_flags_index_and_length() {
        let frames = [
            ReplyFrame { callback: 7, channel: Some((3, false)), raw: true, body: vec![9, 8] },
            ReplyFrame { callback: 8, channel: Some((4, true)), raw: false, body: Vec::new() },
            ReplyFrame { callback: 9, channel: None, raw: false, body: b"{}".to_vec() },
        ];
        let bytes = encode_frames(&frames);
        assert_eq!(bytes.len(), 3 * FRAME_HEADER + 4);
        assert_eq!(&bytes[..4], &7u32.to_le_bytes());
        assert_eq!(bytes[4], 1 | 2);
        assert_eq!(&bytes[5..13], &3u64.to_le_bytes());
        assert_eq!(&bytes[13..17], &2u32.to_le_bytes());
        assert_eq!(&bytes[17..19], &[9, 8]);
        let second = 19;
        assert_eq!(bytes[second + 4], 1 | 4);
        assert_eq!(&bytes[second + 5..second + 13], &4u64.to_le_bytes());
        let third = second + FRAME_HEADER;
        assert_eq!(bytes[third + 4], 0);
        assert_eq!(&bytes[third + 5..third + 13], &0u64.to_le_bytes());
        assert_eq!(&bytes[third + FRAME_HEADER..], b"{}");
    }
}
