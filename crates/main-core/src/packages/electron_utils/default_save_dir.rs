//! `packages/electron-utils/src/default-save-dir.ts`: the default folder where
//! new files land on their first save.
//!
//! The process-wide state of the TypeScript (the folders that failed lately,
//! the fallback in use) is a [`SaveDirState`] the caller holds, and
//! `Date.now()` is a parameter (`now_ms`).

use std::collections::HashMap;
use std::fs;

use jscompat::node_path::host::{dirname, is_absolute, join, resolve};
use jscompat::{JsonValue, json_parse};

use crate::node_fs::{NodeError, node_error};
use crate::packages::electron_utils::atomic_write::owned::{Admission, admitted};
use crate::packages::i18n::PRODUCT_NAME;

/// `DEFAULT_SAVE_DIR_KEY`
pub const DEFAULT_SAVE_DIR_KEY: &str = "defaultSaveDir";

/// `UNUSABLE_RETRY_MS`
const UNUSABLE_RETRY_MS: f64 = 30_000.0;

/// What an `Error` the TypeScript throws says (`error.message`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thrown(pub String);

/// `PathProvider`: `app.getPath(name)` for `userData`, `documents` and
/// `home`. `Err` is what Electron throws when the OS cannot report a folder.
pub trait PathProvider {
    fn get_path(&self, name: &str) -> Result<String, Thrown>;
}

/// `SaveDirState.fallback`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveDirFallback {
    pub blocked: String,
    pub using: String,
}

/// `SaveDirState`
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SaveDirState {
    unusable_until: HashMap<String, f64>,
    fallback: Option<SaveDirFallback>,
}

impl SaveDirState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish worker cache changes without overwriting a concurrent resolver.
    /// A rejected cache publication never requires repeating filesystem work.
    pub fn publish_resolution(&mut self, before: &Self, resolved: Self) -> bool {
        if self != before {
            return false;
        }
        *self = resolved;
        true
    }
}

/// `readDefaultSaveDirSetting(settingsPath)`
pub fn read_default_save_dir_setting(settings_path: &str) -> Option<String> {
    let bytes = fs::read(settings_path).ok()?;
    let raw = json_parse(&String::from_utf8_lossy(&bytes)).ok()?;
    let JsonValue::Object(raw) = raw else {
        return None;
    };
    match raw.get(DEFAULT_SAVE_DIR_KEY) {
        Some(JsonValue::String(value)) if is_absolute(value) => Some(value.clone()),
        _ => None,
    }
}

fn is_directory(dir: &str) -> bool {
    fs::metadata(dir).is_ok_and(|m| m.is_dir())
}

fn missing_directory_levels(dir: &str) -> Vec<String> {
    let mut missing = Vec::new();
    let mut current = resolve(&[dir]);
    while !is_directory(&current) {
        missing.push(current.clone());
        let parent = dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    missing
}

/// `createDirLevels(dir)`
pub fn create_dir_levels(dir: &str) -> Result<(), NodeError> {
    for level in missing_directory_levels(dir).iter().rev() {
        fs::create_dir(level).map_err(|error| node_error(&error, "mkdir", level))?;
    }
    Ok(())
}

/// Prepare/stat outside admission, then admit each actual mkdir separately.
/// The outer error is lost authority; an inner error is the original mkdir failure.
fn create_levels_authorized(
    dir: &str,
    admit: &mut crate::node_fs::FileAdmission<'_>,
) -> Result<Result<(), NodeError>, NodeError> {
    for level in missing_directory_levels(dir).iter().rev() {
        let mut result = None;
        admit(&mut || {
            result =
                Some(fs::create_dir(level).map_err(|error| node_error(&error, "mkdir", level)));
        })?;
        match result {
            Some(Ok(())) => {}
            Some(Err(error)) => return Ok(Err(error)),
            None => {
                return Err(NodeError {
                    code: None,
                    message: "Directory admission did not execute mkdir".into(),
                });
            }
        }
    }
    Ok(Ok(()))
}

pub fn create_dir_levels_authorized(
    dir: &str,
    admit: &mut crate::node_fs::FileAdmission<'_>,
) -> Result<(), NodeError> {
    create_levels_authorized(dir, admit)?
}

fn create_levels_owned(dir: &str, admit: &Admission) -> Result<Result<(), NodeError>, NodeError> {
    for level in missing_directory_levels(dir).into_iter().rev() {
        let result = admitted(admit, move || {
            fs::create_dir(&level).map_err(|error| node_error(&error, "mkdir", &level))
        })?;
        if let Err(error) = result {
            return Ok(Err(error));
        }
    }
    Ok(Ok(()))
}

pub fn create_dir_levels_owned(dir: &str, admit: &Admission) -> Result<(), NodeError> {
    create_levels_owned(dir, admit)?
}

/// `accessSync(dir, constants.W_OK)` as a yes or no
#[cfg(unix)]
fn writable(dir: &str) -> bool {
    match std::ffi::CString::new(dir) {
        // SAFETY: `c_path` is a valid NUL-terminated string for the call
        Ok(c_path) => unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 },
        Err(_) => false,
    }
}

/// libuv refuses write access only to a read-only file, never to a folder
#[cfg(windows)]
fn writable(dir: &str) -> bool {
    fs::metadata(dir).is_ok_and(|m| m.is_dir() || !m.permissions().readonly())
}

/// `isUsableSaveDir(dir)`
pub fn is_usable_save_dir(dir: &str) -> bool {
    if !is_directory(dir) && create_dir_levels(dir).is_err() {
        return false;
    }
    writable(dir)
}

pub fn is_usable_save_dir_authorized(
    dir: &str,
    admit: &mut crate::node_fs::FileAdmission<'_>,
) -> Result<bool, NodeError> {
    if !is_directory(dir) && create_levels_authorized(dir, admit)?.is_err() {
        return Ok(false);
    }
    Ok(writable(dir))
}

pub fn is_usable_save_dir_owned(dir: &str, admit: &Admission) -> Result<bool, NodeError> {
    if !is_directory(dir) && create_levels_owned(dir, admit)?.is_err() {
        return Ok(false);
    }
    Ok(writable(dir))
}

fn usable_now_with(
    state: &mut SaveDirState,
    dir: &str,
    now_ms: f64,
    usable: impl FnOnce() -> Result<bool, NodeError>,
) -> Result<bool, NodeError> {
    let key = resolve(&[dir]);
    if state
        .unusable_until
        .get(&key)
        .is_some_and(|until| now_ms < *until)
    {
        return Ok(false);
    }
    if usable()? {
        state.unusable_until.remove(&key);
        return Ok(true);
    }
    state.unusable_until.insert(key, now_ms + UNUSABLE_RETRY_MS);
    Ok(false)
}

/// `usableNow(dir)`
fn usable_now(state: &mut SaveDirState, dir: &str, now_ms: f64) -> bool {
    let key = resolve(&[dir]);
    if let Some(until) = state.unusable_until.get(&key)
        && now_ms < *until
    {
        return false;
    }
    if is_usable_save_dir(dir) {
        state.unusable_until.remove(&key);
        return true;
    }
    state.unusable_until.insert(key, now_ms + UNUSABLE_RETRY_MS);
    false
}

/// `resolveDefaultSaveDir(configured, fallbackDir)`
pub fn resolve_default_save_dir(
    state: &mut SaveDirState,
    configured: Option<&str>,
    fallbacks: &[String],
    now_ms: f64,
) -> Result<String, Thrown> {
    resolve_default_save_dir_with(
        state,
        configured,
        fallbacks,
        now_ms,
        |state, dir, now_ms| Ok(usable_now(state, dir, now_ms)),
    )
}

fn resolve_default_save_dir_with(
    state: &mut SaveDirState,
    configured: Option<&str>,
    fallbacks: &[String],
    now_ms: f64,
    mut usable: impl FnMut(&mut SaveDirState, &str, f64) -> Result<bool, NodeError>,
) -> Result<String, Thrown> {
    if let Some(configured) = configured.filter(|c| !c.is_empty())
        && usable(state, configured, now_ms).map_err(|error| Thrown(error.message))?
    {
        state.fallback = None;
        return Ok(configured.to_owned());
    }
    let first = fallbacks.first();
    for dir in fallbacks {
        if !usable(state, dir, now_ms).map_err(|error| Thrown(error.message))? {
            continue;
        }
        state.fallback = if first.is_none() || Some(dir) == first {
            None
        } else {
            Some(SaveDirFallback {
                blocked: first.cloned().unwrap_or_default(),
                using: dir.clone(),
            })
        };
        return Ok(dir.clone());
    }
    Err(Thrown(format!(
        "default save dir not usable: {}",
        fallbacks.join(", ")
    )))
}

pub fn resolve_default_save_dir_authorized(
    state: &mut SaveDirState,
    configured: Option<&str>,
    fallbacks: &[String],
    now_ms: f64,
    admit: &mut crate::node_fs::FileAdmission<'_>,
) -> Result<String, Thrown> {
    resolve_default_save_dir_with(
        state,
        configured,
        fallbacks,
        now_ms,
        |state, dir, now_ms| {
            usable_now_with(state, dir, now_ms, || {
                is_usable_save_dir_authorized(dir, admit)
            })
        },
    )
}

pub fn resolve_default_save_dir_owned(
    state: &mut SaveDirState,
    configured: Option<&str>,
    fallbacks: &[String],
    now_ms: f64,
    admit: Admission,
) -> Result<String, Thrown> {
    resolve_default_save_dir_with(
        state,
        configured,
        fallbacks,
        now_ms,
        |state, dir, now_ms| {
            usable_now_with(state, dir, now_ms, || is_usable_save_dir_owned(dir, &admit))
        },
    )
}

/// `defaultSaveDirFallback()`
pub fn default_save_dir_fallback(state: &SaveDirState) -> Option<SaveDirFallback> {
    state.fallback.clone()
}

/// `pathOrNull(app, name)`
fn path_or_null(app: &dyn PathProvider, name: &str) -> Option<String> {
    app.get_path(name).ok().filter(|path| !path.is_empty())
}

/// `documentsDirOrFallback(app)`
pub fn documents_dir_or_fallback(app: &dyn PathProvider) -> Result<String, Thrown> {
    if let Some(documents) = path_or_null(app, "documents") {
        return Ok(documents);
    }
    if let Some(home) = path_or_null(app, "home") {
        return Ok(home);
    }
    app.get_path("userData")
}

/// `configuredDefaultSaveDir(app)`
pub fn configured_default_save_dir(
    app: &dyn PathProvider,
    state: &mut SaveDirState,
    now_ms: f64,
) -> Result<String, Thrown> {
    let (configured, fallbacks) = configured_save_inputs(app)?;
    resolve_default_save_dir(state, configured.as_deref(), &fallbacks, now_ms)
}

pub fn configured_default_save_dir_authorized(
    app: &dyn PathProvider,
    state: &mut SaveDirState,
    now_ms: f64,
    admit: &mut crate::node_fs::FileAdmission<'_>,
) -> Result<String, Thrown> {
    let (configured, fallbacks) = configured_save_inputs(app)?;
    resolve_default_save_dir_authorized(state, configured.as_deref(), &fallbacks, now_ms, admit)
}

pub fn configured_default_save_dir_owned(
    app: &dyn PathProvider,
    state: &mut SaveDirState,
    now_ms: f64,
    admit: Admission,
) -> Result<String, Thrown> {
    let (configured, fallbacks) = configured_save_inputs(app)?;
    resolve_default_save_dir_owned(state, configured.as_deref(), &fallbacks, now_ms, admit)
}

fn configured_save_inputs(app: &dyn PathProvider) -> Result<(Option<String>, Vec<String>), Thrown> {
    let settings_path = join(&[&app.get_path("userData")?, "app-settings.json"]);
    let documents = path_or_null(app, "documents");
    let home = path_or_null(app, "home");
    let mut fallbacks = Vec::new();
    if let Some(documents) = documents {
        fallbacks.push(join(&[&documents, PRODUCT_NAME]));
    }
    if let Some(home) = home {
        fallbacks.push(join(&[&home, PRODUCT_NAME]));
    }
    fallbacks.push(join(&[&app.get_path("userData")?, PRODUCT_NAME]));
    let configured = read_default_save_dir_setting(&settings_path);
    Ok((configured, fallbacks))
}

#[cfg(test)]
mod authorized_tests {
    use super::*;

    #[test]
    fn stale_worker_cache_never_overwrites_a_newer_folder_resolution() {
        let mut live = SaveDirState::new();
        let before = live.clone();
        let mut first = before.clone();
        first.fallback = Some(SaveDirFallback {
            blocked: "/documents".into(),
            using: "/home".into(),
        });
        first.unusable_until.insert("/documents".into(), 31_000.0);
        let mut second = before.clone();
        second.unusable_until.insert("/configured".into(), 32_000.0);
        assert!(live.publish_resolution(&before, second.clone()));
        assert!(!live.publish_resolution(&before, first));
        assert_eq!(live, second);
        let current = live.clone();
        let mut next = current.clone();
        next.unusable_until.clear();
        assert!(live.publish_resolution(&current, next));
        assert!(live.unusable_until.is_empty());
    }
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let prefix = std::env::temp_dir().join("save-dir-authorized-");
            Self(PathBuf::from(
                crate::node_fs::mkdtemp_random(&prefix.to_string_lossy()).unwrap(),
            ))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn expired() -> NodeError {
        NodeError {
            code: None,
            message: "owner expired".into(),
        }
    }

    #[test]
    fn owned_revision_admission_is_rechecked_for_every_actual_mkdir() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let scratch = Scratch::new();
        let target = scratch.0.join("first/second/third");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let admit: Admission = Arc::new(move |effect| {
            if observed.fetch_add(1, Ordering::SeqCst) != 0 {
                return Err(expired());
            }
            Ok(effect())
        });
        let error = create_dir_levels_owned(&target.to_string_lossy(), &admit).unwrap_err();
        assert_eq!(error.message, "owner expired");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(scratch.0.join("first").is_dir());
        assert!(!scratch.0.join("first/second").exists());
    }

    #[test]
    fn owned_admission_failure_never_becomes_an_unusable_folder_retry() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let scratch = Scratch::new();
        let configured = scratch.0.join("configured").to_string_lossy().into_owned();
        let fallback = scratch.0.join("fallback").to_string_lossy().into_owned();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let admit: Admission = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(expired())
        });
        let mut state = SaveDirState::new();
        let before = state.clone();
        let error = resolve_default_save_dir_owned(
            &mut state,
            Some(&configured),
            &[fallback.clone()],
            0.0,
            admit,
        )
        .unwrap_err();
        assert_eq!(error, Thrown("owner expired".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(state, before);
        assert!(!std::path::Path::new(&configured).exists());
        assert!(!std::path::Path::new(&fallback).exists());
    }

    #[test]
    fn owned_mkdir_io_failure_retains_the_original_fallback_policy() {
        use std::sync::Arc;
        let scratch = Scratch::new();
        let blocked = scratch.0.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let configured = blocked.join("child").to_string_lossy().into_owned();
        let fallback = scratch.0.join("fallback").to_string_lossy().into_owned();
        let admit: Admission = Arc::new(|effect| Ok(effect()));
        let mut state = SaveDirState::new();
        assert_eq!(
            resolve_default_save_dir_owned(
                &mut state,
                Some(&configured),
                &[fallback.clone()],
                0.0,
                admit
            )
            .unwrap(),
            fallback
        );
        assert!(state.unusable_until.contains_key(&resolve(&[&configured])));
        assert!(std::path::Path::new(&fallback).is_dir());
    }

    #[test]
    fn revoked_owner_stops_between_actual_directory_creations() {
        let scratch = Scratch::new();
        let target = scratch.0.join("first/second/third");
        let mut calls = 0;
        let result = create_dir_levels_authorized(&target.to_string_lossy(), &mut |effect| {
            calls += 1;
            if calls > 1 {
                return Err(expired());
            }
            effect();
            Ok(())
        });
        assert_eq!(result.unwrap_err().message, "owner expired");
        assert_eq!(calls, 2);
        assert!(scratch.0.join("first").is_dir());
        assert!(!scratch.0.join("first/second").exists());
    }

    #[test]
    fn lost_authority_does_not_try_or_cache_a_fallback_directory() {
        let scratch = Scratch::new();
        let configured = scratch.0.join("configured").to_string_lossy().into_owned();
        let fallback = scratch.0.join("fallback").to_string_lossy().into_owned();
        let mut state = SaveDirState::new();
        let mut calls = 0;
        let result = resolve_default_save_dir_authorized(
            &mut state,
            Some(&configured),
            &[fallback.clone()],
            0.0,
            &mut |_| {
                calls += 1;
                Err(expired())
            },
        );
        assert_eq!(result.unwrap_err(), Thrown("owner expired".into()));
        assert_eq!(calls, 1);
        assert!(!PathBuf::from(&configured).exists());
        assert!(!PathBuf::from(&fallback).exists());
        assert!(state.unusable_until.is_empty());
    }

    #[test]
    fn ordinary_mkdir_failure_retains_the_original_fallback_policy() {
        let scratch = Scratch::new();
        let blocked = scratch.0.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let configured = blocked.join("child").to_string_lossy().into_owned();
        let fallback = scratch
            .0
            .join("usable/child")
            .to_string_lossy()
            .into_owned();
        let mut state = SaveDirState::new();
        let path = resolve_default_save_dir_authorized(
            &mut state,
            Some(&configured),
            &[fallback.clone()],
            0.0,
            &mut |effect| {
                effect();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(path, fallback);
        assert!(PathBuf::from(path).is_dir());
        assert!(state.unusable_until.contains_key(&resolve(&[&configured])));
    }

    #[cfg(unix)]
    #[test]
    fn existing_parent_symlinks_remain_allowed() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("actual")).unwrap();
        std::os::unix::fs::symlink(scratch.0.join("actual"), scratch.0.join("alias")).unwrap();
        let path = scratch.0.join("alias/child");
        create_dir_levels_authorized(&path.to_string_lossy(), &mut |effect| {
            effect();
            Ok(())
        })
        .unwrap();
        assert!(scratch.0.join("actual/child").is_dir());
    }
}
