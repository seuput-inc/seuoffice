//! `webSearch` and `imageSearch` of `@seuoffice/ai-search` (`index.ts`): the
//! keyed backends (Serper, Serply, SearchApi, Tavily, Parallel, Exa,
//! Firecrawl, Keenable), then the keyless DuckDuckGo last resort. Which one
//! runs first is the caller's [`SearchOptions::prefer`]; main-core's
//! `search_tools` turns the settings into it.
//!
//! The network is the shell's reqwest clients, driven with
//! `tauri::async_runtime::block_on` on the worker the port runs the call on.
//! A request that carries a key never follows a redirect (`keyedFetch`: the
//! redirect ends the call, which falls to the next backend); only the keyless
//! DuckDuckGo calls do. Every call has a time limit that covers the body too:
//! 15 seconds, 5 for DuckDuckGo.

use std::time::Duration;

use jscompat::{
    JsonValue, decode_uri_component, encode_uri_component, js_regex, json_parse, json_stringify,
    math_max, math_min, slice_utf16, to_number, trim,
};
use main_core::packages::ai_provider::jsval::{
    as_str, js_string, js_string_or_empty, number, object, prop, text,
};
use main_core::packages::ai_search::index::{SEARCH_BACKENDS, filter_usable_images};
use main_core::packages::ai_search::search_tools::{SearchAnswer, SearchOptions};

use super::parallel_mcp;
use crate::host::net::http;

/// How this app names itself in a User-Agent header; search services log it
const USER_AGENT: &str = "seuoffice";
/// `PRODUCT_NAME`, which the keyless Keenable endpoint identifies the app by
const PRODUCT_NAME: &str = "SeuOffice";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// short timeout: an unreachable backend should fail fast so the next one gets its turn
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
const KEENABLE_SNIPPET_CHARS: f64 = 500.0;
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const COPYRIGHT_HOSTS: [&str; 4] = ["gettyimages", "istockphoto", "shutterstock", "corbis"];

// ── the request ─────────────────────────────────────────────────────

/// One request: a GET, or a POST when `body` is set
struct Call {
    url: String,
    body: Option<String>,
    headers: Vec<(&'static str, String)>,
    timeout: Duration,
    /// follows redirects: only a call that carries no key does
    keyless: bool,
}

impl Call {
    fn get(url: String, headers: Vec<(&'static str, String)>) -> Call {
        Call {
            url,
            body: None,
            headers,
            timeout: REQUEST_TIMEOUT,
            keyless: false,
        }
    }

    fn post(url: &str, body: &JsonValue, mut headers: Vec<(&'static str, String)>) -> Call {
        headers.push(("Content-Type", "application/json".to_owned()));
        Call {
            url: url.to_owned(),
            body: Some(json_stringify(body)),
            headers,
            timeout: REQUEST_TIMEOUT,
            keyless: false,
        }
    }

    fn keyless(url: String, headers: Vec<(&'static str, String)>) -> Call {
        Call {
            url,
            body: None,
            headers,
            timeout: FALLBACK_TIMEOUT,
            keyless: true,
        }
    }
}

struct Reply {
    status: u16,
    /// read only for an answer that is ok
    body: Vec<u8>,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `await resp.json()`
    fn json(&self) -> Option<JsonValue> {
        json_parse(&String::from_utf8_lossy(&self.body)).ok()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `String(err)` of what `fetch` throws
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "AbortError: This operation was aborted".to_owned()
    } else {
        "TypeError: fetch failed".to_owned()
    }
}

/// `fetchWithTimeout`: `Err` is the thrown error as `String(err)` reads it
fn send(call: Call) -> Result<Reply, String> {
    let client = http::client(&call.url, call.keyless);
    let mut request = match call.body {
        Some(body) => client.post(&call.url).body(body),
        None => client.get(&call.url),
    };
    for (name, value) in call.headers {
        request = request.header(name, value);
    }
    let request = request.timeout(call.timeout);
    tauri::async_runtime::block_on(async move {
        let response = request.send().await.map_err(|e| describe(&e))?;
        let status = response.status().as_u16();
        let body = if response.status().is_success() {
            response.bytes().await.map_err(|e| describe(&e))?.to_vec()
        } else {
            Vec::new()
        };
        Ok(Reply { status, body })
    })
}

/// A keyed backend's JSON answer; `None` for a failed call, an answer that is
/// not ok (a redirect included) and a body that is not JSON: each makes the
/// backend answer `null`
fn keyed_json(call: Call) -> Option<JsonValue> {
    let reply = send(call).ok()?;
    if !reply.ok() {
        return None;
    }
    reply.json()
}

// ── what the backends read ──────────────────────────────────────────

fn list(value: Option<&JsonValue>) -> &[JsonValue] {
    match value {
        Some(JsonValue::Array(items)) => items,
        _ => &[],
    }
}

/// `a ?? b`
fn nullish<'a>(a: Option<&'a JsonValue>, b: Option<&'a JsonValue>) -> Option<&'a JsonValue> {
    match a {
        None | Some(JsonValue::Null) => b,
        other => other,
    }
}

/// `a || b || c`: the first truthy value
fn first_truthy<'a>(values: &[Option<&'a JsonValue>]) -> Option<&'a JsonValue> {
    values
        .iter()
        .copied()
        .flatten()
        .find(|value| value.truthy())
}

/// `value.key` through a value that may be none
fn field<'a>(value: Option<&'a JsonValue>, key: &str) -> Option<&'a JsonValue> {
    value.and_then(|value| prop(value, key))
}

/// `/^https?:\/\//i`
fn is_http(url: &str) -> bool {
    let bytes = url.as_bytes();
    bytes
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case(b"http://"))
        || bytes
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case(b"https://"))
}

/// `typeof value === 'string'` and an http(s) address
fn http_url(value: Option<&JsonValue>) -> Option<&str> {
    as_str(value).filter(|url| is_http(url))
}

fn positive_number(value: Option<&JsonValue>) -> Option<f64> {
    match value {
        Some(JsonValue::Number(x)) if *x > 0.0 => Some(*x),
        _ => None,
    }
}

/// `Number(value)`
fn js_number(value: Option<&JsonValue>) -> f64 {
    match value {
        None => f64::NAN,
        Some(JsonValue::Null) => 0.0,
        Some(JsonValue::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(JsonValue::Number(x)) => *x,
        Some(JsonValue::String(s)) => to_number(s),
        Some(_) => f64::NAN,
    }
}

/// `safeHost(url)`: the host name of an address, or `''`
fn safe_host(value: Option<&JsonValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    reqwest::Url::parse(&js_string(value))
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `registrableLabel(labels)`: 'shutterstock' for sub.shutterstock.co.uk
fn registrable_label<'a>(labels: &[&'a str]) -> Option<&'a str> {
    if labels.len() < 2 {
        return None;
    }
    let tld = labels[labels.len() - 1];
    let second = labels[labels.len() - 2];
    let second_level = ["co", "com", "org", "net", "ac", "gov", "edu"].contains(&second);
    if labels.len() >= 3 && tld.len() == 2 && second_level {
        return Some(labels[labels.len() - 3]);
    }
    Some(second)
}

/// `isCopyrightHost(imageUrl)`: an address on a stock-photo host. Scoped to the
/// host name, so a path that merely mentions a stock site is kept.
fn is_copyright_host(image_url: &str) -> bool {
    let host = safe_host(Some(&text(image_url))).to_lowercase();
    if host.is_empty() {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    COPYRIGHT_HOSTS.iter().any(|entry| {
        host == *entry
            || host.ends_with(&format!(".{entry}"))
            || registrable_label(&labels) == Some(*entry)
    })
}

fn hit(title: String, url: &str, snippet: String) -> JsonValue {
    object(vec![
        ("title", text(title)),
        ("url", text(url)),
        ("snippet", text(snippet)),
    ])
}

fn image_entry(
    title: String,
    image_url: String,
    source_url: String,
    source: String,
    size: (Option<f64>, Option<f64>),
) -> JsonValue {
    let mut fields = vec![
        ("title", text(title)),
        ("imageUrl", text(image_url)),
        ("sourceUrl", text(source_url)),
        ("source", text(source)),
    ];
    if let Some(width) = size.0 {
        fields.push(("width", number(width)));
    }
    if let Some(height) = size.1 {
        fields.push(("height", number(height)));
    }
    object(fields)
}

/// `typeof value === 'number'`
fn number_of(value: Option<&JsonValue>) -> Option<f64> {
    match value {
        Some(JsonValue::Number(x)) => Some(*x),
        _ => None,
    }
}

// ── web search ──────────────────────────────────────────────────────

struct Web {
    results: Vec<JsonValue>,
    answer: Option<String>,
    method: &'static str,
}

impl Web {
    fn found(results: Vec<JsonValue>, method: &'static str) -> Option<Web> {
        (!results.is_empty()).then_some(Web {
            results,
            answer: None,
            method,
        })
    }

    fn json(self) -> JsonValue {
        let mut fields = vec![("results", JsonValue::Array(self.results))];
        if let Some(answer) = self.answer {
            fields.push(("answer", text(answer)));
        }
        fields.push(("method", text(self.method)));
        object(fields)
    }
}

/// `answerRaw` when it is a non-empty string
fn answer_of(raw: Option<&JsonValue>) -> Option<String> {
    as_str(raw).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn serper_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let body = object(vec![
        ("q", text(query)),
        ("num", number(max as f64)),
        ("gl", text("us")),
        ("hl", text("en")),
    ]);
    let data = keyed_json(Call::post(
        "https://google.serper.dev/search",
        &body,
        vec![("X-API-KEY", key.to_owned())],
    ))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "organic")) {
        let Some(link) = http_url(prop(item, "link")) else {
            continue;
        };
        results.push(hit(
            js_string_or_empty(prop(item, "title")),
            link,
            js_string_or_empty(prop(item, "snippet")),
        ));
        if results.len() >= max {
            break;
        }
    }
    let answer_box = prop(&data, "answerBox");
    let raw = first_truthy(&[
        field(answer_box, "answer"),
        field(answer_box, "snippet"),
        field(prop(&data, "knowledgeGraph"), "description"),
    ]);
    let answer = answer_of(raw);
    let mut web = Web::found(results, "serper")?;
    web.answer = answer;
    Some(web)
}

fn serply_headers(key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Api-Key", key.to_owned()),
        ("User-Agent", USER_AGENT.to_owned()),
    ]
}

fn searchapi_headers(key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {key}")),
        ("User-Agent", USER_AGENT.to_owned()),
    ]
}

/// `${base}?${new URLSearchParams(pairs)}`
fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse(base).expect("a literal address");
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(name, value);
        }
    }
    url.into()
}

fn serply_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let url = with_query(
        "https://api.serply.io/v1/search",
        &[("q", query), ("num", &max.to_string())],
    );
    let data = keyed_json(Call::get(url, serply_headers(key)))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "results")) {
        let Some(link) = http_url(prop(item, "link")) else {
            continue;
        };
        results.push(hit(
            js_string_or_empty(prop(item, "title")),
            link,
            js_string_or_empty(prop(item, "description")),
        ));
        if results.len() >= max {
            break;
        }
    }
    Web::found(results, "serply")
}

/// Google no longer honors a result count, so one call returns one page (up to
/// 10 organic rows) and the limit is applied locally
fn searchapi_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let url = with_query(
        "https://www.searchapi.io/api/v1/search",
        &[("engine", "google"), ("q", query)],
    );
    let data = keyed_json(Call::get(url, searchapi_headers(key)))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "organic_results")) {
        let Some(link) = http_url(prop(item, "link")) else {
            continue;
        };
        results.push(hit(
            js_string_or_empty(prop(item, "title")),
            link,
            js_string_or_empty(prop(item, "snippet")),
        ));
        if results.len() >= max {
            break;
        }
    }
    Web::found(results, "searchapi")
}

fn tavily_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let body = object(vec![
        ("api_key", text(key)),
        ("query", text(query)),
        ("max_results", number(max as f64)),
        ("include_answer", JsonValue::Bool(true)),
    ]);
    let data = keyed_json(Call::post("https://api.tavily.com/search", &body, Vec::new()))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "results")) {
        let Some(url) = http_url(prop(item, "url")) else {
            continue;
        };
        results.push(hit(
            js_string_or_empty(prop(item, "title")),
            url,
            js_string_or_empty(prop(item, "content")),
        ));
        if results.len() >= max {
            break;
        }
    }
    let answer = answer_of(prop(&data, "answer"));
    let mut web = Web::found(results, "tavily")?;
    web.answer = answer;
    Some(web)
}

/// Parallel's API and free Search MCP return source excerpts, not a
/// synthesized answer. Without a key the anonymous MCP runs, so an
/// unconfigured install still gets real results before the DuckDuckGo scrape.
fn parallel_web(key: &str, query: &str, max: usize) -> Option<Web> {
    let key = trim(key);
    let data = if key.is_empty() {
        parallel_mcp::search(query)?
    } else {
        let body = object(vec![
            ("search_queries", JsonValue::Array(vec![text(query)])),
            ("mode", text("fast")),
        ]);
        keyed_json(Call::post(
            "https://api.parallel.ai/v1/search",
            &body,
            vec![("x-api-key", key.to_owned())],
        ))?
    };
    let mut results = Vec::new();
    for item in list(prop(&data, "results")) {
        // v1 search has no result-count parameter, so the limit is applied here
        if results.len() >= max {
            break;
        }
        let Some(url) = http_url(prop(item, "url")) else {
            continue;
        };
        let title = as_str(prop(item, "title"))
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        let snippet = list(prop(item, "excerpts"))
            .iter()
            .filter_map(|excerpt| as_str(Some(excerpt)))
            .collect::<Vec<_>>()
            .join("\n");
        results.push(hit(title.to_owned(), url, snippet));
    }
    Web::found(results, "parallel")
}

/// Exa's AI search: results carry the page contents asked for, so a capped
/// text pass serves as the snippet
fn exa_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let body = object(vec![
        ("query", text(query)),
        ("numResults", number(max as f64)),
        (
            "contents",
            object(vec![("text", object(vec![("maxCharacters", number(500.0))]))]),
        ),
    ]);
    let data = keyed_json(Call::post(
        "https://api.exa.ai/search",
        &body,
        vec![("x-api-key", key.to_owned())],
    ))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "results")) {
        if results.len() >= max {
            break;
        }
        let url = as_str(prop(item, "url")).unwrap_or_default();
        if !is_http(url) {
            continue;
        }
        let snippet = match (as_str(prop(item, "text")), as_str(prop(item, "summary"))) {
            (Some(text), _) if !text.is_empty() => text,
            (_, Some(summary)) => summary,
            _ => "",
        };
        let title = match prop(item, "title") {
            None | Some(JsonValue::Null) => url.to_owned(),
            Some(title) => js_string(title),
        };
        results.push(hit(title, url, snippet.to_owned()));
    }
    Web::found(results, "exa")
}

/// Firecrawl's /v2/search groups results by source; the web list is the
/// snippet-bearing one (news serves as the fallback)
fn firecrawl_web(key: &str, query: &str, max: usize) -> Option<Web> {
    if key.is_empty() {
        return None;
    }
    let body = object(vec![("query", text(query)), ("limit", number(max as f64))]);
    let data = keyed_json(Call::post(
        "https://api.firecrawl.dev/v2/search",
        &body,
        vec![("Authorization", format!("Bearer {key}"))],
    ))?;
    let data = prop(&data, "data");
    // an empty web list must fall through to news, not read as "no results"
    let web = list(field(data, "web"));
    let raw = if web.is_empty() {
        list(field(data, "news"))
    } else {
        web
    };
    let mut results = Vec::new();
    for item in raw {
        if results.len() >= max {
            break;
        }
        let url = as_str(prop(item, "url")).unwrap_or_default();
        if !is_http(url) {
            continue;
        }
        let title = match prop(item, "title") {
            None | Some(JsonValue::Null) => url.to_owned(),
            Some(title) => js_string(title),
        };
        let snippet = js_string_or_empty(nullish(prop(item, "description"), prop(item, "snippet")));
        results.push(hit(title, url, snippet));
    }
    Web::found(results, "firecrawl")
}

/// Keenable's search API: with a key /v1/search, without one the free
/// /v1/search/public (rate-limited per IP). The keyless call runs only when
/// Keenable is the selected provider.
fn keenable_web(key: &str, query: &str, max: usize, keyless: bool) -> Option<Web> {
    let key = trim(key);
    if key.is_empty() && !keyless {
        return None;
    }
    let mut headers = vec![("X-Keenable-Title", PRODUCT_NAME.to_owned())];
    if !key.is_empty() {
        headers.push(("X-API-Key", key.to_owned()));
    }
    let url = if key.is_empty() {
        "https://api.keenable.ai/v1/search/public"
    } else {
        "https://api.keenable.ai/v1/search"
    };
    let body = object(vec![
        ("query", text(query)),
        ("max_results", number(max as f64)),
    ]);
    let data = keyed_json(Call::post(url, &body, headers))?;
    let mut results = Vec::new();
    for item in list(prop(&data, "results")) {
        if results.len() >= max {
            break;
        }
        let url = as_str(prop(item, "url")).unwrap_or_default();
        if !is_http(url) {
            continue;
        }
        let raw = first_truthy(&[prop(item, "snippet"), prop(item, "description")]);
        let snippet = trim(&js_string_or_empty(raw)).to_owned();
        let title = as_str(prop(item, "title"))
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        results.push(hit(
            title.to_owned(),
            url,
            slice_utf16(&snippet, 0.0, Some(KEENABLE_SNIPPET_CHARS)),
        ));
    }
    Web::found(results, "keenable")
}

/// `normalizeSearchArgs`: a result count clamped to a finite 1..20 and a query
/// cut to 500 characters, as the model sent them unvalidated
fn normalize_args(query: &str, max_results: f64, fallback: f64) -> (String, usize) {
    let max = if max_results.is_finite() {
        math_min(math_max(1.0, max_results.floor()), 20.0)
    } else {
        fallback
    };
    (slice_utf16(query, 0.0, Some(500.0)), max as usize)
}

/// `webSearch(query, maxResults, options)`
pub(super) fn web_search(query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer {
    let (query, max) = normalize_args(query, max_results, 6.0);
    let order = std::iter::once(options.prefer)
        .chain(SEARCH_BACKENDS.into_iter().filter(|id| *id != options.prefer));
    for id in order {
        let found = match id {
            "serper" => serper_web(&options.serper_key, &query, max),
            "serply" => serply_web(&options.serply_key, &query, max),
            "searchapi" => searchapi_web(&options.searchapi_key, &query, max),
            "tavily" => tavily_web(&options.tavily_key, &query, max),
            "parallel" => parallel_web(&options.parallel_key, &query, max),
            "exa" => exa_web(&options.exa_key, &query, max),
            "firecrawl" => firecrawl_web(&options.firecrawl_key, &query, max),
            _ => keenable_web(
                &options.keenable_key,
                &query,
                max,
                options.prefer == "keenable",
            ),
        };
        if let Some(found) = found {
            return Ok(found.json());
        }
    }
    Ok(match duck_web(&query, max) {
        Ok(results) => object(vec![
            ("results", JsonValue::Array(results)),
            ("method", text("duckduckgo")),
        ]),
        // an unreachable backend must not read as an empty result set
        Err(error) => object(vec![
            ("results", JsonValue::Array(Vec::new())),
            ("method", text("error")),
            ("error", text(format!("duckduckgo: {error}"))),
        ]),
    })
}

// ── image search ────────────────────────────────────────────────────

struct Images {
    images: Vec<JsonValue>,
    method: &'static str,
}

fn found_images(images: Vec<JsonValue>, method: &'static str) -> Option<Images> {
    (!images.is_empty()).then_some(Images { images, method })
}

fn serper_images(key: &str, query: &str, max: usize) -> Option<Images> {
    if key.is_empty() {
        return None;
    }
    let body = object(vec![
        ("q", text(query)),
        ("num", number(max.min(10) as f64)),
        ("gl", text("us")),
        ("hl", text("en")),
    ]);
    let data = keyed_json(Call::post(
        "https://google.serper.dev/images",
        &body,
        vec![("X-API-KEY", key.to_owned())],
    ))?;
    let mut images = Vec::new();
    for img in list(prop(&data, "images")) {
        let image_url = js_string_or_empty(nullish(prop(img, "imageUrl"), prop(img, "original")));
        if image_url.is_empty() || is_copyright_host(&image_url) {
            continue;
        }
        let source = match prop(img, "source") {
            None | Some(JsonValue::Null) => safe_host(prop(img, "link")),
            Some(source) => js_string(source),
        };
        images.push(image_entry(
            js_string_or_empty(prop(img, "title")),
            image_url,
            js_string_or_empty(prop(img, "link")),
            source,
            (
                number_of(prop(img, "imageWidth")),
                number_of(prop(img, "imageHeight")),
            ),
        ));
        if images.len() >= max {
            break;
        }
    }
    found_images(images, "serper")
}

/// Serply returns a fixed page of 20 images, so the limit is applied locally
fn serply_images(key: &str, query: &str, max: usize) -> Option<Images> {
    if key.is_empty() {
        return None;
    }
    let url = with_query("https://api.serply.io/v1/image", &[("q", query)]);
    let data = keyed_json(Call::get(url, serply_headers(key)))?;
    let mut images = Vec::new();
    for result in list(prop(&data, "image_results")) {
        let original = prop(result, "original_image");
        let thumb = prop(result, "image");
        let link = prop(result, "link");
        let image_url = js_string_or_empty(nullish(field(original, "src"), field(thumb, "src")));
        if !is_http(&image_url) || is_copyright_host(&image_url) {
            continue;
        }
        let source = match field(link, "domain") {
            None | Some(JsonValue::Null) => safe_host(field(link, "href")),
            Some(domain) => js_string(domain),
        };
        // dimensions arrive as strings ("330")
        let width = js_number(field(original, "width"));
        let height = js_number(field(original, "height"));
        images.push(image_entry(
            js_string_or_empty(nullish(field(link, "title"), field(thumb, "alt"))),
            image_url,
            js_string_or_empty(field(link, "href")),
            source,
            (
                (width.is_finite() && width > 0.0).then_some(width),
                (height.is_finite() && height > 0.0).then_some(height),
            ),
        ));
        if images.len() >= max {
            break;
        }
    }
    found_images(images, "serply")
}

/// SearchApi returns a fixed page of Google Images results, so the limit is
/// applied locally
fn searchapi_images(key: &str, query: &str, max: usize) -> Option<Images> {
    if key.is_empty() {
        return None;
    }
    let url = with_query(
        "https://www.searchapi.io/api/v1/search",
        &[("engine", "google_images"), ("q", query)],
    );
    let data = keyed_json(Call::get(url, searchapi_headers(key)))?;
    let mut images = Vec::new();
    for result in list(prop(&data, "images")) {
        let original = prop(result, "original");
        let source = prop(result, "source");
        let image_url =
            js_string_or_empty(nullish(field(original, "link"), prop(result, "thumbnail")));
        if !is_http(&image_url) || is_copyright_host(&image_url) {
            continue;
        }
        let name = match field(source, "name") {
            None | Some(JsonValue::Null) => safe_host(field(source, "link")),
            Some(name) => js_string(name),
        };
        images.push(image_entry(
            js_string_or_empty(prop(result, "title")),
            image_url,
            js_string_or_empty(field(source, "link")),
            name,
            (
                positive_number(field(original, "width")),
                positive_number(field(original, "height")),
            ),
        ));
        if images.len() >= max {
            break;
        }
    }
    found_images(images, "searchapi")
}

/// `imageSearch(query, maxResults, options)`
pub(super) fn image_search(query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer {
    let (query, max) = normalize_args(query, max_results, 8.0);
    // Tavily and Parallel have no image endpoint; Serper, Serply and SearchApi
    // are the keyed image backends. The preferred one goes first.
    let mut order: Vec<&str> = ["serper", "serply", "searchapi"]
        .into_iter()
        .filter(|id| *id != options.prefer)
        .collect();
    if ["serper", "serply", "searchapi"].contains(&options.prefer) {
        order.insert(0, options.prefer);
    }
    for id in order {
        let found = match id {
            "serper" => serper_images(&options.serper_key, &query, max),
            "serply" => serply_images(&options.serply_key, &query, max),
            _ => searchapi_images(&options.searchapi_key, &query, max),
        };
        // a backend that only offered unusable (tiny) images falls through to the next one
        if let Some(found) = found {
            let usable = filter_usable_images(&found.images);
            if !usable.is_empty() {
                return Ok(images_answer(usable, found.method));
            }
        }
    }
    Ok(match duck_images(&query, max) {
        Ok(images) => images_answer(filter_usable_images(&images), "duckduckgo"),
        // an unreachable backend must not read as an empty gallery
        Err(error) => object(vec![
            ("images", JsonValue::Array(Vec::new())),
            ("method", text("error")),
            ("error", text(format!("duckduckgo: {error}"))),
        ]),
    })
}

fn images_answer(images: Vec<JsonValue>, method: &str) -> JsonValue {
    object(vec![
        ("images", JsonValue::Array(images)),
        ("method", text(method)),
    ])
}

// ── DuckDuckGo fallback (no key / quota exhausted) ──────────────────
// These fail on a network or HTTP failure so the caller can tell "backend
// unreachable" from a genuinely empty result set.

fn browser_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", BROWSER_USER_AGENT.to_owned()),
        ("Accept-Language", "en-US,en;q=0.9".to_owned()),
    ]
}

fn duck_reply(call: Call) -> Result<Reply, String> {
    let reply = send(call)?;
    if reply.ok() {
        Ok(reply)
    } else {
        Err(format!("Error: http {}", reply.status))
    }
}

/// `stripTags(s)`
fn strip_tags(s: &str) -> String {
    let bare = js_regex!(r"<[^>]+>", "g").replace_all(s, |_| String::new());
    trim(
        &bare
            .replace("&quot;", "\"")
            .replace("&#x27;", "'")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
    .to_owned()
}

/// `decodeDuckUrl(href)`: result links are often /l/?uddg=<encoded>
fn decode_duck_url(href: &str) -> String {
    if let Some(encoded) = js_regex!(r"[?&]uddg=([^&]+)")
        .exec(href)
        .and_then(|m| m.get(1))
    {
        // a malformed escape is one dead link, not a failed search
        return decode_uri_component(encoded).unwrap_or_default();
    }
    if href.starts_with("http") {
        href.to_owned()
    } else {
        String::new()
    }
}

fn duck_web(query: &str, max: usize) -> Result<Vec<JsonValue>, String> {
    // DuckDuckGo HTML endpoint (lightweight, no key needed)
    let html = duck_reply(Call::keyless(
        format!(
            "https://html.duckduckgo.com/html/?q={}",
            encode_uri_component(query)
        ),
        browser_headers(),
    ))?
    .text();
    let mut results = Vec::new();
    let anchor = js_regex!(
        r#"<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)<\/a>"#,
        "g"
    );
    for found in anchor.find_iter(&html) {
        if results.len() >= max {
            break;
        }
        let url = decode_duck_url(found.get(1).unwrap_or_default());
        let title = strip_tags(found.get(2).unwrap_or_default());
        if !url.is_empty() && !title.is_empty() {
            results.push(hit(title, &url, String::new()));
        }
    }
    Ok(results)
}

fn duck_images(query: &str, max: usize) -> Result<Vec<JsonValue>, String> {
    // DuckDuckGo i.js needs a vqd token, so it takes two steps
    let token_html = duck_reply(Call::keyless(
        format!("https://duckduckgo.com/?q={}", encode_uri_component(query)),
        browser_headers(),
    ))?
    .text();
    let vqd = js_regex!(r#"vqd=["']?([\d-]+)["']?"#)
        .exec(&token_html)
        .and_then(|m| m.get(1))
        .map(str::to_owned)
        .ok_or_else(|| "Error: no vqd token".to_owned())?;
    let mut headers = browser_headers();
    headers.push(("Referer", "https://duckduckgo.com/".to_owned()));
    let data = duck_reply(Call::keyless(
        format!(
            "https://duckduckgo.com/i.js?l=us-en&o=json&q={}&vqd={vqd}",
            encode_uri_component(query)
        ),
        headers,
    ))?
    .json()
    .ok_or_else(|| "SyntaxError: Unexpected end of JSON input".to_owned())?;
    let mut out = Vec::new();
    for img in list(prop(&data, "results")) {
        let image_url = js_string_or_empty(prop(img, "image"));
        if image_url.is_empty() || is_copyright_host(&image_url) {
            continue;
        }
        out.push(image_entry(
            js_string_or_empty(prop(img, "title")),
            image_url,
            js_string_or_empty(prop(img, "url")),
            safe_host(prop(img, "url")),
            (number_of(prop(img, "width")), number_of(prop(img, "height"))),
        ));
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_hosts_are_blocked_by_host_not_by_path() {
        assert!(is_copyright_host("https://media.gettyimages.com/a.jpg"));
        assert!(is_copyright_host("https://sub.shutterstock.co.uk/a.jpg"));
        assert!(is_copyright_host("https://shutterstock.com/a.jpg"));
        assert!(!is_copyright_host("https://myshutterstock.com/a.jpg"));
        assert!(!is_copyright_host("https://blog.example.com/shutterstock-review.png"));
        assert!(!is_copyright_host("not a url"));
    }

    #[test]
    fn duck_links_are_decoded() {
        assert_eq!(
            decode_duck_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x"),
            "https://example.com/a?b=1"
        );
        assert_eq!(decode_duck_url("https://example.com/"), "https://example.com/");
        assert_eq!(decode_duck_url("/relative"), "");
        assert_eq!(decode_duck_url("/l/?uddg=%E0%A4%A"), "");
    }

    #[test]
    fn duck_titles_lose_tags_and_entities() {
        assert_eq!(
            strip_tags(" <b>Rust</b> &amp; &quot;C&quot; &#x27;x&#39; &lt;y&gt; "),
            "Rust & \"C\" 'x' <y>"
        );
    }

    #[test]
    fn result_count_and_query_are_clamped() {
        assert_eq!(normalize_args("q", 99.0, 6.0).1, 20);
        assert_eq!(normalize_args("q", 0.2, 6.0).1, 1);
        assert_eq!(normalize_args("q", f64::NAN, 6.0).1, 6);
        assert_eq!(normalize_args(&"a".repeat(900), 3.0, 6.0).0.len(), 500);
    }

    #[test]
    fn scheme_check_is_case_insensitive() {
        assert!(is_http("HTTPS://a.b"));
        assert!(is_http("http://a"));
        assert!(!is_http("ftp://a"));
        assert!(!is_http("é"));
    }
}
