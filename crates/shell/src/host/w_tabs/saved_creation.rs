//! Saved-output construction retains its original target through registration,
//! native document readiness and actual browser cleanup.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, atomic::{AtomicU64, Ordering}};

use main_core::shell::tab_manager::TabError;
use shell_contract::ipc::{NativeRequestOwner, NativeSavedDocument, NativeSavedTarget};
use shell_contract::runtime::{CancelSubscription, CancelToken, Thread};
use shell_ipc_w::tabs::host::{NativeBrowserRegistration, ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady, ViewCreationResult};
use tauri_runtime_cef::{FrameEventKind, NativeContextToken};

use super::accepted::{self, PreparedEditorView};
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

pub(super) struct Loaded {
    pub(super) document: NativeSavedDocument,
    pub(super) outcome: shell_contract::editors::SavedDocumentLoadOutcome,
}
pub(super) type Done = Box<dyn FnOnce(ViewCreationResult<Loaded>)>;
pub(super) type Capture = Box<dyn FnOnce(shell_contract::ViewId, NativeRequestOwner) -> Result<NativeRequestOwner, shell_contract::ipc::IpcError>>;
pub(super) type Seed = Box<dyn FnOnce(NativeSavedDocument, shell_contract::editors::Then<shell_contract::editors::SavedDocumentLoadOutcome>)>;

struct Pending {
    target: NativeSavedTarget,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    core: Arc<dyn Thread>,
    document: Option<NativeSavedDocument>,
    ready: Option<ViewCreationReady<shell_contract::ViewId>>,
    capture: Option<Capture>,
    registration: Option<shell_ipc_w::tabs::host::SavedManagerRegistration>,
    seed: Option<Seed>,
    outcome: Option<shell_contract::editors::SavedDocumentLoadOutcome>,
    loaded: Option<tauri_runtime_cef::NativeDocumentReady>,
    failure: Option<String>,
    cleaning: bool,
    done: Done,
    _losses: Vec<CancelSubscription>,
}
thread_local! { static PENDING: RefCell<HashMap<u64, Pending>> = RefCell::new(HashMap::new()); }
thread_local! { static FAILED: RefCell<HashMap<u64, Done>> = RefCell::new(HashMap::new()); }
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fail_prepared(prepared: PreparedEditorView, error: String, done: Done) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    FAILED.with(|entries| { entries.borrow_mut().insert(id, done); });
    let proof = prepared.proof.clone();
    let cx = prepared.cx.clone();
    let core = prepared.core.clone();
    let view = prepared.view;
    let created = prepared.generation.clone();
    drop(prepared);
    let cleanup = accepted::cleanup_proof(cx, view, created, proof);
    let retry = cleanup.clone();
    cleanup(Box::new(move |result| core.post(Box::new(move || {
        let cleanup = match result {
            Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
            Err(_) => ViewCreationCleanupState::Pending(retry),
        };
        if let Some(done) = FAILED.with(|entries| entries.borrow_mut().remove(&id)) {
            done(Err(ViewCreationFailure { error: TabError(error), cleanup }));
        }
    }))));
}

fn lifetime_tokens(
    target: &NativeSavedTarget, participants: &[NativeRequestOwner], cancel: &CancelToken,
    pending: &CancelToken, additional: &[CancelToken],
) -> Result<Vec<CancelToken>, String> {
    let mut tokens = target.output().loss_tokens()?;
    tokens.extend([target.created().loss_token(), target.cancel().clone(), target.pending().clone(), cancel.clone(), pending.clone()]);
    tokens.extend(target.additional().iter().cloned());
    tokens.extend(additional.iter().cloned());
    for owner in std::iter::once(target.output().source_owner())
        .chain(target.participants()).chain(participants.iter()) {
        tokens.push(owner.loss_token().ok_or("A saved-output participant has no native lifetime")?);
        tokens.push(owner.context.renderer().ok_or("A saved-output participant has no renderer lifetime")?.loss_token());
    }
    match target.path() {
        shell_contract::ipc::NativeSavedPath::DriveMoved(receipt)
        | shell_contract::ipc::NativeSavedPath::DriveFallback(receipt) =>
            tokens.extend(shell_ipc_a::drive::committed_reopen_losses(receipt)?),
        shell_contract::ipc::NativeSavedPath::Selected(_) => {}
    }
    Ok(tokens)
}

fn subscribe(id: u64, core: &Arc<dyn Thread>, tokens: Vec<CancelToken>) {
    let losses = tokens.into_iter().map(|token| {
        let core = core.clone();
        token.on_cancel(move || core.post(Box::new(move || fail(id, "The saved-output authority expired".into()))))
    }).collect();
    PENDING.with(|entries| {
        if let Some(state) = entries.borrow_mut().get_mut(&id) { state._losses = losses; }
    });
}

fn fail_owned_ready(
    ready: ViewCreationReady<shell_contract::ViewId>, core: Arc<dyn Thread>, error: String, done: Done,
) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    FAILED.with(|entries| { entries.borrow_mut().insert(id, done); });
    let cleanup = ready.cleanup;
    let retry = cleanup.clone();
    cleanup(Box::new(move |result| core.post(Box::new(move || {
        let cleanup = match result {
            Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
            Err(_) => ViewCreationCleanupState::Pending(retry),
        };
        if let Some(done) = FAILED.with(|entries| entries.borrow_mut().remove(&id)) {
            done(Err(ViewCreationFailure { error: TabError(error), cleanup }));
        }
    }))));
}

/// `prepared` has a blank/staged actor already adopted under the exact saved
/// target. The seed only queues retained input; parsing runs after the gate.
pub(super) fn start(
    mut prepared: PreparedEditorView,
    target: NativeSavedTarget,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    capture: Capture,
    registration: shell_ipc_w::tabs::host::SavedManagerRegistration,
    seed: Seed,
    done: Done,
) {
    if prepared.view != target.view() || prepared.surface != target.surface()
        || !prepared.generation.same_generation(target.created())
        || !prepared.proof.reservation().same_reservation(target.reservation())
        || !prepared.proof.owner().same_owner(target.browser()) {
        return fail_prepared(prepared, "The saved creator does not match its native reservation".into(), done);
    }
    let tokens = match lifetime_tokens(&target, &participants, &cancel, &pending, &additional) {
        Ok(tokens) => tokens,
        Err(error) => return fail_prepared(prepared, error, done),
    };
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let core = prepared.core.clone();
    PENDING.with(|entries| { entries.borrow_mut().insert(id, Pending {
        target: target.clone(), participants: participants.clone(), cancel: cancel.clone(), pending: pending.clone(),
        additional: additional.clone(), core: core.clone(), document: None, ready: None, capture: Some(capture), registration: Some(registration), seed: Some(seed), outcome: None, loaded: None,
        failure: None, cleaning: false, done, _losses: Vec::new(),
    }); });
    subscribe(id, &core, tokens);
    let views = prepared.cx.views.clone();
    let frame_target = target.clone();
    let frame_core = core.clone();
    let first = Arc::new(OnceLock::<NativeContextToken>::new());
    prepared.invocation_on_frame = Some(Arc::new(move |view, event| {
        if !event.is_main || view != frame_target.view() { return; }
        match &event.kind {
            FrameEventKind::NativeContextRegistered { origin } if event.browser_id == origin.browser_id() && event.frame_id == origin.frame_id() => {
                if let Some(original) = first.get() {
                    if original != origin { frame_core.post(Box::new(move || fail(id, "The saved editor document was replaced".into()))); }
                    return;
                }
                let result = (|| {
                    let owner = native_owner::capture_owner(&views, view, frame_target.created())?;
                    if owner.context.authority_ref::<NativeOwnerAuthority>().is_none_or(|proof| &proof.origin != origin) {
                        return Err("The saved editor registration does not match its native browser".into());
                    }
                    Ok(owner)
                })();
                if result.is_ok() { let _ = first.set(origin.clone()); }
                frame_core.post(Box::new(move || registered(id, result)));
            }
            FrameEventKind::NativeDocumentReady { origin } if first.get() == Some(origin)
                && event.browser_id == origin.browser_id() && event.frame_id == origin.frame_id() => {
                let ready = origin.document_ready().ok_or_else(|| "The saved editor ready evidence is unavailable".to_owned());
                frame_core.post(Box::new(move || loaded(id, ready)));
            }
            FrameEventKind::NativeContextRevoked { origin } if first.get() == Some(origin) =>
                frame_core.post(Box::new(move || fail(id, "The saved editor document was revoked".into()))),
            FrameEventKind::NativeRendererTerminated { origin, .. } if first.get() == Some(origin) =>
                frame_core.post(Box::new(move || fail(id, "The saved editor renderer terminated".into()))),
            FrameEventKind::NavigationFailed { .. } =>
                frame_core.post(Box::new(move || fail(id, "The saved editor navigation failed".into()))),
            _ => {}
        }
    }));
    let cx = prepared.cx.clone();
    let view = prepared.view;
    let created = prepared.generation.clone();
    let proof = prepared.proof.clone();
    let gate_target = target.clone();
    accepted::schedule_child(cx, core, view, created, Some(proof), Box::new(move || {
        let mut build = Some(Box::new(move || prepared.install()) as Box<dyn FnOnce() -> Result<crate::host::window::CreatedChild, accepted::InstallError> + Send>);
        let mut accepted = None;
        let extra: Vec<_> = additional.iter().collect();
        let admission = native_owner::with_saved_target_seed_effect(&gate_target, &participants, &cancel, &pending, &extra,
            &mut |_| { accepted = build.take(); Ok(()) });
        match (admission, accepted) {
            (Ok(()), Some(build)) => build(),
            (Err(error), _) => Err(accepted::InstallError { error: TabError(error), close: None, proof: None }),
            _ => Err(accepted::InstallError { error: TabError("The saved constructor was not admitted".into()), close: None, proof: None }),
        }
    }), Box::new(move |result| created_result(id, result)));
}

/// Continue a spare only after its real browser-owner replacement committed.
/// `ready` must carry the successor's captured cleanup and the original
/// registered owner; a registration alone is never treated as readiness.
pub(super) fn start_spare(
    ready: ViewCreationReady<shell_contract::ViewId>,
    target: NativeSavedTarget,
    core: Arc<dyn Thread>,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    capture: Capture,
    registration: shell_ipc_w::tabs::host::SavedManagerRegistration,
    seed: Seed,
    done: Done,
) {
    if ready.view != target.view()
        || !ready.reservation.reservation.same_reservation(target.reservation())
        || !ready.reservation.owner.same_owner(target.browser()) {
        return done(Err(ViewCreationFailure {
            error: TabError("The retained spare does not match the saved browser successor".into()),
            cleanup: ViewCreationCleanupState::Pending(ready.cleanup),
        }));
    }
    let original = match &ready.reservation.registration {
        NativeBrowserRegistration::Registered(owner) => owner.clone(),
        NativeBrowserRegistration::Unregistered =>
            return fail_owned_ready(ready, core, "The retained spare has no original registered owner".into(), done),
    };
    if original.view != target.view()
        || !original.context.renderer().is_some_and(|generation| generation.same_generation(target.created())) {
        return fail_owned_ready(ready, core, "The retained spare renderer does not match its original reservation".into(), done);
    }
    let native_ready = original.context.authority_ref::<NativeOwnerAuthority>()
        .filter(|proof| proof.view == original.view)
        .and_then(|proof| proof.origin.document_ready());
    let Some(native_ready) = native_ready else {
        return fail_owned_ready(ready, core, "The original spare document has not reported native readiness".into(), done);
    };
    let mut tokens = match lifetime_tokens(&target, &participants, &cancel, &pending, &additional) {
        Ok(tokens) => tokens,
        Err(error) => return fail_owned_ready(ready, core, error, done),
    };
    let Some(loss) = original.loss_token() else {
        return fail_owned_ready(ready, core, "The retained spare has no native lifetime".into(), done);
    };
    tokens.push(loss);
    tokens.push(target.created().loss_token());
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    PENDING.with(|entries| { entries.borrow_mut().insert(id, Pending {
        target, participants, cancel, pending, additional, core: core.clone(),
        document: None, ready: Some(ready), capture: Some(capture), registration: Some(registration), seed: Some(seed), outcome: None,
        loaded: Some(native_ready), failure: None, cleaning: false, done, _losses: Vec::new(),
    }); });
    subscribe(id, &core, tokens);
    registered(id, Ok(original));
}

fn registered(id: u64, result: Result<NativeRequestOwner, String>) {
    let raw = match result { Ok(owner) => owner, Err(error) => return fail(id, error) };
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else { return; };
    if state.failure.is_some() {
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return advance(id);
    }
    let Some(capture) = state.capture.take() else {
        state.failure = Some("The saved editor owner capture was already consumed".into());
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return advance(id);
    };
    let Some(registration) = state.registration.take() else {
        state.failure = Some("The saved editor manager registration was already consumed".into());
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return advance(id);
    };
    let target = state.target.clone();
    let mut participants = state.participants.clone();
    participants.push(raw.clone());
    let cancel = state.cancel.clone();
    let pending = state.pending.clone();
    let additional = state.additional.clone();
    PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
    // Actor capture is core-only and can reenter. Keep no state borrow or
    // native admission while it binds the actual staged actor document lease.
    let owner = match capture(raw.view, raw.clone()) {
        Ok(owner) => owner,
        Err(error) => return fail(id, error.message),
    };
    if PENDING.with(|entries| entries.borrow().get(&id).is_none_or(|state| state.failure.is_some() || state.cleaning)) {
        return advance(id);
    }
    let same_origin = raw.context.authority_ref::<NativeOwnerAuthority>()
        .zip(owner.context.authority_ref::<NativeOwnerAuthority>())
        .is_some_and(|(before, after)| before.origin == after.origin);
    if owner.view != raw.view || !same_origin
        || !owner.context.renderer().zip(raw.context.renderer())
            .is_some_and(|(left, right)| left.same_generation(right)) {
        return fail(id, "The saved actor owner does not match the original native registration".into());
    }
    let extra: Vec<_> = additional.iter().collect();
    let mut document = None;
    let mut registration = Some(registration);
    let result = native_owner::with_saved_target_registration_effect(&target, &owner, &participants,
        &cancel, &pending, &extra, &mut |transition| {
            let mut register = || {
                document = Some(target.register_from_native_admitted(owner.clone())?);
                Ok(())
            };
            registration.take().ok_or("The saved manager registration was already consumed")?(
                &target, &owner, transition, &mut register)
        });
    if let Err(error) = result { return fail(id, error); }
    let Some(document) = document else { return fail(id, "The saved editor did not register".into()); };
    registered_document(id, document);
}

fn registered_document(id: u64, document: NativeSavedDocument) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else { return; };
    if state.failure.is_some() {
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return advance(id);
    }
    let Some(seed) = state.seed.take() else {
        state.failure = Some("The saved editor seed was already consumed".into());
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return advance(id);
    };
    state.document = Some(document.clone());
    PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
    // No native gate or RefCell borrow crosses an arbitrary actor callback.
    // The actor itself gates its queue and final parser publication.
    seed(document, Box::new(move |result| seed_finished(id, result)));
}

fn seed_finished(id: u64, result: Result<shell_contract::editors::SavedDocumentLoadOutcome, shell_contract::ipc::IpcError>) {
    match result {
        Ok(outcome) => PENDING.with(|entries| { if let Some(state) = entries.borrow_mut().get_mut(&id) { state.outcome = Some(outcome); } }),
        Err(error) => return fail(id, error.message),
    }
    advance(id);
}
fn loaded(id: u64, result: Result<tauri_runtime_cef::NativeDocumentReady, String>) {
    let ready = match result { Ok(ready) => ready, Err(error) => return fail(id, error) };
    PENDING.with(|entries| { if let Some(state) = entries.borrow_mut().get_mut(&id) { state.loaded = Some(ready); } });
    advance(id);
}
fn created_result(id: u64, result: ViewCreationResult<shell_contract::ViewId>) {
    match result {
        Ok(ready) => PENDING.with(|entries| { if let Some(state) = entries.borrow_mut().get_mut(&id) { state.ready = Some(ready); } }),
        Err(failure) => {
            let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
            if let Some(state) = state { (state.done)(Err(failure)); }
            return;
        }
    }
    advance(id);
}
fn fail(id: u64, error: String) {
    PENDING.with(|entries| { if let Some(state) = entries.borrow_mut().get_mut(&id) {
        if state.failure.is_none() { state.failure = Some(error); }
    } });
    advance(id);
}
fn advance(id: u64) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else { return; };
    if state.cleaning { PENDING.with(|entries| { entries.borrow_mut().insert(id, state); }); return; }
    if state.failure.is_none() && state.loaded.is_some() && state.outcome.is_some() {
        if let (Some(document), Some(_)) = (&state.document, &state.ready) {
            let exact_ready = state.loaded.as_ref().is_some_and(|ready|
                document.owner().context.authority_ref::<NativeOwnerAuthority>()
                    .is_some_and(|proof| &proof.origin == ready.origin()));
            let extra: Vec<_> = state.additional.iter().collect();
            if exact_ready && native_owner::with_saved_document_input_effect(document, &state.participants, &state.cancel,
                &state.pending, &extra, &mut |_| Ok(())).is_ok() {
                let document = state.document.take().unwrap();
                let mut ready = state.ready.take().unwrap();
                ready.reservation.registration = NativeBrowserRegistration::Registered(document.owner().clone());
                let outcome = state.outcome.take().unwrap();
                return (state.done)(Ok(ViewCreationReady { view: Loaded { document, outcome }, cleanup: ready.cleanup, closure: ready.closure, reservation: ready.reservation }));
            }
            state.failure = Some("The saved-output authority expired before delivery".into());
        }
    }
    if state.failure.is_none() || state.ready.is_none() {
        PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
        return;
    }
    let cleanup = state.ready.as_ref().unwrap().cleanup.clone();
    let core = state.core.clone();
    state.cleaning = true;
    PENDING.with(|entries| { entries.borrow_mut().insert(id, state); });
    cleanup(Box::new(move |result| core.post(Box::new(move || cleanup_finished(id, result)))));
}
fn cleanup_finished(id: u64, result: Result<shell_contract::ipc::NativeBrowserRetirement, String>) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else { return; };
    let mut error = state.failure.take().unwrap_or_else(|| "The saved editor failed".into());
    let cleanup = match result {
        Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
        Err(reason) => {
            error.push_str(&format!("; native cleanup failed: {reason}"));
            ViewCreationCleanupState::Pending(state.ready.take().unwrap().cleanup)
        }
    };
    (state.done)(Err(ViewCreationFailure { error: TabError(error), cleanup }));
}
