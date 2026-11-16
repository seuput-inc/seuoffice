//! Diagram model and attachment parsing through the production dependency host.

mod native;

use std::path::Path;

use shell_ipc_d1::diagram::{DiagramEngine, DiagramFile, DiagramFormat, OpenFailure, ParsedText};

pub(super) struct Engine;

fn field<'a>(value: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("diagram engine returned invalid {name}"))
}

fn decode_open(value: &serde_json::Value) -> Result<Result<DiagramFile, OpenFailure>, String> {
    match value.get("ok").and_then(|value| value.as_bool()) {
        Some(false) => {
            let reason = field(&value, "reason")?;
            if !matches!(
                reason,
                "unsupported"
                    | "no_original"
                    | "corrupt_image"
                    | "not_xml"
                    | "decompress_failed"
                    | "no_diagram"
            ) {
                return Err("diagram engine returned invalid failure reason".into());
            }
            Ok(Err(OpenFailure {
                reason: reason.to_owned(),
                message: field(&value, "message")?.to_owned(),
            }))
        }
        Some(true) => {
            let format = match field(&value, "format")? {
                "drawio" => DiagramFormat::Drawio,
                "dio" => DiagramFormat::Dio,
                "drawio.svg" => DiagramFormat::DrawioSvg,
                "drawio.png" => DiagramFormat::DrawioPng,
                _ => return Err("diagram engine returned invalid format".into()),
            };
            let compressed = value
                .get("anyPageCompressed")
                .and_then(|v| v.as_bool())
                .ok_or("diagram engine returned invalid compression flag")?;
            let root = field(&value, "root")?;
            if !matches!(root, "mxfile" | "mxGraphModel") {
                return Err("diagram engine returned invalid root".into());
            }
            Ok(Ok(DiagramFile {
                xml: field(&value, "xml")?.to_owned(),
                format,
                root: root.to_owned(),
                any_page_compressed: compressed,
            }))
        }
        None => Err("diagram engine returned no ok flag".into()),
    }
}

pub(super) fn open_native_invocation(
    name: &str,
    bytes: &[u8],
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<Result<DiagramFile, OpenFailure>, String> {
    let input = serde_json::json!({ "fileName": name, "bytesBase64": jscompat::base64(bytes) });
    let value = crate::engine_host::call_native_invocation(
        "diagram.open",
        &input,
        target,
        owner,
        cancel,
        pending,
    )
    .map_err(|error| error.to_string())?;
    decode_open(&value)
}

pub(crate) fn open_native_command_preparation(
    name: &str,
    bytes: &[u8],
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    startup: Option<(
        &shell_contract::ipc::NativeInvocationOperation,
        &shell_contract::ipc::NativeRequestOwner,
    )>,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<Result<DiagramFile, OpenFailure>, String> {
    if !matches!(
        expected,
        shell_contract::ipc::NativeEditorCommand::OpenFile {
            surface: shell_contract::Surface::Diagram,
            ..
        }
    ) {
        return Err("The native command is not a diagram open".into());
    }
    let input = serde_json::json!({ "fileName": name, "bytesBase64": jscompat::base64(bytes) });
    let value = crate::engine_host::call_native_command_preparation(
        "diagram.open",
        &input,
        operation,
        expected,
        startup,
        participants,
        cancel,
        pending,
    )
    .map_err(|error| error.to_string())?;
    decode_open(&value)
}

pub(crate) fn open_owned_preparation(
    name: &str,
    bytes: &[u8],
    origin: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<Result<DiagramFile, OpenFailure>, String> {
    let input = serde_json::json!({ "fileName": name, "bytesBase64": jscompat::base64(bytes) });
    let value = crate::engine_host::call_owned_many(
        "diagram.open",
        &input,
        origin,
        participants,
        cancel,
        pending,
    )
    .map_err(|error| error.to_string())?;
    decode_open(&value)
}

impl DiagramEngine for Engine {
    fn open_recovery_diagram(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<Result<DiagramFile, OpenFailure>, String> {
        Ok(native::open(name, bytes))
    }

    fn open_diagram(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<Result<DiagramFile, OpenFailure>, String> {
        let input = serde_json::json!({ "fileName": name, "bytesBase64": jscompat::base64(bytes) });
        let value = seu_host::call("diagram.open", &input)
            .map_err(|error| format!("{}: {}", error.name, error.message))?;
        decode_open(&value)
    }

    fn diagram_picture_ext_of(&self, path: &str) -> Option<DiagramFormat> {
        match super::super::w_files::diagram_picture_ext(Path::new(path))? {
            "drawio.svg" => Some(DiagramFormat::DrawioSvg),
            "drawio.png" => Some(DiagramFormat::DrawioPng),
            _ => None,
        }
    }

    fn parse_file_to_text(&self, path: &str) -> ParsedText {
        match super::super::w_files::parsed_value(path) {
            Ok(value) => match value.get("ok").and_then(|v| v.as_bool()) {
                Some(ok) => ParsedText {
                    ok,
                    is_text: value.get("kind").and_then(|v| v.as_str()) == Some("text"),
                    text: value
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    damaged: value
                        .get("damaged")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    error: value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                },
                None => ParsedText {
                    error: Some("file parser returned no ok flag".into()),
                    ..ParsedText::default()
                },
            },
            Err(error) => ParsedText {
                error: Some(error),
                ..ParsedText::default()
            },
        }
    }
}
