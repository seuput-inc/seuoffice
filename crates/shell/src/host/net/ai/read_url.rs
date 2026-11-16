//! `readUrlTool(settingsPath, url, offset, { signal })` of
//! `@seuoffice/ai-search` (`read-url.ts`): fetch one http(s) page through the
//! SSRF guard and hand its readable text to the model, at most
//! [`PAGE_CHARS`] at a time. HTML is converted ([`super::html_text`]); JSON,
//! XML and plain text pass through unchanged. GitHub blob addresses are
//! rewritten to raw.githubusercontent.com, or to the contents API when the
//! settings carry a token. The token goes to https://api.github.com and to no
//! other origin: a read that carries it follows redirects by the credential
//! rule (`keyedDownload`), each hop through the SSRF guard. The tool never
//! fails: every failure is an object with an English sentence and one of the
//! error codes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
use jscompat::{
    JsonValue, encode_uri_component, js_regex, json_parse, json_stringify, slice_utf16, trim,
};
use main_core::packages::ai_provider::fetch::{AiRedirectRefusal, ai_redirect_refused_error};
use main_core::packages::ai_provider::jsval::{length16, number, object, prop2, str_trim, text};
use main_core::packages::ai_provider::settings_secrets::open_ai_secrets;
use main_core::packages::ai_search::media_tools::read_ai_settings_file;
use main_core::packages::electron_utils::remote_image::{BodyError, read_body_capped};
use main_core::packages::electron_utils::safe_remote_url::{
    AbortSignal, FetchError, FetchInit, FetchWithSsrfGuardOptions, Fetcher, GlobalDispatcher,
    Response, fetch_with_ssrf_guard,
};
use shell_contract::runtime::CancelToken;

use super::codec;
use super::fetch::{guarded_fetcher_with_cap, guarded_resolver};
use super::html_text::html_to_text;

/// The part of one page the model gets in one call; the rest is read with offset
const PAGE_CHARS: usize = 50_000;
/// Download cap, applied before and while the body is read
const MAX_BYTES: usize = 10 * 1024 * 1024;
/// One request's whole budget: connect, redirects and body read together
const TIMEOUT: Duration = Duration::from_secs(30);
/// A read without the token follows at most ten redirects; every hop is checked
const MAX_REDIRECTS: f64 = 10.0;
/// A read with the token follows at most five
const MAX_KEYED_REDIRECTS: usize = 5;
/// The one origin the GitHub token is for
const GITHUB_API_ORIGIN: &str = "https://api.github.com";
/// Pages held in process memory, per final address, so a page's continuations do not re-download
const CACHE_TTL: Duration = Duration::from_secs(3 * 60);
const CACHE_MAX_ENTRIES: usize = 8;
const USER_AGENT: &str = "Mozilla/5.0";
const GITHUB_RAW_ACCEPT: &str = "application/vnd.github.raw+json";
/// Hosts whose refusals get the GitHub-specific hints (private repo, token)
const GITHUB_HOSTS: [&str; 4] = [
    "github.com",
    "www.github.com",
    "raw.githubusercontent.com",
    "api.github.com",
];

// ── the answer ──────────────────────────────────────────────────────

/// `ReadUrlFailure`: an English sentence and the code of what went wrong
struct Failure {
    code: &'static str,
    error: String,
    /// the HTTP status behind an `http_error`
    status: Option<u16>,
}

fn failure(code: &'static str, error: impl Into<String>) -> Failure {
    Failure {
        code,
        error: error.into(),
        status: None,
    }
}

impl Failure {
    fn json(self) -> JsonValue {
        let mut fields = vec![
            ("ok", JsonValue::Bool(false)),
            ("error", text(self.error)),
            ("errorCode", text(self.code)),
        ];
        if let Some(status) = self.status {
            fields.push(("status", number(f64::from(status))));
        }
        object(fields)
    }
}

fn http_failure(status: u16, host: &str, has_token: bool) -> Failure {
    let github = GITHUB_HOSTS.contains(&host);
    if github && has_token && (status == 401 || status == 403) {
        return failure(
            "auth",
            format!(
                "GitHub rejected the GitHub token (HTTP {status}). Check the GitHub token in the AI settings."
            ),
        );
    }
    if github && !has_token && status == 404 {
        return failure(
            "auth",
            "GitHub answered with HTTP 404. If this file is in a private repository, add a GitHub token in the AI settings; otherwise check the address.",
        );
    }
    if github && has_token && status == 404 {
        return Failure {
            status: Some(status),
            ..failure(
                "http_error",
                "GitHub answered with HTTP 404 for this file; check the address and that the token can read this repository.",
            )
        };
    }
    Failure {
        status: Some(status),
        ..failure(
            "http_error",
            format!("The server answered with HTTP {status}."),
        )
    }
}

fn unsupported_type(mime: &str) -> Failure {
    let what = if mime.is_empty() {
        "a binary file".to_owned()
    } else {
        format!("a {mime} file")
    };
    failure(
        "unsupported_type",
        format!("This address is {what}, not a web page. Download it and attach the file instead."),
    )
}

fn too_large() -> Failure {
    failure(
        "too_large",
        "The page is larger than 10 MB. Download it and attach the file instead.",
    )
}

// ── the page cache ──────────────────────────────────────────────────

/// One converted page, keyed by its final address and by the address that was
/// asked for. The text is kept as UTF-16 units: offsets count them.
struct Page {
    final_url: String,
    title: Option<String>,
    units: Vec<u16>,
    expires: Instant,
    /// every map key pointing at this entry, so eviction drops them together
    keys: Vec<String>,
}

/// The cache's keys in the order of their last use, oldest first
type Cache = Vec<(String, Arc<Page>)>;

/// Process memory only: pages are re-downloaded after a restart, which is fine
static PAGES: Mutex<Cache> = Mutex::new(Vec::new());

fn pages() -> std::sync::MutexGuard<'static, Cache> {
    PAGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every map key that still points at this entry; a newer entry for the same
/// final address owns that key
fn owned_keys(cache: &Cache, entry: &Arc<Page>) -> Vec<String> {
    entry
        .keys
        .iter()
        .filter(|key| {
            cache
                .iter()
                .any(|(name, found)| name == *key && Arc::ptr_eq(found, entry))
        })
        .cloned()
        .collect()
}

fn delete_key(cache: &mut Cache, key: &str) {
    cache.retain(|(name, _)| name != key);
}

/// Drop the keys the entry still owns; a key another entry has taken over stays
fn drop_entry(cache: &mut Cache, entry: &Arc<Page>) {
    for key in owned_keys(cache, entry) {
        delete_key(cache, &key);
    }
}

/// A cached page, refreshing its recency; an expired one is dropped
fn cached_page(cache: &mut Cache, key: &str, now: Instant) -> Option<Arc<Page>> {
    let entry = cache
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, entry)| entry.clone())?;
    if entry.expires <= now {
        drop_entry(cache, &entry);
        return None;
    }
    for owned in owned_keys(cache, &entry) {
        delete_key(cache, &owned);
        cache.push((owned, entry.clone()));
    }
    Some(entry)
}

fn distinct_entries(cache: &Cache) -> usize {
    let mut seen: Vec<&Arc<Page>> = Vec::new();
    for (_, entry) in cache {
        if !seen.iter().any(|known| Arc::ptr_eq(known, entry)) {
            seen.push(entry);
        }
    }
    seen.len()
}

fn cache_page(
    cache: &mut Cache,
    page: (String, Option<String>, Vec<u16>),
    request_url: &str,
    now: Instant,
) -> Arc<Page> {
    let (final_url, title, units) = page;
    let keys = if request_url == final_url {
        vec![final_url.clone()]
    } else {
        vec![final_url.clone(), request_url.to_owned()]
    };
    let entry = Arc::new(Page {
        final_url,
        title,
        units,
        expires: now + CACHE_TTL,
        keys: keys.clone(),
    });
    // delete before set even for a key that already exists: the newest entry
    // for an address owns it, and the key moves to the newest position
    for key in &keys {
        delete_key(cache, key);
    }
    for key in keys {
        cache.push((key, entry.clone()));
    }
    while distinct_entries(cache) > CACHE_MAX_ENTRIES {
        let (_, oldest) = cache.remove(0);
        drop_entry(cache, &oldest);
    }
    entry
}

/// `slicePage(entry, offset)`
fn slice_page(page: &Page, offset: f64) -> JsonValue {
    let total = page.units.len();
    // an offset past the end (or past what a count can hold) reads nothing
    let from = if offset < total as f64 {
        offset as usize
    } else {
        total
    };
    let to = from.saturating_add(PAGE_CHARS).min(total);
    let slice = String::from_utf16_lossy(&page.units[from..to]);
    let end = offset + (to - from) as f64;
    let truncated = end < total as f64;
    let mut fields = vec![
        ("ok", JsonValue::Bool(true)),
        ("url", text(&page.final_url)),
    ];
    if let Some(title) = &page.title {
        fields.push(("title", text(title)));
    }
    fields.push(("text", text(slice)));
    fields.push(("offset", number(offset)));
    fields.push(("totalChars", number(total as f64)));
    fields.push(("truncated", JsonValue::Bool(truncated)));
    if truncated {
        fields.push(("nextOffset", number(end)));
    }
    object(fields)
}

// ── what the request is ─────────────────────────────────────────────

/// `clip(value)`
fn clip(value: &str) -> String {
    if length16(value) > 120 {
        format!("{}...", slice_utf16(value, 0.0, Some(120.0)))
    } else {
        value.to_owned()
    }
}

fn host_of(raw: &str) -> String {
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .unwrap_or_default()
}

/// A GitHub blob address: owner, repo, one ref segment and the file path.
/// A branch name with a slash cannot be told apart from the file path, so it
/// is not guessed (the failure comes back from GitHub's answer).
fn github_blob(url: &reqwest::Url) -> Option<(String, String, String, String)> {
    let host = url.host_str()?.to_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let parts: Vec<&str> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 5 {
        return None;
    }
    let (owner, repo, kind, reference) = (parts[0], parts[1], parts[2], parts[3]);
    let file = parts[4..].join("/");
    if kind != "blob" || file.is_empty() {
        return None;
    }
    Some((
        owner.to_owned(),
        repo.to_owned(),
        reference.to_owned(),
        file,
    ))
}

/// The request that actually serves the bytes of an address
struct Request {
    url: String,
    blob_api: bool,
    token_for_api: bool,
}

fn github_request(target: &reqwest::Url, token: &str) -> Request {
    let Some((owner, repo, reference, file)) = github_blob(target) else {
        // a contents address the model gave directly carries the token too, but
        // not the raw Accept: its own answer is what should be read
        return Request {
            url: target.to_string(),
            blob_api: false,
            token_for_api: !token.is_empty()
                && target.origin().ascii_serialization() == GITHUB_API_ORIGIN,
        };
    };
    if token.is_empty() {
        return Request {
            url: format!("https://raw.githubusercontent.com/{owner}/{repo}/{reference}/{file}"),
            blob_api: false,
            token_for_api: false,
        };
    }
    Request {
        url: format!(
            "https://api.github.com/repos/{owner}/{repo}/contents/{file}?ref={}",
            encode_uri_component(&reference)
        ),
        blob_api: true,
        token_for_api: true,
    }
}

/// Headers of the request as it first goes out; what a redirect may carry on
/// is decided by [`keyed_download`]
fn request_headers(request: &Request, token: &str) -> Vec<(String, String)> {
    let mut headers = vec![("User-Agent".to_owned(), USER_AGENT.to_owned())];
    if request.token_for_api {
        if request.blob_api {
            headers.push(("Accept".to_owned(), GITHUB_RAW_ACCEPT.to_owned()));
        }
        headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
    }
    headers
}

/// `readAiSettingsFile(path).readUrl?.githubToken?.trim() ?? ''`, the stored
/// token opened with the shell's codec
fn github_token(settings_path: &str) -> String {
    let file = std::fs::read(settings_path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let opened = file.map(|file| match json_parse(&file) {
        Ok(stored) => json_stringify(&open_ai_secrets(codec(), &stored)),
        Err(_) => file,
    });
    let settings = read_ai_settings_file(opened.as_deref());
    str_trim(prop2(Some(&settings), "readUrl", "githubToken"))
}

// ── the transport ───────────────────────────────────────────────────

/// The signal of one read: the caller cancelling, or the 30 second deadline
struct Stop {
    cancel: CancelToken,
    deadline: Instant,
}

impl AbortSignal for Stop {
    fn aborted(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

impl Stop {
    /// Why the chain stopped: the caller cancelling, or a fired deadline. The
    /// cancelled sentence is never shown: an aborted run discards its result
    /// before it reaches the screen.
    fn failure(&self) -> Option<Failure> {
        if self.cancel.is_cancelled() {
            return Some(failure("network", "The read was cancelled."));
        }
        if Instant::now() >= self.deadline {
            return Some(failure(
                "timeout",
                "The page did not answer within 30 seconds.",
            ));
        }
        None
    }
}

/// The guarded transport that remembers the last address it was asked for: the
/// guard follows redirects itself, so that is how the final address is known
struct Tracking {
    inner: Box<dyn Fetcher + Send>,
    last: String,
}

impl Fetcher for Tracking {
    fn global_dispatcher(&mut self) -> GlobalDispatcher {
        self.inner.global_dispatcher()
    }

    fn fetch<'signal>(
        &mut self,
        url: &str,
        init: &FetchInit<'_, 'signal>,
    ) -> Result<Box<dyn Response + 'signal>, FetchError> {
        self.last = url.to_owned();
        self.inner.fetch(url, init)
    }

    fn close_dispatcher(&mut self) {
        self.inner.close_dispatcher();
    }

    fn sleep(&mut self, ms: f64, signal: Option<&dyn AbortSignal>) {
        self.inner.sleep(ms, signal);
    }
}

/// Why a read with the token got no answer
enum Refusal {
    /// the guard refused an address of the chain (or the read was stopped)
    Blocked,
    /// the credential rule ended the chain: `AiRedirectRefusedError`'s message
    Redirect(String),
    Transport(String),
}

/// `KEY_IN_ADDRESS`: a query parameter that carries a key by its name
fn key_in_address(name: &str) -> bool {
    js_regex!(r"^(key|api[-_]?key|access[-_]?token|token)$", "i").test(name)
}

/// `UNKEYED_HEADERS`: the request headers that say nothing about who is asking
fn unkeyed_header(name: &str) -> bool {
    [
        "accept",
        "accept-encoding",
        "accept-language",
        "range",
        "user-agent",
    ]
    .contains(&name.to_lowercase().as_str())
}

/// `siteOf(url)`: scheme and host of an address, for an error text
fn site_of(url: &reqwest::Url) -> String {
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => String::new(),
    };
    format!(
        "{}://{}",
        url.scheme(),
        slice_utf16(&host, 0.0, Some(100.0))
    )
}

/// `keyedDownload(url, init, { base })`: a download asked for with a credential,
/// whose redirects are followed by hand and never carry the credential to
/// another origin: to an https address only, at most five times; to another
/// origin without any header but the few that say nothing about who is asking;
/// not at all when the credential is in the address. Each hop goes through the
/// SSRF guard on its own.
fn keyed_download<'s>(
    url: &str,
    mut headers: Vec<(String, String)>,
    stop: &'s Stop,
    transport: &mut Tracking,
) -> Result<Box<dyn Response + 's>, Refusal> {
    let home = reqwest::Url::parse(url).map_err(|e| Refusal::Transport(e.to_string()))?;
    let key_stays = !home.username().is_empty()
        || home.password().is_some_and(|password| !password.is_empty())
        || home.query_pairs().any(|(name, _)| key_in_address(&name));
    let mut resolver = guarded_resolver();
    let mut at = home.clone();
    let mut keyed = true;
    let mut followed = 0;
    loop {
        let mut options = FetchWithSsrfGuardOptions {
            manual: true,
            headers: Some(headers.clone()),
            signal: Some(stop),
            ..Default::default()
        };
        let mut response =
            match fetch_with_ssrf_guard(&mut *resolver, transport, at.as_str(), &mut options) {
                Err(error) => return Err(Refusal::Transport(error.0)),
                Ok(None) => return Err(Refusal::Blocked),
                Ok(Some(response)) => response,
            };
        let location = response.header("location");
        let redirected = matches!(response.status(), 301 | 302 | 303 | 307 | 308);
        let Some(location) = location.filter(|_| redirected) else {
            return Ok(response);
        };
        response.cancel_body();
        let refused = |target: &str, reason| {
            Refusal::Redirect(ai_redirect_refused_error(target, reason).message)
        };
        let Ok(next) = at.join(&location) else {
            return Err(refused("", AiRedirectRefusal::Unreadable));
        };
        if key_stays {
            return Err(refused(&site_of(&next), AiRedirectRefusal::NotFollowed));
        }
        if followed >= MAX_KEYED_REDIRECTS {
            return Err(refused(&site_of(&next), AiRedirectRefusal::TooMany));
        }
        if next.scheme() != "https" {
            return Err(refused(&site_of(&next), AiRedirectRefusal::Insecure));
        }
        if keyed && next.origin() != home.origin() {
            keyed = false;
            headers.retain(|(name, _)| unkeyed_header(name));
        }
        at = next;
        followed += 1;
    }
}

// ── what the bytes are ──────────────────────────────────────────────

/// Content types read as page text; anything else is a download
fn is_readable_type(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime == "application/xhtml+xml"
        || mime == "application/vnd.github.raw"
}

fn bom_encoding(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        Some("utf-8")
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        Some("utf-16le")
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some("utf-16be")
    } else {
        None
    }
}

/// `<meta charset>` / `<meta http-equiv=content-type content=...charset=…>` in the head bytes
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let (head, _) = WINDOWS_1252.decode_without_bom_handling(head);
    js_regex!(r#"<meta[^>]*charset\s*=\s*["']?\s*([a-z0-9_:.-]+)"#, "i")
        .exec(&head)
        .and_then(|found| found.get(1))
        .map(str::to_owned)
}

/// The body as text: BOM first, then the declared charset, then the meta tag,
/// then UTF-8. Broken characters become replacement characters, never a
/// failure; an unknown charset name reads as UTF-8.
fn decode_body(
    bytes: &[u8],
    declared: Option<String>,
    allow_meta: bool,
    bom: Option<&'static str>,
) -> String {
    let label = bom.map(str::to_owned).or(declared).or_else(|| {
        if allow_meta {
            meta_charset(bytes)
        } else {
            None
        }
    });
    let encoding = label
        .and_then(|label| Encoding::for_label(trim(&label).as_bytes()))
        .unwrap_or(UTF_8);
    encoding.decode_with_bom_removal(bytes).0.into_owned()
}

fn declared_charset(raw_content_type: Option<&str>) -> Option<String> {
    js_regex!(r#"charset\s*=\s*["']?([^;"'\s]+)"#, "i")
        .exec(raw_content_type.unwrap_or_default())
        .and_then(|found| found.get(1))
        .map(str::to_owned)
}

fn looks_like_html(decoded: &str) -> bool {
    js_regex!(r"^\s*(<!doctype\s+html|<html[\s>])", "i").test(decoded)
}

/// Only a real number counts: "500", true and non-numbers read from the start
fn normalize_offset(offset: f64) -> f64 {
    if offset.is_finite() {
        offset.floor().max(0.0)
    } else {
        0.0
    }
}

// ── the tool ────────────────────────────────────────────────────────

/// Read one web page as text. Never fails: the result says `ok: false` with an
/// English sentence and an error code. `offset` continues a long page from
/// where the previous call stopped (at most [`PAGE_CHARS`] per call).
pub(super) fn read_url(
    settings_path: &str,
    url: &str,
    offset: f64,
    cancel: &CancelToken,
) -> JsonValue {
    match read(settings_path, url, normalize_offset(offset), cancel) {
        Ok(answer) => answer,
        Err(failed) => failed.json(),
    }
}

fn read(
    settings_path: &str,
    url: &str,
    start: f64,
    cancel: &CancelToken,
) -> Result<JsonValue, Failure> {
    let trimmed = trim(url);
    let Ok(target) = reqwest::Url::parse(trimmed) else {
        return Err(failure(
            "invalid_url",
            format!("Not a usable web address: {}", clip(trimmed)),
        ));
    };
    if !matches!(target.scheme(), "http" | "https") {
        return Err(failure(
            "invalid_url",
            "Only http and https addresses can be read.",
        ));
    }

    // read live on every call, like the search tools: the token can change between calls
    let token = github_token(settings_path);
    let request = github_request(&target, &token);
    let has_token = !token.is_empty();

    if let Some(page) = cached_page(&mut pages(), &request.url, Instant::now()) {
        return Ok(slice_page(&page, start));
    }

    let stop = Stop {
        cancel: cancel.clone(),
        deadline: Instant::now() + TIMEOUT,
    };
    let mut transport = Tracking {
        inner: guarded_fetcher_with_cap(MAX_BYTES),
        last: request.url.clone(),
    };
    let headers = request_headers(&request, &token);
    let answer = if request.token_for_api {
        keyed_download(&request.url, headers, &stop, &mut transport)
    } else {
        let mut options = FetchWithSsrfGuardOptions {
            max_redirects: Some(MAX_REDIRECTS),
            headers: Some(headers),
            signal: Some(&stop),
            ..Default::default()
        };
        match fetch_with_ssrf_guard(
            &mut *guarded_resolver(),
            &mut transport,
            &request.url,
            &mut options,
        ) {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err(Refusal::Blocked),
            Err(error) => Err(Refusal::Transport(error.0)),
        }
    };
    let mut response = match answer {
        Ok(response) => response,
        Err(Refusal::Redirect(message)) => {
            return Err(stop.failure().unwrap_or_else(|| {
                failure(
                    "blocked",
                    format!(
                        "Stopped at a redirect while reading with the GitHub token. {}",
                        message.replace("API key", "GitHub token")
                    ),
                )
            }));
        }
        Err(Refusal::Transport(message)) => {
            return Err(stop.failure().unwrap_or_else(|| {
                failure("network", format!("Could not reach the address: {message}"))
            }));
        }
        Err(Refusal::Blocked) => {
            return Err(stop.failure().unwrap_or_else(|| {
                failure(
                    "blocked",
                    "Refused to fetch that address: it is, or redirects to, a private or loopback address.",
                )
            }));
        }
    };
    let final_url = transport.last.clone();
    if !response.ok() {
        return Err(http_failure(
            response.status(),
            &host_of(&final_url),
            has_token,
        ));
    }

    let raw_content_type = response.header("content-type");
    let mime = trim(
        raw_content_type
            .as_deref()
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default(),
    )
    .to_lowercase();
    // an absent or generic type is decided from the bytes themselves
    let sniff = mime.is_empty() || mime == "application/octet-stream";
    if !sniff && !is_readable_type(&mime) {
        return Err(unsupported_type(&mime));
    }

    let bytes = match read_body_capped(&mut *response, MAX_BYTES as f64) {
        Ok(bytes) => bytes,
        Err(error) => {
            if let Some(stopped) = stop.failure() {
                return Err(stopped);
            }
            return Err(match error {
                BodyError::TooLarge(_) => too_large(),
                BodyError::Stream(message) if message == "response body too large" => too_large(),
                BodyError::Stream(message) => {
                    failure("network", format!("Could not download the page: {message}"))
                }
            });
        }
    };
    drop(response);
    let bom = bom_encoding(&bytes);
    // a UTF-16 body carries NUL bytes; its BOM is what proves it is text
    if sniff && bom.is_none() && bytes.contains(&0) {
        return Err(unsupported_type(""));
    }

    // the meta charset is an HTML signal only: a `<meta charset=...>` string
    // inside JSON, XML or plain text must not change how those bytes are decoded
    let allow_meta = mime == "text/html" || mime == "application/xhtml+xml";
    let decoded = decode_body(
        &bytes,
        declared_charset(raw_content_type.as_deref()),
        allow_meta,
        bom,
    );
    let as_html = allow_meta || (sniff && looks_like_html(&decoded));
    let (title, converted) = if as_html {
        let base = reqwest::Url::parse(&final_url).unwrap_or(target);
        let page = html_to_text(&decoded, &base);
        (page.title, page.text)
    } else {
        (None, decoded)
    };
    if trim(&converted).is_empty() {
        return Err(failure("empty", "The page had no readable text."));
    }
    let units: Vec<u16> = converted.encode_utf16().collect();
    let page = cache_page(
        &mut pages(),
        (final_url, title, units),
        &request.url,
        Instant::now(),
    );
    Ok(slice_page(&page, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str, text: &str) -> (String, Option<String>, Vec<u16>) {
        (url.to_owned(), None, text.encode_utf16().collect())
    }

    #[test]
    fn github_blob_is_rewritten_by_token() {
        let blob = reqwest::Url::parse("https://github.com/o/r/blob/main/dir/a.md").unwrap();
        assert_eq!(
            github_request(&blob, "").url,
            "https://raw.githubusercontent.com/o/r/main/dir/a.md"
        );
        let keyed = github_request(&blob, "t");
        assert_eq!(
            keyed.url,
            "https://api.github.com/repos/o/r/contents/dir/a.md?ref=main"
        );
        assert!(keyed.blob_api && keyed.token_for_api);
        let other = reqwest::Url::parse("https://example.com/o/r/blob/main/a").unwrap();
        assert!(!github_request(&other, "t").token_for_api);
        let api = reqwest::Url::parse("https://api.github.com/repos/o/r/contents/a").unwrap();
        let direct = github_request(&api, "t");
        assert!(direct.token_for_api && !direct.blob_api);
    }

    #[test]
    fn token_headers_follow_the_request_kind() {
        let blob = Request {
            url: String::new(),
            blob_api: true,
            token_for_api: true,
        };
        let names: Vec<String> = request_headers(&blob, "t")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["User-Agent", "Accept", "Authorization"]);
        let plain = Request {
            url: String::new(),
            blob_api: false,
            token_for_api: false,
        };
        assert_eq!(request_headers(&plain, "t").len(), 1);
    }

    #[test]
    fn github_refusals_carry_their_hint() {
        assert_eq!(http_failure(404, "github.com", false).code, "auth");
        assert_eq!(http_failure(403, "api.github.com", true).code, "auth");
        let missing = http_failure(404, "github.com", true);
        assert_eq!((missing.code, missing.status), ("http_error", Some(404)));
        let other = http_failure(500, "example.com", false);
        assert_eq!(other.error, "The server answered with HTTP 500.");
    }

    #[test]
    fn credential_leaves_only_with_unkeyed_headers() {
        assert!(unkeyed_header("User-Agent"));
        assert!(!unkeyed_header("Authorization"));
        assert!(key_in_address("access_token"));
        assert!(key_in_address("Api-Key"));
        assert!(!key_in_address("ref"));
    }

    #[test]
    fn a_long_page_is_read_in_windows() {
        let long = "ab".repeat(PAGE_CHARS);
        let entry = Arc::new(Page {
            final_url: "https://e.com/".into(),
            title: Some("T".into()),
            units: long.encode_utf16().collect(),
            expires: Instant::now() + CACHE_TTL,
            keys: Vec::new(),
        });
        let first = slice_page(&entry, 0.0);
        assert!(json_stringify(&first).contains("\"truncated\":true,\"nextOffset\":50000"));
        let last = slice_page(&entry, 50000.0);
        assert!(json_stringify(&last).contains("\"truncated\":false}"));
        let past = slice_page(&entry, 1e9);
        assert!(json_stringify(&past).contains("\"text\":\"\""));
    }

    #[test]
    fn cache_evicts_the_oldest_and_keeps_newer_owners() {
        let now = Instant::now();
        let mut cache = Cache::new();
        for n in 0..9 {
            let url = format!("https://e.com/{n}");
            cache_page(&mut cache, page(&url, "x"), &url, now);
        }
        assert_eq!(distinct_entries(&cache), CACHE_MAX_ENTRIES);
        assert!(cached_page(&mut cache, "https://e.com/0", now).is_none());
        assert!(cached_page(&mut cache, "https://e.com/8", now).is_some());
        // one entry under two keys: the asked address and the final one
        cache_page(
            &mut cache,
            page("https://e.com/final", "y"),
            "https://e.com/ask",
            now,
        );
        assert!(cached_page(&mut cache, "https://e.com/ask", now).is_some());
        assert!(cached_page(&mut cache, "https://e.com/final", now).is_some());
        let expired = now + CACHE_TTL;
        assert!(cached_page(&mut cache, "https://e.com/final", expired).is_none());
        assert!(cached_page(&mut cache, "https://e.com/ask", expired).is_none());
    }

    #[test]
    fn bytes_decode_by_bom_then_declared_then_meta() {
        assert_eq!(
            decode_body(b"\xef\xbb\xbfhi", None, false, Some("utf-8")),
            "hi"
        );
        assert_eq!(
            decode_body(b"caf\xe9", Some("iso-8859-1".into()), false, None),
            "café"
        );
        let html = b"<meta charset=\"windows-1252\">caf\xe9";
        assert!(decode_body(html, None, true, None).ends_with("café"));
        assert!(decode_body(html, None, false, None).ends_with("caf\u{fffd}"));
        assert_eq!(
            decode_body(b"ok", Some("no-such".into()), false, None),
            "ok"
        );
    }

    #[test]
    fn sniffing_and_types() {
        assert!(looks_like_html("  <!DOCTYPE html><p>"));
        assert!(looks_like_html("<html lang=en>"));
        assert!(!looks_like_html("{\"a\":1}"));
        assert!(is_readable_type("application/ld+json"));
        assert!(!is_readable_type("application/pdf"));
        assert_eq!(
            declared_charset(Some("text/html; Charset=\"UTF-8\"")).as_deref(),
            Some("UTF-8")
        );
        assert_eq!(normalize_offset(f64::NAN), 0.0);
        assert_eq!(normalize_offset(-4.0), 0.0);
        assert_eq!(normalize_offset(7.9), 7.0);
    }

    #[test]
    fn invalid_addresses_fail_without_a_request() {
        let cancel = CancelToken::default();
        let answer = json_stringify(&read_url("", "example.com", 0.0, &cancel));
        assert!(answer.contains("\"errorCode\":\"invalid_url\""));
        assert!(answer.contains("Not a usable web address: example.com"));
        let answer = json_stringify(&read_url("", "ftp://example.com/a", 0.0, &cancel));
        assert!(answer.contains("Only http and https addresses can be read."));
    }
}
