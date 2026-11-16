//! Namespace operations return to the Docs actor; staging and bulk IO do not.

use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::{
    Admission, EffectValue, ShortEffect,
};
use shell_contract::runtime::Services;
use shell_contract::{ViewId, ipc::NativeRequestOwner, runtime::CancelToken};

use super::Docs;
use super::owned_save::Scope;
use crate::kit;

#[derive(Clone, Copy)]
pub(super) enum Permission {
    ExistingDocument,
    RequestTarget,
    ProjectDocument,
}

type CoreCallback = Box<dyn FnOnce(CoreRequest) + Send>;
pub(super) type CoreEffect = Rc<dyn Fn(ShortEffect) -> Result<EffectValue, NodeError>>;
struct CoreRequest {
    effect: ShortEffect,
    reply: mpsc::SyncSender<CoreReply>,
}
struct CoreReply {
    result: Result<EffectValue, NodeError>,
    next: CoreCallback,
}

fn error(message: impl Into<String>) -> NodeError {
    NodeError {
        code: None,
        message: message.into(),
    }
}

fn arm(services: Services, effect: CoreEffect) -> CoreCallback {
    let next_services = services.clone();
    kit::back_on_core(&services, move |request: CoreRequest| {
        let CoreRequest {
            effect: operation,
            reply,
        } = request;
        let result = effect(operation);
        let next = arm(next_services, effect);
        let _ = reply.send(CoreReply { result, next });
    })
}

pub(super) fn core_admission(services: Services, effect: CoreEffect) -> Admission {
    let current = Mutex::new(Some(arm(services, effect)));
    Arc::new(move |effect| {
        let callback = current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| error("another final save operation is still active"))?;
        let (send, receive) = mpsc::sync_channel(1);
        callback(CoreRequest {
            effect,
            reply: send,
        });
        let completed = receive
            .recv()
            .map_err(|_| error("the Word file-operation continuation was lost"))?;
        *current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(completed.next);
        completed.result
    })
}

pub(super) fn admission(
    docs: &Rc<Docs>,
    scope: Scope,
    path: &str,
    permission: Permission,
) -> Admission {
    let actor = Rc::downgrade(docs);
    let path = Arc::<str>::from(path);
    core_admission(
        docs.services.clone(),
        Rc::new(move |effect| {
            let docs = actor
                .upgrade()
                .ok_or_else(|| error("the Word editor was released before the file operation"))?;
            scope
                .effect_path(&path, || {
                    let view = scope.destination().view;
                    if docs.is_torn_down(view) {
                        return Err(error("the target Word document is no longer open"));
                    }
                    if matches!(permission, Permission::ExistingDocument)
                        && !docs.can_doc_write(view, &path)
                    {
                        return Err(error("save target is not an opened document"));
                    }
                    if matches!(permission, Permission::ProjectDocument)
                        && docs.open_doc.contains(view)
                    {
                        return Err(error("the Word document already has a saved file"));
                    }
                    Ok(effect())
                })
                .map_err(error)
                .and_then(|result| result)
        }),
    )
}

pub(super) fn headless_admission(
    docs: &Rc<Docs>,
    publication: super::open::HeadlessPublication,
) -> Admission {
    let actor = Rc::downgrade(docs);
    let effects = publication.effects.clone();
    core_admission(
        docs.services.clone(),
        Rc::new(move |effect| {
            let docs = actor
                .upgrade()
                .ok_or_else(|| error("the Docs export actor was released"))?;
            let mut effect = Some(effect);
            let mut output = None;
            effects
                .with_native_invocation_effect(
                    &publication.document,
                    &publication.owner,
                    &[],
                    &publication.cancel,
                    &publication.pending,
                    &mut || {
                        let view = publication.owner.view;
                        if docs.is_torn_down(view) {
                            return Err("the Docs export actor is no longer open".into());
                        }
                        let waiter_matches = docs
                            .headless_waiters
                            .borrow()
                            .get(&view)
                            .and_then(|(target, _)| target.as_ref())
                            .is_some_and(|(target, _)| {
                                target
                                    .created()
                                    .same_generation(publication.document.created())
                                    && target.operation().authority_generation().same_generation(
                                        publication.document.operation().authority_generation(),
                                    )
                            });
                        if !waiter_matches {
                            return Err("the Docs export is no longer pending".into());
                        }
                        output = Some(effect
                            .take()
                            .ok_or("the Docs export publication was admitted twice")?(
                        ));
                        Ok(())
                    },
                )
                .map_err(error)?;
            output.ok_or_else(|| error("the Docs export publication was not admitted"))
        }),
    )
}

impl Docs {
    pub(super) fn capture_create_admission(
        self: &Rc<Self>,
        view: ViewId,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
    ) -> Result<Admission, String> {
        self.checked_document_owner(view, destination.clone())
            .map_err(|error| error.to_string())?;
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("native Word publication effects are unavailable")?;
        let actor = Rc::downgrade(self);
        let expected_path = self.open_doc.get(view);
        let origin = origin.clone();
        let destination = destination.clone();
        let cancel = cancel.clone();
        let pending = pending.clone();
        Ok(core_admission(
            self.services.clone(),
            Rc::new(move |effect| {
                let docs = actor
                    .upgrade()
                    .ok_or_else(|| error("the creating Word actor was released"))?;
                let mut operation = Some(effect);
                let mut output = None;
                effects
                    .with_owned_effect(&origin, &destination, &cancel, &pending, &mut || {
                        if docs.is_torn_down(view)
                            || docs
                                .open_doc
                                .with(view, |current| current != expected_path.as_ref())
                        {
                            return Err("the captured Word creation source changed".into());
                        }
                        let effect = operation
                            .take()
                            .ok_or("the Word publication was admitted more than once")?;
                        output = Some(effect());
                        Ok(())
                    })
                    .map_err(error)?;
                output.ok_or_else(|| error("the Word publication was not admitted"))
            }),
        ))
    }
}
