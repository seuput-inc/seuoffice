use std::collections::HashSet;
use std::path::Path;

use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone;
use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    CallContext, NativeInvocationSelection, NativeRequestOwner, RecoveryOpenContext,
    RecoveryOpenTarget, RecoverySeedReceipt,
};
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::{Epub, EpubEditors, OpenRun, Session, ViewState, object};
use crate::kit::{self, Args, IntoIpc};

pub(super) struct Pending {
    target: RecoveryOpenTarget,
    pub(super) receipt: Option<RecoverySeedReceipt>,
    token: String,
    path: String,
    pub(super) document: Option<RendererGeneration>,
    _loss: Vec<CancelSubscription>,
}

#[derive(Clone)]
pub(super) struct OpenUse {
    pub(super) receipt: RecoverySeedReceipt,
    pub(super) owner: NativeRequestOwner,
    pub(super) cancel: CancelToken,
}

fn effects(epub: &Epub) -> Result<OwnedEffectsHost, String> {
    epub.services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "Native recovery effects are unavailable".into())
}

fn owner(cx: &CallContext) -> Result<NativeRequestOwner, String> {
    let context = cx
        .native
        .clone()
        .ok_or("The EPUB recovery renderer has no native owner")?;
    Ok(NativeRequestOwner {
        view: cx.sender.view,
        context,
    })
}

impl EpubEditors {
    pub fn queue_recovery_direct(
        &self,
        view: ViewId,
        target: &RecoveryOpenTarget,
        path: String,
        done: RecoveryOpenDone,
    ) {
        let result = (|| -> Result<(), String> {
            if target.view() != view {
                return Err("The EPUB recovery target does not match the view".into());
            }
            match target.context() {
                RecoveryOpenContext::NativeStartup(operation) => match operation.selection() {
                    NativeInvocationSelection::RecoveryFile {
                        surface: Surface::Epub,
                        path: selected,
                        ..
                    } if selected == Path::new(&path) => {}
                    _ => return Err("The EPUB recovery selection does not match the file".into()),
                },
                RecoveryOpenContext::Renderer { .. } => {
                    return Err("The EPUB direct recovery requires a native selection".into());
                }
            }
            let effects = effects(&self.0)?;
            let token = self.0.shell.random_token();
            let mut pending = Some(Pending {
                target: target.clone(),
                receipt: None,
                token,
                path: path.clone(),
                document: None,
                _loss: Vec::new(),
            });
            let receipt = effects.commit_recovery_seed(target, &mut || {
                target
                    .closure_reservation()
                    .with_initial_owner(target.browser_owner(), |_| {
                        let mut state = self.0.state.borrow_mut();
                        let entry = state.views.entry(view).or_insert_with(|| ViewState {
                            unopened: true,
                            ..ViewState::default()
                        });
                        if entry.recovery.is_some() || entry.session.is_some() {
                            return Err("The EPUB view already has a document".into());
                        }
                        entry.open_path = Some(path.clone());
                        entry.allowed = HashSet::from([path.clone()]);
                        entry.unopened = true;
                        entry.recovery = pending.take();
                        Ok(())
                    })
            })?;
            let mut losses = vec![target.created().loss_token()];
            if let RecoveryOpenContext::NativeStartup(operation) = target.context() {
                losses.extend([
                    operation.loss_token(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                ]);
            }
            let subscriptions = losses
                .into_iter()
                .map(|loss| {
                    let receipt = receipt.clone();
                    loss.on_cancel(move || {
                        if let Ok(notification) =
                            receipt.fail_after_loss("The EPUB recovery target was released".into())
                        {
                            notification.notify();
                        }
                    })
                })
                .collect();
            let mut state = self.0.state.borrow_mut();
            let seed = state
                .views
                .get_mut(&view)
                .and_then(|entry| entry.recovery.as_mut())
                .filter(|seed| seed.target.same_binding(target))
                .ok_or("The EPUB recovery seed was released")?;
            seed.receipt = Some(receipt);
            seed._loss = subscriptions;
            Ok(())
        })();
        done(result.map_err(Failure));
    }
}

impl Epub {
    pub(super) fn begin_recovery_open(
        &self,
        view: ViewId,
        path: &str,
        scope: &OpenUse,
    ) -> Result<RendererGeneration, String> {
        let generation = RendererGeneration::new();
        let mut previous = None;
        effects(self)?.with_recovery_open_effect(
            &scope.receipt,
            &scope.owner,
            &scope.cancel,
            &mut || {
                let mut state = self.state.borrow_mut();
                let entry = state
                    .views
                    .get_mut(&view)
                    .ok_or("The EPUB view was released")?;
                let pending = entry
                    .recovery
                    .as_mut()
                    .ok_or("The EPUB recovery seed was released")?;
                if pending.path != path
                    || !pending
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.matches(scope.receipt.target()))
                {
                    return Err("The EPUB recovery path does not match".into());
                }
                pending.document = None;
                previous = entry.document.replace(generation.clone());
                Ok(())
            },
        )?;
        if let Some(previous) = previous {
            previous.revoke();
        }
        Ok(generation)
    }

    pub(super) fn finish_recovery_open(
        &self,
        view: ViewId,
        path: &str,
        scope: &OpenUse,
        document: &RendererGeneration,
        run: OpenRun,
    ) -> Result<IpcValue, String> {
        let token = matches!(&run, OpenRun::Opened(..)).then(|| self.shell.random_token());
        let mut run = Some(run);
        let mut result = None;
        let mut did_open = false;
        let mut receiver = scope.owner.clone();
        if receiver
            .context
            .document_lease()
            .is_some_and(|lease| !lease.same_generation(document))
        {
            return Err("The EPUB recovery document was replaced".into());
        }
        receiver.context = receiver.context.with_document_lease(document.clone());
        effects(self)?.with_recovery_open_effect(
            &scope.receipt,
            &receiver,
            &scope.cancel,
            &mut || {
                let mut state = self.state.borrow_mut();
                let entry = state
                    .views
                    .get_mut(&view)
                    .ok_or("The EPUB view was released")?;
                let pending = entry
                    .recovery
                    .as_mut()
                    .ok_or("The EPUB recovery seed was released")?;
                if pending.path != path
                    || !pending
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.matches(scope.receipt.target()))
                    || !entry
                        .document
                        .as_ref()
                        .is_some_and(|current| current.same_generation(document))
                {
                    return Err("The EPUB document was replaced".into());
                }
                match run.take().ok_or("The EPUB open result was consumed")? {
                    OpenRun::Unreadable => {
                        entry.unopened = true;
                        entry.save_path = None;
                        entry.session = None;
                        entry.renderer_dirty = false;
                        result = Some(object(vec![
                            ("status", "refused".into_ipc()),
                            ("reason", "unreadable".into_ipc()),
                        ]));
                    }
                    OpenRun::Refused(reason) => {
                        entry.unopened = true;
                        entry.save_path = None;
                        entry.session = None;
                        entry.renderer_dirty = false;
                        result = Some(object(vec![
                            ("status", "refused".into_ipc()),
                            ("reason", reason.into_ipc()),
                        ]));
                    }
                    OpenRun::Opened(bytes, opened, disk) => {
                        let session =
                            std::rc::Rc::new(std::cell::RefCell::new(Session::new(bytes, opened)));
                        let book = session.borrow();
                        let asset = entry
                            .token
                            .get_or_insert(
                                token.clone().ok_or("The EPUB asset token is unavailable")?,
                            )
                            .clone();
                        let hash = book.doc_hash.clone();
                        let answer = object(vec![
                            ("status", "ok".into_ipc()),
                            ("book", book.summary_ipc()),
                            ("assetBase", format!("epub-asset://{asset}/").into_ipc()),
                            ("docHash", hash.as_str().into_ipc()),
                            ("readOnly", book.book_read_only().into_ipc()),
                        ]);
                        drop(book);
                        entry.session = Some(session);
                        if let Some(disk) = disk {
                            entry.disk_states.insert(path.to_owned(), disk);
                        }
                        entry.unopened = false;
                        entry.save_path = Some(path.to_owned());
                        entry.known_hashes.insert(hash);
                        pending.document = Some(document.clone());
                        did_open = true;
                        result = Some(answer);
                    }
                }
                Ok(())
            },
        )?;
        if did_open {
            let receipt = scope.receipt.clone();
            let loss = document.loss_token().on_cancel(move || {
                if let Ok(notification) =
                    receipt.fail_after_loss("The recovered EPUB document was replaced".into())
                {
                    notification.notify();
                }
            });
            self.with_view(view, |entry| {
                if let Some(pending) = entry.recovery.as_mut().filter(|pending| {
                    pending
                        .document
                        .as_ref()
                        .is_some_and(|current| current.same_generation(document))
                }) {
                    pending._loss.push(loss);
                }
            });
        }
        result.ok_or_else(|| "The EPUB open result was not applied".into())
    }

    pub(super) fn recovery_use(&self, cx: &CallContext) -> Result<Option<OpenUse>, String> {
        let receipt = self
            .view(cx.sender.view, |entry| {
                entry
                    .recovery
                    .as_ref()
                    .and_then(|pending| pending.receipt.clone())
            })
            .flatten();
        let Some(receipt) = receipt else {
            return Ok(None);
        };
        let owner = owner(cx)?;
        effects(self)?.with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || Ok(()))?;
        Ok(Some(OpenUse {
            receipt,
            owner,
            cancel: cx.cancel.clone(),
        }))
    }

    pub(super) fn consume_recovery(&self, cx: &CallContext) -> Result<IpcValue, IpcError> {
        let view = cx.sender.view;
        let Some(scope) = self.recovery_use(cx).map_err(kit::error)? else {
            return Ok(self
                .view(view, |entry| entry.open_path.clone())
                .flatten()
                .into_ipc());
        };
        let mut answer = None;
        effects(self)
            .map_err(kit::error)?
            .with_recovery_open_effect(&scope.receipt, &scope.owner, &scope.cancel, &mut || {
                let state = self.state.borrow();
                let pending = state
                    .views
                    .get(&view)
                    .and_then(|entry| entry.recovery.as_ref())
                    .ok_or("The EPUB recovery seed was released")?;
                if !pending
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.matches(scope.receipt.target()))
                {
                    return Err("The EPUB recovery seed was replaced".into());
                }
                answer = Some(object(vec![
                    ("path", pending.path.clone().into_ipc()),
                    ("receipt", pending.token.clone().into_ipc()),
                ]));
                Ok(())
            })
            .map_err(kit::error)?;
        answer.ok_or_else(|| kit::error("The EPUB recovery seed was not delivered"))
    }

    pub(super) fn recovery_initialized(
        &self,
        cx: &CallContext,
        args: Args,
    ) -> Result<IpcValue, IpcError> {
        let id = args
            .str(0)
            .ok_or_else(|| kit::error("The EPUB recovery receipt is required"))?;
        let ok = matches!(args.get(1), IpcValue::Bool(true));
        let error = args
            .str(2)
            .unwrap_or("The EPUB could not be opened")
            .to_owned();
        let scope = self
            .recovery_use(cx)
            .map_err(kit::error)?
            .ok_or_else(|| kit::error("No EPUB recovery is pending"))?;
        let mut document = None;
        effects(self)
            .map_err(kit::error)?
            .with_recovery_open_effect(&scope.receipt, &scope.owner, &scope.cancel, &mut || {
                let state = self.state.borrow();
                let entry = state
                    .views
                    .get(&cx.sender.view)
                    .ok_or("The EPUB view was released")?;
                let pending = entry
                    .recovery
                    .as_ref()
                    .ok_or("The EPUB recovery seed was released")?;
                if pending.token != id
                    || !pending
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.matches(scope.receipt.target()))
                {
                    return Err("The EPUB recovery receipt does not match".into());
                }
                if ok
                    && (!entry.session.is_some()
                        || entry.unopened
                        || !pending
                            .document
                            .as_ref()
                            .zip(entry.document.as_ref())
                            .is_some_and(|(captured, current)| captured.same_generation(current)))
                {
                    return Err("The recovered EPUB document was not opened".into());
                }
                if ok {
                    document = pending.document.clone();
                }
                Ok(())
            })
            .map_err(kit::error)?;
        let mut receiver = scope.owner.clone();
        if let Some(document) = document {
            if receiver
                .context
                .document_lease()
                .is_some_and(|lease| !lease.same_generation(&document))
            {
                return Err(kit::error("The EPUB recovery document was replaced"));
            }
            receiver.context = receiver.context.with_document_lease(document);
        }
        effects(self)
            .map_err(kit::error)?
            .settle_recovery_seed(
                &scope.receipt,
                &receiver,
                &scope.cancel,
                if ok { Ok(()) } else { Err(error) },
            )
            .map_err(kit::error)?;
        self.with_view(cx.sender.view, |entry| {
            if entry
                .recovery
                .as_ref()
                .is_some_and(|pending| pending.token == id)
            {
                entry.recovery = None;
            }
        });
        Ok(IpcValue::Null)
    }
}
