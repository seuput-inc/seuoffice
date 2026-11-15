use main_core::packages::electron_utils::atomic_write::owned::{Admission, admitted};
use main_core::packages::electron_utils::recovery_contract::{RecoveryOpenDecision, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest};
use main_core::packages::electron_utils::recovery_store::{RecoveryCopyState, RecoveryStore, recovery_copy_path};
use main_core::packages::electron_utils::save_conflict::record_disk_state;
use shell_contract::IpcValue;

use super::engine::DiagramEngineHost;
use super::formats::{Detected, detect_format, diagram_base_name};
use super::recovery::file_identity;
use super::recovery_open::{Prepared, file_response};
use crate::host::RecoveryStateHost;
use crate::value::{buffer, obj, string};

fn admit(admission: &Admission) -> Result<(), String> {
    admitted(admission, || ()).map_err(|error| error.message)
}

fn set_restore_chosen(identity: &main_core::packages::electron_utils::recovery_store::RecoveryIdentity,
    path: &str, admission: &Admission) -> Result<(), String> {
    if let Some(plan) = RecoveryStore::prepare_copy_state_from_path(identity, path, RecoveryCopyState::RestoreChosen) {
        plan.write_via(&mut |path, json| {
            main_core::packages::electron_utils::atomic_write::owned::write_json(path, json, admission.clone()).map(|()| true)
        }).map_err(|error| error.message)?;
    }
    Ok(())
}

pub(super) fn file(
    engine: DiagramEngineHost, shared: RecoveryStateHost, directory: String,
    request: RecoveryOpenFileRequest, headless: bool, held_elsewhere: bool, admission: Admission,
) -> Result<Prepared, String> {
    admit(&admission)?;
    let path = request.file_path;
    let failed_path = path.clone();
    let empty = |response: IpcValue| Prepared::File { path: failed_path.clone(), response, save_path: false, clean: false,
        disk: None, owned: None, restored_untitled: false, imported: None, blocked: false };
    let Some(format) = detect_format(&path) else { return Ok(empty(obj([("status", string("unsupported"))]))); };
    if format == Detected::Vsdx {
        let response = match std::fs::read(&path) {
            Ok(bytes) => {
                let title = diagram_base_name(&path);
                return Ok(Prepared::File { path, response: obj([("status", string("import")), ("importer", string("vsdx")), ("bytes", buffer(bytes)), ("title", string(title.clone()))]),
                    save_path: false, clean: false, disk: None, owned: None, restored_untitled: false, imported: Some(title), blocked: false });
            }
            Err(_) => obj([("status", string("unreadable"))]),
        };
        return Ok(empty(response));
    }
    let original = std::fs::read(&path).ok();
    let name = jscompat::node_path::host::basename(&path, None);
    let mut owned = None;
    let mut blocked = false;
    if request.decision == RecoveryOpenDecision::Restore && !headless && !held_elsewhere {
        let identity = file_identity(&path);
        let copy_path = recovery_copy_path(&directory, &identity).map_err(|error| error.message)?;
        if let Ok(bytes) = std::fs::read(&copy_path) {
            let restored = (|| -> Result<(), String> {
                set_restore_chosen(&identity, &copy_path, &admission)?;
                crate::owned_recovery::mark_loaded(&shared, &directory, &identity, &admission).map_err(|error| error.message)?;
                Ok(())
            })();
            if restored.is_ok() {
                admit(&admission)?;
                owned = Some((copy_path, identity));
                if let Ok(copy) = engine.open_recovery_diagram(&name, &bytes)? {
                    let original_opens = match original.as_ref() {
                        Some(bytes) => engine.open_recovery_diagram(&name, bytes)?.is_ok(),
                        None => false,
                    };
                    let disk = if original_opens { record_disk_state(&path) } else { None };
                    return Ok(Prepared::File { path, response: file_response(copy, Some(if original_opens { "file" } else { "untitled" })),
                        save_path: original_opens, clean: false, disk, owned, restored_untitled: !original_opens, imported: None, blocked: false });
                }
                admit(&admission)?;
            } else {
                admit(&admission)?;
                blocked = true;
            }
        }
    }
    let Some(bytes) = original else {
        let mut prepared = empty(obj([("status", string("unreadable"))]));
        if let Prepared::File { owned: slot, blocked: flag, .. } = &mut prepared { *slot = owned; *flag = blocked; }
        return Ok(prepared);
    };
    admit(&admission)?;
    match engine.open_recovery_diagram(&name, &bytes)? {
        Ok(file) => {
            let disk = record_disk_state(&path);
            Ok(Prepared::File { path, response: file_response(file, None), save_path: true, clean: true, disk, owned, restored_untitled: false, imported: None, blocked })
        }
        Err(failure) => Ok(Prepared::File { path, response: obj([("status", string(failure.reason))]), save_path: false, clean: false,
            disk: None, owned, restored_untitled: false, imported: None, blocked }),
    }
}

pub(super) fn untitled(
    engine: DiagramEngineHost, shared: RecoveryStateHost, directory: String,
    request: RecoveryOpenUntitledRequest, admission: Admission,
) -> Result<Prepared, String> {
    admit(&admission)?;
    let copy_path = recovery_copy_path(&directory, &request.copy).map_err(|error| error.message)?;
    let bytes = std::fs::read(&copy_path).map_err(|_| "the recovery copy could not be read".to_owned())?;
    crate::owned_recovery::mark_loaded(&shared, &directory, &request.copy, &admission).map_err(|error| error.message)?;
    let name = match &request.copy {
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity::File(file) => jscompat::node_path::host::basename(&file.file_path, None),
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity::Untitled(_) => "copy.drawio".to_owned(),
    };
    admit(&admission)?;
    let opened = engine.open_recovery_diagram(&name, &bytes)?.map_err(|failure| format!("the recovery copy does not open ({})", failure.reason))?;
    Ok(Prepared::Untitled { xml: opened.xml, request, copy_path })
}
