//! Conflict-safe persistence for converted Markdown and its owned assets.
use super::markdown::{AssetRecord, MarkdownConversion};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::pdf::tools::JobFailure;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

const CURRENT: &str = ".seuoffice-assets.json";
const LEGACY: &str = ".genoffice-assets.json";
fn cancelled(token: &CancelToken) -> Result<(), JobFailure> {
    if token.is_cancelled() {
        Err(JobFailure::new("cancelled", None))
    } else {
        Ok(())
    }
}
fn io(error: std::io::Error, path: &Path, action: &str) -> JobFailure {
    let path = path.to_string_lossy();
    JobFailure::new(
        "failed",
        Some(&main_core::node_fs::node_error(&error, action, &path).message),
    )
}
fn conflict(message: String) -> JobFailure {
    JobFailure::new("asset-conflict", Some(&message))
}
fn safe(name: &str) -> bool {
    !name.is_empty()
        && name.encode_utf16().count() <= 180
        && !matches!(name, "." | ".." | CURRENT | LEGACY)
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}
fn record(item: &Value) -> Result<AssetRecord, ()> {
    let name = item.get("name").and_then(Value::as_str).ok_or(())?;
    let sha = item.get("sha256").and_then(Value::as_str).ok_or(())?;
    if !safe(name)
        || sha.len() != 64
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(());
    }
    Ok(AssetRecord {
        name: name.to_owned(),
        sha256: sha.to_owned(),
    })
}
#[derive(Default)]
struct Manifest {
    files: Vec<AssetRecord>,
    documents: serde_json::Map<String, Value>,
    pending: serde_json::Map<String, Value>,
}
fn references(value: Option<&Value>) -> Result<serde_json::Map<String, Value>, ()> {
    let Some(value) = value else {
        return Ok(serde_json::Map::new());
    };
    let object = value.as_object().ok_or(())?;
    let mut result = serde_json::Map::new();
    for (key, value) in object {
        if !safe(key) {
            return Err(());
        }
        let mut names: Vec<String> = value
            .as_array()
            .ok_or(())?
            .iter()
            .map(|v| v.as_str().filter(|s| safe(s)).map(str::to_owned).ok_or(()))
            .collect::<Result<_, _>>()?;
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        names.dedup();
        result.insert(
            key.clone(),
            Value::Array(names.into_iter().map(Value::String).collect()),
        );
    }
    Ok(result)
}
fn parse(text: Option<&str>) -> Result<Manifest, ()> {
    let Some(text) = text.filter(|s| !s.trim_matches(jscompat::is_js_space).is_empty()) else {
        return Ok(Manifest::default());
    };
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(());
    }
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for item in value.get("files").and_then(Value::as_array).ok_or(())? {
        let item = record(item)?;
        if !seen.insert(item.name.clone()) {
            return Err(());
        }
        files.push(item);
    }
    Ok(Manifest {
        files,
        documents: references(value.get("documents"))?,
        pending: references(value.get("pending"))?,
    })
}
fn merge(mut manifest: Manifest, document: &str, records: &[AssetRecord]) -> Result<String, ()> {
    if !safe(document) {
        return Err(());
    }
    let mut owned: HashMap<String, String> = manifest
        .files
        .iter()
        .map(|r| (r.name.clone(), r.sha256.clone()))
        .collect();
    for item in records {
        let checked = record(&json!({"name":item.name,"sha256":item.sha256}))?;
        if let Some(old) = owned.get(&checked.name) {
            if old != &checked.sha256 {
                return Err(());
            }
        } else {
            owned.insert(checked.name.clone(), checked.sha256.clone());
            manifest.files.push(checked);
        }
    }
    manifest.files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut names: Vec<_> = records.iter().map(|r| r.name.clone()).collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    names.dedup();
    if names.is_empty() {
        manifest.documents.shift_remove(document);
    } else {
        manifest.documents.insert(
            document.to_owned(),
            Value::Array(names.into_iter().map(Value::String).collect()),
        );
    }
    let files: Vec<_> = manifest
        .files
        .iter()
        .map(|r| json!({"name":r.name,"sha256":r.sha256}))
        .collect();
    serde_json::to_string_pretty(&json!({"version":1,"files":files,"documents":manifest.documents,"pending":manifest.pending})).map(|s|format!("{s}\n")).map_err(|_| ())
}
fn optional(path: &Path) -> Result<Option<Vec<u8>>, JobFailure> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e, path, "open")),
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn atomic(path: &Path, bytes: &[u8], cancel: &CancelToken) -> Result<(), JobFailure> {
    let path = path
        .to_str()
        .ok_or_else(|| JobFailure::new("failed", Some("output path is not UTF-8")))?;
    super::atomic::write(path, bytes, cancel)
}
pub(super) fn write(
    out_path: &str,
    conversion: &MarkdownConversion,
    cancel: &CancelToken,
) -> Result<(usize, usize), JobFailure> {
    cancelled(cancel)?;
    let output = Path::new(out_path);
    let dir = output.parent().unwrap_or_else(|| Path::new("."));
    let assets = dir.join("assets");
    let name = if assets.join(CURRENT).exists() {
        CURRENT
    } else {
        LEGACY
    };
    let manifest_path = assets.join(name);
    let old = optional(&manifest_path)?;
    let old_text = old.as_deref().map(String::from_utf8_lossy);
    let manifest =
        parse(old_text.as_deref()).map_err(|_| conflict(format!("assets/{name} is unreadable")))?;
    let owned: HashMap<_, _> = manifest
        .files
        .iter()
        .map(|r| (r.name.as_str(), r.sha256.as_str()))
        .collect();
    let entries: HashMap<_, _> = conversion
        .manifest_entries
        .iter()
        .map(|r| (r.name.as_str(), r))
        .collect();
    let mut to_write = Vec::new();
    let mut records = Vec::new();
    let mut reused = 0;
    for asset in &conversion.assets {
        if !safe(&asset.name) {
            return Err(conflict(format!(
                "assets/{} has an unsafe name",
                asset.name
            )));
        }
        let item = entries
            .get(asset.name.as_str())
            .map(|r| (*r).clone())
            .unwrap_or_else(|| AssetRecord {
                name: asset.name.clone(),
                sha256: asset.sha256.clone(),
            });
        match optional(&assets.join(&asset.name))? {
            None => {
                to_write.push(asset);
                records.push(item);
            }
            Some(data) if hash(&data) == asset.sha256 => {
                reused += 1;
                if owned.get(asset.name.as_str()) == Some(&asset.sha256.as_str()) {
                    records.push(item);
                }
            }
            Some(_) => {
                return Err(conflict(format!(
                    "assets/{} already holds another file",
                    asset.name
                )));
            }
        }
    }
    let document = output
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| JobFailure::new("failed", Some("invalid Markdown output path")))?;
    let next = if !records.is_empty() || old.is_some() {
        Some(
            merge(manifest, document, &records)
                .map_err(|_| conflict(format!("assets/{name} cannot take these pictures")))?,
        )
    } else {
        None
    };
    if !to_write.is_empty() {
        cancelled(cancel)?;
        fs::create_dir_all(&assets).map_err(|e| io(e, &assets, "mkdir"))?;
    }
    for asset in &to_write {
        cancelled(cancel)?;
        let path = assets.join(&asset.name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&asset.data) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(io(e, &path, "write"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if optional(&path)?.as_deref().map(hash).as_deref() != Some(asset.sha256.as_str()) {
                    return Err(conflict(format!(
                        "assets/{} already holds another file",
                        asset.name
                    )));
                }
            }
            Err(e) => return Err(io(e, &path, "open")),
        }
    }
    if let Some(next) = next
        && old_text.as_deref() != Some(next.as_str())
    {
        atomic(&manifest_path, next.as_bytes(), cancel)?;
    }
    atomic(output, conversion.markdown.as_bytes(), cancel)?;
    Ok((to_write.len(), reused))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_unsafe_or_duplicate_manifest_entries() {
        assert!(parse(Some(r#"{"version":1,"files":[{"name":"../x","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#)).is_err());
        assert!(
            parse(Some(
                r#"{"version":1,"files":[],"documents":{"a.md":["../x"]}}"#
            ))
            .is_err()
        );
    }
    #[test]
    fn preserves_other_documents_and_rejects_conflicting_owned_hash() {
        let hash = "a".repeat(64);
        let old = format!(
            r#"{{"version":1,"files":[{{"name":"x.png","sha256":"{hash}"}}],"documents":{{"old.md":["x.png"]}},"pending":{{"draft":["x.png"]}}}}"#
        );
        let next = merge(
            parse(Some(&old)).unwrap(),
            "new.md",
            &[AssetRecord {
                name: "x.png".into(),
                sha256: hash.clone(),
            }],
        )
        .unwrap();
        let value: Value = serde_json::from_str(&next).unwrap();
        assert_eq!(value["documents"]["old.md"], json!(["x.png"]));
        assert_eq!(value["documents"]["new.md"], json!(["x.png"]));
        assert_eq!(value["pending"]["draft"], json!(["x.png"]));
        assert!(
            merge(
                parse(Some(&old)).unwrap(),
                "new.md",
                &[AssetRecord {
                    name: "x.png".into(),
                    sha256: "b".repeat(64)
                }]
            )
            .is_err()
        );
    }
}
