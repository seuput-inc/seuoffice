//! Asynchronous native-provenance boundary for document-scoped asset schemes.

use std::sync::{Arc, Mutex};

use main_core::docs::byte_handoff::MediaPermit;
use main_core::docs::lazy_media::MediaAnswer;
use shell_contract::{ipc::NativeRequestOwner, runtime::{CancelToken, Thread}};
use shell_contract::Surface;
use tauri::http::{self, StatusCode};
use tauri::{AppHandle, UriSchemeResponder};
use tauri_runtime_cef::{NativeSchemeCancelSubscription, NativeSchemeResponseTarget, SchemeRequestProvenance};

use crate::views::{DocsMediaAuthorization, NativeAssetAuthorization, SlidesMediaAuthorization, ViewTable};
use super::document_assets::{self, AssetKind, AssetPermit, TargetPermit};

#[derive(Clone, Copy)]
pub enum Scheme { Docx, Html, Markdown, Slides }

type Reply = Arc<Mutex<Option<UriSchemeResponder>>>;

fn respond(reply: &Reply, answer: MediaAnswer) {
    let mut builder = http::Response::builder().status(answer.status)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    for (name, value) in answer.headers { builder = builder.header(name, value); }
    let response = builder.body(answer.body).unwrap_or_else(|_| {
        let mut response = http::Response::new(Vec::new());
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        response
    });
    let responder = reply.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(responder) = responder { responder.respond(response); }
}

fn denied(reply: &Reply) {
    respond(reply, MediaAnswer { status: 403, headers: Vec::new(), body: Vec::new() });
}

fn failed(reply: &Reply) {
    respond(reply, MediaAnswer { status: 500, headers: Vec::new(), body: Vec::new() });
}

struct Request {
    label: String,
    provenance: SchemeRequestProvenance,
    uri: String,
    accept: Option<String>,
    range: Option<String>,
    reply: Reply,
    response: NativeSchemeResponseTarget,
    cancel: CancelToken,
    _cancellation: NativeSchemeCancelSubscription,
}


#[derive(Clone)]
enum Authorization {
    Docs(DocsMediaAuthorization),
    Asset(NativeAssetAuthorization, AssetPermit),
    Slides(SlidesMediaAuthorization),
    Target(TargetPermit),
}

impl Authorization {
    fn current(&self) -> bool {
        match self {
            Self::Docs(auth) => auth.is_current(),
            Self::Asset(auth, permit) => auth.is_current() && permit.is_current().unwrap_or(false),
            Self::Slides(auth) => auth.is_current(),
            Self::Target(permit) => permit.is_current().unwrap_or(false),
        }
    }

    /// Ordinary document leases are already in the canonical native owner
    /// admission. Only asset grant state needs its separate short registry gate.
    fn with_current_delivery(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match self {
            Self::Docs(_) | Self::Slides(_) => effect(),
            Self::Asset(_, permit) => permit.with_current_delivery(effect),
            Self::Target(permit) => permit.with_current_delivery(effect),
        }
    }
}

fn original_slide_uri(uri: &str) -> Option<String> {
    let url = tauri::Url::parse(uri).ok()?;
    if url.scheme() == "seuoffice-slide-media" { return Some(uri.to_owned()); }
    if url.scheme() != "http" || url.host_str()? != "seuoffice-slide-media.localhost" { return None; }
    let path = url.path().strip_prefix('/')?;
    let (authority, rest) = path.split_once('/')?;
    if authority.is_empty() || authority.contains('%') { return None; }
    let mut original = format!("seuoffice-slide-media://{authority}/{rest}");
    if let Some(query) = url.query() { original.push('?'); original.push_str(query); }
    Some(original)
}

fn execute(request: Request, auth: Authorization, owner: Option<NativeRequestOwner>) {
    tauri::async_runtime::spawn_blocking(move || {
        if !auth.current() { denied(&request.reply); return; }
        let result = match &auth {
            Authorization::Docs(auth) => document_assets::docx_original_uri(&request.uri)
                .ok_or_else(|| "invalid document media URL".to_owned())
                .and_then(|uri| super::d2::docx_media_response_for_permit(auth.permit.clone(), &uri, request.cancel.clone())),
            Authorization::Asset(_, permit) => document_assets::handle_permit(permit.clone(), &request.uri, request.accept.as_deref(), request.range.as_deref()),
            Authorization::Target(permit) => document_assets::handle_target(permit.clone(), &request.uri, request.accept.as_deref(), request.range.as_deref()),
            Authorization::Slides(auth) => Ok(match original_slide_uri(&request.uri)
                .and_then(|uri| super::d1::slides_media::read(auth.view, &auth.lease(), &uri)) {
                Some(media) => MediaAnswer { status: 200, headers: vec![
                    ("Content-Length".into(), media.body.len().to_string()),
                    ("Cache-Control".into(), "no-store".into()),
                    ("Content-Type".into(), media.mime),
                ], body: media.body },
                None => MediaAnswer { status: 404, headers: Vec::new(), body: Vec::new() },
            }),
        };
        deliver(request, auth, owner, result);
    });
}

fn deliver(request: Request, auth: Authorization, owner: Option<NativeRequestOwner>, result: Result<MediaAnswer, String>) {
    let answer = match result { Ok(answer) => answer, Err(_) => { failed(&request.reply); return; } };
    let mut builder = http::Response::builder().status(answer.status)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    for (name, value) in answer.headers { builder = builder.header(name, value); }
    let response = match builder.body(std::borrow::Cow::Owned(answer.body)) {
        Ok(response) => response, Err(_) => { failed(&request.reply); return; }
    };
    let prepared = match request.response.prepare(response) {
        Ok(prepared) => prepared, Err(_) => { denied(&request.reply); return; }
    };
    let Some(responder) = request.reply.lock().unwrap_or_else(|error| error.into_inner()).take() else { return; };
    let mut pending = Some((prepared, responder));
    let mut accepted = None;
    let mut accept = || {
        let (prepared, responder) = pending.take().ok_or("The resource response was already consumed.")?;
        accepted = Some(prepared.accept(responder));
        Ok(())
    };
    if let Some(owner) = &owner {
        // The captured request token was matched to this exact registered owner
        // before IO. This canonical gate also retains the original app lease.
        let _ = crate::ipc::native_owner::with_paired_effect(
            owner, owner, &request.cancel, &request.cancel,
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
        Some(Ok(answer)) => { let _ = answer.finish(); }
        Some(Err(rejected)) => {
            *request.reply.lock().unwrap_or_else(|error| error.into_inner()) = Some(rejected.into_responder());
            denied(&request.reply);
        }
        None => {
            if let Some((_, responder)) = pending {
                *request.reply.lock().unwrap_or_else(|error| error.into_inner()) = Some(responder);
                denied(&request.reply);
            }
        }
    }
}

/// Register each scheme with a closure that supplies its trusted table and core thread.
pub fn handle(
    scheme: Scheme,
    _app: AppHandle,
    views: Arc<ViewTable>,
    core: Arc<dyn Thread>,
    label: String,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let reply = Arc::new(Mutex::new(Some(responder)));
    let Some(provenance) = request.extensions().get::<SchemeRequestProvenance>().cloned() else { denied(&reply); return; };
    let Some(response) = request.extensions().get::<NativeSchemeResponseTarget>().cloned() else { denied(&reply); return; };
    let cancel = CancelToken::new();
    let cancelled = cancel.clone();
    let cancellation = match response.on_cancel(move || cancelled.cancel()) {
        Ok(subscription) => subscription, Err(_) => { denied(&reply); return; }
    };
    let request = Request {
        label, provenance, response, cancel, _cancellation: cancellation, uri: request.uri().to_string(),
        accept: request.headers().get(http::header::ACCEPT).and_then(|v| v.to_str().ok()).map(str::to_owned),
        range: request.headers().get(http::header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned),
        reply,
    };
    if matches!(scheme, Scheme::Html | Scheme::Markdown) {
        let target = match document_assets::is_target_browser(&request.provenance) {
            Ok(target) => target,
            Err(_) => { denied(&request.reply); return; }
        };
        if target {
            let permit = match document_assets::authorize_target(&request.uri, &request.provenance) {
                Ok(Some(permit)) => permit,
                Ok(None) => { denied(&request.reply); return; }
                Err(_) => { failed(&request.reply); return; }
            };
            execute(request, Authorization::Target(permit), None);
            return;
        }
    }
    let Some(origin) = request.provenance.native_document_owner().cloned() else { denied(&request.reply); return; };
        core.post(Box::new(move || {
            let failure = request.reply.clone();
            let ran = super::w::on_core_local(move |provided| {
                let Some((sender, renderer, _)) = views.authorize_verified_native_origin(&request.label, &origin) else {
                    denied(&request.reply); return;
                };
                let Some(captured) = views.capture_native_owner(sender.view, &renderer) else {
                    denied(&request.reply); return;
                };
                if captured.origin != origin {
                    denied(&request.reply); return;
                }
                let browser_owner = captured.browser_owner;
                let auth = match scheme {
                    Scheme::Docx => views.authorize_docs_media(&request.label, &request.provenance)
                        .filter(|auth| auth.view == sender.view)
                        .and_then(|auth| provided.all::<shell_ipc_d2::docs::editors::DocsEditors>().into_iter().next()
                            .and_then(|editor| editor.media_permit(auth.view))
                            .filter(|permit: &MediaPermit| permit.same_session(&auth.permit))
                            .and_then(|_| crate::ipc::native_owner::owner_with_browser(auth.view, origin.clone(), renderer.clone(), auth.clone(), browser_owner.clone())
                                .map(|owner| (Authorization::Docs(auth), owner)))),
                    Scheme::Html | Scheme::Markdown => views.authorize_verified_native_asset_origin(&request.label, &origin)
                        .and_then(|(_, auth, asset_renderer)| {
                            if auth.view != sender.view || !asset_renderer.same_generation(&renderer) { return None; }
                            let kind = match (scheme, auth.surface) {
                                (Scheme::Html, Surface::Html) => AssetKind::Html,
                                (Scheme::Markdown, Surface::Markdown) => AssetKind::Markdown,
                                _ => return None,
                            };
                            let owner = crate::ipc::native_owner::asset_owner_with_browser(auth.view, origin.clone(), auth.clone(), asset_renderer, browser_owner.clone())?;
                            document_assets::authorize(auth.view, kind).ok().flatten().map(|permit| (Authorization::Asset(auth, permit), owner))
                        }),
                    Scheme::Slides => views.authorize_slides_media(&request.label, &request.provenance)
                        .filter(|auth| auth.view == sender.view)
                        .and_then(|auth| crate::ipc::native_owner::owner_with_browser(auth.view, origin.clone(), renderer.clone(), auth.lease(), browser_owner.clone())
                            .map(|owner| (Authorization::Slides(auth), owner))),
                };
                if let Some((auth, owner)) = auth { execute(request, auth, Some(owner)); } else { denied(&request.reply); }
            });
            if !ran { denied(&failure); }
        }));
}

#[cfg(test)]
mod tests {
    use super::original_slide_uri;

    #[test]
    fn slide_url_restores_only_the_registered_wire_origin() {
        let original = "seuoffice-slide-media://archive/id?mime=image%2Fpng";
        assert_eq!(original_slide_uri(original).as_deref(), Some(original));
        assert_eq!(original_slide_uri("http://seuoffice-slide-media.localhost/archive/id?mime=image%2Fpng").as_deref(), Some(original));
        assert!(original_slide_uri("http://other.localhost/archive/id").is_none());
        assert!(original_slide_uri("http://seuoffice-slide-media.localhost/%61rchive/id").is_none());
    }
}
