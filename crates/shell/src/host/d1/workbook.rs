//! Production WorkbookEngine: native sidecar, gateway ports and isolated schemas.

use std::path::Path;
use std::sync::Arc;

use shell_contract::{IpcError, IpcValue};
use shell_ipc_d1::sheets::engine::{
    Converted, CsvConverted, EngineResult, ImportControl, LegacyConversion, OdsConversion,
    OpenedWorkbook, ParsedText, RecalcCell, RecalcEdit, RecalcRead, UnlockFailure, Unlocked,
    WorkbookEngine, engine_error,
};
use shell_ipc_d1::value::{from_json, to_json};

use super::{
    HostContext, sidecar::Client, workbook_crypto, workbook_csv_xlsx, workbook_pdf,
    workbook_promote, workbook_readers, workbook_redactions, workbook_rpc::Rpc, workbook_save,
    workbook_schema,
};
#[path = "workbook_snapshot.rs"]
pub(super) mod native_snapshot;

pub(super) struct Engine {
    rpc: Rpc,
    cx: HostContext,
}

impl Engine {
    pub fn new(cx: &HostContext) -> EngineResult<Self> {
        let path = super::sidecar::binary_path(&cx.app).map_err(engine_error)?;
        let path = path
            .to_str()
            .ok_or_else(|| engine_error("XLSX sidecar path is not UTF-8"))?;
        let client = Arc::new(Client::new(path.to_owned()));
        client.start().map_err(engine_error)?;
        Ok(Self {
            rpc: Rpc(client),
            cx: cx.clone(),
        })
    }

    pub fn shutdown(&self) {
        self.rpc.0.shutdown();
    }
}

impl WorkbookEngine for Engine {
    fn open(
        &self,
        path: &str,
        locale: &str,
        short_date: &str,
        timeout_ms: Option<f64>,
    ) -> EngineResult<OpenedWorkbook> {
        self.rpc.open(path, locale, short_date, timeout_ms)
    }
    fn snapshot_native_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>> {
        native_snapshot::create(document, cancel, pending, None)
    }
    fn snapshot_native_bytes(
        &self,
        bytes: &[u8],
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>> {
        native_snapshot::create(document, cancel, pending, Some(bytes))
    }
    fn open_native_invocation(
        &self,
        snapshot: &Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>,
        locale: &str,
        short_date: &str,
        timeout_ms: Option<f64>,
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<OpenedWorkbook> {
        native_snapshot::validate(snapshot, document, cancel, pending)?;
        let opened = self.rpc.open_native(
            snapshot.path(),
            locale,
            short_date,
            timeout_ms,
            document,
            cancel,
            pending,
        )?;
        if let Err(error) = native_snapshot::admit(document, cancel, pending, || ()) {
            let _ = self.rpc.close(&opened.session_id);
            return Err(error);
        }
        Ok(opened)
    }
    fn snapshot_saved_document(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>> {
        native_snapshot::create_saved(document, actual, actual_cancel, None)
    }
    fn snapshot_saved_bytes(
        &self,
        bytes: &[u8],
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>> {
        native_snapshot::create_saved(document, actual, actual_cancel, Some(bytes))
    }
    fn open_saved_document(
        &self,
        snapshot: &Arc<dyn shell_ipc_d1::sheets::engine::NativeWorkbookSnapshot>,
        locale: &str,
        short_date: &str,
        timeout_ms: Option<f64>,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<OpenedWorkbook> {
        let path = native_snapshot::validate_saved(snapshot, document, actual, actual_cancel)?;
        let opened = self.rpc.open_saved(
            path,
            locale,
            short_date,
            timeout_ms,
            document,
            actual,
            actual_cancel,
        )?;
        if let Err(error) = native_snapshot::admit_saved(document, actual, actual_cancel, |_| ()) {
            let _ = self.rpc.close(&opened.session_id);
            return Err(error);
        }
        Ok(opened)
    }
    fn close(&self, session_id: &str) -> EngineResult<()> {
        self.rpc.close(session_id)
    }
    fn on_process_exit(&self, listener: Box<dyn Fn() + Send + Sync>) {
        let listener: Arc<dyn Fn() + Send + Sync> = Arc::from(listener);
        if self.rpc.0.on_process_exit(listener.clone()).is_err() {
            listener();
        }
    }
    fn read_range(&self, request: &IpcValue) -> EngineResult<IpcValue> {
        self.rpc.read("read_range", "range", request)
    }
    fn read_formula_cells(&self, request: &IpcValue) -> EngineResult<IpcValue> {
        self.rpc.read("read_formula_cells", "formulaCells", request)
    }
    fn find_cells(&self, request: &IpcValue) -> EngineResult<IpcValue> {
        self.rpc.read("find_cells", "findCells", request)
    }
    fn read_row_outline(&self, request: &IpcValue) -> EngineResult<IpcValue> {
        self.rpc.read("read_row_outline", "rowOutline", request)
    }
    fn read_media(&self, request: &IpcValue) -> EngineResult<IpcValue> {
        self.rpc.read("read_media", "media", request)
    }
    fn list_external_links(&self, path: &str, base_dir: &str) -> EngineResult<IpcValue> {
        self.rpc
            .external_links("list_external_links", path, base_dir, false)
    }
    fn update_external_links(&self, path: &str, base_dir: &str) -> EngineResult<IpcValue> {
        self.rpc
            .external_links("update_external_links", path, base_dir, true)
    }
    fn read_external_link_usage(&self, path: &str) -> EngineResult<IpcValue> {
        self.rpc.archive(
            "read_external_link_usage",
            serde_json::json!({ "path": path }),
        )
    }
    fn recalc_cells(
        &self,
        path: &str,
        edits: &[RecalcEdit],
        reads: &[RecalcRead],
    ) -> EngineResult<Vec<RecalcCell>> {
        self.rpc.recalc(path, edits, reads)
    }
    fn read_pivot_definition(
        &self,
        snapshot: &str,
        pivot_path: &str,
        cache_path: &str,
    ) -> EngineResult<IpcValue> {
        workbook_readers::pivot(&self.rpc, snapshot, pivot_path, cache_path)
    }
    fn read_slicer_state(&self, snapshot: &str) -> EngineResult<IpcValue> {
        workbook_readers::slicers(&self.rpc, snapshot)
    }
    fn list_connections(&self, snapshot: &str) -> EngineResult<IpcValue> {
        workbook_readers::connections(&self.rpc, snapshot)
    }
    fn refresh_connection(&self, snapshot: &str, connection_id: &str) -> EngineResult<IpcValue> {
        workbook_readers::refresh_connection(&self.rpc, snapshot, connection_id)
    }
    fn read_queries(&self, snapshot: &str) -> EngineResult<IpcValue> {
        workbook_readers::queries(&self.rpc, snapshot)
    }
    fn read_redactions(&self, snapshot: &str) -> EngineResult<IpcValue> {
        workbook_redactions::read(&self.rpc, snapshot)
    }
    fn write_workbook(
        &self,
        source: &str,
        target: &str,
        resolved: &serde_json::Value,
    ) -> EngineResult<IpcValue> {
        workbook_save::write(&self.rpc, source, target, resolved)
    }
    fn convert_to_ods(&self, source: &str, target: &str) -> EngineResult<OdsConversion> {
        self.rpc.convert_to_ods(source, target)
    }
    fn convert_workbook(&self, path: &str, target: &str) -> EngineResult<Converted> {
        self.rpc.convert_workbook(path, target)
    }
    fn convert_legacy_workbook(
        &self,
        path: &str,
        target: &str,
        preferred_charset: Option<&str>,
    ) -> EngineResult<LegacyConversion> {
        super::workbook_legacy::convert(&self.rpc, path, target, preferred_charset)
    }
    fn csv_to_xlsx_for_open(
        &self,
        csv: &[u8],
        charset: Option<&str>,
        sheet_name: &str,
        delimiter: Option<char>,
        control: Option<&ImportControl<'_>>,
    ) -> EngineResult<CsvConverted> {
        workbook_csv_xlsx::convert(csv, charset, sheet_name, delimiter, false, control)
    }
    fn blank_xlsx(&self) -> EngineResult<Vec<u8>> {
        workbook_csv_xlsx::convert(b"", None, "Sheet1", None, false, None)
            .map(|result| result.buffer)
    }
    fn sheet_csv_to_xlsx(&self, content: &str, sheet_name: &str) -> EngineResult<Vec<u8>> {
        workbook_csv_xlsx::convert(
            content.as_bytes(),
            Some("utf-8"),
            sheet_name,
            None,
            true,
            None,
        )
        .map(|result| result.buffer)
    }
    fn promote_file_atomically(&self, from: &str, to: &str) -> EngineResult<()> {
        workbook_promote::promote(from, to)
    }
    fn is_cfb_header(&self, head: &[u8]) -> bool {
        main_core::packages::office_encryption::is_cfb_header(head)
    }
    fn is_encrypted_office_file(&self, bytes: &[u8]) -> bool {
        main_core::packages::office_encryption::is_encrypted_office_file(bytes)
    }
    fn encrypt_ooxml(&self, bytes: &[u8], password: &str) -> EngineResult<Vec<u8>> {
        main_core::packages::office_encryption::encrypt_ooxml(bytes, password).map_err(engine_error)
    }
    fn unlock_office_file(
        &self,
        bytes: &[u8],
        ask: &mut dyn FnMut(bool) -> Result<Option<String>, UnlockFailure>,
    ) -> Result<Option<Unlocked>, UnlockFailure> {
        workbook_crypto::unlock(bytes, ask)
    }
    fn render_pdf(&self, html: &str, request: &IpcValue) -> EngineResult<Vec<u8>> {
        workbook_pdf::render(&self.cx, html, request)
    }
    fn render_pdf_owned(
        &self,
        html: &str,
        request: &IpcValue,
        authority: shell_ipc_d1::sheets::engine::WorkbookPrintAuthority<'_>,
    ) -> EngineResult<Vec<u8>> {
        workbook_pdf::render_owned(&self.cx, html, request, authority)
    }
    fn parse_save_request(&self, input: &IpcValue) -> EngineResult<IpcValue> {
        workbook_schema::parse("save", input)
    }
    fn parse_export_pdf_request(&self, input: &IpcValue) -> EngineResult<IpcValue> {
        workbook_schema::parse("exportPdf", input)
    }
    fn parse_edits_chunk(&self, edits_json: &str) -> EngineResult<Vec<serde_json::Value>> {
        let parsed = jscompat::json_parse(edits_json).map_err(|error| IpcError {
            name: Some("SyntaxError".into()),
            message: error.message().to_owned(),
        })?;
        let parsed = workbook_schema::parse("editsChunk", &from_json(&parsed))?;
        let IpcValue::Array(edits) = parsed else {
            return Err(engine_error("workbook edit schema returned a non-array"));
        };
        edits
            .iter()
            .map(|edit| {
                to_json(edit)
                    .map(|value| jscompat::json_value_to_serde(&value))
                    .ok_or_else(|| engine_error("workbook edit is not JSON"))
            })
            .collect()
    }
    fn parse_file_to_text(&self, path: &str) -> ParsedText {
        match super::super::w_files::parsed_value(path) {
            Ok(value) => match value.get("ok").and_then(|value| value.as_bool()) {
                Some(ok) => ParsedText {
                    ok,
                    is_text: value.get("kind").and_then(|value| value.as_str()) == Some("text"),
                    text: value
                        .get("text")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    damaged: value
                        .get("damaged")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    error: value
                        .get("error")
                        .and_then(|value| value.as_str())
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
    fn diagram_picture_ext_of(&self, path: &str) -> Option<&'static str> {
        super::super::w_files::diagram_picture_ext(Path::new(path))
    }
}
