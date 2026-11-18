use super::*;
use shell_contract::runtime::{RendererGeneration, Views};
use shell_ipc_w::tabs::host::GeneratedViewCommit;
use shell_ipc_w::tabs::host::{ViewCreationCleanup, ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady, ViewCreationResult};

type Done = Box<dyn FnOnce(ViewCreationResult<GeneratedViewCommit>)>;

#[derive(Default)]
pub(super) struct GeneratedBarrierSlot(std::sync::Mutex<GeneratedBarrierState>);

#[derive(Default)]
struct GeneratedBarrierState {
    barrier: Option<shell_ipc_w::tabs::host::GeneratedPrecloseBarrier>,
    failure: Option<shell_ipc_w::tabs::host::GeneratedPrecloseFailure>,
    passed: bool,
    running: bool,
    waiters: Vec<Box<dyn FnOnce() + Send>>,
}

impl GeneratedBarrierSlot {
    pub(super) fn installer(self: &Arc<Self>) -> shell_ipc_w::tabs::host::GeneratedPrecloseInstaller {
        let slot = self.clone();
        Box::new(move |barrier, failure| {
            let mut state = slot.0.lock().map_err(|_| "The generated preclose barrier is unavailable")?;
            if state.barrier.is_some() || state.passed || state.running || !state.waiters.is_empty() {
                return Err("The generated preclose barrier was already installed or used".into());
            }
            state.barrier = Some(barrier);
            state.failure = Some(failure);
            drop(state);
            let retry = Arc::downgrade(&slot);
            Ok(Arc::new(move || { if let Some(slot) = retry.upgrade() { slot.attempt(); } }) as shell_ipc_w::tabs::host::GeneratedPrecloseRetry)
        })
    }

    pub(super) fn is_installed(&self) -> bool {
        self.0.lock().ok().is_some_and(|state| state.barrier.is_some() && !state.passed && !state.running)
    }

    fn wait(self: &Arc<Self>, done: Box<dyn FnOnce() + Send>) {
        let immediate = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.passed || state.barrier.is_none() {
                state.passed = true;
                Some(done)
            } else {
                state.waiters.push(done);
                None
            }
        };
        if let Some(done) = immediate { done(); } else { self.attempt(); }
    }

    fn attempt(self: &Arc<Self>) {
        let barrier = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.passed || state.running || state.waiters.is_empty() { return; }
            let Some(barrier) = state.barrier.clone() else { return; };
            state.running = true;
            barrier
        };
        let slot = self.clone();
        barrier(Box::new(move |result| {
            let (waiters, failure) = {
                let mut state = slot.0.lock().unwrap_or_else(|error| error.into_inner());
                state.running = false;
                match result {
                    Ok(()) => {
                        state.passed = true;
                        (std::mem::take(&mut state.waiters), None)
                    }
                    Err(error) => (Vec::new(), state.failure.clone().map(|failure| (failure, error))),
                }
            };
            if let Some((failure, error)) = failure { failure(error); }
            for waiter in waiters { waiter(); }
        }));
    }
}


struct Pending {
    cx: HostContext,
    view: ViewId,
    created: RendererGeneration,
    url: String,
    loading: bool,
    done: Done,
    failure: Option<TabError>,
    close: Option<tauri_runtime_cef::NativeBrowserClose>,
    binding: shell_ipc_w::tabs::host::NativeBrowserBinding,
    register: Option<shell_ipc_w::tabs::host::GeneratedManagerRegistration>,
    cleanup: ViewCreationCleanup,
    source: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    request_pending: CancelToken,
    closing: bool,
    native: Option<tauri::Webview>,
    registered: Option<(tauri_runtime_cef::NativeContextToken,NativeRequestOwner)>,
    ready: Option<tauri_runtime_cef::NativeContextToken>,
    attached: bool,
    navigation: Option<Result<(), String>>,
    installation_settled: bool,
}

thread_local! { static PENDING: RefCell<HashMap<u64, Pending>> = RefCell::new(HashMap::new()); }

fn cleanup_for(pending: &Pending) -> ViewCreationCleanup { pending.cleanup.clone() }

pub(crate) fn schedule_generated(prepared: PreparedEditorView, owner: NativeRequestOwner, participants: Vec<NativeRequestOwner>, cancel: CancelToken, request_pending: CancelToken, binding: shell_ipc_w::tabs::host::NativeBrowserBinding, register: shell_ipc_w::tabs::host::GeneratedManagerRegistration, cleanup: ViewCreationCleanup, done: Done) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let ui = prepared.cx.ui.clone();
    let task_ui = ui.clone();
    let core = prepared.core.clone();
    let view = prepared.view;
    let created = prepared.generation.clone();
    let cx = prepared.cx.clone();
    PENDING.with(|pending| { pending.borrow_mut().insert(id, Pending {
        cx: cx.clone(), view, created: created.clone(), url: prepared.url.clone(), loading: false, done, failure: None,
        close: Some(prepared.proof.close()), binding, register: Some(register), cleanup,
        source: owner.clone(), participants: participants.clone(), cancel: cancel.clone(), request_pending: request_pending.clone(),
        closing: false, native: None, registered: None, ready: None, attached: false, navigation: None, installation_settled: false,
    }); });
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_accepted_created_ui_many(task_ui.as_ref(), &owner, &participants, &created, &cancel, &request_pending, move || prepared.install_mode(Some(id)))
            .map_err(|error| InstallError { error: TabError(error), close: None, proof: None }).and_then(|result| result);
        let finish = Box::new(move |result: Result<crate::host::window::CreatedChild, InstallError>| {
            core.post(Box::new(move || match result {
                Ok(child) => {
                    let (failure, navigation) = PENDING.with(|entries| {
                        let mut entries = entries.borrow_mut();
                        entries.get_mut(&id).map(|pending| {
                            pending.close = Some(child.close);
                            pending.native = Some(child.webview);
                            pending.attached = true;
                            pending.installation_settled = true;
                            (pending.failure.take(), pending.navigation.take())
                        }).unwrap_or((None, None))
                    });
                    if let Some(error) = failure { fail(id, error); }
                    else if let Some(navigation) = navigation { navigation_ready(id, navigation); }
                }
                Err(error) => {
                    PENDING.with(|entries| {
                        if let Some(pending) = entries.borrow_mut().get_mut(&id) {
                            pending.installation_settled = true;
                            if error.close.is_some() { pending.close = error.close.clone(); }
                        }
                    });
                    fail(id, error.error);
                }
            }));
        });
        match result {
            Ok(child) => await_creation(child, finish),
            Err(error) => finish(Err(error)),
        }
    }));
}

pub(super) fn constructed(core: Arc<dyn shell_contract::runtime::Thread>, id: u64, native:tauri::Webview, close: tauri_runtime_cef::NativeBrowserClose) {
    core.post(Box::new(move || {
        PENDING.with(|entries| {
            if let Some(pending) = entries.borrow_mut().get_mut(&id) {
                pending.native = Some(native);
                pending.close = Some(close);
            }
        });
    }));
}

pub(crate) fn discard_failed_spare(cx:HostContext,view:ViewId,created:RendererGeneration,native:tauri::Webview,proof:crate::host::window::NativeClosureProof,barrier:Arc<GeneratedBarrierSlot>,error:TabError,done:Done) {
    let id=NEXT.fetch_add(1,Ordering::Relaxed);
    fail_pending(id,Pending { cx,view,created,url:String::new(),loading:false,done,failure:None,close:Some(proof.close()),proof,barrier,closing:false,native:Some(native),registered:None,ready:None,attached:true,navigation:None,installation_settled:true },error);
}

pub(super) fn navigation_ready(id: u64, result: Result<(), String>) {
    let pending = PENDING.with(|pending| pending.borrow_mut().remove(&id));
    let Some(mut pending) = pending else { return; };
    if !pending.attached {
        pending.navigation = Some(result);
        PENDING.with(|entries| { entries.borrow_mut().insert(id, pending); });
        return;
    }
    if let Err(error) = result { return fail_pending(id, pending, TabError(error)); }
    if !pending.created.is_active() { return fail_pending(id, pending, TabError("The generated document was closed before initialization".into())); }
    if pending.loading { return fail_pending(id, pending, TabError("The generated document load was already started".into())); }
    pending.loading = true;
    let Some(native) = pending.native.clone() else {
        return fail_pending(id, pending, TabError("The generated native view is unavailable".into()));
    };
    let url = match crate::app_scheme::webview_url(&pending.url) {
        Some(url) => url,
        None => return fail_pending(id, pending, TabError("The generated renderer page is unavailable".into())),
    };
    let ui = pending.cx.ui.clone();
    let task_ui = ui.clone();
    let source = pending.source.clone();
    let participants = pending.participants.clone();
    let cancel = pending.cancel.clone();
    let request_pending = pending.request_pending.clone();
    let created = pending.created.clone();
    PENDING.with(|entries| { entries.borrow_mut().insert(id, pending); });
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_accepted_created_ui_many(
            task_ui.as_ref(), &source, &participants, &created, &cancel, &request_pending,
            || native.navigate(url).map_err(|error| error.to_string()),
        ).and_then(|result| result);
        if let Err(error) = result {
            crate::host::w::post_core(move |_, _| fail(id, TabError(error)));
        }
    }));
}

fn fail_pending(id: u64, mut pending: Pending, error: TabError) {
    if !pending.installation_settled {
        pending.failure = Some(error);
        PENDING.with(|entries| { entries.borrow_mut().insert(id, pending); });
        return;
    }
    if pending.closing {
        PENDING.with(|entries| { entries.borrow_mut().insert(id, pending); });
        return;
    }
    let cleanup = cleanup_for(&pending);
    pending.failure = Some(error);
    pending.closing = true;
    PENDING.with(|entries| { entries.borrow_mut().insert(id, pending); });
    cleanup(Box::new(move |result| {
        crate::host::w::post_core(move |_, _| finish_cleanup(id, result));
    }));
}

fn finish_cleanup(id: u64, result: Result<shell_contract::ipc::NativeBrowserRetirement, String>) {
    let pending = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut pending) = pending else { return; };
    let mut error = pending.failure.take().expect("cleanup owns its original failure");
    let cleanup = match result {
        Err(cleanup) => {
            error.0 = format!("{}; generated view cleanup failed: {cleanup}", error.0);
            ViewCreationCleanupState::Pending(cleanup_for(&pending))
        }
        Ok(receipt) => {
            if matches!(&receipt, shell_contract::ipc::NativeBrowserRetirement::Closed(_)) {
                pending.cx.views.remove_view_if_generation(pending.view, &pending.created);
            }
            ViewCreationCleanupState::Acknowledged(receipt)
        }
    };
    (pending.done)(Err(ViewCreationFailure { error, cleanup }));
}

fn fail(id: u64, error: TabError) {
    let pending = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    if let Some(pending) = pending { fail_pending(id, pending, error); }
}

fn post_pending(job:impl FnOnce()+Send+'static) {
    if let Some(core)=NATIVE_WINDOWS.get().and_then(|host|host.core.get()) {
        core.post(Box::new(job));
    }
}

fn pending_for_view(view:ViewId)->Option<u64> {
    PENDING.with(|entries|entries.borrow().iter().find_map(|(id,pending)|
        (pending.view==view && pending.failure.is_none()).then_some(*id)))
}

pub(crate) fn generated_load_failed(view:ViewId) {
    post_pending(move || {
        if let Some(id)=pending_for_view(view) { fail(id,TabError("The generated editor failed to load".into())); }
    });
}

pub(crate) fn generated_native_registered(view:ViewId,origin:tauri_runtime_cef::NativeContextToken) {
    post_pending(move || {
        let Some(id)=pending_for_view(view) else { return; };
        let pending=PENDING.with(|entries|entries.borrow_mut().remove(&id));
        let Some(mut pending)=pending else { return; };
        if !pending.loading {
            PENDING.with(|entries|{entries.borrow_mut().insert(id,pending);});
            return;
        }
        if let Some((registered,_))=&pending.registered {
            if registered!=&origin { return fail_pending(id,pending,TabError("The initial native document registration was replaced".into())); }
        } else {
            let target = match crate::ipc::native_owner::capture_owner(&pending.cx.views, view, &pending.created) {
                Ok(target) => target, Err(error) => return fail_pending(id, pending, TabError(error)),
            };
            if target.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
                .is_none_or(|authority| authority.origin != origin) {
                return fail_pending(id, pending, TabError("The initial document registered another native context".into()));
            }
            let Some(register) = pending.register.take() else {
                return fail_pending(id, pending, TabError("The generated manager registration was already consumed".into()));
            };
            let mut participants = pending.participants.clone();
            participants.push(target.clone());
            let mut published = None;
            let result = crate::ipc::native_owner::with_created_many_effect(
                &pending.source, &participants, &pending.created, &pending.cancel, &pending.request_pending, || {
                    pending.binding.reservation.with_owner(&pending.binding.owner, |transition| {
                        register(&target, transition, &mut || {
                            published = Some((origin.clone(), target.clone()));
                            Ok(())
                        })
                    })
                },
            ).unwrap_or_else(|| Err("The generated registration authority expired".into()));
            if let Err(error) = result { return fail_pending(id, pending, TabError(error)); }
            let Some(registered) = published else {
                return fail_pending(id, pending, TabError("The generated registration was not published".into()));
            };
            pending.registered = Some(registered);
        }
        finish_native_ready(id,pending);
    });
}

pub(crate) fn generated_document_ready(view:ViewId,origin:tauri_runtime_cef::NativeContextToken) {
    post_pending(move || {
        let Some(id)=pending_for_view(view) else { return; };
        let pending=PENDING.with(|entries|entries.borrow_mut().remove(&id));
        let Some(mut pending)=pending else { return; };
        if pending.loading { pending.ready=Some(origin); }
        finish_native_ready(id,pending);
    });
}

pub(crate) fn generated_native_revoked(view:ViewId,origin:tauri_runtime_cef::NativeContextToken) {
    post_pending(move || {
        let Some(id)=pending_for_view(view) else { return; };
        let revoked=PENDING.with(|entries|entries.borrow().get(&id)
            .is_some_and(|pending|pending.registered.as_ref().is_some_and(|(registered,_)|registered==&origin)));
        if revoked { fail(id,TabError("The initial native document was revoked before completion".into())); }
    });
}

fn finish_native_ready(id:u64,mut pending:Pending) {
    let matching=pending.ready.as_ref().zip(pending.registered.as_ref())
        .is_some_and(|(ready,(registered,_))|ready==registered);
    if !matching {
        PENDING.with(|entries|{entries.borrow_mut().insert(id,pending);});
        return;
    }
    let (_,target)=pending.registered.take().expect("matching native registration");
    let cleanup = cleanup_for(&pending);
    let mut reservation = pending.binding.clone();
    reservation.registration = shell_ipc_w::tabs::host::NativeBrowserRegistration::Registered(target.clone());
    let closure = reservation.closure.clone();
    match GeneratedViewCommit::loaded(target,pending.created.clone()) {
        Ok(receipt)=>(pending.done)(Ok(ViewCreationReady { view: receipt, cleanup, closure, reservation })),
        Err(error)=>fail_pending(id,pending,error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn preclose_failure_retains_cleanup_until_retry_succeeds() {
        let slot = Arc::new(GeneratedBarrierSlot::default());
        let pending = Arc::new(std::sync::Mutex::new(None::<Box<dyn FnOnce(Result<(), String>) + Send>>));
        let operation = pending.clone();
        let failures = Arc::new(AtomicUsize::new(0));
        let reported = failures.clone();
        let retry = slot.installer()(
            Arc::new(move |done| *operation.lock().unwrap() = Some(done)),
            Arc::new(move |_| { reported.fetch_add(1, Ordering::SeqCst); }),
        ).unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let effect = released.clone();
        slot.wait(Box::new(move || { effect.fetch_add(1, Ordering::SeqCst); }));
        pending.lock().unwrap().take().unwrap()(Err("hydration failed".into()));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        retry();
        pending.lock().unwrap().take().unwrap()(Ok(()));
        assert_eq!(released.load(Ordering::SeqCst), 1);
        retry();
        assert!(pending.lock().unwrap().is_none());
    }
}
