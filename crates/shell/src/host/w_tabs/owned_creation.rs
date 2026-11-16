use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use crate::native::{DocumentContext, NativeEventKind};
use main_core::shell::tab_manager::TabError;
use shell_contract::{
    ViewId,
    ipc::NativeRequestOwner,
    runtime::{CancelSubscription, CancelToken, RendererGeneration, Thread},
};
use shell_ipc_w::tabs::host::{
    ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady, ViewCreationResult,
};

use super::accepted::{self, PreparedEditorView};
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

type Done = Box<dyn FnOnce(ViewCreationResult<NativeRequestOwner>)>;
struct Pending {
    source: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    created: RendererGeneration,
    core: Arc<dyn Thread>,
    owner: Option<NativeRequestOwner>,
    ready: Option<ViewCreationReady<ViewId>>,
    failure: Option<String>,
    cleaning: bool,
    done: Done,
    _losses: Vec<CancelSubscription>,
}
thread_local! { static PENDING: RefCell<HashMap<u64, Pending>> = RefCell::new(HashMap::new()); }
thread_local! { static FAILED: RefCell<HashMap<u64, Done>> = RefCell::new(HashMap::new()); }
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fail_prepared(prepared: PreparedEditorView, error: TabError, done: Done) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    FAILED.with(|entries| {
        entries.borrow_mut().insert(id, done);
    });
    let proof = prepared.proof.clone();
    let cx = prepared.cx.clone();
    let core = prepared.core.clone();
    let view = prepared.view;
    let created = prepared.generation.clone();
    drop(prepared);
    let cleanup = accepted::cleanup_proof(cx, view, created, proof);
    let retry = cleanup.clone();
    cleanup(Box::new(move |result| {
        core.post(Box::new(move || {
            let cleanup = match result {
                Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
                Err(_) => ViewCreationCleanupState::Pending(retry),
            };
            if let Some(done) = FAILED.with(|entries| entries.borrow_mut().remove(&id)) {
                done(Err(ViewCreationFailure { error, cleanup }));
            }
        }))
    }));
}

pub(super) fn start(
    mut prepared: PreparedEditorView,
    source: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    initialize: shell_ipc_w::tabs::host::EditorInitializer,
    done: Done,
) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let view = prepared.view;
    let created = prepared.generation.clone();
    let core = prepared.core.clone();
    let views = prepared.cx.views.clone();
    let Some(source_loss) = source.loss_token() else {
        return fail_prepared(
            prepared,
            TabError("The source has no native lifetime".into()),
            done,
        );
    };
    if let Err(error) = initialize(
        view,
        &created,
        prepared.proof.reservation(),
        prepared.proof.owner(),
    ) {
        return fail_prepared(prepared, error, done);
    }
    let losses = [
        source_loss,
        cancel.clone(),
        pending.clone(),
        created.loss_token(),
    ]
    .into_iter()
    .map(|token| {
        let core = core.clone();
        token.on_cancel(move || {
            core.post(Box::new(move || {
                fail(id, "The editor creation authority expired".into())
            }))
        })
    })
    .collect();
    PENDING.with(|entries| {
        entries.borrow_mut().insert(
            id,
            Pending {
                source: source.clone(),
                cancel: cancel.clone(),
                pending: pending.clone(),
                created: created.clone(),
                core: core.clone(),
                owner: None,
                ready: None,
                failure: None,
                cleaning: false,
                done,
                _losses: losses,
            },
        );
    });
    let first = Arc::new(OnceLock::<DocumentContext>::new());
    let frame_core = core.clone();
    let frame_created = created.clone();
    prepared.invocation_on_frame = Some(Arc::new(move |event_view, event| {
        if event_view != view {
            return;
        }
        match &event.kind {
            NativeEventKind::DocumentRegistered { context: origin }
                if origin.browser().is_same_browser(&event.browser) =>
            {
                if let Some(old) = first.get() {
                    if old != origin {
                        frame_core.post(Box::new(move || {
                            fail(id, "The editor document was replaced".into())
                        }));
                    }
                    return;
                }
                let result = (|| {
                    let owner = native_owner::capture_owner(&views, view, &frame_created)?;
                    if owner
                        .context
                        .authority_ref::<NativeOwnerAuthority>()
                        .is_none_or(|proof| &proof.origin != origin)
                    {
                        return Err("The editor registration does not match its browser".to_owned());
                    }
                    Ok(owner)
                })();
                if result.is_ok() {
                    let _ = first.set(origin.clone());
                }
                frame_core.post(Box::new(move || registered(id, result)));
            }
            NativeEventKind::DocumentRevoked { document, .. }
                if first
                    .get()
                    .is_some_and(|origin| origin.identity() == document) =>
            {
                frame_core.post(Box::new(move || {
                    fail(id, "The editor document was revoked".into())
                }))
            }
            NativeEventKind::NavigationFailed { .. } => frame_core.post(Box::new(move || {
                fail(id, "The editor navigation failed".into())
            })),
            _ => {}
        }
    }));
    accepted::schedule(
        prepared,
        source,
        cancel,
        pending,
        Box::new(move |result| created_result(id, result)),
    );
}

fn registered(id: u64, result: Result<NativeRequestOwner, String>) {
    match result {
        Ok(owner) => PENDING.with(|entries| {
            if let Some(state) = entries.borrow_mut().get_mut(&id) {
                state.owner = Some(owner);
            }
        }),
        Err(error) => return fail(id, error),
    }
    advance(id);
}
fn created_result(id: u64, result: ViewCreationResult<ViewId>) {
    match result {
        Ok(ready) => PENDING.with(|entries| {
            if let Some(state) = entries.borrow_mut().get_mut(&id) {
                state.ready = Some(ready);
            }
        }),
        Err(failure) => {
            let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
            if let Some(state) = state {
                (state.done)(Err(failure));
            }
            return;
        }
    }
    advance(id);
}
fn fail(id: u64, error: String) {
    PENDING.with(|entries| {
        if let Some(state) = entries.borrow_mut().get_mut(&id) {
            if state.failure.is_none() {
                state.failure = Some(error);
            }
        }
    });
    advance(id);
}
fn advance(id: u64) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else {
        return;
    };
    if state.cleaning {
        PENDING.with(|entries| {
            entries.borrow_mut().insert(id, state);
        });
        return;
    }
    if state.failure.is_none() {
        if let (Some(owner), Some(_)) = (&state.owner, &state.ready) {
            if native_owner::with_created_many_effect(
                &state.source,
                std::slice::from_ref(owner),
                &state.created,
                &state.cancel,
                &state.pending,
                || (),
            )
            .is_some()
            {
                let owner = state.owner.take().unwrap();
                let ready = state.ready.take().unwrap();
                return (state.done)(Ok(ViewCreationReady {
                    view: owner,
                    cleanup: ready.cleanup,
                    closure: ready.closure,
                    reservation: ready.reservation,
                }));
            }
            state.failure = Some("The editor creation authority expired before delivery".into());
        }
    }
    if state.failure.is_none() || state.ready.is_none() {
        PENDING.with(|entries| {
            entries.borrow_mut().insert(id, state);
        });
        return;
    }
    let cleanup = state.ready.as_ref().unwrap().cleanup.clone();
    let core = state.core.clone();
    state.cleaning = true;
    PENDING.with(|entries| {
        entries.borrow_mut().insert(id, state);
    });
    cleanup(Box::new(move |result| {
        core.post(Box::new(move || cleanup_finished(id, result)))
    }));
}
fn cleanup_finished(id: u64, result: Result<shell_contract::ipc::NativeClosureReceipt, String>) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else {
        return;
    };
    let mut error = state
        .failure
        .take()
        .unwrap_or_else(|| "The editor creation failed".into());
    let cleanup = match result {
        Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
        Err(reason) => {
            error.push_str(&format!("; native cleanup failed: {reason}"));
            ViewCreationCleanupState::Pending(state.ready.take().unwrap().cleanup)
        }
    };
    (state.done)(Err(ViewCreationFailure {
        error: TabError(error),
        cleanup,
    }));
}
