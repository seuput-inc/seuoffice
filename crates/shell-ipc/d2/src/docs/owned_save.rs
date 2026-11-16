//! Owned save requests live in the existing close-save waiter table.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use jscompat::{JsObject, JsonValue};
use main_core::node_fs::NodeError;
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::{MessageBox, OwnedEffectsHost};
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use shell_contract::{IpcValue, Surface, ViewId};

use super::Docs;
use crate::kit::{self, Timer};

#[derive(Default)]
struct ReloadState {
    marker: Mutex<Option<shell_contract::hosts::AcceptedPdfReload>>,
    receipt: Mutex<Option<Arc<shell_contract::hosts::CommittedGeneratedPath>>>,
    cancellation: Mutex<Option<shell_contract::ipc::AcceptedOwnerLosses>>,
    pendings: Mutex<Vec<CancelToken>>,
    subscriptions: Mutex<Vec<CancelSubscription>>,
}

impl ReloadState {
    fn register(&self, pending: CancelToken) {
        self.pendings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(pending);
    }

    fn cancel_all(&self) {
        let pendings = self
            .pendings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for pending in pendings {
            pending.cancel();
        }
    }
}

#[derive(Clone)]
enum SaveAuthority {
    Renderer(NativeRequestOwner),
    Native(shell_contract::ipc::NativeExistingDocumentBinding),
}

struct Authority {
    effects: OwnedEffectsHost,
    authority: SaveAuthority,
    destination: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    token: String,
    target: Option<String>,
    picked: Mutex<Option<String>>,
    selected: Mutex<Option<shell_contract::ipc::NativeSaveDestination>>,
    landed: Mutex<Option<String>>,
    reload: Arc<ReloadState>,
    defer_stop_to_parent: bool,
    parent: Option<shell_contract::WindowId>,
}

#[derive(Clone)]
pub(super) struct Scope(Arc<Authority>, Option<NativeRequestOwner>, Vec<CancelToken>);

impl Scope {
    pub(super) fn token(&self) -> &str {
        &self.0.token
    }
    pub(super) fn origin(&self) -> Option<&NativeRequestOwner> {
        match &self.0.authority {
            SaveAuthority::Renderer(origin) => Some(origin),
            SaveAuthority::Native(_) => None,
        }
    }
    pub(super) fn native_binding(
        &self,
    ) -> Option<&shell_contract::ipc::NativeExistingDocumentBinding> {
        match &self.0.authority {
            SaveAuthority::Native(binding) => Some(binding),
            SaveAuthority::Renderer(_) => None,
        }
    }
    pub(super) fn additional_cancellation(&self) -> &[CancelToken] {
        &self.2
    }
    pub(super) fn all_participants(&self) -> Vec<NativeRequestOwner> {
        self.0
            .participants
            .iter()
            .cloned()
            .chain(self.1.iter().cloned())
            .collect()
    }
    /// The owners besides the destination that a staged publication admits.
    pub(super) fn publication_participants(&self) -> Vec<NativeRequestOwner> {
        self.additional_participants()
            .iter()
            .cloned()
            .chain(self.1.iter().cloned())
            .collect()
    }
    pub(super) fn destination(&self) -> &NativeRequestOwner {
        &self.0.destination
    }
    pub(super) fn participants(&self) -> &[NativeRequestOwner] {
        &self.0.participants
    }
    pub(super) fn additional_participants(&self) -> &[NativeRequestOwner] {
        &self.0.participants[..self.0.participants.len() - 1]
    }
    pub(super) fn cancel(&self) -> &CancelToken {
        &self.0.cancel
    }
    pub(super) fn pending(&self) -> &CancelToken {
        &self.0.pending
    }
    pub(super) fn effects(&self) -> &OwnedEffectsHost {
        &self.0.effects
    }
    pub(super) fn target(&self) -> Option<&str> {
        self.0.target.as_deref()
    }
    pub(super) fn parent(&self) -> Option<shell_contract::WindowId> {
        self.0.parent
    }
    pub(super) fn print_to_pdf(
        &self,
        contents: &dyn shell_contract::hosts::Contents,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let participants = self.all_participants();
        match &self.0.authority {
            SaveAuthority::Renderer(origin) => contents.print_to_pdf_owned_many_extra(
                origin,
                self.destination(),
                &participants,
                self.cancel(),
                self.pending(),
                &self.2,
                options,
            ),
            SaveAuthority::Native(binding) => contents.print_to_pdf_existing_document(
                binding,
                &participants,
                self.cancel(),
                self.pending(),
                &self.2,
                options,
            ),
        }
    }
    pub(super) fn stop(&self) {
        self.0.reload.cancel_all();
    }
    pub(super) fn stop_after_save(&self) {
        if !self.0.defer_stop_to_parent || !self.has_generated_receipt() {
            self.0.pending.cancel();
        }
    }

    pub(super) fn reload_accepted(&self) -> shell_contract::hosts::PdfReloadAccepted {
        let reload = self.0.reload.clone();
        let origin = self.origin().cloned();
        let cancel = self.0.cancel.clone();
        let pending = self.0.pending.clone();
        Box::new(move |marker| {
            let losses = match origin.as_ref().filter(|origin| marker.retires(origin)) {
                Some(origin) => match origin.accepted_reload_losses(&marker, &cancel, &pending) {
                    Ok(losses) => Some(losses),
                    Err(_) => {
                        reload.cancel_all();
                        return;
                    }
                },
                None => None,
            };
            let mut subscriptions = reload
                .subscriptions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let tokens: Vec<_> = match losses.as_ref() {
                Some(losses) => losses.tokens().cloned().collect(),
                None => vec![
                    marker.browser_loss().clone(),
                    marker.reservation_loss().clone(),
                ],
            };
            for loss in tokens {
                let weak = Arc::downgrade(&reload);
                subscriptions.push(loss.on_cancel(move || {
                    if let Some(reload) = weak.upgrade() {
                        reload.cancel_all();
                    }
                }));
            }
            if let Some(losses) = losses {
                *reload
                    .cancellation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(losses);
                *reload
                    .marker
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(marker);
            }
        })
    }

    pub(super) fn retires_expected_origin(&self, owner: &NativeRequestOwner) -> bool {
        self.0
            .reload
            .marker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|marker| {
                self.origin().is_some_and(|origin| marker.retires(origin)) && marker.retires(owner)
            })
    }

    pub(super) fn expected_dispatcher_cancel(&self) -> bool {
        self.0
            .reload
            .cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|losses| {
                !losses.cancel().same_token(&self.0.cancel) && !losses.cancel().is_cancelled()
            })
    }

    pub(super) fn retain_generated_receipt(
        &self,
        receipt: Arc<shell_contract::hosts::CommittedGeneratedPath>,
    ) -> Result<(), String> {
        let mut current = self
            .0
            .reload
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.is_some() {
            return Err("The generated document was already completed".into());
        }
        *current = Some(receipt);
        Ok(())
    }

    pub(super) fn with_generated_terminal_commit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.0
            .reload
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .ok_or_else(|| "The generated document has no completed receipt".to_owned())?
            .with_terminal_commit(effect)
    }

    pub(super) fn has_generated_receipt(&self) -> bool {
        self.0
            .reload
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    pub(super) fn effect<T>(&self, effect: impl FnOnce() -> T) -> Result<T, String> {
        let mut effect = Some(effect);
        let mut output = None;
        let participants = self.all_participants();
        let additional = self.2.iter().collect::<Vec<_>>();
        let mut admitted = || {
            let effect = effect
                .take()
                .ok_or("save effect admission was invoked more than once")?;
            output = Some(effect());
            Ok(())
        };
        match &self.0.authority {
            SaveAuthority::Renderer(origin) => self.0.effects.with_owned_effect_many_extra(
                origin,
                &participants,
                &self.0.cancel,
                &self.0.pending,
                &additional,
                &mut admitted,
            ),
            SaveAuthority::Native(binding) => {
                self.0.effects.with_existing_document_save_effect_extra(
                    binding,
                    &participants,
                    &self.0.cancel,
                    &self.0.pending,
                    &additional,
                    &mut admitted,
                )
            }
        }?;
        output.ok_or_else(|| "save effect was not admitted".into())
    }

    pub(super) fn capture_current_destination(
        &self,
        docs: &Docs,
        path: &str,
    ) -> Result<(), String> {
        let Some(binding) = self.native_binding() else {
            return Ok(());
        };
        self.effect(|| {
            let view = self.destination().view;
            if docs.open_doc.get(view).as_deref() != Some(path) || !docs.can_doc_write(view, path) {
                return Err("the current Word document path was replaced".to_owned());
            }
            let selected = shell_contract::ipc::NativeSaveDestination::from_current_document(
                binding.clone(),
                PathBuf::from(path),
            )?;
            *self
                .0
                .selected
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(selected);
            Ok(())
        })?
    }

    /// A native save whose first file the project writer placed at `path`
    /// selects exactly that file, while the document still has none.
    pub(super) fn capture_project_destination(
        &self,
        docs: &Docs,
        path: &str,
    ) -> Result<(), String> {
        let Some(binding) = self.native_binding() else {
            return Ok(());
        };
        self.effect(|| {
            if docs.open_doc.contains(self.destination().view) {
                return Err("the Word document already has a saved file".to_owned());
            }
            let selected = shell_contract::ipc::NativeSaveDestination::from_project_document(
                binding.clone(),
                PathBuf::from(path),
            )?;
            *self
                .0
                .selected
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(selected);
            Ok(())
        })?
    }

    pub(super) fn selected_destination(
        &self,
        path: &str,
    ) -> Result<shell_contract::ipc::NativeSaveDestination, String> {
        self.0
            .selected
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|destination| destination.path() == std::path::Path::new(path))
            .cloned()
            .ok_or_else(|| "the native save destination does not match the selected path".into())
    }

    /// Publishes a staged output on this core turn under the save's authority.
    /// A native save publishes against the destination it selected for `path`;
    /// a renderer save has no validating variant, so the authority is checked
    /// immediately before the admitted publication with nothing able to run
    /// in between.
    pub(super) fn publish_staged(
        &self,
        path: &str,
        mut staged: crate::staged::StagedOutput,
    ) -> Result<crate::staged::Published, String> {
        self.check()?;
        let commit = staged.take()?;
        let result = match &self.0.authority {
            SaveAuthority::Renderer(origin) => self.0.effects.publish_owned_staged_write(
                origin,
                &self.0.destination,
                &self.publication_participants(),
                &self.0.cancel,
                &self.0.pending,
                &self.2,
                std::path::Path::new(path),
                commit,
            ),
            SaveAuthority::Native(_) => {
                let selection = match self.selected_destination(path) {
                    Ok(selection) => selection,
                    Err(error) => {
                        staged.restore(commit);
                        return Err(error);
                    }
                };
                self.0.effects.publish_native_staged_write_validated(
                    &selection,
                    &self.all_participants(),
                    &self.0.cancel,
                    &self.0.pending,
                    &self.2,
                    commit,
                    &mut || Ok(()),
                )
            }
        };
        staged.conclude(result)
    }

    pub(super) fn effect_path<T>(
        &self,
        path: &str,
        effect: impl FnOnce() -> T,
    ) -> Result<T, String> {
        if self.native_binding().is_none() {
            return self.effect(effect);
        }
        let destination = self.selected_destination(path)?;
        let participants = self.all_participants();
        let additional = self.2.iter().collect::<Vec<_>>();
        let mut effect = Some(effect);
        let mut output = None;
        self.0.effects.with_existing_document_destination_effect(
            &destination,
            std::path::Path::new(path),
            &participants,
            self.cancel(),
            self.pending(),
            &additional,
            &mut || {
                output = Some(effect
                    .take()
                    .ok_or("save file admission was invoked more than once")?(
                ));
                Ok(())
            },
        )?;
        output.ok_or_else(|| "save file effect was not admitted".into())
    }

    pub(super) fn check(&self) -> Result<(), String> {
        self.effect(|| ())
    }

    pub(super) fn admit_file_effect(&self, effect: &mut dyn FnMut()) -> Result<(), NodeError> {
        self.effect(effect).map_err(|message| NodeError {
            code: None,
            message,
        })
    }

    pub(super) fn ensure_directory(&self, directory: &std::path::Path) -> Result<(), String> {
        self.check()?;
        let missing = directory
            .ancestors()
            .take_while(|path| !path.is_dir())
            .collect::<Vec<_>>();
        for path in missing.into_iter().rev() {
            match self.effect(|| std::fs::create_dir(path))? {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(error) => {
                    return Err(super::node_text(&super::fs_error(
                        &error,
                        "mkdir",
                        &directory.to_string_lossy(),
                    )));
                }
            }
        }
        Ok(())
    }

    fn authenticates(&self, cx: &CallContext) -> bool {
        cx.sender.view == self.0.destination.view
            && cx
                .native
                .as_ref()
                .and_then(|native| native.renderer())
                .zip(self.0.destination.context.renderer())
                .is_some_and(|(actual, expected)| actual.same_generation(expected))
    }

    fn capture_callback(&self, cx: &CallContext) -> Result<Self, String> {
        if !self.authenticates(cx) {
            return Err("owned save request belongs to another renderer".into());
        }
        let actual = NativeRequestOwner {
            view: cx.sender.view,
            context: cx
                .native
                .clone()
                .ok_or("the save callback has no native authority")?,
        };
        let mut additional = self.2.clone();
        additional.push(cx.cancel.clone());
        let controls = additional.iter().collect::<Vec<_>>();
        let mut effect = || Ok(());
        match &self.0.authority {
            SaveAuthority::Renderer(_) => self.0.effects.with_same_native_origin_effect(
                self.destination(),
                &actual,
                self.participants(),
                self.cancel(),
                self.pending(),
                &controls,
                &mut effect,
            ),
            SaveAuthority::Native(binding) => {
                self.0.effects.with_existing_document_same_origin_effect(
                    binding,
                    &actual,
                    self.participants(),
                    self.cancel(),
                    self.pending(),
                    &controls,
                    &mut effect,
                )
            }
        }?;
        Ok(Self(self.0.clone(), Some(actual), additional))
    }

    pub(super) fn record_pick(&self, path: String) -> Result<(), String> {
        let mut picked = self
            .0
            .picked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.effect(|| *picked = Some(path))
    }

    pub(super) fn note_landed(&self, path: &str) -> Result<(), String> {
        let mut landed = self
            .0
            .landed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.effect(|| *landed = Some(path.into()))
    }

    fn saved_target(&self) -> bool {
        self.target().is_none_or(|target| {
            self.0
                .landed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_deref()
                == Some(target)
        })
    }

    pub(super) fn take_pick(&self, path: &str) -> Result<bool, String> {
        let mut picked = self
            .0
            .picked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.effect(|| {
            if picked.as_deref() == Some(path) {
                picked.take();
                true
            } else {
                false
            }
        })
    }
}

pub(super) struct SaveWait {
    pub(super) view: ViewId,
    pub(super) timer: Timer,
    pub(super) then: Then<bool>,
    pub(super) scope: Option<Scope>,
    pub(super) loss: Option<(Vec<CancelSubscription>, Option<CancelSubscription>)>,
}

fn target_path(target: Option<PathBuf>) -> Result<Option<String>, shell_contract::IpcError> {
    match target {
        Some(path) if path.is_absolute() => path
            .into_os_string()
            .into_string()
            .map(Some)
            .map_err(|_| kit::error("save_document requires a Unicode path")),
        Some(_) => Err(kit::error("save_document requires an absolute path")),
        None => Ok(None),
    }
}

fn capture(
    docs: &Rc<Docs>,
    view: ViewId,
    parent: Option<shell_contract::WindowId>,
    origin: NativeRequestOwner,
    cancel: CancelToken,
    target: Option<String>,
) -> Result<Scope, shell_contract::IpcError> {
    if docs.is_torn_down(view)
        || !docs
            .services
            .views
            .sender_of(view)
            .is_some_and(|sender| sender.surface == Surface::Docs)
    {
        return Err(kit::error("the target document is no longer open"));
    }
    let effects = docs
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| kit::error("native owned save effects are unavailable"))?;
    let expected = docs
        .services
        .views
        .renderer_generation(view)
        .ok_or_else(|| kit::error("the target document renderer is unavailable"))?;
    let destination = docs.checked_document_owner(
        view,
        effects.capture_owner(view, &expected).map_err(kit::error)?,
    )?;
    let scope = Scope(
        Arc::new(Authority {
            effects,
            authority: SaveAuthority::Renderer(origin),
            participants: vec![destination.clone()],
            destination,
            cancel,
            pending: CancelToken::default(),
            token: format!("save:{}:{}", docs.shell.random_uuid(), kit::unique()),
            target,
            picked: Mutex::new(None),
            selected: Mutex::new(None),
            landed: Mutex::new(None),
            parent,
            reload: Arc::new(ReloadState::default()),
            defer_stop_to_parent: false,
        }),
        None,
        Vec::new(),
    );
    scope.0.reload.register(scope.pending().clone());
    scope.check().map_err(kit::error)?;
    Ok(scope)
}

pub(super) fn capture_close(
    docs: &Rc<Docs>,
    view: ViewId,
    parent: Option<shell_contract::WindowId>,
    origin: NativeRequestOwner,
    cancel: CancelToken,
) -> Result<Scope, shell_contract::IpcError> {
    capture(docs, view, parent, origin, cancel, None)
}

pub(super) fn capture_close_many(
    docs: &Rc<Docs>,
    view: ViewId,
    parent: Option<shell_contract::WindowId>,
    origin: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    destination: NativeRequestOwner,
    cancel: CancelToken,
) -> Result<Scope, shell_contract::IpcError> {
    let destination = docs.checked_document_owner(view, destination)?;
    let effects = docs
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| kit::error("native owned save effects are unavailable"))?;
    participants.push(destination.clone());
    let scope = Scope(
        Arc::new(Authority {
            effects,
            authority: SaveAuthority::Renderer(origin),
            destination,
            participants,
            cancel,
            pending: CancelToken::default(),
            token: format!("save:{}:{}", docs.shell.random_uuid(), kit::unique()),
            target: None,
            picked: Mutex::new(None),
            selected: Mutex::new(None),
            landed: Mutex::new(None),
            parent,
            reload: Arc::new(ReloadState::default()),
            defer_stop_to_parent: false,
        }),
        None,
        Vec::new(),
    );
    scope.0.reload.register(scope.pending().clone());
    scope.check().map_err(kit::error)?;
    Ok(scope)
}

pub(super) fn start(
    docs: &Rc<Docs>,
    view: ViewId,
    mode: SaveMode,
    target: Option<PathBuf>,
    origin: NativeRequestOwner,
    cancel: CancelToken,
    then: Then<bool>,
) {
    let scope = target_path(target)
        .and_then(|target| capture(docs, view, docs.parent(view), origin, cancel, target));
    match scope {
        Ok(scope) => enqueue(docs, scope, mode, None, then),
        Err(error) => then(Err(error)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_native(
    docs: &Rc<Docs>,
    view: ViewId,
    mode: SaveMode,
    target: Option<PathBuf>,
    binding: shell_contract::ipc::NativeExistingDocumentBinding,
    cancel: CancelToken,
    pending: CancelToken,
    then: Then<bool>,
) {
    use shell_contract::ipc::{
        NativeEditorCommand, NativeInvocationSelection, NativeSaveSelection,
    };
    let selected = match (binding.operation().selection(), &target) {
        (
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                surface: Surface::Docs,
                view: selected,
                selection: NativeSaveSelection::Mode(selected_mode),
            }),
            None,
        ) => *selected == view && *selected_mode == mode,
        (
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                surface: Surface::Docs,
                view: selected,
                selection: NativeSaveSelection::ExactPath(selected_path),
            }),
            Some(path),
        ) => *selected == view && selected_path == path && mode == SaveMode::Save,
        _ => false,
    };
    if !selected {
        return then(Err(kit::error(
            "the native save does not match its selection",
        )));
    }
    let scope = (|| {
        let selected = if target.is_some() {
            Some(
                shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding.clone())
                    .map_err(kit::error)?,
            )
        } else {
            None
        };
        let target = target_path(target)?;
        let destination = docs.checked_document_owner(view, binding.owner().clone())?;
        let effects = docs
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| kit::error("native owned save effects are unavailable"))?;
        let scope = Scope(
            Arc::new(Authority {
                effects,
                authority: SaveAuthority::Native(binding),
                participants: vec![destination.clone()],
                destination,
                cancel,
                pending,
                token: format!("save:{}:{}", docs.shell.random_uuid(), kit::unique()),
                target,
                picked: Mutex::new(None),
                selected: Mutex::new(selected),
                landed: Mutex::new(None),
                parent: docs.parent(view),
                reload: Arc::new(ReloadState::default()),
                defer_stop_to_parent: false,
            }),
            None,
            Vec::new(),
        );
        scope.0.reload.register(scope.pending().clone());
        scope.check().map_err(kit::error)?;
        Ok::<_, shell_contract::IpcError>(scope)
    })();
    match scope {
        Ok(scope) => enqueue(docs, scope, mode, None, then),
        Err(error) => then(Err(error)),
    }
}

pub(super) fn start_with_captured_scope(
    docs: &Rc<Docs>,
    parent: Scope,
    mode: SaveMode,
    then: Then<bool>,
) {
    if let Err(error) = parent.check() {
        return then(Err(kit::error(error)));
    }
    if let Err(error) =
        docs.checked_document_owner(parent.destination().view, parent.destination().clone())
    {
        return then(Err(error));
    }
    let scope = Scope(
        Arc::new(Authority {
            effects: parent.effects().clone(),
            authority: parent.0.authority.clone(),
            destination: parent.destination().clone(),
            participants: parent.participants().to_vec(),
            cancel: parent.cancel().clone(),
            pending: CancelToken::default(),
            token: format!("save:{}:{}", docs.shell.random_uuid(), kit::unique()),
            target: None,
            picked: Mutex::new(None),
            selected: Mutex::new(None),
            landed: Mutex::new(None),
            parent: parent.parent(),
            reload: parent.0.reload.clone(),
            defer_stop_to_parent: true,
        }),
        parent.1.clone(),
        parent.2.clone(),
    );
    scope.0.reload.register(scope.pending().clone());
    enqueue(docs, scope, mode, Some(parent.pending().clone()), then);
}

fn enqueue(
    docs: &Rc<Docs>,
    scope: Scope,
    mode: SaveMode,
    parent_pending: Option<CancelToken>,
    then: Then<bool>,
) {
    let view = scope.destination().view;
    let effects = scope.effects().clone();
    let cancel = scope.cancel().clone();
    let mut losses = vec![(cancel.clone(), None), (scope.pending().clone(), None)];
    if let Some(binding) = scope.native_binding() {
        losses.extend([
            (binding.operation().loss_token(), None),
            (binding.operation().cancel().clone(), None),
            (binding.operation().shutdown().clone(), None),
        ]);
    }
    losses.extend(
        scope
            .additional_cancellation()
            .iter()
            .cloned()
            .map(|token| (token, None)),
    );
    for owner in scope
        .origin()
        .into_iter()
        .chain(scope.all_participants().iter())
    {
        let Some(loss) = owner.loss_token() else {
            return then(Err(kit::error("the save owner has no native lease")));
        };
        let Some(renderer) = owner.context.renderer() else {
            return then(Err(kit::error("the save owner has no renderer lifetime")));
        };
        losses.push((loss, Some(owner.clone())));
        losses.push((renderer.loss_token(), Some(owner.clone())));
    }
    let id = kit::unique();
    let parent_subscription = parent_pending.map(|parent| {
        let stop = scope.pending().clone();
        let weak = Rc::downgrade(docs);
        let callback = kit::back_on_core(&docs.services, move |_: ()| {
            if let Some(docs) = weak.upgrade() {
                docs.close.finish_save(
                    id,
                    Err(kit::error("the parent save operation was cancelled")),
                );
            }
        });
        parent.on_cancel(move || {
            stop.cancel();
            callback(());
        })
    });
    if let Err(error) = scope.check() {
        return then(Err(kit::error(error)));
    }
    let weak = Rc::downgrade(docs);
    let timer = kit::after(
        &docs.services,
        std::time::Duration::from_millis(120_000),
        move || {
            if let Some(docs) = weak.upgrade() {
                docs.close.finish_save(
                    id,
                    Err(kit::error("the document save timed out after 120000ms")),
                );
            }
        },
    );
    docs.close.saves.borrow_mut().insert(
        id,
        SaveWait {
            view,
            timer,
            then,
            scope: Some(scope.clone()),
            loss: None,
        },
    );
    let subscriptions = losses
        .into_iter()
        .map(|(loss, owner)| {
            let weak = Rc::downgrade(docs);
            let callback = kit::back_on_core(&docs.services, move |_: ()| {
                if let Some(docs) = weak.upgrade() {
                    docs.close
                        .finish_save(id, Err(kit::error("the document save owner was cancelled")));
                }
            });
            let observed_scope = scope.clone();
            let pending = scope.pending().clone();
            let is_pending = loss.same_token(scope.pending());
            let is_request_cancel = loss.same_token(scope.cancel());
            loss.on_cancel(move || {
                if owner
                    .as_ref()
                    .is_some_and(|owner| observed_scope.retires_expected_origin(owner))
                {
                    return;
                }
                if is_request_cancel && observed_scope.expected_dispatcher_cancel() {
                    return;
                }
                if !is_pending {
                    pending.cancel();
                }
                callback(());
            })
        })
        .collect::<Vec<_>>();
    if let Some(waiter) = docs.close.saves.borrow_mut().get_mut(&id) {
        waiter.loss = Some((subscriptions, parent_subscription));
    }
    let mut message = vec![
        ("requestId".into(), IpcValue::String(scope.token().into())),
        (
            "mode".into(),
            IpcValue::String(
                match mode {
                    SaveMode::Save => "save",
                    SaveMode::SaveAs => "save-as",
                }
                .into(),
            ),
        ),
    ];
    if let Some(path) = scope.target() {
        message.push(("targetPath".into(), IpcValue::String(path.into())));
        message.push(("overwrite".into(), IpcValue::Bool(true)));
    }
    let target = scope.participants().len() - 1;
    let participants = scope.all_participants();
    let additional = scope.additional_cancellation().iter().collect::<Vec<_>>();
    let result = match &scope.0.authority {
        SaveAuthority::Renderer(origin) => effects.emit_owned_many_extra(
            origin,
            &participants,
            target,
            &cancel,
            scope.pending(),
            &additional,
            "docs:close-save-request",
            vec![IpcValue::Object(message)],
        ),
        SaveAuthority::Native(binding) => effects.emit_existing_document_save_many_extra(
            binding,
            &participants,
            target,
            &cancel,
            scope.pending(),
            &additional,
            "docs:close-save-request",
            vec![IpcValue::Object(message)],
        ),
    };
    if let Err(error) = result {
        docs.close.finish_save(id, Err(kit::error(error)));
    }
}

impl Docs {
    pub(super) fn checked_document_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, shell_contract::IpcError> {
        if native.view != view || self.is_torn_down(view) {
            return Err(kit::error(
                "the Word document owner does not match the target",
            ));
        }
        let permit = self
            .media_permit(view)
            .ok_or_else(|| kit::error("the Word document media session is unavailable"))?;
        let effects = self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .ok_or_else(|| kit::error("native owned save effects are unavailable"))?;
        let identity = permit.owner();
        if identity.view != view.0
            || effects.docs_document_identity(&native) != Some((view, identity.generation))
            || !permit.is_active()
            || !native
                .context
                .renderer()
                .zip(self.services.views.renderer_generation(view).as_ref())
                .is_some_and(|(captured, current)| {
                    captured.same_generation(current) && captured.is_active()
                })
        {
            return Err(kit::error("the Word document media owner was replaced"));
        }
        Ok(native)
    }

    pub(super) fn owned_save_scope(
        &self,
        cx: &CallContext,
        token: &IpcValue,
    ) -> Result<Option<Scope>, shell_contract::IpcError> {
        let token = match token {
            IpcValue::Undefined | IpcValue::Null => return Ok(None),
            IpcValue::String(token) => token,
            _ => return Err(kit::error("invalid owned save request")),
        };
        let scope = self
            .close
            .saves
            .borrow()
            .values()
            .filter_map(|waiter| waiter.scope.as_ref())
            .find(|scope| scope.token() == token)
            .cloned()
            .ok_or_else(|| kit::error("owned save request is no longer pending"))?;
        Ok(Some(scope.capture_callback(cx).map_err(kit::error)?))
    }
}

pub(super) fn answer(docs: &Docs, cx: &CallContext, token: &str, ok: bool, error: Option<&str>) {
    let pending = docs.close.saves.borrow().iter().find_map(|(id, waiter)| {
        waiter
            .scope
            .as_ref()
            .filter(|scope| scope.token() == token)
            .and_then(|scope| scope.capture_callback(cx).ok())
            .map(|scope| (*id, scope))
    });
    if let Some((id, scope)) = pending {
        if scope.has_generated_receipt() {
            let mut waiter = None;
            let mut accepted = None;
            let admitted = scope.with_generated_terminal_commit(&mut || {
                let result = if !ok && let Some(error) = error {
                    Err(error.to_owned())
                } else if ok && !scope.saved_target() {
                    Err("the requested document was not saved".into())
                } else {
                    Ok(ok)
                };
                waiter = docs.close.take_save(id);
                if waiter.is_none() {
                    return Err("the document save is no longer pending".into());
                }
                accepted = Some(result);
                Ok(())
            });
            match (admitted, waiter, accepted) {
                (Ok(()), Some(waiter), Some(result)) => {
                    super::close::Close::complete_save(waiter, result.map_err(kit::error))
                }
                (Err(error), _, _) => docs.close.finish_save(id, Err(kit::error(error))),
                _ => docs
                    .close
                    .finish_save(id, Err(kit::error("the document save was not admitted"))),
            }
            return;
        }
        let result = scope.check().and_then(|()| {
            if !ok && let Some(error) = error {
                Err(error.to_owned())
            } else if ok && !scope.saved_target() {
                Err("the requested document was not saved".into())
            } else {
                Ok(ok)
            }
        });
        docs.close.finish_save(id, result.map_err(kit::error));
    }
}

pub(super) fn message(
    docs: &Rc<Docs>,
    scope: Scope,
    options: MessageBox,
    then: Box<dyn FnOnce(Result<usize, String>)>,
) {
    let completed_scope = scope.clone();
    let done = kit::back_on_core(
        &docs.services,
        move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
            then(answer.and_then(|answer| completed_scope.check().map(|()| answer.response)));
        },
    );
    match &scope.0.authority {
        SaveAuthority::Renderer(origin) => docs.dialogs.message_owned_many_extra(
            scope.parent(),
            options,
            origin.clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.2.clone(),
            Box::new(done),
        ),
        SaveAuthority::Native(binding) => docs.dialogs.message_existing_document(
            scope.parent(),
            options,
            binding.clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.2.clone(),
            Box::new(done),
        ),
    }
}

pub(super) fn save_dialog(
    docs: &Rc<Docs>,
    scope: Scope,
    options: JsObject<JsonValue>,
    normalize: Option<(main_core::docs::doc_format::DocFormat, bool)>,
    then: Box<dyn FnOnce(Result<JsonValue, shell_contract::IpcError>)>,
) {
    use main_core::packages::electron_utils::dialog_memory::{
        FallbackDir, prepare_dialog_options, remember_save_path,
    };
    let resolve = || {
        docs.default_save_dir()
            .map(Some)
            .map_err(main_core::packages::electron_utils::default_save_dir::Thrown)
    };
    let prepared = {
        let memory = docs
            .memory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prepare_dialog_options(&memory, &options, Some(&FallbackDir::Resolve(&resolve)))
    };
    let memory = docs.memory.clone();
    let completed_scope = scope.clone();
    enum Picked {
        Renderer(PathBuf),
        Native(shell_contract::ipc::NativeSaveDestination),
    }
    let done = kit::back_on_core(
        &docs.services,
        move |picked: Result<Option<Picked>, String>| {
            let result = picked.and_then(|picked| {
                completed_scope.check()?;
                let mut result = JsObject::new();
                result.set("canceled", JsonValue::Bool(picked.is_none()));
                if let Some(picked) = picked {
                    let (raw, path, native) = match picked {
                        Picked::Renderer(path) => {
                            let path = path.to_string_lossy().into_owned();
                            (path.clone(), path, None)
                        }
                        Picked::Native(destination) => {
                            let raw = destination.path().to_string_lossy().into_owned();
                            let destination = match normalize {
                                Some((main_core::docs::doc_format::DocFormat::Docx, exports)) => {
                                    destination.normalize_dialog_format(
                                        shell_contract::ipc::NativeSaveFormat::DocsDocx {
                                            offer_exports: exports,
                                        },
                                    )?
                                }
                                Some((main_core::docs::doc_format::DocFormat::Odt, exports)) => {
                                    destination.normalize_dialog_format(
                                        shell_contract::ipc::NativeSaveFormat::DocsOdt {
                                            offer_exports: exports,
                                        },
                                    )?
                                }
                                None => destination,
                            };
                            (
                                raw,
                                destination.path().to_string_lossy().into_owned(),
                                Some(destination),
                            )
                        }
                    };
                    let mut memory = memory
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    completed_scope.effect(|| remember_save_path(&mut memory, &raw))?;
                    drop(memory);
                    if let Some(native) = native {
                        let mut selected = completed_scope
                            .0
                            .selected
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        completed_scope.effect(|| *selected = Some(native))?;
                    }
                    result.set("filePath", JsonValue::String(path));
                }
                Ok(JsonValue::Object(result))
            });
            then(result.map_err(kit::error));
        },
    );
    let options = super::adapters::file_dialog(&prepared);
    match &scope.0.authority {
        SaveAuthority::Renderer(origin) => docs.dialogs.save_owned_many_extra(
            scope.parent(),
            options,
            origin.clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.2.clone(),
            Box::new(move |result| done(result.map(|path| path.map(Picked::Renderer)))),
        ),
        SaveAuthority::Native(binding) => docs.dialogs.save_existing_document(
            scope.parent(),
            options,
            binding.clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.2.clone(),
            Box::new(move |result| done(result.map(|path| path.map(Picked::Native)))),
        ),
    }
}

fn cleanup_saved(
    docs: &Rc<Docs>,
    view: ViewId,
    paths: Vec<String>,
    scope: Scope,
    then: Box<dyn FnOnce(Result<(), String>)>,
) {
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryDeleteReason, RecoveryDeleteResult, RecoveryIdentity,
    };
    let untitled = docs.book.borrow().untitled_of(view);
    let shared = docs.recovery_shared.clone();
    let user_data = docs.user_data(&[]);
    let shell = docs.shell.clone();
    let worker_scope = scope.clone();
    let completed = docs.clone();
    kit::on_worker(
        &docs.services,
        move || {
            super::recovery::with_store(&shared, &user_data, &shell, |store| {
                let remove = |identity: &RecoveryIdentity| -> Result<bool, String> {
                    let mut rejected = None;
                    let result = store.delete_copy_authorized(
                        identity,
                        RecoveryDeleteReason::Saved,
                        &mut |effect| {
                            let result = worker_scope.admit_file_effect(effect);
                            if let Err(error) = &result {
                                rejected = Some(error.message.clone());
                            }
                            result
                        },
                    );
                    if let Some(reason) = rejected {
                        return Err(reason);
                    }
                    Ok(matches!(result, Ok(RecoveryDeleteResult::Deleted)))
                };
                for path in paths {
                    remove(&super::recovery::file_identity(&path))?;
                }
                match untitled {
                    Some(identity) if remove(&RecoveryIdentity::Untitled(identity.clone()))? => {
                        Ok(Some(identity))
                    }
                    _ => Ok(None),
                }
            })
        },
        move |result| {
            let result = result.and_then(|untitled| {
                scope.effect(|| {
                    if let Some(identity) = untitled {
                        completed
                            .book
                            .borrow_mut()
                            .finish_saved_untitled(view, &identity);
                    }
                })
            });
            then(result);
        },
    );
}

fn push_recent(
    docs: &Rc<Docs>,
    path: String,
    scope: Scope,
    then: Box<dyn FnOnce(Result<(), String>)>,
) {
    if docs.shell.is_headless() {
        return then(scope.check());
    }
    let refresh = match &scope.0.authority {
        SaveAuthority::Renderer(origin) => docs.shell.prepare_docs_menu_refresh_owned(
            origin.clone(),
            scope.destination().clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.effects().clone(),
        ),
        SaveAuthority::Native(binding) => docs.shell.prepare_docs_menu_refresh_existing_document(
            binding.clone(),
            scope.all_participants(),
            scope.cancel().clone(),
            scope.pending().clone(),
            scope.2.clone(),
            scope.effects().clone(),
        ),
    };
    let recent_path = docs.recent_path();
    let io_path = recent_path.clone();
    let worker_scope = scope.clone();
    let done = kit::back_on_core(
        &docs.services,
        move |result: Result<Result<bool, String>, NodeError>| {
            let changed = match result {
                Ok(Ok(changed)) => changed,
                Ok(Err(error)) => return then(Err(error)),
                Err(_) => true,
            };
            if let Err(error) = scope.check() {
                return then(Err(error));
            }
            if !changed {
                return then(Ok(()));
            }
            match refresh {
                Err(error) => then(Err(error)),
                Ok(Some(refresh)) => refresh(then),
                Ok(None) => then(Ok(())),
            }
        },
    );
    main_core::shell::recent_files::metadata::submit(
        &recent_path,
        move || {
            if let Err(error) = worker_scope.check() {
                return Ok(Err(error));
            }
            let recent = match super::read_json(&io_path) {
                Some(JsonValue::Array(items)) => items,
                _ => Vec::new(),
            };
            let item = super::js_str(&path);
            if recent.first() == Some(&item) {
                return Ok(Ok(false));
            }
            let mut next = Vec::with_capacity((recent.len() + 1).min(100));
            next.push(item.clone());
            next.extend(
                recent
                    .into_iter()
                    .filter(|previous| *previous != item)
                    .take(99),
            );
            let mut rejected = None;
            let result =
                main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                    &io_path,
                    &JsonValue::Array(next),
                    &mut |effect| {
                        let result = worker_scope.admit_file_effect(effect);
                        if let Err(error) = &result {
                            rejected = Some(error.message.clone());
                        }
                        result
                    },
                );
            if let Some(reason) = rejected {
                return Ok(Err(reason));
            }
            let _ = result;
            Ok(Ok(true))
        },
        done,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn after_write(
    docs: &Rc<Docs>,
    view: ViewId,
    path: String,
    stale_paths: Vec<String>,
    scope: Scope,
    notify: bool,
    remember_open: bool,
    then: Box<dyn FnOnce(Result<String, String>)>,
) {
    let completed = docs.clone();
    let cleanup_scope = scope.clone();
    cleanup_saved(
        docs,
        view,
        stale_paths,
        cleanup_scope,
        Box::new(move |result| {
            if let Err(error) = result {
                return then(Err(error));
            }
            let saved_docs = completed.clone();
            let recent_scope = scope.clone();
            push_recent(
                &completed,
                path.clone(),
                recent_scope,
                Box::new(move |result| {
                    if let Err(error) = result {
                        return then(Err(error));
                    }
                    if let Err(error) = scope.effect(|| {
                        if remember_open {
                            saved_docs.allow_doc_write(view, &path);
                            saved_docs.remember_open_doc(view, &path);
                        }
                    }) {
                        return then(Err(error));
                    }
                    if !notify {
                        return then(scope.note_landed(&path).map(|()| path));
                    }
                    let acknowledged_docs = saved_docs.clone();
                    let authority = scope.0.authority.clone();
                    let destination = match &authority {
                        SaveAuthority::Native(_) => match scope.selected_destination(&path) {
                            Ok(destination) => Some(destination),
                            Err(error) => return then(Err(error)),
                        },
                        SaveAuthority::Renderer(_) => None,
                    };
                    let participants = scope.all_participants();
                    let cancel = scope.cancel().clone();
                    let pending = scope.pending().clone();
                    let additional = scope.2.clone();
                    let effects = scope.effects().clone();
                    let target = scope.destination().clone();
                    let selected_path = PathBuf::from(&path);
                    let done: Box<dyn FnOnce(_)> = Box::new(
                        move |result: Result<shell_contract::hosts::SavedFile<PathBuf>, String>| {
                            let saved = match result {
                                Ok(saved) => saved,
                                Err(error) => return then(Err(error)),
                            };
                            let final_path = saved.path.to_string_lossy().into_owned();
                            if let Err(error) = scope.effect(|| {
                        if acknowledged_docs.open_doc.get(view).as_deref() != Some(final_path.as_str()) {
                            return Err("the saved Word document path was not acknowledged".to_owned());
                        }
                        if !acknowledged_docs.can_doc_write(view, &final_path) {
                            return Err("the saved Word document write grant was not acknowledged".to_owned());
                        }
                        Ok(())
                    }).and_then(|result| result) { return then(Err(error)); }
                            saved.session.finalize(Box::new(move |result| {
                                then(
                                    result
                                        .and_then(|()| scope.note_landed(&path))
                                        .map(|()| final_path),
                                );
                            }));
                        },
                    );
                    match authority {
                        SaveAuthority::Renderer(origin) => saved_docs.shell.file_saved_owned(
                            origin,
                            target,
                            participants,
                            cancel,
                            pending,
                            effects,
                            selected_path,
                            done,
                        ),
                        SaveAuthority::Native(_) => match destination {
                            Some(destination) => saved_docs.shell.file_saved_existing_document(
                                destination,
                                participants,
                                cancel,
                                pending,
                                additional,
                                effects,
                                done,
                            ),
                            None => done(Err("the native save destination is unavailable".into())),
                        },
                    }
                }),
            );
        }),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn file_renamed_many(
    docs: &Rc<Docs>,
    view: ViewId,
    old: &std::path::Path,
    new: &std::path::Path,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    then: Then<()>,
) {
    let destination = match docs.checked_document_owner(view, destination) {
        Ok(owner) => owner,
        Err(error) => return then(Err(error)),
    };
    let Some(effects) = docs.services.hosts.get::<OwnedEffectsHost>().cloned() else {
        return then(Err(kit::error("native owned save effects are unavailable")));
    };
    participants.push(destination.clone());
    let old = old.to_string_lossy().into_owned();
    let new = new.to_string_lossy().into_owned();
    let mut moved = None;
    let extra = additional.iter().collect::<Vec<_>>();
    if let Err(error) = effects.with_owned_effect_many_extra(
        &origin,
        &participants,
        &cancel,
        &pending,
        &extra,
        &mut || {
            if docs.open_doc.get(view).as_deref() != Some(old.as_str()) {
                return Err("the Word document path changed before relocation".into());
            }
            moved = Some(docs.rename_document_state(view, &old, &new));
            Ok(())
        },
    ) {
        return then(Err(kit::error(error)));
    }
    let Some(media_result) = moved else {
        return then(Err(kit::error("the Word relocation was not admitted")));
    };
    let events = docs.services.events.clone();
    let target = destination.clone();
    let completed = docs.clone();
    let completed_origin = origin.clone();
    let completed_participants = participants.clone();
    let completed_cancel = cancel.clone();
    let completed_pending = pending.clone();
    let completed_additional = additional.clone();
    let completed_effects = effects.clone();
    let done = kit::back_on_core(&docs.services, move |result: Result<(), String>| {
        if let Err(error) = result {
            return then(Err(kit::error(error)));
        }
        let Err(media_error) = media_result else {
            return then(Ok(()));
        };
        let extra = completed_additional.iter().collect::<Vec<_>>();
        if let Err(error) = completed_effects.with_owned_effect_many_extra(
            &completed_origin,
            &completed_participants,
            &completed_cancel,
            &completed_pending,
            &extra,
            &mut || {
                completed.torn_down.borrow_mut().insert(view);
                completed.doc_writable.remove(view);
                completed.pdf_writable.remove(view);
                Ok(())
            },
        ) {
            return then(Err(kit::error(error)));
        }
        let dialog = completed.dialogs.clone();
        let title = completed.tm("dlgOpenDoc");
        let detail =
            format!("Document saving was disabled because the media service failed: {media_error}");
        let dialog_done =
            kit::back_on_core(&completed.services, move |result: Result<(), String>| {
                then(Err(kit::error(result.err().unwrap_or(media_error))));
            });
        completed_effects.post_ui_owned_many_extra(
            completed_origin,
            completed_participants,
            completed_cancel,
            completed_pending,
            completed_additional,
            Box::new(move || {
                dialog.error(&title, &detail);
                Ok(())
            }),
            Box::new(dialog_done),
        );
    });
    let payload = super::Obj::new()
        .set("oldPath", old)
        .set("newPath", new)
        .done();
    effects.post_ui_owned_many_extra(
        origin,
        participants,
        cancel,
        pending,
        additional,
        Box::new(move || events.emit_to_owner_checked(&target, "docs:renamed", vec![payload])),
        Box::new(done),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn file_renamed_existing_document(
    docs: &Rc<Docs>,
    view: ViewId,
    old: &std::path::Path,
    new: &std::path::Path,
    authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
    expected_path: &std::path::Path,
    destination: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
    then: Then<()>,
) {
    let destination = match docs.checked_document_owner(view, destination.clone()) {
        Ok(owner) => owner,
        Err(error) => return then(Err(error)),
    };
    let snapshot = match authority.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return then(Err(kit::error(error))),
    };
    if snapshot.selection().path() != expected_path {
        return then(Err(kit::error(
            "the native save selection changed before relocation",
        )));
    }
    let Some(effects) = docs.services.hosts.get::<OwnedEffectsHost>().cloned() else {
        return then(Err(kit::error("native owned save effects are unavailable")));
    };
    let mut captured = participants.to_vec();
    captured.push(destination);
    let target = captured.len() - 1;
    let extra = additional.iter().collect::<Vec<_>>();
    let old = old.to_string_lossy().into_owned();
    let new = new.to_string_lossy().into_owned();
    let mut moved = None;
    if let Err(error) = effects.with_native_save_revision_snapshot_effect(
        &snapshot,
        &captured,
        cancel,
        pending,
        &extra,
        &mut || {
            if snapshot.selection().path() != expected_path {
                return Err("the native save selection changed before relocation".into());
            }
            if docs.open_doc.get(view).as_deref() != Some(old.as_str()) {
                return Err("the Word document path changed before relocation".into());
            }
            moved = Some(docs.rename_document_state(view, &old, &new));
            Ok(())
        },
    ) {
        return then(Err(kit::error(error)));
    }
    let Some(media_result) = moved else {
        return then(Err(kit::error("the Word relocation was not admitted")));
    };
    let payload = super::Obj::new()
        .set("oldPath", old)
        .set("newPath", new)
        .done();
    if let Err(error) = effects.emit_native_save_revision_snapshot(
        &snapshot,
        &captured,
        target,
        cancel,
        pending,
        &extra,
        "docs:renamed",
        vec![payload],
    ) {
        return then(Err(kit::error(error)));
    }
    let Err(media_error) = media_result else {
        return then(Ok(()));
    };
    if let Err(error) = effects.with_native_save_revision_snapshot_effect(
        &snapshot,
        &captured,
        cancel,
        pending,
        &extra,
        &mut || {
            if snapshot.selection().path() != expected_path {
                return Err("the native save selection changed before relocation".into());
            }
            docs.torn_down.borrow_mut().insert(view);
            docs.doc_writable.remove(view);
            docs.pdf_writable.remove(view);
            Ok(())
        },
    ) {
        return then(Err(kit::error(error)));
    }
    let dialog = docs.dialogs.clone();
    let title = docs.tm("dlgOpenDoc");
    let detail =
        format!("Document saving was disabled because the media service failed: {media_error}");
    let done = kit::back_on_core(&docs.services, move |result: Result<(), String>| {
        then(Err(kit::error(result.err().unwrap_or(media_error))));
    });
    effects.post_ui_native_save_revision_snapshot(
        snapshot,
        captured,
        cancel.clone(),
        pending.clone(),
        additional.to_vec(),
        Box::new(move || {
            dialog.error(&title, &detail);
            Ok(())
        }),
        Box::new(done),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn file_renamed_recovery(
    docs: &Rc<Docs>,
    view: ViewId,
    old: &std::path::Path,
    new: &std::path::Path,
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    then: Then<()>,
) {
    let destination = match docs.checked_document_owner(view, destination.clone()) {
        Ok(owner) => owner,
        Err(error) => return then(Err(error)),
    };
    let Some(effects) = docs.services.hosts.get::<OwnedEffectsHost>().cloned() else {
        return then(Err(kit::error("native recovery effects are unavailable")));
    };
    let mut captured = participants.to_vec();
    captured.push(destination);
    let target = captured.len() - 1;
    let old = old.to_string_lossy().into_owned();
    let new = new.to_string_lossy().into_owned();
    let mut moved = None;
    if let Err(error) = effects.with_recovery_open_effect_many(
        receipt,
        receiver,
        &captured,
        cancel,
        pending,
        &mut || {
            if docs.open_doc.get(view).as_deref() != Some(old.as_str()) {
                return Err("the Word document path changed before relocation".into());
            }
            moved = Some(docs.rename_document_state(view, &old, &new));
            Ok(())
        },
    ) {
        return then(Err(kit::error(error)));
    }
    let Some(media_result) = moved else {
        return then(Err(kit::error("the Word relocation was not admitted")));
    };
    let payload = super::Obj::new()
        .set("oldPath", old)
        .set("newPath", new)
        .done();
    if let Err(error) = effects.emit_recovery_open_many(
        receipt,
        receiver,
        &captured,
        target,
        cancel,
        pending,
        "docs:renamed",
        vec![payload],
    ) {
        return then(Err(kit::error(error)));
    }
    if let Err(error) = media_result {
        if let Err(admission) = effects.with_recovery_open_effect_many(
            receipt,
            receiver,
            &captured,
            cancel,
            pending,
            &mut || {
                docs.torn_down.borrow_mut().insert(view);
                docs.doc_writable.remove(view);
                docs.pdf_writable.remove(view);
                Ok(())
            },
        ) {
            return then(Err(kit::error(admission)));
        }
        let dialog = docs.dialogs.clone();
        let title = docs.tm("dlgOpenDoc");
        let detail =
            format!("Document saving was disabled because the media service failed: {error}");
        let done = kit::back_on_core(&docs.services, move |result: Result<(), String>| {
            then(Err(kit::error(result.err().unwrap_or(error))));
        });
        effects.post_ui_recovery_open_many(
            receipt.clone(),
            receiver.clone(),
            captured,
            cancel.clone(),
            pending.clone(),
            Box::new(move || {
                dialog.error(&title, &detail);
                Ok(())
            }),
            Box::new(done),
        );
        return;
    }
    then(Ok(()));
}
