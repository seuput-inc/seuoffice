//! Extraction of user skill archives into the caller's staging directory.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use ooxml_pkg::{ZipLimits, assert_zip_inflates_within_limits};
use ooxml_pkg::jszip::{self, LoadedEntry};

use super::user_skill_md::MAX_USER_SKILL_BYTES;
use super::user_skills_import::{SkillImportError, Thrown};

fn import_error(kind: &'static str, message: impl Into<String>) -> Thrown {
    SkillImportError::new(kind, message).into()
}

fn zip_error(message: String) -> Thrown {
    let kind = if message.starts_with("zip rejected:")
        && (message.contains("limit") || message.contains("exceeds"))
    {
        "too-large"
    } else {
        "unsafe"
    };
    import_error(kind, message)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathProblem {
    Unsafe,
    Unportable,
}

fn path_problem(name: &str) -> Option<PathProblem> {
    let path = name.strip_suffix('/').unwrap_or(name);
    let bytes = path.as_bytes();
    if path.is_empty()
        || bytes.iter().any(|b| *b < 32 || *b == 127 || *b == b'\\')
        || path.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Some(PathProblem::Unsafe);
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
        return Some(PathProblem::Unsafe);
    }
    for part in parts {
        let stem = part.split('.').next().unwrap_or("");
        let device = stem.eq_ignore_ascii_case("con")
            || stem.eq_ignore_ascii_case("prn")
            || stem.eq_ignore_ascii_case("aux")
            || stem.eq_ignore_ascii_case("nul")
            || (stem.len() == 4
                && (stem.as_bytes()[..3].eq_ignore_ascii_case(b"com")
                    || stem.as_bytes()[..3].eq_ignore_ascii_case(b"lpt"))
                && stem.as_bytes()[3].is_ascii_digit());
        if device || part.ends_with('.') || part.ends_with(' ')
            || part.bytes().any(|b| b"<>:\"|?*".contains(&b))
        {
            return Some(PathProblem::Unportable);
        }
    }
    None
}

fn root_license(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["license", "licence", "copying", "unlicense"].iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with('.') || rest.starts_with('-')
        })
    })
}

fn write_error(entry: &LoadedEntry, error: io::Error) -> Thrown {
    let collision = matches!(error.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory);
    if collision {
        import_error("unsafe", format!("Two archive entries land on one path: {}", entry.name))
    } else {
        import_error("failed", format!("Could not write {}", entry.name))
    }
}

/// Extract selected archive entries. The caller removes the staging directory on failure.
pub fn extract(
    bytes: &[u8],
    dest: &str,
    strip_top: bool,
    sub_path: &str,
    include_root_license: bool,
) -> Result<(), Thrown> {
    let limits = ZipLimits {
        max_parts: 20_000,
        max_part_bytes: MAX_USER_SKILL_BYTES as f64,
        max_total_bytes: (4 * MAX_USER_SKILL_BYTES) as f64,
    };
    assert_zip_inflates_within_limits(bytes, &limits).map_err(zip_error)?;
    let loaded = jszip::load(bytes, false).map_err(zip_error)?;

    // JSZip keeps one object per resolved name, replacing an earlier duplicate.
    let mut entries: Vec<LoadedEntry> = Vec::new();
    let mut indices = HashMap::new();
    for entry in loaded {
        let raw = &entry.unsafe_original_name;
        if path_problem(raw) == Some(PathProblem::Unsafe) || raw != &entry.name {
            return Err(import_error("unsafe", format!("The archive holds an unsafe path: {raw}")));
        }
        if let Some(index) = indices.get(&entry.name).copied() {
            entries[index] = entry;
        } else {
            indices.insert(entry.name.clone(), entries.len());
            entries.push(entry);
        }
    }
    let top = if strip_top && !entries.is_empty() {
        let first = entries[0].name.trim_end_matches('/').split('/').next().unwrap_or("");
        entries.iter().all(|entry| {
            let name = entry.name.trim_end_matches('/');
            let mut parts = name.split('/');
            parts.next() == Some(first) && (parts.next().is_some() || entry.is_dir)
        }).then(|| first.to_owned())
    } else {
        None
    };
    let sub_parts: Vec<_> = if sub_path.is_empty() { Vec::new() } else { sub_path.split('/').collect() };
    let mut taken = Vec::new();
    for entry in &entries {
        let name = entry.name.strip_suffix('/').unwrap_or(&entry.name);
        let mut parts: Vec<_> = name.split('/').collect();
        if top.is_some() { parts.remove(0); }
        if parts.is_empty() || parts[0] == "__MACOSX" { continue; }
        let in_sub = parts.starts_with(&sub_parts) && (parts.len() > sub_parts.len() || entry.is_dir);
        let license = include_root_license && !entry.is_dir && parts.len() == 1 && root_license(parts[0]);
        if !in_sub && !license { continue; }
        if path_problem(&entry.unsafe_original_name).is_some() {
            return Err(import_error("unsafe", format!("The archive holds a name Windows cannot use: {}", entry.name)));
        }
        if entry.unix_permissions.is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err(import_error("unsafe", format!("The archive holds a link: {}", entry.name)));
        }
        taken.push((entry, parts));
    }
    let mut total = 0_u64;
    for (entry, parts) in taken {
        let mut target = Path::new(dest).to_path_buf();
        for part in parts { target.push(part); }
        if entry.is_dir {
            fs::create_dir_all(&target).map_err(|e| write_error(entry, e))?;
            continue;
        }
        let content = entry.content.as_ref().map_err(|_| import_error("failed", format!("Could not write {}", entry.name)))?;
        total += content.len() as u64;
        if total > 4 * MAX_USER_SKILL_BYTES {
            return Err(import_error("too-large", "The archive is too large"));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| write_error(entry, e))?;
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(&target)
            .map_err(|e| write_error(entry, e))?;
        file.write_all(content).map_err(|e| write_error(entry, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooxml_pkg::write::{Record, RecordData, generate_records};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "skill-archive-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let records: Vec<_> = files.iter().map(|(name, bytes)| Record {
            name,
            comment: "",
            dir: false,
            date: 0.0,
            dos_permissions: None,
            data: RecordData::Content { bytes, deflate: false },
        }).collect();
        generate_records(&records).unwrap()
    }

    fn kind(result: Result<(), Thrown>) -> &'static str {
        match result.unwrap_err() {
            Thrown::Import(error) => error.kind,
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn selects_subfolder_and_preserves_root_license() {
        let temp = Temp::new();
        let bytes = zip(&[
            ("repo/skills/one/SKILL.md", b"skill"),
            ("repo/LICENSE.md", b"license"),
            ("repo/other/file", b"ignored"),
            ("repo/__MACOSX/junk", b"ignored"),
        ]);
        extract(&bytes, temp.path(), true, "skills/one", true).unwrap();
        assert_eq!(fs::read(temp.0.join("skills/one/SKILL.md")).unwrap(), b"skill");
        assert_eq!(fs::read(temp.0.join("LICENSE.md")).unwrap(), b"license");
        assert!(!temp.0.join("other").exists());
        assert!(!temp.0.join("__MACOSX").exists());
    }

    #[test]
    fn rejects_traversal_even_outside_selection() {
        let temp = Temp::new();
        let bytes = zip(&[("repo/skills/SKILL.md", b"ok"), ("repo/other/../../escape", b"bad")]);
        assert_eq!(kind(extract(&bytes, temp.path(), true, "skills", false)), "unsafe");
        assert!(!temp.0.exists());
    }

    #[test]
    fn rejects_unportable_selected_names_and_path_collisions() {
        let temp = Temp::new();
        let bytes = zip(&[("CON/../bad", b"bad")]);
        assert_eq!(kind(extract(&bytes, temp.path(), false, "", false)), "unsafe");
        let bytes = zip(&[("NUL.txt", b"bad")]);
        assert_eq!(kind(extract(&bytes, temp.path(), false, "", false)), "unsafe");
        let bytes = zip(&[("file", b"one"), ("file/child", b"two")]);
        assert_eq!(kind(extract(&bytes, temp.path(), false, "", false)), "unsafe");
    }

    #[test]
    fn duplicate_zip_names_follow_last_entry_and_existing_file_is_rejected() {
        let temp = Temp::new();
        let bytes = zip(&[("skill.md", b"first"), ("skill.md", b"last")]);
        extract(&bytes, temp.path(), false, "", false).unwrap();
        assert_eq!(fs::read(temp.0.join("skill.md")).unwrap(), b"last");
        assert_eq!(kind(extract(&bytes, temp.path(), false, "", false)), "unsafe");
    }

    #[test]
    fn rejects_invalid_zip_and_excess_part_count() {
        let temp = Temp::new();
        assert_eq!(kind(extract(b"not a zip", temp.path(), false, "", false)), "unsafe");
        let files = vec![("a", b"x".as_slice()); 20_001];
        assert_eq!(kind(extract(&zip(&files), temp.path(), false, "", false)), "too-large");
        assert!(!temp.0.exists());
    }

    #[test]
    fn rejects_selected_unix_symlink() {
        let temp = Temp::new();
        let mut bytes = zip(&[("link", b"target")]);
        let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bytes[central + 5] = 3;
        bytes[central + 38..central + 42].copy_from_slice(&(0o120777_u32 << 16).to_le_bytes());
        assert_eq!(kind(extract(&bytes, temp.path(), false, "", false)), "unsafe");
        assert!(!temp.0.exists());
    }
}
