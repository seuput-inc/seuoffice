use crate::nss_manifest;
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};
fn validate_elf(path: &Path, artifact: &nss_manifest::Artifact, target: &str) -> Result<(), String> {
    if artifact.kind == "integrity" {
        if !artifact.needed.is_empty() { return Err(format!("{}: integrity file has dependencies", artifact.id)); }
        return Ok(());
    }
    let machine = match target {
        "x86_64-unknown-linux-gnu" => "Advanced Micro Devices X86-64",
        "aarch64-unknown-linux-gnu" => "AArch64",
        _ => return Err(format!("unsupported NSS target {target}")),
    };
    let header = Command::new("readelf").env("LC_ALL", "C").arg("-hW").arg(path).output().map_err(|e| format!("readelf: {e}"))?;
    let header_text = std::str::from_utf8(&header.stdout).map_err(|e| e.to_string())?;
    if !header.status.success() || !header_text.lines().any(|line| line.trim().strip_prefix("Class:").is_some_and(|v| v.trim() == "ELF64"))
        || !header_text.lines().any(|line| line.trim().strip_prefix("Machine:").is_some_and(|v| v.trim() == machine)) {
        return Err(format!("{}: incompatible ELF architecture", artifact.id));
    }
    let output = Command::new("readelf").env("LC_ALL", "C").arg("-dW").arg(path).output().map_err(|e| format!("readelf: {e}"))?;
    if !output.status.success() { return Err(format!("{}: readelf failed", artifact.id)); }
    let text = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let mut needed = Vec::new();
    let mut soname = None;
    for line in text.lines() {
        if ["(RPATH)", "(RUNPATH)", "(AUDIT)", "(DEPAUDIT)", "(FILTER)", "(AUXILIARY)"].iter().any(|tag| line.contains(tag)) {
            return Err(format!("{}: forbidden dynamic loader directive", artifact.id));
        }
        if line.contains("(NEEDED)") || line.contains("(SONAME)") {
            let value = line.split_once('[').and_then(|(_, tail)| tail.split_once(']')).map(|(v, _)| v).ok_or("invalid readelf output")?;
            if line.contains("(SONAME)") { soname = Some(value.to_owned()); }
            else {
                let fd = value.strip_prefix("/proc/self/fd/").and_then(|v| v.parse::<i32>().ok()).ok_or_else(|| format!("{}: non-descriptor dependency {value}", artifact.id))?;
                needed.push(fd);
            }
        }
    }
    needed.sort_unstable();
    let mut declared = artifact.needed.clone();
    declared.sort_unstable();
    if needed != declared || soname != artifact.soname { return Err(format!("{}: ELF identity or dependency mismatch", artifact.id)); }
    Ok(())
}
pub(crate) fn embed_nss(dir: &Path) -> Result<(), String> {
    let manifest_path = dir.join("manifest.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest = fs::read(&manifest_path).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let target = env::var("TARGET").map_err(|e| e.to_string())?;
    let artifacts = nss_manifest::parse(&manifest, &target)?;
    let metadata: serde_json::Value = serde_json::from_slice(&manifest).map_err(|e| e.to_string())?;
    for (key, relative) in [
        ("patch_sha256", "../../tools/nss-helper/patch_nss.py"),
        ("helper_sha256", "src/host/d2/pdf/crypto/nss_export_helper.c"),
        ("producer_sha256", "../../tools/nss-helper/produce.py"),
        ("contract_sha256", "../../tools/nss-helper/contract.json"),
    ] {
        println!("cargo:rerun-if-changed={relative}");
        let bytes = fs::read(relative).map_err(|e| format!("{relative}: {e}"))?;
        let expected = metadata["source"][key].as_str().ok_or_else(|| format!("NSS manifest: missing {key}"))?;
        if format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(format!("NSS manifest: {key} does not match checked-in source"));
        }
    }
    let notices = metadata["notices"].as_array().ok_or("NSS manifest: missing notices")?;
    let required = [
        ("nss", "licenses/nss.txt"),
        ("nspr", "licenses/nspr.txt"),
        ("glibc-copying", "licenses/glibc-copying.txt"),
        ("glibc-lgpl", "licenses/glibc-lgpl.txt"),
        ("glibc-notices", "licenses/glibc-notices.txt"),
        ("sqlite", "licenses/sqlite.txt"),
    ];
    if notices.len() != required.len() { return Err("NSS manifest: incomplete notices".into()); }
    for (id, relative) in required {
        let matching: Vec<_> = notices.iter().filter(|notice| notice["id"].as_str() == Some(id)).collect();
        if matching.len() != 1 || matching[0]["path"].as_str() != Some(relative)
            || !matching[0]["url"].as_str().is_some_and(|url| url.starts_with("https://")) {
            return Err(format!("NSS manifest: invalid notice {id}"));
        }
        let path = dir.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if bytes.is_empty() || matching[0]["sha256"].as_str() != Some(format!("{:x}", Sha256::digest(&bytes)).as_str()) {
            return Err(format!("NSS manifest: notice {id} digest mismatch"));
        }
    }
    for artifact in &artifacts {
        let path = dir.join(&artifact.id);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if bytes.len() as u64 != artifact.size || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(format!("{}: size or digest mismatch", artifact.id));
        }
        validate_elf(&path, artifact, &target)?;
    }
    let out = Path::new(&env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?).join("nss-manifest.json");
    fs::write(out, manifest).map_err(|e| e.to_string())?;
    println!("cargo:rustc-env=SEUOFFICE_NSS_TARGET={target}");
    println!("cargo:rustc-cfg=seuoffice_nss_bundle");
    Ok(())
}
