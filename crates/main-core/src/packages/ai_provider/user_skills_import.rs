//! Skill import models, staged installation, export and source records.
//! Preparation accepts archive and preview callbacks so filesystem policy can
//! stay in this module while the caller supplies the remaining services.
//!
//! A Node file call that throws is a [`NodeError`]; a `SkillImportError` is a
//! [`SkillImportError`]; the `RangeError` of a SKILL.md read is a
//! [`SkillMdError`]. [`Thrown`] is whichever of them an async function
//! rejects with.

use std::fs;
use std::io;
use std::path::Path;

use jscompat::node_path::host;
use jscompat::{JsMap, JsObject, JsonValue, json_parse, json_stringify_pretty, to_iso_string};
use sha2::{Digest, Sha256};

use super::user_skill_md::{
    IEUM_SOURCE_FILE, MAX_USER_SKILL_BYTES, MAX_USER_SKILL_FILES, SkillMdError, is_inside_dir,
    parse_skill_md,
};
use super::user_skills_folder::{SkillFolderLimits, list_skill_folder_files};
use crate::node_fs::{NodeError, copy_file, node_error, node_error2};

// ---- errors.ts ----

/// `SkillImportError`: why taking in a skill, checking one for an update or
/// exporting one did not happen. `kind` is one of `SkillImportErrorKind`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillImportError {
    pub kind: &'static str,
    pub message: String,
}

impl SkillImportError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        SkillImportError {
            kind,
            message: message.into(),
        }
    }
}

/// What an async function of this package rejects with
#[derive(Debug, Clone, PartialEq)]
pub enum Thrown {
    Import(SkillImportError),
    Range(SkillMdError),
    Node(NodeError),
    /// a plain `Error`
    Plain(String),
}

impl From<SkillImportError> for Thrown {
    fn from(error: SkillImportError) -> Self {
        Thrown::Import(error)
    }
}

impl From<SkillMdError> for Thrown {
    fn from(error: SkillMdError) -> Self {
        Thrown::Range(error)
    }
}

impl From<NodeError> for Thrown {
    fn from(error: NodeError) -> Self {
        Thrown::Node(error)
    }
}

// ---- the file calls the skills code makes ----

/// `mkdir(path, { recursive: true })`
pub fn mkdir_recursive(path: &str) -> Result<(), NodeError> {
    fs::create_dir_all(path).map_err(|e| node_error(&e, "mkdir", path))
}

/// `mkdir(path)`
pub fn mkdir(path: &str) -> Result<(), NodeError> {
    fs::create_dir(path).map_err(|e| node_error(&e, "mkdir", path))
}

/// `writeFile(path, data)`
pub fn write_file(path: &str, data: &[u8]) -> Result<(), NodeError> {
    fs::write(path, data).map_err(|e| node_error(&e, "open", path))
}

/// `writeFile(path, data, { flag: 'wx' })`
pub fn write_file_exclusive(path: &str, data: &[u8]) -> Result<(), NodeError> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| node_error(&e, "open", path))?;
    file.write_all(data)
        .map_err(|e| node_error(&e, "write", path))
}

/// `rename(from, to)`
pub fn rename(from: &str, to: &str) -> Result<(), NodeError> {
    fs::rename(from, to).map_err(|e| node_error2(&e, "rename", from, to))
}

/// `copyFile(from, to)`
pub fn copy_file_over(from: &str, to: &str) -> Result<(), NodeError> {
    copy_file(from, to, false)
}

/// `readFile(path)`
pub fn read_file(path: &str) -> Result<Vec<u8>, NodeError> {
    fs::read(path).map_err(|e| {
        if is_dir(path) {
            crate::node_fs::node_error_fd(&e, "read")
        } else {
            node_error(&e, "open", path)
        }
    })
}

/// `readFile(path, 'utf8')`: invalid bytes read as U+FFFD
pub fn read_text(path: &str) -> Result<String, NodeError> {
    read_file(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// `rm(path, { recursive: true, force: true }).catch(() => undefined)`
pub fn remove_quietly(path: &str) {
    let target = Path::new(path);
    let removed = match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(target),
        Ok(_) => fs::remove_file(target),
        Err(e) => Err(e),
    };
    // a missing path and a path that cannot go are both left as they are
    let _: Result<(), io::Error> = removed;
}

/// `stat(path)` succeeded: the path is there (links followed)
pub fn exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

// ---- source-record.ts ----

/// `IeumSourceRecord`: the GitHub source record at the top of a skill installed
/// from GitHub
#[derive(Debug, Clone, PartialEq)]
pub struct IeumSourceRecord {
    /// `owner/repo`
    pub repo: String,
    /// the branch name settled when the skill was taken
    pub ref_: String,
    /// the skill folder inside the repository, `""` for the top
    pub path: String,
    pub commit: String,
    /// ISO time, whole seconds, UTC
    pub installed_at: String,
    /// `skillContentHash` of the installed folder
    pub content_hash: String,
}

impl IeumSourceRecord {
    /// The record as an object, keys in the documented order
    pub fn to_json(&self) -> JsonValue {
        let mut object = JsObject::new();
        let text = |value: &str| JsonValue::String(value.to_owned());
        object.set("kind", text("github"));
        object.set("repo", text(&self.repo));
        object.set("ref", text(&self.ref_));
        object.set("path", text(&self.path));
        object.set("commit", text(&self.commit));
        object.set("installedAt", text(&self.installed_at));
        object.set("contentHash", text(&self.content_hash));
        JsonValue::Object(object)
    }
}

/// `isoSeconds(date)`: `2026-10-21T09:00:00Z`, ISO time without the milliseconds
pub fn iso_seconds(ms: f64) -> Result<String, SkillMdError> {
    let iso = to_iso_string(ms).ok_or_else(|| SkillMdError("Invalid time value".to_owned()))?;
    // `.replace(/\.\d{3}Z$/, 'Z')`
    Ok(match iso.strip_suffix('Z') {
        Some(head) if head.len() >= 4 && head.as_bytes()[head.len() - 4] == b'.' => {
            format!("{}Z", &head[..head.len() - 4])
        }
        _ => iso,
    })
}

/// `renderIeumSourceRecord(record)`: the record as written to disk
pub fn render_ieum_source_record(record: &IeumSourceRecord) -> String {
    format!("{}\n", json_stringify_pretty(&record.to_json(), 2))
}

/// `readIeumSourceRecord(skillDir)`: `None` when missing or not of the documented shape
pub fn read_ieum_source_record(skill_dir: &str) -> Option<IeumSourceRecord> {
    let text = read_text(&host::join(&[skill_dir, IEUM_SOURCE_FILE])).ok()?;
    let JsonValue::Object(value) = json_parse(&text).ok()? else {
        return None;
    };
    let text_of = |key: &str| match value.get(key) {
        Some(JsonValue::String(text)) => Some(text.clone()),
        _ => None,
    };
    let repo = text_of("repo");
    let ref_ = text_of("ref");
    let path = text_of("path");
    let commit = text_of("commit");
    if value.get("kind") != Some(&JsonValue::String("github".to_owned())) {
        return None;
    }
    let (Some(repo), Some(ref_), Some(path), Some(commit)) = (repo, ref_, path, commit) else {
        return None;
    };
    if repo.is_empty() || ref_.is_empty() || commit.is_empty() {
        return None;
    }
    Some(IeumSourceRecord {
        repo,
        ref_,
        path,
        commit,
        installed_at: text_of("installedAt").unwrap_or_default(),
        content_hash: text_of("contentHash").unwrap_or_default(),
    })
}

// ---- folder.ts: the content hash ----

/// `skillContentHash(dir, { skillMd })`: SHA-256 (hex) of a skill folder's
/// paths, sizes and contents, leaving out the GitHub source record at its top.
/// `skill_md` hashes the SKILL.md as the given function rewrites its text.
pub fn skill_content_hash(
    dir: &str,
    skill_md: Option<&dyn Fn(&str) -> String>,
) -> Result<String, Thrown> {
    let Some(listed) = list_skill_folder_files(dir, None) else {
        return Err(Thrown::Plain(format!(
            "Cannot read the skill folder: {dir}"
        )));
    };
    let mut hash = Sha256::new();
    for file in &listed.files {
        if file.rel == IEUM_SOURCE_FILE {
            continue;
        }
        let read = read_file(&file.path)?;
        let content = match skill_md {
            Some(rewrite) if file.rel == "SKILL.md" => {
                rewrite(&String::from_utf8_lossy(&read)).into_bytes()
            }
            _ => read,
        };
        hash.update(format!("{}\0{}\0", file.rel, content.len()).as_bytes());
        hash.update(&content);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// ---- import.ts: shapes ----

/// Folders made while installing, inside the app's skills folder
pub const SKILL_STAGING_PREFIX: &str = ".ieum-staging-";

/// `SkillWarning`
#[derive(Debug, Clone, PartialEq)]
pub struct SkillWarning {
    pub rule: String,
    pub message_key: String,
    pub file: String,
    pub line: f64,
    pub excerpt: String,
}

/// `SkillImportCandidate`: one skill found by an import, as the preview shows it
#[derive(Debug, Clone, PartialEq)]
pub struct SkillImportCandidate {
    pub key: String,
    pub rel: String,
    pub dir: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub files: usize,
    pub bytes: u64,
    pub has_scripts: bool,
    pub has_template: bool,
    pub urls: Vec<String>,
    pub url_count: usize,
    pub license: Option<String>,
    pub warnings: Vec<SkillWarning>,
    pub warning_count: usize,
    pub collision: bool,
    pub empty_body: bool,
    /// a `SkillImportProblem`: an unreadable reason or `duplicate`
    pub problem: Option<&'static str>,
}

/// `SkillImportSource`: what the person handed over to take skills from
#[derive(Debug, Clone, PartialEq)]
pub enum SkillImportSource {
    Folder { folder: String },
    Zip { file: String },
    SkillFile { file: String },
    Text { text: String },
    Github { address: String },
}

fn skill_archive_layout<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    use std::collections::HashSet;
    let mut folders = HashSet::new();
    let mut skills = HashSet::new();
    for name in names {
        let is_dir = name.ends_with('/');
        let path = name.strip_suffix('/').unwrap_or(name);
        let parts: Vec<_> = path.split('/').collect();
        let top = parts[0];
        if top.is_empty() || top == "__MACOSX" || top.starts_with('.') { continue; }
        if parts.len() == 1 {
            if !is_dir && top.eq_ignore_ascii_case("SKILL.md") { return true; }
            if is_dir { folders.insert(top); }
        } else {
            folders.insert(top);
            if parts.len() == 2 && parts[1].eq_ignore_ascii_case("SKILL.md") { skills.insert(top); }
        }
    }
    !folders.is_empty() && folders.iter().all(|folder| skills.contains(folder))
}

/// Classify a dropped skill file or a ZIP whose layout contains only skill folders.
/// Unreadable, oversized, malformed and Codex-home inputs are not skill drops.
pub fn dropped_skill_source_of(path: &str, home_dir: &str) -> Option<SkillImportSource> {
    use std::io::Read;
    let name = path.rsplit(['/', '\\']).next().unwrap_or("");
    let named = name.eq_ignore_ascii_case("SKILL.md");
    if !named && !name.to_ascii_lowercase().ends_with(".zip") { return None; }
    if super::user_skill_md::is_inside_codex_home(path, home_dir) { return None; }
    if named { return Some(SkillImportSource::SkillFile { file: path.to_owned() }); }
    const LIMIT: u64 = 50 * 1024 * 1024;
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > LIMIT { return None; }
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > LIMIT { return None; }
    let limits = ooxml_pkg::ZipLimits { max_parts: 20_000, max_part_bytes: MAX_USER_SKILL_BYTES as f64, max_total_bytes: (4 * MAX_USER_SKILL_BYTES) as f64 };
    ooxml_pkg::assert_zip_inflates_within_limits(&bytes, &limits).ok()?;
    let entries = ooxml_pkg::jszip::load(&bytes, false).ok()?;
    skill_archive_layout(entries.iter().map(|entry| entry.unsafe_original_name.as_str()))
        .then(|| SkillImportSource::Zip { file: path.to_owned() })
}

/// `SkillImportOrigin`: where an import came from
#[derive(Debug, Clone, PartialEq)]
pub enum SkillImportOrigin {
    Folder {
        folder: String,
    },
    Zip {
        file: String,
    },
    SkillFile {
        file: String,
    },
    Text,
    Github {
        repo: String,
        ref_: String,
        commit: String,
        path: String,
    },
}

/// `SkillImport`: a prepared import waiting for the person's choice
#[derive(Debug, Clone, PartialEq)]
pub struct SkillImport {
    pub origin: SkillImportOrigin,
    pub root: String,
    pub candidates: Vec<SkillImportCandidate>,
    pub root_license: Option<String>,
    /// this import's temporary folder; `disposeSkillImport` removes it
    pub temp_dir: Option<String>,
}

/// `SkillFileChange`
#[derive(Debug, Clone, PartialEq)]
pub struct SkillFileChange {
    pub rel: String,
    /// `added`, `removed` or `modified`
    pub change: &'static str,
}

/// `GitHubSkillUpdateCheck`
#[derive(Debug, Clone, PartialEq)]
pub enum GitHubSkillUpdateCheck {
    UpToDate {
        record: IeumSourceRecord,
        commit: String,
        locally_edited: bool,
    },
    Changed(Box<ChangedCheck>),
}

/// The `changed` form of [`GitHubSkillUpdateCheck`]
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedCheck {
    pub record: IeumSourceRecord,
    pub commit: String,
    pub locally_edited: bool,
    pub changed_files: Vec<SkillFileChange>,
    pub update: SkillImport,
}

/// `SkillCollisionChoice` by candidate key, in the order the request has them
pub type CollisionChoices = Vec<(String, String)>;

/// `InstalledSkill`
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledSkill {
    pub key: String,
    pub name: String,
    pub dir: String,
    pub replaced: bool,
}

/// `InstallImportedSkillsResult`
#[derive(Debug, Clone, PartialEq)]
pub struct InstallImportedSkillsResult {
    pub installed: Vec<InstalledSkill>,
    pub skipped: Vec<String>,
}

pub fn diff_skill_folders(installed: &str, updated: &str) -> Result<Vec<SkillFileChange>, Thrown> {
    fn files(dir: &str) -> Result<Vec<super::user_skills_folder::SkillFolderFile>, Thrown> {
        let listing = list_skill_folder_files(dir, Some(&default_limits()))
            .ok_or_else(|| SkillImportError::new("failed", format!("Cannot read {dir}")))?;
        if listing.too_large {
            return Err(SkillImportError::new("too-large", "A skill folder is too large").into());
        }
        Ok(listing.files.into_iter().filter(|file| file.rel != IEUM_SOURCE_FILE).collect())
    }
    let before = files(installed)?;
    let after = files(updated)?;
    let before_by_name: std::collections::HashMap<_, _> = before.iter().map(|file| (file.rel.as_str(), file)).collect();
    let after_names: std::collections::HashSet<_> = after.iter().map(|file| file.rel.as_str()).collect();
    let mut changes = Vec::new();
    for file in &after {
        let change = match before_by_name.get(file.rel.as_str()) {
            None => Some("added"),
            Some(old) if old.size != file.size => Some("modified"),
            Some(old) => {
                let a = read_file(&old.path)?;
                let b = read_file(&file.path)?;
                (a != b).then_some("modified")
            }
        };
        if let Some(change) = change {
            changes.push(SkillFileChange { rel: file.rel.clone(), change });
        }
    }
    for file in before {
        if !after_names.contains(file.rel.as_str()) {
            changes.push(SkillFileChange { rel: file.rel, change: "removed" });
        }
    }
    changes.sort_by(|a, b| super::user_skills_folder::cmp_utf16(&a.rel, &b.rel));
    Ok(changes)
}

pub fn check_github_skill_update(
    skill_dir: &str,
    app_skills_dir: &str,
    temp_dir: &str,
    validate_repo: impl FnOnce(&str) -> Result<(), Thrown>,
    fetch_commit: impl FnOnce(&str, &str) -> Result<String, Thrown>,
    fetch_archive: impl FnOnce(&str, &str) -> Result<Vec<u8>, Thrown>,
    extract: impl FnOnce(&[u8], &str, &str) -> Result<(), Thrown>,
    describe: impl FnOnce(&str, &str, &str, &str) -> Result<Option<SkillImportCandidate>, Thrown>,
    aborted: Option<&dyn Fn() -> bool>,
) -> Result<GitHubSkillUpdateCheck, Thrown> {
    if !is_inside_dir(skill_dir, app_skills_dir) || is_inside_dir(app_skills_dir, skill_dir) {
        return Err(SkillImportError::new("invalid-source", "Only skills of the app folder are updated").into());
    }
    let record = read_ieum_source_record(skill_dir)
        .ok_or_else(|| SkillImportError::new("not-github", "The skill has no GitHub source record"))?;
    validate_repo(&record.repo)?;
    if !record.path.is_empty() && record.path.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\\')) {
        return Err(SkillImportError::new("not-github", "The source record has an unusable path").into());
    }
    let commit = fetch_commit(&record.repo, &record.ref_)?;
    let installed_hash = skill_content_hash(skill_dir, None)?;
    let local_edits = if installed_hash != record.content_hash {
        let parse_error = std::cell::RefCell::new(None);
        let normalized = skill_content_hash(skill_dir, Some(&|text| {
            match super::user_skill_md::without_skill_editors(text) {
                Ok(text) => text,
                Err(error) => {
                    *parse_error.borrow_mut() = Some(error);
                    String::new()
                }
            }
        }))?;
        if let Some(error) = parse_error.into_inner() { return Err(error.into()); }
        normalized != record.content_hash
    } else { false };
    if commit == record.commit {
        return Ok(GitHubSkillUpdateCheck::UpToDate { record, commit, locally_edited: local_edits });
    }
    let archive = fetch_archive(&record.repo, &commit)?;
    if aborted.is_some_and(|aborted| aborted()) {
        return Err(SkillImportError::new("cancelled", "Cancelled").into());
    }
    mkdir_recursive(temp_dir)?;
    let work = host::join(&[temp_dir, &format!("ieum-skill-update-{}", random_hex(6))]);
    mkdir(&work)?;
    let result = (|| {
        let root = host::join(&[&work, "repo"]);
        mkdir(&root)?;
        extract(&archive, &root, &record.path)?;
        if aborted.is_some_and(|aborted| aborted()) {
            return Err(SkillImportError::new("cancelled", "Cancelled").into());
        }
        let new_dir = if record.path.is_empty() { root.clone() } else { host::join(&[&root, &record.path]) };
        if !find_skill_folders(&root, &record.path, &[], None).iter().any(|rel| rel == &record.path) {
            return Err(SkillImportError::new("no-skills", "The skill is no longer there").into());
        }
        let new_hash = skill_content_hash(&new_dir, None)?;
        if new_hash == record.content_hash || new_hash == installed_hash {
            return Ok(GitHubSkillUpdateCheck::UpToDate { record: record.clone(), commit: commit.clone(), locally_edited: local_edits });
        }
        let candidate = describe(&new_dir, &record.path, app_skills_dir, skill_dir)?
            .ok_or_else(|| SkillImportError::new("no-skills", "The skill is no longer there"))?;
        let changed_files = diff_skill_folders(skill_dir, &new_dir)?;
        let update = SkillImport {
            origin: SkillImportOrigin::Github { repo: record.repo.clone(), ref_: record.ref_.clone(), commit: commit.clone(), path: record.path.clone() },
            root, candidates: vec![candidate], root_license: None, temp_dir: Some(work.clone()),
        };
        Ok(GitHubSkillUpdateCheck::Changed(Box::new(ChangedCheck {
            record: record.clone(), commit: commit.clone(), locally_edited: local_edits, changed_files, update,
        })))
    })();
    if !matches!(&result, Ok(GitHubSkillUpdateCheck::Changed(_))) {
        remove_quietly(&work);
    }
    result
}

pub fn install_imported_skills(
    imported: &SkillImport,
    app_skills_dir: &str,
    selected: &[String],
    choices: &CollisionChoices,
    options: &mut CommitSkillInstallOptions<'_>,
) -> Result<InstallImportedSkillsResult, Thrown> {
    use std::collections::HashSet;
    let existing = app_skill_folders_by_name(app_skills_dir)?;
    let mut selected_keys = HashSet::new();
    let mut names = HashSet::new();
    let mut plans = Vec::new();
    let mut skipped = Vec::new();
    for key in selected {
        if !selected_keys.insert(key) {
            continue;
        }
        let candidate = imported.candidates.iter().find(|candidate| &candidate.key == key)
            .ok_or_else(|| SkillImportError::new("not-installable", format!("No skill {key} in this import")))?;
        if candidate.problem == Some("too-large") {
            return Err(SkillImportError::new("too-large", format!("{} is too large", candidate.name)).into());
        }
        if candidate.problem.is_some() || super::user_skill_md::is_reserved_user_skill_name(&candidate.name) {
            return Err(SkillImportError::new("not-installable", format!("{} cannot be installed", candidate.name)).into());
        }
        let lower = candidate.name.to_lowercase();
        if !names.insert(lower.clone()) {
            return Err(SkillImportError::new("not-installable", format!("{} is chosen twice", candidate.name)).into());
        }
        let taken = existing.get(&lower).cloned().unwrap_or_default();
        if !taken.is_empty() {
            let choice = choices.iter().find(|(chosen, _)| chosen == key).map(|(_, choice)| choice.as_str());
            match choice {
                None => return Err(SkillImportError::new("needs-choice", format!("{} is already there", candidate.name)).into()),
                Some("skip") => {
                    skipped.push(key.clone());
                    continue;
                }
                Some(_) => {}
            }
        }
        let source = match &imported.origin {
            SkillImportOrigin::Github { repo, ref_, commit, .. } => Some(SkillInstallSource {
                repo: repo.clone(), ref_: ref_.clone(), commit: commit.clone(), path: candidate.rel.clone(),
            }),
            _ => None,
        };
        plans.push(SkillInstallPlan { candidate: candidate.clone(), folder_name: candidate.name.clone(), replace_dirs: taken, source });
    }
    let dirs = if plans.is_empty() { Vec::new() } else { commit_skill_install(app_skills_dir, &plans, options)? };
    let installed = plans.into_iter().zip(dirs).map(|(plan, dir)| InstalledSkill {
        key: plan.candidate.key, name: plan.candidate.name, dir, replaced: !plan.replace_dirs.is_empty(),
    }).collect();
    Ok(InstallImportedSkillsResult { installed, skipped })
}

pub fn export_skill_bundle_zip(skill_dirs: &[String], out_path: &str, now_ms: f64) -> Result<(), Thrown> {
    use std::collections::HashSet;
    use ooxml_pkg::write::{Record, RecordData, generate_records};
    if skill_dirs.is_empty() {
        return Err(SkillImportError::new("not-installable", "No skill to export").into());
    }
    let mut names = HashSet::new();
    let mut contents = Vec::new();
    for dir in skill_dirs {
        let skill = super::user_skills_folder::read_user_skill_folder(dir, "app", None, None)?
            .ok_or_else(|| SkillImportError::new("not-installable", "This skill cannot be exported"))?;
        if !names.insert(skill.name.to_lowercase()) {
            return Err(SkillImportError::new("not-installable", format!("Two skills are named {}", skill.name)).into());
        }
        let listed = list_skill_folder_files(dir, Some(&default_limits()))
            .ok_or_else(|| SkillImportError::new("failed", format!("Cannot read {dir}")))?;
        if listed.too_large {
            return Err(SkillImportError::new("too-large", "The skill folder is too large").into());
        }
        for file in listed.files {
            if file.rel != IEUM_SOURCE_FILE {
                contents.push((format!("{}/{}", skill.name, file.rel), read_file(&file.path)?));
            }
        }
    }
    let records: Vec<_> = contents.iter().map(|(name, bytes)| Record {
        name, comment: "", dir: false, date: now_ms, dos_permissions: None,
        data: RecordData::Content { bytes, deflate: true },
    }).collect();
    let bytes = generate_records(&records).map_err(|e| Thrown::Plain(e.to_string()))?;
    let partial = host::join(&[&host::dirname(out_path), &format!(".{}.{}.part", host::basename(out_path, None), random_hex(4))]);
    if write_file_exclusive(&partial, &bytes).and_then(|()| rename(&partial, out_path)).is_err() {
        remove_quietly(&partial);
        return Err(SkillImportError::new("failed", format!("Could not write {out_path}")).into());
    }
    Ok(())
}


pub fn find_skill_folders(root: &str, start: &str, skip_dirs: &[String], home_dir: Option<&str>) -> Vec<String> {
    fn walk(dir: &str, rel: &str, depth: usize, skip_dirs: &[String], home: Option<&str>, found: &mut Vec<String>) {
        if skip_dirs.iter().any(|skip| is_inside_dir(dir, skip))
            || home.is_some_and(|home| super::user_skill_md::is_inside_codex_home(dir, home))
        {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        if entries.iter().any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let matches = if cfg!(windows) { name.eq_ignore_ascii_case("SKILL.md") } else { name == "SKILL.md" };
            matches && entry.file_type().is_ok_and(|kind| kind.is_file() || kind.is_symlink())
        }) {
            found.push(rel.to_owned());
            return;
        }
        if depth >= 8 { return; }
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), ".git" | "node_modules" | "__MACOSX")
                || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let child = host::join(&[dir, &name]);
            let child_rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            walk(&child, &child_rel, depth + 1, skip_dirs, home, found);
        }
    }
    let mut found = Vec::new();
    let dir = if start.is_empty() { root.to_owned() } else { host::join(&[root, start]) };
    walk(&dir, start, 0, skip_dirs, home_dir, &mut found);
    found.sort_by(|a, b| super::user_skills_folder::cmp_utf16(a, b));
    found
}

pub fn license_file_in(dir: &str) -> Option<String> {
    let mut names: Vec<_> = fs::read_dir(dir).ok()?.filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            let lower = name.to_lowercase();
            ["license", "licence", "copying", "unlicense"].iter().any(|prefix| {
                lower.strip_prefix(prefix).is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('-'))
            })
        }).collect();
    names.sort_by(|a, b| super::user_skills_folder::cmp_utf16(a, b));
    names.into_iter().next()
}

fn skill_text(text: &str) -> Result<String, Thrown> {
    let source = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lead = 0;
    let mut offset = 0;
    let mut fenced = false;
    let mut opening = None;
    for line in source.split_inclusive('\n') {
        if lead > 5 { break; }
        let trimmed = line.trim();
        if trimmed == "---" {
            opening = Some(offset);
            break;
        }
        if !trimmed.is_empty() { lead += 1; }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") { fenced = true; }
        offset += line.len();
    }
    let Some(start) = opening else {
        return Err(SkillImportError::new("no-skills", "No skill was found").into());
    };
    let mut rest = source[start..].trim_start_matches([' ', '\t']).to_owned();
    if !rest.lines().skip(1).any(|line| line.trim() == "---") {
        return Err(SkillImportError::new("unclosed-frontmatter", "The front matter is not closed").into());
    }
    if fenced {
        let mut offset = 0;
        let mut fences = Vec::new();
        for line in rest.split_inclusive('\n') {
            let trimmed = line.trim_start_matches([' ', '\t']);
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") { fences.push(offset); }
            offset += line.len();
        }
        if fences.len() % 2 == 1 {
            rest.truncate(*fences.last().expect("odd fence count"));
        }
    }
    if parse_skill_md(&rest)?.and_then(|parsed| parsed.name).is_none_or(|name| name.trim().is_empty()) {
        return Err(SkillImportError::new("no-skills", "No skill was found").into());
    }
    Ok(rest)
}

pub fn prepare_single_file_import(
    text: &str,
    origin: SkillImportOrigin,
    app_skills_dir: &str,
    temp_dir: &str,
    describe: impl FnOnce(&str, &[String], &str) -> Result<Vec<SkillImportCandidate>, Thrown>,
) -> Result<SkillImport, Thrown> {
    if text.len() > 2 * 1024 * 1024 {
        return Err(SkillImportError::new("too-large", "The SKILL.md text is too large").into());
    }
    let text = skill_text(text)?;
    let name = parse_skill_md(&text)?.and_then(|parsed| parsed.name)
        .map(|name| name.trim().to_owned()).filter(|name| !name.is_empty())
        .ok_or_else(|| SkillImportError::new("no-skills", "No skill was found"))?;
    mkdir_recursive(temp_dir)?;
    let work = host::join(&[temp_dir, &format!("ieum-skill-import-{}", random_hex(6))]);
    mkdir(&work)?;
    let result = (|| {
        let root = host::join(&[&work, "text"]);
        let folder = if super::user_skills_states::is_valid_user_skill_name(&name) { name.as_str() } else { "skill" };
        let dir = host::join(&[&root, folder]);
        mkdir_recursive(&dir)?;
        write_file(&host::join(&[&dir, "SKILL.md"]), text.as_bytes())?;
        let candidates = describe(&root, &[folder.to_owned()], app_skills_dir)?;
        if candidates.is_empty() {
            return Err(SkillImportError::new("no-skills", "No skill was found").into());
        }
        Ok(SkillImport { origin, root, candidates, root_license: None, temp_dir: Some(work.clone()) })
    })();
    if result.is_err() {
        remove_quietly(&work);
    }
    result
}

pub fn prepare_skill_file_import(
    file: &str,
    app_skills_dir: &str,
    home_dir: &str,
    temp_dir: &str,
    describe: impl FnOnce(&str, &[String], &str) -> Result<Vec<SkillImportCandidate>, Thrown>,
) -> Result<SkillImport, Thrown> {
    use std::io::Read;
    if super::user_skill_md::is_inside_codex_home(file, home_dir) {
        return Err(SkillImportError::new("invalid-source", "The file is inside the Codex home").into());
    }
    let metadata = fs::metadata(file).ok().filter(|metadata| metadata.is_file())
        .ok_or_else(|| SkillImportError::new("not-found", "The file cannot be read"))?;
    const LIMIT: u64 = 2 * 1024 * 1024;
    if metadata.len() > LIMIT {
        return Err(SkillImportError::new("too-large", "The SKILL.md file is too large").into());
    }
    let mut bytes = Vec::new();
    fs::File::open(file).and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|_| SkillImportError::new("not-found", "The file cannot be read"))?;
    if bytes.len() as u64 > LIMIT {
        return Err(SkillImportError::new("too-large", "The SKILL.md file is too large").into());
    }
    let text = String::from_utf8_lossy(&bytes);
    prepare_single_file_import(&text, SkillImportOrigin::SkillFile { file: file.to_owned() }, app_skills_dir, temp_dir, describe)
}

pub fn prepare_folder_import(
    folder: &str,
    app_skills_dir: &str,
    home_dir: &str,
    describe: impl FnOnce(&str, &[String], &str) -> Result<Vec<SkillImportCandidate>, Thrown>,
) -> Result<SkillImport, Thrown> {
    if is_inside_dir(folder, app_skills_dir) {
        return Err(SkillImportError::new("invalid-source", "The folder is inside the app skills folder").into());
    }
    if super::user_skill_md::is_inside_codex_home(folder, home_dir) {
        return Err(SkillImportError::new("invalid-source", "The folder is inside the Codex home").into());
    }
    if !fs::metadata(folder).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(SkillImportError::new("no-skills", "No skill was found").into());
    }
    let rels = find_skill_folders(folder, "", &[app_skills_dir.to_owned()], Some(home_dir));
    let candidates = describe(folder, &rels, app_skills_dir)?;
    if candidates.is_empty() {
        return Err(SkillImportError::new("no-skills", "No skill was found").into());
    }
    Ok(SkillImport {
        origin: SkillImportOrigin::Folder { folder: folder.to_owned() },
        root: folder.to_owned(),
        candidates,
        root_license: license_file_in(folder),
        temp_dir: None,
    })
}

pub fn prepare_zip_import(
    file: &str,
    app_skills_dir: &str,
    temp_dir: &str,
    extract: impl FnOnce(&[u8], &str) -> Result<(), Thrown>,
    describe: impl FnOnce(&str, &[String], &str) -> Result<Vec<SkillImportCandidate>, Thrown>,
) -> Result<SkillImport, Thrown> {
    let metadata = fs::metadata(file).ok().filter(|metadata| metadata.is_file())
        .ok_or_else(|| SkillImportError::new("not-found", "The zip file cannot be read"))?;
    const LIMIT: u64 = 50 * 1024 * 1024;
    if metadata.len() > LIMIT {
        return Err(SkillImportError::new("too-large", format!("The zip file is {} bytes (limit {LIMIT})", metadata.len())).into());
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    fs::File::open(file)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|_| SkillImportError::new("not-found", "The zip file cannot be read"))?;
    if bytes.len() as u64 > LIMIT {
        return Err(SkillImportError::new("too-large", format!("The zip file is {} bytes (limit {LIMIT})", bytes.len())).into());
    }
    mkdir_recursive(temp_dir)?;
    let work = host::join(&[temp_dir, &format!("ieum-skill-import-{}", random_hex(6))]);
    mkdir(&work)?;
    let result = (|| {
        let root = host::join(&[&work, "zip"]);
        mkdir(&root)?;
        extract(&bytes, &root)?;
        let rels = find_skill_folders(&root, "", &[], None);
        let candidates = describe(&root, &rels, app_skills_dir)?;
        if candidates.is_empty() {
            return Err(SkillImportError::new("no-skills", "No skill was found").into());
        }
        Ok(SkillImport {
            origin: SkillImportOrigin::Zip { file: file.to_owned() },
            root_license: license_file_in(&root),
            root,
            candidates,
            temp_dir: Some(work.clone()),
        })
    })();
    if result.is_err() {
        remove_quietly(&work);
    }
    result
}

pub fn prepare_github_archive(
    origin: SkillImportOrigin,
    only_skill: Option<&str>,
    bytes: &[u8],
    app_skills_dir: &str,
    temp_dir: &str,
    extract: impl FnOnce(&[u8], &str, &str) -> Result<(), Thrown>,
    describe: impl FnOnce(&str, &[String], &str, Option<&str>) -> Result<Vec<SkillImportCandidate>, Thrown>,
    aborted: Option<&dyn Fn() -> bool>,
) -> Result<SkillImport, Thrown> {
    let SkillImportOrigin::Github { path, .. } = &origin else {
        return Err(SkillImportError::new("invalid-source", "Not a GitHub import").into());
    };
    if !path.is_empty() && path.split('/').any(|part| {
        part.is_empty() || part == "." || part == ".." || part.contains('\\')
            || part.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
    }) {
        return Err(SkillImportError::new("bad-address", "Not a GitHub address the app accepts").into());
    }
    if aborted.is_some_and(|aborted| aborted()) {
        return Err(SkillImportError::new("cancelled", "Cancelled").into());
    }
    mkdir_recursive(temp_dir)?;
    let work = host::join(&[temp_dir, &format!("ieum-skill-import-{}", random_hex(6))]);
    mkdir(&work)?;
    let result = (|| {
        let root = host::join(&[&work, "repo"]);
        mkdir(&root)?;
        extract(bytes, &root, path)?;
        if aborted.is_some_and(|aborted| aborted()) {
            return Err(SkillImportError::new("cancelled", "Cancelled").into());
        }
        let start = if path.is_empty() { root.clone() } else { host::join(&[&root, path]) };
        if !fs::metadata(start).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(SkillImportError::new("no-skills", "No skill was found").into());
        }
        let rels = find_skill_folders(&root, path, &[], None);
        let candidates = describe(&root, &rels, app_skills_dir, only_skill)?;
        if aborted.is_some_and(|aborted| aborted()) {
            return Err(SkillImportError::new("cancelled", "Cancelled").into());
        }
        if candidates.is_empty() {
            return Err(SkillImportError::new("no-skills", "No skill was found").into());
        }
        Ok(SkillImport { origin: origin.clone(), root_license: license_file_in(&root), root, candidates, temp_dir: Some(work.clone()) })
    })();
    if result.is_err() {
        remove_quietly(&work);
    }
    result
}

fn default_limits() -> SkillFolderLimits {
    SkillFolderLimits {
        files: MAX_USER_SKILL_FILES,
        bytes: MAX_USER_SKILL_BYTES,
    }
}

/// `appSkillFoldersByName(appSkillsDir)`: the app's own skills by lowercase
/// name: each folder under its folder name and under the `name` its `SKILL.md`
/// gives. Staging folders are left out.
pub(crate) fn app_skill_folders_by_name(
    app_skills_dir: &str,
) -> Result<JsMap<Vec<String>>, SkillMdError> {
    let mut by_name: JsMap<Vec<String>> = JsMap::new();
    let mut add = |name: &str, dir: &str| {
        let key = name.to_lowercase();
        let mut dirs = by_name.get(&key).cloned().unwrap_or_default();
        if !dirs.iter().any(|known| known == dir) {
            dirs.push(dir.to_owned());
        }
        by_name.set(key, dirs);
    };
    for name in read_dir_names(app_skills_dir) {
        if name.starts_with(SKILL_STAGING_PREFIX) {
            continue;
        }
        let dir = host::join(&[app_skills_dir, &name]);
        if !is_dir(&dir) {
            continue;
        }
        add(&name, &dir);
        let text = read_text(&host::join(&[&dir, "SKILL.md"])).ok();
        let found = match &text {
            Some(text) => parse_skill_md(text)?
                .and_then(|parsed| parsed.name)
                .map(|name| jscompat::trim(&name).to_owned())
                .filter(|name| !name.is_empty()),
            None => None,
        };
        if let Some(found) = found {
            add(&found, &dir);
        }
    }
    Ok(by_name)
}

/// `readDirents(dir)`'s names, in the order `readdir` gives them (sorted by
/// name as libuv sorts them); an unreadable folder holds none
pub fn read_dir_names(dir: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names
}

/// `copySkillFolder(from, to, limits)`: copy a skill folder's files (links
/// inside it as the files they lead to), leaving out a source record at its
/// top. `too-large` over the limits.
fn copy_skill_folder(from: &str, to: &str, limits: &SkillFolderLimits) -> Result<(), Thrown> {
    let Some(listed) = list_skill_folder_files(from, Some(limits)) else {
        return Err(SkillImportError::new("failed", format!("Cannot read {from}")).into());
    };
    if listed.too_large {
        return Err(SkillImportError::new("too-large", "A skill folder is too large").into());
    }
    mkdir_recursive(to)?;
    for file in &listed.files {
        if file.rel == IEUM_SOURCE_FILE {
            continue;
        }
        let parts: Vec<&str> = file.rel.split('/').collect();
        let mut target = to.to_owned();
        for part in &parts {
            target = host::join(&[&target, part]);
        }
        mkdir_recursive(&host::dirname(&target))?;
        copy_file_over(&file.path, &target)?;
    }
    Ok(())
}

/// `SkillInstallPlan`: one skill to put into the app's skills folder
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInstallPlan {
    pub candidate: SkillImportCandidate,
    /// the folder name under the app's skills folder
    pub folder_name: String,
    /// existing folders it replaces
    pub replace_dirs: Vec<String>,
    /// for a GitHub skill: the record to write, without the time and hash
    pub source: Option<SkillInstallSource>,
}

/// `SkillInstallPlan.source`
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInstallSource {
    pub repo: String,
    pub ref_: String,
    pub path: String,
    pub commit: String,
}

/// A replaced folder that could not be moved away (the OS trash refused it)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardFailed;

/// How a replaced folder goes away: `Err` is a folder that could not go
pub type DiscardReplaced<'a> = &'a mut dyn FnMut(&str) -> Result<(), DiscardFailed>;

/// `CommitSkillInstallOptions`
pub struct CommitSkillInstallOptions<'a> {
    /// `now()` in milliseconds since the epoch
    pub now_ms: f64,
    /// how a replaced folder goes away (the shell passes the OS trash);
    /// `Err` is a folder that could not go. Removed for good when not given.
    pub discard_replaced: Option<DiscardReplaced<'a>>,
    pub limits: Option<SkillFolderLimits>,
    pub random: Option<&'a mut dyn FnMut() -> String>,
}

/// a folder an install moved aside: where it was and where it is now
struct Aside {
    from: String,
    aside: String,
}

/// `commitSkillInstall(appSkillsDir, plans, options)`: put skills into the
/// app's skills folder: each is written completely into a staging folder
/// first, then folders it replaces are moved aside and it is renamed into
/// place. If anything fails, what was moved is moved back and nothing is
/// installed.
pub fn commit_skill_install(
    app_skills_dir: &str,
    plans: &[SkillInstallPlan],
    options: &mut CommitSkillInstallOptions<'_>,
) -> Result<Vec<String>, Thrown> {
    let limits = options.limits.clone().unwrap_or_else(default_limits);
    for plan in plans {
        for dir in &plan.replace_dirs {
            // a direct child of the app's skills folder, never that folder itself
            let child = is_inside_dir(app_skills_dir, &host::dirname(dir))
                && !is_inside_dir(app_skills_dir, dir);
            if !child || !is_inside_dir(dir, app_skills_dir) {
                return Err(SkillImportError::new(
                    "invalid-source",
                    "Only skills of the app folder are replaced",
                )
                .into());
            }
        }
    }
    mkdir_recursive(app_skills_dir)?;
    let suffix = match options.random.as_mut() {
        Some(random) => random(),
        None => random_hex(6),
    };
    let stage = host::join(&[app_skills_dir, &format!("{SKILL_STAGING_PREFIX}{suffix}")]);
    mkdir(&stage)?;
    let mut moved: Vec<(String, String)> = Vec::new();
    // per plan, the folders it replaced as moved aside into the staging folder
    let mut replaced: Vec<Vec<Aside>> = plans.iter().map(|_| Vec::new()).collect();
    let mut installed: Vec<String> = Vec::new();
    let attempt = (|| -> Result<(), Thrown> {
        let mut staged: Vec<String> = Vec::new();
        for (index, plan) in plans.iter().enumerate() {
            let dir = host::join(&[&stage, &format!("new-{index}")]);
            copy_skill_folder(&plan.candidate.dir, &dir, &limits)?;
            if let Some(source) = &plan.source {
                let record = IeumSourceRecord {
                    repo: source.repo.clone(),
                    ref_: source.ref_.clone(),
                    path: source.path.clone(),
                    commit: source.commit.clone(),
                    installed_at: iso_seconds(options.now_ms)?,
                    content_hash: skill_content_hash(&dir, None)?,
                };
                write_file(
                    &host::join(&[&dir, IEUM_SOURCE_FILE]),
                    render_ieum_source_record(&record).as_bytes(),
                )?;
            }
            staged.push(dir);
        }
        for (index, plan) in plans.iter().enumerate() {
            for (n, dir) in plan.replace_dirs.iter().enumerate() {
                if !exists(dir) {
                    continue;
                }
                let aside = host::join(&[
                    &stage,
                    &format!("old-{index}-{n}"),
                    &host::basename(dir, None),
                ]);
                mkdir_recursive(&host::dirname(&aside))?;
                rename(dir, &aside)?;
                moved.push((dir.clone(), aside.clone()));
                replaced[index].push(Aside {
                    from: dir.clone(),
                    aside,
                });
            }
            let target = host::join(&[app_skills_dir, &plan.folder_name]);
            if exists(&target) {
                return Err(SkillImportError::new(
                    "needs-choice",
                    format!("{} is already there", plan.folder_name),
                )
                .into());
            }
            rename(&staged[index], &target)?;
            moved.push((staged[index].clone(), target.clone()));
            installed.push(target);
        }
        Ok(())
    })();
    if let Err(error) = attempt {
        let mut restored = true;
        for (from, to) in moved.iter().rev() {
            // `moved` holds where each folder was and where it went: put it back
            if rename(to, from).is_err() {
                restored = false;
            }
        }
        // a folder that could not be moved back stays in the staging folder rather than being lost
        if restored {
            remove_quietly(&stage);
        }
        return Err(match error {
            Thrown::Import(error) => Thrown::Import(error),
            _ => SkillImportError::new("failed", "Installing the skill failed").into(),
        });
    }
    if let Some(discard) = options.discard_replaced.as_mut()
        && !discard_or_restore(&stage, &replaced, &installed, &mut **discard)
    {
        return Err(SkillImportError::new(
            "trash-failed",
            "A replaced skill could not be moved to the trash; it was put back",
        )
        .into());
    }
    remove_quietly(&stage);
    Ok(installed)
}

/// Hand the replaced folders to `discard` (the OS trash). If one cannot go,
/// nothing is deleted for good: every plan none of whose folders has gone yet
/// is undone (its new folder out, its old ones back), and the staging folder
/// is kept when anything could not be put back. False when one could not go.
fn discard_or_restore(
    stage: &str,
    replaced: &[Vec<Aside>],
    installed: &[String],
    discard: &mut dyn FnMut(&str) -> Result<(), DiscardFailed>,
) -> bool {
    let mut gone: Vec<&str> = Vec::new();
    let mut failed = false;
    for folders in replaced {
        for folder in folders {
            if discard(&folder.aside).is_ok() {
                gone.push(&folder.aside);
            } else {
                failed = true;
                break;
            }
        }
        if failed {
            break;
        }
    }
    if !failed {
        return true;
    }
    let mut restored = true;
    for (index, folders) in replaced.iter().enumerate().rev() {
        // a plan whose old folder is already in the trash stays installed: undoing it would lose both
        if folders
            .iter()
            .any(|folder| gone.contains(&folder.aside.as_str()))
        {
            if folders
                .iter()
                .any(|folder| !gone.contains(&folder.aside.as_str()))
            {
                restored = false;
            }
            continue;
        }
        let undone = (|| -> Result<(), NodeError> {
            rename(
                &installed[index],
                &host::join(&[stage, &format!("undone-{index}")]),
            )?;
            for folder in folders {
                rename(&folder.aside, &folder.from)?;
            }
            Ok(())
        })();
        if undone.is_err() {
            restored = false;
        }
    }
    if restored {
        remove_quietly(stage);
    }
    false
}

/// `randomBytes(n).toString('hex')`
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("the platform random source");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {


    #[test]
    fn extracts_pasted_frontmatter_and_reports_truncation() {
        let text = "Here is the skill:\n```yaml\n---\nname: example\ndescription: Example\n---\nBody\n```\n";
        assert_eq!(
            super::skill_text(text).unwrap(),
            "---\nname: example\ndescription: Example\n---\nBody\n"
        );
        assert!(matches!(
            super::skill_text("---\nname: example\n"),
            Err(super::Thrown::Import(error)) if error.kind == "unclosed-frontmatter"
        ));
    }

    #[test]
    fn failed_single_file_preview_removes_work_directory() {
        let temp = std::env::temp_dir().join(format!("skill-import-test-{}", super::random_hex(8)));
        std::fs::create_dir(&temp).unwrap();
        let root = temp.to_string_lossy().into_owned();
        let result = super::prepare_single_file_import(
            "---\nname: sample\ndescription: Sample\n---\nBody\n",
            super::SkillImportOrigin::Text,
            &root,
            &root,
            |_, _, _| Err(super::SkillImportError::new("failed", "preview failed").into()),
        );
        assert!(matches!(result, Err(super::Thrown::Import(error)) if error.kind == "failed"));
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
        std::fs::remove_dir(temp).unwrap();
    }

    #[test]
    fn drop_layout_accepts_single_or_complete_bundle_only() {
        assert!(super::skill_archive_layout(["SKILL.md", "assets/icon.png"]));
        assert!(super::skill_archive_layout(["one/SKILL.md", "two/skill.md", "README.md", "__MACOSX/extra"]));
        assert!(!super::skill_archive_layout(["one/SKILL.md", "two/readme.md"]));
        assert!(!super::skill_archive_layout(["nested/other/SKILL.md"]));
    }
}
