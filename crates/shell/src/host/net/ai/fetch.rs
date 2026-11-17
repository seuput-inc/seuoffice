//! `fetchRemoteImage(url)` and `readBodyCapped` for the AI channels: main-core's
//! guarded download (the SSRF gate, the retries, the cap) over the operating
//! system's resolver and reqwest. Blocking: the caller is on a worker, and a
//! request is driven with `tauri::async_runtime::block_on`.
//!
//! A hop to a host name connects to the addresses the gate validated (the
//! pinned lookup of the TypeScript's per-hop `Agent`), so the system proxy is
//! not used for it; a hop to an address literal goes through the shell's
//! shared client and its proxy.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use main_core::packages::electron_utils::remote_image::{
    MAX_REMOTE_IMAGE_BYTES, MemoryResponse, fetch_remote_image, read_body_capped,
};
use main_core::packages::electron_utils::safe_remote_url::{
    AbortSignal, FetchError, FetchInit, FetchWithSsrfGuardOptions, Fetcher, GlobalDispatcher,
    LookupAddress, Resolver, Response, SafeRemoteTarget, fetch_with_ssrf_guard,
    resolve_safe_remote_target,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use shell_contract::runtime::CancelToken;
use shell_ipc_a::ai_core::RemoteImage;

use crate::host::net::http;

/// undici's connect and body timeouts
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(300);

/// `dns.lookup(host, { all: true })`
struct SystemResolver;

impl Resolver for SystemResolver {
    fn lookup_all(&mut self, host: &str) -> Result<Vec<LookupAddress>, FetchError> {
        let found = (host, 0)
            .to_socket_addrs()
            .map_err(|e| FetchError(e.to_string()))?;
        Ok(found
            .map(|addr| LookupAddress {
                address: addr.ip().to_string(),
                family: if addr.is_ipv6() { 6.0 } else { 4.0 },
            })
            .collect())
    }

    fn lookup_all_abort(
        &mut self,
        host: &str,
        signal: &dyn AbortSignal,
    ) -> Result<Vec<LookupAddress>, FetchError> {
        if signal.aborted() {
            return Err(FetchError("This operation was aborted".into()));
        }
        let host = host.to_owned();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("safe-dns".into())
            .spawn(move || {
                let _ = send.send(SystemResolver.lookup_all(&host));
            })
            .map_err(|error| FetchError(error.to_string()))?;
        loop {
            if signal.aborted() {
                return Err(FetchError("This operation was aborted".into()));
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => {
                    if signal.aborted() {
                        return Err(FetchError("This operation was aborted".into()));
                    }
                    return result;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(FetchError("DNS lookup failed".into()));
                }
            }
        }
    }
}

/// One client whose connection goes to the validated addresses of `url`'s host
fn pinned_client(url: &str, target: &SafeRemoteTarget) -> Result<reqwest::Client, FetchError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| FetchError(e.to_string()))?;
    let port = parsed.port_or_known_default().unwrap_or(0);
    let addrs: Vec<SocketAddr> = target
        .addresses
        .iter()
        .filter_map(|a| a.address.parse().ok().map(|ip| SocketAddr::new(ip, port)))
        .collect();
    let host = parsed.host_str().unwrap_or(&target.hostname);
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(Duration::from_secs(60))
        .resolve_to_addrs(host, &addrs)
        .build()
        .map_err(|e| FetchError(e.to_string()))
}

fn header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// A hop's answer, its body not yet read
struct Download<'signal> {
    status: u16,
    headers: HeaderMap,
    response: Option<reqwest::Response>,
    cancel: Option<CancelToken>,
    signal: Option<&'signal dyn AbortSignal>,
}

impl Response for Download<'_> {
    fn status(&self) -> u16 {
        self.status
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn has_body(&self) -> bool {
        true
    }

    fn cancel_body(&mut self) {
        self.response = None;
    }

    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
        let Some(response) = self.response.as_mut() else {
            return Ok(None);
        };
        let result = tauri::async_runtime::block_on(async {
            tokio::select! {
                biased;
                _ = wait_for_abort(self.signal, self.cancel.as_ref()) => Err(FetchError("This operation was aborted".into())),
                result = response.chunk() => result.map_err(|e| FetchError(e.to_string())),
            }
        });
        if result.is_err() {
            self.response = None;
        }
        result.map(|chunk| chunk.map(|bytes| bytes.to_vec()))
    }

    fn array_buffer(&mut self) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.read_chunk()? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// The transport of a hop: one `GET`, its redirect handed back
struct Hops(Option<CancelToken>);

impl Fetcher for Hops {
    /// No dispatcher of ours rides a proxy for a pinned hop
    fn global_dispatcher(&mut self) -> GlobalDispatcher {
        GlobalDispatcher::NotAnObject
    }

    fn fetch<'signal>(
        &mut self,
        url: &str,
        init: &FetchInit<'_, 'signal>,
    ) -> Result<Box<dyn Response + 'signal>, FetchError> {
        let client = match init.pinned {
            Some(target) if !target.literal => pinned_client(url, target)?,
            _ => http::client(url, false),
        };
        let mut request = client.get(url).timeout(BODY_TIMEOUT);
        if let Some(headers) = init.headers {
            request = request.headers(header_map(headers));
        }
        let response = tauri::async_runtime::block_on(async {
            tokio::select! {
                biased;
                _ = wait_for_abort(init.signal, self.0.as_ref()) => Err(FetchError("This operation was aborted".into())),
                result = request.send() => result.map_err(|e| FetchError(e.to_string())),
            }
        })?;
        Ok(Box::new(Download {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            response: Some(response),
            cancel: self.0.clone(),
            signal: init.signal,
        }))
    }

    fn close_dispatcher(&mut self) {}

    fn sleep(&mut self, ms: f64, signal: Option<&dyn AbortSignal>) {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms as u64);
        while std::time::Instant::now() < deadline
            && !self.0.as_ref().is_some_and(CancelToken::is_cancelled)
            && !signal.is_some_and(AbortSignal::aborted)
        {
            std::thread::sleep(
                Duration::from_millis(20)
                    .min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
        }
    }
}

/// The resolver for a caller that runs main-core's SSRF guard itself (the mind map's
/// link preview and picture fetch): the guard validates every hop, so this one only answers
/// `dns.lookup(host, { all: true })`.
pub(crate) fn guard_resolver() -> Box<dyn Resolver + Send> {
    Box::new(SystemResolver)
}

/// The transport for the same callers: one `GET` per hop, connected to the addresses the guard
/// validated (undici's `fetch` with the per-hop pinned `Agent`).
pub(crate) fn guard_fetcher() -> Box<dyn Fetcher + Send> {
    Box::new(Hops(None))
}

/// A resolver exposed to editor engines, without exposing the raw transport.
pub(crate) fn guarded_resolver() -> Box<dyn Resolver + Send> {
    Box::new(EditorResolver)
}

struct EditorResolver;
impl Resolver for EditorResolver {
    fn lookup_all(&mut self, host: &str) -> Result<Vec<LookupAddress>, FetchError> {
        SystemResolver.lookup_all(host)
    }
    fn lookup_all_abort(
        &mut self,
        host: &str,
        signal: &dyn AbortSignal,
    ) -> Result<Vec<LookupAddress>, FetchError> {
        SystemResolver.lookup_all_abort(host, signal)
    }
}

/// Editor callers normally run their own SSRF guard. This adapter also
/// validates and pins each hop itself, so calling its Fetcher directly cannot
/// bypass the boundary or supply a forged target.
pub(crate) fn guarded_fetcher() -> Box<dyn Fetcher + Send> {
    guarded_fetcher_with_cap(MAX_REMOTE_IMAGE_BYTES as usize)
}

/// The same guarded transport with a caller-specific response body ceiling.
pub(crate) fn guarded_fetcher_with_cap(cap: usize) -> Box<dyn Fetcher + Send> {
    Box::new(EditorFetcher { cap })
}

struct EditorFetcher {
    cap: usize,
}
impl Fetcher for EditorFetcher {
    fn global_dispatcher(&mut self) -> GlobalDispatcher {
        GlobalDispatcher::NotAnObject
    }
    fn fetch<'signal>(
        &mut self,
        url: &str,
        init: &FetchInit<'_, 'signal>,
    ) -> Result<Box<dyn Response + 'signal>, FetchError> {
        let target = resolve_safe_remote_target(&mut SystemResolver, Some(url), init.signal)
            .ok_or_else(|| FetchError("unsafe or invalid remote URL".into()))?;
        let checked = FetchInit {
            headers: init.headers,
            signal: init.signal,
            pinned: Some(&target),
        };
        let response = Hops(None).fetch(&target.url, &checked)?;
        Ok(Box::new(CappedEditorResponse {
            inner: response,
            remaining: self.cap,
        }))
    }
    fn close_dispatcher(&mut self) {}
    fn sleep(&mut self, ms: f64, signal: Option<&dyn AbortSignal>) {
        Hops(None).sleep(ms, signal);
    }
}

struct CappedEditorResponse<'signal> {
    inner: Box<dyn Response + 'signal>,
    remaining: usize,
}
impl Response for CappedEditorResponse<'_> {
    fn status(&self) -> u16 {
        self.inner.status()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.inner.header(name)
    }
    fn has_body(&self) -> bool {
        self.inner.has_body()
    }
    fn cancel_body(&mut self) {
        self.inner.cancel_body();
    }
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
        let Some(chunk) = self.inner.read_chunk()? else {
            return Ok(None);
        };
        if chunk.len() > self.remaining {
            self.inner.cancel_body();
            return Err(FetchError("response body too large".into()));
        }
        self.remaining -= chunk.len();
        Ok(Some(chunk))
    }
    fn array_buffer(&mut self) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.read_chunk()? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Markdown image-host upload: no redirect is followed with credentials, and
/// the response body is streamed under the image-host's 1 MiB ceiling.
pub(crate) fn upload_fetch(
    url: &str,
    init: &main_core::markdown::image_host::FetchInit<'_>,
) -> Result<
    main_core::markdown::image_host::FetchResponse,
    main_core::markdown::image_host::FetchFailure,
> {
    use main_core::markdown::image_host::{
        FetchBody, FetchFailure, FetchResponse, ResponseHeaders,
    };
    const LIMIT: usize = 1024 * 1024;
    let fail = |error: String| FetchFailure::Error(error);
    let target = resolve_safe_remote_target(&mut SystemResolver, Some(url), None)
        .ok_or_else(|| fail("unsafe or invalid remote URL".into()))?;
    let client = if target.literal {
        http::client(&target.url, false)
    } else {
        pinned_client(&target.url, &target).map_err(|error| fail(error.0))?
    };
    let method =
        reqwest::Method::from_bytes(init.method.as_bytes()).map_err(|e| fail(e.to_string()))?;
    let mut headers = HeaderMap::new();
    for (name, value) in &init.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|error| fail(error.to_string()))?;
        let value = HeaderValue::from_str(value).map_err(|error| fail(error.to_string()))?;
        headers.insert(name, value);
    }
    let mut request = client
        .request(method, &target.url)
        .headers(headers)
        .timeout(Duration::from_millis(u64::from(init.timeout_ms)));
    request = match &init.body {
        FetchBody::Bytes(bytes) => request.body(bytes.to_vec()),
        FetchBody::Text(text) => request.body(text.clone()),
        FetchBody::Form {
            field,
            file_name,
            mime,
            bytes,
        } => {
            let part = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(file_name.clone())
                .mime_str(mime)
                .map_err(|e| fail(e.to_string()))?;
            request.multipart(reqwest::multipart::Form::new().part((*field).to_owned(), part))
        }
    };
    tauri::async_runtime::block_on(async move {
        let mut response = request
            .send()
            .await
            .map_err(|error| fail(error.to_string()))?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_length = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let redirected = matches!(status, 301 | 302 | 303 | 307 | 308) && location.is_some();
        let text = if redirected {
            None
        } else if content_length
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > LIMIT as u64)
        {
            Some(Err("response body too large".into()))
        } else {
            let mut bytes = Vec::new();
            let result = loop {
                match response.chunk().await {
                    Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= LIMIT => {
                        bytes.extend_from_slice(&chunk)
                    }
                    Ok(Some(_)) => break Err("response body too large".into()),
                    Ok(None) => break Ok(String::from_utf8_lossy(&bytes).into_owned()),
                    Err(error) => break Err(error.to_string()),
                }
            };
            Some(result)
        };
        Ok(FetchResponse {
            ok: (200..=299).contains(&status),
            status: f64::from(status),
            opaque_redirect: false,
            headers: ResponseHeaders::Get {
                location,
                content_length,
            },
            text,
        })
    })
}

/// `fetchRemoteImage(url)` then `readBodyCapped(resp, MAX_REMOTE_IMAGE_BYTES)`:
/// `None` for a refused address, a response that is not ok, a body over the
/// cap or a failure of any kind. `tmpdir` is `os.tmpdir()`.
pub fn remote_image(url: &str, tmpdir: &str) -> Option<RemoteImage> {
    let mut response = fetch_remote_image(
        &mut SystemResolver,
        &mut Hops(None),
        tmpdir,
        url,
        None,
        None,
    )?;
    if !response.ok() {
        return None;
    }
    let content_type = response.header("content-type").unwrap_or_default();
    let bytes = read_body_capped(&mut *response, MAX_REMOTE_IMAGE_BYTES as f64).ok()?;
    Some(RemoteImage {
        bytes,
        content_type,
    })
}

async fn wait_for_abort(signal: Option<&dyn AbortSignal>, cancel: Option<&CancelToken>) {
    if signal.is_none() && cancel.is_none() {
        std::future::pending::<()>().await;
        return;
    }
    loop {
        if signal.is_some_and(AbortSignal::aborted) || cancel.is_some_and(CancelToken::is_cancelled)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct GuardCancel<'a>(&'a CancelToken);
impl AbortSignal for GuardCancel<'_> {
    fn aborted(&self) -> bool {
        self.0.is_cancelled()
    }
}

type MediaDownload = Result<(Vec<u8>, Option<String>, Option<String>), String>;

/// A bounded download, with SSRF validation and address pinning at every hop.
pub(super) fn media_download(url: &str, cancel: &CancelToken) -> MediaDownload {
    const LIMIT: usize = 200 * 1024 * 1024;
    let image = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back())
                .map(str::to_owned)
        })
        .is_some_and(|name| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|ext| name.to_ascii_lowercase().ends_with(&format!(".{ext}")))
        });
    let mut hops = Hops(Some(cancel.clone()));
    let signal = GuardCancel(cancel);
    let mut response = if image {
        fetch_remote_image(
            &mut SystemResolver,
            &mut hops,
            &std::env::temp_dir().to_string_lossy(),
            url,
            None,
            Some(&signal),
        )
    } else {
        let mut options = FetchWithSsrfGuardOptions {
            headers: Some(vec![("User-Agent".into(), "Mozilla/5.0".into())]),
            signal: Some(&signal),
            ..Default::default()
        };
        fetch_with_ssrf_guard(&mut SystemResolver, &mut hops, url, &mut options).map_err(|e| e.0)?
    }
    .ok_or_else(|| format!("Could not download {url}"))?;
    if !response.ok() {
        return Err(format!("Could not download {url}"));
    }
    if response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > LIMIT as u64)
    {
        response.cancel_body();
        return Err(format!("{url} is too large to analyze"));
    }
    let content_type = response.header("content-type");
    let name = reqwest::Url::parse(url).ok().and_then(|u| {
        u.path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    });
    let mut bytes = Vec::new();
    loop {
        if cancel.is_cancelled() {
            response.cancel_body();
            return Err("This operation was aborted".into());
        }
        let Some(chunk) = response.read_chunk().map_err(|e| e.0)? else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > LIMIT {
            response.cancel_body();
            return Err(format!("{url} is too large to analyze"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, content_type, name))
}

pub(crate) fn net_fetch_cancellable(
    url: &str,
    resources: Option<&std::path::PathBuf>,
    cancel: &CancelToken,
) -> Result<Box<dyn Response + Send>, String> {
    net_fetch_inner(url, resources, Some(cancel), None)
}

/// Docs media requests carry the document lease captured by the core handler.
pub(crate) fn net_fetch_for_docs(
    url: &str,
    resources: Option<&std::path::PathBuf>,
    permit: &main_core::docs::byte_handoff::MediaPermit,
    cancel: &CancelToken,
) -> Result<Box<dyn Response + Send>, String> {
    if !permit.is_active() {
        return Err("The document media session is closed.".into());
    }
    let result = net_fetch_inner(url, resources, Some(cancel), Some(permit))?;
    if !permit.is_active() {
        return Err("The document media session is closed.".into());
    }
    Ok(result)
}

fn net_fetch_inner(
    url: &str,
    resources: Option<&std::path::PathBuf>,
    cancel: Option<&CancelToken>,
    permit: Option<&main_core::docs::byte_handoff::MediaPermit>,
) -> Result<Box<dyn Response + Send>, String> {
    let check = || {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            Err("This operation was aborted".to_owned())
        } else {
            Ok(())
        }
    };
    check()?;
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    if let Some(original) = crate::host::document_assets::docx_original_uri(url) {
        let permit = permit.ok_or("Document media requires an authenticated document session.")?;
        if !permit.is_active() {
            return Err("The document media session is closed.".into());
        }
        let cancel = cancel.ok_or("Document media requires request cancellation authority.")?;
        let answer = crate::host::d2::docx_media_response_for_permit(
            permit.clone(),
            &original,
            cancel.clone(),
        )?;
        check()?;
        if !permit.is_active() {
            return Err("The document media session is closed.".into());
        }
        if answer.body.len() > MAX_REMOTE_IMAGE_BYTES as usize {
            return Err("response body too large".into());
        }
        return Ok(Box::new(MemoryResponse::new(
            answer.status,
            answer.headers,
            answer.body,
        )));
    }
    let renderer_url = if parsed.scheme() == "genoffice-app" {
        Some(parsed.clone())
    } else {
        crate::app_scheme::electron_url(&parsed).and_then(|url| reqwest::Url::parse(&url).ok())
    };
    if let Some(renderer_url) = renderer_url {
        let host = renderer_url
            .host_str()
            .ok_or_else(|| "invalid asset host".to_string())?;
        let request = tauri::http::Request::builder()
            .uri(renderer_url.path())
            .body(Vec::new())
            .map_err(|error| error.to_string())?;
        let response = crate::app_scheme::serve(
            &crate::app_scheme::roots(resources.cloned(), false),
            host,
            &request,
        );
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_owned()))
            })
            .collect();
        let bytes = response.into_body();
        check()?;
        if bytes.len() > MAX_REMOTE_IMAGE_BYTES as usize {
            return Err("response body too large".into());
        }
        return Ok(Box::new(MemoryResponse::new(status, headers, bytes)));
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("unsupported URL scheme".into());
    }
    let signal = cancel.map(GuardCancel);
    let mut options = FetchWithSsrfGuardOptions {
        signal: signal.as_ref().map(|signal| signal as &dyn AbortSignal),
        ..Default::default()
    };
    let mut response = fetch_with_ssrf_guard(
        &mut SystemResolver,
        &mut Hops(cancel.cloned()),
        url,
        &mut options,
    )
    .map_err(|error| error.0)?
    .ok_or_else(|| "unsafe or invalid remote URL".to_string())?;
    let status = response.status();
    let headers = ["content-type", "content-length"]
        .into_iter()
        .filter_map(|name| response.header(name).map(|value| (name.to_owned(), value)))
        .collect();
    let bytes = read_body_capped(&mut *response, MAX_REMOTE_IMAGE_BYTES as f64)
        .map_err(|error| error.message().to_owned())?;
    check()?;
    Ok(Box::new(MemoryResponse::new(status, headers, bytes)))
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    use main_core::markdown::image_host::{FetchBody, FetchInit as UploadInit};

    #[test]
    fn cancelled_net_fetch_does_not_start_url_or_network_work() {
        let cancel = CancelToken::default();
        cancel.cancel();
        let error = match net_fetch_cancellable("not a url", None, &cancel) {
            Ok(_) => panic!("cancelled fetch unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error, "This operation was aborted");
        assert!(
            net_fetch_cancellable("http://127.0.0.1/private", None, &CancelToken::default())
                .is_err()
        );
    }

    #[test]
    fn document_media_fetch_requires_a_live_document_permit() {
        let uri = "seuoffice-docx-media://handoff/forged";
        let cancel = CancelToken::default();
        let error = match net_fetch_cancellable(uri, None, &cancel) {
            Err(error) => error,
            Ok(_) => panic!("unscoped document media was accepted"),
        };
        assert_eq!(
            error,
            "Document media requires an authenticated document session."
        );
        let wire = "http://seuoffice-docx-media.localhost/handoff/forged";
        let error = match net_fetch_cancellable(wire, None, &cancel) {
            Err(error) => error,
            Ok(_) => panic!("unscoped wire media was accepted"),
        };
        assert_eq!(
            error,
            "Document media requires an authenticated document session."
        );
        let permit = main_core::docs::byte_handoff::MediaPermit::mint(9).unwrap();
        permit.revoke();
        let error = match net_fetch_for_docs(uri, None, &permit, &cancel) {
            Err(error) => error,
            Ok(_) => panic!("revoked document media was accepted"),
        };
        assert_eq!(error, "The document media session is closed.");
    }

    #[test]
    fn editor_fetcher_rejects_private_ip_without_trusting_the_caller() {
        let mut fetcher = guarded_fetcher();
        let init = FetchInit {
            headers: None,
            signal: None,
            pinned: None,
        };
        assert!(fetcher.fetch("http://127.0.0.1/private", &init).is_err());
    }

    #[test]
    fn upload_refuses_loopback_before_sending_credentials() {
        let init = UploadInit {
            method: "PUT",
            headers: vec![("Authorization".into(), "synthetic".into())],
            body: FetchBody::Bytes(b"fixture"),
            timeout_ms: 20_000,
            redirect: "manual",
        };
        assert!(upload_fetch("https://127.0.0.1/upload", &init).is_err());
    }

    #[test]
    fn capped_editor_response_fails_instead_of_truncating() {
        let inner = MemoryResponse::new(200, Vec::new(), b"abc".to_vec());
        let mut response = CappedEditorResponse {
            inner: Box::new(inner),
            remaining: 2,
        };
        assert!(response.array_buffer().is_err());
    }

    #[test]
    fn editor_response_obeys_the_selected_ceiling() {
        let image_limit = MAX_REMOTE_IMAGE_BYTES as usize;
        let table_limit = 16 * 1024 * 1024;
        let mut image = CappedEditorResponse {
            inner: Box::new(MemoryResponse::new(
                200,
                Vec::new(),
                vec![0; image_limit + 1],
            )),
            remaining: image_limit,
        };
        assert!(image.array_buffer().is_err());
        let mut image_above_table = CappedEditorResponse {
            inner: Box::new(MemoryResponse::new(
                200,
                Vec::new(),
                vec![0; table_limit + 1],
            )),
            remaining: image_limit,
        };
        assert_eq!(
            image_above_table.array_buffer().unwrap().len(),
            table_limit + 1
        );
        let mut table = CappedEditorResponse {
            inner: Box::new(MemoryResponse::new(200, Vec::new(), vec![0; table_limit])),
            remaining: table_limit,
        };
        assert_eq!(table.array_buffer().unwrap().len(), table_limit);
        let mut oversized = CappedEditorResponse {
            inner: Box::new(MemoryResponse::new(
                200,
                Vec::new(),
                vec![0; table_limit + 1],
            )),
            remaining: table_limit,
        };
        assert!(oversized.array_buffer().is_err());
    }

    struct TestSignal(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl AbortSignal for TestSignal {
        fn aborted(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn abort_interrupts_a_request_waiting_for_headers() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            ready.send(()).unwrap();
            assert_eq!(socket.read(&mut request).unwrap(), 0);
        });
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let signal = TestSignal(flag.clone());
        let abort = std::thread::spawn(move || {
            started.recv().unwrap();
            flag.store(true, Ordering::SeqCst);
        });
        let init = FetchInit {
            headers: None,
            signal: Some(&signal),
            pinned: None,
        };
        let result = Hops(None).fetch(&format!("http://{addr}/"), &init);
        assert!(result.is_err());
        abort.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn abort_interrupts_body_read_and_closes_connection() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .unwrap();
            assert_eq!(socket.read(&mut request).unwrap(), 0);
        });
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let signal = TestSignal(flag.clone());
        let init = FetchInit {
            headers: None,
            signal: Some(&signal),
            pinned: None,
        };
        let mut response = Hops(None).fetch(&format!("http://{addr}/"), &init).unwrap();
        let abort = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::SeqCst);
        });
        assert!(response.read_chunk().is_err());
        assert!(response.read_chunk().unwrap().is_none());
        abort.join().unwrap();
        server.join().unwrap();
    }
}
