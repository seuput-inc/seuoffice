//! Streaming workbook save orchestration from xlsx-package-io.ts.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use shell_contract::IpcValue;
use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};
use shell_ipc_d1::value::{as_str, field};

use super::workbook_manifest::{Entry, assert_preserved};
use super::workbook_rpc::Rpc;
use super::workbook_save_planner::{EntrySource, MutationPlan, plan};
use super::workbook_temp::WorkDir;

const MAX_PATCH_ENTRY_BYTES: f64 = 500.0 * 1024.0 * 1024.0;

struct Source<'a> {
    rpc: &'a Rpc,
    path: &'a str,
    entries: &'a [Entry],
    by_name: HashMap<&'a str, &'a Entry>,
    cache: HashMap<String, Arc<String>>,
}

impl EntrySource for Source<'_> {
    fn paths(&mut self) -> Result<Vec<String>, String> {
        Ok(self
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect())
    }
    fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
        if let Some(text) = self.cache.get(path) {
            return Ok(text.clone());
        }
        let entry = self
            .by_name
            .get(path)
            .ok_or_else(|| format!("Workbook is missing {path}."))?;
        if entry.uncompressed_size > MAX_PATCH_ENTRY_BYTES {
            return Err(format!(
                "{path} is {} bytes uncompressed — too large to edit. Entries above 500MB can be preserved but not patched.",
                jscompat::number_to_string(entry.uncompressed_size)
            ));
        }
        let text = self.rpc.read_text(self.path, path).map_err(|error| {
            if error.message == format!("Workbook is missing {path}.") {
                format!("Sidecar did not extract {path}.")
            } else {
                error.message
            }
        })?;
        let text = Arc::new(text);
        self.cache.insert(path.to_owned(), text.clone());
        Ok(text)
    }
    fn can_patch(&mut self, path: &str) -> Result<bool, String> {
        Ok(self
            .by_name
            .get(path)
            .map_or(0.0, |entry| entry.uncompressed_size)
            <= MAX_PATCH_ENTRY_BYTES)
    }
    fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
        let result = self
            .rpc
            .archive(
                "scan_entries",
                json!({ "path": self.path, "entries": [path], "needle": needle }),
            )
            .map_err(|error| error.message)?;
        let result = super::workbook_schema::parse("archiveScanEntries", &result)
            .map_err(|error| error.message)?;
        let IpcValue::Array(matches) = field(&result, "matches") else {
            return Err("archive scan returned invalid matches".into());
        };
        Ok(matches.iter().any(|item| as_str(item) == Some(path)))
    }
    fn release_text(&mut self, path: &str) {
        self.cache.remove(path);
    }
    fn short_date_pattern(&mut self) -> Result<String, String> {
        let locale = crate::host::system_info::system_locale()?;
        let output = seu_host::call("locale.shortDate", &Value::String(locale))
            .map_err(|error| error.message)?;
        output
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "short date formatter returned a non-string".to_owned())
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn same_manifest(left: &[Entry], right: &[Entry]) -> bool {
    fn bits(value: f64) -> u64 {
        if value == 0.0 { 0 } else { value.to_bits() }
    }
    if left.len() != right.len() {
        return false;
    }
    let keys: HashSet<_> = left
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                bits(entry.crc32),
                bits(entry.compressed_size),
                bits(entry.uncompressed_size),
            )
        })
        .collect();
    right.iter().all(|entry| {
        keys.contains(&(
            entry.name.as_str(),
            bits(entry.crc32),
            bits(entry.compressed_size),
            bits(entry.uncompressed_size),
        ))
    })
}

fn write_contents<T: AsRef<[u8]>>(
    dir: &Path,
    prefix: &str,
    entries: &[(String, T)],
) -> EngineResult<Vec<Value>> {
    entries
        .iter()
        .enumerate()
        .map(|(index, (name, content))| {
            let path = dir.join(format!("{prefix}-{index}.bin"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    engine_error(
                        main_core::node_fs::node_error(&error, "open", &path.to_string_lossy())
                            .message,
                    )
                })?;
            file.write_all(content.as_ref()).map_err(|error| {
                engine_error(
                    main_core::node_fs::node_error(&error, "write", &path.to_string_lossy())
                        .message,
                )
            })?;
            Ok(json!({ "name": name, "contentPath": path.to_string_lossy() }))
        })
        .collect()
}

pub(super) fn write(
    rpc: &Rpc,
    source: &str,
    target: &str,
    resolved: &Value,
) -> EngineResult<IpcValue> {
    let dir = WorkDir::create("ai-excel-save-")?;
    let target_path = Path::new(target);
    let base = target_path
        .file_name()
        .ok_or_else(|| engine_error("save target has no file name"))?
        .to_string_lossy();
    let temp = Temporary(
        target_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{base}.{}.tmp", jscompat::random_uuid())),
    );
    let temp_path = temp
        .0
        .to_str()
        .ok_or_else(|| engine_error("temporary save path is not UTF-8"))?;
    let manifest = rpc.manifest(source)?;
    let mut entry_source = Source {
        rpc,
        path: source,
        entries: &manifest,
        by_name: manifest
            .iter()
            .map(|entry| (entry.name.as_str(), entry))
            .collect(),
        cache: HashMap::new(),
    };
    let plan: MutationPlan = plan(&mut entry_source, resolved).map_err(engine_error)?;
    let replacements = write_contents(dir.path(), "replace", &plan.replaced)?;
    let mut additions = write_contents(dir.path(), "add", &plan.added)?;
    additions.extend(write_contents(dir.path(), "add-bin", &plan.added_binary)?);
    let (before, after) = rpc.saved_manifests(json!({
        "sourcePath": source, "targetPath": temp_path,
        "replacements": replacements, "removals": plan.removed_entries, "additions": additions,
    }))?;
    if !same_manifest(&manifest, &before) {
        return Err(engine_error(
            "The workbook changed on disk while saving — aborted.",
        ));
    }
    let replaced = plan.replaced.iter().map(|(name, _)| name.clone()).collect();
    let added: Vec<_> = plan
        .added
        .iter()
        .map(|(name, _)| name.clone())
        .chain(plan.added_binary.iter().map(|(name, _)| name.clone()))
        .collect();
    let removed = plan.removed_entries.iter().cloned().collect();
    assert_preserved(&before, &after, &replaced, &added, &removed)?;
    super::workbook_promote::promote(temp_path, target)?;
    Ok(IpcValue::Array(
        plan.touched_entries
            .into_iter()
            .map(IpcValue::String)
            .collect(),
    ))
}
