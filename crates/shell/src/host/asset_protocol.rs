//! Asynchronous native-provenance boundary for document-scoped asset schemes.

use std::sync::{Arc, Mutex};

use super::document_assets::{self, AssetKind, AssetPermit, TargetPermit};
use super::{epub_asset, html_preview};
use crate::native::{
    DocumentContext, RequestCancelSubscription, ResponseGate, SchemeRequestContext,
};
use crate::views::{
    DocsMediaAuthorization, NativeAssetAuthorization, SlidesMediaAuthorization, ViewTable,
};
use main_core::docs::byte_handoff::MediaPermit;
use main_core::docs::lazy_media::MediaAnswer;
use main_core::html::preview_protocol::PreviewEntry;
use shell_contract::Surface;
use shell_contract::{
    ipc::NativeRequestOwner,
    runtime::{CancelToken, RendererGeneration, Thread},
};
use shell_ipc_d2::epub::hosts::Bytes;
use tauri::http::{self, StatusCode};
use tauri::{AppHandle, UriSchemeResponder};

#[derive(Clone, Copy)]
pub enum Scheme {
    Docx,
    Html,
    Markdown,
    Slides,
    Preview,
    Epub,
}

type Reply = Arc<Mutex<Option<UriSchemeResponder>>>;

/// The start of a response a page of any origin may read, with the answer's own headers.
///
/// An answer that allows every origin itself (a handed-off document's bytes, a template font)
/// must not say so a second time. WebView2 is given one line per header, and Chromium refuses a
/// cross-origin response whose `Access-Control-Allow-Origin` holds more than one value; WebKit
/// is given a dictionary, where the second value replaces the first.
fn builder_of(status: u16, headers: Vec<(String, String)>) -> http::response::Builder {
    let allow_origin = http::header::ACCESS_CONTROL_ALLOW_ORIGIN;
    let mut builder = http::Response::builder()
        .status(status)
        .header(allow_origin.clone(), "*");
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case(allow_origin.as_str()) {
            builder = builder.header(name, value);
        }
    }
    builder
}

fn respond(reply: &Reply, answer: MediaAnswer) {
    let builder = builder_of(answer.status, answer.headers);
    let response = builder.body(answer.body).unwrap_or_else(|_| {
        let mut response = http::Response::new(Vec::new());
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        response
    });
    let (status, bytes) = (response.status().as_u16(), response.body().len());
    let responder = reply
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    crate::trace::trace!(
        "asset",
        "answered {status} ({bytes} bytes){}",
        if responder.is_some() {
            ""
        } else {
            ", but the request was already answered"
        }
    );
    if let Some(responder) = responder {
        responder.respond(response);
    }
}

fn denied(reply: &Reply) {
    respond(
        reply,
        MediaAnswer {
            status: 403,
            headers: Vec::new(),
            body: Vec::new(),
        },
    );
}

fn failed(reply: &Reply) {
    respond(
        reply,
        MediaAnswer {
            status: 500,
            headers: Vec::new(),
            body: Vec::new(),
        },
    );
}

/// An answer that is a status and nothing else.
fn bare(status: u16) -> MediaAnswer {
    MediaAnswer {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

struct Request {
    label: String,
    context: SchemeRequestContext,
    uri: String,
    accept: Option<String>,
    range: Option<String>,
    reply: Reply,
    response: ResponseGate,
    cancel: CancelToken,
    _cancellation: RequestCancelSubscription,
}

#[derive(Clone)]
enum Authorization {
    Docs(DocsMediaAuthorization),
    Asset(NativeAssetAuthorization, AssetPermit),
    Slides(SlidesMediaAuthorization),
    Target(Box<TargetPermit>),
    /// The HTML editor's preview page. `None`: no buffer of this view for this requester.
    Preview(NativeAssetAuthorization, Option<Box<PreviewEntry>>),
    Epub(EpubAsset),
}

/// One entry of an open book, asked for by the document of the view that owns the book.
#[derive(Clone)]
struct EpubAsset {
    origin: DocumentContext,
    renderer: RendererGeneration,
    /// `None`: the token names no open book of this view.
    book: Option<Bytes>,
    head: bool,
    path: String,
}

impl Authorization {
    fn current(&self) -> bool {
        match self {
            Self::Docs(auth) => auth.is_current(),
            Self::Asset(auth, permit) => auth.is_current() && permit.is_current().unwrap_or(false),
            Self::Slides(auth) => auth.is_current(),
            Self::Target(permit) => permit.is_current().unwrap_or(false),
            Self::Preview(auth, _) => auth.is_current(),
            Self::Epub(asset) => asset.origin.is_live() && asset.renderer.is_active(),
        }
    }

    /// Ordinary document leases are already in the canonical native owner
    /// admission. Only asset grant state needs its separate short registry gate.
    fn with_current_delivery(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Docs(_) | Self::Slides(_) | Self::Preview(..) | Self::Epub(_) => effect(),
            Self::Asset(_, permit) => permit.with_current_delivery(effect),
            Self::Target(permit) => permit.with_current_delivery(effect),
        }
    }
}

fn original_slide_uri(uri: &str) -> Option<String> {
    let url = tauri::Url::parse(uri).ok()?;
    match url.scheme() {
        "seuoffice-slide-media" => {
            // wry hands a handler `<scheme>://localhost/<authority>/<rest>` on Windows for the
            // page's `http://<scheme>.localhost/<authority>/<rest>` (see
            // `document_assets::docx_original_uri`)
            if !(cfg!(windows) && url.host_str() == Some("localhost")) {
                return Some(uri.to_owned());
            }
        }
        "http" if url.host_str()? == "seuoffice-slide-media.localhost" => {}
        _ => return None,
    }
    let path = url.path().strip_prefix('/')?;
    let (authority, rest) = path.split_once('/')?;
    if authority.is_empty() || authority.contains('%') {
        return None;
    }
    let mut original = format!("seuoffice-slide-media://{authority}/{rest}");
    if let Some(query) = url.query() {
        original.push('?');
        original.push_str(query);
    }
    Some(original)
}

fn execute(request: Request, auth: Authorization, owner: Option<NativeRequestOwner>) {
    tauri::async_runtime::spawn_blocking(move || {
        if !auth.current() {
            denied(&request.reply);
            return;
        }
        let result = match &auth {
            Authorization::Docs(auth) => document_assets::docx_original_uri(&request.uri)
                .ok_or_else(|| "invalid document media URL".to_owned())
                .and_then(|uri| {
                    super::d2::docx_media_response_for_permit(
                        auth.permit.clone(),
                        &uri,
                        request.cancel.clone(),
                    )
                }),
            Authorization::Asset(_, permit) => document_assets::handle_permit(
                permit.clone(),
                &request.uri,
                request.accept.as_deref(),
                request.range.as_deref(),
            ),
            Authorization::Target(permit) => document_assets::handle_target(
                (**permit).clone(),
                &request.uri,
                request.accept.as_deref(),
                request.range.as_deref(),
            ),
            Authorization::Slides(auth) => Ok(
                match original_slide_uri(&request.uri)
                    .and_then(|uri| super::d1::slides_media::read(auth.view, &auth.lease(), &uri))
                {
                    Some(media) => MediaAnswer {
                        status: 200,
                        headers: vec![
                            ("Content-Length".into(), media.body.len().to_string()),
                            ("Cache-Control".into(), "no-store".into()),
                            ("Content-Type".into(), media.mime),
                        ],
                        body: media.body,
                    },
                    None => MediaAnswer {
                        status: 404,
                        headers: Vec::new(),
                        body: Vec::new(),
                    },
                },
            ),
            Authorization::Preview(_, entry) => Ok(entry
                .as_deref()
                .map_or_else(|| bare(404), html_preview::answer)),
            Authorization::Epub(asset) => match &asset.book {
                Some(book) => epub_asset::serve(book.clone(), asset.head, asset.path.clone()),
                None => Ok(epub_asset::refusal(404)),
            },
        };
        deliver(request, auth, owner, result);
    });
}

fn deliver(
    request: Request,
    auth: Authorization,
    owner: Option<NativeRequestOwner>,
    result: Result<MediaAnswer, String>,
) {
    let answer = match result {
        Ok(answer) => answer,
        Err(_) => {
            failed(&request.reply);
            return;
        }
    };
    crate::trace::trace!(
        "asset",
        "'{}' is answered {} ({} bytes)",
        request.label,
        answer.status,
        answer.body.len()
    );
    let builder = builder_of(answer.status, answer.headers);
    let response = match builder.body(std::borrow::Cow::Owned(answer.body)) {
        Ok(response) => response,
        Err(_) => {
            failed(&request.reply);
            return;
        }
    };
    let prepared = match request.response.prepare(response) {
        Ok(prepared) => prepared,
        Err(_) => {
            denied(&request.reply);
            return;
        }
    };
    let Some(responder) = request
        .reply
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    else {
        return;
    };
    let mut pending = Some((prepared, responder));
    let mut accepted = None;
    let mut accept = || {
        let (prepared, responder) = pending
            .take()
            .ok_or("The resource response was already consumed.")?;
        accepted = Some(prepared.accept(responder));
        Ok(())
    };
    if let Some(owner) = &owner {
        // The captured request token was matched to this exact registered owner
        // before IO. This canonical gate also retains the original app lease.
        let _ = crate::ipc::native_owner::with_paired_effect(
            owner,
            owner,
            &request.cancel,
            &request.cancel,
            || auth.with_current_delivery(&mut accept),
        );
    } else {
        // Only a pre-Registered hidden root may use its original one-shot
        // target capability instead of a V8 owner. Its target gate is mandatory.
        if matches!(&auth, Authorization::Target(_)) {
            let _ = auth.with_current_delivery(&mut accept);
        }
    }
    match accepted {
        Some(Ok(answer)) => {
            let _ = answer.submit();
        }
        Some(Err(rejected)) => {
            *request
                .reply
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(rejected.into_responder());
            denied(&request.reply);
        }
        None => {
            if let Some((_, responder)) = pending {
                *request
                    .reply
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(responder);
                denied(&request.reply);
            }
        }
    }
}

/// Register each scheme with a closure that supplies its trusted table and core thread.
#[allow(clippy::too_many_arguments)]
pub fn handle(
    scheme: Scheme,
    _app: AppHandle,
    views: Arc<ViewTable>,
    core: Arc<dyn Thread>,
    label: String,
    context: SchemeRequestContext,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let reply = Arc::new(Mutex::new(Some(responder)));
    let method = request.method().clone();
    // the start of the address only: a hand-off address ends in its one-time token
    crate::trace::trace!(
        "asset",
        "'{label}' {method} {}",
        request
            .uri()
            .to_string()
            .chars()
            .take(48)
            .collect::<String>()
    );
    let response = context.response_gate();
    let cancel = CancelToken::default();
    let cancelled = cancel.clone();
    let cancellation = match context.on_cancel(move || cancelled.cancel()) {
        Ok(subscription) => subscription,
        Err(_) => {
            denied(&reply);
            return;
        }
    };
    let request = Request {
        label,
        context,
        response,
        cancel,
        _cancellation: cancellation,
        uri: request.uri().to_string(),
        accept: request
            .headers()
            .get(http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        range: request
            .headers()
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        reply,
    };
    if matches!(scheme, Scheme::Html | Scheme::Markdown) {
        let target = match document_assets::is_target_browser(&request.context) {
            Ok(target) => target,
            Err(_) => {
                denied(&request.reply);
                return;
            }
        };
        if target {
            let permit = match document_assets::authorize_target(&request.uri, &request.context) {
                Ok(Some(permit)) => permit,
                Ok(None) => {
                    denied(&request.reply);
                    return;
                }
                Err(_) => {
                    failed(&request.reply);
                    return;
                }
            };
            execute(request, Authorization::Target(Box::new(permit)), None);
            return;
        }
    }
    // what the address names is decided before any authority is spent on it, as Electron's
    // handlers decided it before they looked anything up
    let preview_host = match scheme {
        Scheme::Preview => match html_preview::view_of(&request.uri) {
            Ok(view) => Some(view),
            Err(status) => {
                respond(&request.reply, bare(status));
                return;
            }
        },
        _ => None,
    };
    let epub_target = match scheme {
        Scheme::Epub if method != http::Method::GET && method != http::Method::HEAD => {
            respond(&request.reply, epub_asset::refusal(405));
            return;
        }
        Scheme::Epub => match epub_asset::parse(&request.uri) {
            Ok(target) => Some(target),
            Err(status) => {
                respond(&request.reply, epub_asset::refusal(status));
                return;
            }
        },
        _ => None,
    };
    let Some(origin) = request.context.current_document_context() else {
        denied(&request.reply);
        return;
    };
    core.post(Box::new(move || {
        let failure = request.reply.clone();
        let ran = super::w::on_core_local(move |provided| {
            let Some((sender, renderer, _)) =
                views.authorize_verified_native_origin(&request.label, &origin)
            else {
                denied(&request.reply);
                return;
            };
            let Some(captured) = views.capture_native_owner(sender.view, &renderer) else {
                denied(&request.reply);
                return;
            };
            if captured.origin != origin {
                denied(&request.reply);
                return;
            }
            let browser_owner = captured.browser_owner;
            let auth = match scheme {
                Scheme::Docx => views
                    .authorize_docs_media(&request.label, &request.context)
                    .filter(|auth| auth.view == sender.view)
                    .and_then(|auth| {
                        provided
                            .all::<shell_ipc_d2::docs::editors::DocsEditors>()
                            .into_iter()
                            .next()
                            .and_then(|editor| editor.media_permit(auth.view))
                            .filter(|permit: &MediaPermit| permit.same_session(&auth.permit))
                            .and_then(|_| {
                                crate::ipc::native_owner::owner_with_browser(
                                    auth.view,
                                    origin.clone(),
                                    renderer.clone(),
                                    auth.clone(),
                                    browser_owner.clone(),
                                )
                                .map(|owner| (Authorization::Docs(auth), owner))
                            })
                    }),
                Scheme::Html | Scheme::Markdown => views
                    .authorize_verified_native_asset_origin(&request.label, &origin)
                    .and_then(|(_, auth, asset_renderer)| {
                        if auth.view != sender.view || !asset_renderer.same_generation(&renderer) {
                            return None;
                        }
                        let kind = match (scheme, auth.surface) {
                            (Scheme::Html, Surface::Html) => AssetKind::Html,
                            (Scheme::Markdown, Surface::Markdown) => AssetKind::Markdown,
                            _ => return None,
                        };
                        let owner = crate::ipc::native_owner::asset_owner_with_browser(
                            auth.view,
                            origin.clone(),
                            auth.clone(),
                            asset_renderer,
                            browser_owner.clone(),
                        )?;
                        document_assets::authorize(auth.view, kind)
                            .ok()
                            .flatten()
                            .map(|permit| (Authorization::Asset(auth, permit), owner))
                    }),
                Scheme::Slides => views
                    .authorize_slides_media(&request.label, &request.context)
                    .filter(|auth| auth.view == sender.view)
                    .and_then(|auth| {
                        crate::ipc::native_owner::owner_with_browser(
                            auth.view,
                            origin.clone(),
                            renderer.clone(),
                            auth.lease(),
                            browser_owner.clone(),
                        )
                        .map(|owner| (Authorization::Slides(auth), owner))
                    }),
                Scheme::Preview => preview_host.and_then(|host| {
                    let (_, auth, asset_renderer) =
                        views.authorize_verified_native_asset_origin(&request.label, &origin)?;
                    if auth.view != sender.view
                        || auth.surface != Surface::Html
                        || !asset_renderer.same_generation(&renderer)
                    {
                        return None;
                    }
                    // a buffer that is not this view's to read is answered as no view, as
                    // Electron answered a host that named none
                    let entry = provided
                        .all::<shell_ipc_d2::html::HtmlEditors>()
                        .into_iter()
                        .next()
                        .and_then(|editors| editors.preview_entry(auth.view, host))
                        .map(Box::new);
                    crate::ipc::native_owner::asset_owner_with_browser(
                        auth.view,
                        origin.clone(),
                        auth.clone(),
                        asset_renderer,
                        browser_owner.clone(),
                    )
                    .map(|owner| (Authorization::Preview(auth, entry), owner))
                }),
                Scheme::Epub => epub_target.and_then(|target| {
                    if captured.surface != Surface::Epub {
                        return None;
                    }
                    // a token the view does not own is answered as no book
                    let book = provided
                        .all::<shell_ipc_d2::epub::EpubEditors>()
                        .into_iter()
                        .next()
                        .and_then(|editors| editors.asset_book(sender.view, &target.token));
                    crate::ipc::native_owner::owner_with_browser(
                        sender.view,
                        origin.clone(),
                        renderer.clone(),
                        (),
                        browser_owner.clone(),
                    )
                    .map(|owner| {
                        let asset = EpubAsset {
                            origin: origin.clone(),
                            renderer: renderer.clone(),
                            book,
                            head: method == http::Method::HEAD,
                            path: target.path,
                        };
                        (Authorization::Epub(asset), owner)
                    })
                }),
            };
            if let Some((auth, owner)) = auth {
                execute(request, auth, Some(owner));
            } else {
                denied(&request.reply);
            }
        });
        if !ran {
            denied(&failure);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::{builder_of, original_slide_uri};
    use tauri::http;

    #[test]
    fn an_answer_that_allows_every_origin_itself_says_so_once() {
        let response = builder_of(
            200,
            vec![
                ("Content-Type".into(), "application/octet-stream".into()),
                ("Access-Control-Allow-Origin".into(), "*".into()),
            ],
        )
        .body(Vec::<u8>::new())
        .unwrap();
        let allowed = response
            .headers()
            .get_all(http::header::ACCESS_CONTROL_ALLOW_ORIGIN);
        assert_eq!(allowed.iter().count(), 1);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }

    #[test]
    fn slide_url_restores_only_the_registered_wire_origin() {
        let original = "seuoffice-slide-media://archive/id?mime=image%2Fpng";
        assert_eq!(original_slide_uri(original).as_deref(), Some(original));
        assert_eq!(
            original_slide_uri(
                "http://seuoffice-slide-media.localhost/archive/id?mime=image%2Fpng"
            )
            .as_deref(),
            Some(original)
        );
        assert!(original_slide_uri("http://other.localhost/archive/id").is_none());
        assert!(
            original_slide_uri("http://seuoffice-slide-media.localhost/%61rchive/id").is_none()
        );
    }
}
