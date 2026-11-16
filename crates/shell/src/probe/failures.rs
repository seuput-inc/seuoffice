//! P7: the signals a navigation gives when it does not succeed, and what the request policy
//! does (design P7; S1m, S1w, S4, S8 inferences).
//!
//! One browser is walked through navigations that fail in different ways while its native
//! event stream is recorded; `p7.step` lines in the record mark where each begins and ends.
//! Further windows test the policy: a refused main-frame navigation, a sub-resource over http
//! to a loopback server the probe runs, and a request to a shell scheme the policy refuses.
//! The policy closure records every request it is asked about, with the main-frame and
//! top-level flags, so the iframe case (S8: a one-shot rule must not take a frame's document
//! for the page) is in the record too.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use serde_json::json;

use super::harness::{
    Ctx, Direct, Opened, app_url, destroy, open_direct, record_op, scheme_url, wait_done, wait_load,
};
use super::log::{self, record};
use crate::native::{Blocks, RequestPolicy};

/// A loopback HTTP server: answers every request `ok` with CORS open, and records it.
fn serve() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    Ok(port)
}

fn answer(mut stream: TcpStream) {
    let mut buffer = [0u8; 2048];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
    record(
        "rust",
        "server.request",
        json!({ "line": head.lines().next().unwrap_or("") }),
    );
    let body = "ok";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Allows everything and says what it was asked.
fn logging_policy(view: &'static str) -> RequestPolicy {
    RequestPolicy::new(move |request| {
        record(
            "rust",
            "policy.request",
            json!({ "view": view, "url": request.url, "bound_scheme": request.bound_scheme, "top_level": request.top_level, "main_frame": request.main_frame }),
        );
        true
    })
}

fn navigate(ctx: &Ctx, label: &str, url: &str) -> Result<(), String> {
    let url = tauri::Url::parse(url).map_err(|error| error.to_string())?;
    ctx.app
        .get_webview(label)
        .ok_or_else(|| format!("no webview {label}"))?
        .navigate(url)
        .map_err(|error| error.to_string())
}

/// One navigation of an open browser, with its events marked.
fn step(ctx: &Ctx, opened: &Opened, name: &str, url: &str, settle_ms: u64) {
    let base = opened.timeline.len();
    record(
        "rust",
        "p7.step",
        json!({ "name": name, "url": url, "view": opened.label, "base": base }),
    );
    let started = log::now_ms();
    record_op(
        &format!("navigate.{name}"),
        &opened.label,
        started,
        navigate(ctx, &opened.label, url).map(|()| json!(null)),
    );
    opened.timeline.wait(Duration::from_secs(25), |recs| {
        recs.iter()
            .skip(base)
            .any(|rec| rec.kind == "LoadFinished" || rec.kind == "NavigationFailed")
    });
    std::thread::sleep(Duration::from_millis(settle_ms));
    record(
        "rust",
        "p7.step.end",
        json!({ "name": name, "view": opened.label, "events": opened.timeline.len() - base, "url_now": opened.browser.url().map(|url| url.to_string()) }),
    );
}

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let port = serve()?;
    record("rust", "p7.server", json!({ "port": port }));

    let label = "probe-p7-main";
    let mut spec = Direct::new(label, &app_url("shell", "p7-ok.html"));
    spec.policy = Some(logging_policy("probe-p7-main"));
    let opened = open_direct(ctx, spec)?;
    if !wait_load(&opened, 0, 40) {
        return Err("the first page did not load".into());
    }
    std::thread::sleep(Duration::from_millis(600));
    step(
        ctx,
        &opened,
        "unregistered-scheme",
        "x-seuoffice-probe-unregistered://host/path",
        1200,
    );
    step(
        ctx,
        &opened,
        "app-404",
        &app_url("shell", "missing-404.html"),
        1200,
    );
    step(
        ctx,
        &opened,
        "standin-500",
        &scheme_url("html-asset", "status/500"),
        1200,
    );
    step(
        ctx,
        &opened,
        "standin-200",
        &scheme_url("html-asset", "status/200"),
        1200,
    );
    step(
        ctx,
        &opened,
        "connection-refused",
        "http://127.0.0.1:9/",
        1500,
    );
    step(
        ctx,
        &opened,
        "redirect",
        &scheme_url("html-asset", "redirect?to=%2Fstatus%2F200"),
        1500,
    );

    // a navigation overtaken by the next one before it answers
    let base = opened.timeline.len();
    record(
        "rust",
        "p7.step",
        json!({ "name": "supersede", "view": label, "base": base }),
    );
    let slow = scheme_url("html-asset", "doc-slow?ms=3000");
    record_op(
        "navigate.supersede.first",
        label,
        log::now_ms(),
        navigate(ctx, label, &slow).map(|()| json!(null)),
    );
    std::thread::sleep(Duration::from_millis(400));
    record_op(
        "navigate.supersede.second",
        label,
        log::now_ms(),
        navigate(ctx, label, &app_url("shell", "p7-ok.html")).map(|()| json!(null)),
    );
    opened.timeline.wait(Duration::from_secs(20), |recs| {
        recs.iter().skip(base).any(|rec| rec.kind == "LoadFinished")
    });
    std::thread::sleep(Duration::from_millis(4000));
    record(
        "rust",
        "p7.step.end",
        json!({ "name": "supersede", "view": label, "events": opened.timeline.len() - base }),
    );

    step(
        ctx,
        &opened,
        "frame",
        &app_url("shell", "p7-frame.html"),
        2000,
    );
    destroy(ctx, label);

    // a main-frame navigation the policy refuses
    let refuse = "probe-p7-refuse";
    let mut spec = Direct::new(refuse, &app_url("shell", "p7-ok.html"));
    spec.policy = Some(RequestPolicy::new(|request| {
        let refused = request.main_frame && request.url.contains("refused-target");
        record(
            "rust",
            "policy.request",
            json!({ "view": "probe-p7-refuse", "url": request.url, "top_level": request.top_level, "main_frame": request.main_frame, "allowed": !refused }),
        );
        !refused
    }));
    let opened = open_direct(ctx, spec)?;
    wait_load(&opened, 0, 40);
    std::thread::sleep(Duration::from_millis(600));
    step(
        ctx,
        &opened,
        "policy-refused",
        &app_url("shell", "refused-target.html"),
        1500,
    );
    destroy(ctx, refuse);

    // sub-resources over http: allowed by one policy, refused by the other
    for (tag, policy) in [
        ("allow", RequestPolicy::new(|_| true)),
        (
            "block",
            RequestPolicy::new(|request| !request.url.starts_with("http://127.0.0.1")).with_blocks(
                Blocks {
                    web: true,
                    loopback_open: false,
                    files: false,
                },
            ),
        ),
    ] {
        let net = format!("probe-p7-net-{tag}");
        let from = log::pages().len();
        let mut spec = Direct::new(
            &net,
            &format!("{}?port={port}&tag={tag}", app_url("shell", "p7-net.html")),
        );
        spec.policy = Some(policy);
        open_direct(ctx, spec)?;
        record(
            "rust",
            "p7.net",
            json!({ "tag": tag, "page_done": wait_done(&net, from, 25) }),
        );
        destroy(ctx, &net);
    }

    // a request to a shell scheme the policy refuses
    let scheme = "probe-p7-scheme";
    let from = log::pages().len();
    let mut spec = Direct::new(scheme, &app_url("shell", "p7-refuse.html"));
    spec.policy = Some(RequestPolicy::new(|request| {
        !request.url.contains("html-asset")
    }));
    open_direct(ctx, spec)?;
    record(
        "rust",
        "p7.scheme",
        json!({ "page_done": wait_done(scheme, from, 25) }),
    );
    std::thread::sleep(Duration::from_millis(1000));
    destroy(ctx, scheme);
    Ok(())
}
