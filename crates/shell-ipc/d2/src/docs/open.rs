//! Opening a document: `prepareDocumentForOpen` (a legacy .doc converted to
//! a .docx copy first), `loadDocx` (password, size, recovery copy, lazy
//! pictures, grants), the channels that open (`docs:open`, `docs:open-path`,
//! `docs:open-decrypt`, `docs:consume-*`), `docs:recent`, the headless
//! export's two channels, and the shell's calls that send `docs:opened`.
//!
//! `loadDocx` runs in three parts: a worker reads, sizes, decrypts and
//! archives the file; the core thread shows the errors and the recovery
//! prompt (a dialog); a worker decrypts or checks the restored copy and
//! hands the bytes off; the core thread records the grants.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use jscompat::JsonValue;
use jscompat::node_path::host::{dirname, join, resolve};
use main_core::docs::doc_format::{
    DocFormat, doc_format_of_path, is_odt_package, recovery_ext_for,
};
use main_core::packages::electron_utils::legacy_import_prefs::{
    read_legacy_import_skip, write_legacy_import_skip,
};
use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision;
use main_core::packages::electron_utils::recovery_prompt::{
    RecoveryOpenResult, ResolveRecoveryOnOpenOptions, resolve_recovery_on_open,
};
use main_core::packages::electron_utils::recovery_store::RecoveryCopyState;
#[cfg(any(test, feature = "probe"))]
use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use shell_contract::hosts::MessageBox;
#[cfg(any(test, feature = "probe"))]
use shell_contract::ipc::Target;
use shell_contract::ipc::{Reply, Responder};
use shell_contract::{IpcValue, ViewId, WindowId};

#[cfg(any(test, feature = "probe"))]
use super::UntitledOpen;
use super::adapters::{HostMessageBox, Parent, picked_path};
use super::hosts::{DecryptError, basename};
use super::passwords::{OpenKind, docx_open_kind};
use super::recovery::file_identity;
use super::{Docs, MAX_OPEN_BYTES, Obj, js_filters, js_obj, js_str, read_json, sha256_hex, stat};
use crate::kit::{self, Entry, IntoIpc};

/// `LEGACY_DOC_LIMITS.maxBytes` of `packages/doc2docx`
const LEGACY_DOC_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// `ORIGINALS_MAX_BYTES`: the cap of the originals archive
const ORIGINALS_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// What `loadDocx` answers
pub(crate) enum Loaded {
    Null,
    NeedsPassword { path: String, name: String },
    Opened(IpcValue),
}

impl Loaded {
    fn into_ipc(self) -> IpcValue {
        match self {
            Loaded::Null => IpcValue::Null,
            Loaded::NeedsPassword { path, name } => Obj::new()
                .set("needsPassword", true)
                .set("path", path)
                .set("name", name)
                .done(),
            Loaded::Opened(v) => v,
        }
    }
}

/// What `loadDocx` throws: the decryption's refusal, or an `Error`'s message
pub(crate) enum LoadError {
    Decrypt(DecryptError),
    Thrown(String),
}

impl LoadError {
    fn message(&self) -> &str {
        match self {
            LoadError::Decrypt(e) => &e.message,
            LoadError::Thrown(m) => m,
        }
    }
}

type LoadThen = Box<dyn FnOnce(Result<Loaded, LoadError>)>;
type PrepareThen = Box<dyn FnOnce(Option<(String, Option<RecoveryLoadScope>)>)>;
type PublishedFile = Result<(String, std::sync::Arc<std::fs::File>), String>;
type PreparedRecovery = Result<
    (
        Option<Vec<u8>>,
        Option<main_core::packages::electron_utils::recovery_store::RecoveryMetadataPlan>,
    ),
    String,
>;

#[derive(Clone)]
pub(crate) struct LoadAuthority {
    pub(super) owner: shell_contract::ipc::NativeRequestOwner,
    pub(super) cancel: shell_contract::runtime::CancelToken,
}

pub(super) enum PreparedRecoveryCopy {
    Original,
    Restore(
        Option<Box<main_core::packages::electron_utils::recovery_store::RecoveryPinnedSnapshot>>,
    ),
}

#[derive(Clone)]
pub(crate) struct RecoveryLoadScope {
    pub(super) receipt: shell_contract::ipc::RecoverySeedReceipt,
    pub(super) snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    pub(super) owner: shell_contract::ipc::NativeRequestOwner,
    pub(super) cancel: shell_contract::runtime::CancelToken,
    pub(super) effects: shell_contract::hosts::OwnedEffectsHost,
    pub(super) permit:
        std::sync::Arc<std::sync::Mutex<Option<main_core::docs::byte_handoff::MediaPermit>>>,
    pub(super) original_permit: main_core::docs::byte_handoff::MediaPermit,
    pub(super) prepared_copy: std::sync::Arc<std::sync::Mutex<Option<PreparedRecoveryCopy>>>,
    pub(super) source_file: Option<std::sync::Arc<std::fs::File>>,
    pub(super) source_path: Option<String>,
    pub(super) work: Option<shell_contract::ipc::RecoveryWorkTicket>,
}

impl RecoveryLoadScope {
    fn effect<T>(&self, action: impl FnOnce() -> T) -> Result<T, String> {
        let mut action = Some(action);
        let mut value = None;
        self.effects.with_recovery_open_snapshot(
            &self.receipt,
            &self.snapshot,
            &self.owner,
            &[],
            &self.cancel,
            &self.cancel,
            &mut || {
                value = Some(action
                    .take()
                    .ok_or("the recovery open effect was admitted twice")?(
                ));
                Ok(())
            },
        )?;
        value.ok_or_else(|| "the recovery open effect was not admitted".into())
    }

    fn admission(&self) -> main_core::packages::electron_utils::atomic_write::owned::Admission {
        let scope = self.clone();
        std::sync::Arc::new(move |effect| {
            scope
                .effect(effect)
                .map_err(|message| main_core::node_fs::NodeError {
                    code: None,
                    message,
                })
        })
    }

    fn file_effect(&self, effect: &mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(effect)
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })
    }

    fn asset_effect(
        &self,
        effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(effect)
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })?
    }
}

/// The first part of `loadDocx`, on a worker
enum Read {
    Null,
    TooLarge,
    Damaged,
    NeedsPassword,
    Plain {
        encrypted: bool,
        plain: Vec<u8>,
        hash: String,
    },
}

/// `isWord97Path(filePath)`
fn is_word97_path(path: &str) -> bool {
    jscompat::js_regex!(r"\.(doc|dot)$", "i").test(path)
}

/// Read a captured file without changing the descriptor's shared cursor. The
/// same source can therefore be used again after a password prompt.
fn read_pinned_source(
    file: &std::fs::File,
    path: &str,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;

    let mut result = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    loop {
        if cancel.is_cancelled() {
            return Err("the document open was cancelled".into());
        }
        #[cfg(unix)]
        let count = file.read_at(&mut buffer, offset);
        #[cfg(windows)]
        let count = file.seek_read(&mut buffer, offset);
        let count = match count {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(super::fs_error(&error, "read", path).message),
        };
        if count == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..count]);
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| "document size overflow".to_owned())?;
    }
    Ok(result)
}

static PRUNE_RUNNING: AtomicBool = AtomicBool::new(false);

/// `archiveOriginal(filePath, hash, size)` and its `pruneOriginals`
fn archive_original(user_data: &str, path: &str, hash: &str, size: u64) -> Result<(), String> {
    if size > ORIGINALS_MAX_BYTES {
        return Ok(());
    }
    let dir = join(&[user_data, "originals"]);
    std::fs::create_dir_all(&dir).map_err(|e| super::fs_error(&e, "mkdir", &dir).message)?;
    let target = join(&[&dir, &format!("{hash}{}", recovery_ext_for(path))]);
    if !std::path::Path::new(&target).exists() {
        main_core::node_fs::copy_file(path, &target, false).map_err(|e| e.message)?;
    }
    prune_originals(&dir);
    Ok(())
}

fn archive_original_recovery(
    user_data: &str,
    path: &str,
    hash: &str,
    size: u64,
    bytes: &[u8],
    scope: &RecoveryLoadScope,
) -> Result<(), String> {
    if size > ORIGINALS_MAX_BYTES {
        return scope.effect(|| ());
    }
    let dir = join(&[user_data, "originals"]);
    let target = join(&[&dir, &format!("{hash}{}", recovery_ext_for(path))]);
    let admission = scope.admission();
    main_core::packages::electron_utils::atomic_write::owned::create_parent(&target, &admission)
        .map_err(|error| error.message)?;
    if !std::path::Path::new(&target).exists() {
        let output = scope
            .effect(|| main_core::html::asset_lifecycle::CapturedAssetOutput::capture(&target))?
            .map_err(|error| error.message)?;
        let mut payload = output
            .create_payload(&mut |effect| scope.asset_effect(effect))
            .map_err(|error| error.message)?;
        payload.write(bytes).map_err(|error| error.message)?;
        payload
            .publish(&output, true, &mut |effect| scope.asset_effect(effect))
            .map_err(|error| error.message)?;
    }
    prune_originals_using(&dir, Some(scope))
}

fn prune_originals(dir: &str) {
    let _ = prune_originals_using(dir, None);
}

fn prune_originals_using(dir: &str, scope: Option<&RecoveryLoadScope>) -> Result<(), String> {
    if PRUNE_RUNNING.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PRUNE_RUNNING.store(false, Ordering::Release);
        }
    }
    let _reset = Reset;
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file().then(|| {
                    (
                        e.path(),
                        meta.len(),
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    )
                })
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.2);
        for (p, size, _) in files {
            if total <= ORIGINALS_MAX_BYTES {
                break;
            }
            let removed = match scope {
                Some(scope) => scope.effect(|| std::fs::remove_file(&p).is_ok())?,
                None => std::fs::remove_file(&p).is_ok(),
            };
            if removed {
                total -= size;
            }
        }
    }
    Ok(())
}

/// `canonicalPath(path)`: through symlinks; a missing name through its folder
fn canonical_path(path: &str) -> String {
    let absolute = resolve(&[path]);
    if let Ok(real) = std::fs::canonicalize(&absolute) {
        return real.to_string_lossy().into_owned();
    }
    match std::fs::canonicalize(dirname(&absolute)) {
        Ok(dir) => join(&[&dir.to_string_lossy(), &basename(&absolute)]),
        Err(_) => absolute,
    }
}

/// `sameFilePath(a, b)`: case is ignored on every platform
fn same_file_path(a: &str, b: &str) -> bool {
    canonical_path(a).to_lowercase() == canonical_path(b).to_lowercase()
}

impl Docs {
    fn remember_legacy_import_skip_recovery(
        &self,
        settings: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let queue = main_core::shell::app_settings::queue::for_path(&settings);
        let back = kit::back_on_core(&self.services, move |(result, completion)| {
            let _completion: main_core::shell::app_settings::queue::Completion = completion;
            then(result);
        });
        queue.submit(move |completion| {
            let _work = scope.work.clone();
            let mut rejected = None;
            let result = main_core::packages::electron_utils::legacy_import_prefs::write_legacy_import_skip_authorized(
                &settings, "word97", true, &mut |effect| {
                    scope.file_effect(effect).inspect_err(|error| {
                        rejected = Some(error.message.clone());
                    })
                },
            );
            // Ordinary write failures were ignored; authority loss must stop.
            let result = match rejected { Some(error) => Err(error), None => { let _ = result; Ok(()) } };
            back((result, completion));
        });
    }

    fn convert_legacy_recovery(
        &self,
        path: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(Result<Vec<u8>, String>)>,
    ) {
        let engine = self.engine.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = scope.work.clone();
                if scope.source_path.as_deref() != Some(path.as_str()) {
                    return Err("the recovery conversion source changed".into());
                }
                let file = scope
                    .source_file
                    .as_ref()
                    .ok_or("the recovery conversion source is unavailable")?;
                scope.effect(|| ())?;
                let bytes = read_pinned_source(file, &path, &scope.cancel)?;
                let output = engine.convert_legacy_doc_recovery(
                    bytes,
                    scope.receipt.clone(),
                    scope.owner.clone(),
                    scope.cancel.clone(),
                )?;
                scope.effect(|| ())?;
                Ok(output)
            },
            then,
        );
    }

    fn publish_legacy_recovery(
        &self,
        target: shell_contract::ipc::RecoveryCommittedDestination,
        bytes: Vec<u8>,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(PublishedFile)>,
    ) {
        kit::on_worker(
            &self.services,
            move || {
                let _work = scope.work.clone();
                if !scope.receipt.matches(target.receipt().target())
                    || !target.snapshot().same_snapshot(&scope.snapshot)
                {
                    return Err("the recovery conversion destination changed".into());
                }
                let path = target.path().to_string_lossy().into_owned();
                let output = scope
                    .effect(|| {
                        main_core::html::asset_lifecycle::CapturedAssetOutput::capture(&path)
                    })?
                    .map_err(|error| error.message)?;
                let mut payload = output
                    .create_payload(&mut |effect| scope.asset_effect(effect))
                    .map_err(|error| error.message)?;
                payload.write(&bytes).map_err(|error| error.message)?;
                let file = payload
                    .publish_with_file(&output, true, &mut |effect| scope.asset_effect(effect))
                    .map_err(|error| error.message)?;
                Ok((path, std::sync::Arc::new(file)))
            },
            then,
        );
    }

    fn default_save_dir_recovery(
        &self,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(Result<String, String>)>,
    ) {
        let paths = self.services.paths.clone();
        let state = self.save_dir.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = scope.work.clone();
                loop {
                    let snapshot = state
                        .lock()
                        .map_err(|_| "the save directory state is unavailable")?
                        .clone();
                    let mut proposed = snapshot.clone();
                    let rejected = std::sync::Arc::new(AtomicBool::new(false));
                    let marker = rejected.clone();
                    let admission = scope.admission();
                    let checked: main_core::packages::electron_utils::atomic_write::owned::Admission =
                    std::sync::Arc::new(move |effect| admission(effect).inspect_err(|_| {
                        marker.store(true, Ordering::Release);
                    }));
                    let result = main_core::packages::electron_utils::default_save_dir::configured_default_save_dir_owned(
                    &super::adapters::Paths(paths.as_ref()), &mut proposed, super::recovery_now_ms(), checked,
                );
                    if rejected.load(Ordering::Acquire) {
                        return Err("the recovery save directory admission was revoked".into());
                    }
                    let committed = scope.effect(|| {
                        let Ok(mut current) = state.try_lock() else {
                            return false;
                        };
                        if *current != snapshot {
                            return false;
                        }
                        *current = proposed;
                        true
                    })?;
                    if committed {
                        return Ok(result.unwrap_or_default());
                    }
                }
            },
            then,
        );
    }

    fn push_recent_recovery(
        &self,
        path: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        if self.shell.is_headless() {
            return then(scope.effect(|| false));
        }
        let recent_path = self.recent_path();
        let io_path = recent_path.clone();
        let back = kit::back_on_core(
            &self.services,
            move |result: Result<Result<bool, String>, main_core::node_fs::NodeError>| {
                then(
                    result
                        .map_err(|error| error.message)
                        .and_then(|value| value),
                );
            },
        );
        main_core::shell::recent_files::metadata::submit(
            &recent_path,
            move || {
                let _work = scope.work.clone();
                let recent = match read_json(&io_path) {
                    Some(JsonValue::Array(items)) => items,
                    _ => Vec::new(),
                };
                if recent.first() == Some(&js_str(&path)) {
                    return Ok(scope.effect(|| false));
                }
                let mut next = vec![js_str(&path)];
                next.extend(recent.into_iter().filter(|item| *item != js_str(&path)));
                next.truncate(100);
                let mut denied = None;
                let result =
                    main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                        &io_path,
                        &JsonValue::Array(next),
                        &mut |effect| {
                            scope.file_effect(effect).inspect_err(|error| {
                                denied = Some(error.message.clone());
                            })
                        },
                    );
                if let Some(error) = denied {
                    return Ok(Err(error));
                }
                // As in pushRecent, filesystem failures still request a menu refresh.
                let _ = result;
                Ok(scope.effect(|| true))
            },
            back,
        );
    }

    fn legacy_error_recovery(
        &self,
        parent: Option<WindowId>,
        detail: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce()>,
    ) {
        let back = kit::back_on_core(&self.services, move |_| then());
        self.dialogs.message_recovery_owned(
            parent,
            MessageBox {
                kind: Some("error".into()),
                message: self.tm("dlgOpenDoc"),
                detail: Some(detail),
                ..Default::default()
            },
            scope.receipt,
            scope.snapshot,
            scope.owner,
            scope.cancel.clone(),
            scope.cancel,
            back,
        );
    }

    fn prepare_legacy_recovery(
        self: &Rc<Self>,
        path: String,
        parent: Option<WindowId>,
        scope: RecoveryLoadScope,
        then: PrepareThen,
    ) {
        let settings = self.user_data(&["app-settings.json"]);
        let worker_settings = settings.clone();
        let worker_scope = scope.clone();
        let worker_path = path.clone();
        let docs = self.clone();
        kit::on_worker(
            &self.services,
            move || -> Result<(bool, bool), String> {
                let _work = worker_scope.work.clone();
                if worker_scope.source_path.as_deref() != Some(worker_path.as_str()) {
                    return Err("the recovery conversion source changed".into());
                }
                let file = worker_scope
                    .source_file
                    .as_ref()
                    .ok_or("the recovery conversion source is unavailable")?;
                worker_scope.effect(|| ())?;
                let size = file.metadata().map_err(|error| error.to_string())?.len();
                Ok((
                    size > LEGACY_DOC_MAX_BYTES,
                    read_legacy_import_skip(&worker_settings, "word97"),
                ))
            },
            move |result| {
                let (too_large, skip) = match result {
                    Ok(value) => value,
                    Err(_) => return then(None),
                };
                if too_large {
                    let detail = format!(
                        "{}: {}",
                        basename(&path),
                        docs.tm_with(
                            "errTooLarge",
                            &[("mb", &(LEGACY_DOC_MAX_BYTES / 1024 / 1024).to_string())]
                        )
                    );
                    return docs.legacy_error_recovery(
                        parent,
                        detail,
                        scope,
                        Box::new(move || then(None)),
                    );
                }
                let extension = path[path.len().saturating_sub(3)..].to_lowercase();
                let after_docs = docs.clone();
                let after_scope = scope.clone();
                let proceed: Box<dyn FnOnce(bool)> = Box::new(move |accepted| {
                    if !accepted {
                        return then(None);
                    }
                    let suggested = join(&[
                        &dirname(&path),
                        &jscompat::js_regex!(r"\.(doc|dot)$", "i")
                            .replace_all(&basename(&path), |_| ".docx".to_owned()),
                    ]);
                    after_docs.pick_legacy_recovery(path, suggested, parent, after_scope, then);
                });
                if skip {
                    return proceed(true);
                }
                let options = MessageBox {
                    kind: Some("warning".into()),
                    message: docs.tm_with("docImportMsg", &[("ext", &extension)]),
                    detail: Some(docs.tm("docImportDetail")),
                    buttons: vec![docs.tm("docImportConvertBtn"), docs.tm("btnCancel")],
                    default_id: Some(0),
                    cancel_id: Some(1),
                    checkbox_label: Some(docs.tm("importSkipConfirm")),
                    ..Default::default()
                };
                let answer_docs = docs.clone();
                let answer_scope = scope.clone();
                let back = kit::back_on_core(
                    &docs.services,
                    move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                        let Ok(answer) = answer else {
                            return proceed(false);
                        };
                        if answer.response != 0 {
                            return proceed(false);
                        }
                        if answer.checkbox_checked {
                            answer_docs.remember_legacy_import_skip_recovery(
                                settings,
                                answer_scope,
                                Box::new(move |result| proceed(result.is_ok())),
                            );
                        } else {
                            proceed(true);
                        }
                    },
                );
                docs.dialogs.message_recovery_owned(
                    parent,
                    options,
                    scope.receipt,
                    scope.snapshot,
                    scope.owner,
                    scope.cancel.clone(),
                    scope.cancel,
                    back,
                );
            },
        );
    }

    fn pick_legacy_recovery(
        self: &Rc<Self>,
        source: String,
        suggested: String,
        parent: Option<WindowId>,
        scope: RecoveryLoadScope,
        then: PrepareThen,
    ) {
        let docs = self.clone();
        let saved_scope = scope.clone();
        self.default_save_dir_recovery(scope, Box::new(move |fallback| {
            let fallback = match fallback { Ok(value) => value, Err(_) => return then(None) };
            let memory = docs.memory.clone();
            let worker_scope = saved_scope.clone();
            let title = docs.tm("dlgSaveAs");
            let filter = docs.tm("filterWord");
            let worker_suggested = suggested.clone();
            let ready = docs.clone();
            kit::on_worker(&docs.services, move || -> Result<shell_contract::hosts::FileDialog, String> {
                let _work = worker_scope.work.clone();
                let memory = memory.lock().map_err(|_| "dialog memory is unavailable")?.clone();
                worker_scope.effect(|| ())?;
                let options = js_obj(vec![
                    ("title", js_str(title)), ("defaultPath", js_str(worker_suggested)),
                    ("filters", js_filters(&[(filter, vec!["docx"])])),
                ]);
                let fallback = main_core::packages::electron_utils::dialog_memory::FallbackDir::Dir(&fallback);
                let options = main_core::packages::electron_utils::dialog_memory::prepare_dialog_options(
                    &memory, &options, Some(&fallback),
                );
                Ok(super::adapters::file_dialog(&options))
            }, move |options| {
                let options = match options { Ok(value) => value, Err(_) => return then(None) };
                let answer_docs = ready.clone();
                let answer_scope = saved_scope.clone();
                let back = kit::back_on_core(&ready.services, move |result: Result<Option<shell_contract::ipc::RecoveryCommittedDestination>, String>| {
                    let target = match result { Ok(Some(target)) => target, _ => return then(None) };
                    let chosen = target.path().to_string_lossy().into_owned();
                    let scope = answer_scope;
                    let memory = answer_docs.memory.clone();
                    let worker_scope = scope.clone();
                    let worker_source = source.clone();
                    let worker_chosen = chosen.clone();
                    let checked_docs = answer_docs.clone();
                    kit::on_worker(&answer_docs.services, move || -> Result<(bool, bool), String> {
                        let _work = worker_scope.work.clone();
                        loop {
                            let remembered = worker_scope.effect(|| {
                                let Ok(mut memory) = memory.try_lock() else { return false; };
                                main_core::packages::electron_utils::dialog_memory::remember_save_path(&mut memory, &worker_chosen);
                                true
                            })?;
                            if remembered { break; }
                            std::thread::yield_now();
                        }
                        let same = same_file_path(&worker_chosen, &worker_source);
                        let needs_suffix = doc_format_of_path(&worker_chosen) != Some(DocFormat::Docx);
                        let appended_exists = needs_suffix && std::path::Path::new(&format!("{worker_chosen}.docx")).exists();
                        Ok((same, appended_exists))
                    }, move |checked| {
                        let (same, retry) = match checked { Ok(value) => value, Err(_) => return then(None) };
                        if same {
                            let retry_docs = checked_docs.clone();
                            let retry_scope = scope.clone();
                            let detail = checked_docs.tm("docImportOriginalTarget");
                            return checked_docs.legacy_error_recovery(parent, detail, scope, Box::new(move || {
                                retry_docs.pick_legacy_recovery(source, suggested, parent, retry_scope, then);
                            }));
                        }
                        if retry {
                            return checked_docs.pick_legacy_recovery(source, format!("{chosen}.docx"), parent, scope, then);
                        }
                        let target = target.for_docs_docx();
                        let publish_docs = checked_docs.clone();
                        let publish_scope = scope.clone();
                        checked_docs.convert_legacy_recovery(source.clone(), scope, Box::new(move |converted| {
                            let bytes = match converted {
                                Ok(value) => value,
                                Err(_) if parent.is_some() => {
                                    let detail = format!("{}: {}", basename(&source), publish_docs.tm("errUnreadable"));
                                    return publish_docs.legacy_error_recovery(parent, detail, publish_scope, Box::new(move || then(None)));
                                }
                                Err(_) => return then(None),
                            };
                            let complete_docs = publish_docs.clone();
                            let final_scope = publish_scope.clone();
                            publish_docs.publish_legacy_recovery(target, bytes, publish_scope, Box::new(move |result| {
                                match result {
                                    Ok((path, file)) => {
                                        let mut scope = final_scope;
                                        scope.source_path = Some(path.clone());
                                        scope.source_file = Some(file);
                                        then(Some((path, Some(scope))));
                                    }
                                    Err(_) if parent.is_some() => {
                                        let detail = format!("{}: {}", basename(&source), complete_docs.tm("errUnreadable"));
                                        complete_docs.legacy_error_recovery(parent, detail, final_scope, Box::new(move || then(None)));
                                    }
                                    Err(_) => then(None),
                                }
                            }));
                        }));
                    });
                });
                ready.dialogs.save_recovery_owned(parent, options, saved_scope.receipt, saved_scope.snapshot, saved_scope.owner,
                    saved_scope.cancel.clone(), saved_scope.cancel, back);
            });
        }));
    }

    /// `prepareDocumentForOpen(filePath, parent)`: `then(None)` cancels the open
    pub(crate) fn prepare_for_open(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        parent: Option<WindowId>,
        recovery: Option<RecoveryLoadScope>,
        then: PrepareThen,
    ) {
        if !is_word97_path(&path) {
            return then(Some((path, recovery)));
        }
        if let Some(scope) = recovery {
            return self.prepare_legacy_recovery(path, parent, scope, then);
        }
        if let Ok((_, size)) = stat(&path)
            && size as u64 > LEGACY_DOC_MAX_BYTES
        {
            let mb = (LEGACY_DOC_MAX_BYTES / 1024 / 1024).to_string();
            let detail = format!(
                "{}: {}",
                basename(&path),
                self.tm_with("errTooLarge", &[("mb", &mb)])
            );
            self.show_open_error(parent, detail);
            return then(None);
        }
        let extension = path[path.len().saturating_sub(3)..].to_lowercase();
        let settings = self.user_data(&["app-settings.json"]);
        if !read_legacy_import_skip(&settings, "word97") {
            let answer = self.dialogs.message(
                parent,
                &MessageBox {
                    kind: Some("warning".into()),
                    message: self.tm_with("docImportMsg", &[("ext", &extension)]),
                    detail: Some(self.tm("docImportDetail")),
                    buttons: vec![self.tm("docImportConvertBtn"), self.tm("btnCancel")],
                    default_id: Some(0),
                    cancel_id: Some(1),
                    checkbox_label: Some(self.tm("importSkipConfirm")),
                    ..Default::default()
                },
            );
            if answer.response != 0 {
                return then(None);
            }
            if answer.checkbox_checked {
                // unwritable settings: ask again next time
                let _ = write_legacy_import_skip(&settings, "word97", true);
            }
        }
        let Some(target) = self.pick_legacy_import_target(view, &path, parent) else {
            return then(None);
        };
        let engine = self.engine.clone();
        let docs = Rc::clone(self);
        let (source, dest) = (path.clone(), target.clone());
        kit::on_worker(
            &self.services,
            move || -> Result<(), String> {
                let bytes = std::fs::read(&source).map_err(|e| e.to_string())?;
                let docx = engine.convert_legacy_doc(bytes)?;
                main_core::docs::atomic_write::atomic_write_file(&dest, &docx)
                    .map_err(|e| e.message)
            },
            move |result| match result {
                Ok(()) => then(Some((target, None))),
                Err(_) => {
                    if parent.is_some() {
                        let detail = format!("{}: {}", basename(&path), docs.tm("errUnreadable"));
                        docs.show_open_error(parent, detail);
                    }
                    then(None)
                }
            },
        );
    }

    /// `pickLegacyImportTarget(sourcePath, parent)`
    fn pick_legacy_import_target(
        &self,
        view: ViewId,
        source: &str,
        parent: Option<WindowId>,
    ) -> Option<String> {
        let mut default_path = join(&[
            &dirname(source),
            &jscompat::js_regex!(r"\.(doc|dot)$", "i")
                .replace_all(&basename(source), |_| ".docx".to_owned()),
        ]);
        loop {
            let result = self.save_dialog_in_default_dir(
                view,
                js_obj(vec![
                    ("title", js_str(self.tm("dlgSaveAs"))),
                    ("defaultPath", js_str(default_path.clone())),
                    (
                        "filters",
                        js_filters(&[(self.tm("filterWord"), vec!["docx"])]),
                    ),
                ]),
            );
            let picked = picked_path(&result, "filePath")?;
            if same_file_path(&picked, source) {
                self.show_open_error(parent, self.tm("docImportOriginalTarget"));
                continue;
            }
            if doc_format_of_path(&picked) == Some(DocFormat::Docx) {
                return Some(picked);
            }
            let target = format!("{picked}.docx");
            if !std::path::Path::new(&target).exists() {
                return Some(target);
            }
            // offered under its full name, replacing the file there is the user's answer
            default_path = target;
        }
    }

    /// `loadDocx(filePath, wcId, password)`
    pub(crate) fn load_docx(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        password: Option<String>,
        recovery: Option<RecoveryLoadScope>,
        authority: LoadAuthority,
        then: LoadThen,
    ) {
        let Some(format) = doc_format_of_path(&path) else {
            return then(Ok(Loaded::Null));
        };
        let Some(lease) = self
            .media_views
            .borrow()
            .get(&view)
            .map(|slot| slot.lease.clone())
        else {
            return then(Err(LoadError::Thrown(
                "The document media view is not registered.".into(),
            )));
        };
        let Some(old) = lease.current() else {
            return then(Err(LoadError::Thrown(
                "The document media session is closed.".into(),
            )));
        };
        if recovery
            .as_ref()
            .is_some_and(|scope| !old.same_session(&scope.original_permit))
        {
            return then(Err(LoadError::Thrown(
                "The recovery document was replaced.".into(),
            )));
        }
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return then(Err(LoadError::Thrown(
                "Native document effects are unavailable.".into(),
            )));
        };
        let candidate = match lease.prepare_document() {
            Ok(candidate) => candidate,
            Err(error) => return then(Err(LoadError::Thrown(error))),
        };
        let prior = lease.pending();
        let stage = super::media_transition::DocsMediaStageAdapter {
            lease: lease.clone(),
            old: old.clone(),
            prior: prior.clone(),
            candidate: candidate.clone(),
        };
        if let Err(error) = effects.stage_docs_media(
            &authority.owner,
            &[],
            &authority.cancel,
            &authority.cancel,
            &stage,
            recovery.as_ref().map(|scope| &scope.receipt),
        ) {
            candidate.permit().revoke();
            return then(Err(LoadError::Thrown(error)));
        }
        if let Some(prior) = prior {
            let _ = self.engine.forget_lazy_media_owner(prior);
        }
        let permit = candidate.permit().clone();
        let worker_permit = permit.clone();
        let worker_cancel = authority.cancel.clone();
        let password = password.or_else(|| self.passwords.borrow().password_for(view, Some(&path)));
        let engine = self.engine.clone();
        let headless = self.shell.is_headless();
        let user_data = self.user_data(&[]);
        let docs = Rc::clone(self);
        let source = path.clone();
        let pw = password.clone();
        let worker_recovery = recovery.clone();
        kit::on_worker(
            &self.services,
            move || -> Result<Read, LoadError> {
                let _work = worker_recovery
                    .as_ref()
                    .and_then(|scope| scope.work.clone());
                let retained = match worker_recovery.as_ref() {
                    Some(scope)
                        if matches!(format, DocFormat::Odt | DocFormat::Docx)
                            && scope.source_path.as_deref() == Some(source.as_str()) =>
                    {
                        Some(scope.source_file.clone().ok_or_else(|| {
                            LoadError::Thrown("The captured document source is unavailable".into())
                        })?)
                    }
                    _ => None,
                };
                let pinned = retained.as_deref();
                if pinned.is_none() && !std::path::Path::new(&source).exists() {
                    return Ok(Read::Null);
                }
                let size = match pinned {
                    Some(file) => file
                        .metadata()
                        .map_err(|error| {
                            LoadError::Thrown(super::fs_error(&error, "stat", &source).message)
                        })?
                        .len(),
                    None => stat(&source).map_err(|e| LoadError::Thrown(e.message))?.1 as u64,
                };
                let lazy = if format == DocFormat::Docx {
                    engine
                        .open_lazy_docx(&source, retained.clone(), worker_permit, worker_cancel)
                        .map_err(LoadError::Thrown)?
                } else {
                    None
                };
                if lazy.as_ref().map_or(size, |l| l.bytes.len() as u64) > MAX_OPEN_BYTES {
                    return Ok(Read::TooLarge);
                }
                let (original, lazy_hash) = match lazy {
                    Some(l) => (l.bytes, Some(l.hash)),
                    None => {
                        let bytes = match pinned {
                            Some(file) => read_pinned_source(
                                file,
                                &source,
                                &worker_recovery
                                    .as_ref()
                                    .expect("retained recovery source")
                                    .cancel,
                            )
                            .map_err(LoadError::Thrown)?,
                            None => std::fs::read(&source).map_err(|e| {
                                LoadError::Thrown(super::fs_error(&e, "open", &source).message)
                            })?,
                        };
                        (bytes, None)
                    }
                };
                let kind = if format == DocFormat::Docx {
                    docx_open_kind(&original)
                } else {
                    OpenKind::Plain
                };
                if kind == OpenKind::Damaged {
                    return Ok(Read::Damaged);
                }
                let encrypted = kind == OpenKind::Encrypted;
                let hash = lazy_hash.unwrap_or_else(|| sha256_hex(&original));
                let (plain, encrypted_original) = if encrypted {
                    let Some(pw) = pw else {
                        return Ok(Read::NeedsPassword);
                    };
                    let plain = engine
                        .decrypt_docx(&original, &pw)
                        .map_err(LoadError::Decrypt)?;
                    (plain, Some(original))
                } else {
                    (original, None)
                };
                if !headless {
                    match &worker_recovery {
                        Some(scope) => archive_original_recovery(
                            &user_data,
                            &source,
                            &hash,
                            size,
                            encrypted_original.as_deref().unwrap_or(&plain),
                            scope,
                        ),
                        None => archive_original(&user_data, &source, &hash, size),
                    }
                    .map_err(LoadError::Thrown)?;
                }
                Ok(Read::Plain {
                    encrypted,
                    plain,
                    hash,
                })
            },
            move |read| {
                if !permit.is_active() {
                    return then(Err(LoadError::Thrown(
                        "The document media session is closed.".into(),
                    )));
                }
                if !matches!(&read, Ok(Read::Plain { .. })) {
                    docs.abort_media_document(view, &permit);
                }
                match read {
                    Err(e) => then(Err(e)),
                    Ok(Read::Null) => then(Ok(Loaded::Null)),
                    Ok(Read::TooLarge) => {
                        let mb = (MAX_OPEN_BYTES / 1024 / 1024).to_string();
                        let detail = format!(
                            "{}: {}",
                            basename(&path),
                            docs.tm_with("errTooLarge", &[("mb", &mb)])
                        );
                        if let Some(scope) = recovery {
                            docs.legacy_error_recovery(
                                docs.parent(view),
                                detail,
                                scope,
                                Box::new(move || then(Ok(Loaded::Null))),
                            )
                        } else {
                            docs.show_open_error(docs.parent(view), detail);
                            then(Ok(Loaded::Null))
                        }
                    }
                    Ok(Read::Damaged) => {
                        let detail = format!("{}: {}", basename(&path), docs.tm("errUnreadable"));
                        if let Some(scope) = recovery {
                            docs.legacy_error_recovery(
                                docs.parent(view),
                                detail,
                                scope,
                                Box::new(move || then(Ok(Loaded::Null))),
                            )
                        } else {
                            docs.show_open_error(docs.parent(view), detail);
                            then(Ok(Loaded::Null))
                        }
                    }
                    Ok(Read::NeedsPassword) => then(Ok(Loaded::NeedsPassword {
                        name: basename(&path),
                        path,
                    })),
                    Ok(Read::Plain {
                        encrypted,
                        plain,
                        hash,
                    }) => {
                        let disk_password = if encrypted { password } else { None };
                        if let Some(scope) = recovery {
                            if let Err(error) = scope.effect(|| {
                                docs.passwords.borrow_mut().remember(
                                    view,
                                    &path,
                                    disk_password.as_deref(),
                                );
                            }) {
                                docs.abort_media_document(view, &permit);
                                return then(Err(LoadError::Thrown(error)));
                            }
                            let finished = docs.clone();
                            let resolved_path = path.clone();
                            let finishing_scope = scope.clone();
                            docs.resolve_prepared_recovery(
                                view,
                                path.clone(),
                                scope,
                                Box::new(move |result| match result {
                                    Err(error) => {
                                        finished.abort_media_document(view, &permit);
                                        then(Err(LoadError::Thrown(error)));
                                    }
                                    Ok((restored, later)) => finished.finish_load(
                                        view,
                                        resolved_path,
                                        format,
                                        encrypted,
                                        plain,
                                        hash,
                                        restored,
                                        disk_password,
                                        permit,
                                        Some(finishing_scope),
                                        later,
                                        lease,
                                        old,
                                        authority,
                                        then,
                                    ),
                                }),
                            );
                        } else {
                            docs.passwords.borrow_mut().remember(
                                view,
                                &path,
                                disk_password.as_deref(),
                            );
                            match docs.resolve_open(view, &path) {
                                Err(e) => {
                                    docs.abort_media_document(view, &permit);
                                    then(Err(LoadError::Thrown(e)))
                                }
                                Ok(restored) => docs.finish_load(
                                    view,
                                    path,
                                    format,
                                    encrypted,
                                    plain,
                                    hash,
                                    restored,
                                    disk_password,
                                    permit,
                                    None,
                                    None,
                                    lease,
                                    old,
                                    authority,
                                    then,
                                ),
                            }
                        }
                    }
                }
            },
        );
    }

    fn resolve_prepared_recovery(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(PreparedRecovery)>,
    ) {
        let selected = scope.effect(|| {
            let decision = self
                .open_decisions
                .get(view)
                .filter(|(selected, _)| selected == &path)
                .map(|(_, decision)| decision);
            if decision.is_some() {
                self.open_decisions.remove(view);
            }
            decision
        });
        let decision = match selected {
            Ok(Some(decision)) => decision,
            Ok(None) => {
                let converted = scope.effect(|| {
                    scope.source_path.as_deref() == Some(path.as_str())
                        && self
                            .open_decisions
                            .get(view)
                            .is_some_and(|(source, _)| is_word97_path(&source))
                });
                return match converted {
                    Ok(true) => self.resolve_converted_recovery(view, path, scope, then),
                    Ok(false) => then(Err("the recovery open decision changed".into())),
                    Err(error) => then(Err(error)),
                };
            }
            Err(error) => return then(Err(error)),
        };
        let shared = self.recovery_shared.clone();
        let root = self.user_data(&[]);
        let shell = self.shell.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = scope.work.clone();
                let mut slot = scope
                    .prepared_copy
                    .lock()
                    .map_err(|_| "the recovery copy lock was poisoned".to_owned())?;
                let prepared = scope.effect(|| {
                    slot.take()
                        .ok_or_else(|| "the recovery copy was already consumed".to_owned())
                })??;
                drop(slot);
                if !matches!(
                    (&prepared, decision),
                    (
                        PreparedRecoveryCopy::Original,
                        RecoveryOpenDecision::Original
                    ) | (
                        PreparedRecoveryCopy::Restore(_),
                        RecoveryOpenDecision::Restore
                    )
                ) {
                    return Err(
                        "the recovery open decision no longer matches the pinned copy".into(),
                    );
                }
                let identity = file_identity(&path);
                super::recovery::with_store(&shared, &root, &shell, |store| {
                    scope
                        .effect(|| store.clear_discarded(&identity))?
                        .map_err(|error| error.message)
                })?;
                match prepared {
                    PreparedRecoveryCopy::Original | PreparedRecoveryCopy::Restore(None) => {
                        Ok((None, None))
                    }
                    PreparedRecoveryCopy::Restore(Some(snapshot)) => {
                        if let Some(plan) =
                            snapshot.prepare_copy_state(RecoveryCopyState::Undecided)
                        {
                            let mut rejected = None;
                            let result = plan.write_via(&mut |path, value| {
                            let result = main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                                path, value, &mut |effect| scope.file_effect(effect).inspect_err(|error| {
                                    rejected = Some(error.message.clone());
                                }),
                            );
                            if rejected.is_some() { result.map(|()| true) } else { Ok(result.is_ok()) }
                        });
                            result.map_err(|error| error.message)?;
                        }
                        let later = snapshot.prepare_copy_state(RecoveryCopyState::Later);
                        Ok((Some(snapshot.bytes), later))
                    }
                }
            },
            then,
        );
    }

    fn resolve_converted_recovery(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        scope: RecoveryLoadScope,
        then: Box<dyn FnOnce(PreparedRecovery)>,
    ) {
        if self.shell.is_headless() {
            return then(scope.effect(|| (None, None)));
        }
        let shared = self.recovery_shared.clone();
        let root = self.user_data(&[]);
        let shell = self.shell.clone();
        let worker_scope = scope.clone();
        let worker_path = path.clone();
        let docs = self.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = worker_scope.work.clone();
                let identity = file_identity(&worker_path);
                super::recovery::with_store(&shared, &root, &shell, |store| {
                    worker_scope
                        .effect(|| store.clear_discarded(&identity))?
                        .map_err(|error| error.message)?;
                    store.open_plan(&identity).map_err(|error| error.message)
                })
            },
            move |result| {
                use main_core::packages::electron_utils::recovery_store::RecoveryOpenPlan;
                match result {
                    Err(error) => then(Err(error)),
                    Ok(RecoveryOpenPlan::None) => then(Ok((None, None))),
                    Ok(RecoveryOpenPlan::Restore(_)) => {
                        docs.consume_converted_recovery(path, scope, false, then)
                    }
                    Ok(RecoveryOpenPlan::Ask(copy)) => {
                        let labels = main_core::packages::electron_utils::recovery_labels::recovery_prompt_labels(
                        docs.lang().as_str(), &basename(&path),
                    );
                        let detail = if copy.file_changed_after_copy {
                            format!("{}\n\n{}", labels.detail, labels.file_changed)
                        } else {
                            labels.detail
                        };
                        let options = MessageBox {
                            kind: Some("question".into()),
                            message: labels.message,
                            detail: Some(detail),
                            buttons: vec![labels.restore, labels.discard],
                            default_id: Some(0),
                            cancel_id: Some(0),
                            ..Default::default()
                        };
                        let answered_docs = docs.clone();
                        let answered_scope = scope.clone();
                        let back = kit::back_on_core(
                            &docs.services,
                            move |answer: Result<
                                shell_contract::hosts::MessageBoxAnswer,
                                String,
                            >| {
                                match answer {
                                    Ok(answer) => answered_docs.consume_converted_recovery(
                                        path,
                                        answered_scope,
                                        answer.response == 1,
                                        then,
                                    ),
                                    Err(error) => then(Err(error)),
                                }
                            },
                        );
                        docs.dialogs.message_recovery_owned(
                            docs.parent(view),
                            options,
                            scope.receipt,
                            scope.snapshot,
                            scope.owner,
                            scope.cancel.clone(),
                            scope.cancel,
                            back,
                        );
                    }
                }
            },
        );
    }

    fn consume_converted_recovery(
        self: &Rc<Self>,
        path: String,
        scope: RecoveryLoadScope,
        discard: bool,
        then: Box<dyn FnOnce(PreparedRecovery)>,
    ) {
        let shared = self.recovery_shared.clone();
        let root = self.user_data(&[]);
        let shell = self.shell.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = scope.work.clone();
                let identity = file_identity(&path);
                if discard {
                    super::recovery::with_store(&shared, &root, &shell, |store| {
                        store.delete_copy_authorized(&identity,
                        main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::Discarded,
                        &mut |effect| scope.file_effect(effect))
                        .map_err(|error| error.message)?;
                        Ok::<(), String>(())
                    })?;
                    return Ok((None, None));
                }
                let pinned = scope.effect(|| {
                    main_core::packages::electron_utils::recovery_store::RecoveryPinnedRead::open(
                        &root, &identity,
                    )
                })?;
                let Some(snapshot) = pinned.ok().and_then(|file| file.read().ok()) else {
                    return Ok((None, None));
                };
                if let Some(plan) = snapshot.prepare_copy_state(RecoveryCopyState::Undecided) {
                    let mut denied = None;
                    let result = plan.write_via(&mut |path, value| {
                    let result = main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                        path, value, &mut |effect| scope.file_effect(effect).inspect_err(|error| {
                            denied = Some(error.message.clone());
                        }),
                    );
                    if denied.is_some() { result.map(|()| true) } else { Ok(result.is_ok()) }
                });
                    result.map_err(|error| error.message)?;
                }
                let later = snapshot.prepare_copy_state(RecoveryCopyState::Later);
                Ok((Some(snapshot.bytes), later))
            },
            then,
        );
    }

    /// `docsRecovery.resolveOpen`: the restored copy's bytes, or `None` for the file
    fn resolve_open(&self, view: ViewId, path: &str) -> Result<Option<Vec<u8>>, String> {
        if self.shell.is_headless() {
            return Ok(None);
        }
        let decision = self
            .open_decisions
            .get(view)
            .filter(|(p, _)| p == path)
            .map(|(_, d)| d);
        if decision.is_some() {
            self.open_decisions.remove(view);
        }
        let identity = file_identity(path);
        let parent = self.parent(view).map(Parent);
        let lang = self.lang();
        self.with_store(|store| {
            store.clear_discarded(&identity).map_err(|e| e.message)?;
            match decision {
                Some(RecoveryOpenDecision::Original) => Ok(None),
                Some(RecoveryOpenDecision::Restore) => {
                    let Some(bytes) = store.read_copy(&identity) else {
                        return Ok(None);
                    };
                    store.mark_copy_loaded(&identity).map_err(|e| e.message)?;
                    Ok(Some(bytes))
                }
                None => {
                    let name = basename(path);
                    let result = resolve_recovery_on_open(&ResolveRecoveryOnOpenOptions {
                        store,
                        identity: &identity,
                        dialog: &HostMessageBox(&self.dialogs),
                        parent: parent.as_ref(),
                        lang: lang.as_str(),
                        document_name: Some(&name),
                    })
                    .map_err(|e| e.message)?;
                    Ok(match result {
                        RecoveryOpenResult::Original => None,
                        RecoveryOpenResult::Restore { bytes, .. } => Some(bytes),
                    })
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_recovery_loaded(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        data_url: String,
        hash: String,
        encrypted: bool,
        recovered: bool,
        failed: bool,
        disk: Option<(f64, f64)>,
        scope: RecoveryLoadScope,
        later: Option<main_core::packages::electron_utils::recovery_store::RecoveryMetadataPlan>,
        then: LoadThen,
    ) {
        let docs = self.clone();
        let worker_scope = scope.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _work = worker_scope.work.clone();
                if failed && let Some(plan) = later {
                    let mut rejected = None;
                    let result = plan.write_via(&mut |path, value| {
                    let result = main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                        path, value, &mut |effect| worker_scope.file_effect(effect).inspect_err(|error| {
                            rejected = Some(error.message.clone());
                        }),
                    );
                    if rejected.is_some() { result.map(|()| true) } else { Ok(result.is_ok()) }
                });
                    result.map_err(|error| error.message)?;
                }
                worker_scope.effect(|| ())
            },
            move |result| {
                if let Err(error) = result {
                    return then(Err(LoadError::Thrown(error)));
                }
                let after_recent_docs = docs.clone();
                let after_recent_scope = scope.clone();
                let recent_path = path.clone();
                docs.push_recent_recovery(
                    recent_path,
                    scope,
                    Box::new(move |result| {
                        let changed = match result {
                            Ok(changed) => changed,
                            Err(error) => return then(Err(LoadError::Thrown(error))),
                        };
                        let docs = after_recent_docs;
                        let scope = after_recent_scope;
                        let next_docs = docs.clone();
                        let next_scope = scope.clone();
                        let proceed: Box<dyn FnOnce(Result<(), String>)> =
                            Box::new(move |result| {
                                if let Err(error) = result {
                                    return then(Err(LoadError::Thrown(error)));
                                }
                                if let Err(error) = next_scope.effect(|| {
                                    next_docs.allow_doc_write(view, &path);
                                    next_docs.remember_open_doc(view, &path);
                                }) {
                                    return then(Err(LoadError::Thrown(error)));
                                }
                                let final_docs = next_docs.clone();
                                let final_scope = next_scope.clone();
                                let final_path = path.clone();
                                next_docs.shell.file_opened_recovery(
                                    path,
                                    next_scope.receipt.clone(),
                                    next_scope.owner.clone(),
                                    next_scope.cancel.clone(),
                                    next_scope.cancel.clone(),
                                    next_scope.effects.clone(),
                                    Box::new(move |result| {
                                        if let Err(error) = result {
                                            return then(Err(LoadError::Thrown(error)));
                                        }
                                        if let Err(error) = final_scope.effect(|| {
                                            final_docs.passwords.borrow_mut().mark_encrypted(
                                                view,
                                                &final_path,
                                                encrypted,
                                            );
                                            final_docs.remember_disk(
                                                view,
                                                &final_path,
                                                hash.clone(),
                                                disk,
                                            );
                                        }) {
                                            return then(Err(LoadError::Thrown(error)));
                                        }
                                        then(Ok(Loaded::Opened(
                                            Obj::new()
                                                .set("name", basename(&final_path))
                                                .set("dataUrl", data_url)
                                                .set("hash", hash)
                                                .set("encrypted", encrypted)
                                                .set(
                                                    "recovered",
                                                    if recovered {
                                                        IpcValue::Bool(true)
                                                    } else {
                                                        IpcValue::Undefined
                                                    },
                                                )
                                                .prepend("path", final_path)
                                                .done(),
                                        )));
                                    }),
                                );
                            });
                        if !changed {
                            return proceed(Ok(()));
                        }
                        match docs.shell.prepare_docs_menu_refresh_recovery(
                            scope.receipt.clone(),
                            scope.owner.clone(),
                            scope.cancel.clone(),
                            scope.cancel.clone(),
                            scope.effects.clone(),
                        ) {
                            Ok(Some(refresh)) => refresh(proceed),
                            Ok(None) => proceed(Ok(())),
                            Err(error) => proceed(Err(error)),
                        }
                    }),
                );
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_load(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        format: DocFormat,
        encrypted: bool,
        plain: Vec<u8>,
        hash: String,
        restored: Option<Vec<u8>>,
        disk_password: Option<String>,
        permit: main_core::docs::byte_handoff::MediaPermit,
        recovery: Option<RecoveryLoadScope>,
        later: Option<main_core::packages::electron_utils::recovery_store::RecoveryMetadataPlan>,
        lease: main_core::docs::byte_handoff::MediaViewLease,
        old: main_core::docs::byte_handoff::MediaPermit,
        authority: LoadAuthority,
        then: LoadThen,
    ) {
        let engine = self.engine.clone();
        let docs = Rc::clone(self);
        let source = path.clone();
        let worker_permit = permit.clone();
        let worker_cancel = authority.cancel.clone();
        let pinned = recovery.as_ref().and_then(|scope| {
            (scope.source_path.as_deref() == Some(source.as_str()))
                .then(|| (scope.source_file.clone(), scope.cancel.clone()))
        });
        let worker_work = recovery.as_ref().and_then(|scope| scope.work.clone());
        kit::on_worker(
            &self.services,
            move || {
                let _work = worker_work;
                let mut recovered = restored.is_some();
                let mut failed = false;
                let mut bytes = restored.unwrap_or_else(|| plain.clone());
                // a copy of a protected document is encrypted too; an unreadable one falls back
                if encrypted && docx_open_kind(&bytes) != OpenKind::Plain {
                    let decrypted = match &disk_password {
                        Some(pw) => engine.decrypt_docx(&bytes, pw).ok(),
                        None => None,
                    };
                    match decrypted {
                        Some(b) => bytes = b,
                        None => {
                            bytes = plain.clone();
                            recovered = false;
                            failed = true;
                        }
                    }
                }
                // a copy under an ODT's path must be the ODT itself
                if recovered && format == DocFormat::Odt && !is_odt_package(&bytes) {
                    bytes = plain;
                    recovered = false;
                    failed = true;
                }
                if recovered && format == DocFormat::Docx {
                    let file = match &pinned {
                        Some((Some(file), _)) => Some(file.clone()),
                        Some((None, _)) => {
                            return Err("The captured document source is unavailable".into());
                        }
                        None => None,
                    };
                    engine.adopt_lazy_media_hashes(
                        &bytes,
                        &source,
                        file,
                        worker_permit.clone(),
                        worker_cancel,
                    )?;
                }
                let disk = match &pinned {
                    Some((Some(file), _)) => file.metadata().ok().map(|meta| {
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
                        (mtime, meta.len() as f64)
                    }),
                    Some((None, _)) => {
                        return Err("The captured document source is unavailable".into());
                    }
                    None => stat(&source).ok(),
                };
                Ok::<_, String>((
                    engine.hand_off_bytes(bytes, worker_permit)?,
                    recovered,
                    failed,
                    disk,
                ))
            },
            move |result| {
                if !permit.is_active() {
                    return then(Err(LoadError::Thrown(
                        "The document media session is closed.".into(),
                    )));
                }
                let (data_url, recovered, failed, disk) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        docs.abort_media_document(view, &permit);
                        return then(Err(LoadError::Thrown(error)));
                    }
                };
                let mut recovery = recovery;
                let transition = super::media_transition::DocsMediaTransitionAdapter {
                    lease,
                    old: old.clone(),
                    successor: permit.clone(),
                };
                let Some(effects) = docs
                    .services
                    .hosts
                    .get::<shell_contract::hosts::OwnedEffectsHost>()
                else {
                    docs.abort_media_document(view, &permit);
                    return then(Err(LoadError::Thrown(
                        "Native document effects are unavailable.".into(),
                    )));
                };
                let candidate = match super::disposal::candidate(&docs, view) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        docs.abort_media_document(view, &permit);
                        return then(Err(LoadError::Thrown(error)));
                    }
                };
                let mut retain =
                    |owner: &shell_contract::ipc::NativeRequestOwner,
                     commit: &mut dyn FnMut() -> Result<(), String>| {
                        super::disposal::retain_successor(
                            candidate.as_ref(),
                            &transition.lease,
                            &old,
                            &permit,
                            owner,
                            commit,
                        )
                    };
                let successor = match effects.transition_docs_media(
                    &authority.owner,
                    &[],
                    &authority.cancel,
                    &authority.cancel,
                    &transition,
                    recovery.as_ref().map(|scope| &scope.receipt),
                    &mut retain,
                ) {
                    Ok(owner) => owner,
                    Err(error) => {
                        docs.abort_media_document(view, &permit);
                        return then(Err(LoadError::Thrown(error)));
                    }
                };
                #[cfg(any(test, feature = "probe"))]
                docs.retain_synthetic_successor(view, &successor);
                let _ = docs.engine.forget_lazy_media_owner(old);
                if let Some(scope) = recovery.as_mut() {
                    scope.owner = successor;
                    let stored = scope.effect(|| {
                        let mut slot = scope
                            .permit
                            .try_lock()
                            .map_err(|_| "the recovery media slot is busy".to_owned())?;
                        *slot = Some(permit.clone());
                        Ok::<(), String>(())
                    });
                    if let Err(error) = stored.and_then(|value| value) {
                        return then(Err(LoadError::Thrown(error)));
                    }
                }
                if let Some(scope) = recovery {
                    return docs.finish_recovery_loaded(
                        view, path, data_url, hash, encrypted, recovered, failed, disk, scope,
                        later, then,
                    );
                }
                if failed {
                    docs.with_store(|store| {
                        let _ =
                            store.set_copy_state(&file_identity(&path), RecoveryCopyState::Later);
                    });
                }
                docs.push_recent(&path);
                docs.allow_doc_write(view, &path);
                docs.remember_open_doc(view, &path);
                docs.shell.file_opened(view, &path);
                docs.passwords
                    .borrow_mut()
                    .mark_encrypted(view, &path, encrypted);
                docs.remember_disk(view, &path, hash.clone(), disk);
                then(Ok(Loaded::Opened(
                    Obj::new()
                        .set("name", basename(&path))
                        .set("dataUrl", data_url)
                        .set("hash", hash)
                        .set("encrypted", encrypted)
                        .set(
                            "recovered",
                            if recovered {
                                IpcValue::Bool(true)
                            } else {
                                IpcValue::Undefined
                            },
                        )
                        .prepend("path", path)
                        .done(),
                )))
            },
        );
    }

    /// `prepareDocumentForOpen` then `loadDocx`, answering the invoke
    fn open_into(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        responder: Responder,
        mut recovery: Option<(String, RecoveryLoadScope)>,
        authority: LoadAuthority,
    ) {
        let work = match recovery.as_mut() {
            Some((token, scope)) => match self.begin_recovery_work(view, token) {
                Ok(ticket) => {
                    scope.work = Some(ticket.clone());
                    Some(ticket)
                }
                Err(error) => return responder.reply(Err(kit::error(error))),
            },
            None => None,
        };
        let docs = Rc::clone(self);
        let response = RecoveryReply {
            responder: Some(responder),
            docs: docs.clone(),
            view,
            token: recovery.as_ref().map(|(token, _)| token.clone()),
            permit: recovery
                .as_ref()
                .map(|(_, scope)| scope.original_permit.clone()),
            _work: work,
        };
        self.prepare_for_open(
            view,
            path,
            self.parent(view),
            recovery.as_ref().map(|(_, scope)| scope.clone()),
            Box::new(move |prepared| match prepared {
                None => {
                    eprintln!(
                        "[docs] view {}: the open was not prepared (path refused)",
                        view.0
                    );
                    let result = recovery.map_or(IpcValue::Null, |(token, scope)| {
                        docs.recovery_response(
                            view,
                            &token,
                            Obj::new()
                                .set("recoveryError", "the recovery file could not be opened")
                                .done(),
                            Some(scope.original_permit),
                        )
                    });
                    response.reply(Ok(result));
                }
                Some((path, prepared_scope)) => {
                    let response_docs = docs.clone();
                    let recovery = match (recovery, prepared_scope) {
                        (Some((token, _)), Some(scope)) => Some((token, scope)),
                        (None, None) => None,
                        _ => {
                            return response.reply(Err(kit::error(
                                "the prepared Docs recovery scope does not match its request",
                            )));
                        }
                    };
                    let scope = recovery.as_ref().map(|(_, scope)| scope.clone());
                    docs.load_docx(
                        view,
                        path,
                        None,
                        scope,
                        authority,
                        Box::new(move |result| {
                            let opened = matches!(&result, Ok(Loaded::Opened(_)));
                            let reply = load_reply(result);
                            eprintln!(
                                "[docs] view {}: the open answered {}",
                                view.0,
                                match &reply {
                                    Ok(IpcValue::Null) => "nothing".to_owned(),
                                    Ok(_) => format!("a document (opened: {opened})"),
                                    Err(error) => format!("an error: {}", error.message),
                                }
                            );
                            let reply = match recovery {
                                None => reply,
                                Some((token, scope)) => {
                                    let value = match reply {
                                        Ok(IpcValue::Null) => Obj::new()
                                            .set(
                                                "recoveryError",
                                                "the recovery file could not be opened",
                                            )
                                            .done(),
                                        Ok(value) => value,
                                        Err(error) => Obj::new()
                                            .set("recoveryError", format!("{error:?}"))
                                            .done(),
                                    };
                                    let permit = if opened {
                                        scope.permit.lock().ok().and_then(|permit| permit.clone())
                                    } else {
                                        Some(scope.original_permit.clone())
                                    };
                                    Ok(response_docs.recovery_response(view, &token, value, permit))
                                }
                            };
                            response.reply(reply);
                        }),
                    );
                }
            }),
        );
    }

    /// `openResultFor(contents, result)`
    #[cfg(any(test, feature = "probe"))]
    fn send_opened(&self, view: ViewId, result: IpcValue) {
        if result == IpcValue::Null || !self.alive(view) {
            return;
        }
        self.services
            .events
            .emit(Target::View(view), "docs:opened", vec![result]);
    }

    /// `openDocsFileWithDecision(contents, request)`: the shell answered the
    /// recovery question already (launch list)
    #[cfg(any(test, feature = "probe"))]
    pub(crate) fn open_file_with_decision(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        decision: RecoveryOpenDecision,
        authority: LoadAuthority,
    ) {
        if !self.alive(view) || doc_format_of_path(&path).is_none() {
            return;
        }
        self.open_decisions.insert(view, (path.clone(), decision));
        if !self.booted.borrow().contains(&view) {
            self.window_opens.insert(view, path);
            return;
        }
        let docs = Rc::clone(self);
        self.load_docx(
            view,
            path,
            None,
            None,
            authority,
            Box::new(move |r| match r {
                Ok(loaded) => docs.send_opened(view, loaded.into_ipc()),
                Err(e) => docs
                    .dialogs
                    .error(&docs.tm("dlgOpenDoc"), &super::error_text(e.message())),
            }),
        );
    }

    /// `openDocsUntitledCopy(contents, request)`: a copy loads as an untitled document
    #[cfg(any(test, feature = "probe"))]
    pub(crate) fn open_untitled_copy(
        self: &Rc<Self>,
        view: ViewId,
        copy: RecoveryIdentity,
        title: String,
        save_as_default_path: Option<String>,
    ) {
        if !self.alive(view) || copy.kind() != "docs" {
            return;
        }
        let loading = self.take_untitled_copy(view, &copy, &title, save_as_default_path);
        if !self.booted.borrow().contains(&view) {
            // the renderer's first-document request takes it
            self.pending_untitled.insert(view, loading);
            return;
        }
        if let Some((fields, bytes)) = loading {
            let permit = match self.begin_media_document(view) {
                Ok(permit) => permit,
                Err(error) => {
                    self.dialogs
                        .error(&self.tm("dlgOpenDoc"), &super::error_text(&error));
                    return;
                }
            };
            let worker_permit = permit.clone();
            let engine = self.engine.clone();
            let docs = Rc::clone(self);
            kit::on_worker(
                &self.services,
                move || engine.hand_off_bytes(bytes, worker_permit),
                move |result| {
                    if !permit.is_active() || !docs.alive(view) {
                        return;
                    }
                    match result {
                        Ok(url) => match docs.commit_media_document(view, &permit) {
                            Ok(()) => docs.send_opened(view, with_data_url(fields, url)),
                            Err(error) => {
                                docs.abort_media_document(view, &permit);
                                docs.dialogs
                                    .error(&docs.tm("dlgOpenDoc"), &super::error_text(&error));
                            }
                        },
                        Err(error) => {
                            docs.abort_media_document(view, &permit);
                            docs.dialogs
                                .error(&docs.tm("dlgOpenDoc"), &super::error_text(&error));
                        }
                    }
                },
            );
        }
    }

    /// `takeDocsUntitledCopy(contents, request)`
    #[cfg(any(test, feature = "probe"))]
    fn take_untitled_copy(
        &self,
        view: ViewId,
        copy: &RecoveryIdentity,
        title: &str,
        save_as_default_path: Option<String>,
    ) -> Option<UntitledOpen> {
        let ext = match copy {
            RecoveryIdentity::File(f) => recovery_ext_for(&f.file_path),
            RecoveryIdentity::Untitled(u) if u.ext == ".odt" => ".odt",
            RecoveryIdentity::Untitled(_) => ".docx",
        };
        self.with_store(|store| {
            let (bytes, identity) =
                self.book
                    .borrow_mut()
                    .take_untitled_copy(store, view, copy, title, ext, |b| {
                        docx_open_kind(b) == OpenKind::Plain
                    })?;
            if !self.alive(view) {
                return None;
            }
            let as_identity = RecoveryIdentity::Untitled(identity.clone());
            let info = store.copy_info(&as_identity).ok().flatten();
            Some((
                untitled_recovery_fields(&identity, title, ext, save_as_default_path, info),
                bytes,
            ))
        })
    }
}

pub(super) fn untitled_recovery_fields(
    identity: &main_core::packages::electron_utils::recovery_store::RecoveryUntitledIdentity,
    title: &str,
    ext: &str,
    save_as_default_path: Option<String>,
    info: Option<main_core::packages::electron_utils::recovery_store::RecoveryCopyInfo>,
) -> Vec<(String, IpcValue)> {
    let title = if !title.is_empty() {
        title.to_owned()
    } else if let Some(t) = info
        .as_ref()
        .map(|i| i.title.clone())
        .filter(|t| !t.is_empty())
    {
        t
    } else {
        identity.untitled_id.chars().take(8).collect()
    };
    let want = if ext == ".odt" {
        DocFormat::Odt
    } else {
        DocFormat::Docx
    };
    let named = |candidate: &str| -> String {
        let format = doc_format_of_path(candidate);
        if format == Some(want) {
            return candidate.to_owned();
        }
        let stem = match (format, candidate.rfind('.')) {
            (Some(_), Some(dot)) => &candidate[..dot],
            _ => candidate,
        };
        format!("{stem}{ext}")
    };
    let origin = save_as_default_path.or_else(|| info.and_then(|i| i.original_path));
    let name = named(&origin.as_deref().map_or(title, basename));
    let mut fields = Obj::new()
        .set("path", name.clone())
        .set("name", name)
        .set("dataUrl", "")
        .set("hash", "")
        .set("untitledCopy", true)
        .set("recovered", true);
    if let Some(origin) = origin {
        fields = fields.set("saveAsDefaultPath", origin);
    }
    fields.0
}

impl Obj {
    /// the key first, as the literal writes it
    fn prepend(mut self, key: &str, value: impl IntoIpc) -> Self {
        self.0.insert(0, (key.to_owned(), value.into_ipc()));
        self
    }
}

/// `{ ...result, dataUrl }`: the spread keeps the key where it was
fn with_data_url(mut fields: Vec<(String, IpcValue)>, url: String) -> IpcValue {
    for (k, v) in &mut fields {
        if k == "dataUrl" {
            *v = IpcValue::String(url.clone());
        }
    }
    IpcValue::Object(fields)
}

/// What an invoke that returned `loadDocx(...)` answers
fn load_reply(r: Result<Loaded, LoadError>) -> Reply {
    match r {
        Ok(loaded) => Ok(loaded.into_ipc()),
        Err(e) => Err(kit::error(e.message())),
    }
}

struct RecoveryReply {
    responder: Option<Responder>,
    docs: Rc<Docs>,
    view: ViewId,
    token: Option<String>,
    permit: Option<main_core::docs::byte_handoff::MediaPermit>,
    _work: Option<shell_contract::ipc::RecoveryWorkTicket>,
}

impl RecoveryReply {
    fn reply(mut self, reply: Reply) {
        if let Some(responder) = self.responder.take() {
            responder.reply(reply);
        }
    }
}

impl Drop for RecoveryReply {
    fn drop(&mut self) {
        let (Some(responder), Some(token)) = (self.responder.take(), self.token.take()) else {
            return;
        };
        let failure = Obj::new()
            .set("recoveryError", "the recovery open worker was discarded")
            .done();
        responder.reply(Ok(self.docs.recovery_response(
            self.view,
            &token,
            failure,
            self.permit.take(),
        )));
    }
}

pub(super) fn entries(docs: &Rc<Docs>) -> Vec<Entry> {
    let mut all = Vec::new();

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:open",
        move |cx, _args, responder| {
            let view = cx.sender.view;
            let authority = match d.capture_load_authority(cx) {
                Ok(authority) => authority,
                Err(error) => return responder.reply(Err(error)),
            };
            let filters = js_filters(&[
                (d.tm("filterSupported"), vec!["docx", "odt", "doc", "dot"]),
                (d.tm("filterWord"), vec!["docx", "doc", "dot"]),
                (d.tm("filterOdt"), vec!["odt"]),
            ]);
            let result = d.open_dialog(
                view,
                js_obj(vec![
                    ("title", js_str(d.tm("dlgOpenDoc"))),
                    ("filters", filters),
                    ("properties", JsonValue::Array(vec![js_str("openFile")])),
                ]),
            );
            match picked_path(&result, "filePaths") {
                None => responder.reply(Ok(IpcValue::Null)),
                Some(path) => d.open_into(view, path, responder, None, authority),
            }
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:open-path",
        move |cx, args, responder| {
            let authority = match d.capture_load_authority(cx) {
                Ok(authority) => authority,
                Err(error) => return responder.reply(Err(error)),
            };
            // no allowlist: the renderer names the file (loadDocx refuses other formats)
            match args.str(0) {
                Some(path) => {
                    d.open_into(cx.sender.view, path.to_owned(), responder, None, authority)
                }
                // loadDocx's `typeof filePath === 'string'` check
                None => responder.reply(Ok(IpcValue::Null)),
            }
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:open-decrypt",
        move |cx, args, responder| {
            let (Some(path), Some(password)) = (args.str(0), args.str(1).filter(|p| !p.is_empty()))
            else {
                return responder.reply(Ok(decrypt_failed("error", "invalid arguments")));
            };
            let view = cx.sender.view;
            let authority = match d.capture_load_authority(cx) {
                Ok(authority) => authority,
                Err(error) => return responder.reply(Err(error)),
            };
            let recovery = d.recovery_password_token(view, path);
            let mut scope = match recovery.as_deref() {
                Some(token) => match d.capture_recovery_load_scope(cx, token) {
                    Ok(scope) => {
                        d.recovery_password_retry(view, token);
                        Some(scope)
                    }
                    Err(error) => return responder.reply(Err(kit::error(error))),
                },
                None => None,
            };
            let work = match recovery.as_deref() {
                Some(token) => match d.begin_recovery_work(view, token) {
                    Ok(ticket) => {
                        if let Some(scope) = &mut scope {
                            scope.work = Some(ticket.clone());
                        }
                        Some(ticket)
                    }
                    Err(error) => return responder.reply(Err(kit::error(error))),
                },
                None => None,
            };
            let docs = d.clone();
            d.load_docx(
                cx.sender.view,
                path.to_owned(),
                Some(password.to_owned()),
                scope.clone(),
                authority,
                Box::new(move |r| {
                    let _work = work;
                    responder.reply(Ok(match r {
                        Ok(Loaded::Null) => decrypt_failed("error", "file not found"),
                        // the file was swapped for a plain docx between prompt and submit
                        Ok(Loaded::NeedsPassword { .. }) => {
                            decrypt_failed("error", "not encrypted")
                        }
                        Ok(Loaded::Opened(result)) => {
                            let result = match recovery {
                                Some(token) => docs.recovery_response(
                                    view,
                                    &token,
                                    result,
                                    scope.as_ref().and_then(|scope| {
                                        scope.permit.lock().ok().and_then(|permit| permit.clone())
                                    }),
                                ),
                                None => result,
                            };
                            Obj::new().set("ok", true).set("result", result).done()
                        }
                        Err(LoadError::Decrypt(e)) => decrypt_failed(e.reason, &e.message),
                        Err(LoadError::Thrown(m)) => {
                            decrypt_failed("error", &super::error_text(&m))
                        }
                    }))
                }),
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:consume-pending-open",
        move |cx, _args, responder| {
            let view = cx.sender.view;
            let responder = match super::saved_load::queue_consumer(&d, cx, responder) {
                Ok(ready) => {
                    if ready {
                        super::saved_load::start(&d, view);
                    }
                    return;
                }
                Err(responder) => responder,
            };
            let authority = match d.capture_load_authority(cx) {
                Ok(authority) => authority,
                Err(error) => return responder.reply(Err(error)),
            };
            // in the shell every tab's document is queued for it (no argv fallback)
            let recovery = d.recovery_token(view);
            let scope = match recovery.as_deref() {
                Some(token) => match d.capture_recovery_load_scope(cx, token) {
                    Ok(scope) => Some(scope),
                    Err(error) => return responder.reply(Err(kit::error(error))),
                },
                None => None,
            };
            let queued = match if recovery.is_some() {
                d.recovery_ingress(cx, || d.window_opens.remove(view))
            } else {
                d.take_headless_ingress(cx, || d.window_opens.remove(view))
            } {
                Ok(queued) => queued,
                Err(error) => return responder.reply(Err(kit::error(error))),
            };
            eprintln!(
                "[docs] view {}: consume-pending-open finds {queued:?} (recovery: {})",
                view.0,
                recovery.is_some()
            );
            match queued {
                None => responder.reply(Ok(IpcValue::Null)),
                Some(path) => d.open_into(view, path, responder, recovery.zip(scope), authority),
            }
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke("docs:recovery-initialized", move |cx, args| {
        let token = args
            .str(0)
            .ok_or_else(|| kit::error("the recovery receipt is missing"))?;
        let ok = args
            .bool(1)
            .ok_or_else(|| kit::error("the recovery result is invalid"))?;
        let result = if ok {
            Ok(())
        } else {
            Err(args
                .str(2)
                .unwrap_or("the recovered document could not be initialized")
                .to_owned())
        };
        if super::saved_load::settle(&d, cx, token, result.clone()).map_err(kit::error)? {
            return Ok(IpcValue::Undefined);
        }
        d.settle_recovery(cx, token, result).map_err(kit::error)?;
        Ok(IpcValue::Undefined)
    }));

    let d = docs.clone();
    all.push(kit::invoke("docs:consume-new-blank", move |cx, _args| {
        eprintln!(
            "[docs] view {}: consume-new-blank (recovery blank token: {}, owned new blank: {}, plain new blank: {})",
            cx.sender.view.0,
            d.recovery_blank_token(cx.sender.view).is_some(),
            d.owned_new_blank.get(cx.sender.view).is_some(),
            d.new_blank.borrow().contains(&cx.sender.view)
        );
        if let Some(token) = d.recovery_blank_token(cx.sender.view) {
            let scope = d
                .capture_recovery_load_scope(cx, &token)
                .map_err(kit::error)?;
            let _work = d
                .begin_recovery_work(cx.sender.view, &token)
                .map_err(kit::error)?;
            let consumed = d
                .recovery_ingress(cx, || d.new_blank.borrow_mut().remove(&cx.sender.view))
                .map_err(kit::error)?;
            if !consumed {
                return Err(kit::error("the blank recovery seed was already consumed"));
            }
            return Ok(d.recovery_response(
                cx.sender.view,
                &token,
                Obj::new().set("blank", true).done(),
                Some(scope.original_permit),
            ));
        }
        let Some(created) = d.owned_new_blank.get(cx.sender.view) else {
            return Ok(d.new_blank.borrow_mut().remove(&cx.sender.view).into_ipc());
        };
        let context = cx
            .native
            .clone()
            .ok_or_else(|| kit::error("The initialized document has no native owner"))?;
        let consumer = shell_contract::ipc::NativeRequestOwner {
            view: cx.sender.view,
            context,
        };
        let actual = consumer
            .context
            .renderer()
            .ok_or_else(|| kit::error("The initialized document renderer is unavailable"))?;
        let effects = d
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .ok_or_else(|| kit::error("Native initial document effects are unavailable"))?;
        let mut consumed = false;
        effects
            .with_owned_effect(&consumer, &consumer, &cx.cancel, &cx.cancel, &mut || {
                if !created.is_active() || !created.same_generation(actual) {
                    return Err("The blank initializer belongs to another document".into());
                }
                consumed = d.owned_new_blank.remove(cx.sender.view).is_some();
                Ok(())
            })
            .map_err(kit::error)?;
        Ok(consumed.into_ipc())
    }));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:consume-copy",
        move |cx, _args, responder| {
            let view = cx.sender.view;
            // from now on a document for this renderer goes out as docs:opened
            d.booted.borrow_mut().insert(view);
            let recovery = d.recovery_token(view);
            let recovery_scope = match recovery.as_deref() {
                Some(token) => match d.capture_recovery_load_scope(cx, token) {
                    Ok(scope) => Some(scope),
                    Err(error) => return responder.reply(Err(kit::error(error))),
                },
                None => None,
            };
            let restoring = match d.recovery_ingress(cx, || d.pending_untitled.remove(view)) {
                Ok(restoring) => restoring,
                Err(error) => return responder.reply(Err(kit::error(error))),
            };
            if let Some(restoring) = restoring
                && let Some((fields, bytes)) = restoring
                && d.alive(view)
            {
                let work = match recovery.as_deref() {
                    Some(token) => match d.begin_recovery_work(view, token) {
                        Ok(ticket) => Some(ticket),
                        Err(error) => return responder.reply(Err(kit::error(error))),
                    },
                    None => None,
                };
                let lease = if recovery_scope.is_some() {
                    match d.media_views.borrow().get(&view).map(|slot| slot.lease.clone()) {
                        Some(lease) => Some(lease),
                        None => return responder.reply(Err(kit::error("the captured Docs media view is unavailable"))),
                    }
                } else { None };
                let staged = match (&recovery_scope, &lease) {
                    (Some(scope), Some(lease)) => (|| {
                        let prior = lease.pending();
                        let candidate = lease.prepare_document()?;
                        let stage = super::media_transition::DocsMediaStageAdapter {
                            lease: lease.clone(), old: scope.original_permit.clone(), prior: prior.clone(), candidate: candidate.clone(),
                        };
                        scope.effects.stage_docs_media(&scope.owner, &[], &scope.cancel, &scope.cancel, &stage, Some(&scope.receipt))?;
                        if let Some(prior) = prior { let _ = d.engine.forget_lazy_media_owner(prior); }
                        Ok(candidate.permit().clone())
                    })(),
                    _ => d.begin_media_document(view),
                };
                let permit = match staged {
                    Ok(permit) => permit,
                    Err(error) => return match recovery {
                        Some(token) => responder.reply(Ok(d.recovery_response(view, &token, Obj::new().set("recoveryError", error).done(),
                            recovery_scope.map(|scope| scope.original_permit)))),
                        None => responder.reply(Err(kit::error(error))),
                    },
                };
                let worker_permit = permit.clone();
                let engine = d.engine.clone();
                let docs = Rc::clone(&d);
                let worker_work = work.clone();
                let response = RecoveryReply { responder: Some(responder), docs: docs.clone(), view, token: recovery.clone(),
                    permit: recovery_scope.as_ref().map(|scope| scope.original_permit.clone()), _work: work };
                kit::on_worker(&d.services, move || { let _work = worker_work; engine.hand_off_bytes(bytes, worker_permit) }, move |result| {
                    let result = result.and_then(|url| {
                        if let (Some(scope), Some(lease)) = (&recovery_scope, &lease) {
                            let transition = super::media_transition::DocsMediaTransitionAdapter {
                                lease: lease.clone(), old: scope.original_permit.clone(), successor: permit.clone(),
                            };
                            let candidate = super::disposal::candidate(&docs, view)?;
                            let mut retain = |owner: &shell_contract::ipc::NativeRequestOwner, commit: &mut dyn FnMut() -> Result<(), String>| {
                                super::disposal::retain_successor(candidate.as_ref(), lease, &scope.original_permit, &permit, owner, commit)
                            };
                            let successor = scope.effects.transition_docs_media(&scope.owner, &[], &scope.cancel, &scope.cancel,
                                &transition, Some(&scope.receipt), &mut retain)?;
                            #[cfg(any(test, feature = "probe"))]
                            docs.retain_synthetic_successor(view, &successor);
                            #[cfg(not(any(test, feature = "probe")))]
                            let _ = successor;
                            let _ = docs.engine.forget_lazy_media_owner(scope.original_permit.clone());
                        } else {
                            docs.commit_media_document(view, &permit)?;
                        }
                        Ok(with_data_url(fields, url))
                    });
                    if result.is_err() { docs.abort_media_document(view, &permit); }
                    match recovery {
                        Some(token) => {
                            let value = result.unwrap_or_else(|error| Obj::new().set("recoveryError", error).done());
                            response.reply(Ok(docs.recovery_response(view, &token, value, Some(permit.clone()))));
                        }
                        None => response.reply(result.map_err(kit::error)),
                    }
                });
                return;
            }
            let copy = d.doc_copies.remove(view);
            let Some((path, name)) = copy.filter(|(_, name)| doc_format_of_path(name).is_some())
            else {
                return responder.reply(Ok(IpcValue::Null));
            };
            let permit = match d.begin_media_document(view) {
                Ok(permit) => permit,
                Err(error) => return responder.reply(Err(kit::error(error))),
            };
            let worker_permit = permit.clone();
            let engine = d.engine.clone();
            let docs = Rc::clone(&d);
            kit::on_worker(
                &d.services,
                move || {
                    let (_, size) = stat(&path).ok()?;
                    if size as u64 > MAX_OPEN_BYTES {
                        return None;
                    }
                    let bytes = std::fs::read(&path).ok()?;
                    Some(engine.hand_off_bytes(bytes, worker_permit))
                },
                move |url| {
                    let result = match url {
                        None => { docs.abort_media_document(view, &permit); Ok(IpcValue::Null) },
                        Some(Err(error)) => { docs.abort_media_document(view, &permit); Err(kit::error(error)) },
                        Some(Ok(url)) => match docs.commit_media_document(view, &permit) {
                            Err(error) => { docs.abort_media_document(view, &permit); Err(kit::error(error)) },
                            Ok(()) => Ok(Obj::new()
                            .set("path", name.clone())
                            .set("name", name)
                            .set("dataUrl", url)
                            .set("hash", "")
                            .set("untitledCopy", true)
                            .done()),
                        },
                    };
                    responder.reply(result);
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke(
        "docs:consume-headless-export",
        move |cx, _args| {
            Ok(
                match d
                    .take_headless_ingress(cx, || d.headless_targets.remove(cx.sender.view))
                    .map_err(kit::error)?
                {
                    None => IpcValue::Null,
                    Some((out_path, format)) => Obj::new()
                        .set("outPath", out_path)
                        .set("format", format)
                        .done(),
                },
            )
        },
    ));

    let d = docs.clone();
    all.push(kit::send("docs:headless-export-done", move |cx, args| {
        let target = d
            .headless_waiters
            .borrow()
            .get(&cx.sender.view)
            .and_then(|(target, _)| target.clone());
        let settle = if let Some((target, pending)) = target {
            let native = cx
                .native
                .as_ref()
                .ok_or_else(|| kit::error("the Docs export report has no native sender"))?;
            let candidate = shell_contract::ipc::NativeRequestOwner {
                view: cx.sender.view,
                context: native.clone(),
            };
            d.checked_document_owner(cx.sender.view, candidate.clone())?;
            let document = target.registered_document().map_err(kit::error)?;
            let effects = d
                .services
                .hosts
                .get::<shell_contract::hosts::OwnedEffectsHost>()
                .ok_or_else(|| kit::error("the Docs export admission is unavailable"))?;
            let mut waiter = None;
            effects
                .with_native_invocation_effect(
                    &document,
                    &candidate,
                    &[],
                    &cx.cancel,
                    &pending,
                    &mut || {
                        waiter = d.headless_waiters.borrow_mut().remove(&cx.sender.view);
                        Ok(())
                    },
                )
                .map_err(kit::error)?;
            waiter
        } else {
            d.headless_waiters.borrow_mut().remove(&cx.sender.view)
        };
        if let Some((_, settle)) = settle {
            let state = args.get(0);
            let ok = kit::field(state, "ok") == &IpcValue::Bool(true);
            let error = kit::as_str(kit::field(state, "error")).map(str::to_owned);
            settle(if ok {
                Ok(())
            } else {
                Err(error.unwrap_or_else(|| "docs export failed".into()))
            });
        }
        Ok(())
    }));

    let d = docs.clone();
    all.push(kit::invoke("docs:recent", move |_cx, _args| {
        Ok(match read_json(&d.recent_path()) {
            Some(v) => kit::from_js_json(&v),
            None => IpcValue::Array(Vec::new()),
        })
    }));

    all
}

/// `{ ok: false, reason, error }` of `docs:open-decrypt`
fn decrypt_failed(reason: &str, error: &str) -> IpcValue {
    Obj::new()
        .set("ok", false)
        .set("reason", reason)
        .set("error", error)
        .done()
}

#[derive(Clone)]
pub(super) struct HeadlessPublication {
    pub(super) document: shell_contract::ipc::NativeInvocationDocument,
    pub(super) owner: shell_contract::ipc::NativeRequestOwner,
    pub(super) cancel: shell_contract::runtime::CancelToken,
    pub(super) effects: shell_contract::hosts::OwnedEffectsHost,
    pub(super) pending: shell_contract::runtime::CancelToken,
}

impl Docs {
    fn take_headless_ingress<T>(
        &self,
        cx: &shell_contract::ipc::CallContext,
        effect: impl FnOnce() -> T,
    ) -> Result<T, String> {
        if !self.shell.is_headless() {
            return Ok(effect());
        }
        let selected = self
            .headless_waiters
            .borrow()
            .get(&cx.sender.view)
            .and_then(|(target, _)| target.as_ref().map(|(target, _)| target.clone()));
        let (path, format) = match selected
            .as_ref()
            .map(|target| target.operation().selection())
        {
            Some(shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                output,
                format,
                ..
            }) => (
                output
                    .to_str()
                    .ok_or("the Docs export output path is not Unicode")?
                    .to_owned(),
                format.clone(),
            ),
            _ => (String::new(), String::new()),
        };
        let Some(publication) = self.capture_headless_publication(cx, &path, &format)? else {
            return Ok(effect());
        };
        let mut effect = Some(effect);
        let mut output = None;
        publication.effects.with_native_invocation_effect(
            &publication.document,
            &publication.owner,
            &[],
            &publication.cancel,
            &publication.pending,
            &mut || {
                output = Some(effect
                    .take()
                    .ok_or("the Docs export queue was consumed twice")?(
                ));
                Ok(())
            },
        )?;
        output.ok_or_else(|| "the Docs export queue was not admitted".into())
    }

    pub(super) fn capture_headless_publication(
        &self,
        cx: &shell_contract::ipc::CallContext,
        path: &str,
        format: &str,
    ) -> Result<Option<HeadlessPublication>, String> {
        if !self.shell.is_headless() {
            return Ok(None);
        }
        #[cfg(any(test, feature = "probe"))]
        if self
            .services
            .hosts
            .get::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .is_some()
            && self
                .headless_waiters
                .borrow()
                .get(&cx.sender.view)
                .is_some_and(|(target, _)| target.is_none())
        {
            return Ok(None);
        }
        let (target, pending) = self
            .headless_waiters
            .borrow()
            .get(&cx.sender.view)
            .and_then(|(target, _)| target.clone())
            .ok_or("the Docs headless export is no longer pending")?;
        match target.operation().selection() {
            shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                surface: shell_contract::Surface::Docs,
                output,
                format: selected,
                ..
            } if output == std::path::Path::new(path) && selected == format => {}
            _ => return Err("the Docs export does not match the selected output".into()),
        }
        let native = cx
            .native
            .clone()
            .ok_or("the Docs export has no authenticated native sender")?;
        let owner = shell_contract::ipc::NativeRequestOwner {
            view: cx.sender.view,
            context: native,
        };
        self.checked_document_owner(cx.sender.view, owner.clone())
            .map_err(|error| error.to_string())?;
        let document = target.registered_document()?;
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("the Docs export admission is unavailable")?;
        effects.with_native_invocation_effect(
            &document,
            &owner,
            &[],
            &cx.cancel,
            &pending,
            &mut || Ok(()),
        )?;
        Ok(Some(HeadlessPublication {
            document,
            owner,
            cancel: cx.cancel.clone(),
            pending,
            effects,
        }))
    }
}

impl Docs {
    fn seed_headless_export(&self, view: ViewId, input: String, out_path: String, format: &str) {
        self.window_opens.insert(view, input);
        self.headless_targets
            .insert(view, (out_path.clone(), format.to_owned()));
        self.allow_pdf_write(view, &out_path);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_headless_export_native(
        self: &Rc<Self>,
        view: ViewId,
        input: String,
        out_path: String,
        format: &str,
        timeout: std::time::Duration,
        target: &shell_contract::ipc::NativeInvocationTarget,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let matches_selection = matches!(target.operation().selection(),
            shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                surface: shell_contract::Surface::Docs, input: selected_input, output: selected_output, format: selected_format, ..
            } if selected_input == std::path::Path::new(&input)
                && selected_output == std::path::Path::new(&out_path) && selected_format == format);
        if !matches_selection {
            return then(Err(
                "the native Docs export does not match its selected input and output".into(),
            ));
        }
        if target.view() != view
            || target
                .with_active(|| self.seed_headless_export(view, input, out_path, format))
                .is_none()
        {
            return then(Err(
                "the native Docs export reservation is no longer active".into(),
            ));
        }
        self.install_headless_waiter(view, timeout, Some(target.clone()), then);
    }

    /// `exportDocsHeadless(input, outPath, format, timeoutMs)` up to its
    /// window: the shell made the hidden view `view`; its renderer takes the
    /// document and the target from the queues, and `then` gets the report
    #[cfg(any(test, feature = "probe"))]
    pub(crate) fn begin_headless_export(
        self: &Rc<Self>,
        view: ViewId,
        input: String,
        out_path: String,
        format: &str,
        timeout: std::time::Duration,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.seed_headless_export(view, input, out_path, format);
        self.install_headless_waiter(view, timeout, None, then);
    }

    fn install_headless_waiter(
        self: &Rc<Self>,
        view: ViewId,
        timeout: std::time::Duration,
        target: Option<shell_contract::ipc::NativeInvocationTarget>,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let docs = Rc::clone(self);
        let ms = timeout.as_millis();
        let timer = kit::after(&self.services, timeout, move || {
            let waiter = docs.headless_waiters.borrow_mut().remove(&view);
            if let Some((_, settle)) = waiter {
                settle(Err(format!("docs export timed out after {ms}ms")));
            }
        });
        let pending = shell_contract::runtime::CancelToken::default();
        let captured = target.map(|target| (target, pending.clone()));
        let docs = Rc::clone(self);
        self.headless_waiters.borrow_mut().insert(
            view,
            (
                captured,
                Box::new(move |report| {
                    pending.cancel();
                    kit::clear(timer);
                    docs.headless_targets.remove(view);
                    docs.window_opens.remove(view);
                    then(report)
                }),
            ),
        );
    }
}

#[cfg(test)]
mod pinned_source_tests {
    use super::read_pinned_source;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn password_retry_reads_the_same_file_without_moving_its_cursor() {
        let path = std::env::temp_dir().join(format!("docs-pinned-{}", jscompat::random_uuid()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"captured document").unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        let cancel = shell_contract::runtime::CancelToken::default();
        assert_eq!(
            read_pinned_source(&file, "fixture", &cancel).unwrap(),
            b"captured document"
        );
        assert_eq!(
            read_pinned_source(&file, "fixture", &cancel).unwrap(),
            b"captured document"
        );
        let mut byte = [0];
        file.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'p']);
        cancel.cancel();
        assert!(read_pinned_source(&file, "fixture", &cancel).is_err());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
