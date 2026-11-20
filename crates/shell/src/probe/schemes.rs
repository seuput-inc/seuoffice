//! The schemes probe mode registers on the real builder, after the shell's own:
//!
//! - `probe-sink`: where fixture pages report. A GET with the record in the query.
//! - `seuoffice-ipc`: the shell's own handler (`ipc::sync::handle`) behind a wrapper that
//!   records each request (method, path, whether the nonce header and how much body arrived)
//!   before delegating. The response is the real one.
//! - the five schemes CEF registered natively (`seuoffice-docx-media`, `md-asset`,
//!   `html-asset`, `seuoffice-slide-media`, `drawio-app`): stand-ins that serve fixture
//!   documents instead of the shell's handlers, which only answer documents they authorise.
//!   Registering the same name again replaces the handler (Tauri keeps a map), so a probe run
//!   does not use the real asset handlers. Which scheme a document loads from is what P3
//!   asks, not what the handler does.

use std::borrow::Cow;
use std::time::Duration;

use serde_json::{Value, json};
use tauri::http::{self, StatusCode};
use tauri::{UriSchemeContext, UriSchemeResponder, Url, Wry};

use super::log::{self, record};

pub const SINK: &str = "probe-sink";

pub const STANDINS: [&str; 5] = [
    "seuoffice-docx-media",
    "md-asset",
    "html-asset",
    "seuoffice-slide-media",
    "drawio-app",
];

const P3_PAGE: &str = include_str!("../../probe-fixtures/pages/p3-context.html");
const PROBE_JS: &str = include_str!("../../probe-fixtures/pages/probe.js");
const PIXEL_PNG: &[u8] = include_bytes!("../../probe-fixtures/pages/pixel.png");
const TINY_WASM: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

pub fn install(builder: tauri::Builder<Wry>) -> tauri::Builder<Wry> {
    let mut builder = builder
        .register_asynchronous_uri_scheme_protocol(SINK, sink)
        .register_asynchronous_uri_scheme_protocol(crate::ipc::sync::SCHEME, ipc_recorder);
    for name in STANDINS {
        builder = builder.register_asynchronous_uri_scheme_protocol(
            name,
            move |ctx, request, responder| {
                standin(name, ctx, request, responder);
            },
        );
    }
    builder
}

type Answer = http::Response<Cow<'static, [u8]>>;

/// Every probe answer is readable from any page, including a cross-origin-isolated one.
fn reply(status: StatusCode, content_type: &str, body: impl Into<Cow<'static, [u8]>>) -> Answer {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header("cross-origin-resource-policy", "cross-origin")
        .header(http::header::CACHE_CONTROL, "no-store")
        .body(body.into())
        .expect("a response of fixed parts")
}

fn query_of(request: &http::Request<Vec<u8>>) -> Vec<(String, String)> {
    let query = request.uri().query().unwrap_or("");
    Url::parse(&format!("http://probe.invalid/?{query}")).map_or_else(
        |_| Vec::new(),
        |url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        },
    )
}

fn param(query: &[(String, String)], name: &str) -> Option<String> {
    query
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

fn sink(
    ctx: UriSchemeContext<'_, Wry>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let label = ctx.webview_label().to_owned();
    let query = query_of(&request);
    if let Some(text) = param(&query, "d") {
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut map)) => {
                let ev = map
                    .remove("ev")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                map.insert("view".into(), json!(label));
                let value = Value::Object(map);
                log::pages().push(&ev, value.clone());
                record("page", &ev, value);
            }
            _ => record(
                "rust",
                "sink.unreadable",
                json!({ "view": label, "text": text }),
            ),
        }
    }
    responder.respond(reply(
        StatusCode::NO_CONTENT,
        "text/plain",
        Vec::<u8>::new(),
    ));
}

fn ipc_recorder(
    ctx: UriSchemeContext<'_, Wry>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let header = |name: &str| request.headers().get(name).map(|value| value.len());
    let value = json!({
        "scheme": crate::ipc::sync::SCHEME,
        "view": ctx.webview_label(),
        "method": request.method().as_str(),
        "uri": request.uri().to_string(),
        "path": request.uri().path(),
        "body_len": request.body().len(),
        "doc_header_len": header("x-seuoffice-doc"),
        "origin": request.headers().get("origin").and_then(|value| value.to_str().ok()),
        "asked_headers": request
            .headers()
            .get("access-control-request-headers")
            .and_then(|value| value.to_str().ok()),
    });
    record("scheme", "scheme.request", value);
    crate::ipc::sync::handle(ctx, request, responder);
}

fn standin(
    scheme: &'static str,
    ctx: UriSchemeContext<'_, Wry>,
    request: http::Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let view = ctx.webview_label().to_owned();
    let path = request.uri().path().trim_start_matches('/').to_owned();
    let query = query_of(&request);
    let milliseconds = param(&query, "ms")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0)
        .min(20_000);
    let id = param(&query, "id").unwrap_or_default();
    record(
        "scheme",
        "scheme.request",
        json!({ "scheme": scheme, "view": view, "method": request.method().as_str(), "path": path, "query": request.uri().query() }),
    );
    let answer = match path.as_str() {
        "p3/page.html" => reply(StatusCode::OK, "text/html; charset=utf-8", P3_PAGE.as_bytes()),
        "p3/probe.js" => reply(StatusCode::OK, "text/javascript; charset=utf-8", PROBE_JS.as_bytes()),
        "p3/pixel.png" => reply(StatusCode::OK, "image/png", PIXEL_PNG),
        "p3/tiny.wasm" => reply(StatusCode::OK, "application/wasm", TINY_WASM),
        "p3/data.json" => reply(StatusCode::OK, "application/json", &b"{\"ok\":true}"[..]),
        "slow" => reply(StatusCode::OK, "text/plain", format!("slow-{id}").into_bytes()),
        "doc-slow" => reply(
            StatusCode::OK,
            "text/html; charset=utf-8",
            &b"<!doctype html><meta charset=utf-8><title>slow document</title><p>slow document</p>"[..],
        ),
        "redirect" => http::Response::builder()
            .status(StatusCode::FOUND)
            .header(http::header::LOCATION, param(&query, "to").unwrap_or_default())
            .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Cow::Borrowed(&b""[..]))
            .expect("a response of fixed parts"),
        other => match other.strip_prefix("status/").and_then(|code| code.parse::<u16>().ok()) {
            Some(code) => reply(
                StatusCode::from_u16(code).unwrap_or(StatusCode::IM_A_TEAPOT),
                "text/html; charset=utf-8",
                format!("<!doctype html><meta charset=utf-8><title>status {code}</title><p>status {code}</p>").into_bytes(),
            ),
            None => reply(StatusCode::NOT_FOUND, "text/plain", &b"not found"[..]),
        },
    };
    let delayed = matches!(path.as_str(), "slow" | "doc-slow") && milliseconds > 0;
    if !delayed {
        record(
            "scheme",
            "scheme.response",
            json!({ "scheme": scheme, "view": view, "path": path, "id": id, "status": answer.status().as_u16() }),
        );
        responder.respond(answer);
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(milliseconds));
        record(
            "scheme",
            "scheme.response",
            json!({ "scheme": scheme, "view": view, "path": path, "id": id, "status": answer.status().as_u16(), "after_ms": milliseconds }),
        );
        responder.respond(answer);
    });
}
