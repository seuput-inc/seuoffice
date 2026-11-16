//! The initiating request and destination renderer are independent lifetimes.
mod close;
mod native_rename;
mod publication;
use super::*;
pub(crate) use native_rename::NativeRenameScope;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{NativeContext, NativeRequestOwner};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use std::cell::Cell;

#[derive(Clone)]
pub(crate) struct RecoveryRenameScope {
    pub(crate) receipt: shell_contract::ipc::RecoverySeedReceipt,
    pub(crate) receiver: NativeRequestOwner,
    pub(crate) destination: NativeRequestOwner,
    pub(crate) participants: Vec<NativeRequestOwner>,
    pub(crate) cancel: CancelToken,
    pub(crate) pending: CancelToken,
    pub(crate) effects: OwnedEffectsHost,
    pub(crate) work: shell_contract::ipc::RecoveryWorkTicket,
}

impl RecoveryRenameScope {
    pub(crate) fn effect<T>(
        &self,
        effect: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut effect = Some(effect);
        let mut result = None;
        self.effects.with_recovery_open_effect_many(
            &self.receipt,
            &self.receiver,
            &self.participants,
            &self.cancel,
            &self.pending,
            &mut || {
                result = Some(effect.take().ok_or("recovery rename effect repeated")?()?);
                Ok(())
            },
        )?;
        result.ok_or_else(|| "recovery rename effect was not admitted".into())
    }

    pub(crate) fn admit_file(
        &self,
        effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError> {
        let mut result = None;
        self.effect(|| {
            result = Some(effect());
            Ok(())
        })
        .map_err(|message| main_core::node_fs::NodeError {
            code: None,
            message,
        })?;
        result.unwrap_or_else(|| {
            Err(main_core::node_fs::NodeError {
                code: None,
                message: "recovery rename effect was not admitted".into(),
            })
        })
    }
}

thread_local! {
    static NEXT: Cell<u64> = const { Cell::new(1) };
    static WAITING: RefCell<HashMap<u64, (Weak<TextDocs>, ViewId)>> = RefCell::new(HashMap::new());
}
#[derive(Clone)]
pub(crate) struct OwnedSave(Arc<Request>);
#[derive(Clone)]
pub(crate) enum SaveAuthority {
    Renderer(NativeRequestOwner),
    Native(shell_contract::ipc::NativeExistingDocumentBinding),
}
struct SelectedState {
    revision: shell_contract::ipc::NativeSaveRevisionAuthority,
    snapshot: std::sync::Mutex<shell_contract::ipc::NativeSaveRevisionSnapshot>,
}
impl SelectedState {
    fn new(
        destination: shell_contract::ipc::NativeSaveDestination,
        aliases: Vec<NativeRequestOwner>,
    ) -> Result<Self, String> {
        let revision = shell_contract::ipc::NativeSaveRevisionAuthority::new(destination, aliases);
        let snapshot = revision.snapshot()?;
        Ok(Self {
            revision,
            snapshot: std::sync::Mutex::new(snapshot),
        })
    }
    fn snapshot(&self) -> Result<shell_contract::ipc::NativeSaveRevisionSnapshot, String> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| "the captured native save snapshot is unavailable".into())
    }
}
struct Request {
    default_dir: std::sync::OnceLock<String>,
    id: u64,
    authority: SaveAuthority,
    destination: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    effects: OwnedEffectsHost,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    mode: SaveMode,
    closing: bool,
    claimed: std::sync::atomic::AtomicBool,
    callback: std::sync::OnceLock<(NativeRequestOwner, CancelToken)>,
    selected: std::sync::OnceLock<SelectedState>,
    target: Option<PathBuf>,
    original_save: Option<String>,
    original_open: Option<String>,
    _subscriptions: Vec<CancelSubscription>,
}
impl OwnedSave {
    pub(crate) fn is_save_as(&self) -> bool {
        self.0.mode == SaveMode::SaveAs
    }
    pub(crate) fn target(&self) -> Option<&Path> {
        self.0.target.as_deref()
    }
    pub(crate) fn default_directory(&self) -> Option<&str> {
        self.0.default_dir.get().map(String::as_str)
    }
    pub(crate) fn cancel(&self) -> CancelToken {
        self.0.cancel.clone()
    }
    pub(crate) fn pending(&self) -> CancelToken {
        self.0.pending.clone()
    }
    pub(crate) fn additional(&self) -> &[CancelToken] {
        &self.0.additional
    }
    pub(crate) fn authority(&self) -> &SaveAuthority {
        &self.0.authority
    }
    pub(crate) fn origin(&self) -> Option<&NativeRequestOwner> {
        match &self.0.authority {
            SaveAuthority::Renderer(origin) => Some(origin),
            SaveAuthority::Native(_) => None,
        }
    }
    pub(crate) fn destination(&self) -> &NativeRequestOwner {
        &self.0.destination
    }
    pub(crate) fn participants(&self) -> &[NativeRequestOwner] {
        if self.0.participants.is_empty() {
            std::slice::from_ref(&self.0.destination)
        } else {
            &self.0.participants
        }
    }
    pub(crate) fn effects(&self) -> &OwnedEffectsHost {
        &self.0.effects
    }
    pub(crate) fn admit_file(
        &self,
        docs: &TextDocs,
        effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError> {
        let mut result = None;
        self.effect(docs, || {
            result = Some(effect());
            Ok(())
        })
        .map_err(|message| main_core::node_fs::NodeError {
            code: None,
            message,
        })?;
        result.unwrap_or_else(|| {
            Err(main_core::node_fs::NodeError {
                code: None,
                message: "save effect was not admitted".into(),
            })
        })
    }
    pub(crate) fn check_document(&self, docs: &TextDocs, view: ViewId) -> Result<(), String> {
        self.effect(docs, || Ok(()))?;
        if docs.save_path(view) != self.0.original_save
            || docs.open_path(view) != self.0.original_open
        {
            return Err("the destination document changed during save".into());
        }
        Ok(())
    }
    pub(crate) fn admit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match &self.0.authority {
            SaveAuthority::Renderer(origin) => {
                let participants = self.captured_participants();
                let extra = self.callback_cancels();
                let extra = extra.iter().collect::<Vec<_>>();
                self.0.effects.with_owned_effect_many_extra(
                    origin,
                    &participants,
                    &self.0.cancel,
                    &self.0.pending,
                    &extra,
                    effect,
                )
            }
            SaveAuthority::Native(binding) => {
                let callback = self.0.callback.get();
                let mut participants = self.participants().to_vec();
                if let Some((owner, _)) = callback {
                    participants.push(owner.clone());
                }
                let extra = self.callback_cancels();
                let extra = extra.iter().collect::<Vec<_>>();
                if let Some(selected) = self.0.selected.get() {
                    let snapshot = selected.snapshot()?;
                    self.0.effects.with_native_save_revision_snapshot_effect(
                        &snapshot,
                        &participants,
                        &self.0.cancel,
                        &self.0.pending,
                        &extra,
                        effect,
                    )
                } else {
                    self.0.effects.with_existing_document_save_effect_extra(
                        binding,
                        &participants,
                        &self.0.cancel,
                        &self.0.pending,
                        &extra,
                        effect,
                    )
                }
            }
        }
    }
    pub(crate) fn native_revision(
        &self,
    ) -> Option<&shell_contract::ipc::NativeSaveRevisionAuthority> {
        self.0.selected.get().map(|selected| &selected.revision)
    }
    pub(crate) fn native_destination(
        &self,
    ) -> Result<Option<shell_contract::ipc::NativeSaveDestination>, String> {
        self.0
            .selected
            .get()
            .map(|selected| {
                selected
                    .snapshot()
                    .map(|snapshot| snapshot.selection().clone())
            })
            .transpose()
    }
    pub(crate) fn captured_participants(&self) -> Vec<NativeRequestOwner> {
        let mut participants = self.participants().to_vec();
        if let Some((owner, _)) = self.0.callback.get() {
            participants.push(owner.clone());
        }
        participants
    }
    pub(crate) fn callback_cancels(&self) -> Vec<CancelToken> {
        let mut extra = self.0.additional.clone();
        if let Some((_, cancel)) = self.0.callback.get() {
            extra.push(cancel.clone());
        }
        extra
    }
    pub(crate) fn worker_cancel(&self) -> (CancelToken, Vec<CancelSubscription>) {
        let worker = CancelToken::default();
        let mut tokens = vec![self.cancel(), self.pending()];
        tokens.extend(self.callback_cancels());
        for owner in self.captured_participants() {
            if let Some(loss) = owner.loss_token() {
                tokens.push(loss);
            }
        }
        match self.authority() {
            SaveAuthority::Renderer(origin) => {
                if let Some(loss) = origin.loss_token() {
                    tokens.push(loss)
                }
            }
            SaveAuthority::Native(binding) => tokens.extend([
                binding.operation().cancel().clone(),
                binding.operation().shutdown().clone(),
                binding.operation().loss_token(),
            ]),
        }
        let subscriptions = tokens
            .into_iter()
            .map(|token| {
                let worker = worker.clone();
                token.on_cancel(move || worker.cancel())
            })
            .collect();
        (worker, subscriptions)
    }
    pub(crate) fn select_native_destination(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
    ) -> Result<(), String> {
        let SaveAuthority::Native(binding) = self.authority() else {
            return Err("the save is not a native operation".into());
        };
        if !destination.matches_binding(binding) {
            return Err("the save destination belongs to another operation".into());
        }
        let mut revision = Some(SelectedState::new(
            destination,
            vec![self.destination().clone()],
        )?);
        self.admit(&mut || {
            self.0
                .selected
                .set(
                    revision
                        .take()
                        .ok_or("the save destination was already consumed")?,
                )
                .map_err(|_| "the save destination was already selected".into())
        })
    }
    /// A native save whose first file the project writer placed at `path`
    /// selects exactly that file; a renderer save has no selection to make.
    pub(crate) fn select_project_destination(&self, path: &Path) -> Result<(), String> {
        let SaveAuthority::Native(binding) = self.authority() else {
            return Ok(());
        };
        self.select_native_destination(
            shell_contract::ipc::NativeSaveDestination::from_project_document(
                binding.clone(),
                path.to_owned(),
            )?,
        )
    }
    pub(crate) fn check_target(&self, path: &Path) -> Result<(), String> {
        if matches!(self.authority(), SaveAuthority::Native(_)) {
            let selected = self
                .native_destination()?
                .ok_or("the native save destination was not selected")?;
            if selected.path() != path {
                return Err("the save target differs from the selected destination".into());
            }
        }
        self.admit(&mut || Ok(()))
    }
    pub(crate) fn effect<T>(
        &self,
        _docs: &TextDocs,
        effect: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut effect = Some(effect);
        let mut result = None;
        self.admit(&mut || {
            let action = effect
                .take()
                .ok_or("save effect was admitted more than once")?;
            result = Some(action()?);
            Ok(())
        })?;
        result.ok_or_else(|| "save effect was not admitted".into())
    }
    pub(crate) fn matches(&self, native: Option<&NativeContext>) -> bool {
        native
            .and_then(NativeContext::renderer)
            .zip(self.0.destination.context.renderer())
            .is_some_and(|(a, b)| a.same_generation(b))
    }
}
impl TextDocs {
    pub(crate) fn capture_document_owner(
        &self,
        view: ViewId,
        mut native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        if native.view != view {
            return Err(kit::error("the native owner belongs to another document"));
        }
        let renderer = native
            .context
            .renderer()
            .ok_or_else(|| kit::error("the document owner has no renderer lifetime"))?;
        if !renderer.is_active()
            || !self
                .services
                .views
                .renderer_generation(view)
                .is_some_and(|current| current.same_generation(renderer) && current.is_active())
        {
            return Err(kit::error(
                "the document owner belongs to another renderer lifetime",
            ));
        }
        let lease = self
            .read_view(view, |state| state.tracked.then(|| state.lease.clone()))
            .flatten()
            .ok_or_else(|| kit::error("the document is no longer open"))?;
        if !lease.is_active() {
            return Err(kit::error("the document lifetime has ended"));
        }
        if let Some(previous) = native.context.document_lease() {
            if !previous.same_generation(&lease) || !previous.is_active() {
                return Err(kit::error(
                    "the document owner belongs to another document lifetime",
                ));
            }
        } else {
            native.context = native.context.with_document_lease(lease);
        }
        Ok(native)
    }
    pub(crate) fn take_owned_save(
        &self,
        view: ViewId,
        native: Option<&NativeContext>,
    ) -> Result<Option<OwnedSave>, String> {
        let pending = self.owned_saves.borrow().get(&view).cloned();
        if let Some(pending) = &pending {
            if !pending.matches(native) {
                return Err("save response belongs to another renderer lifetime".into());
            }
            pending.check_document(self, view)?;
            if pending
                .0
                .claimed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Err("a save response is already being applied".into());
            }
        }
        Ok(pending)
    }
    pub(crate) fn take_owned_save_from_call(
        &self,
        cx: &shell_contract::ipc::CallContext,
    ) -> Result<Option<OwnedSave>, String> {
        let view = cx.sender.view;
        let Some(request) = self.owned_saves.borrow().get(&view).cloned() else {
            return Ok(None);
        };
        if !request.matches(cx.native.as_ref()) {
            return Err("save response belongs to another renderer lifetime".into());
        }
        request.check_document(self, view)?;
        let native = cx
            .native
            .clone()
            .ok_or("the save response has no authenticated renderer")?;
        let actual = self
            .capture_document_owner(
                view,
                NativeRequestOwner {
                    view,
                    context: native,
                },
            )
            .map_err(|error| error.message)?;
        let mut participants = request.participants().to_vec();
        participants.push(actual.clone());
        if let Some(origin) = request.origin() {
            participants.push(origin.clone());
        }
        let mut claim = || {
            if request
                .0
                .claimed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Err("a save response is already being applied".into());
            }
            request
                .0
                .callback
                .set((actual.clone(), cx.cancel.clone()))
                .map_err(|_| "save callback was already captured".to_owned())?;
            Ok(())
        };
        match request.authority() {
            SaveAuthority::Renderer(_) => request.effects().with_same_native_origin_effect(
                request.destination(),
                &actual,
                &participants,
                &request.cancel(),
                &request.pending(),
                &[&cx.cancel],
                &mut claim,
            )?,
            SaveAuthority::Native(binding) => {
                request.effects().with_existing_document_save_effect_extra(
                    binding,
                    &participants,
                    &request.cancel(),
                    &request.pending(),
                    &[&cx.cancel],
                    &mut claim,
                )?
            }
        }
        Ok(Some(request))
    }
    fn begin_owned(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        target: Option<PathBuf>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        captured: Option<NativeRequestOwner>,
        closing: bool,
        then: Then<bool>,
    ) {
        if self.is_destroyed(view) {
            return then(Err(kit::error("the document is no longer open")));
        }
        if self.save_waiters.borrow().contains_key(&view)
            || self.owned_saves.borrow().contains_key(&view)
        {
            return then(Err(kit::error("a document save is already pending")));
        }
        let Some(expected) = self.services.views.renderer_generation(view) else {
            return then(Err(kit::error(
                "the destination renderer lifetime is unavailable",
            )));
        };
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err(kit::error("owned save effects are unavailable")));
        };
        let destination = match captured {
            Some(owner) => owner,
            None => match effects.capture_owner(view, &expected) {
                Ok(owner) => owner,
                Err(error) => return then(Err(kit::error(error))),
            },
        };
        let destination = match self.capture_document_owner(view, destination) {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        let Some(origin_loss) = origin.loss_token() else {
            return then(Err(kit::error(
                "the originating request lifetime is unavailable",
            )));
        };
        let combined = CancelToken::default();
        let mut subscriptions = Vec::new();
        let Some(destination_loss) = destination.loss_token() else {
            return then(Err(kit::error(
                "the destination native lifetime is unavailable",
            )));
        };
        for token in [cancel.clone(), origin_loss, destination_loss] {
            let combined = combined.clone();
            subscriptions.push(token.on_cancel(move || combined.cancel()));
        }
        let id = NEXT.with(|next| {
            let id = next.get();
            next.set(id.checked_add(1).expect("text save identities exhausted"));
            id
        });
        let core = self.services.core.clone();
        subscriptions.push(combined.on_cancel(move || {
            core.post(Box::new(move || {
                let waiting = WAITING.with(|waiting| waiting.borrow().get(&id).cloned());
                if let Some((docs, view)) = waiting {
                    if let Some(docs) = docs.upgrade() {
                        let active = docs
                            .owned_saves
                            .borrow()
                            .get(&view)
                            .is_some_and(|request| request.0.id == id);
                        if active {
                            docs.owned_saves.borrow_mut().remove(&view);
                            let waiter = docs.save_waiters.borrow_mut().remove(&view);
                            if let Some(waiter) = waiter {
                                waiter(false);
                            }
                        }
                    }
                }
            }))
        }));
        let request = OwnedSave(Arc::new(Request {
            default_dir: std::sync::OnceLock::new(),
            id,
            authority: SaveAuthority::Renderer(origin),
            destination,
            participants: Vec::new(),
            effects,
            pending: combined,
            cancel,
            additional: Vec::new(),
            mode,
            closing,
            claimed: std::sync::atomic::AtomicBool::new(false),
            callback: std::sync::OnceLock::new(),
            selected: std::sync::OnceLock::new(),
            target,
            original_save: self.save_path(view),
            original_open: self.open_path(view),
            _subscriptions: subscriptions,
        }));
        if let Err(error) = request.check_document(self, view) {
            return then(Err(kit::error(error)));
        }
        if (self.hooks.presenting)(view)
            || (request.target().is_none()
                && mode == SaveMode::Save
                && !self.is_dirty(view)
                && self.save_path(view).is_some())
        {
            return then(Ok(true));
        }
        WAITING.with(|waiting| {
            waiting.borrow_mut().insert(id, (Rc::downgrade(self), view));
        });
        self.owned_saves.borrow_mut().insert(view, request.clone());
        let docs = Rc::downgrade(self);
        let finished = request.clone();
        self.arm(
            &self.save_waiters,
            view,
            Box::new(move |result| {
                if let Some(docs) = docs.upgrade() {
                    let same = docs
                        .owned_saves
                        .borrow()
                        .get(&view)
                        .is_some_and(|r| r.0.id == id);
                    if same {
                        docs.owned_saves.borrow_mut().remove(&view);
                    }
                }
                WAITING.with(|waiting| {
                    waiting.borrow_mut().remove(&id);
                });
                let cancelled =
                    finished.cancel().is_cancelled() || finished.pending().is_cancelled();
                if !matches!(&result, Ok(true)) {
                    finished.pending().cancel();
                }
                if cancelled {
                    then(Err(kit::error("document save request was cancelled")))
                } else {
                    then(result)
                }
            }),
        );
        let mode = if mode == SaveMode::SaveAs {
            "saveAs"
        } else {
            "save"
        };
        let (channel, args) = if closing {
            (self.ch.close_save_request, Vec::new())
        } else {
            (self.ch.save_request, vec![mode.into_ipc()])
        };
        if let Err(_) = request.effect(self, || {
            self.services.events.emit(Target::View(view), channel, args);
            Ok(())
        }) {
            self.owned_saves.borrow_mut().remove(&view);
            let waiter = self.save_waiters.borrow_mut().remove(&view);
            if let Some(waiter) = waiter {
                waiter(false);
            }
        }
    }
    pub(crate) fn request_save_owned(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        self.begin_owned(view, mode, None, origin, cancel, None, false, then)
    }
    fn begin_native(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        target: Option<PathBuf>,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        if self.is_destroyed(view) {
            return then(Err(kit::error("the document is no longer open")));
        }
        if self.save_waiters.borrow().contains_key(&view)
            || self.owned_saves.borrow().contains_key(&view)
        {
            return then(Err(kit::error("a document save is already pending")));
        }
        if binding.owner().view != view {
            return then(Err(kit::error(
                "the native save binding belongs to another document",
            )));
        }
        let surface = if self.ch.app == "html" {
            shell_contract::Surface::Html
        } else {
            shell_contract::Surface::Markdown
        };
        let selection_matches = match binding.operation().selection() {
            shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                shell_contract::ipc::NativeEditorCommand::SaveDocument {
                    surface: actual,
                    view: actual_view,
                    selection,
                },
            ) if *actual == surface && *actual_view == view => {
                match (selection, target.as_deref()) {
                    (shell_contract::ipc::NativeSaveSelection::Mode(selected), None) => {
                        *selected == mode
                    }
                    (
                        shell_contract::ipc::NativeSaveSelection::ExactPath(selected),
                        Some(target),
                    ) => selected == target,
                    _ => false,
                }
            }
            _ => false,
        };
        if !selection_matches {
            return then(Err(kit::error(
                "the native save request differs from its selected operation",
            )));
        }
        let destination = match self.capture_document_owner(view, binding.owner().clone()) {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err(kit::error("owned save effects are unavailable")));
        };
        let Some(loss) = destination.loss_token() else {
            return then(Err(kit::error(
                "the destination native lifetime is unavailable",
            )));
        };
        let id = NEXT.with(|next| {
            let id = next.get();
            next.set(id.checked_add(1).expect("text save identities exhausted"));
            id
        });
        let signal = CancelToken::default();
        let mut subscriptions = Vec::new();
        for token in [
            cancel.clone(),
            pending.clone(),
            loss,
            binding.operation().cancel().clone(),
            binding.operation().shutdown().clone(),
            binding.operation().loss_token(),
        ] {
            let signal = signal.clone();
            subscriptions.push(token.on_cancel(move || signal.cancel()));
        }
        let core = self.services.core.clone();
        subscriptions.push(signal.on_cancel(move || {
            core.post(Box::new(move || {
                let waiting = WAITING.with(|waiting| waiting.borrow().get(&id).cloned());
                if let Some((docs, view)) = waiting {
                    if let Some(docs) = docs.upgrade() {
                        let active = docs.owned_saves.borrow().get(&view).is_some_and(|request| {
                            request.0.id == id
                                && !request.0.claimed.load(std::sync::atomic::Ordering::Acquire)
                        });
                        if active {
                            docs.owned_saves.borrow_mut().remove(&view);
                            let waiter = docs.save_waiters.borrow_mut().remove(&view);
                            if let Some(waiter) = waiter {
                                waiter(false);
                            }
                        }
                    }
                }
            }))
        }));
        let request = OwnedSave(Arc::new(Request {
            default_dir: std::sync::OnceLock::new(),
            id,
            authority: SaveAuthority::Native(binding),
            destination,
            participants: Vec::new(),
            effects,
            cancel,
            pending,
            additional: Vec::new(),
            mode,
            closing: false,
            claimed: std::sync::atomic::AtomicBool::new(false),
            callback: std::sync::OnceLock::new(),
            selected: std::sync::OnceLock::new(),
            target,
            original_save: self.save_path(view),
            original_open: self.open_path(view),
            _subscriptions: subscriptions,
        }));
        if let Err(error) = request.check_document(self, view) {
            return then(Err(kit::error(error)));
        }
        let SaveAuthority::Native(binding) = request.authority() else {
            return then(Err(kit::error("the native save binding is unavailable")));
        };
        let selection = request.effect(self, || {
            let selected = if let Some(target) = request.target() {
                let selected = shell_contract::ipc::NativeSaveDestination::from_exact_selection(
                    binding.clone(),
                )?;
                if selected.path() != target {
                    return Err("the requested path does not match the native selection".into());
                }
                Some(selected)
            } else if mode == SaveMode::Save {
                self.save_path(view)
                    .filter(|path| !path.is_empty())
                    .map(|path| {
                        shell_contract::ipc::NativeSaveDestination::from_current_document(
                            binding.clone(),
                            PathBuf::from(path),
                        )
                    })
                    .transpose()?
            } else {
                None
            };
            if let Some(selected) = selected {
                request
                    .0
                    .selected
                    .set(SelectedState::new(
                        selected,
                        vec![request.destination().clone()],
                    )?)
                    .map_err(|_| "the native destination was already selected")?;
            }
            Ok(())
        });
        if let Err(error) = selection {
            return then(Err(kit::error(error)));
        }
        if (self.hooks.presenting)(view)
            || (request.target().is_none()
                && mode == SaveMode::Save
                && !self.is_dirty(view)
                && self.save_path(view).is_some())
        {
            return then(Ok(true));
        }
        WAITING.with(|waiting| {
            waiting.borrow_mut().insert(id, (Rc::downgrade(self), view));
        });
        self.owned_saves.borrow_mut().insert(view, request.clone());
        let docs = Rc::downgrade(self);
        let finished = request.clone();
        self.arm(
            &self.save_waiters,
            view,
            Box::new(move |result| {
                if let Some(docs) = docs.upgrade() {
                    let same = docs
                        .owned_saves
                        .borrow()
                        .get(&view)
                        .is_some_and(|r| r.0.id == id);
                    if same {
                        docs.owned_saves.borrow_mut().remove(&view);
                    }
                }
                WAITING.with(|waiting| {
                    waiting.borrow_mut().remove(&id);
                });
                match finished.admit(&mut || Ok(())) {
                    Err(error) => then(Err(kit::error(error))),
                    Ok(()) => then(result),
                }
            }),
        );
        let mode = if mode == SaveMode::SaveAs {
            "saveAs"
        } else {
            "save"
        };
        if let SaveAuthority::Native(binding) = request.authority() {
            if request
                .effects()
                .emit_existing_document_save_many(
                    binding,
                    request.participants(),
                    0,
                    &request.cancel(),
                    &request.pending(),
                    self.ch.save_request,
                    vec![mode.into_ipc()],
                )
                .is_err()
            {
                self.owned_saves.borrow_mut().remove(&view);
                let waiter = self.save_waiters.borrow_mut().remove(&view);
                if let Some(waiter) = waiter {
                    waiter(false);
                }
            }
        }
    }
    pub(crate) fn request_save_native(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        self.begin_native(view, mode, None, binding, cancel, pending, then)
    }
    pub(crate) fn save_to_path_native(
        self: &Rc<Self>,
        view: ViewId,
        path: PathBuf,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<()>,
    ) {
        self.begin_native(
            view,
            SaveMode::Save,
            Some(path),
            binding,
            cancel,
            pending,
            Box::new(move |result| {
                then(result.and_then(|ok| {
                    if ok {
                        Ok(())
                    } else {
                        Err(kit::error("could not save the document"))
                    }
                }))
            }),
        );
    }
    pub(crate) fn save_to_path_owned(
        self: &Rc<Self>,
        view: ViewId,
        path: PathBuf,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<()>,
    ) {
        self.begin_owned(
            view,
            SaveMode::Save,
            Some(path),
            origin,
            cancel,
            None,
            false,
            Box::new(move |result| {
                then(result.and_then(|ok| {
                    if ok {
                        Ok(())
                    } else {
                        Err(kit::error("could not save the document"))
                    }
                }))
            }),
        );
    }
}

impl TextDocs {
    pub(crate) fn save_dialog_owned(
        self: &Rc<Self>,
        view: ViewId,
        options: JsObject<JsonValue>,
        request: OwnedSave,
        then: impl FnOnce(Result<Option<String>, String>) + 'static,
    ) {
        use main_core::packages::electron_utils::dialog_memory::{
            prepare_dialog_options, remember_save_path,
        };
        let prepared = prepare_dialog_options(&self.remembered(), &options, None);
        let parent = self.window_of(view);
        match request.authority() {
            SaveAuthority::Renderer(origin) => {
                let docs = Rc::clone(self);
                let guard = request.clone();
                let done = kit::back_on_core(
                    &self.services,
                    move |result: Result<Option<PathBuf>, String>| {
                        let picked = match result {
                            Ok(p) => p,
                            Err(error) => return then(Err(error)),
                        };
                        if let Some(path) =
                            picked.as_ref().filter(|path| !path.as_os_str().is_empty())
                        {
                            if let Err(error) = guard.effect(&docs, || {
                                remember_save_path(&mut docs.remembered(), &path.to_string_lossy());
                                Ok(())
                            }) {
                                return then(Err(error));
                            }
                        }
                        then(Ok(picked.map(|p| p.to_string_lossy().into_owned())));
                    },
                );
                self.dialogs.save_owned(
                    parent,
                    crate::text_recovery::ui::file_dialog(&prepared),
                    origin.clone(),
                    request.destination().clone(),
                    request.cancel(),
                    request.pending(),
                    done,
                )
            }
            SaveAuthority::Native(binding) => {
                let docs = Rc::clone(self);
                let guard = request.clone();
                let html = self.ch.app == "html";
                let save_as = request.is_save_as();
                let done = kit::back_on_core(
                    &self.services,
                    move |result: Result<
                        Option<shell_contract::ipc::NativeSaveDestination>,
                        String,
                    >| {
                        let selected = match result {
                            Err(error) => return then(Err(error)),
                            Ok(None) => return then(Ok(None)),
                            Ok(Some(selected)) => selected,
                        };
                        let selected = if !html || save_as {
                            let format = if html {
                                shell_contract::ipc::NativeSaveFormat::Html
                            } else {
                                shell_contract::ipc::NativeSaveFormat::Markdown
                            };
                            match selected.normalize_dialog_format(format) {
                                Ok(selected) => selected,
                                Err(error) => return then(Err(error)),
                            }
                        } else {
                            selected
                        };
                        let raw = selected.raw_path().to_string_lossy().into_owned();
                        let path = selected.path().to_string_lossy().into_owned();
                        if let Err(error) = guard.select_native_destination(selected) {
                            return then(Err(error));
                        }
                        if let Err(error) = guard.effect(&docs, || {
                            remember_save_path(&mut docs.remembered(), &raw);
                            Ok(())
                        }) {
                            return then(Err(error));
                        }
                        then(Ok(Some(path)))
                    },
                );
                self.dialogs.save_existing_document(
                    parent,
                    crate::text_recovery::ui::file_dialog(&prepared),
                    binding.clone(),
                    request.captured_participants(),
                    request.cancel(),
                    request.pending(),
                    request.callback_cancels(),
                    done,
                )
            }
        }
    }
}

impl TextDocs {
    fn saved_to_owned(
        &self,
        view: ViewId,
        target: &str,
        request: &OwnedSave,
    ) -> Result<(), String> {
        request.check_document(self, view)?;
        request.effect(self, || {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .ok_or("the document is no longer open")?;
            state.save = Some(target.to_owned());
            state.open = Some(target.to_owned());
            state.allowed.insert(target.to_owned());
            state.dirty = false;
            Ok(())
        })?;
        let tracked = self.read_view(view, |state| state.tracked).unwrap_or(false);
        (self.hooks.asset_paths_owned)(request, tracked, Some(target), Some(target))
    }
    fn saved_to_native(
        self: &Rc<Self>,
        view: ViewId,
        target: String,
        request: OwnedSave,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let destination = match request.authority() {
            SaveAuthority::Native(_) => request.destination().clone(),
            SaveAuthority::Renderer(_) => {
                return then(Err("native save binding is unavailable".into()));
            }
        };
        if let Err(error) = request.check_document(self, view) {
            return then(Err(error));
        }
        let tracked = self.read_view(view, |state| state.tracked).unwrap_or(false);
        let prepare = self.hooks.native_save_asset_paths.clone();
        let worker_target = target.clone();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(
                        &destination,
                        tracked,
                        Some(&worker_target),
                        Some(&worker_target),
                    )
                }))
                .unwrap_or_else(|_| Err("native save asset preparation panicked".into()))
            },
            move |stage| match stage {
                Err(error) => then(Err(error)),
                Ok(stage) => docs.commit_native_save_paths(view, target, request, stage, then),
            },
        );
    }

    fn commit_native_save_paths(
        self: &Rc<Self>,
        view: ViewId,
        target: String,
        request: OwnedSave,
        mut stage: super::super::RecoveryAssetStage,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let waiter = (stage.subscribe)();
        let applied = request.effect(self, || {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .ok_or("the document is no longer open")?;
            if !state.tracked
                || state.save != request.0.original_save
                || state.open != request.0.original_open
            {
                return Err("the document changed while its save was prepared".into());
            }
            if !(stage.commit)()? {
                return Ok(false);
            }
            state.save = Some(target.clone());
            state.open = Some(target.clone());
            state.allowed.insert(target.clone());
            state.dirty = false;
            Ok(true)
        });
        match applied {
            Err(error) => then(Err(error)),
            Ok(true) => then(Ok(())),
            Ok(false) => {
                let SaveAuthority::Native(binding) = request.authority() else {
                    return then(Err("native save binding is unavailable".into()));
                };
                let Some(loss) = request.destination().loss_token() else {
                    return then(Err(
                        "the native save destination lifetime is unavailable".into()
                    ));
                };
                let tokens = vec![
                    request.cancel(),
                    request.pending(),
                    loss,
                    binding.operation().cancel().clone(),
                    binding.operation().shutdown().clone(),
                    binding.operation().loss_token(),
                ];
                let docs = Rc::clone(self);
                kit::on_worker(
                    &self.services,
                    move || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waiter(tokens)))
                            .unwrap_or_else(|_| Err("native save asset wait panicked".into()))
                    },
                    move |result| {
                        if let Err(error) = result {
                            return then(Err(error));
                        }
                        docs.commit_native_save_paths(view, target, request, stage, then);
                    },
                );
            }
        }
    }
    pub(crate) fn write_owned(
        self: &Rc<Self>,
        input: super::owned_write::Input,
        before: Option<RecoveryIdentity>,
        request: OwnedSave,
        then: impl FnOnce(Result<IpcValue, String>) + 'static,
    ) {
        let view = input.view;
        let current = input.current.clone();
        let target = input.target.clone();
        if let Err(error) = request.check_document(self, view) {
            return then(Err(error));
        }
        let docs = Rc::clone(self);
        let worker = request.clone();
        kit::on_worker(
            &self.services,
            move || super::owned_write::Committed::commit(input, &worker),
            move |committed| {
                let mut committed = match committed {
                    Ok(value) => value,
                    Err(error) => return then(Err(error)),
                };
                let native = matches!(request.authority(), SaveAuthority::Native(_));
                let state_docs = Rc::clone(&docs);
                let state_request = request.clone();
                let state_target = target.clone();
                let continuation: Box<dyn FnOnce(Result<(), String>)> = Box::new(move |saved| {
                    if let Err(error) = saved {
                        return then(Err(error));
                    }
                    let services = docs.services.clone();
                    let recovery = docs.recovery.clone();
                    let cleanup = request.clone();
                    recovery.finish_owned(view,before,main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::Saved,cleanup,move|cleaned|{
                    if let Err(error)=cleaned{return then(Err(error))}
                    let disk=committed.disk.take();
                    if let Err(error)=request.effect(&docs,||{
                        let mut views=docs.views.borrow_mut();let state=views.get_mut(&view).ok_or("the document is no longer open")?;
                        if let Some(disk)=disk{state.disk.insert(target.clone(),disk);}
                        if let Some(current)=current.filter(|current|current!=&target){state.disk.remove(&current);}Ok(())
                    }){return then(Err(error))}
                    kit::on_worker(&services,move||committed.reconcile(&request),then);
                });
                });
                if native {
                    state_docs.saved_to_native(view, state_target, state_request, continuation)
                } else {
                    let result = state_docs.saved_to_owned(view, &state_target, &state_request);
                    continuation(result)
                }
            },
        );
    }
}

impl TextDocs {
    pub(crate) fn file_renamed_recovery(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() {
            return then(Err(kit::error(
                "the destination document owner was not captured before its move",
            )));
        }
        let destination = match self.capture_document_owner(view, destination.clone()) {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        if let Some(active) = self.owned_saves.borrow().get(&view) {
            let origin_captured = active.origin().is_some_and(|origin| {
                std::iter::once(receiver)
                    .chain(participants.iter())
                    .any(|owner| Arc::ptr_eq(&owner.context.as_arc(), &origin.context.as_arc()))
            });
            if !active.cancel().same_token(cancel)
                || !active.pending().same_token(pending)
                || !Arc::ptr_eq(
                    &active.destination().context.as_arc(),
                    &destination.context.as_arc(),
                )
                || !origin_captured
            {
                return then(Err(kit::error(
                    "document rename conflicts with its pending save",
                )));
            }
        }
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err(kit::error("recovery rename effects are unavailable")));
        };
        let work = match receipt.begin_work() {
            Ok(work) => work,
            Err(error) => return then(Err(kit::error(error))),
        };
        let participants = std::iter::once(destination.clone())
            .chain(participants.iter().cloned())
            .collect();
        let scope = RecoveryRenameScope {
            receipt: receipt.clone(),
            receiver: receiver.clone(),
            destination,
            participants,
            cancel: cancel.clone(),
            pending: pending.clone(),
            effects,
            work,
        };
        let old = old_path.to_string_lossy().into_owned();
        let new = new_path.to_string_lossy().into_owned();
        let before = match scope.effect(|| {
            let views = self.views.borrow();
            let state = views.get(&view).ok_or("the document is no longer open")?;
            if state.save.as_deref() != Some(old.as_str()) {
                return Err("the saved document changed before its move".into());
            }
            Ok((state.tracked, state.open.clone(), state.save.clone()))
        }) {
            Ok(value) => value,
            Err(error) => return then(Err(kit::error(error))),
        };
        let next_open = if before.1.as_deref() == Some(old.as_str()) {
            Some(new.clone())
        } else {
            before.1.clone()
        };
        let next_save = Some(new.clone());
        let tracked = before.0;
        let prepare = self.hooks.rename_recovery_asset_paths.clone();
        let worker_scope = scope.clone();
        let worker_open = next_open.clone();
        let worker_save = next_save.clone();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                let _work = worker_scope.work.clone();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(
                        &worker_scope,
                        tracked,
                        worker_open.as_deref(),
                        worker_save.as_deref(),
                    )
                }))
                .unwrap_or_else(|_| Err("the recovery asset preparation worker panicked".into()))
            },
            move |prepared| {
                let grant = match prepared {
                    Ok(grant) => grant,
                    Err(error) => return then(Err(kit::error(error))),
                };
                docs.commit_recovery_rename(
                    view, old, new, before, next_open, next_save, scope, grant, then,
                );
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_recovery_rename(
        self: &Rc<Self>,
        view: ViewId,
        old: String,
        new: String,
        before: (bool, Option<String>, Option<String>),
        next_open: Option<String>,
        next_save: Option<String>,
        scope: RecoveryRenameScope,
        mut grant: super::super::RecoveryAssetStage,
        then: Then<()>,
    ) {
        let waiter = (grant.subscribe)();
        let applied = scope.effect(|| {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .ok_or("the document is no longer open")?;
            if state.tracked != before.0 || state.open != before.1 || state.save != before.2 {
                return Err("the document changed while its recovery rename was prepared".into());
            }
            if !(grant.commit)()? {
                return Ok(false);
            }
            state.save = next_save.clone();
            state.open = next_open.clone();
            if state.allowed.contains(&old) {
                state.allowed.insert(new.clone());
            }
            if let Some(recorded) = state.disk.remove(&old) {
                state.disk.insert(new.clone(), recorded);
            }
            Ok(true)
        });
        match applied {
            Err(error) => return then(Err(kit::error(error))),
            Ok(false) => {
                let mut tokens = vec![
                    scope.cancel.clone(),
                    scope.pending.clone(),
                    scope.receipt.target().created().loss_token(),
                ];
                for owner in std::iter::once(&scope.receiver).chain(scope.participants.iter()) {
                    let Some(loss) = owner.loss_token() else {
                        return then(Err(kit::error(
                            "a recovery rename owner lifetime is unavailable",
                        )));
                    };
                    tokens.push(loss);
                }
                if let shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation) =
                    scope.receipt.target().context()
                {
                    tokens.extend([
                        operation.cancel().clone(),
                        operation.shutdown().clone(),
                        operation.loss_token(),
                    ]);
                }
                let docs = Rc::clone(self);
                let work = scope.work.clone();
                return kit::on_worker(
                    &self.services,
                    move || {
                        let _work = work;
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waiter(tokens)))
                            .unwrap_or_else(|_| {
                                Err("the recovery asset wait worker panicked".into())
                            })
                    },
                    move |result| {
                        if let Err(error) = result {
                            return then(Err(kit::error(error)));
                        }
                        docs.commit_recovery_rename(
                            view, old, new, before, next_open, next_save, scope, grant, then,
                        );
                    },
                );
            }
            Ok(true) => {}
        }
        let html = self.ch.app == "html";
        let worker = scope.clone();
        let (from, to) = (old.clone(), new.clone());
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                let _work = worker.work.clone();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
                if html{main_core::html::asset_lifecycle::rename_owned_asset_document_authorized(&from,&to,&mut|effect|worker.admit_file(effect))}
                else{main_core::markdown::asset_lifecycle::rename_owned_asset_document_authorized(&from,&to,&mut|effect|worker.admit_file(effect))}
            })).map_err(|_|"the recovery asset rename worker panicked".to_owned())?.map_err(|error|error.message)
            },
            move |result| {
                if let Err(error) = result {
                    return then(Err(kit::error(error)));
                }
                let recovery = docs.recovery.clone();
                let recovery_scope = scope.clone();
                let next = new.clone();
                let finished = move |result: Result<(), String>| {
                    if let Err(error) = result {
                        return then(Err(kit::error(error)));
                    }
                    let result = scope.effects.emit_recovery_open_many(
                        &scope.receipt,
                        &scope.receiver,
                        &scope.participants,
                        0,
                        &scope.cancel,
                        &scope.pending,
                        docs.ch.file_renamed,
                        vec![new.into_ipc()],
                    );
                    then(result.map_err(kit::error));
                };
                recovery.renamed_recovery_async(
                    view,
                    &old,
                    &next,
                    recovery_scope,
                    Box::new(finished),
                );
            },
        );
    }

    pub(crate) fn file_renamed_owned(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        self.file_renamed_many_owned_extra(
            view,
            old_path,
            new_path,
            origin,
            destination,
            &[],
            cancel,
            pending,
            &[],
            then,
        )
    }

    pub(crate) fn file_renamed_many_owned_extra(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() {
            return then(Err(kit::error(
                "the destination document owner was not captured before its move",
            )));
        }
        let destination = match self.capture_document_owner(view, destination.clone()) {
            Ok(destination) => destination,
            Err(error) => return then(Err(error)),
        };
        if let Some(active) = self.owned_saves.borrow().get(&view) {
            if !active.cancel().same_token(cancel)
                || !active.pending().same_token(pending)
                || !active.origin().is_some_and(|active_origin| {
                    Arc::ptr_eq(&active_origin.context.as_arc(), &origin.context.as_arc())
                })
                || !Arc::ptr_eq(
                    &active.destination().context.as_arc(),
                    &destination.context.as_arc(),
                )
            {
                return then(Err(kit::error(
                    "document rename conflicts with its pending save",
                )));
            }
        }
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err(kit::error("owned rename effects are unavailable")));
        };
        let participants = if participants.is_empty() {
            Vec::new()
        } else {
            std::iter::once(destination.clone())
                .chain(participants.iter().cloned())
                .collect()
        };
        let request = OwnedSave(Arc::new(Request {
            default_dir: std::sync::OnceLock::new(),
            id: 0,
            authority: SaveAuthority::Renderer(origin.clone()),
            destination,
            participants,
            effects,
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
            mode: SaveMode::Save,
            closing: false,
            claimed: std::sync::atomic::AtomicBool::new(true),
            callback: std::sync::OnceLock::new(),
            selected: std::sync::OnceLock::new(),
            target: None,
            original_save: self.save_path(view),
            original_open: self.open_path(view),
            _subscriptions: Vec::new(),
        }));
        if let Err(error) = request.check_document(self, view) {
            return then(Err(kit::error(error)));
        }
        let old = old_path.to_string_lossy().into_owned();
        let new = new_path.to_string_lossy().into_owned();
        if self.save_path(view).as_deref() != Some(old.as_str()) {
            return then(Err(kit::error(
                "the saved document changed before its move",
            )));
        }
        if let Err(error) = request.effect(self, || {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .ok_or("the document is no longer open")?;
            if state.save.as_deref() == Some(old.as_str()) {
                state.save = Some(new.clone());
            }
            if state.open.as_deref() == Some(old.as_str()) {
                state.open = Some(new.clone());
            }
            if state.allowed.contains(&old) {
                state.allowed.insert(new.clone());
            }
            if let Some(recorded) = state.disk.remove(&old) {
                state.disk.insert(new.clone(), recorded);
            }
            Ok(())
        }) {
            return then(Err(kit::error(error)));
        }
        let (tracked, open, save) = self
            .read_view(view, |state| {
                (state.tracked, state.open.clone(), state.save.clone())
            })
            .unwrap_or_default();
        if let Err(error) =
            (self.hooks.asset_paths_owned)(&request, tracked, open.as_deref(), save.as_deref())
        {
            return then(Err(kit::error(error)));
        }
        let html = self.ch.app == "html";
        let docs = Rc::clone(self);
        let worker = request.clone();
        let (from, to) = (old.clone(), new.clone());
        kit::on_worker(
            &self.services,
            move || {
                if html {
                    main_core::html::asset_lifecycle::rename_owned_asset_document_authorized(
                        &from,
                        &to,
                        &mut |effect| super::owned_write::admit(&worker, effect),
                    )
                } else {
                    main_core::markdown::asset_lifecycle::rename_owned_asset_document_authorized(
                        &from,
                        &to,
                        &mut |effect| super::owned_write::admit(&worker, effect),
                    )
                }
            },
            move |renamed| {
                if let Err(error) = renamed {
                    return then(Err(kit::error(error.message)));
                }
                let follows = docs.save_path(view).as_deref() == Some(new.as_str());
                let recovery = docs.recovery.clone();
                let scope = request.clone();
                let new_path = new.clone();
                let finished = move |result: Result<(), String>| {
                    if let Err(error) = result {
                        return then(Err(kit::error(error)));
                    }
                    let Some(origin) = request.origin() else {
                        return then(Err(kit::error("rename has no renderer origin")));
                    };
                    let extra = request.callback_cancels();
                    let extra = extra.iter().collect::<Vec<_>>();
                    let result = request.effects().emit_owned_many_extra(
                        origin,
                        request.participants(),
                        0,
                        &request.cancel(),
                        &request.pending(),
                        &extra,
                        docs.ch.file_renamed,
                        vec![new.into_ipc()],
                    );
                    then(result.map_err(kit::error));
                };
                if follows {
                    recovery.renamed_owned_async(view, &old, &new_path, scope, Box::new(finished));
                } else {
                    finished(Ok(()));
                }
            },
        );
    }
}

impl TextDocs {
    pub(crate) fn conflict_stops_save_owned(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        auto: bool,
        request: OwnedSave,
        then: impl FnOnce(Result<bool, String>) + 'static,
    ) {
        if let Err(error) = request.check_document(self, view) {
            return then(Err(error));
        }
        if !(self.hooks.open_elsewhere)(&path) {
            return then(Ok(false));
        }
        let recorded = self
            .read_view(view, |state| state.disk.get(&path).cloned())
            .flatten();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                main_core::packages::electron_utils::save_conflict::is_modified_since(
                    recorded.as_ref(),
                    &path,
                )
            },
            move |conflict| {
                if let Err(error) = request.check_document(&docs, view) {
                    return then(Err(error));
                }
                if !conflict {
                    return then(Ok(false));
                }
                if auto {
                    return then(Ok(true));
                }
                let labels =
                    main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(
                        &docs.lang(),
                    );
                let options = MessageBoxOptions {
                    kind: "warning",
                    title: None,
                    message: labels.message,
                    detail: Some(labels.detail),
                    buttons: vec![labels.overwrite, labels.cancel],
                    default_id: 0,
                    cancel_id: Some(1),
                    no_link: Some(true),
                };
                let checked = request.clone();
                let completed = Rc::clone(&docs);
                let done = kit::back_on_core(
                    &docs.services,
                    move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                        if let Err(error) = checked.check_document(&completed, view) {
                            return then(Err(error));
                        }
                        then(answer.map(|answer| answer.response != 0))
                    },
                );
                match request.authority() {
                    SaveAuthority::Renderer(origin) => docs.dialogs.message_owned(
                        docs.window_of(view),
                        message_box(&options),
                        origin.clone(),
                        request.destination().clone(),
                        request.cancel(),
                        request.pending(),
                        done,
                    ),
                    SaveAuthority::Native(binding) => docs.dialogs.message_existing_document(
                        docs.window_of(view),
                        message_box(&options),
                        binding.clone(),
                        request.captured_participants(),
                        request.cancel(),
                        request.pending(),
                        request.callback_cancels(),
                        done,
                    ),
                }
            },
        );
    }
}

impl TextDocs {
    pub(crate) fn message_owned(
        self: &Rc<Self>,
        view: ViewId,
        options: shell_contract::hosts::MessageBox,
        request: OwnedSave,
        then: impl FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + 'static,
    ) {
        if let Err(error) = request.check_document(self, view) {
            return then(Err(error));
        }
        let checked = request.clone();
        let docs = Rc::clone(self);
        let done = kit::back_on_core(&self.services, move |answer| {
            if let Err(error) = checked.check_document(&docs, view) {
                return then(Err(error));
            }
            then(answer)
        });
        match request.authority() {
            SaveAuthority::Renderer(origin) => self.dialogs.message_owned(
                self.window_of(view),
                options,
                origin.clone(),
                request.destination().clone(),
                request.cancel(),
                request.pending(),
                done,
            ),
            SaveAuthority::Native(binding) => self.dialogs.message_existing_document(
                self.window_of(view),
                options,
                binding.clone(),
                request.captured_participants(),
                request.cancel(),
                request.pending(),
                request.callback_cancels(),
                done,
            ),
        }
    }

    /// Stages the export beside its target on a worker and publishes it on this
    /// core turn under the save's authority; nothing public is truncated.
    pub(crate) fn publish_export_owned(
        self: &Rc<Self>,
        view: ViewId,
        target: String,
        bytes: Vec<u8>,
        request: OwnedSave,
        then: impl FnOnce(Result<crate::staged::Published, String>) + 'static,
    ) {
        if let Err(error) = request.check_document(self, view) {
            return then(Err(error));
        }
        if let Err(error) = request.check_target(Path::new(&target)) {
            return then(Err(error));
        }
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                crate::staged::StagedOutput::prepare(
                    Path::new(&target),
                    bytes,
                    file_commit::PublicationMode::Replace,
                )
                .map(|staged| (target, staged))
            },
            move |staged| {
                then(
                    staged
                        .and_then(|(target, staged)| request.publish_staged(&target, staged))
                        .and_then(|published| {
                            request.check_document(&docs, view).map(|()| published)
                        }),
                )
            },
        );
    }
}

impl TextDocs {
    pub(crate) fn prepare_default_directory_owned(
        self: &Rc<Self>,
        request: OwnedSave,
        then: impl FnOnce(Result<(), String>) + 'static,
    ) {
        let state = self.save_dir.clone();
        let paths = self.services.paths.clone();
        let now = self.now_ms();
        let admission = request.publication_admission(&self.services);
        let docs = Rc::clone(self);
        let published = state.clone();
        kit::on_worker(
            &self.services,
            move || {
                let before = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let mut resolved = before.clone();
                main_core::packages::electron_utils::default_save_dir::configured_default_save_dir_owned(
                &AppPathProvider(paths.as_ref()),&mut resolved,now,admission,
            ).map(|path|(path,before,resolved)).map_err(|error|error.0)
            },
            move |result| {
                then(result.and_then(|(path, before, resolved)| {
                    request.effect(&docs, || {
                        if let Ok(mut state) = published.try_lock() {
                            state.publish_resolution(&before, resolved);
                        }
                        let _ = request.0.default_dir.set(path);
                        Ok(())
                    })
                }))
            },
        );
    }
}

impl TextDocs {
    pub(crate) fn handle_owned_save_ack(
        &self,
        cx: &shell_contract::ipc::CallContext,
        success: bool,
    ) -> bool {
        let view = cx.sender.view;
        let Some(request) = self.owned_saves.borrow().get(&view).cloned() else {
            return false;
        };
        if !success
            && request.matches(cx.native.as_ref())
            && !request.0.claimed.load(std::sync::atomic::Ordering::SeqCst)
        {
            let Some(native) = cx.native.clone() else {
                return true;
            };
            let Ok(actual) = self.capture_document_owner(
                view,
                NativeRequestOwner {
                    view,
                    context: native,
                },
            ) else {
                return true;
            };
            let mut participants = request.participants().to_vec();
            participants.push(actual.clone());
            let mut accepted = false;
            let mut effect = || {
                accepted = true;
                Ok(())
            };
            let admitted = match request.authority() {
                SaveAuthority::Renderer(origin) => {
                    participants.push(origin.clone());
                    request.effects().with_same_native_origin_effect(
                        request.destination(),
                        &actual,
                        &participants,
                        &request.cancel(),
                        &request.pending(),
                        &[&cx.cancel],
                        &mut effect,
                    )
                }
                SaveAuthority::Native(binding) => {
                    request.effects().with_existing_document_save_effect_extra(
                        binding,
                        &participants,
                        &request.cancel(),
                        &request.pending(),
                        &[&cx.cancel],
                        &mut effect,
                    )
                }
            };
            if admitted.is_ok() && accepted {
                let waiter = self.save_waiters.borrow_mut().remove(&view);
                if let Some(waiter) = waiter {
                    waiter(false)
                }
            }
        }
        true
    }
}

impl TextDocs {
    pub(crate) fn finish_saved_owned(
        &self,
        view: ViewId,
        saved: shell_contract::hosts::SavedFile<PathBuf>,
        request: &OwnedSave,
        then: impl FnOnce(Result<PathBuf, String>) + 'static,
    ) {
        let path = saved.path.to_string_lossy();
        let same_document = self
            .read_view(view, |state| {
                state.tracked
                    && request
                        .destination()
                        .context
                        .document_lease()
                        .is_some_and(|lease| lease.same_generation(&state.lease))
                    && state.save.as_deref() == Some(path.as_ref())
                    && state.open.as_deref() == Some(path.as_ref())
            })
            .unwrap_or(false);
        let same_recovery = matches!(self.recovery.identity_of(view),Some(RecoveryIdentity::File(identity)) if identity.file_path==path.as_ref());
        if request.cancel().is_cancelled() || !same_document || !same_recovery {
            return then(Err(
                "the saved document did not acknowledge its final path".into()
            ));
        }
        let shell_contract::hosts::SavedFile { path, session } = saved;
        session.finalize(Box::new(move |published| then(published.map(|()| path))));
    }
}
