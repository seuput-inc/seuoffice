//! Stand-ins for the sheets hosts, for the unit tests and for `ipc-probe`.
//! Answers come from `case["sheets"]` (the case's `hosts.d1`), each call is a
//! line of the shared host log named after the TypeScript function it stands
//! for, and the files the engine would write are small marker files.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use file_commit::PublicationMode;
use jscompat::node_path::host::basename;
use serde_json::Value;
use shell_contract::ipc::NativeSavedInput;
use shell_contract::{Hosts, IpcError, IpcValue, ViewId, WindowId};

use super::conv::serde_to_ipc;
use super::engine::*;
use super::schema;
use crate::slides::fake::synthetic_generated;
use crate::value::{num, obj, string};

type Log = Arc<Mutex<Vec<Value>>>;

/// The work folder of an ODS export is named after the process's temp folder
/// and a random suffix; the log writes it as `<ods>`
fn scrub(path: &str) -> String {
    match path.find("seuoffice-ods-export-") {
        Some(at) => {
            let rest = &path[at..];
            let tail = rest.find('/').map_or("", |end| &rest[end..]);
            format!("<ods>{tail}")
        }
        None => path.to_owned(),
    }
}

fn io(path: &str, e: std::io::Error) -> IpcError {
    engine_error(format!("{e}: {path}"))
}

pub struct FakeEngine {
    log: Option<Log>,
    script: Value,
    sessions: AtomicU64,
    exit: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    effects: Mutex<Option<shell_contract::hosts::OwnedEffectsHost>>,
}

impl FakeEngine {
    /// The kit of the unit tests installs the owned effects after the fakes were made, so a
    /// test that opens a saved workbook hands them to its engine here.
    #[cfg(all(test, unix))]
    pub fn use_effects(&self, effects: shell_contract::hosts::OwnedEffectsHost) {
        *self.effects.lock().unwrap() = Some(effects);
    }

    fn line(&self, items: Vec<Value>) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(Value::Array(items));
        }
    }

    fn answer(&self, name: &str) -> IpcValue {
        match self.script.get(name) {
            Some(value) => serde_to_ipc(value),
            None => obj([("fake", string(name))]),
        }
    }

    fn fails(&self, name: &str) -> Result<(), IpcError> {
        match self
            .script
            .get(name)
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
        {
            Some(message) => Err(engine_error(message)),
            None => Ok(()),
        }
    }

    fn mark(&self, target: &str, bytes: &[u8]) -> EngineResult<()> {
        fs::write(target, bytes).map_err(|e| io(target, e))
    }

    /// the sidecar died: the listener runs, as the engine's process watcher would
    pub fn exit(&self) {
        if let Some(listener) = self.exit.lock().unwrap().as_ref() {
            listener();
        }
    }
}
struct FakeNativeSnapshot {
    path: String,
    document: shell_contract::ipc::NativeInvocationDocument,
    retained: std::sync::atomic::AtomicBool,
}

impl NativeWorkbookSnapshot for FakeNativeSnapshot {
    fn path(&self) -> &str {
        &self.path
    }
    fn retain_for_session(&self) -> EngineResult<()> {
        if self.retained.swap(true, Ordering::AcqRel) {
            return Err(engine_error("Snapshot already retained"));
        }
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Drop for FakeNativeSnapshot {
    fn drop(&mut self) {
        if !self.retained.load(Ordering::Acquire) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct FakeSavedSnapshot {
    path: String,
    document: shell_contract::ipc::NativeSavedDocument,
    retained: std::sync::atomic::AtomicBool,
}

impl NativeWorkbookSnapshot for FakeSavedSnapshot {
    fn path(&self) -> &str {
        &self.path
    }
    fn retain_for_session(&self) -> EngineResult<()> {
        if self.retained.swap(true, Ordering::AcqRel) {
            return Err(engine_error("Snapshot already retained"));
        }
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Drop for FakeSavedSnapshot {
    fn drop(&mut self) {
        if !self.retained.load(Ordering::Acquire) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl FakeEngine {
    fn saved_input(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<shell_contract::ipc::NativeSavedInput> {
        let effects = self
            .effects
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| engine_error("Saved workbook effects are unavailable"))?;
        let target = document.target();
        let additional: Vec<_> = target.additional().iter().collect();
        let mut input = None;
        effects
            .with_native_saved_document_same_origin(
                document,
                actual,
                actual_cancel,
                target.participants(),
                target.cancel(),
                target.pending(),
                &additional,
                &mut |value| {
                    input = Some(value.clone());
                    Ok(())
                },
            )
            .map_err(engine_error)?;
        input.ok_or_else(|| engine_error("Saved workbook input was not admitted"))
    }
}

fn fake_native_valid(
    document: &shell_contract::ipc::NativeInvocationDocument,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> EngineResult<()> {
    shell_contract::hosts::synthetic::with_native_invocation(
        document,
        document.owner(),
        &[],
        cancel,
        pending,
        &mut || Ok(()),
    )
    .map_err(engine_error)
}

fn fake_snapshot(
    bytes: &[u8],
    extension: &str,
    document: &shell_contract::ipc::NativeInvocationDocument,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
    fake_native_valid(document, cancel, pending)?;
    let path = std::env::temp_dir().join(format!(
        "sheets-fake-{}.{}",
        jscompat::random_uuid(),
        extension
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| engine_error(error.to_string()))?;
    use std::io::Write;
    file.write_all(bytes).map_err(|error| {
        let _ = fs::remove_file(&path);
        engine_error(error.to_string())
    })?;
    let snapshot = Arc::new(FakeNativeSnapshot {
        path: path.to_string_lossy().into_owned(),
        document: document.clone(),
        retained: std::sync::atomic::AtomicBool::new(false),
    });
    fake_native_valid(document, cancel, pending)?;
    Ok(snapshot)
}

impl WorkbookEngine for FakeEngine {
    fn snapshot_saved_document(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
        if document.target().surface() != shell_contract::Surface::Sheets {
            return Err(engine_error("The saved document is not a workbook"));
        }
        let input = self.saved_input(document, actual, actual_cancel)?;
        let mut path =
            std::env::temp_dir().join(format!("sheets-fake-{}", jscompat::random_uuid()));
        if let Some(extension) = document.target().path().path().extension() {
            path.set_extension(extension);
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| engine_error(error.to_string()))?;
        use std::io::Write;
        if let Err(error) = file.write_all(input.bytes()) {
            let _ = fs::remove_file(&path);
            return Err(engine_error(error.to_string()));
        }
        let path_text = match path.to_str() {
            Some(path) => path.to_owned(),
            None => {
                let _ = fs::remove_file(&path);
                return Err(engine_error("The workbook snapshot path is not Unicode"));
            }
        };
        let snapshot = Arc::new(FakeSavedSnapshot {
            path: path_text,
            document: document.clone(),
            retained: std::sync::atomic::AtomicBool::new(false),
        });
        self.saved_input(document, actual, actual_cancel)?;
        Ok(snapshot)
    }
    fn snapshot_saved_bytes(
        &self,
        bytes: &[u8],
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
        if document.target().surface() != shell_contract::Surface::Sheets {
            return Err(engine_error("The saved document is not a workbook"));
        }
        self.saved_input(document, actual, actual_cancel)?;
        let path =
            std::env::temp_dir().join(format!("sheets-fake-{}.xlsx", jscompat::random_uuid()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| engine_error(error.to_string()))?;
        use std::io::Write;
        if let Err(error) = file.write_all(bytes) {
            let _ = fs::remove_file(&path);
            return Err(engine_error(error.to_string()));
        }
        let path_text = match path.to_str() {
            Some(path) => path.to_owned(),
            None => {
                let _ = fs::remove_file(&path);
                return Err(engine_error("The workbook snapshot path is not Unicode"));
            }
        };
        let snapshot = Arc::new(FakeSavedSnapshot {
            path: path_text,
            document: document.clone(),
            retained: std::sync::atomic::AtomicBool::new(false),
        });
        self.saved_input(document, actual, actual_cancel)?;
        Ok(snapshot)
    }

    fn open_saved_document(
        &self,
        snapshot: &Arc<dyn NativeWorkbookSnapshot>,
        locale: &str,
        short_date: &str,
        timeout: Option<f64>,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<OpenedWorkbook> {
        let snapshot = snapshot
            .as_any()
            .downcast_ref::<FakeSavedSnapshot>()
            .ok_or_else(|| engine_error("Invalid saved workbook snapshot"))?;
        if !snapshot.document.target().same_target(document.target()) {
            return Err(engine_error(
                "The snapshot belongs to another saved document",
            ));
        }
        self.saved_input(document, actual, actual_cancel)?;
        let opened = self.open(&snapshot.path, locale, short_date, timeout)?;
        if let Err(error) = self.saved_input(document, actual, actual_cancel) {
            self.close(&opened.session_id)?;
            return Err(error);
        }
        Ok(opened)
    }

    fn snapshot_native_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
        fake_native_valid(document, cancel, pending)?;
        let path = match document.operation().selection() {
            shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                surface: shell_contract::Surface::Sheets,
                input,
                ..
            } => input,
            _ => return Err(engine_error("The invocation is not a Sheets export")),
        };
        let bytes = fs::read(path).map_err(|error| engine_error(error.to_string()))?;
        fake_snapshot(
            &bytes,
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("xlsx"),
            document,
            cancel,
            pending,
        )
    }
    fn snapshot_native_bytes(
        &self,
        bytes: &[u8],
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
        fake_snapshot(bytes, "xlsx", document, cancel, pending)
    }
    fn open_native_invocation(
        &self,
        snapshot: &Arc<dyn NativeWorkbookSnapshot>,
        locale: &str,
        short_date: &str,
        timeout: Option<f64>,
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> EngineResult<OpenedWorkbook> {
        fake_native_valid(document, cancel, pending)?;
        let snapshot = snapshot
            .as_any()
            .downcast_ref::<FakeNativeSnapshot>()
            .ok_or_else(|| engine_error("Invalid snapshot"))?;
        if snapshot.document.owner().view != document.owner().view
            || !snapshot
                .document
                .created()
                .same_generation(document.created())
            || !snapshot
                .document
                .operation()
                .authority_generation()
                .same_generation(document.operation().authority_generation())
        {
            return Err(engine_error("Snapshot belongs to another invocation"));
        }
        let opened = self.open(&snapshot.path, locale, short_date, timeout)?;
        if let Err(error) = fake_native_valid(document, cancel, pending) {
            self.close(&opened.session_id)?;
            return Err(error);
        }
        Ok(opened)
    }

    fn open(
        &self,
        path: &str,
        locale: &str,
        short_date: &str,
        _timeout: Option<f64>,
    ) -> EngineResult<OpenedWorkbook> {
        self.line(vec![
            "xlsx.open".into(),
            scrub(path).into(),
            locale.into(),
            short_date.into(),
        ]);
        self.fails("open")?;
        let n = self.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = format!("00000000-0000-4000-8000-{n:012}");
        let sheets = vec![OpenedSheet {
            id: "s1".into(),
            name: "Sheet1".into(),
            source_xml_bytes: self
                .script
                .get("sheet")
                .and_then(|s| s.get("sourceXmlBytes"))
                .and_then(Value::as_f64),
        }];
        // the sheet as the sidecar's result schema returns it
        let sheet = match self.script.get("sheet") {
            Some(sheet) => serde_to_ipc(sheet),
            None => obj([("id", string("s1")), ("name", string("Sheet1"))]),
        };
        let value = obj([
            ("sessionId", string(session_id.clone())),
            ("name", string(basename(path, None))),
            ("entryCount", num(1.0)),
            ("sheets", IpcValue::Array(vec![sheet])),
            ("styles", IpcValue::Array(Vec::new())),
            ("dxfStyles", IpcValue::Array(Vec::new())),
            ("visuals", IpcValue::Array(Vec::new())),
            ("definedNames", IpcValue::Array(Vec::new())),
        ]);
        Ok(OpenedWorkbook {
            value,
            session_id,
            sheets,
        })
    }

    fn close(&self, session_id: &str) -> EngineResult<()> {
        self.line(vec!["xlsx.close".into(), session_id.into()]);
        Ok(())
    }

    fn on_process_exit(&self, listener: Box<dyn Fn() + Send + Sync>) {
        *self.exit.lock().unwrap() = Some(listener);
    }

    fn read_range(&self, _request: &IpcValue) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readRange".into()]);
        self.fails("readRange")?;
        Ok(self.answer("readRange"))
    }

    fn read_formula_cells(&self, _request: &IpcValue) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readFormulaCells".into()]);
        Ok(self.answer("readFormulaCells"))
    }

    fn find_cells(&self, _request: &IpcValue) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.findCells".into()]);
        Ok(self.answer("findCells"))
    }

    fn read_row_outline(&self, _request: &IpcValue) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readRowOutline".into()]);
        Ok(self.answer("readRowOutline"))
    }

    fn read_media(&self, _request: &IpcValue) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readMedia".into()]);
        Ok(self.answer("readMedia"))
    }

    fn list_external_links(&self, path: &str, base_dir: &str) -> EngineResult<IpcValue> {
        self.line(vec![
            "xlsx.listExternalLinks".into(),
            path.into(),
            base_dir.into(),
        ]);
        Ok(self.answer("listExternalLinks"))
    }

    fn update_external_links(&self, path: &str, base_dir: &str) -> EngineResult<IpcValue> {
        self.line(vec![
            "xlsx.updateExternalLinks".into(),
            path.into(),
            base_dir.into(),
        ]);
        Ok(self.answer("updateExternalLinks"))
    }

    fn read_external_link_usage(&self, path: &str) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readExternalLinkUsage".into(), path.into()]);
        Ok(self.answer("readExternalLinkUsage"))
    }

    fn recalc_cells(
        &self,
        path: &str,
        edits: &[RecalcEdit],
        reads: &[RecalcRead],
    ) -> EngineResult<Vec<RecalcCell>> {
        self.line(vec![
            "xlsx.recalcCells".into(),
            path.into(),
            edits.len().into(),
            reads.len().into(),
        ]);
        Ok(edits
            .iter()
            .map(|edit| RecalcCell {
                sheet: edit.sheet.clone(),
                row: edit.row,
                column: edit.column,
                formatted: edit.input.clone(),
                number: edit.input.parse().ok(),
                is_error: false,
                is_formula: edit.input.starts_with('='),
            })
            .collect())
    }

    fn read_pivot_definition(
        &self,
        snapshot: &str,
        pivot: &str,
        cache: &str,
    ) -> EngineResult<IpcValue> {
        self.line(vec![
            "xlsx.readPivotDefinition".into(),
            snapshot.into(),
            pivot.into(),
            cache.into(),
        ]);
        Ok(self.answer("readPivotDefinition"))
    }

    fn read_slicer_state(&self, snapshot: &str) -> EngineResult<IpcValue> {
        // a workbook without slicer parts is answered before the parser runs
        let answer = match self.script.get("readSlicerState") {
            Some(value) => serde_to_ipc(value),
            None => IpcValue::Array(Vec::new()),
        };
        if answer != IpcValue::Array(Vec::new()) {
            self.line(vec!["xlsx.readSlicerState".into(), snapshot.into()]);
        }
        Ok(answer)
    }

    fn list_connections(&self, snapshot: &str) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.listConnections".into(), snapshot.into()]);
        Ok(self.answer("listConnections"))
    }

    fn refresh_connection(&self, snapshot: &str, id: &str) -> EngineResult<IpcValue> {
        self.line(vec![
            "xlsx.refreshConnection".into(),
            snapshot.into(),
            id.into(),
        ]);
        let flag = |name: &str| {
            self.script
                .get("refreshOutcome")
                .and_then(|v| v.get(name))
                .and_then(Value::as_bool)
                == Some(true)
        };
        if flag("unknown") {
            return Err(engine_error(
                "This workbook has no refreshable web query with that id.",
            ));
        }
        if flag("noTable") {
            return Err(engine_error("Could not read a table from that address."));
        }
        Ok(self.answer("refreshConnection"))
    }

    fn read_queries(&self, snapshot: &str) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readQueries".into(), snapshot.into()]);
        Ok(self.answer("readQueries"))
    }

    fn read_redactions(&self, snapshot: &str) -> EngineResult<IpcValue> {
        self.line(vec!["xlsx.readRedactions".into(), snapshot.into()]);
        match self.script.get("readRedactions") {
            Some(value) => Ok(serde_to_ipc(value)),
            None => Ok(obj([("status", string("absent"))])),
        }
    }

    fn write_workbook(
        &self,
        source: &str,
        target: &str,
        _resolved: &Value,
    ) -> EngineResult<IpcValue> {
        self.line(vec![
            "xlsx.writeWorkbook".into(),
            scrub(source).into(),
            scrub(target).into(),
        ]);
        self.fails("writeWorkbook")?;
        let mut bytes = fs::read(source).unwrap_or_default();
        bytes.extend_from_slice(b"+edits");
        self.mark(target, &bytes)?;
        Ok(IpcValue::Array(vec![string("xl/worksheets/sheet1.xml")]))
    }

    fn convert_to_ods(&self, source: &str, target: &str) -> EngineResult<OdsConversion> {
        self.line(vec![
            "xlsx.convertToOds".into(),
            scrub(source).into(),
            scrub(target).into(),
        ]);
        self.fails("convertToOds")?;
        self.mark(target, b"ods")?;
        let scripted = self.script.get("convertToOds");
        let formulas = scripted
            .and_then(|v| v.get("formulasDropped"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let dropped: Vec<(&'static str, f64)> = [
            "macros",
            "charts",
            "images",
            "shapes",
            "conditionalFormats",
            "validations",
            "pivots",
            "slicers",
            "sparklines",
            "comments",
        ]
        .into_iter()
        .map(|kind| {
            let count = scripted
                .and_then(|v| v.get("dropped"))
                .and_then(|d| d.get(kind))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            (kind, count)
        })
        .collect();
        Ok(OdsConversion {
            value: obj([
                ("formulasDropped", num(formulas)),
                (
                    "dropped",
                    IpcValue::Object(
                        dropped
                            .iter()
                            .map(|(kind, count)| ((*kind).to_owned(), num(*count)))
                            .collect(),
                    ),
                ),
            ]),
            formulas_dropped: formulas,
            dropped,
        })
    }

    fn convert_workbook(&self, path: &str, target: &str) -> EngineResult<Converted> {
        self.line(vec![
            "xlsx.convertWorkbook".into(),
            scrub(path).into(),
            scrub(target).into(),
        ]);
        self.fails("convertWorkbook")?;
        self.mark(target, b"xlsx")?;
        Ok(Converted {
            sheets: 1.0,
            formulas_dropped: 0.0,
        })
    }

    fn convert_legacy_workbook(
        &self,
        path: &str,
        target: &str,
        charset: Option<&str>,
    ) -> EngineResult<LegacyConversion> {
        self.line(vec![
            "legacy.convertLegacyWorkbook".into(),
            scrub(path).into(),
            scrub(target).into(),
            charset.map_or(Value::Null, Value::from),
        ]);
        self.fails("convertLegacyWorkbook")?;
        self.mark(target, b"xlsx")?;
        Ok(LegacyConversion::Workbook {
            formulas_dropped: 0.0,
        })
    }

    fn csv_to_xlsx_for_open(
        &self,
        csv: &[u8],
        charset: Option<&str>,
        sheet: &str,
        delimiter: Option<char>,
        control: Option<&ImportControl<'_>>,
    ) -> EngineResult<CsvConverted> {
        self.line(vec![
            "csv.csvToXlsxBufferForOpen".into(),
            csv.len().into(),
            charset.map_or(Value::Null, Value::from),
            sheet.into(),
            delimiter.map_or(Value::Null, |c| Value::from(c.to_string())),
        ]);
        if let (Some(control), Some(progress)) = (control, self.script.get("progress")) {
            (control.on_progress)(serde_to_ipc(progress));
        }
        Ok(CsvConverted {
            buffer: b"xlsx".to_vec(),
            empty: csv.is_empty(),
        })
    }

    fn blank_xlsx(&self) -> EngineResult<Vec<u8>> {
        self.line(vec!["csv.blankXlsx".into()]);
        Ok(b"xlsx".to_vec())
    }

    fn sheet_csv_to_xlsx(&self, content: &str, sheet: &str) -> EngineResult<Vec<u8>> {
        self.line(vec![
            "csv.sheetCsvToXlsxBuffer".into(),
            content.into(),
            sheet.into(),
        ]);
        let mut bytes = b"xlsx:".to_vec();
        bytes.extend_from_slice(content.as_bytes());
        Ok(bytes)
    }

    fn promote_file_atomically(&self, from: &str, to: &str) -> EngineResult<()> {
        self.line(vec![
            "xlsx.promoteFileAtomically".into(),
            from.into(),
            to.into(),
        ]);
        fs::rename(from, to).map_err(|e| io(from, e))
    }

    fn is_cfb_header(&self, head: &[u8]) -> bool {
        head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1])
    }

    fn is_encrypted_office_file(&self, bytes: &[u8]) -> bool {
        self.is_cfb_header(bytes)
    }

    fn encrypt_ooxml(&self, bytes: &[u8], password: &str) -> EngineResult<Vec<u8>> {
        self.line(vec![
            "encryption.encryptOoxml".into(),
            bytes.len().into(),
            password.into(),
        ]);
        let mut out = bytes.to_vec();
        out.extend_from_slice(format!("+enc:{password}").as_bytes());
        Ok(out)
    }

    fn unlock_office_file(
        &self,
        bytes: &[u8],
        ask: &mut dyn FnMut(bool) -> Result<Option<String>, UnlockFailure>,
    ) -> Result<Option<Unlocked>, UnlockFailure> {
        self.line(vec![
            "encryption.unlockOfficeFile".into(),
            bytes.len().into(),
        ]);
        match ask(false)? {
            Some(password) => Ok(Some(Unlocked {
                bytes: b"xlsx".to_vec(),
                password,
                asked: true,
            })),
            None => Ok(None),
        }
    }

    fn render_pdf(&self, html: &str, _request: &IpcValue) -> EngineResult<Vec<u8>> {
        let _ = html;
        self.line(vec!["pdf.renderPdf".into()]);
        Ok(b"%PDF-1.7".to_vec())
    }
    fn render_pdf_owned(
        &self,
        html: &str,
        request: &IpcValue,
        authority: super::engine::WorkbookPrintAuthority<'_>,
    ) -> EngineResult<Vec<u8>> {
        match authority {
            super::engine::WorkbookPrintAuthority::Interactive {
                origin,
                destination,
                cancel,
                pending,
            } => owned_fake_check(origin, destination, cancel, pending)
                .map_err(crate::value::thrown)?,
            super::engine::WorkbookPrintAuthority::NativeInvocation {
                target,
                owner,
                cancel,
                pending,
            } => {
                if target.view() != owner.view
                    || !owner
                        .context
                        .renderer()
                        .is_some_and(|renderer| renderer.same_generation(target.created()))
                {
                    return Err(crate::value::thrown(
                        "The synthetic headless renderer was replaced",
                    ));
                }
                target
                    .with_active(|| owned_fake_check(owner, owner, cancel, pending))
                    .ok_or_else(|| {
                        crate::value::thrown("The synthetic headless operation expired")
                    })?
                    .map_err(crate::value::thrown)?;
            }
            super::engine::WorkbookPrintAuthority::ExistingDocument {
                binding,
                participants,
                cancel,
                pending,
                additional,
            } => {
                let mut result = None;
                shell_contract::hosts::synthetic::with_existing_document_save_extra(
                    binding,
                    participants,
                    cancel,
                    pending,
                    &additional.iter().collect::<Vec<_>>(),
                    &mut || {
                        result = Some(self.render_pdf(html, request));
                        Ok(())
                    },
                )
                .map_err(crate::value::thrown)?;
                return result
                    .ok_or_else(|| crate::value::thrown("The synthetic print was not admitted"))?;
            }
        }
        self.render_pdf(html, request)
    }

    fn parse_save_request(&self, input: &IpcValue) -> EngineResult<IpcValue> {
        // zod's `.default([])` of the request's lists
        const LISTS: [&str; 18] = [
            "sheetOps",
            "sheetOrder",
            "edits",
            "structuralOps",
            "filterStates",
            "hyperlinkEdits",
            "cfStates",
            "dvStates",
            "noteStates",
            "pageSetupStates",
            "pivotAdditions",
            "pivotRefreshUpdates",
            "protectedRangeStates",
            "sheetProtections",
            "sparklineAdditions",
            "tableAdditions",
            "visualAdditions",
            "formulaValues",
        ];
        let IpcValue::Object(entries) = input else {
            return Err(not_an_object(input));
        };
        let mut entries = entries.clone();
        for key in LISTS {
            if !entries.iter().any(|(k, _)| k == key) {
                entries.push((key.to_owned(), IpcValue::Array(Vec::new())));
            }
        }
        if !entries.iter().any(|(k, _)| k == "slicerState") {
            entries.push(("slicerState".to_owned(), IpcValue::Null));
        }
        Ok(IpcValue::Object(entries))
    }

    fn parse_export_pdf_request(&self, input: &IpcValue) -> EngineResult<IpcValue> {
        match input {
            IpcValue::Object(_) => Ok(input.clone()),
            _ => Err(not_an_object(input)),
        }
    }

    fn parse_edits_chunk(&self, edits_json: &str) -> EngineResult<Vec<Value>> {
        // `JSON.parse` then `saveEditsChunkArraySchema` (an array of 1..=100000)
        let value = match serde_json::from_str::<Value>(edits_json) {
            Ok(value) => value,
            Err(_) => {
                let message = if edits_json == "not json" {
                    "Unexpected token 'o', \"not json\" is not valid JSON".to_owned()
                } else {
                    "Unexpected token in JSON".to_owned()
                };
                return Err(IpcError {
                    name: Some("SyntaxError".into()),
                    message,
                });
            }
        };
        match value {
            Value::Array(edits) if !edits.is_empty() => Ok(edits),
            other => {
                // an empty or non-array value is refused with zod's own issues
                let probe = schema::array(schema::string()).min(1).max(100_000);
                match schema::parse(&probe, &serde_to_ipc(&other)) {
                    Err(error) => Err(error),
                    Ok(()) => Ok(Vec::new()),
                }
            }
        }
    }

    fn parse_file_to_text(&self, path: &str) -> ParsedText {
        self.line(vec!["fileParse.parseFileToText".into(), path.into()]);
        // `script["parse"][basename]` is the parser's own answer for that file
        if let Some(scripted) = self
            .script
            .get("parse")
            .and_then(|p| p.get(basename(path, None)))
        {
            return ParsedText {
                ok: scripted.get("ok").and_then(Value::as_bool).unwrap_or(false),
                is_text: scripted.get("kind").and_then(Value::as_str) == Some("text"),
                text: scripted
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                damaged: scripted
                    .get("damaged")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                error: scripted
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
        }
        match fs::read(path) {
            Ok(bytes) => ParsedText {
                ok: true,
                is_text: true,
                text: Some(String::from_utf8_lossy(&bytes).into_owned()),
                ..ParsedText::default()
            },
            Err(_) => ParsedText {
                error: Some("unreadable".into()),
                ..ParsedText::default()
            },
        }
    }

    fn diagram_picture_ext_of(&self, path: &str) -> Option<&'static str> {
        // `script["pictureExt"][basename]`: the diagram name the parser finds in a picture
        match self
            .script
            .get("pictureExt")
            .and_then(|p| p.get(basename(path, None)))
            .and_then(Value::as_str)
        {
            Some("drawio.png") => Some("drawio.png"),
            Some("drawio.svg") => Some("drawio.svg"),
            _ => None,
        }
    }
}

struct FakePrompt(String);

impl PasswordPrompt for FakePrompt {
    fn ask(&self, _retry: bool) -> Result<Option<String>, UnlockFailure> {
        Ok(Some(self.0.clone()))
    }

    fn close(&self) {}
}

/// a delay and what to run after it
pub type Timer = (u64, Box<dyn FnOnce() + Send>);

pub struct FakeShell {
    log: Option<Log>,
    script: Value,
    os_shell: Option<shell_contract::hosts::OsShellHost>,
    /// what `after` was asked to run, by delay
    pub timers: Mutex<Vec<Timer>>,
    pub opened: Mutex<Vec<(ViewId, String)>>,
    uuids: AtomicU64,
    #[cfg(test)]
    clock_override: Mutex<Option<f64>>,
}

impl FakeShell {
    fn line(&self, items: Vec<Value>) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(Value::Array(items));
        }
    }
    #[cfg(test)]
    pub fn set_clock(&self, now: f64) {
        *self.clock_override.lock().unwrap() = Some(now);
    }

    /// runs the earliest timer scheduled with this delay, and no other
    #[cfg(test)]
    pub fn fire_oldest(&self, ms: u64) {
        let job = {
            let mut timers = self.timers.lock().unwrap();
            let at = timers.iter().position(|(at, _)| *at == ms);
            at.map(|at| timers.remove(at).1)
        };
        if let Some(job) = job {
            job();
        }
    }

    /// runs the timers scheduled with this delay
    pub fn fire(&self, ms: u64) {
        let due: Vec<_> = {
            let mut timers = self.timers.lock().unwrap();
            let (due, rest): (Vec<_>, Vec<_>) = timers.drain(..).partition(|(at, _)| *at == ms);
            *timers = rest;
            due
        };
        for (_, job) in due {
            job();
        }
    }
}

fn log_open_generated(log: &Option<Log>, path: &str) {
    if let Some(log) = log {
        log.lock().unwrap().push(Value::Array(vec![
            "shell.openGeneratedPath".into(),
            path.into(),
        ]));
    }
}

fn check_written_input(
    output: &shell_contract::ipc::SavedPublication,
    written: Option<&NativeSavedInput>,
) -> Result<(), String> {
    if written.is_some_and(|input| !input.output().same_publication(output)) {
        return Err("The synthetic saved input does not match the publication".into());
    }
    Ok(())
}

fn generated_path(output: &shell_contract::ipc::SavedPublication) -> Result<String, String> {
    output
        .path()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "The synthetic generated path is not Unicode".into())
}

/// What the shell's committed receipt binds for the publication's own authority.
fn committed_receipt(
    output: &shell_contract::ipc::SavedPublication,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
) -> Result<shell_contract::hosts::CommittedGeneratedPath, String> {
    match output {
        shell_contract::ipc::SavedPublication::Native(native) => {
            shell_contract::hosts::CommittedGeneratedPath::from_existing_document_save(
                native.selection().clone(),
                native.selection().binding().owner().clone(),
                participants,
                cancel,
                pending,
                additional,
                effects,
            )
        }
        shell_contract::ipc::SavedPublication::Renderer(renderer) => {
            let mut captured = participants;
            captured.extend_from_slice(renderer.participants());
            let mut controls = additional;
            controls.push(renderer.cancel().clone());
            controls.push(renderer.pending().clone());
            controls.extend_from_slice(renderer.additional());
            shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
                renderer.origin().clone(),
                renderer.destination().clone(),
                renderer.destination().clone(),
                cancel,
                pending,
                effects,
            )
            .and_then(|receipt| receipt.with_captured_participants(&captured))
            .and_then(|receipt| receipt.with_captured_cancellations(&controls))
        }
    }
}

fn owned_fake_check(
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<(), String> {
    if cancel.is_cancelled() || pending.is_cancelled() {
        return Err("Save cancelled".into());
    }
    for owner in [origin, destination] {
        if !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active())
            || owner.loss_token().is_none_or(|loss| loss.is_cancelled())
        {
            return Err("Native save owner was released".into());
        }
    }
    Ok(())
}

impl SheetsShell for FakeShell {
    fn workbook_opened(&self, view: ViewId, path: &str) {
        self.line(vec![
            "shell.workbookOpened".into(),
            view.0.into(),
            path.into(),
        ]);
        self.opened.lock().unwrap().push((view, path.to_owned()));
    }
    fn workbook_opened_owned(
        &self,
        view: ViewId,
        path: String,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        then(
            owned_fake_check(&origin, &destination, &cancel, &pending).map(|()| {
                self.workbook_opened(view, &path);
                let session = match self
                    .script
                    .get("savedPublicationError")
                    .and_then(Value::as_str)
                {
                    Some(message) => {
                        let message = message.to_owned();
                        shell_contract::hosts::SavedSessionReceipt::Pending(Box::new(move |done| {
                            done(Err(message))
                        }))
                    }
                    None => shell_contract::hosts::SavedSessionReceipt::NoPublication,
                };
                shell_contract::hosts::SavedFile { path, session }
            }),
        );
    }
    fn workbook_opened_existing_document(
        &self,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: std::path::PathBuf,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    ) {
        let extra: Vec<_> = additional.iter().collect();
        let result = revision.snapshot().and_then(|snapshot| {
            if snapshot.selection().path() != expected_path {
                return Err("The native workbook save path changed".into());
            }
            effects
                .with_native_save_revision_snapshot_effect(
                    &snapshot,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        let recipient = participants
                            .first()
                            .ok_or("The saved workbook recipient is missing")?;
                        self.workbook_opened(recipient.view, &expected_path.to_string_lossy());
                        Ok(())
                    },
                )
                .map(|()| {
                    let session = match self
                        .script
                        .get("savedPublicationError")
                        .and_then(Value::as_str)
                    {
                        Some(message) => {
                            let message = message.to_owned();
                            shell_contract::hosts::SavedSessionReceipt::Pending(Box::new(
                                move |done| done(Err(message)),
                            ))
                        }
                        None => shell_contract::hosts::SavedSessionReceipt::NoPublication,
                    };
                    shell_contract::hosts::SavedFile {
                        path: expected_path,
                        session,
                    }
                })
        });
        done(result);
    }

    fn activate_tab(&self, view: ViewId) {
        self.line(vec!["shell.activateTab".into(), view.0.into()]);
    }

    fn password_prompt_owned(
        &self,
        _parent: Option<WindowId>,
        file_name: &str,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<Box<dyn PasswordPrompt>>, String> {
        owned_fake_check(origin, destination, cancel, pending)?;
        self.line(vec!["shell.passwordPrompt".into(), file_name.into()]);
        Ok(self
            .script
            .get("password")
            .and_then(Value::as_str)
            .map(|p| Box::new(FakePrompt(p.to_owned())) as Box<dyn PasswordPrompt>))
    }

    fn startup(&self) -> Startup {
        let list = |key: &str| -> Vec<Value> {
            self.script
                .get("startup")
                .and_then(|s| s.get(key))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let pair = |entry: &Value, second: &str| {
            Some((
                ViewId(entry.get("view")?.as_u64()? as u32),
                entry.get(second)?.as_str()?.to_owned(),
            ))
        };
        Startup {
            queued: list("queued")
                .iter()
                .filter_map(|e| pair(e, "path"))
                .collect(),
            new_blank: self
                .script
                .get("startup")
                .and_then(|s| s.get("newBlank"))
                .and_then(Value::as_bool)
                == Some(true),
            untitled: list("untitled")
                .iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect(),
            mcp_writable: list("mcpWritable")
                .iter()
                .filter_map(|e| pair(e, "path"))
                .collect(),
            headless: list("headless")
                .iter()
                .filter_map(|e| {
                    let view = ViewId(e.get("view")?.as_u64()? as u32);
                    let input = e.get("input").and_then(Value::as_str).unwrap_or("");
                    Some((view, input.to_owned(), e.get("out")?.as_str()?.to_owned()))
                })
                .collect(),
            register_views: !self.script.is_null(),
            unregistered: list("unregistered")
                .iter()
                .filter_map(|v| Some(ViewId(v.as_u64()? as u32)))
                .collect(),
        }
    }

    fn now_ms(&self) -> f64 {
        #[cfg(test)]
        if let Some(now) = *self.clock_override.lock().unwrap() {
            return now;
        }
        match self.script.get("clock").and_then(Value::as_f64) {
            Some(clock) => clock,
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_millis() as f64),
        }
    }

    fn random_uuid(&self) -> String {
        let n = self.uuids.fetch_add(1, Ordering::SeqCst) + 1;
        format!("11111111-0000-4000-8000-{n:012}")
    }

    fn is_headless(&self) -> bool {
        self.script.get("headless").and_then(Value::as_bool) == Some(true)
    }

    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        self.line(vec!["shell.newDocumentDir".into(), view.0.into()]);
        self.script
            .get("newDocumentDir")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        _admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<(shell_contract::ipc::SavedPublication, NativeSavedInput)>, String> {
        owned_fake_check(origin, destination, cancel, pending)?;
        let view = destination.view;
        self.line(vec![
            "shell.writeNewDocument".into(),
            view.0.into(),
            file_name.into(),
            bytes.len().into(),
        ]);
        let scripted = self.script.get("writeNewDocument");
        if let Some(message) = scripted
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
        {
            return Err(message.to_owned());
        }
        let Some(path) = scripted
            .and_then(|v| v.get("path"))
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
        else {
            return Ok(None);
        };
        // the shell writes into the project folder, which it has made; a later
        // document of the case may land on the same scripted path
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut staged =
            crate::staged::StagedOutput::prepare(&path, bytes.to_vec(), PublicationMode::Replace)
                .map_err(|error| error.message)?;
        let commit = staged.take().map_err(|error| error.message)?;
        let result = effects.publish_owned_staged_write(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            &path,
            commit,
        );
        staged
            .conclude(result)
            .map(Some)
            .map_err(|error| error.message)
    }

    fn create_document_owned(
        &self,
        _request: IpcValue,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        _effects: shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        then: Box<dyn FnOnce(Result<CreateDocumentAnswer, String>) + Send>,
    ) {
        let result = owned_fake_check(&origin, &destination, &cancel, &pending).and_then(|()| {
            let answer = self
                .script
                .get("createDocument")
                .map(serde_to_ipc)
                .ok_or_else(|| "Document creation is unavailable in this host.".to_owned())?;
            self.line(vec!["shell.createDocument".into(), view.0.into()]);
            main_core::packages::electron_utils::atomic_write::owned::admitted(
                &admission,
                move || answer,
            )
            .map_err(|error| error.message)
        });
        then(result);
    }

    fn open_generated_path(&self, path: &str) -> bool {
        self.line(vec!["shell.openGeneratedPath".into(), path.into()]);
        false
    }
    fn open_generated_path_owned(
        &self,
        output: shell_contract::ipc::SavedPublication,
        written: Option<NativeSavedInput>,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _accepted: shell_contract::hosts::PdfReloadAccepted,
        _wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        check_written_input(&output, written.as_ref())?;
        let path = generated_path(&output)?;
        let log = self.log.clone();
        Ok(synthetic_generated(Box::new(move || {
            let extra: Vec<_> = additional.iter().collect();
            output.with_effect(
                effects.as_ref(),
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || {
                    log_open_generated(&log, &path);
                    Ok(())
                },
            )?;
            Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened)
        })))
    }

    fn export_pdf_written_owned(
        &self,
        output: shell_contract::ipc::SavedPublication,
        written: Option<NativeSavedInput>,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _accepted: shell_contract::hosts::PdfReloadAccepted,
        _wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        check_written_input(&output, written.as_ref())?;
        let path = generated_path(&output)?;
        let log = self.log.clone();
        let shell = self
            .os_shell
            .clone()
            .ok_or("Synthetic reveal host is unavailable")?;
        Ok(synthetic_generated(Box::new(move || {
            let extra: Vec<_> = additional.iter().collect();
            output.with_effect(
                effects.as_ref(),
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || {
                    log_open_generated(&log, &path);
                    shell.show_item_in_folder(std::path::Path::new(&path));
                    Ok(())
                },
            )?;
            let receipt =
                committed_receipt(&output, participants, cancel, pending, additional, effects)?;
            Ok(shell_contract::hosts::GeneratedPathOutcome::Opened(
                Arc::new(receipt),
            ))
        })))
    }

    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let result = match self.script.get("unsaved").and_then(Value::as_array) {
            Some(paths) if paths.iter().any(|p| p.as_str() == Some(path.as_str())) => {
                Err("The file has unsaved changes.".into())
            }
            _ => Ok(()),
        };
        then(result);
    }

    fn is_path_open_elsewhere(&self, path: &str) -> bool {
        self.script
            .get("openElsewhere")
            .and_then(Value::as_array)
            .is_some_and(|paths| paths.iter().any(|p| p.as_str() == Some(path)))
    }

    fn short_date_pattern(&self) -> Result<String, String> {
        Ok("m/d/yyyy".into())
    }

    fn requested_locale(&self) -> String {
        "en".into()
    }

    fn delimited_import_limit(&self) -> f64 {
        self.script
            .get("importLimit")
            .and_then(Value::as_f64)
            .unwrap_or(256.0 * 1024.0 * 1024.0)
    }

    fn app_version(&self) -> String {
        "0.0.0-test".into()
    }

    fn after(&self, ms: u64, job: Box<dyn FnOnce() + Send>) {
        self.timers.lock().unwrap().push((ms, job));
    }

    fn template_fonts_dir(&self) -> String {
        "/fonts".into()
    }
}

/// The screens and windows the capturer lists; window `n` is `window:n`
const SOURCES: [(&str, &str); 3] = [
    ("screen:0", "Screen 1"),
    ("window:1", "Window 1"),
    ("window:7", "Notes"),
];

/// `script["screenStatus"]`: the answers of the permission check in turn, then `granted`
pub struct FakeScreen {
    statuses: Mutex<Vec<String>>,
}

impl ScreenCapture for FakeScreen {
    fn media_access_status(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<String, String> {
        owned_fake_check(origin, origin, cancel, cancel)?;
        let mut statuses = self.statuses.lock().unwrap();
        Ok(if statuses.is_empty() {
            "granted".into()
        } else {
            statuses.remove(0)
        })
    }

    fn list_sources(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        exclude: Option<WindowId>,
    ) -> Result<Vec<ScreenSource>, String> {
        owned_fake_check(origin, origin, cancel, cancel)?;
        let own = exclude.map(|window| format!("window:{}", window.0));
        Ok(SOURCES
            .iter()
            .filter(|(id, _)| own.as_deref() != Some(*id))
            .map(|(id, name)| ScreenSource {
                id: (*id).into(),
                name: (*name).into(),
                thumbnail: String::new(),
            })
            .collect())
    }

    fn capture(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        id: &str,
    ) -> Result<Option<ScreenImage>, String> {
        owned_fake_check(origin, origin, cancel, cancel)?;
        Ok(SOURCES
            .iter()
            .any(|(known, _)| *known == id)
            .then(|| ScreenImage {
                png: b"\x89PNG".to_vec(),
                width: 2.0,
                height: 1.0,
            }))
    }
}

/// This app's fake hosts: they answer from `case["sheets"]` and log to the
/// shared host log when the probe inserted one
pub fn insert_fakes(hosts: &mut Hosts, case: &Value) {
    let script = case.get("sheets").cloned().unwrap_or(Value::Null);
    let log = hosts.get::<Log>().cloned();
    let engine = Arc::new(FakeEngine {
        log: log.clone(),
        script: script.clone(),
        sessions: AtomicU64::new(0),
        exit: Mutex::new(None),
        effects: Mutex::new(
            hosts
                .get::<shell_contract::hosts::OwnedEffectsHost>()
                .cloned(),
        ),
    });
    let statuses: Vec<String> = script
        .get("screenStatus")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let shell = Arc::new(FakeShell {
        log,
        script,
        os_shell: hosts.get::<shell_contract::hosts::OsShellHost>().cloned(),
        timers: Mutex::new(Vec::new()),
        opened: Mutex::new(Vec::new()),
        uuids: AtomicU64::new(0),
        #[cfg(test)]
        clock_override: Mutex::new(None),
    });
    hosts.insert::<WorkbookEngineHost>(engine.clone());
    hosts.insert::<SheetsShellHost>(shell.clone());
    hosts.insert::<ScreenCaptureHost>(Arc::new(FakeScreen {
        statuses: Mutex::new(statuses),
    }));
    hosts.insert::<Arc<FakeEngine>>(engine);
    hosts.insert::<Arc<FakeShell>>(shell);
}

/// What zod says of a request that is not an object
fn not_an_object(input: &IpcValue) -> IpcError {
    match schema::parse(&schema::strict(Vec::new()), input) {
        Err(error) => error,
        Ok(()) => engine_error("request must be an object"),
    }
}
