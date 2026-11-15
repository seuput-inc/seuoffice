//! The `diagram` channels of batch D1: `apps/diagram/src/main/`
//! (`diagram-main.ts`, `export-main.ts`, `headless-main.ts`, `mcp-queries.ts`,
//! `ai-attachments.ts`, `recovery-main.ts`).
//!
//! All state sits in one [`DiagramViews`] handle created by [`routes`] and
//! captured by every handler. The shell reaches it through [`views`] to do
//! what the TypeScript's exported functions did: [`DiagramViews::open_view`]
//! when it creates a tab (`grantAndTrack`), the close guard, the menu's save,
//! the in-app MCP server's questions. What the TypeScript awaited on a
//! renderer's reply is a continuation stored in the state and resumed by the
//! channel that carries the reply; waits that ended on a timer end on a timer
//! thread that posts back to the core thread.

mod attachments;
mod editors;
mod engine;
mod export;
#[cfg(any(test, feature = "probe"))]
mod fakes;
mod files;
mod formats;
mod headless;
mod lifecycle;
mod owned;
mod queries;
mod recovery;
mod recovery_open;
mod recovery_prepare;
mod saved_load;
mod shell;
mod state;
mod strings;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jscompat::random_uuid;
use main_core::packages::electron_utils::recovery_store::{
    RecoveryStore, RecoveryStoreOptions, create_recovery_store,
};
use main_core::packages::i18n::Lang;
use shell_contract::editors::DocumentEditors;
use shell_contract::hosts::{DialogsHost, OsShellHost};
use shell_contract::ipc::{RegisterError, Target};
use shell_contract::provided::Provided;
use shell_contract::runtime::{PathName, Services};
use shell_contract::{Batch, IpcValue, ViewId, WindowId};

use crate::host::{LocaleHost, RecoveryStateHost, Shared, path_string};
use crate::route::{self, Route};

pub use attachments::{
    ATTACHMENT_IMAGE_MAX_BYTES, ATTACHMENT_MAX_BYTES, ATTACHMENT_READ_MAX_CHARS,
};
pub use engine::{DiagramEngine, DiagramEngineHost, DiagramFile, OpenFailure, ParsedText};
pub use export::{looks_like as export_looks_like, print_pages_of};
pub use formats::diagram_base_name;
pub use formats::{Detected, DiagramFormat, detect_format, openable_link};
pub use lifecycle::{PreparedNativeDiagram, TabIdentity, prepare_hidden_open_path};
pub use recovery::DiagramRecoveryLookup;
pub use shell::{DiagramShell, DiagramShellHost};
pub use state::PageRead;
pub use strings::open_refusal;

use state::{State, WaitEnd};

/// Everything the handlers and the shell's calls share. Core thread only.
pub(crate) struct Inner {
    id: u64,
    services: Services,
    engine: DiagramEngineHost,
    shell: DiagramShellHost,
    dialogs: DialogsHost,
    os_shell: OsShellHost,
    locale: LocaleHost,
    shared: Arc<Shared>,
    recovery_state: RecoveryStateHost,
    state: RefCell<State>,
    text_cache: Arc<Mutex<attachments::TextCache>>,
    terminal_registration: RefCell<Option<crate::host::NativeTerminalRegistration>>,
}

struct DetachedDiagram {
    view: state::ViewState,
    recovery: Option<state::RecoveryView>,
    reads: Vec<state::ReadWaiter>,
    applies: Vec<state::ApplyWaiter>,
    chain: Option<std::collections::VecDeque<state::ApplyJob>>,
    headless: Option<state::HeadlessJob>,
}

/// The diagram editor's per-view state and what the shell asks of it. Cheap
/// to clone; use it on the core thread.
#[derive(Clone)]
pub struct DiagramViews(Rc<Inner>);

thread_local! {
    static INSTANCES: RefCell<HashMap<u64, Weak<Inner>>> = RefCell::new(HashMap::new());
    static LAST: Cell<u64> = const { Cell::new(0) };
}

/// The handle [`routes`] made last, for the shell that creates diagram tabs.
/// Core thread only; none before the batch registered.
pub fn views() -> Option<DiagramViews> {
    lookup(LAST.with(Cell::get))
}

fn lookup(id: u64) -> Option<DiagramViews> {
    INSTANCES
        .with(|all| all.borrow().get(&id).and_then(Weak::upgrade))
        .map(DiagramViews)
}

impl DiagramViews {
    fn new(services: &Services) -> Result<DiagramViews, RegisterError> {
        let hosts = &services.hosts;
        let id = LAST.with(|last| {
            last.set(last.get() + 1);
            last.get()
        });
        let inner = Rc::new(Inner {
            id,
            services: services.clone(),
            engine: hosts.need::<DiagramEngineHost>(Batch::D1)?,
            shell: hosts.need::<DiagramShellHost>(Batch::D1)?,
            dialogs: hosts.need::<DialogsHost>(Batch::D1)?,
            os_shell: hosts.need::<OsShellHost>(Batch::D1)?,
            locale: hosts.need::<LocaleHost>(Batch::D1)?,
            shared: Shared::need(hosts)?,
            recovery_state: hosts.need::<RecoveryStateHost>(Batch::D1)?,
            state: RefCell::new(State::default()),
            text_cache: Arc::default(),
            terminal_registration: RefCell::new(None),
        });
        INSTANCES.with(|all| all.borrow_mut().insert(id, Rc::downgrade(&inner)));
        Ok(DiagramViews(inner))
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.0.state.borrow_mut())
    }

    fn lang(&self) -> Lang {
        self.0.locale.ui_lang()
    }

    /// `tm(key)`
    fn tm(&self, key: &str) -> String {
        strings::tm(self.lang(), key)
    }

    /// `!contents.isDestroyed()`
    fn alive(&self, view: ViewId) -> bool {
        self.0.services.views.sender_of(view).is_some()
    }

    /// `contents.send(channel, ...args)`
    fn emit(&self, view: ViewId, channel: &'static str, args: Vec<IpcValue>) {
        self.0
            .services
            .events
            .emit(Target::View(view), channel, args);
    }

    /// `BrowserWindow.fromWebContents(contents)`
    fn window_of(&self, view: ViewId) -> Option<WindowId> {
        self.0.services.views.sender_of(view).map(|s| s.window)
    }

    fn user_data_dir(&self) -> String {
        path_string(self.0.services.paths.path(PathName::UserData))
    }

    /// Runs `job` on the core thread after `delay` (`setTimeout`). The timer
    /// is a thread that only sleeps and posts; `job` finds this handle again
    /// by its id, so nothing non-`Send` crosses.
    fn after(&self, delay: Duration, job: impl FnOnce(&DiagramViews) + Send + 'static) {
        let id = self.0.id;
        let core = self.0.services.core.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            core.post(Box::new(move || {
                if let Some(views) = lookup(id) {
                    job(&views);
                }
            }));
        });
    }

    /// The shared recovery store over this app's data folder
    /// (`diagramRecoveryStore()`)
    fn with_store<R>(&self, f: impl FnOnce(&RecoveryStore<'_>) -> R) -> R {
        let shared = self
            .0
            .recovery_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dir = self.user_data_dir();
        let user_data_dir = move || dir.clone();
        let now_ms = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_millis() as f64)
        };
        let uuid = random_uuid;
        let store = create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &user_data_dir,
                now_ms: &now_ms,
                random_uuid: &uuid,
            },
            &shared,
        );
        f(&store)
    }

    /// The views and the headless job a probe case starts with
    #[cfg(any(test, feature = "probe"))]
    fn apply_probe(&self, probe: &fakes::ProbeOpen) {
        for entry in &probe.open {
            self.open_view(entry.view, entry.path.as_deref());
            if let Some(decision) = entry.decision {
                self.set_view_open_decision(entry.view, decision);
            }
            if let Some(seed) = &entry.seed {
                self.queue_seed(entry.view, seed);
            }
        }
        if let Some((view, path)) = &probe.headless {
            self.open_hidden_view(*view, path);
            let job = crate::value::obj([
                ("kind", crate::value::string("export")),
                ("format", crate::value::string("png")),
            ]);
            self.run_headless_job(*view, job, Box::new(|_| true), Box::new(|_| {}));
        }
    }

    fn native_terminal(
        &self,
        event: shell_contract::runtime::NativeViewTerminal,
    ) -> Result<(), String> {
        let view = event.view;
        let owner = self.with_state(|state| {
            state
                .views
                .get(&view)
                .and_then(|entry| entry.browser_owner.clone())
        });
        let Some(owner) = owner else { return Ok(()) };
        if !owner.same_owner(&event.owner)
            || !owner.reservation().same_reservation(&event.reservation)
        {
            return Ok(());
        }
        let detached = event.receipt.with_terminal_owner(&owner, |transition| {
            if !transition.matches(&event.reservation, &owner) {
                return Err("The diagram terminal receipt does not match".into());
            }
            self.with_state(|state| {
                if !state
                    .views
                    .get(&view)
                    .and_then(|entry| entry.browser_owner.as_ref())
                    .is_some_and(|current| current.same_owner(&owner))
                {
                    return Ok(None);
                }
                let Some(actor) = state.views.remove(&view) else {
                    return Ok(None);
                };
                let reads = state
                    .reads
                    .extract_if(|_, waiter| waiter.view == view)
                    .map(|(_, waiter)| waiter)
                    .collect();
                let applies = state
                    .applies
                    .extract_if(|_, waiter| waiter.view == view)
                    .map(|(_, waiter)| waiter)
                    .collect();
                Ok(Some(DetachedDiagram {
                    view: actor,
                    recovery: state.recovery.remove(&view),
                    reads,
                    applies,
                    chain: state.apply_chains.remove(&view),
                    headless: state.headless.remove(&view),
                }))
            })
        });
        let Some(mut detached) = detached? else {
            return Ok(());
        };
        self.cancel_detached_recovery(&mut detached.view);
        let dirty = detached.view.dirty;
        self.finish_detached_recovery(detached.recovery.take(), dirty);
        for waiter in detached.reads {
            (waiter.done)(Err("the diagram is no longer open".into()));
        }
        for waiter in detached.applies {
            (waiter.done)(Err("the diagram is no longer open".into()));
        }
        if let Some(chain) = detached.chain {
            for job in chain {
                (job.done)(Err("the diagram is no longer open".into()));
            }
        }
        if let Some(headless) = detached.headless {
            (headless.done)(crate::value::obj([
                ("ok", IpcValue::Bool(false)),
                (
                    "error",
                    crate::value::string("the diagram is no longer open"),
                ),
            ]));
        }
        let mut gone = detached.view;
        if let Some(document) = gone.document.take() {
            document.revoke();
        }
        for (_, pending) in gone.owned_saves.drain() {
            pending.cancel_and_finish();
        }
        for (_, _, reply) in gone.first_save_queue.drain(..) {
            reply.reply(Err(crate::value::thrown("The diagram view was closed")));
        }
        for (_, close) in gone.owned_closes.drain() {
            close.cancel_and_finish();
        }
        for (_, publication) in gone.owned_publications.drain() {
            publication.cancel_and_finish();
        }
        for (_, then) in gone.owned_renames.drain() {
            then(Err(crate::value::thrown("The diagram view was closed")));
        }
        if let Some(copy_now) = gone.copy_now.take() {
            (copy_now.finish)(Err(main_core::shell::renderer_disposal::Failure(
                "The diagram view was closed".into(),
            )));
        }
        if let Some(waiter) = gone.close_save.take() {
            (waiter.resume)(WaitEnd::Answer(false));
        }
        if let Some(waiter) = gone.save.take() {
            (waiter.resume)(WaitEnd::Answer(false));
        }
        Ok(())
    }

    /// `e.sender` gone: what `grantAndTrack`'s `destroyed` listener did
    fn view_gone(&self, view: ViewId) {
        if self.with_state(|state| {
            state.views.get(&view).is_some_and(|entry| {
                entry.saved_actor_identity.is_some() || entry.browser_owner.is_some()
            })
        }) {
            return;
        }
        saved_load::cancel(self, view);
        self.cancel_recovery_open(view);
        let removed = self.with_state(|state| state.views.remove(&view));
        let dirty = removed.as_ref().is_some_and(|v| v.dirty);
        self.recovery_view_gone(view, dirty);
        // a question to the renderer that can no longer be answered
        self.fail_queries_of(view);
        let Some(mut gone) = removed else { return };
        if let Some(document) = gone.document.take() {
            document.revoke();
        }
        for (_, pending) in gone.owned_saves.drain() {
            pending.cancel_and_finish();
        }
        for (_, _, reply) in gone.first_save_queue.drain(..) {
            reply.reply(Err(crate::value::thrown("The diagram view was closed")));
        }
        for (_, close) in gone.owned_closes.drain() {
            close.cancel_and_finish();
        }
        for (_, publication) in gone.owned_publications.drain() {
            publication.cancel_and_finish();
        }
        for (_, then) in gone.owned_renames.drain() {
            then(Err(crate::value::thrown("The diagram view was closed")));
        }
        if let Some(copy_now) = gone.copy_now.take() {
            (copy_now.finish)(Err(main_core::shell::renderer_disposal::Failure(
                "The diagram view was closed".into(),
            )));
        }
        if let Some(waiter) = gone.close_save.take() {
            (waiter.resume)(WaitEnd::Answer(false));
        }
        if let Some(waiter) = gone.save.take() {
            (waiter.resume)(WaitEnd::Answer(false));
        }
    }
}

impl
    main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
        ViewId,
        shell_contract::ipc::RecoveryOpenTarget,
    > for DiagramViews
{
    fn kind(&self) -> &str {
        "diagram"
    }
    fn write_copies_now(
        &self,
        done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>,
    ) {
        DiagramViews::write_copies_now(self, std::time::Duration::from_secs(5), done);
    }
    fn open_file_with_decision(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        self.prepare_recovery_file(*view, request, target, done);
    }
    fn open_untitled_copy(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        self.prepare_recovery_copy(*view, request, target, done);
    }
    fn is_unprotectable(&self, _view: &ViewId) -> bool {
        false
    }
}

/// `registerDiagramIpc()`: the 27 channels the renderer calls
pub fn routes(services: &Services, provided: &Rc<Provided>) -> Result<Vec<Route>, RegisterError> {
    let views = DiagramViews::new(services)?;
    let instance = views.0.id;
    let terminal = crate::host::on_native_view_terminal(services, move |event| {
        if let Some(views) = lookup(instance) {
            views.native_terminal(event)
        } else {
            Ok(())
        }
    })?;
    *views.0.terminal_registration.borrow_mut() = Some(terminal);
    provided.add::<dyn DocumentEditors>(Rc::new(editors::DiagramEditors(views.clone())));
    provided
        .add::<dyn main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
                ViewId,
                shell_contract::ipc::RecoveryOpenTarget,
            >>(Rc::new(views.clone()));
    #[cfg(any(test, feature = "probe"))]
    if let Some(probe) = services.hosts.get::<fakes::ProbeOpen>() {
        views.apply_probe(probe);
    }
    let gone = views.clone();
    crate::host::on_view_gone(services, move |view| gone.view_gone(view));

    macro_rules! invoke {
        ($channel:literal, $method:ident) => {{
            let views = views.clone();
            route::invoke($channel, move |cx, args| views.$method(cx, args))
        }};
    }
    macro_rules! later {
        ($channel:literal, $method:ident) => {{
            let views = views.clone();
            route::invoke_later($channel, move |cx, args, reply| {
                views.$method(cx, args, reply)
            })
        }};
    }
    macro_rules! send {
        ($channel:literal, $method:ident) => {{
            let views = views.clone();
            route::send($channel, move |cx, args| views.$method(cx, args))
        }};
    }

    Ok(vec![
        {
            let views = views.clone();
            route::invoke_later("diagram:consume-pending", move |cx, args, reply| {
                saved_load::consume_pending(&views, cx, args, reply)
            })
        },
        {
            let views = views.clone();
            route::invoke_later("diagram:open-file", move |cx, args, reply| {
                saved_load::open_file(&views, cx, args, reply)
            })
        },
        invoke!("diagram:save-target", save_target),
        later!("diagram:save", save_dispatch),
        later!("diagram:save-into-project", save_into_project_owned),
        invoke!("diagram:open-link", open_link),
        send!("diagram:open-help", open_help),
        invoke!("diagram:open-failed", open_failed),
        send!("diagram:dirty-changed", dirty_changed),
        send!("diagram:close-save-result", close_save_result),
        send!("diagram:save-request-ack", save_request_ack),
        invoke!("diagram:recovery-copy", recovery_copy),
        send!("diagram:recovery-copy-now-done", recovery_copy_now_done),
        invoke!("diagram:seed-take", seed_take),
        invoke!("diagram:recovery-initialized", recovery_initialized),
        invoke!("diagram:export-images", export_images),
        later!("diagram:export-pdf", export_pdf),
        invoke!("diagram:files-pick", files_pick),
        invoke!("diagram:files-add", files_add),
        invoke!("diagram:files-add-pasted-image", files_add_pasted_image),
        later!("diagram:files-read", files_read),
        invoke!("diagram:files-read-image", files_read_image),
        send!("diagram:provisional-title", provisional_title),
        send!("diagram:read-result", read_result),
        send!("diagram:apply-result", apply_result),
        send!("diagram:bytes-result", bytes_result),
        invoke!("diagram:headless-job", headless_job),
        send!("diagram:headless-done", headless_done),
    ])
}

/// This app's fake hosts for [`crate::testkit::Kit`] and the probe; `case` is
/// the case's `hosts.d1` value (see [`fakes`])
#[cfg(any(test, feature = "probe"))]
pub fn insert_fakes(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    fakes::insert(hosts, case);
}

#[cfg(feature = "probe")]
thread_local! {
    /// continuations of triggers that answer after a timer
    static PENDING: RefCell<Vec<Option<shell_contract::editors::Then<IpcValue>>>> = const { RefCell::new(Vec::new()) };
}

/// A JSON argument of a trigger as the renderer's value
#[cfg(feature = "probe")]
fn ipc_of(value: &serde_json::Value) -> IpcValue {
    use serde_json::Value;
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(flag) => IpcValue::Bool(*flag),
        Value::Number(n) => IpcValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => IpcValue::String(text.clone()),
        Value::Array(items) => IpcValue::Array(items.iter().map(ipc_of).collect()),
        Value::Object(entries) => IpcValue::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), ipc_of(item)))
                .collect(),
        ),
    }
}

/// The page read as the object `diagramReadPage` resolves with
#[cfg(feature = "probe")]
fn page_read_value(read: PageRead) -> IpcValue {
    let mut entries = vec![("text".to_owned(), IpcValue::String(read.text))];
    if let (Some(pages), Some(page)) = (read.pages, read.page) {
        entries.push(("pages".to_owned(), pages));
        entries.push(("page".to_owned(), page));
    }
    IpcValue::Object(entries)
}

/// This app's triggers for [`crate::probe_trigger`]: `name` without its
/// `d1/` prefix, each named after the exported function of `diagram-main.ts`
/// or `mcp-queries.ts` it runs; false when it is not one of them
#[cfg(feature = "probe")]
pub fn probe_trigger(
    name: &str,
    view: ViewId,
    args: &serde_json::Value,
    _services: &Services,
    provided: &Rc<Provided>,
    then: shell_contract::editors::Then<IpcValue>,
) -> bool {
    use shell_contract::editors::{SaveMode, editor_for};

    let Some(what) = name.strip_prefix("diagram:") else {
        return false;
    };
    let Some(handle) = views() else {
        return false;
    };
    let text = |i: usize| {
        args.get(i)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let gone = |message: String| crate::value::thrown(message);
    let editor = editor_for(provided, view);
    match what {
        "render-process-gone" => {
            handle.view_crashed(view);
            then(Ok(IpcValue::Undefined));
        }
        "destroyed" => {
            handle.view_gone(view);
            then(Ok(IpcValue::Undefined));
        }
        "requestDiagramSave" => {
            let mode = if text(0) == "saveAs" {
                SaveMode::SaveAs
            } else {
                SaveMode::Save
            };
            handle.request_save(
                view,
                mode,
                Box::new(move |saved| then(Ok(IpcValue::Bool(saved)))),
            );
        }
        "requestDiagramClose" => {
            let parent = args
                .get(0)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                .then(|| handle.window_of(view))
                .flatten();
            handle.request_close(
                view,
                parent,
                Box::new(move |close| then(Ok(IpcValue::Bool(close)))),
            );
        }
        "diagramSaveToPath" => handle.save_to_path(
            view,
            &text(0),
            Box::new(move |saved| then(saved.map(|()| IpcValue::Undefined).map_err(gone))),
        ),
        "diagramFileRenamed" => {
            handle.file_renamed(view, &text(0), &text(1));
            then(Ok(IpcValue::Undefined));
        }
        "sendDiagramMenuCommand" => {
            handle.send_menu_command(view, &text(0));
            then(Ok(IpcValue::Undefined));
        }
        "sendDiagramExportRequest" => {
            match editor {
                Some(editor) => editor.export_request(view, &text(0)),
                None => handle.send_export_request(view, &text(0)),
            }
            then(Ok(IpcValue::Undefined));
        }
        "diagramReadText" => handle.read_text(
            view,
            Box::new(move |read| then(read.map(IpcValue::String).map_err(gone))),
        ),
        "diagramReadPage" => {
            let page = text(0);
            handle.read_page(
                view,
                Some(page.as_str()),
                Box::new(move |read| then(read.map(page_read_value).map_err(gone))),
            );
        }
        "diagramApplyOps" => {
            let ops = match args.get(0).map(ipc_of) {
                Some(IpcValue::Array(ops)) => ops,
                _ => Vec::new(),
            };
            handle.apply_ops(
                view,
                ops,
                Box::new(move |result| then(result.map_err(gone))),
            );
        }
        "writeDiagramCopiesNow" => {
            let timeout = args
                .get(0)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(5000);
            handle.write_copies_now(
                Duration::from_millis(timeout),
                Box::new(move |result| {
                    then(
                        result
                            .map(|_| IpcValue::Undefined)
                            .map_err(|error| crate::value::thrown(error.0)),
                    )
                }),
            );
        }
        // the "Don't Save" hold ends, and the tab that stayed open is asked
        // for a copy again: answers once the hold and a margin have passed
        "discardDiagramRecovery" => {
            handle.discard_recovery(view);
            // `then` is not `Send`: it waits in a core-thread slot the timer takes it from
            let slot = PENDING.with(|p| {
                p.borrow_mut().push(Some(then));
                p.borrow().len() - 1
            });
            handle.after(
                recovery::DONT_SAVE_HOLD + Duration::from_millis(200),
                move |_| {
                    if let Some(then) = PENDING.with(|p| p.borrow_mut()[slot].take()) {
                        then(Ok(IpcValue::Undefined));
                    }
                },
            );
        }
        "diagramTabIdentity" => then(Ok(match handle.tab_identity(view) {
            None => IpcValue::Null,
            Some(identity) => {
                let mut entries = vec![
                    (
                        "sourcePath".to_owned(),
                        identity
                            .source_path
                            .map_or(IpcValue::Null, IpcValue::String),
                    ),
                    ("untitled".to_owned(), IpcValue::Bool(identity.untitled)),
                    ("title".to_owned(), IpcValue::String(identity.title)),
                ];
                if let Some(imported) = identity.imported {
                    entries.push(("imported".to_owned(), IpcValue::String(imported.to_owned())));
                }
                IpcValue::Object(entries)
            }
        })),
        _ => return false,
    }
    true
}
