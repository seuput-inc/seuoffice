//! `parallelMcpSearch(query)` of `@seuoffice/ai-search`: the anonymous Parallel
//! Search MCP, the keyless route before the DuckDuckGo scrape. One Streamable
//! HTTP session: `initialize`, the `initialized` notification, one
//! `web_search` tool call, then the session is ended. The whole exchange,
//! streamed bodies included, has 15 seconds.

use std::time::Duration;

use jscompat::{JsonValue, json_parse, json_stringify};
use main_core::packages::ai_provider::jsval::{as_str, number, object, prop, text};

use crate::host::net::http;

const ENDPOINT: &str = "https://search.parallel.ai/mcp";
/// `LATEST_PROTOCOL_VERSION` of the MCP SDK the TypeScript runs on
const PROTOCOL_VERSION: &str = "2025-11-25";
const TIMEOUT: Duration = Duration::from_secs(15);

struct Session {
    client: reqwest::Client,
    id: Option<String>,
    version: Option<String>,
}

fn request(id: u32, method: &str, params: JsonValue) -> JsonValue {
    object(vec![
        ("jsonrpc", text("2.0")),
        ("id", number(f64::from(id))),
        ("method", text(method)),
        ("params", params),
    ])
}

/// The JSON-RPC message of an event stream's `data:` lines, when it answers `id`
fn answer_in(event: &str, id: u32) -> Option<JsonValue> {
    let mut kind = "message";
    let mut data = Vec::new();
    for line in event.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            kind = value.trim();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if kind != "message" || data.is_empty() {
        return None;
    }
    let message = json_parse(&data.join("\n")).ok()?;
    match prop(&message, "id") {
        Some(JsonValue::Number(n)) if *n == f64::from(id) => Some(message),
        _ => None,
    }
}

impl Session {
    /// One POST; for a request (`expect` is its id) the JSON-RPC result, read
    /// from a JSON body or from the event stream as soon as its message arrives
    async fn post(
        &mut self,
        message: &JsonValue,
        expect: Option<u32>,
    ) -> Result<Option<JsonValue>, String> {
        let mut builder = self
            .client
            .post(ENDPOINT)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(json_stringify(message));
        if let Some(id) = &self.id {
            builder = builder.header("mcp-session-id", id);
        }
        if let Some(version) = &self.version {
            builder = builder.header("mcp-protocol-version", version);
        }
        let mut response = builder.send().await.map_err(|e| e.to_string())?;
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.id = Some(id.to_owned());
        }
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let Some(id) = expect else {
            return Ok(None);
        };
        let stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        let message = if stream {
            let mut buffer = Vec::<u8>::new();
            let mut answer = None;
            while answer.is_none() {
                let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? else {
                    break;
                };
                buffer.extend_from_slice(&chunk);
                let pending = String::from_utf8_lossy(&buffer).replace("\r\n", "\n");
                let mut events: Vec<&str> = pending.split("\n\n").collect();
                // the last piece is an event that is not complete yet
                events.pop();
                answer = events.into_iter().find_map(|event| answer_in(event, id));
            }
            if answer.is_none() {
                // the stream ended: a last event may lack its blank line
                let pending = String::from_utf8_lossy(&buffer).replace("\r\n", "\n");
                answer = answer_in(&pending, id);
            }
            answer
        } else {
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            json_parse(&String::from_utf8_lossy(&body))
                .ok()
                .filter(|message| matches!(prop(message, "id"), Some(JsonValue::Number(n)) if *n == f64::from(id)))
        };
        let Some(message) = message else {
            return Err("no answer".to_owned());
        };
        if let Some(error) = prop(&message, "error") {
            return Err(json_stringify(error));
        }
        Ok(prop(&message, "result").cloned())
    }
}

/// The text blocks of a tool result may carry the structured result as JSON
fn tool_answer(result: &JsonValue) -> Option<JsonValue> {
    if prop(result, "isError").is_some_and(JsonValue::truthy) {
        return None;
    }
    if let Some(structured) = prop(result, "structuredContent").filter(|v| v.truthy()) {
        return Some(structured.clone());
    }
    let Some(JsonValue::Array(blocks)) = prop(result, "content") else {
        return None;
    };
    for block in blocks {
        if as_str(prop(block, "type")) != Some("text") {
            continue;
        }
        if let Some(body) = as_str(prop(block, "text"))
            && let Ok(parsed) = json_parse(body)
        {
            return Some(parsed);
        }
    }
    None
}

async fn exchange(query: &str) -> Result<Option<JsonValue>, String> {
    let mut session = Session {
        client: http::client(ENDPOINT, true),
        id: None,
        version: None,
    };
    let result = run(&mut session, query).await;
    if let Some(id) = &session.id {
        // cleanup failure must not discard usable search results
        let _ = session
            .client
            .delete(ENDPOINT)
            .header("mcp-session-id", id)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    }
    result
}

async fn run(session: &mut Session, query: &str) -> Result<Option<JsonValue>, String> {
    let init = request(
        0,
        "initialize",
        object(vec![
            ("protocolVersion", text(PROTOCOL_VERSION)),
            ("capabilities", object(Vec::new())),
            (
                "clientInfo",
                object(vec![
                    ("name", text("seuoffice")),
                    ("version", text("0.1.0")),
                ]),
            ),
        ]),
    );
    let started = session.post(&init, Some(0)).await?;
    session.version = started
        .as_ref()
        .and_then(|result| as_str(prop(result, "protocolVersion")))
        .map(str::to_owned);
    let initialized = object(vec![
        ("jsonrpc", text("2.0")),
        ("method", text("notifications/initialized")),
    ]);
    session.post(&initialized, None).await?;
    let call = request(
        1,
        "tools/call",
        object(vec![
            ("name", text("web_search")),
            (
                "arguments",
                object(vec![
                    ("objective", text(query)),
                    ("search_queries", JsonValue::Array(vec![text(query)])),
                ]),
            ),
        ]),
    );
    Ok(session
        .post(&call, Some(1))
        .await?
        .and_then(|result| tool_answer(&result)))
}

/// The structured search result, or `None` for a tool error, an answer that
/// holds none, a failure of any kind and the deadline
pub(super) fn search(query: &str) -> Option<JsonValue> {
    tauri::async_runtime::block_on(async {
        tokio::time::timeout(TIMEOUT, exchange(query))
            .await
            .ok()?
            .ok()?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_stream_answer_is_found_by_its_id() {
        let event = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":2}}";
        assert!(answer_in(event, 1).is_some());
        assert!(answer_in(event, 0).is_none());
        assert!(answer_in("data: {\"id\":1}\nevent: ping", 1).is_none());
        assert!(answer_in(": comment", 1).is_none());
    }

    #[test]
    fn tool_result_falls_back_to_json_text_blocks() {
        let result = json_parse(
            r#"{"content":[{"type":"text","text":"note"},{"type":"text","text":"{\"results\":[]}"}]}"#,
        )
        .unwrap();
        let answer = tool_answer(&result).unwrap();
        assert!(prop(&answer, "results").is_some());
        let failed = json_parse(r#"{"isError":true,"structuredContent":{"a":1}}"#).unwrap();
        assert!(tool_answer(&failed).is_none());
    }
}
