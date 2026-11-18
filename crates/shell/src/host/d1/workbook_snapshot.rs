use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use shell_contract::ipc::{NativeInvocationDocument, NativeInvocationSelection};
use shell_contract::runtime::CancelToken;
use shell_ipc_d1::sheets::engine::{EngineResult, NativeWorkbookSnapshot, engine_error};

pub(super) struct Snapshot {
    path: String,
    document: NativeInvocationDocument,
    retained: AtomicBool,
}
impl NativeWorkbookSnapshot for Snapshot {
    fn path(&self) -> &str { &self.path }
    fn retain_for_session(&self) -> EngineResult<()> {
        if self.retained.swap(true, Ordering::AcqRel) { return Err(engine_error("The workbook snapshot was already retained")); }
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any { self }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.retained.load(Ordering::Acquire) { let _ = std::fs::remove_file(&self.path); }
    }
}

pub(super) fn admit<R>(document: &NativeInvocationDocument, cancel: &CancelToken, pending: &CancelToken, action: impl FnOnce() -> R) -> EngineResult<R> {
    crate::ipc::native_owner::with_native_invocation_effect(document.target(), document.owner(), cancel, pending, action)
        .ok_or_else(|| engine_error("The workbook native invocation expired"))
}

pub(super) fn create(document: &NativeInvocationDocument, cancel: &CancelToken, pending: &CancelToken, bytes: Option<&[u8]>) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
    let input = match document.operation().selection() {
        NativeInvocationSelection::HeadlessExport { surface: shell_contract::Surface::Sheets, input, .. } => input.clone(),
        _ => return Err(engine_error("The native invocation is not a Sheets export")),
    };
    let dir = std::env::temp_dir().join("seuoffice-sheets-sessions");
    let mut path = dir.join(jscompat::random_uuid());
    if bytes.is_some() {
        path.set_extension("xlsx");
    } else if let Some(extension) = input.extension() {
        path.set_extension(extension);
    }
    let action = admit(document, cancel, pending, || || -> EngineResult<()> {
        use std::io::Write;
        std::fs::create_dir_all(&dir).map_err(|error| engine_error(error.to_string()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut output = options.open(&path).map_err(|error| engine_error(error.to_string()))?;
        if let Some(bytes) = bytes { output.write_all(bytes).map_err(|error| engine_error(error.to_string()))?; }
        else { std::io::copy(&mut std::fs::File::open(&input).map_err(|error| engine_error(error.to_string()))?, &mut output).map_err(|error| engine_error(error.to_string()))?; }
        Ok(())
    })?;
    if let Err(error) = action() { let _ = std::fs::remove_file(&path); return Err(error); }
    let text = match path.to_str() { Some(text) => text.to_owned(), None => { let _ = std::fs::remove_file(&path); return Err(engine_error("The workbook snapshot path is not Unicode")); } };
    let lease = Arc::new(Snapshot { path: text, document: document.clone(), retained: AtomicBool::new(false) });
    admit(document, cancel, pending, || ())?;
    Ok(lease)
}

pub(super) fn validate<'a>(snapshot: &'a Arc<dyn NativeWorkbookSnapshot>, document: &NativeInvocationDocument, cancel: &CancelToken, pending: &CancelToken) -> EngineResult<&'a Snapshot> {
    let snapshot = snapshot.as_any().downcast_ref::<Snapshot>().ok_or_else(|| engine_error("The workbook snapshot is not native"))?;
    if snapshot.document.owner().view != document.owner().view || !snapshot.document.created().same_generation(document.created())
        || !snapshot.document.operation().authority_generation().same_generation(document.operation().authority_generation()) {
        return Err(engine_error("The workbook snapshot belongs to another invocation"));
    }
    admit(&snapshot.document, cancel, pending, || ())?;
    Ok(snapshot)
}

struct SavedSnapshot {
    path: String,
    document: shell_contract::ipc::NativeSavedDocument,
    actual: shell_contract::ipc::NativeRequestOwner,
    actual_cancel: shell_contract::runtime::CancelToken,
    retained: AtomicBool,
}

impl NativeWorkbookSnapshot for SavedSnapshot {
    fn path(&self) -> &str { &self.path }
    fn retain_for_session(&self) -> EngineResult<()> {
        if self.retained.swap(true, Ordering::AcqRel) { return Err(engine_error("The workbook snapshot was already retained")); }
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any { self }
}

impl Drop for SavedSnapshot {
    fn drop(&mut self) {
        if !self.retained.load(Ordering::Acquire) { let _ = std::fs::remove_file(&self.path); }
    }
}

pub(super) fn admit_saved<R>(document: &shell_contract::ipc::NativeSavedDocument, actual: &shell_contract::ipc::NativeRequestOwner, actual_cancel: &shell_contract::runtime::CancelToken, action: impl FnOnce(&shell_contract::ipc::NativeSavedInput) -> R) -> EngineResult<R> {
    let target = document.target();
    let mut action = Some(action);
    let mut result = None;
    let additional: Vec<_> = target.additional().iter().collect();
    crate::ipc::native_owner::with_native_saved_document_same_origin(document, actual, actual_cancel, target.participants(), target.cancel(), target.pending(), &additional, &mut |input| {
        let action = action.take().ok_or("The saved workbook input was admitted more than once")?;
        result = Some(action(input));
        Ok(())
    }).map_err(engine_error)?;
    result.ok_or_else(|| engine_error("The saved workbook input was not admitted"))
}

pub(super) fn create_saved(document: &shell_contract::ipc::NativeSavedDocument, actual: &shell_contract::ipc::NativeRequestOwner, actual_cancel: &shell_contract::runtime::CancelToken, bytes: Option<&[u8]>) -> EngineResult<Arc<dyn NativeWorkbookSnapshot>> {
    if document.target().surface() != shell_contract::Surface::Sheets {
        return Err(engine_error("The saved document is not a workbook"));
    }
    let input = admit_saved(document, actual, actual_cancel, Clone::clone)?;
    let dir = std::env::temp_dir().join("seuoffice-sheets-sessions");
    let mut path = dir.join(jscompat::random_uuid());
    if bytes.is_some() { path.set_extension("xlsx"); }
    else if let Some(extension) = document.target().path().path().extension() { path.set_extension(extension); }
    let write = || -> EngineResult<()> {
        use std::io::Write;
        std::fs::create_dir_all(&dir).map_err(|error| engine_error(error.to_string()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut output = options.open(&path).map_err(|error| engine_error(error.to_string()))?;
        output.write_all(bytes.unwrap_or_else(|| input.bytes())).map_err(|error| engine_error(error.to_string()))?;
        Ok(())
    };
    if let Err(error) = write() { let _ = std::fs::remove_file(&path); return Err(error); }
    let text = match path.to_str() { Some(text) => text.to_owned(), None => { let _ = std::fs::remove_file(&path); return Err(engine_error("The workbook snapshot path is not Unicode")); } };
    let lease = Arc::new(SavedSnapshot { path: text, document: document.clone(), actual: actual.clone(), actual_cancel: actual_cancel.clone(), retained: AtomicBool::new(false) });
    admit_saved(document, actual, actual_cancel, |_| ())?;
    Ok(lease)
}
fn same_captured_owner(original: &shell_contract::ipc::NativeRequestOwner, requested: &shell_contract::ipc::NativeRequestOwner) -> bool {
    original.view == requested.view
        && original.loss_token().zip(requested.loss_token())
            .is_some_and(|(original, requested)| original.same_token(&requested))
}


pub(super) fn validate_saved<'a>(snapshot: &'a Arc<dyn NativeWorkbookSnapshot>, document: &shell_contract::ipc::NativeSavedDocument, actual: &shell_contract::ipc::NativeRequestOwner, actual_cancel: &shell_contract::runtime::CancelToken) -> EngineResult<&'a str> {
    let snapshot = snapshot.as_any().downcast_ref::<SavedSnapshot>().ok_or_else(|| engine_error("The saved workbook snapshot is not native"))?;
    if !snapshot.document.target().same_target(document.target())
        || !same_captured_owner(snapshot.document.owner(), document.owner())
        || !same_captured_owner(&snapshot.actual, actual) || !snapshot.actual_cancel.same_token(actual_cancel) {
        return Err(engine_error("The workbook snapshot belongs to another saved document"));
    }
    admit_saved(&snapshot.document, &snapshot.actual, &snapshot.actual_cancel, |_| ())?;
    admit_saved(document, actual, actual_cancel, |_| ())?;
    Ok(&snapshot.path)
}

#[cfg(test)]
mod tests {
    use super::same_captured_owner;
    use shell_contract::{ViewId, ipc::{NativeContext, NativeRequestOwner}, runtime::{CancelToken, RendererGeneration}};

    #[test]
    fn saved_snapshot_rejects_a_successor_owner_for_the_same_view() {
        let owner = NativeRequestOwner { view: ViewId(7),
            context: NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::new()) };
        let successor = NativeRequestOwner { view: owner.view,
            context: NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::new()) };
        assert!(same_captured_owner(&owner, &owner.clone()));
        assert!(!same_captured_owner(&owner, &successor));
    }
}
