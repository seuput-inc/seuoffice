//! `ipcRenderer.sendSync` (the stage 3 shell contract, section 2): a
//! synchronous XHR of the bridge to the scheme `seuoffice-ipc`. The CEF
//! runtime serves a custom scheme at `http://<scheme>.localhost/`, so the
//! request is `POST http://seuoffice-ipc.localhost/<channel>` (the bridge
//! builds the URL with Tauri's `convertFileSrc`); the body is the frame
//! `{args}`, the answer the frame `{value}` or `{error}`.
//!
//! Besides the table's one `sendSync` channel the scheme answers three
//! reserved channels that are not in the table, on any thread, from the
//! sender's view record: the drop paths of `webUtils.getPathForFile` and the
//! zoom factor of `webFrame`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shell_contract::error::IpcError;
use shell_contract::ipc::{ChannelKind, NativeRequestOwner, Reply, Sender};
use shell_contract::value::IpcValue;
use shell_contract::runtime::{CancelSubscription, CancelToken};
use shell_core::wire::{Encoder, decode_list, frame, unframe};
use tauri::http::{self, Method, StatusCode};
use tauri::{AppHandle, Manager, Runtime, UriSchemeContext, UriSchemeResponder};
use tauri_runtime_cef::WebviewCefExt;

use super::{Ipc, dispatch};
use crate::trace::trace;
use crate::{ipc::native_owner::NativeOwnerAuthority, views::NativeReservedViewState};

pub const SCHEME: &str = "seuoffice-ipc";

/// `webUtils.getPathForFile`: the paths of the sender's latest drop, in order
const DROPPED_PATHS: &str = "webutils:dropped-paths";
const SET_ZOOM_FACTOR: &str = "webframe:set-zoom-factor";
const GET_ZOOM_FACTOR: &str = "webframe:get-zoom-factor";

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
        let callback = completion.clone();
        let queued = webview.with_cef_webview_async(move |result| {
            use cef::{ImplBrowser, ImplBrowserHost};
            let reply = result.ok().and_then(|view| {
                if view.snapshot().browser_id != origin.browser_id()
                    || !view.frame_navigation_state().is_same_browser(origin.browser_state()) { return None; }
                origin.with_live(|| renderer.with_active(|| loss.with_active(|| {
                    match channel.as_str() {
                        DROPPED_PATHS => Ok(IpcValue::Array(reserved_state.dropped().into_iter()
                            .map(|path| IpcValue::String(path.to_string_lossy().into_owned())).collect())),
                        GET_ZOOM_FACTOR => Ok(IpcValue::Number(reserved_state.zoom())),
                        SET_ZOOM_FACTOR => {
                            if let Some(IpcValue::Number(factor)) = args.first()
                                && factor.is_finite() && *factor > 0.0 {
                                let Some(host) = view.browser().host() else { return Err(revoked()) };
                                host.set_zoom_level(factor.ln() / 1.2_f64.ln());
                                reserved_state.set_zoom(*factor);
                            }
                            Ok(IpcValue::Undefined)
                        }
                        _ => Err(revoked()),
                    }
                }))).flatten().flatten()
            }).unwrap_or_else(|| Err(revoked()));
            callback.finish(reply);
        });
        if queued.is_err() { completion.finish(Err(revoked())); }
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

pub fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    if request.method() == Method::OPTIONS {
        responder.respond(response(StatusCode::NO_CONTENT, Vec::new()));
        return;
    }
    let ipc = ctx.app_handle().state::<Ipc>();
    let Ok((sender, native)) = ipc.call_sender_label(ctx.webview_label()) else {
        trace!("sync", "refused: '{}' has no bridge", ctx.webview_label());
        responder.respond(text_error(
            StatusCode::FORBIDDEN,
            "this webview has no bridge to the shell",
        ));
        return;
    };
    let channel =
        percent_encoding::percent_decode_str(request.uri().path().trim_start_matches('/'))
            .decode_utf8_lossy()
            .into_owned();
    trace!(
        "sync",
        "view {}: '{channel}' ({} bytes)",
        sender.view.0,
        request.body().len()
    );
    ipc.note_call();
    let args = match decode_args(request.body()) {
        Ok(args) => args,
        Err(error) => {
            responder.respond(text_error(StatusCode::BAD_REQUEST, &error.to_string()));
            return;
        }
    };

    if let Some(value) = reserved(ctx.app_handle(), &ipc, sender, &channel, &args) {
        responder.respond(response(StatusCode::OK, answer_frame(&Ok(value))));
        return;
    }

    ipc.core.post(Box::new(move || {
        dispatch(
            sender,
            ChannelKind::SendSync,
            None,
            native,
            channel,
            args,
            Box::new(move |reply| {
                responder.respond(response(StatusCode::OK, answer_frame(&reply)))
            }),
        );
    }));
}

/// The reserved channels, or none when `channel` is a table channel
fn reserved<R: Runtime>(
    app: &AppHandle<R>,
    ipc: &Ipc,
    sender: Sender,
    channel: &str,
    args: &[IpcValue],
) -> Option<IpcValue> {
    match channel {
        DROPPED_PATHS => Some(IpcValue::Array(
            ipc.views
                .dropped(sender.view)
                .into_iter()
                .map(|p| IpcValue::String(p.to_string_lossy().into_owned()))
                .collect(),
        )),
        SET_ZOOM_FACTOR => {
            if let Some(IpcValue::Number(factor)) = args.first()
                && factor.is_finite()
                && *factor > 0.0
            {
                ipc.views.set_zoom(sender.view, *factor);
                if let Some(label) = ipc.views.label_of_view(sender.view)
                    && let Some(webview) = app.get_webview(&label)
                    && let Err(error) = webview.set_zoom(*factor)
                {
                    eprintln!("shell: webFrame.setZoomFactor on '{label}': {error}");
                }
            }
            Some(IpcValue::Undefined)
        }
        GET_ZOOM_FACTOR => Some(IpcValue::Number(ipc.views.zoom(sender.view))),
        _ => None,
    }
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
}
