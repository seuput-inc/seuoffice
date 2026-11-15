use super::*;
use std::path::PathBuf;
use super::super::owned_save::{SaveRevision, SaveScope};
use main_core::packages::electron_utils::atomic_write::owned::{Admission, write_file};

struct Staging(PathBuf);
impl Staging {
    fn create(id: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("seuoffice-slides-pdf-{id}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
        builder.create(&path).map_err(|error| error.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Staging { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

pub(in crate::slides) fn export_pdf(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let mut op = args.get(0).clone();
    let path = match as_str(field(&op, "filePath")) { Some(path) => path.to_owned(), None => return responder.reply(Err(thrown("The PDF destination is required"))) };
    let save_id = as_str(field(&op, "saveRequestId")).map(str::to_owned);
    let scope = match app.export_scope(cx, as_str(field(&op, "saveRequestId")), "pdf", &path) { Ok(scope) => scope, Err(error) => return responder.reply(Err(error)) };
    let revision = match scope.revision(app) { Ok(revision) => revision, Err(error) => return responder.reply(Err(error)) };
    let admission = scope.revision_destination_admission(app, revision.clone(), path.clone());
    let id = app.shell.random_uuid(); let shell = app.shell.clone(); let request = scope.request.clone(); let invocation = scope.invocation.clone(); let destination = scope.native_destination.clone();
    let services = app.services.clone(); let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let result = stage_and_write(&*shell, &mut op, &path, &id, &request, invocation, destination, admission);
        super::super::post_to(&services, index, move |app| app.pdf_export_written(scope, revision, save_id, path, result, responder));
    }));
}

fn stage_and_write(shell: &dyn super::super::engine::SlidesShell, op: &mut IpcValue, path: &str, id: &str, request: &crate::owned_save::OwnedSave, invocation: Option<shell_contract::ipc::NativeInvocationDocument>, destination: Option<shell_contract::ipc::NativeSaveDestination>, admission: Admission) -> Result<(), String> {
    let staging = Staging::create(id)?;
    let staged = staging.0.join("presentation.pdf");
    let staged = staged.to_str().ok_or("The PDF staging path is not Unicode")?;
    let IpcValue::Object(entries) = op else { return Err("The PDF export request must be an object".into()); };
    let entry = entries.iter_mut().find(|(key, _)| key == "filePath").ok_or("The PDF destination is required")?;
    entry.1 = string(staged);
    let participants = request.participants(&[]);
    let result = match (&request.authority, invocation, destination) {
        (crate::owned_save::SaveAuthority::Renderer(origin), Some(document), None) => shell.export_pdf_native_invocation(op, &document, origin, &request.destination, &request.cancel, &request.pending, &participants, &request.additional, &request.effects),
        (crate::owned_save::SaveAuthority::Renderer(origin), None, None) => shell.export_pdf_owned(op, origin, &request.destination, &request.cancel, &request.pending, &participants, &request.additional, &request.effects),
        (crate::owned_save::SaveAuthority::Native(_), None, Some(destination)) => shell.export_pdf_existing_document(op, &destination, &participants, &request.cancel, &request.pending, &request.additional, &request.effects),
        _ => return Err("The PDF save authority does not match its destination".into()),
    };
    if !result.ok { return Err(result.error.unwrap_or_else(|| "The PDF renderer did not complete".into())); }
    if result.path.as_deref() != Some(staged) { return Err("The PDF renderer returned another staging path".into()); }
    let bytes = std::fs::read(staged).map_err(|error| error.to_string())?;
    write_file(path, &bytes, admission).map_err(|error| error.message)
}

type GeneratedResult = Result<shell_contract::hosts::GeneratedPathOutcome, String>;
type GeneratedConsumer = Box<dyn FnOnce(&App, &GeneratedResult) -> Box<dyn FnOnce()>>;

struct GeneratedInbox {
    delivery: std::sync::Mutex<Option<shell_contract::hosts::GeneratedTerminalDelivery>>,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    index: usize,
    id: String,
}
impl GeneratedInbox {
    fn send(self: &std::sync::Arc<Self>, delivery: shell_contract::hosts::GeneratedTerminalDelivery) {
        let mut delivery = Some(delivery);
        let mut slot = self.delivery.lock().unwrap_or_else(|error| error.into_inner());
        if slot.is_none() { *slot = delivery.take(); }
        drop(slot);
        drop(delivery);
        self.wake();
    }
    fn wake(self: &std::sync::Arc<Self>) {
        let id = self.id.clone(); let index = self.index;
        self.core.post(Box::new(move || {
            super::super::with_app(index, |app| resume_generated_publication(app, &id));
        }));
    }
}

pub(in crate::slides) struct PendingGeneratedPublication {
    inbox: std::sync::Arc<GeneratedInbox>,
    consumer: Option<GeneratedConsumer>,
    task: Option<std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending>>,
    prepared: Option<Box<dyn shell_contract::hosts::PreparedGeneratedPath>>,
    observer: Option<(shell_contract::hosts::GeneratedPathObserver, shell_contract::hosts::GeneratedPathWake)>,
    failure: Option<String>,
    settled: bool,
    acknowledging: bool,
}

fn begin_generated_publication(app: &App, consumer: GeneratedConsumer) -> Result<(String, shell_contract::hosts::GeneratedPathWake), GeneratedConsumer> {
    let id = app.shell.random_uuid();
    let mut state = app.state.borrow_mut();
    if state.generated_publications.contains_key(&id) || state.generated_publications.try_reserve(1).is_err() { return Err(consumer); }
    let inbox = std::sync::Arc::new(GeneratedInbox { delivery: std::sync::Mutex::new(None), core: app.services.core.clone(), index: app.registry_index, id: id.clone() });
    state.generated_publications.insert(id.clone(), PendingGeneratedPublication {
        inbox: inbox.clone(), consumer: Some(consumer), task: None, prepared: None, observer: None, failure: None, settled: false, acknowledging: false,
    });
    drop(state);
    let wake = std::sync::Arc::new(move || inbox.wake());
    Ok((id, wake))
}

fn fail_generated_preparation(app: &App, id: &str, error: String) {
    let entry = app.state.borrow_mut().generated_publications.remove(id);
    if let Some(mut entry) = entry {
        if let Some(consumer) = entry.consumer.take() { consumer(app, &Err(error))(); }
    }
}

fn install_generated_publication(app: &App, id: &str, prepared: Box<dyn shell_contract::hosts::PreparedGeneratedPath>, wake: shell_contract::hosts::GeneratedPathWake) {
    let task = prepared.task();
    let inbox = {
        let mut state = app.state.borrow_mut();
        let Some(entry) = state.generated_publications.get_mut(id) else { return };
        entry.task = Some(task.clone());
        entry.prepared = Some(prepared);
        entry.inbox.clone()
    };
    let observer: shell_contract::hosts::GeneratedPathObserver = Box::new(move |delivery| inbox.send(delivery));
    if let Err(failure) = task.observe(observer, wake) {
        if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) {
            entry.failure = Some(failure.error);
            entry.observer = Some((failure.observer, failure.wake));
        }
        return;
    }
    start_generated_publication(app, id);
}

fn start_generated_publication(app: &App, id: &str) {
    let prepared = app.state.borrow_mut().generated_publications.get_mut(id).and_then(|entry| entry.prepared.take());
    let Some(prepared) = prepared else { return };
    if let Err(failure) = prepared.start() {
        if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) {
            entry.failure = Some(failure.error);
            entry.prepared = Some(failure.prepared);
        }
    }
}

fn dispatch_generated_publication(app: &App, id: &str) {
    let inbox = match app.state.borrow().generated_publications.get(id) { Some(entry) => entry.inbox.clone(), None => return };
    let reply = {
        let delivery = inbox.delivery.lock().unwrap_or_else(|error| error.into_inner());
        let Some(delivery) = delivery.as_ref() else { return };
        if matches!(delivery.result(), Ok(shell_contract::hosts::GeneratedPathOutcome::CleanupPending(_))) { return; }
        let consumer = app.state.borrow_mut().generated_publications.get_mut(id).and_then(|entry| entry.consumer.take());
        consumer.map(|consumer| consumer(app, delivery.result()))
    };
    if let Some(reply) = reply {
        reply();
        if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) { entry.settled = true; }
    }
    finish_generated_publication(app, id);
}

fn finish_generated_publication(app: &App, id: &str) {
    let inbox = {
        let mut state = app.state.borrow_mut();
        let Some(entry) = state.generated_publications.get_mut(id).filter(|entry| entry.settled && !entry.acknowledging) else { return };
        entry.acknowledging = true;
        entry.inbox.clone()
    };
    let delivery = inbox.delivery.lock().unwrap_or_else(|error| error.into_inner()).take();
    let Some(mut delivery) = delivery else {
        if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) { entry.acknowledging = false; }
        return;
    };
    match delivery.acknowledge() {
        Ok(()) => { app.state.borrow_mut().generated_publications.remove(id); }
        Err(error) => {
            *inbox.delivery.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(delivery);
            if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) { entry.failure = Some(error); entry.acknowledging = false; }
        }
    }
}

fn resume_generated_publication(app: &App, id: &str) {
    let (task, observer, inbox, settled) = {
        let mut state = app.state.borrow_mut();
        let Some(entry) = state.generated_publications.get_mut(id) else { return };
        (entry.task.clone(), entry.observer.take(), entry.inbox.clone(), entry.settled)
    };
    if settled { finish_generated_publication(app, id); return; }
    if let (Some(task), Some((observer, wake))) = (task.as_ref(), observer) {
        match task.observe(observer, wake) {
            Ok(()) if !task.is_cancelling() => start_generated_publication(app, id),
            Ok(()) => {}
            Err(failure) => if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) {
                entry.failure = Some(failure.error);
                entry.observer = Some((failure.observer, failure.wake));
            },
        }
    }
    if task.as_ref().is_some_and(|task| task.is_cancelling()) {
        let prepared = app.state.borrow_mut().generated_publications.get_mut(id).and_then(|entry| entry.prepared.take());
        drop(prepared);
    }
    let empty = inbox.delivery.lock().unwrap_or_else(|error| error.into_inner()).is_none();
    if empty {
        if let Some(task) = task.as_ref() {
            match task.take_redelivery() {
                Ok(Some(delivery)) => {
                    let mut slot = inbox.delivery.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.is_none() { *slot = Some(delivery); }
                }
                Ok(None) => {}
                Err(error) => if let Some(entry) = app.state.borrow_mut().generated_publications.get_mut(id) { entry.failure = Some(error); },
            }
        }
    }
    dispatch_generated_publication(app, id);
}

pub(in crate::slides) fn wake_generated_publications(app: &App) {
    let ids: Vec<_> = app.state.borrow().generated_publications.keys().cloned().collect();
    for id in ids { resume_generated_publication(app, &id); }
}

impl App {
    fn pdf_export_written(&self, scope: SaveScope, revision: SaveRevision, save_id: Option<String>, path: String, result: Result<(shell_contract::ipc::SavedPublication, Option<shell_contract::ipc::NativeSavedInput>), String>, responder: Responder) {
        let (output, input) = match result.map_err(thrown).and_then(|value| scope.effect_at_revision(self, &revision, || value)) {
            Ok(value) => value,
            Err(error) => return responder.reply(Err(error)),
        };
        if self.shell.is_headless() { return responder.reply(Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(path))]))); }
        let request = scope.request.clone(); let reply_path = path.clone();
        let accepted = scope.reload_losses.clone().unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(super::super::owned_save::ReloadLossWatch::default())));
        let accepted_callback = accepted.clone(); let pending = request.pending.clone(); let origin = match &request.authority {
            crate::owned_save::SaveAuthority::Renderer(origin) => Some(origin.clone()),
            crate::owned_save::SaveAuthority::Native(_) => None,
        }; let cancel = request.cancel.clone();
        let callback_core = self.services.core.clone(); let callback_index = self.registry_index; let callback_id = save_id.clone();
        let on_accepted = Box::new(move |marker: shell_contract::hosts::AcceptedPdfReload| {
            let bundle = if let Some(origin) = origin.as_ref().filter(|origin| marker.retires(origin)) {
                match origin.accepted_reload_losses(&marker, &cancel, &pending) {
                    Ok(bundle) => Some(bundle),
                    Err(error) => {
                        if let Ok(mut watch) = accepted_callback.lock() { watch.failure = Some(error); }
                        pending.cancel(); return;
                    }
                }
            } else { None };
            let tokens = bundle.as_ref().map_or_else(|| vec![marker.browser_loss().clone(), marker.reservation_loss().clone()], |bundle| bundle.tokens().cloned().collect());
            let mut subscriptions = Vec::with_capacity(tokens.len());
            for loss in tokens {
                let pending = pending.clone(); let core = callback_core.clone(); let id = callback_id.clone();
                subscriptions.push(loss.on_cancel(move || {
                    pending.cancel();
                    if let Some(id) = &id {
                        let id = id.clone();
                        core.post(Box::new(move || { crate::slides::with_app(callback_index, |app| app.finish_owned_save(&id, Err(thrown("The PDF reload operation was released")))); }));
                    }
                }));
            }
            match accepted_callback.lock() {
                Ok(mut watch) => { watch.marker = Some(marker); watch.bundle = bundle; watch.subscriptions = subscriptions; }
                Err(_) => pending.cancel(),
            }
        });
        let consumer: GeneratedConsumer = Box::new(move |app: &App, answer: &GeneratedResult| {
                let result = answer.as_ref().map_err(|error| thrown(error.clone())).and_then(|outcome| {
                    match outcome {
                        shell_contract::hosts::GeneratedPathOutcome::NotOpened => scope.effect_at_revision(app, &revision, || ())?,
                        shell_contract::hosts::GeneratedPathOutcome::Opened(receipt) => {
                            {
                                let watch = accepted.lock().map_err(|_| thrown("The PDF reload operation state is unavailable"))?;
                                if let Some(error) = &watch.failure { return Err(thrown(error.clone())); }
                                if watch.marker.as_ref().is_some_and(|marker| marker.browser_loss().is_cancelled() || marker.reservation_loss().is_cancelled())
                                    || watch.bundle.as_ref().is_some_and(|bundle| bundle.tokens().any(shell_contract::runtime::CancelToken::is_cancelled)) { return Err(thrown("The PDF reload reservation was released")); }
                            }
                            receipt.with_terminal_commit(&mut || scope.check_revision_state(app, &revision).map_err(|error| error.message)).map_err(thrown)?;
                            if let Some(id) = &save_id {
                                let mut state = app.state.borrow_mut();
                                let save = state.owned_saves.get_mut(id).filter(|save| save.scope.request.pending.same_token(&scope.request.pending)).ok_or_else(|| thrown("The save copy request was released"))?;
                                save.copy_completion = Some((revision.clone(), receipt.clone()));
                            }
                        }
                        shell_contract::hosts::GeneratedPathOutcome::CleanupPending(_) => return Err(thrown("The PDF cleanup did not reach a terminal result")),
                    }
                    Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(reply_path))]))
                });
                Box::new(move || responder.reply(result))
            });
        let (id, wake) = match begin_generated_publication(self, consumer) {
            Ok(value) => value,
            Err(consumer) => return consumer(self, &Err("The PDF completion could not be reserved".into()))(),
        };
        match self.shell.prepare_reveal_export(output, input, request.participants(&[]), request.cancel.clone(), request.pending.clone(), request.additional.clone(), request.effects.clone(), on_accepted, wake.clone()) {
            Ok(prepared) => install_generated_publication(self, &id, prepared, wake),
            Err(error) => fail_generated_preparation(self, &id, error),
        }
    }
}
