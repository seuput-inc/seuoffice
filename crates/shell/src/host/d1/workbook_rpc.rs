//! Workbook operations backed by the native XLSX sidecar.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use shell_contract::IpcValue;
use shell_ipc_d1::sheets::engine::{Converted, EngineResult, OdsConversion, OpenedSheet, OpenedWorkbook, RecalcCell, RecalcEdit, RecalcRead, engine_error};
use shell_ipc_d1::value::{as_bool, as_num, as_str, field, from_json, to_json};

use super::{sidecar::{Client, Request}, workbook_manifest::Entry, workbook_schema, workbook_temp::WorkDir};

pub(super) struct Rpc(pub Arc<Client>);

fn ipc(value: &Value) -> IpcValue {
    from_json(&jscompat::json_value_from_serde(value))
}

fn json_value(value: &IpcValue) -> EngineResult<Value> {
    to_json(value).map(|v| jscompat::json_value_to_serde(&v))
        .ok_or_else(|| engine_error("workbook request cannot be serialized as JSON"))
}

fn object(value: &IpcValue) -> EngineResult<Map<String, Value>> {
    json_value(value)?.as_object().cloned().ok_or_else(|| engine_error("workbook request is not an object"))
}

fn required<'a>(value: &'a IpcValue, key: &str) -> EngineResult<&'a str> {
    as_str(field(value, key)).ok_or_else(|| engine_error(format!("workbook result has invalid {key}")))
}

fn entries(value: &IpcValue, key: &str) -> EngineResult<Vec<Entry>> {
    let IpcValue::Array(items) = field(value, key) else { return Err(engine_error(format!("archive result has invalid {key}"))); };
    items.iter().map(|item| {
        let number = |key| as_num(field(item, key)).ok_or_else(|| engine_error(format!("archive entry has invalid {key}")));
        Ok(Entry {
            name: required(item, "name")?.to_owned(),
            crc32: number("crc32")?,
            compressed_size: number("compressedSize")?,
            uncompressed_size: number("uncompressedSize")?,
        })
    }).collect()
}

impl Rpc {
    fn call(&self, request: Request) -> EngineResult<IpcValue> {
        self.0.request(request).map_err(engine_error).map(|value| value.as_ref().map(ipc).unwrap_or(IpcValue::Undefined))
    }

    pub fn open(&self, path: &str, locale: &str, short_date: &str, timeout_ms: Option<f64>) -> EngineResult<OpenedWorkbook> {
        let value = self.call(Request::Open { path: path.into(), locale: locale.into(), short_date: short_date.into(), timeout_ms })?;
        Self::parse_open(value)
    }

    pub fn open_native(&self, path: &str, locale: &str, short_date: &str, timeout_ms: Option<f64>,
        document: &shell_contract::ipc::NativeInvocationDocument, cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken) -> EngineResult<OpenedWorkbook> {
        let value = self.0.request_native(Request::Open { path: path.into(), locale: locale.into(), short_date: short_date.into(), timeout_ms }, document, cancel, pending)
            .map_err(engine_error)?.as_ref().map(ipc).unwrap_or(IpcValue::Undefined);
        Self::parse_open(value)
    }

    pub fn open_saved(&self, path: &str, locale: &str, short_date: &str, timeout_ms: Option<f64>,
        document: &shell_contract::ipc::NativeSavedDocument, actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken) -> EngineResult<OpenedWorkbook> {
        let value = self.0.request_saved(Request::Open { path: path.into(), locale: locale.into(), short_date: short_date.into(), timeout_ms }, document, actual, actual_cancel)
            .map_err(engine_error)?.as_ref().map(ipc).unwrap_or(IpcValue::Undefined);
        Self::parse_open(value)
    }

    fn parse_open(value: IpcValue) -> EngineResult<OpenedWorkbook> {
        let value = workbook_schema::parse("open", &value)?;
        let session_id = required(&value, "sessionId")?.to_owned();
        let IpcValue::Array(items) = field(&value, "sheets") else { return Err(engine_error("workbook result has invalid sheets")); };
        let sheets = items.iter().map(|sheet| Ok(OpenedSheet {
            id: required(sheet, "id")?.to_owned(),
            name: required(sheet, "name")?.to_owned(),
            source_xml_bytes: as_num(field(sheet, "sourceXmlBytes")),
        })).collect::<EngineResult<Vec<_>>>()?;
        Ok(OpenedWorkbook { value, session_id, sheets })
    }

    pub fn close(&self, session_id: &str) -> EngineResult<()> {
        self.call(Request::Close(session_id.to_owned())).map(|_| ())
    }

    pub fn read(&self, command: &str, schema: &str, request: &IpcValue) -> EngineResult<IpcValue> {
        let result = self.call(Request::Read { command: command.into(), input: object(request)? })?;
        workbook_schema::parse(schema, &result)
    }

    pub fn archive(&self, command: &str, input: Value) -> EngineResult<IpcValue> {
        let input = input.as_object().cloned().ok_or_else(|| engine_error("workbook archive request is not an object"))?;
        self.call(Request::Archive { command: command.into(), input })
    }

    pub fn manifest(&self, path: &str) -> EngineResult<Vec<Entry>> {
        let result = self.call(Request::ArchiveManifest(path.to_owned()))?;
        entries(&workbook_schema::parse("archiveManifest", &result)?, "entries")
    }

    pub fn names(&self, path: &str) -> EngineResult<Vec<String>> {
        let result = self.call(Request::ArchiveManifest(path.to_owned()))?;
        let result = workbook_schema::parse("archiveNames", &result)?;
        let IpcValue::Array(items) = field(&result, "entries") else { return Err(engine_error("archive result has invalid entries")); };
        items.iter().map(|item| required(item, "name").map(str::to_owned)).collect()
    }

    pub fn read_text(&self, path: &str, name: &str) -> EngineResult<String> {
        let directory = WorkDir::create("ai-excel-read-")?;
        let output_dir = directory.path().to_str().ok_or_else(|| engine_error("archive staging path is not UTF-8"))?;
        let result = self.archive("read_entries", json!({ "path": path, "entries": [name], "outputDir": output_dir }))?;
        let result = workbook_schema::parse("archiveReadEntries", &result)?;
        let IpcValue::Array(items) = field(&result, "entries") else { return Err(engine_error("archive result has invalid entries")); };
        let file = items.first().and_then(|item| as_str(field(item, "path"))).filter(|path| !path.is_empty())
            .ok_or_else(|| engine_error(format!("Workbook is missing {name}.")))?;
        let bytes = std::fs::read(file).map_err(|error| engine_error(main_core::node_fs::node_error(&error, "open", file).message))?;
        let text = String::from_utf8_lossy(&bytes);
        super::workbook_save_planner::normalize_ooxml_part_prefix(&text).map_err(engine_error)
    }

    pub fn saved_manifests(&self, request: Value) -> EngineResult<(Vec<Entry>, Vec<Entry>)> {
        let result = self.archive("save_archive", request)?;
        let result = workbook_schema::parse("archiveSave", &result)?;
        Ok((entries(&result, "beforeEntries")?, entries(&result, "afterEntries")?))
    }

    pub fn external_links(&self, command: &str, path: &str, base_dir: &str, update: bool) -> EngineResult<IpcValue> {
        let mut input = json!({ "path": path, "baseDir": base_dir });
        if update { input["allowRemoteSources"] = Value::Bool(true); }
        let result = self.archive(command, input)?;
        workbook_schema::parse(if update { "externalLinksUpdate" } else { "externalLinks" }, &result)
    }

    pub fn convert_workbook(&self, path: &str, target: &str) -> EngineResult<Converted> {
        let input = Map::from_iter([("path".into(), json!(path)), ("targetPath".into(), json!(target))]);
        let result = self.call(Request::ConvertWorkbook(input))?;
        Ok(Converted {
            sheets: as_num(field(&result, "sheets")).unwrap_or(0.0),
            formulas_dropped: as_num(field(&result, "formulasDropped")).unwrap_or(0.0),
        })
    }

    pub fn convert_to_ods(&self, source: &str, target: &str) -> EngineResult<OdsConversion> {
        let input = Map::from_iter([("path".into(), json!(source)), ("targetPath".into(), json!(target))]);
        let value = self.call(Request::ConvertToOds(input))?;
        let dropped_value = field(&value, "dropped");
        let dropped = ["macros", "charts", "images", "shapes", "conditionalFormats", "validations", "pivots", "slicers", "sparklines", "comments"]
            .into_iter().map(|name| (name, as_num(field(dropped_value, name)).unwrap_or(0.0))).collect();
        let formulas_dropped = as_num(field(&value, "formulasDropped")).unwrap_or(0.0);
        Ok(OdsConversion { value, formulas_dropped, dropped })
    }

    pub fn recalc(&self, path: &str, edits: &[RecalcEdit], reads: &[RecalcRead]) -> EngineResult<Vec<RecalcCell>> {
        let edits: Vec<_> = edits.iter().map(|edit| json!({ "sheet": edit.sheet, "row": edit.row, "column": edit.column, "input": edit.input })).collect();
        let reads: Vec<_> = reads.iter().map(|read| Ok(json!({ "sheet": read.sheet, "range": json_value(&read.range)? }))).collect::<EngineResult<_>>()?;
        let result = self.archive("recalc_cells", json!({ "path": path, "edits": edits, "reads": reads }))?;
        let result = workbook_schema::parse("sidecarRecalc", &result)?;
        let IpcValue::Array(cells) = field(&result, "cells") else { return Err(engine_error("workbook recalc result has invalid cells")); };
        cells.iter().map(|cell| Ok(RecalcCell {
            sheet: required(cell, "sheet")?.to_owned(),
            row: as_num(field(cell, "row")).ok_or_else(|| engine_error("invalid recalculation row"))?,
            column: as_num(field(cell, "column")).ok_or_else(|| engine_error("invalid recalculation column"))?,
            formatted: required(cell, "formatted")?.to_owned(),
            number: as_num(field(cell, "number")),
            is_error: as_bool(field(cell, "isError")).unwrap_or(false),
            is_formula: as_bool(field(cell, "isFormula")).ok_or_else(|| engine_error("invalid recalculation formula flag"))?,
        })).collect()
    }
}
