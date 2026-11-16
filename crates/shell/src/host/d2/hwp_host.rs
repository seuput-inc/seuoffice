mod attachments;
pub(crate) mod keyboard;
mod open_files;
mod shell;

use std::path::PathBuf;
use std::sync::Arc;

use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::slides::ai_ipc::MediaToolOptions;
use serde_json::json;
use shell_contract::runtime::CancelToken;
use shell_contract::{Hosts, ViewId};
use shell_ipc_d2::hwp::{
    FetchedImage, HwpEngine, HwpEngineHost, HwpMemo, HwpShellHost, ParsedFile, TrackedChanges,
};

use super::{hwp_parse, hwp_tracked};
use crate::host::HostContext;

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let engine: HwpEngineHost = Arc::new(Engine);
    let shell: HwpShellHost = Arc::new(shell::Shell::new(cx.clone()));
    hosts.insert(engine);
    hosts.insert(shell);
}

struct Engine;

impl HwpEngine for Engine {
    fn tracked_changes_of(&self, file: &[u8]) -> TrackedChanges {
        hwp_tracked::tracked(file)
    }
    fn without_tracked_changes(&self, file: &[u8]) -> Option<Vec<u8>> {
        hwp_tracked::without(file)
    }
    fn hwp_memos_of(&self, file: &[u8]) -> Result<Vec<HwpMemo>, String> {
        hwp_parse::memos(file)
    }
    fn hwp_to_text(&self, file: &[u8]) -> Result<String, String> {
        hwp_parse::text(file)
    }
    fn open_files(
        &self,
        then: Box<dyn FnOnce(Option<Result<Vec<OpenFileState>, OpenFilesSourceFailed>>)>,
    ) {
        open_files::open_files(then);
    }
    fn diagram_picture_has_no_original(&self, file_name: String, bytes: Vec<u8>) -> bool {
        attachments::diagram_picture_has_no_original(file_name, bytes)
    }
    fn parse_file_to_text(&self, path: String) -> ParsedFile {
        attachments::parse(&path)
    }
    fn checked_hwp_bytes(&self, bytes: Vec<u8>, ext: String) -> Result<Vec<u8>, String> {
        hwp_parse::checked_bytes(bytes, &ext)
    }
    fn fetch_image_bytes(&self, url: String) -> Option<FetchedImage> {
        let tmp = std::env::temp_dir();
        let image = crate::host::net::ai::fetch::remote_image(&url, &tmp.to_string_lossy())?;
        let mime = if image.content_type.contains("png") {
            "image/png"
        } else if image.content_type.contains("gif") {
            "image/gif"
        } else {
            "image/jpeg"
        };
        Some(FetchedImage {
            base64: jscompat::base64(&image.bytes),
            mime: mime.to_owned(),
        })
    }
    fn analyze_media(
        &self,
        _view: ViewId,
        settings_path: PathBuf,
        media_urls: Vec<String>,
        requirements: String,
        cancel: CancelToken,
    ) -> Result<serde_json::Value, String> {
        let op = jscompat::json_value_from_serde(
            &json!({"mediaUrls": media_urls, "requirements": requirements}),
        );
        let statuses = crate::host::net::ai::subscription_statuses();
        let options = MediaToolOptions {
            media_roots: Vec::new(),
            context: jscompat::json_value_from_serde(
                &json!({"path":"screen", "statuses": jscompat::json_value_to_serde(&statuses)}),
            ),
        };
        crate::host::net::ai::media_tools::analyze_media(
            &settings_path.to_string_lossy(),
            &op,
            &options,
            &cancel,
        )
        .map(|value| jscompat::json_value_to_serde(&value))
        .map_err(|error| {
            let value = jscompat::json_value_to_serde(&error);
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| value.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
    }
}
