use std::io::{self, BufRead, Read, Write};
use std::sync::Arc;

use main_core::shell::file_index::extract::{Extracted, FileParser, ParsedFile, ParsedFileKind};
use main_core::shell::file_index::extract_worker::{WorkerRequest, WorkerResponse, handle_message};
use main_core::shell::file_index::scan::ScannedFile;
use serde_json::{Value, json};
use shell_ipc_w::editor_files::{FileParseHost, ParsedKind};

const MAX_LINE: u64 = 64 * 1024 * 1024;

struct Parser(FileParseHost);
impl FileParser for Parser {
    fn redact_html_source(&mut self, source: &str) -> Result<String, String> {
        file_parse::html::redact_html_source(source)
    }
    fn parse_file_to_text(&mut self, path: &str) -> Result<ParsedFile, String> {
        let result = self.0.parse_file_to_text(std::path::Path::new(path))?;
        Ok(ParsedFile {
            ok: result.ok,
            text: result.text,
            error: result.error,
            kind: match result.kind {
                ParsedKind::Text => ParsedFileKind::Text,
                ParsedKind::Image => ParsedFileKind::Image,
                ParsedKind::Unsupported => ParsedFileKind::Unsupported,
            },
        })
    }
}

pub(super) fn encode_request(request: &WorkerRequest) -> Value {
    match request {
        WorkerRequest::Extract { id, path } => json!({"id":id,"type":"extract","path":path}),
        WorkerRequest::Scan { id, root } => json!({"id":id,"type":"scan","root":root}),
    }
}

fn request(value: &Value) -> Result<WorkerRequest, String> {
    let id = value
        .get("id")
        .and_then(Value::as_f64)
        .ok_or("worker request has no id")?;
    match value.get("type").and_then(Value::as_str) {
        Some("extract") => Ok(WorkerRequest::Extract {
            id,
            path: value
                .get("path")
                .and_then(Value::as_str)
                .ok_or("worker request has no path")?
                .to_owned(),
        }),
        Some("scan") => Ok(WorkerRequest::Scan {
            id,
            root: value
                .get("root")
                .and_then(Value::as_str)
                .ok_or("worker request has no root")?
                .to_owned(),
        }),
        _ => Err("invalid index worker request".into()),
    }
}

pub(super) fn response(value: &Value) -> Result<WorkerResponse, String> {
    let id = value
        .get("id")
        .and_then(Value::as_f64)
        .ok_or("worker response has no id")?;
    match value.get("type").and_then(Value::as_str) {
        Some("extract") => {
            let result = &value["result"];
            let extracted = match result.get("kind").and_then(Value::as_str) {
                Some("text") => Extracted::Text(
                    result
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("worker text missing")?
                        .to_owned(),
                ),
                Some("name-only") => Extracted::NameOnly,
                Some("error") => Extracted::Error(
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .ok_or("worker error missing")?
                        .to_owned(),
                ),
                _ => return Err("invalid worker extraction response".into()),
            };
            Ok(WorkerResponse::Extract {
                id,
                result: extracted,
            })
        }
        Some("scan") => {
            let files = value
                .get("files")
                .and_then(Value::as_array)
                .ok_or("worker file list missing")?
                .iter()
                .map(|item| {
                    Ok(ScannedFile {
                        path: item
                            .get("path")
                            .and_then(Value::as_str)
                            .ok_or("worker file path missing")?
                            .to_owned(),
                        mtime_ms: item
                            .get("mtimeMs")
                            .and_then(Value::as_f64)
                            .ok_or("worker file time missing")?,
                        size_bytes: item
                            .get("sizeBytes")
                            .and_then(Value::as_f64)
                            .ok_or("worker file size missing")?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let truncated = value
                .get("truncated")
                .and_then(Value::as_bool)
                .ok_or("worker scan flag missing")?;
            Ok(WorkerResponse::Scan {
                id,
                files,
                truncated,
            })
        }
        _ => Err("invalid index worker response".into()),
    }
}

/// Runs on a worker thread after the hidden worker CEF app readies EngineHost.
pub(crate) fn run_index_worker(mut output: std::fs::File) -> Result<i32, String> {
    let parser: FileParseHost = Arc::new(super::super::w_files::ShellFileParser);
    let mut parser = Parser(parser);
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        let mut line = Vec::new();
        let count = input
            .by_ref()
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(0);
        }
        if count as u64 > MAX_LINE || !line.ends_with(b"\n") {
            return Err("index worker request exceeds limit".into());
        }
        let value: Value = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        let response = handle_message(&request(&value)?, &mut parser).to_value();
        serde_json::to_writer(&mut output, &response).map_err(|error| error.to_string())?;
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|error| error.to_string())?;
    }
}
