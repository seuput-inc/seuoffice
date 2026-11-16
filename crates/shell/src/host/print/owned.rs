use super::*;
use crate::host::contents::owned as page;
use crate::native::WebviewExt;
use shell_contract::ipc::NativeInvocationTarget;
use shell_contract::ipc::NativeRequestOwner;

#[derive(Clone)]
enum Authority {
    Interactive,
    NativeInvocation {
        target: NativeInvocationTarget,
        participants: Vec<NativeRequestOwner>,
        actor: CancelToken,
    },
    ExistingDocument {
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        additional: Vec<CancelToken>,
    },
    ManyExtra {
        participants: Vec<NativeRequestOwner>,
        additional: Vec<CancelToken>,
        invocation: Option<NativeInvocationTarget>,
    },
}

struct Lifetime {
    stop: CancelToken,
    _losses: Vec<CancelSubscription>,
    _closed: CancelSubscription,
    target: Mutex<Option<crate::host::contents::OwnedPrintTarget>>,
    window: Arc<Mutex<Option<WebviewWindow>>>,
}
#[derive(Clone)]
pub(super) struct Owner {
    pub origin: NativeRequestOwner,
    pub destination: NativeRequestOwner,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    ui: Arc<dyn Thread>,
    lifetime: Arc<Lifetime>,
    authority: Authority,
}
impl Owner {
    pub fn new(
        cx: &HostContext,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        loaded: LoadSignal,
    ) -> Result<Self, String> {
        Self::build(
            cx,
            origin,
            destination,
            cancel,
            pending,
            loaded,
            Authority::Interactive,
        )
    }
    pub fn new_invocation(
        cx: &HostContext,
        target: NativeInvocationTarget,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        loaded: LoadSignal,
    ) -> Result<Self, String> {
        Self::build(
            cx,
            owner.clone(),
            owner,
            cancel,
            pending,
            loaded,
            Authority::NativeInvocation {
                target,
                participants: Vec::new(),
                actor: CancelToken::default(),
            },
        )
    }
    pub fn new_invocation_many(
        cx: &HostContext,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: CancelToken,
        pending: CancelToken,
        actor: CancelToken,
        loaded: LoadSignal,
    ) -> Result<Self, String> {
        let mut all = Vec::with_capacity(participants.len() + 1);
        all.push(origin.clone());
        all.extend_from_slice(participants);
        let owner = document.owner().clone();
        Self::build(
            cx,
            owner.clone(),
            owner,
            cancel,
            pending,
            loaded,
            Authority::NativeInvocation {
                target: document.target().clone(),
                participants: all,
                actor,
            },
        )
    }
    pub fn new_existing_document(
        cx: &HostContext,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: CancelToken,
        pending: CancelToken,
        additional: &[CancelToken],
        loaded: LoadSignal,
    ) -> Result<Self, String> {
        let owner = binding.owner().clone();
        Self::build(
            cx,
            owner.clone(),
            owner,
            cancel,
            pending,
            loaded,
            Authority::ExistingDocument {
                binding: binding.clone(),
                participants: participants.to_vec(),
                additional: additional.to_vec(),
            },
        )
    }
    pub fn new_many_extra(
        cx: &HostContext,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: CancelToken,
        pending: CancelToken,
        additional: &[CancelToken],
        invocation: Option<NativeInvocationTarget>,
        loaded: LoadSignal,
    ) -> Result<Self, String> {
        let mut all = Vec::with_capacity(participants.len() + 1);
        all.push(destination.clone());
        all.extend_from_slice(participants);
        Self::build(
            cx,
            origin,
            destination,
            cancel,
            pending,
            loaded,
            Authority::ManyExtra {
                participants: all,
                additional: additional.to_vec(),
                invocation,
            },
        )
    }
    fn build(
        cx: &HostContext,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        loaded: LoadSignal,
        authority: Authority,
    ) -> Result<Self, String> {
        let origin_loss = origin
            .loss_token()
            .ok_or("Print origin lifetime is unavailable.")?;
        let destination_loss = destination
            .loss_token()
            .ok_or("Print destination lifetime is unavailable.")?;
        let stop = CancelToken::default();
        let mut losses = Vec::new();
        for loss in [&origin_loss, &destination_loss, &cancel, &pending] {
            let stop = stop.clone();
            losses.push(loss.on_cancel(move || stop.cancel()));
        }
        if let Authority::NativeInvocation {
            target,
            participants,
            actor,
        } = &authority
        {
            for participant in participants {
                let loss = participant
                    .loss_token()
                    .ok_or("Print participant lifetime is unavailable.")?;
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
            let stop_on_actor = stop.clone();
            losses.push(actor.on_cancel(move || stop_on_actor.cancel()));
            let invocation_loss = target.operation().loss_token();
            let stop_on_loss = stop.clone();
            losses.push(invocation_loss.on_cancel(move || stop_on_loss.cancel()));
            for loss in [
                target.operation().cancel(),
                target.operation().shutdown(),
                &target.created().loss_token(),
            ] {
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
        }
        if let Authority::ExistingDocument {
            binding,
            participants,
            additional,
        } = &authority
        {
            for loss in additional {
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
            for owner in std::iter::once(binding.owner()).chain(participants.iter()) {
                let mut tokens = Vec::new();
                if let Some(document) = owner.context.document_lease() {
                    tokens.push(document.loss_token());
                }
                if let Some(document) = owner.pdf_document() {
                    tokens.push(document.loss_token());
                }
                if let Some(revision) = owner.pdf_revision() {
                    tokens.push(revision.loss_token());
                }
                for loss in tokens {
                    let stop = stop.clone();
                    losses.push(loss.on_cancel(move || stop.cancel()));
                }
            }
            for participant in participants {
                let loss = participant
                    .loss_token()
                    .ok_or("Print participant lifetime is unavailable.")?;
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
            for loss in [
                binding.operation().cancel().clone(),
                binding.operation().shutdown().clone(),
                binding.operation().loss_token(),
            ] {
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
        }
        if let Authority::ManyExtra {
            participants,
            additional,
            invocation,
        } = &authority
        {
            for owner in std::iter::once(&origin).chain(participants.iter()) {
                let loss = owner
                    .loss_token()
                    .ok_or("Print participant lifetime is unavailable.")?;
                losses.push(loss.on_cancel({
                    let stop = stop.clone();
                    move || stop.cancel()
                }));
                for loss in owner
                    .context
                    .document_lease()
                    .map(|value| value.loss_token())
                    .into_iter()
                    .chain(owner.pdf_document().map(|value| value.loss_token()))
                    .chain(owner.pdf_revision().map(|value| value.loss_token()))
                {
                    let stop = stop.clone();
                    losses.push(loss.on_cancel(move || stop.cancel()));
                }
            }
            for loss in additional {
                let stop = stop.clone();
                losses.push(loss.on_cancel(move || stop.cancel()));
            }
            if let Some(invocation) = invocation {
                for loss in [
                    invocation.created().loss_token(),
                    invocation.operation().cancel().clone(),
                    invocation.operation().shutdown().clone(),
                    invocation.operation().loss_token(),
                ] {
                    let stop = stop.clone();
                    losses.push(loss.on_cancel(move || stop.cancel()));
                }
            }
        }
        let window = Arc::new(Mutex::new(None::<WebviewWindow>));
        let closing = window.clone();
        let ui = cx.ui.clone();
        let closed = stop.on_cancel(move || {
            if let Ok(mut loaded) = loaded.lock() {
                if let Some(signal) = loaded.take() {
                    let _ = signal.try_send(());
                }
            }
            let window = closing.lock().ok().and_then(|mut window| window.take());
            if let Some(window) = window {
                ui.post(Box::new(move || {
                    let _ = window.destroy();
                }));
            }
        });
        let owner = Self {
            origin,
            destination,
            cancel,
            pending,
            authority,
            ui: cx.ui.clone(),
            lifetime: Arc::new(Lifetime {
                stop,
                _losses: losses,
                _closed: closed,
                window,
                target: Mutex::new(None),
            }),
        };
        owner.check()?;
        Ok(owner)
    }
    pub fn check(&self) -> Result<(), String> {
        self.effect(|| ())
    }
    pub fn bind_asset_authority(
        &self,
        guard: &crate::host::document_assets::TargetGuard,
    ) -> Result<(), String> {
        match &self.authority {
            Authority::Interactive => guard.set_owned_authority(
                &self.origin,
                &self.destination,
                &self.cancel,
                &self.pending,
            ),
            Authority::NativeInvocation {
                target,
                participants,
                actor,
            } => guard.set_native_invocation_authority_many(
                target,
                &self.origin,
                participants,
                &self.cancel,
                &self.pending,
                actor,
            ),
            Authority::ExistingDocument {
                binding,
                participants,
                additional,
            } => guard.set_existing_document_authority(
                binding,
                participants,
                &self.cancel,
                &self.pending,
                &self.lifetime.stop,
                additional,
            ),
            Authority::ManyExtra {
                participants,
                additional,
                invocation,
            } => guard.set_many_extra_authority(
                &self.origin,
                participants,
                &self.cancel,
                &self.pending,
                &self.lifetime.stop,
                additional,
                invocation.as_ref(),
            ),
        }
    }
    pub fn effect<R>(&self, action: impl FnOnce() -> R) -> Result<R, String> {
        let admitted = match &self.authority {
            Authority::Interactive => crate::ipc::native_owner::with_paired_effect(
                &self.origin,
                &self.destination,
                &self.cancel,
                &self.pending,
                || self.lifetime.stop.with_active(action),
            ),
            Authority::NativeInvocation {
                target,
                participants,
                actor,
            } => crate::ipc::native_owner::with_native_invocation_print_many_effect(
                target,
                &self.origin,
                participants,
                &self.cancel,
                &self.pending,
                actor,
                &self.lifetime.stop,
                target.created(),
                || Some(action()),
            ),
            Authority::ExistingDocument {
                binding,
                participants,
                additional,
            } => crate::ipc::native_owner::with_existing_document_print_effect(
                binding,
                participants,
                &self.cancel,
                &self.pending,
                &self.lifetime.stop,
                binding.operation().authority_generation(),
                &additional.iter().collect::<Vec<_>>(),
                || Some(action()),
            ),
            Authority::ManyExtra {
                participants,
                additional,
                invocation,
            } => {
                let generation = self
                    .origin
                    .context
                    .renderer()
                    .ok_or("Print origin renderer is unavailable.")?;
                let controls = additional.iter().collect::<Vec<_>>();
                match invocation {
                    Some(target) => {
                        crate::ipc::native_owner::with_native_invocation_print_many_extra_effect(
                            target,
                            &self.origin,
                            participants,
                            &self.cancel,
                            &self.pending,
                            &self.lifetime.stop,
                            generation,
                            &controls,
                            || Some(action()),
                        )
                    }
                    None => crate::ipc::native_owner::with_owned_print_many_extra_effect(
                        &self.origin,
                        participants,
                        &self.cancel,
                        &self.pending,
                        &self.lifetime.stop,
                        generation,
                        &controls,
                        || Some(action()),
                    ),
                }
            }
        };
        admitted
            .flatten()
            .ok_or_else(|| "Print request expired.".into())
    }
    pub fn worker(&self) -> Result<(), String> {
        if self.ui.is_current() || crate::host::w::on_core_local(|_| {}) {
            return Err("Owned printing must run on a worker.".into());
        }
        self.check()
    }
    pub fn on_ui<R: Send + 'static>(
        &self,
        action: impl FnOnce() -> Result<R, String> + Send + 'static,
    ) -> Result<R, String> {
        self.worker()?;
        let (tx, rx) = mpsc::sync_channel(1);
        let lost = tx.clone();
        let _loss = self.lifetime.stop.on_cancel(move || {
            let _ = lost.try_send(Err("Print request expired.".to_owned()));
        });
        self.ui.post(Box::new(move || {
            let _ = tx.try_send(action());
        }));
        match rx.recv_timeout(LOAD_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.stop();
                Err("ERR_TIMED_OUT: print construction was not completed".into())
            }
        }
    }
    pub fn accepted<R>(&self, action: impl FnOnce() -> R) -> Result<R, String> {
        if !self.ui.is_current() {
            return Err("Print construction requires the UI thread.".into());
        }
        let accepted = self.effect(|| action)?;
        Ok(accepted())
    }
    pub fn retain_window(&self, window: &WebviewWindow) -> Result<(), String> {
        *self
            .lifetime
            .window
            .lock()
            .map_err(|_| "Print window lifetime poisoned.")? = Some(window.clone());
        if self.lifetime.stop.is_cancelled() || self.check().is_err() {
            let window = window.clone();
            self.ui.post(Box::new(move || {
                let _ = window.destroy();
            }));
            return Err("Print request expired.".into());
        }
        Ok(())
    }
    pub fn invalidate_target(&self) {
        let target = self
            .lifetime
            .target
            .lock()
            .ok()
            .and_then(|mut target| target.take());
        if let Some(target) = target {
            target.generation.revoke();
        }
    }

    pub fn bind_target(&self, webview: &tauri::Webview) -> Result<(), String> {
        self.worker()?;
        let frame = webview.native_browser()?;
        let document = frame
            .document()
            .ok_or_else(|| "The print document is not ready.".to_owned())?;
        let target = crate::host::contents::OwnedPrintTarget {
            frame,
            document,
            generation: shell_contract::runtime::RendererGeneration::new(),
        };
        self.check()?;
        let old = self
            .lifetime
            .target
            .lock()
            .map_err(|_| "Print document identity poisoned.")?
            .replace(target);
        if let Some(old) = old {
            old.generation.revoke();
        }
        Ok(())
    }

    pub fn check_drawio_target(
        &self,
        grant: &crate::drawio_scheme::print::PrintGrant,
    ) -> Result<(), String> {
        if !grant.matches_target(&self.target()?)? {
            return Err("The diagram print document changed before binding.".into());
        }
        self.check()
    }

    fn target(&self) -> Result<crate::host::contents::OwnedPrintTarget, String> {
        self.lifetime
            .target
            .lock()
            .map_err(|_| "Print document identity poisoned.")?
            .clone()
            .ok_or_else(|| "The print document is not ready.".into())
    }
    pub fn stop(&self) {
        self.lifetime.stop.cancel();
    }
    pub fn loss_token(&self) -> CancelToken {
        self.lifetime.stop.clone()
    }
    pub fn is_stopped(&self) -> bool {
        self.lifetime.stop.is_cancelled()
    }
    /// Navigates the one print browser. A navigation the browser refuses, fails or loses
    /// before it commits ends the print.
    pub fn navigate(&self, webview: &tauri::Webview, url: Url) -> Result<(), String> {
        self.worker()?;
        let (tx, rx) = mpsc::sync_channel(1);
        let owner = self.clone();
        webview.with_native(move |native| {
            let result = owner
                .effect(|| {
                    let ended = owner.clone();
                    native
                        .identity
                        .navigator()
                        .navigate_observed(url, move |outcome| {
                            if outcome.is_err() {
                                ended.stop();
                            }
                        })
                        .map(drop)
                })
                .and_then(|result| result);
            let _ = tx.try_send(result);
        })?;
        rx.recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "Print navigation was not admitted.".to_owned())??;
        self.check()
    }

    /// The page operations' authority: what `effect` admits, as the contents host builds it.
    fn page_owner(&self) -> Result<page::Owner, String> {
        let stop = self.lifetime.stop.clone();
        let (authority, actor) = match &self.authority {
            Authority::Interactive => (
                page::Authority::Pair {
                    origin: self.origin.clone(),
                    destination: self.destination.clone(),
                },
                stop,
            ),
            Authority::NativeInvocation {
                target,
                participants,
                actor,
            } => (
                page::Authority::Native {
                    invocation: target.clone(),
                    owner: self.origin.clone(),
                    participants: participants.clone(),
                    stop,
                },
                actor.clone(),
            ),
            Authority::ExistingDocument {
                binding,
                participants,
                additional,
            } => (
                page::Authority::ExistingDocument {
                    binding: binding.clone(),
                    participants: participants.clone(),
                    additional: additional.clone(),
                    stop: stop.clone(),
                },
                stop,
            ),
            Authority::ManyExtra {
                participants,
                additional,
                invocation,
            } => (
                match invocation {
                    Some(target) => page::Authority::NativeManyExtra {
                        invocation: target.clone(),
                        owner: self.origin.clone(),
                        participants: participants.clone(),
                        additional: additional.clone(),
                        stop: stop.clone(),
                    },
                    None => page::Authority::OwnedMany {
                        origin: self.origin.clone(),
                        participants: participants.clone(),
                        additional: additional.clone(),
                        stop: stop.clone(),
                    },
                },
                stop,
            ),
        };
        Ok(page::Owner {
            authority,
            cancel: self.cancel.clone(),
            pending: self.pending.clone(),
            actor,
            target: self.target()?,
        })
    }
    pub fn evaluate(
        &self,
        webview: &tauri::Webview,
        js: &str,
        await_promise: bool,
    ) -> Result<Value, crate::native::PageError> {
        use crate::native::PageError;
        self.worker().map_err(PageError::Platform)?;
        self.page_owner()
            .map_err(PageError::Platform)?
            .evaluate(webview, js, await_promise)
    }
    pub fn print_pdf(
        &self,
        webview: &tauri::Webview,
        spec: crate::native::PdfSpec,
    ) -> Result<Vec<u8>, String> {
        self.worker()?;
        self.page_owner()?.print_pdf(webview, spec)
    }
}
