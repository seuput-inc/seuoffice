//! What the runner writes: `<out>/e2e.jsonl`, one JSON object per step, and the side log of the
//! dialogs the app showed (`<out>/e2e-dialogs.jsonl`).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value};

pub(super) const REPORT_FILE: &str = "e2e.jsonl";
pub(super) const DIALOG_LOG_FILE: &str = "e2e-dialogs.jsonl";

/// The outcome of one step. The runner never decides pass or fail of a scenario: a checker
/// reads these lines.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StepRecord {
    /// the step's index in the scenario's `steps`, from 0
    pub(super) step: usize,
    pub(super) op: &'static str,
    /// a query's `as`
    pub(super) name: Option<String>,
    pub(super) ok: bool,
    pub(super) ms: u64,
    pub(super) value: Option<Value>,
    pub(super) error: Option<String>,
}

impl StepRecord {
    /// `{"step", "op", "as"?, "ok", "ms", "value"?, "error"?}` in that key order
    pub(super) fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("step".into(), Value::from(self.step));
        object.insert("op".into(), Value::from(self.op));
        if let Some(name) = &self.name {
            object.insert("as".into(), Value::from(name.as_str()));
        }
        object.insert("ok".into(), Value::from(self.ok));
        object.insert("ms".into(), Value::from(self.ms));
        if let Some(value) = &self.value {
            object.insert("value".into(), value.clone());
        }
        if let Some(error) = &self.error {
            object.insert("error".into(), Value::from(error.as_str()));
        }
        Value::Object(object)
    }
}

pub(super) struct Report {
    file: Mutex<File>,
}

impl Report {
    /// Creates the output folder and an empty `e2e.jsonl` in it
    pub(super) fn create(out: &Path) -> Result<Self, String> {
        fs::create_dir_all(out)
            .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
        let path = out.join(REPORT_FILE);
        let file = File::create(&path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// One line, on disk before this returns: a quit step ends the process right after
    pub(super) fn write(&self, record: &StepRecord) -> Result<(), String> {
        let mut line = record.to_json().to_string();
        line.push('\n');
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(line.as_bytes())
            .and_then(|()| file.sync_data())
            .map_err(|error| format!("cannot write {REPORT_FILE}: {error}"))
    }
}

/// Appends one JSON line to a side log of the output folder
pub(super) fn append_line(out: &Path, file: &str, value: &Value) -> Result<(), String> {
    let path = out.join(file);
    let mut handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut line = value.to_string();
    line.push('\n');
    handle
        .write_all(line.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> StepRecord {
        StepRecord {
            step: 3,
            op: "query",
            name: Some("prep-title".into()),
            ok: true,
            ms: 12,
            value: Some(json!({"title": "한글"})),
            error: None,
        }
    }

    #[test]
    fn a_record_serializes_in_the_contract_key_order() {
        assert_eq!(
            record().to_json().to_string(),
            r#"{"step":3,"op":"query","as":"prep-title","ok":true,"ms":12,"value":{"title":"한글"}}"#
        );
    }

    #[test]
    fn optional_keys_are_left_out_and_errors_are_kept() {
        let failed = StepRecord {
            step: 0,
            op: "waitReady",
            name: None,
            ok: false,
            ms: 30_000,
            value: None,
            error: Some("timed out".into()),
        };
        assert_eq!(
            failed.to_json().to_string(),
            r#"{"step":0,"op":"waitReady","ok":false,"ms":30000,"error":"timed out"}"#
        );
        // a null result is a value, not an absent one
        let null = StepRecord {
            value: Some(Value::Null),
            name: None,
            ..record()
        };
        assert_eq!(
            null.to_json().to_string(),
            r#"{"step":3,"op":"query","ok":true,"ms":12,"value":null}"#
        );
    }

    #[test]
    fn the_report_is_one_line_per_record() {
        let dir = std::env::temp_dir().join(format!("seuoffice-e2e-report-{}", std::process::id()));
        let report = Report::create(&dir).unwrap();
        report.write(&record()).unwrap();
        report.write(&record()).unwrap();
        let text = fs::read_to_string(dir.join(REPORT_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["step"], 3);
            assert_eq!(parsed["op"], "query");
        }
        append_line(&dir, DIALOG_LOG_FILE, &json!({"dialog": "save"})).unwrap();
        assert!(dir.join(DIALOG_LOG_FILE).is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
}
