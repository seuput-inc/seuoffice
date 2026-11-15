//! Fakes for the batch's tests and for `ipc-probe`: a registry that applies
//! the shell's start rules, the contract's hosts recording what handlers
//! asked of them, and a call helper that waits for the reply.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use main_core::packages::i18n::Lang;
use shell_contract::hosts::{
    Clipboard, Contents, Dialogs, FileDialog, Hosts, MainWindow, MessageBox, MessageBoxAnswer,
    OsShell,
};
use shell_contract::ipc::{
    CallContext, ChannelKind, EventSink, Handler, RegisterError, Registry, Responder, Sender,
    Target,
};
use shell_contract::provided::Provided;
use shell_contract::runtime::{AppPaths, CancelToken, PathName, Services, Thread, Views, Workers};
use shell_contract::{
    Batch, CHANNELS, ClipboardHost, ContentsHost, DialogsHost, IpcError, IpcValue, MainWindowHost,
    OsShellHost, Surface, ViewId, WindowId,
};

use main_core::packages::electron_utils::default_save_dir::SaveDirState;
use main_core::packages::electron_utils::dialog_memory::DialogMemory;

use crate::host::{Locale, LocaleHost, RecoveryStateHost};

/// The shell's start rules (contract section 5), with the handlers kept
#[derive(Default)]
pub struct TestRegistry {
    pub handlers: BTreeMap<&'static str, (ChannelKind, Rc<dyn Handler>)>,
    /// what the batch added for the other batches (its document editors)
    pub provided: Rc<Provided>,
    views: Option<Arc<FakeViews>>,
}

impl Registry for TestRegistry {
    fn register(
        &mut self,
        by: Batch,
        channel: &'static str,
        kind: ChannelKind,
        handler: Rc<dyn Handler>,
    ) -> Result<(), RegisterError> {
        let spec = CHANNELS
            .iter()
            .find(|spec| spec.name == channel)
            .ok_or(RegisterError::Unknown(channel))?;
        if spec.batch != by {
            return Err(RegisterError::WrongBatch {
                channel,
                owner: spec.batch,
                by,
            });
        }
        if !spec.kinds.contains(&kind) || kind == ChannelKind::Event {
            return Err(RegisterError::WrongKind { channel, kind });
        }
        if self.handlers.contains_key(channel) {
            return Err(RegisterError::Twice(channel));
        }
        self.handlers.insert(channel, (kind, handler));
        Ok(())
    }

    fn provided(&self) -> Rc<Provided> {
        self.provided.clone()
    }
}

/// A thread that runs a job at once on the caller's thread
pub struct Inline {
    pub current: bool,
}

impl Thread for Inline {
    fn post(&self, job: Box<dyn FnOnce() + Send>) {
        job()
    }

    fn is_current(&self) -> bool {
        self.current
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Workers that run a job at once, or keep it while [`Kit::hold_workers`]
/// holds them, so a test can let other calls happen before it finishes
#[derive(Default)]
pub struct HeldWorkers {
    held: Mutex<Option<Vec<Job>>>,
}

impl Workers for HeldWorkers {
    fn spawn(&self, job: Job) {
        let mut held = self.held.lock().unwrap();
        match held.as_mut() {
            Some(jobs) => jobs.push(job),
            None => {
                drop(held);
                job()
            }
        }
    }
}

/// Every emitted event, in order
pub struct Events(
    pub Mutex<Vec<(Target, &'static str, Vec<IpcValue>)>>,
    Arc<FakeViews>,
);

impl EventSink for Events {
    fn emit(&self, target: Target, channel: &'static str, args: Vec<IpcValue>) {
        self.0.lock().unwrap().push((target, channel, args));
    }

    fn emit_to_owner_checked(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        channel: &'static str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let generation = owner
            .context
            .renderer()
            .filter(|generation| generation.is_active())
            .ok_or_else(|| "renderer generation unavailable".to_owned())?;
        if self.1.sender_of(owner.view).is_none()
            || !self
                .1
                .renderer_generation(owner.view)
                .is_some_and(|current| current.is_active() && current.same_generation(generation))
        {
            return Err("renderer generation does not own the view".into());
        }
        self.emit(Target::View(owner.view), channel, args);
        Ok(())
    }
}

#[derive(Default)]
pub struct FakeNativeRetirements {
    listeners: Mutex<
        Vec<(
            Arc<std::sync::atomic::AtomicBool>,
            Arc<
                dyn Fn(shell_contract::runtime::NativeViewTerminal) -> Result<(), String>
                    + Send
                    + Sync,
            >,
        )>,
    >,
    pending: Mutex<VecDeque<shell_contract::runtime::NativeViewTerminal>>,
}
impl FakeNativeRetirements {
    pub fn emit(&self, event: shell_contract::runtime::NativeViewTerminal) {
        self.pending.lock().unwrap().push_back(event);
    }
}
impl shell_contract::runtime::NativeViewRetirements for FakeNativeRetirements {
    fn on_native_terminal(
        &self,
        listener: Box<
            dyn Fn(shell_contract::runtime::NativeViewTerminal) -> Result<(), String> + Send + Sync,
        >,
    ) -> shell_contract::runtime::NativeViewRetirementSubscription {
        let subscription = shell_contract::runtime::NativeViewRetirementSubscription::new();
        self.listeners
            .lock()
            .unwrap()
            .push((subscription.state(), Arc::from(listener)));
        subscription
    }
    fn authenticated_browser_owner(
        &self,
        _: &shell_contract::ipc::NativeRequestOwner,
    ) -> Option<shell_contract::ipc::NativeBrowserOwner> {
        None
    }
    fn drain_native_terminals(&self) -> Result<usize, String> {
        let mut count = 0;
        loop {
            let Some(event) = self
                .pending
                .lock()
                .map_err(|_| "test terminal queue poisoned")?
                .pop_front()
            else {
                break;
            };
            let listeners = self
                .listeners
                .lock()
                .map_err(|_| "test terminal listeners poisoned")?
                .clone();
            for (active, listener) in listeners {
                if active.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(error) = listener(event.clone()) {
                        self.pending
                            .lock()
                            .map_err(|_| "test terminal queue poisoned")?
                            .push_front(event);
                        return Err(error);
                    }
                }
            }
            count += 1;
        }
        Ok(count)
    }
    fn pending_native_terminals(&self) -> Result<usize, String> {
        self.pending
            .lock()
            .map(|pending| pending.len())
            .map_err(|_| "test terminal queue poisoned".into())
    }
    fn wake_native_terminals(&self) {
        let _ = self.drain_native_terminals();
    }
}

/// The views a case declares; [`Kit::destroy`] runs the listeners
#[derive(Default)]
pub struct FakeViews {
    pub views: Mutex<Vec<Sender>>,
    #[allow(clippy::type_complexity)]
    pub listeners: Mutex<Vec<Box<dyn Fn(ViewId) + Send + Sync>>>,
    generations:
        Mutex<std::collections::HashMap<ViewId, shell_contract::runtime::RendererGeneration>>,
    replacements:
        Mutex<Vec<Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>>>,
}

impl Views for FakeViews {
    fn sender_of(&self, view: ViewId) -> Option<Sender> {
        self.views
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.view == view)
            .copied()
    }

    fn all(&self) -> Vec<Sender> {
        self.views.lock().unwrap().clone()
    }

    fn on_destroyed(&self, listener: Box<dyn Fn(ViewId) + Send + Sync>) {
        self.listeners.lock().unwrap().push(listener);
    }

    fn renderer_generation(
        &self,
        view: ViewId,
    ) -> Option<shell_contract::runtime::RendererGeneration> {
        self.generations.lock().unwrap().get(&view).cloned()
    }

    fn on_renderer_replaced(
        &self,
        listener: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
    ) {
        self.replacements.lock().unwrap().push(listener);
    }
}

/// `app.getPath` under one folder: `<root>/<name>`
pub struct FolderPaths(pub PathBuf);

impl AppPaths for FolderPaths {
    fn path(&self, name: PathName) -> PathBuf {
        self.0.join(format!("{name:?}").to_lowercase())
    }
}

/// What a fake host was asked, in order, as text
pub type Log = Arc<Mutex<Vec<String>>>;

/// Dialogs that answer from queues the case fills; an empty queue cancels
#[derive(Default)]
pub struct FakeDialogs {
    pub log: Log,
    pub opens: Mutex<VecDeque<Option<Vec<PathBuf>>>>,
    pub saves: Mutex<VecDeque<Option<PathBuf>>>,
    pub messages: Mutex<VecDeque<usize>>,
}

fn synthetic_owned_dialog_admit(
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<(), String> {
    let source = origin
        .context
        .renderer()
        .ok_or("synthetic dialog source is unbound")?;
    let target = destination
        .context
        .renderer()
        .ok_or("synthetic dialog destination is unbound")?;
    let source_loss = origin.loss_token();
    let target_loss = destination.loss_token();
    source
        .with_active_pair(target, || {
            shell_contract::runtime::CancelToken::with_active_all(
                [
                    Some(cancel),
                    Some(pending),
                    source_loss.as_ref(),
                    target_loss.as_ref(),
                ],
                || (),
            )
        })
        .flatten()
        .ok_or_else(|| "Renderer document is no longer available.".into())
}

impl Dialogs for FakeDialogs {
    shell_contract::synthetic_recovery_dialog_methods!();
    fn message_prepared(
        &self,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::MessageBox,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::recovery_dialog(
            &target,
            &snapshot,
            || self.message(parent, &options),
        ));
    }
    fn save_prepared(
        &self,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::FileDialog,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::RecoveryPreparedDestination>, String>)
                + Send,
        >,
    ) {
        done(shell_contract::hosts::synthetic::recovery_save(
            &target,
            &snapshot,
            || self.save(parent, &options),
        ));
    }
    fn open_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<Vec<std::path::PathBuf>>, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.open(parent, &options)),
        );
    }
    fn save_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<std::path::PathBuf>, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.save(parent, &options)),
        );
    }
    fn message_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.message(parent, &options)),
        );
    }
    fn save_owned_many(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<std::path::PathBuf>, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::admit_many(
            &origin,
            &participants,
            &cancel,
            &pending,
            || self.save(parent, &options),
        ));
    }
    fn message_owned_many(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::admit_many(
            &origin,
            &participants,
            &cancel,
            &pending,
            || self.message(parent, &options),
        ));
    }
    fn open(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<Vec<PathBuf>> {
        self.log
            .lock()
            .unwrap()
            .push(format!("open {parent:?} {options:?}"));
        self.opens.lock().unwrap().pop_front().flatten()
    }

    fn save(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<PathBuf> {
        self.log
            .lock()
            .unwrap()
            .push(format!("save {parent:?} {options:?}"));
        self.saves.lock().unwrap().pop_front().flatten()
    }

    fn message(&self, parent: Option<WindowId>, options: &MessageBox) -> MessageBoxAnswer {
        self.log
            .lock()
            .unwrap()
            .push(format!("message {parent:?} {options:?}"));
        let response = self
            .messages
            .lock()
            .unwrap()
            .pop_front()
            .or(options.cancel_id)
            .unwrap_or(0);
        MessageBoxAnswer {
            response,
            checkbox_checked: false,
        }
    }

    fn error(&self, title: &str, content: &str) {
        self.log
            .lock()
            .unwrap()
            .push(format!("error {title:?} {content:?}"));
    }
}

/// `shell.*` that records and succeeds
#[derive(Default)]
pub struct FakeOsShell {
    pub log: Log,
}

impl OsShell for FakeOsShell {
    fn open_external(&self, url: &str) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("openExternal {url}"));
        Ok(())
    }

    fn open_path(&self, path: &Path) -> String {
        self.log
            .lock()
            .unwrap()
            .push(format!("openPath {}", path.display()));
        String::new()
    }

    fn show_item_in_folder(&self, path: &Path) {
        self.log
            .lock()
            .unwrap()
            .push(format!("showItemInFolder {}", path.display()));
    }

    fn trash_item(&self, path: &Path) -> Result<(), String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("trashItem {}", path.display()));
        Ok(())
    }
    fn trash_item_authorized(
        &self,
        path: &std::path::Path,
        admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String> {
        admit(&mut || self.trash_item(path))
    }
}

/// A clipboard held in memory
#[derive(Default)]
pub struct FakeClipboard {
    pub text: Mutex<String>,
    pub html: Mutex<String>,
    pub png: Mutex<Vec<u8>>,
}

impl Clipboard for FakeClipboard {
    fn read_text(&self) -> String {
        self.text.lock().unwrap().clone()
    }

    fn write_text(&self, text: &str) {
        *self.text.lock().unwrap() = text.to_owned();
    }

    fn read_html(&self) -> String {
        self.html.lock().unwrap().clone()
    }

    fn write_html(&self, html: &str, text: Option<&str>) {
        *self.html.lock().unwrap() = html.to_owned();
        if let Some(text) = text {
            *self.text.lock().unwrap() = text.to_owned();
        }
    }

    fn read_image_png(&self) -> Vec<u8> {
        self.png.lock().unwrap().clone()
    }

    fn write_image_png(&self, png: &[u8]) {
        *self.png.lock().unwrap() = png.to_vec();
    }

    fn available_formats(&self) -> Vec<String> {
        let mut formats = Vec::new();
        if !self.text.lock().unwrap().is_empty() {
            formats.push("text/plain".to_owned());
        }
        if !self.html.lock().unwrap().is_empty() {
            formats.push("text/html".to_owned());
        }
        if !self.png.lock().unwrap().is_empty() {
            formats.push("image/png".to_owned());
        }
        formats
    }
}

/// Web contents: every view sits in the window the case declared for it
pub struct FakeContents {
    pub log: Log,
    pub views: Arc<FakeViews>,
}

impl Contents for FakeContents {
    fn window_of(&self, view: ViewId) -> Option<WindowId> {
        self.views.sender_of(view).map(|s| s.window)
    }

    fn capture_png(&self, view: ViewId) -> Result<Vec<u8>, String> {
        self.log.lock().unwrap().push(format!("capture {view:?}"));
        Ok(b"\x89PNG".to_vec())
    }

    fn print_to_pdf_owned(
        &self,
        _: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return Err("Print request expired.".into());
        }
        self.print_to_pdf(destination.view, options)
    }
    fn print_to_pdf_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let extra = additional.iter().collect::<Vec<_>>();
        let mut admitted = false;
        shell_contract::hosts::synthetic::with_existing_document_save_extra(
            binding,
            participants,
            cancel,
            pending,
            &extra,
            &mut || {
                admitted = true;
                Ok(())
            },
        )?;
        if !admitted {
            return Err("Print request expired.".into());
        }
        self.print_to_pdf(binding.owner().view, options)
    }
    fn print_to_pdf_owned_many_extra(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        shell_contract::hosts::synthetic::with_owned_effect_many_extra(
            origin,
            &owners,
            cancel,
            pending,
            &extra,
            &mut || Ok(()),
        )?;
        self.print_to_pdf(destination.view, options)
    }
    fn print_to_pdf_native_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let mut options = Some(options);
        let mut admitted = None;
        shell_contract::hosts::synthetic::with_native_invocation(
            document,
            owner,
            participants,
            cancel,
            pending,
            &mut || {
                admitted = options.take();
                Ok(())
            },
        )?;
        self.print_to_pdf(owner.view, admitted.ok_or("Print request expired.")?)
    }
    fn print_to_pdf(&self, view: ViewId, options: IpcValue) -> Result<Vec<u8>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("printToPDF {view:?} {options:?}"));
        Ok(b"%PDF-1.7".to_vec())
    }

    fn print(&self, view: ViewId, options: IpcValue) -> Result<bool, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("print {view:?} {options:?}"));
        Ok(true)
    }

    fn zoom_factor(&self, _view: ViewId) -> f64 {
        1.0
    }

    fn set_zoom_factor(&self, view: ViewId, factor: f64) {
        self.log
            .lock()
            .unwrap()
            .push(format!("zoom {view:?} {factor}"));
    }

    fn focus(&self, view: ViewId) {
        self.log.lock().unwrap().push(format!("focus {view:?}"));
    }

    fn close_window(&self, window: WindowId) {
        self.log.lock().unwrap().push(format!("close {window:?}"));
    }
}

/// No shell window: the apps run as tabs of nothing
pub struct FakeMainWindow(pub Option<(ViewId, WindowId)>);

impl MainWindow for FakeMainWindow {
    fn view(&self) -> Option<ViewId> {
        self.0.map(|(view, _)| view)
    }

    fn window(&self) -> Option<WindowId> {
        self.0.map(|(_, window)| window)
    }
}

pub struct FixedLocale(pub Lang);

impl Locale for FixedLocale {
    fn ui_lang(&self) -> Lang {
        self.0
    }
}

/// One test's services and the fakes behind them
pub struct Kit {
    pub services: Services,
    pub events: Arc<Events>,
    pub views: Arc<FakeViews>,
    pub retirements: Arc<FakeNativeRetirements>,
    pub dialogs: Arc<FakeDialogs>,
    pub os_shell: Arc<FakeOsShell>,
    pub clipboard: Arc<FakeClipboard>,
    pub log: Log,
    pub workers: Arc<HeldWorkers>,
    pub root: PathBuf,
}

impl Kit {
    /// The contract's hosts as fakes, `app.getPath` under `root`, English,
    /// plus whatever `more` inserts (an app's own hosts)
    pub fn new(root: &Path, more: impl FnOnce(&mut Hosts)) -> Kit {
        let log: Log = Arc::default();
        let views = Arc::new(FakeViews::default());
        let retirements = Arc::new(FakeNativeRetirements::default());
        let events = Arc::new(Events(Mutex::default(), views.clone()));
        let dialogs = Arc::new(FakeDialogs {
            log: log.clone(),
            ..FakeDialogs::default()
        });
        let os_shell = Arc::new(FakeOsShell { log: log.clone() });
        let clipboard = Arc::new(FakeClipboard::default());
        let mut hosts = Hosts::default();
        hosts.insert::<shell_contract::runtime::NativeViewRetirementsHost>(retirements.clone());
        hosts.insert::<DialogsHost>(dialogs.clone());
        hosts.insert::<OsShellHost>(os_shell.clone());
        hosts.insert::<ClipboardHost>(clipboard.clone());
        hosts.insert::<ContentsHost>(Arc::new(FakeContents {
            log: log.clone(),
            views: views.clone(),
        }));
        hosts.insert::<MainWindowHost>(Arc::new(FakeMainWindow(None)));
        hosts.insert::<LocaleHost>(Arc::new(FixedLocale(Lang::En)));
        // the shell's one dialog memory and default-save-folder state
        hosts.insert::<Arc<Mutex<DialogMemory>>>(Arc::default());
        hosts.insert::<Arc<Mutex<SaveDirState>>>(Arc::default());
        hosts.insert::<RecoveryStateHost>(Arc::new(Mutex::new(
            main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                "0123456789abcdef",
            ),
        )));
        more(&mut hosts);
        let workers = Arc::new(HeldWorkers::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: Arc::new(Inline { current: false }),
            core: Arc::new(Inline { current: true }),
            workers: workers.clone(),
            events: events.clone(),
            views: views.clone(),
            paths: Arc::new(FolderPaths(root.to_path_buf())),
            hosts: Arc::new(hosts),
        });
        Kit {
            services,
            events,
            views,
            retirements,
            dialogs,
            os_shell,
            clipboard,
            log,
            workers,
            root: root.to_path_buf(),
        }
    }

    /// A host the kit or `more` inserted, to script or inspect it
    pub fn host<T: Any + Send + Sync + Clone>(&self) -> T {
        self.services
            .hosts
            .get::<T>()
            .cloned()
            .expect("the kit has this host")
    }

    /// Declares view `id` in window `id` with `surface`
    pub fn view(&self, id: u32, surface: Surface) -> Sender {
        let sender = Sender {
            view: ViewId(id),
            window: WindowId(id),
            surface,
        };
        {
            let mut views = self.views.views.lock().unwrap();
            views.retain(|existing| existing.view != sender.view);
            views.push(sender);
        }
        let old = self.views.generations.lock().unwrap().insert(
            sender.view,
            shell_contract::runtime::RendererGeneration::new(),
        );
        if let Some(old) = old {
            old.revoke();
            for listener in self.views.replacements.lock().unwrap().iter() {
                listener(sender.view, old.clone());
            }
        }
        sender
    }

    /// The view is gone: its listeners run, as the shell runs them
    pub fn destroy(&self, view: ViewId) {
        self.views.views.lock().unwrap().retain(|s| s.view != view);
        if let Some(generation) = self.views.generations.lock().unwrap().remove(&view) {
            generation.revoke();
        }
        for listener in self.views.listeners.lock().unwrap().iter() {
            listener(view);
        }
    }

    /// Takes the events emitted so far
    pub fn take_events(&self) -> Vec<(Target, &'static str, Vec<IpcValue>)> {
        std::mem::take(&mut *self.events.0.lock().unwrap())
    }

    /// Takes what the fake hosts were asked so far
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    /// From now on a worker's job waits for [`Kit::release_workers`]
    pub fn hold_workers(&self) {
        *self.workers.held.lock().unwrap() = Some(Vec::new());
    }

    /// Runs the held jobs in the order they were spawned; workers run at
    /// once again
    pub fn release_workers(&self) {
        let jobs = self.workers.held.lock().unwrap().take().unwrap_or_default();
        for job in jobs {
            job()
        }
    }
}

/// Calls `channel` as `sender` would; the reply arrives on the receiver,
/// at once or after the held workers ran
pub fn start(
    registry: &TestRegistry,
    sender: Sender,
    channel: &str,
    args: Vec<IpcValue>,
) -> mpsc::Receiver<Result<IpcValue, IpcError>> {
    let cx = CallContext {
        sender,
        request: None,
        cancel: CancelToken::default(),
        native: if sender.surface == Surface::Slides {
            registry
                .views
                .as_ref()
                .and_then(|views| views.renderer_generation(sender.view))
                .map(|generation| shell_contract::ipc::NativeContext::with_renderer((), generation))
        } else {
            None
        },
    };
    start_with_context(registry, cx, channel, args)
}

/// Invoke with an explicitly captured synthetic origin, never native attestation.
pub fn start_with_owner(
    registry: &TestRegistry,
    sender: Sender,
    channel: &str,
    args: Vec<IpcValue>,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: CancelToken,
) -> mpsc::Receiver<Result<IpcValue, IpcError>> {
    assert_eq!(sender.view, owner.view, "synthetic sender and owner differ");
    assert!(
        owner
            .context
            .authority_ref::<shell_contract::hosts::synthetic::SyntheticNativeOrigin>()
            .is_some(),
        "an explicitly synthetic origin is required"
    );
    let cx = CallContext {
        sender,
        request: None,
        cancel,
        native: Some(owner.context.clone()),
    };
    start_with_context(registry, cx, channel, args)
}

fn start_with_context(
    registry: &TestRegistry,
    cx: CallContext,
    channel: &str,
    args: Vec<IpcValue>,
) -> mpsc::Receiver<Result<IpcValue, IpcError>> {
    let (_, handler) = registry
        .handlers
        .get(channel)
        .unwrap_or_else(|| panic!("no handler for {channel}"));
    let (tx, rx) = mpsc::channel();
    handler.call(
        &cx,
        args,
        Responder::new(move |reply| {
            let _ = tx.send(reply);
        }),
    );
    rx
}

/// Calls `channel` as `sender` would and returns its reply; panics when the
/// handler kept the responder without answering (workers run inline here)
pub fn call(
    registry: &TestRegistry,
    sender: Sender,
    channel: &str,
    args: Vec<IpcValue>,
) -> Result<IpcValue, IpcError> {
    let kind = registry
        .handlers
        .get(channel)
        .map(|(kind, _)| *kind)
        .unwrap_or_else(|| panic!("no handler for {channel}"));
    let reply = start(registry, sender, channel, args)
        .try_recv()
        .unwrap_or_else(|_| panic!("{channel} did not answer"));
    if kind == ChannelKind::Send {
        assert_eq!(reply.as_ref().ok(), Some(&IpcValue::Undefined));
    }
    reply
}

/// The registry after `register` ran over `kit`
pub fn registered(kit: &Kit) -> TestRegistry {
    let mut registry = TestRegistry::default();
    registry.views = Some(kit.views.clone());
    crate::register(&mut registry, &kit.services).expect("D1 registers");
    registry
}

thread_local! {
    static TEMP_SEQ: RefCell<u32> = const { RefCell::new(0) };
}

/// A fresh empty folder under the system temp folder
pub fn temp_dir(name: &str) -> PathBuf {
    let seq = TEMP_SEQ.with(|seq| {
        *seq.borrow_mut() += 1;
        *seq.borrow()
    });
    let dir = std::env::temp_dir().join(format!(
        "shell-ipc-d1-{name}-{}-{seq}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
