//! The generate window ("make a new document from these files",
//! `apps/shell/src/main/generate/generate-window.ts`) and the ways into it:
//! the ten `generate:*` channels the window's own page calls, the two Home
//! calls (`generate:open-with-paths`, `generate:open-with-group`) and the
//! `generate:state` event the window is pushed.
//!
//! The module is in `main-core` (`main_core::shell::generate`); this is its
//! shell: [`GenerateWindowPort`] holds the module's window state and answers
//! the channels, and the Electron parts come through [`host::GenerateShell`]
//! (the window, the tabs and the engines `main-core` does not hold yet) and
//! the shared hosts (dialogs, the OS shell, the main window).
//!
//! The port uses the one hand-off store (`handoff.ts`: what "make" queues
//! for a new tab, taken once by its editor), shared with the skills channels
//! through `crate::user_skills::shared_handoff_store`, and owns the make
//! window entry an editor's File menu uses (`openGenerateWindowForDocument`):
//! [`GenerateWindowPort::handoff_store`] and
//! [`GenerateWindowPort::make_window_opener`] are what the shell hands to
//! the menus (`attach_make_window`).
//!
//! Nothing blocks the core thread on a renderer or a timer: the open tabs
//! (`deps.list()`) are asked for first and the call goes on when they are
//! known; "save" waits for the editor's answer and for the recheck timer as
//! resumable steps; the file picker of "add" runs on a worker.

mod bridge;
pub mod host;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use main_core::packages::electron_utils::generate_handoff::HandoffStore;
use main_core::packages::electron_utils::unsaved_files::{
    OpenFileState, OpenFilesHolder, OpenFilesSourceFailed,
};
use main_core::shell::document_links_shell::GenerateWindowRequest;
use main_core::shell::generate::generate_window::{
    GenerateWindow as WindowModule, WindowId as ModuleWindow, WindowStep, WindowNeed, WindowRun,
    WindowOutcome, WindowOpen,
};
use main_core::shell::generate::open_materials::{OpenDocumentTab, OpenSnapshot};
use main_core::shell::generate::session::SessionAnswer;
use main_core::shell::renderer_disposal::Failure;
use serde_json::Value;
use shell_contract::hosts::{DialogsHost, MainWindowHost, OsShellHost};
use shell_contract::ipc::{CallContext, NativeContext, Responder};
use shell_contract::runtime::{CancelToken, Services};
use shell_contract::{IpcError, IpcValue, WindowId};

use self::bridge::{Bridge, dialog_result, file_dialog};
use self::host::{GenerateAuthority, GenerateOrigin, GenerateShellHost, GenerateWindowEvent};
use crate::ports::GenerateWindow as GenerateWindowChannels;

/// The state of the port every callback reaches: shared with the window's
/// listeners, which the host calls from any thread (they post to the core
/// thread, where the state is touched)
pub struct Inner {
    shell: GenerateShellHost,
    services: Services,
    dialogs: DialogsHost,
    os_shell: OsShellHost,
    main_window: MainWindowHost,
    /// the module state of generate-window.ts
    window: Mutex<WindowModule>,
}

#[derive(Clone)]
pub struct GenerateWindowEvents {
    inner: Arc<Inner>,
    binding: Arc<Mutex<EventBinding>>,
}

enum EventBinding {
    Unbound,
    Bound(CancelToken),
    Revoked,
}

#[derive(Clone)]
pub struct BoundGenerateWindowEvents {
    inner: Arc<Inner>,
    window: WindowId,
    identity: NativeContext,
    cancel: CancelToken,
    binding: Arc<Mutex<EventBinding>>,
}

impl GenerateWindowEvents {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        Self { inner, binding: Arc::new(Mutex::new(EventBinding::Unbound)) }
    }

    /// Called once by the exact first registered document, never by a view lookup.
    pub fn bind(&self, window: WindowId, identity: NativeContext, cancel: CancelToken) -> Result<BoundGenerateWindowEvents, String> {
        let previous = {
            let mut binding = self.binding.lock().unwrap_or_else(PoisonError::into_inner);
            match &*binding {
                EventBinding::Unbound => {
                    if cancel.is_cancelled() {
                        *binding = EventBinding::Revoked;
                        return Err("The generate window was already closed.".into());
                    }
                    *binding = EventBinding::Bound(cancel.clone());
                    return Ok(BoundGenerateWindowEvents { inner: self.inner.clone(), window, identity, cancel, binding: self.binding.clone() });
                }
                EventBinding::Bound(previous) => Some(previous.clone()),
                EventBinding::Revoked => None,
            }.inspect(|_| *binding = EventBinding::Revoked)
        };
        if let Some(previous) = previous { previous.cancel(); }
        Err("The generate window document was already bound or closed.".into())
    }

    /// The producer supplies its original native window-cleanup proof.
    pub fn closed(&self, window: WindowId, identity: NativeContext) {
        let previous = {
            let mut binding = self.binding.lock().unwrap_or_else(PoisonError::into_inner);
            let previous = match &*binding { EventBinding::Bound(cancel) => Some(cancel.clone()), _ => None };
            *binding = EventBinding::Revoked;
            previous
        };
        if let Some(previous) = previous { previous.cancel(); }
        BoundGenerateWindowEvents { inner: self.inner.clone(), window, identity, cancel: CancelToken::default(), binding: self.binding.clone() }.closed();
    }
}

impl BoundGenerateWindowEvents {
    fn on_core(&self, event: GenerateWindowEvent, job: impl FnOnce(&Arc<Inner>, &GenerateAuthority, WindowId) + Send + 'static) {
        let inner = self.inner.clone();
        let authority = GenerateAuthority {
            origin: GenerateOrigin::WindowEvent { window: self.window, identity: self.identity.clone(), event },
            cancel: self.cancel.clone(),
        };
        let window = self.window;
        self.inner.services.core.post(Box::new(move || {
            if let Err(error) = inner.check_authority(&authority) {
                eprintln!("[generate] native event refused: {error}");
                return;
            }
            job(&inner, &authority, window)
        }));
    }

    pub fn ready_to_show(&self, origin: NativeContext) {
        self.on_core(GenerateWindowEvent::Ready { origin }, |inner, authority, window| {
            Inner::finish_step(None, inner.run(authority, |w, b| w.on_ready_to_show(b, module_window(window))).map(|()| WindowOutcome::Unit));
        });
    }

    pub fn focus(&self) {
        self.on_core(GenerateWindowEvent::Focus, |inner, authority, window| {
            inner.snapshot(authority.clone(), move |inner, authority, snap| match snap {
                Ok(snap) => {
                    let step = inner.run(authority, |w, b| w.on_focus(b, snap, module_window(window)));
                    inner.carry_step(authority.clone(), step, None);
                }
                Err(error) => eprintln!("[generate] cannot refresh open documents: {error}"),
            });
        });
    }

    pub fn closed(&self) {
        *self.binding.lock().unwrap_or_else(PoisonError::into_inner) = EventBinding::Revoked;
        self.cancel.cancel();
        self.on_core(GenerateWindowEvent::Closed, |inner, authority, window| {
            Inner::finish_step(None, inner.run(authority, |w, b| w.on_closed(b, module_window(window))).map(|()| WindowOutcome::Unit));
        });
    }

    pub fn theme_updated(&self) {
        self.on_core(GenerateWindowEvent::Theme, |inner, authority, window| {
            Inner::finish_step(None, inner.run(authority, |w, b| w.on_theme_updated(b, module_window(window))).map(|()| WindowOutcome::Unit));
        });
    }

    pub fn open_link(&self, origin: NativeContext, url: String) {
        self.on_core(GenerateWindowEvent::OpenLink { origin }, move |inner, authority, _| {
            Inner::finish_step(None, inner.run(authority, |w, b| w.window_open(b, &url)).map(|()| WindowOutcome::Unit));
        });
    }
}

fn module_window(window: WindowId) -> ModuleWindow {
    i64::from(window.0)
}

impl Inner {
    fn window(&self) -> MutexGuard<'_, WindowModule> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check_authority(&self, authority: &GenerateAuthority) -> Result<(), String> {
        authority.check_cancelled()?;
        self.shell.check_authority(authority)
    }

    /// Runs `f` on the module state with the host of a call
    fn run<R>(self: &Arc<Self>, authority: &GenerateAuthority, f: impl FnOnce(&mut WindowModule, &mut Bridge<'_>) -> R) -> R {
        let mut bridge = Bridge::new(self, authority);
        let mut window = self.window();
        f(&mut window, &mut bridge)
    }

    /// `deps.list()`, then `then` on the core thread with the open tabs and
    /// the open files (the same list) as the module reads them
    fn snapshot(
        self: &Arc<Self>,
        authority: GenerateAuthority,
        then: impl FnOnce(&Arc<Inner>, &GenerateAuthority, Result<&OpenSnapshot<'_>, String>) + Send + 'static,
    ) {
        if let Err(error) = self.check_authority(&authority) {
            then(self, &authority, Err(error));
            return;
        }
        let inner = self.clone();
        let request_authority = authority.clone();
        self.shell.open_documents(&request_authority, Box::new(move |tabs| {
            let core = inner.services.core.clone();
            let job = move || {
                if let Err(error) = inner.check_authority(&authority) {
                    then(&inner, &authority, Err(error));
                    return;
                }
                match tabs {
                    Ok(tabs) => {
                        let holder = open_files_of(&tabs);
                        then(&inner, &authority, Ok(&OpenSnapshot { files: &holder, tabs: &tabs }));
                    }
                    Err(error) => then(&inner, &authority, Err(error)),
                }
            };
            if core.is_current() {
                job()
            } else {
                core.post(Box::new(job))
            }
        }));
    }

    fn finish_step(reply: Option<Responder>, result: Result<WindowOutcome, Failure>) {
        let result = result.map(|outcome| match outcome {
            WindowOutcome::Unit => IpcValue::Undefined,
            WindowOutcome::Made(made) => IpcValue::Bool(made),
        }).map_err(|Failure(message)| IpcError { name: Some("Error".into()), message });
        if let Some(reply) = reply {
            reply.reply(result);
        } else if let Err(error) = result {
            eprintln!("[generate] background operation failed: {}", error.message);
        }
    }

    fn resume_step(self: &Arc<Self>, authority: GenerateAuthority, run: WindowRun, answer: SessionAnswer, reply: Option<Responder>) {
        let inner = self.clone();
        self.services.core.post(Box::new(move || {
            inner.snapshot(authority, move |inner, authority, snap| match snap {
                Ok(snap) => {
                    let step = inner.run(authority, |w, b| w.resume(b, snap, run, answer));
                    inner.carry_step(authority.clone(), step, reply);
                }
                Err(error) => {
                    inner.window().discard_run(run);
                    Self::finish_step(reply, Err(Failure(error)));
                }
            });
        }));
    }

    fn complete_open(self: &Arc<Self>, authority: GenerateAuthority, run: WindowRun, result: host::GenerateOpenResult, reply: Option<Responder>) {
        let inner = self.clone();
        self.services.core.post(Box::new(move || {
            let committed = match result {
                Err(error) => return inner.resume_step(authority, run, SessionAnswer::OpenFailed(Failure(error)), reply),
                Ok(committed) => committed,
            };
            let host::GenerateCommittedOpen { receipt, target, cleanup } = committed;
            let mut pending_run = Some(run);
            let mut pending_receipt = Some(receipt);
            let mut completed = None;
            let admitted = target.with_terminal_commit(&mut || {
                let run = pending_run.take().ok_or("The generated operation was already consumed.")?;
                let receipt = pending_receipt.take().ok_or("The generated receipt was already consumed.")?;
                let result = inner.window().complete_committed_open(run, receipt).map_err(|error| error.0)?;
                completed = Some(result);
                Ok(())
            });
            if let Err(error) = admitted {
                if let Some(run) = pending_run { inner.window().discard_run(run); }
                return Self::finish_step(reply, Err(Failure(error)));
            }
            let completed = match completed {
                Some(committed) => Ok(committed),
                None => {
                    if let Some(run) = pending_run { inner.window().discard_run(run); }
                    Err(Failure("The target admission did not commit the generated operation.".into()))
                }
            };
            match completed {
                Ok(committed) if committed.cleanup_window == module_window(cleanup.window) => {
                    cleanup.complete(Box::new(move |result| {
                        Self::finish_step(reply, result.map(|()| committed.outcome).map_err(Failure));
                    }));
                }
                Ok(_) => Self::finish_step(reply, Err(Failure("The committed operation does not own this cleanup window.".into()))),
                Err(error) => Self::finish_step(reply, Err(error)),
            }
        }));
    }

    fn carry_step(self: &Arc<Self>, authority: GenerateAuthority, step: WindowStep, reply: Option<Responder>) {
        match step {
            WindowStep::Done(result) => Self::finish_step(reply, result),
            WindowStep::Need(need, run) => {
                if let Err(error) = self.check_authority(&authority) {
                    self.window().discard_run(run);
                    return Self::finish_step(reply, Err(Failure(error)));
                }
                match need {
                    WindowNeed::OpenTarget { handoff, opener, token, window } => {
                        let inner = self.clone();
                        let resumed = authority.clone();
                        self.shell.open_target_tab(&authority, handoff, opener,
                            host::GenerateOpenRequest { token, window: WindowId(window as u32) },
                            Box::new(move |result| inner.complete_open(resumed, run, result, reply)));
                    }
                    WindowNeed::OpenConverted { document, opener, token, window } => {
                        let inner = self.clone();
                        let resumed = authority.clone();
                        self.shell.open_converted_tab(&authority, document, opener,
                            host::GenerateOpenRequest { token, window: WindowId(window as u32) },
                            Box::new(move |result| inner.complete_open(resumed, run, result, reply)));
                    }
                    need => {
                        let inner = self.clone();
                        let resume_authority = authority.clone();
                        let resume = move |answer| inner.resume_step(resume_authority, run, answer, reply);
                        match need {
                            WindowNeed::Save(need) => self.shell.save(&authority, need,
                                Box::new(move |answer| resume(SessionAnswer::Save(answer)))),
                            WindowNeed::Sleep(ms) => self.shell.set_timeout(&authority, ms,
                                Box::new(move || resume(SessionAnswer::Slept))),
                            WindowNeed::Reveal(tab_id) => self.shell.reveal_tab(&authority, tab_id,
                                Box::new(move |result| resume(SessionAnswer::Revealed(result.map_err(Failure))))),
                            need @ (WindowNeed::Inspect(_) | WindowNeed::Notice(_) | WindowNeed::Convert(_) | WindowNeed::OpenDialog { .. }) => {
                                let inner = self.clone();
                                self.services.workers.spawn(Box::new(move || {
                                    let answer = match need {
                                        WindowNeed::Inspect(job) => SessionAnswer::Inspected(bridge::inspect_job(&inner, &authority, job)),
                                        WindowNeed::Notice(job) => SessionAnswer::Noticed(bridge::notice_job(&inner, &authority, job)),
                                        WindowNeed::Convert(job) => SessionAnswer::Converted(bridge::convert_job(&inner, &authority, job)),
                                        WindowNeed::OpenDialog { window, options } => {
                                            let result = inner.check_authority(&authority).map_err(Failure).and_then(|()| {
                                                inner.shell.show_open_dialog(&authority, inner.dialogs.clone(), WindowId(window as u32), &file_dialog(&options))
                                                    .map(dialog_result).map_err(Failure)
                                            });
                                            SessionAnswer::Dialog(result)
                                        }
                                        _ => unreachable!("non-worker generate need"),
                                    };
                                    resume(answer);
                                }));
                            }
                            WindowNeed::OpenTarget { .. } | WindowNeed::OpenConverted { .. } => unreachable!("terminal generate need"),
                        }
                    }
                }
            }
        }
    }

    /// `openGenerateWindowForDocument(request)`
    fn open_for_document(self: &Arc<Self>, request: GenerateWindowRequest<WindowId>, authority: GenerateAuthority) {
        self.snapshot(authority, move |inner, authority, snap| {
            let snap = match snap {
                Ok(snap) => snap,
                Err(error) => {
                    eprintln!("[generate] cannot list open documents: {error}");
                    return;
                }
            };
            let parent = request.parent.map(module_window);
            let result = inner.run(authority, |w, b| {
                w.open_generate_window_for_document(
                    b,
                    snap,
                    request.web_contents_id,
                    request.file_path.as_deref(),
                    &request.title,
                    parent,
                )
            });
            if let Err(Failure(message)) = result.result {
                eprintln!("[generate] the make window did not open: {message}");
            }
            inner.carry_step(authority.clone(), result.background, None);
        });
    }
}

/// The open files the module asks about: the open documents' paths and flags
fn open_files_of(tabs: &[OpenDocumentTab]) -> OpenFilesHolder {
    let files: Vec<OpenFileState> = tabs
        .iter()
        .map(|tab| OpenFileState {
            file_path: tab.file_path.clone(),
            dirty: tab.dirty,
        })
        .collect();
    let mut holder = OpenFilesHolder::new();
    holder.set_open_files_source(Some(Box::new(
        move || -> Result<Vec<OpenFileState>, OpenFilesSourceFailed> { Ok(files.clone()) },
    )));
    holder
}

/// `undefined` when it went well, else a throw
fn unit(result: Result<(), Failure>) -> Result<IpcValue, IpcError> {
    match result {
        Ok(()) => Ok(IpcValue::Undefined),
        Err(Failure(message)) => Err(IpcError {
            name: Some("Error".into()),
            message,
        }),
    }
}

/// An IPC argument as the module takes it (`None` for `undefined`)
fn json_of(value: &IpcValue) -> Option<Value> {
    Some(match value {
        IpcValue::Undefined | IpcValue::Bytes(..) | IpcValue::Date(_) => return None,
        IpcValue::Null => Value::Null,
        IpcValue::Bool(b) => Value::Bool(*b),
        IpcValue::Number(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
        IpcValue::String(s) => Value::String(s.clone()),
        IpcValue::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| json_of(item).unwrap_or(Value::Null))
                .collect(),
        ),
        IpcValue::Object(entries) => Value::Object(
            entries
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), json_of(v)?)))
                .collect(),
        ),
    })
}

fn jarg(args: &[IpcValue], index: usize) -> Option<Value> {
    args.get(index).and_then(json_of)
}

/// What the module answered, as the renderer receives it
pub fn ipc_of(value: &Value) -> IpcValue {
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(b) => IpcValue::Bool(*b),
        Value::Number(n) => IpcValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => IpcValue::String(s.clone()),
        Value::Array(items) => IpcValue::Array(items.iter().map(ipc_of).collect()),
        Value::Object(map) => {
            IpcValue::Object(map.iter().map(|(k, v)| (k.clone(), ipc_of(v))).collect())
        }
    }
}

/// `event.sender.id`
fn sender_of(cx: &CallContext) -> f64 {
    f64::from(cx.sender.view.0)
}

pub struct GenerateWindowPort {
    inner: Arc<Inner>,
    store: Rc<RefCell<HandoffStore>>,
}

impl GenerateWindowPort {
    pub fn new(
        services: &Services,
        shell: GenerateShellHost,
        dialogs: DialogsHost,
        os_shell: OsShellHost,
        main_window: MainWindowHost,
        store: Rc<RefCell<HandoffStore>>,
    ) -> Self {
        let mut module = WindowModule::new();
        // `configureGenerateWindow(...)`: the handlers are the port's, so only the first call's answer is dropped
        let _ = module.configure();
        let ids = shell.clone();
        // `setGenerateTabIdResolver((id) => next.tabIdFor(id))`
        store
            .borrow_mut()
            .set_generate_tab_id_resolver(Some(Box::new(move |id| Ok(ids.tab_id_for(id)))));
        GenerateWindowPort {
            inner: Arc::new(Inner {
                shell,
                services: services.clone(),
                dialogs,
                os_shell,
                main_window,
                window: Mutex::new(module),
            }),
            store,
        }
    }

    /// The one hand-off store: what "make" queues for the new tab, which its
    /// editor takes once. The shell gives it to the menus and the tab manager.
    pub fn handoff_store(&self) -> Rc<RefCell<HandoffStore>> {
        self.store.clone()
    }

    /// `openGenerateWindowForDocument`: what an editor's File menu calls
    /// ("make another document from this one"). The window opens once the
    /// open tabs are known, so a caller must not hold a borrow of the store
    /// across it.
    pub fn make_window_opener(&self) -> Box<dyn Fn(GenerateWindowRequest<WindowId>, GenerateAuthority)> {
        let inner = self.inner.clone();
        Box::new(move |request, authority| inner.open_for_document(request, authority))
    }

    fn authority(&self, cx: &CallContext) -> Result<GenerateAuthority, IpcError> {
        let authority = GenerateAuthority::renderer(cx);
        self.inner.check_authority(&authority).map_err(|message| IpcError { name: Some("Error".into()), message })?;
        Ok(authority)
    }

    /// `generate:open-with-paths` and `generate:open-with-group` take the
    /// open tabs first, then the module checks the sender
    fn open_from_home(
        &self,
        cx: &CallContext,
        reply: Responder,
        open: impl FnOnce(
            &mut WindowModule,
            &mut Bridge<'_>,
            &OpenSnapshot<'_>,
            f64,
        ) -> WindowOpen
        + Send
        + 'static,
    ) {
        let authority = match self.authority(cx) {
            Ok(authority) => authority,
            Err(error) => return reply.reply(Err(error)),
        };
        let sender = sender_of(cx);
        self.inner.snapshot(authority, move |inner, authority, snap| {
            let snap = match snap {
                Ok(snap) => snap,
                Err(error) => return reply.reply(unit(Err(Failure(error)))),
            };
            let result = inner.run(authority, |w, b| open(w, b, snap, sender));
            reply.reply(unit(result.result));
            inner.carry_step(authority.clone(), result.background, None);
        });
    }
}

impl GenerateWindowChannels for GenerateWindowPort {
    fn generate_add_materials(&self, cx: &CallContext, _args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        self.inner.snapshot(authority, move |inner, authority, snap| match snap {
            Ok(snap) => {
                let step = inner.run(authority, |w, b| w.handle_add_materials(b, snap, sender));
                inner.carry_step(authority.clone(), step, Some(reply));
            }
            Err(error) => reply.reply(unit(Err(Failure(error)))),
        });
    }

    fn generate_cancel(&self, cx: &CallContext, _args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let result = self.inner.run(&authority, |w, b| w.handle_cancel(b, sender));
        reply.reply(unit(result))
    }

    fn generate_get_state(&self, cx: &CallContext, _args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let state = self.inner.run(&authority, |w, b| w.handle_get_state(b, sender));
        reply.reply(state.map(|state| match state {
            Some(state) => serde_json::to_value(&state).map_or(IpcValue::Null, |v| ipc_of(&v)),
            None => IpcValue::Null,
        }).map_err(|Failure(message)| IpcError { name: Some("Error".into()), message }))
    }

    fn generate_get_theme(&self, cx: &CallContext, _args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let theme = self.inner.run(&authority, |w, b| w.handle_get_theme(&*b, sender));
        reply.reply(theme.map(IpcValue::String).map_err(|Failure(message)| IpcError { name: Some("Error".into()), message }))
    }

    fn generate_make(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let text = jarg(&args, 0);
        self.inner.snapshot(authority, move |inner, authority, snap| {
            let snap = match snap {
                Ok(snap) => snap,
                Err(error) => return reply.reply(unit(Err(Failure(error)))),
            };
            let step = inner.run(authority, |w, b| w.handle_make(b, snap, sender, text.as_ref()));
            inner.carry_step(authority.clone(), step, Some(reply));
        });
    }

    fn generate_open_with_group(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let group = jarg(&args, 0);
        self.open_from_home(cx, reply, move |w, b, snap, sender| {
            w.handle_open_with_group(b, snap, sender, group.as_ref())
        })
    }

    fn generate_open_with_paths(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let paths = jarg(&args, 0);
        self.open_from_home(cx, reply, move |w, b, snap, sender| {
            w.handle_open_with_paths(b, snap, sender, paths.as_ref())
        })
    }

    fn generate_refresh(&self, cx: &CallContext, _args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        self.inner.snapshot(authority, move |inner, authority, snap| {
            let snap = match snap {
                Ok(snap) => snap,
                Err(error) => return reply.reply(unit(Err(Failure(error)))),
            };
            let step = inner.run(authority, |w, b| w.handle_refresh(b, snap, sender));
            inner.carry_step(authority.clone(), step, Some(reply));
        });
    }

    fn generate_remove_material(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let key = jarg(&args, 0);
        let result = self.inner
            .run(&authority, |w, b| w.handle_remove_material(b, sender, key.as_ref()));
        reply.reply(unit(result))
    }

    fn generate_save_material(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let key = jarg(&args, 0);
        self.inner.snapshot(authority, move |inner, authority, snap| {
            let snap = match snap {
                Ok(snap) => snap,
                Err(error) => return reply.reply(unit(Err(Failure(error)))),
            };
            let step = inner.run(authority, |w, b| w.handle_save_material(b, snap, sender, key.as_ref()));
            inner.carry_step(authority.clone(), step, Some(reply));
        });
    }

    fn generate_set_convert(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let kind = jarg(&args, 0);
        let result = self.inner
            .run(&authority, |w, b| w.handle_set_convert(b, sender, kind.as_ref()));
        reply.reply(unit(result))
    }

    fn generate_set_kind(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder) {
        let authority = match self.authority(cx) { Ok(authority) => authority, Err(error) => return reply.reply(Err(error)) };
        let sender = sender_of(cx);
        let kind = jarg(&args, 0);
        let result = self.inner
            .run(&authority, |w, b| w.handle_set_kind(b, sender, kind.as_ref()));
        reply.reply(unit(result))
    }
}

/// The host the shell inserts for this port. Building it takes the shared
/// hand-off store and adds the port to what the batches provide.
pub fn factory() -> crate::Factory<dyn GenerateWindowChannels> {
    crate::Factory::new(|services, provided| {
        let batch = shell_contract::Batch::A;
        let hosts = &services.hosts;
        let port = Rc::new(GenerateWindowPort::new(
            services,
            hosts.need::<GenerateShellHost>(batch)?,
            hosts.need::<DialogsHost>(batch)?,
            hosts.need::<OsShellHost>(batch)?,
            hosts.need::<MainWindowHost>(batch)?,
            crate::user_skills::shared_handoff_store(provided),
        ));
        provided.add::<GenerateWindowPort>(port.clone());
        let port: Rc<dyn GenerateWindowChannels> = port;
        Ok(port)
    })
}
