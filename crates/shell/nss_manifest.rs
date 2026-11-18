use std::collections::{BTreeMap, BTreeSet};
use serde_json::Value;

#[derive(Debug)]
pub struct Artifact {
    pub id: String,
    pub fd: i32,
    pub sha256: String,
    pub size: u64,
    pub kind: String,
    pub soname: Option<String>,
    pub needed: Vec<i32>,
    pub check_for: Option<String>,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
        .ok_or_else(|| format!("NSS manifest: missing {key}"))
}
fn hex_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn parse(bytes: &[u8], target: &str) -> Result<Vec<Artifact>, String> {
    let manifest: Value = serde_json::from_slice(bytes).map_err(|e| format!("NSS manifest: {e}"))?;
    let contract: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/nss-helper/contract.json")))
        .map_err(|e| format!("NSS contract: {e}"))?;
    let contract_entries = contract["artifacts"].as_array().ok_or("NSS contract: missing artifacts")?;
    if manifest.get("format").and_then(Value::as_u64) != Some(1)
        || text(&manifest, "arch")? != target.split('-').next().unwrap_or("")
        || text(&manifest["abi"], "target")? != target {
        return Err("NSS manifest: incompatible format or target".into());
    }
    for key in ["source", "toolchain", "abi"] {
        let object = manifest.get(key).and_then(Value::as_object).filter(|v| !v.is_empty())
            .ok_or_else(|| format!("NSS manifest: missing {key} provenance"))?;
        if object.values().any(|v| v.as_str().is_none_or(str::is_empty)) {
            return Err(format!("NSS manifest: invalid {key} provenance"));
        }
    }
    let source = &manifest["source"];
    for key in ["nss", "nspr", "sqlite", "glibc"] {
        let pin = &contract["sources"][key];
        let expected = if key == "nspr" {
            format!("{}|included-in-{}|sha256:{}", text(pin, "version")?, text(pin, "contained_in")?, text(pin, "sha256")?)
        } else {
            format!("{}|{}|sha256:{}", text(pin, "filename")?, text(pin, "url")?, text(pin, "sha256")?)
        };
        if text(source, key)? != expected { return Err(format!("NSS manifest: unpinned {key} source")); }
    }
    if !hex_digest(text(source, "patch_sha256")?) { return Err("NSS manifest: invalid patch digest".into()); }
    let entries = manifest.get("artifacts").and_then(Value::as_array).ok_or("NSS manifest: missing artifacts")?;
    if entries.len() != contract_entries.len() { return Err("NSS manifest: incomplete artifact set".into()); }
    let mut artifacts = Vec::with_capacity(entries.len());
    let mut ids = BTreeSet::new();
    let mut fds = BTreeSet::new();
    for entry in entries {
        let id = text(entry, "id")?;
        if id == "." || id == ".." || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) || !ids.insert(id.to_owned()) {
            return Err("NSS manifest: invalid or duplicate artifact name".into());
        }
        let fd = entry.get("fd").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).filter(|n| (200..300).contains(n)).ok_or("NSS manifest: invalid descriptor")?;
        if !fds.insert(fd) { return Err("NSS manifest: duplicate descriptor".into()); }
        let expected = contract_entries.iter().find(|item| item["fd"].as_i64() == Some(i64::from(fd)))
            .ok_or("NSS manifest: descriptor absent from contract")?;
        if text(expected, "id")? != id { return Err("NSS manifest: unexpected artifact identity".into()); }
        let sha256 = text(entry, "sha256")?;
        if !hex_digest(sha256) { return Err("NSS manifest: invalid artifact digest".into()); }
        let size = entry.get("size").and_then(Value::as_u64).filter(|n| *n > 0).ok_or("NSS manifest: invalid artifact size")?;
        let kind = text(entry, "kind")?;
        if text(expected, "kind")? != kind { return Err("NSS manifest: artifact role does not match descriptor".into()); }
        let soname = entry.get("soname").and_then(Value::as_str).map(str::to_owned);
        if (kind == "library" && soname.is_none()) || (kind != "library" && kind != "loader" && soname.is_some()) { return Err("NSS manifest: invalid SONAME".into()); }
        let needed = entry.get("needed").and_then(Value::as_array).ok_or("NSS manifest: missing dependency edges")?.iter().map(|n| n.as_i64().and_then(|v| i32::try_from(v).ok()).ok_or_else(|| "NSS manifest: invalid dependency".to_owned())).collect::<Result<Vec<_>, _>>()?;
        let check_for = entry.get("check_for").and_then(Value::as_str).map(str::to_owned);
        if expected.get("check_for").and_then(Value::as_str) != check_for.as_deref() {
            return Err("NSS manifest: integrity target differs from contract".into());
        }
        if (kind == "integrity") != check_for.is_some() { return Err("NSS manifest: invalid integrity target".into()); }
        artifacts.push(Artifact { id: id.into(), fd, sha256: sha256.into(), size, kind: kind.into(), soname, needed, check_for });
    }
    let by_fd: BTreeMap<_, _> = artifacts.iter().map(|a| (a.fd, a)).collect();
    for artifact in &artifacts {
        let mut edges = BTreeSet::new();
        for fd in &artifact.needed {
            if !edges.insert(*fd) || !by_fd.get(fd).is_some_and(|a| a.kind == "library" || a.kind == "loader") { return Err("NSS manifest: invalid dependency closure".into()); }
        }
        if let Some(target) = &artifact.check_for {
            if !artifacts.iter().any(|a| &a.id == target && a.kind == "library") { return Err("NSS manifest: invalid integrity mapping".into()); }
        }
    }
    artifacts.sort_by_key(|a| a.fd);
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_unpinned_or_incomplete_provenance_before_opening_artifacts() {
        let incomplete = json!({
            "format": 1,
            "arch": "x86_64",
            "abi": { "target": "x86_64-unknown-linux-gnu" },
            "source": { "nss": "untrusted" },
            "toolchain": { "cc": "cc" },
            "artifacts": []
        });
        let bytes = serde_json::to_vec(&incomplete).unwrap();
        assert!(parse(&bytes, "aarch64-unknown-linux-gnu").is_err());
        assert!(parse(&bytes, "x86_64-unknown-linux-gnu").is_err());
    }
}
