//! The fakes of [`WindowsHost`] and [`TabHooks`] for `crates/ipc-probe`
//! (tools/ipc-diff), the Rust half of `tools/ipc-diff/groups/w/tabs-lib`.
//! One world answers both traits and the document editors: every call is one
//! line of the case's log, named and shaped as the TypeScript world writes it
//! (`tabs-lib/world.ts`), and every answer comes from the case's `hosts.w`:
//!
//! - `shell`: `{width, height, scale, focused, minimized}` of the shell window
//! - `workArea`, `cursor`: the screen
//! - `views`: `{"<webContents id>": {dirty, pending, memo, destroyed, queryFails}}`
//! - `closeAnswers`: what each editor's close prompt answers, in order (`true`
//!   when it runs out; `"throws"` rejects)
//! - `menus`: for each popup menu the label it picks (`null`: dismissed)
//! - `appMenu`: `[{label, enabled, type, submenu: [...]}]`, the application menu
//! - `homeKinds`: the visible new-document kinds in home's order
//! - `strings`: `tm(key)`; `fail`: names of `create*View` calls that throw
//! - `defaultSaveDir`, `blankXlsx`, `blankPdf` (hex), `refuse` (diagram path to reason)

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use main_core::shell::detached_windows::WindowOptions;
use main_core::shell::split_layout::Rect;
use main_core::shell::tab_manager::{
    DiagramIdentity, HwpUntitledCopy, Opener, SplitViewState, TabError, TabKind, TabResult,
    UnloadedTabOpenContext, ViewSpec,
};
use serde_json::{Map, Value, json};
use shell_contract::Hosts;
use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::error::IpcError;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::provided::Provided;
use shell_contract::runtime::Services;

use super::host::{
    DocAction, HostEvent, HostEvents, MenuEntry, MenuEntryKind, ShellWindow, TabHooks, TabsChanged,
    TimerKey, ViewCreationCleanup, ViewCreationCleanupState, ViewCreationFailure,
    ViewCreationReady, ViewCreationResult, ViewListen, WindowEvent, WindowState, WindowsHost,
    WindowsHostRef,
};

/// The call log of the case, the one `ipc-probe` puts in `Hosts`
pub type ProbeLog = Arc<Mutex<Vec<Value>>>;
pub type ProbeViewRegistrar = Arc<
    dyn Fn(
            shell_contract::ipc::Sender,
            shell_contract::runtime::RendererGeneration,
        ) -> Result<(), String>
        + Send
        + Sync,
>;

type ProbeShellEvent = (
    shell_contract::ipc::Sender,
    shell_contract::runtime::RendererGeneration,
);
type ProbeShellCallback = Box<dyn FnOnce(Option<ProbeShellEvent>) + Send>;

#[derive(Default)]
pub struct ProbeShellRegistration {
    pending: Mutex<Option<(ViewId, WindowId, ProbeShellCallback)>>,
}

impl ProbeShellRegistration {
    fn arm(
        &self,
        view: ViewId,
        window: WindowId,
        callback: ProbeShellCallback,
    ) -> Result<(), String> {
        let mut pending = self.pending.lock().map_err(|error| error.to_string())?;
        if pending.is_some() {
            return Err("The synthetic first shell listener is already armed".into());
        }
        *pending = Some((view, window, callback));
        Ok(())
    }

    pub fn registered(
        &self,
        sender: shell_contract::ipc::Sender,
        generation: shell_contract::runtime::RendererGeneration,
    ) {
        let callback = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !pending.as_ref().is_some_and(|(view, window, _)| {
                sender.view == *view
                    && sender.window == *window
                    && sender.surface == shell_contract::Surface::Shell
            }) {
                return;
            }
            pending.take().map(|(_, _, callback)| callback)
        };
        if let Some(callback) = callback {
            callback(Some((sender, generation)));
        }
    }
}

impl Drop for ProbeShellRegistration {
    fn drop(&mut self) {
        let callback = self
            .pending
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some((_, _, callback)) = callback {
            callback(None);
        }
    }
}

#[derive(Default)]
struct ViewState {
    dirty: bool,
    pending: f64,
    memo: bool,
    destroyed: bool,
    query_fails: bool,
}

struct FixtureClosure {
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    created: shell_contract::runtime::RendererGeneration,
    state: Mutex<FixtureClosureState>,
}

struct FixtureClosureState {
    issuer: Option<shell_contract::ipc::NativeClosureIssuer>,
    receipt: Option<shell_contract::ipc::NativeClosureReceipt>,
    retry_permit: Option<shell_contract::ipc::NativeBrowserClosePermit>,
    watchers: Vec<Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>>,
}

impl FixtureClosure {
    fn new(view: ViewId, created: shell_contract::runtime::RendererGeneration) -> Self {
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
        Self {
            reservation,
            owner,
            created,
            state: Mutex::new(FixtureClosureState {
                issuer: Some(issuer),
                receipt: None,
                retry_permit: None,
                watchers: Vec::new(),
            }),
        }
    }

    fn close(&self) -> shell_contract::ipc::NativeClosureReceipt {
        let (receipt, watchers) = {
            let mut state = self.state.lock().unwrap();
            if let Some(receipt) = &state.receipt {
                return receipt.clone();
            }
            let receipt = state
                .issuer
                .take()
                .expect("fixture closure issuer")
                .closed();
            state.receipt = Some(receipt.clone());
            (receipt, std::mem::take(&mut state.watchers))
        };
        self.created.revoke();
        for watcher in watchers {
            watcher(receipt.clone());
        }
        receipt
    }

    fn subscribe(
        &self,
        watcher: Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>,
    ) {
        let mut state = self.state.lock().unwrap();
        if let Some(receipt) = state.receipt.clone() {
            drop(state);
            watcher(receipt);
        } else {
            state.watchers.push(watcher);
        }
    }
}

type FixtureCloseDone =
    Box<dyn FnOnce(Result<shell_contract::ipc::NativeBrowserRetirement, String>) + Send>;

#[derive(Default)]
struct FixturePreclose {
    state: Mutex<FixturePrecloseState>,
}

#[derive(Default)]
struct FixturePrecloseState {
    barrier: Option<super::host::GeneratedPrecloseBarrier>,
    failure: Option<super::host::GeneratedPrecloseFailure>,
    pending: bool,
    running: bool,
    completed: bool,
    waiters: Vec<FixtureCloseDone>,
}

impl FixturePreclose {
    fn install(
        self: &Arc<Self>,
        close: ViewCreationCleanup,
        barrier: super::host::GeneratedPrecloseBarrier,
        failure: super::host::GeneratedPrecloseFailure,
    ) -> Result<super::host::GeneratedPrecloseRetry, String> {
        let mut state = self.state.lock().unwrap();
        if state.barrier.is_some() || state.running || state.completed {
            return Err("The synthetic preclose barrier is already installed or closing".into());
        }
        state.barrier = Some(barrier);
        state.failure = Some(failure);
        drop(state);
        let gate = self.clone();
        Ok(Arc::new(move || gate.clone().start(close.clone(), None)))
    }

    fn cleanup(self: Arc<Self>, close: ViewCreationCleanup, done: FixtureCloseDone) {
        self.start(close, Some(done));
    }

    fn start(self: Arc<Self>, close: ViewCreationCleanup, done: Option<FixtureCloseDone>) {
        let barrier = {
            let mut state = self.state.lock().unwrap();
            if let Some(done) = done {
                state.waiters.push(done);
                state.pending = true;
            }
            if state.running || !state.pending {
                return;
            }
            state.running = true;
            state.barrier.clone()
        };
        let complete = move |result: Result<(), String>| {
            if let Err(error) = result {
                let (waiters, failure) = {
                    let mut state = self.state.lock().unwrap();
                    state.running = false;
                    (std::mem::take(&mut state.waiters), state.failure.clone())
                };
                if let Some(failure) = failure {
                    failure(error.clone());
                }
                for waiter in waiters {
                    waiter(Err(error.clone()));
                }
                return;
            }
            close(Box::new(move |result| {
                let (waiters, failure) = {
                    let mut state = self.state.lock().unwrap();
                    state.running = false;
                    if result.is_ok() {
                        state.completed = true;
                        state.pending = false;
                        state.barrier = None;
                    }
                    (std::mem::take(&mut state.waiters), state.failure.clone())
                };
                if let Err(error) = &result
                    && let Some(failure) = failure
                {
                    failure(error.clone());
                }
                for waiter in waiters {
                    waiter(result.clone());
                }
            }));
        };
        match barrier {
            Some(barrier) => barrier(Box::new(complete)),
            None => complete(Ok(())),
        }
    }
}

fn gated_cleanup(gate: Arc<FixturePreclose>, close: ViewCreationCleanup) -> ViewCreationCleanup {
    Arc::new(move |done| gate.clone().cleanup(close.clone(), done))
}

#[cfg(test)]
fn fixture_preclose_cleanup(
    fake: &Fake,
    view: ViewId,
    gate: Arc<FixturePreclose>,
) -> ViewCreationCleanup {
    gated_cleanup(gate, fixture_cleanup(fake, view))
}

struct FixtureWindowProof {
    window: WindowId,
    state: Mutex<FixtureWindowProofState>,
}

struct FixtureWindowProofState {
    revision: Option<u64>,
    destroyed: bool,
    title: String,
}

struct FixtureWindowGuard<'a>(MutexGuard<'a, FixtureWindowProofState>);

impl super::window_snapshot::NativeWindowGuard for FixtureWindowGuard<'_> {
    fn revision(&self) -> u64 {
        self.0.revision.unwrap_or(0)
    }
    fn stable(&self) -> bool {
        self.0.revision.is_some()
    }
    fn destroyed(&self) -> bool {
        self.0.destroyed
    }
}

impl super::window_snapshot::NativeWindowProof for FixtureWindowProof {
    fn lock(&self) -> Box<dyn super::window_snapshot::NativeWindowGuard + '_> {
        Box::new(FixtureWindowGuard(self.state.lock().unwrap()))
    }
    fn observe(
        self: Arc<Self>,
        done: Box<dyn FnOnce(Result<super::window_snapshot::WindowObservation, String>) + Send>,
    ) {
        let captured = {
            let state = self.state.lock().unwrap();
            state
                .revision
                .map(|revision| (revision, state.destroyed, state.title.clone()))
        };
        let result = captured
            .ok_or("Synthetic window revision is invalid".to_owned())
            .map(|(revision, destroyed, title)| {
                let reservation = super::window_snapshot::WindowReservation::new(self.window, self);
                super::window_snapshot::WindowObservation::new(
                    reservation,
                    revision,
                    destroyed,
                    title,
                )
            });
        done(result);
    }
    fn set_title(&self, title: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.destroyed {
            return Err("The synthetic window is closed".into());
        }
        let next = state.revision.and_then(|revision| revision.checked_add(1));
        state.revision = next;
        if next.is_none() {
            return Err("The synthetic window revision is invalid".into());
        }
        state.title = title.to_owned();
        Ok(())
    }
}

impl FixtureWindowProof {
    fn new(window: WindowId, title: String) -> Arc<Self> {
        Arc::new(Self {
            window,
            state: Mutex::new(FixtureWindowProofState {
                revision: Some(0),
                destroyed: false,
                title,
            }),
        })
    }
    fn update(&self, update: impl FnOnce(&mut FixtureWindowProofState)) {
        let mut state = self.state.lock().unwrap();
        state.revision = state.revision.and_then(|revision| revision.checked_add(1));
        update(&mut state);
    }
}

struct WinState {
    destroyed: bool,
    minimized: bool,
    focused: bool,
    bounds: Rect,
    position: (f64, f64),
    proof: Arc<FixtureWindowProof>,
}

struct World {
    case: Value,
    log: ProbeLog,
    muted: bool,
    events: Option<Arc<dyn HostEvents>>,
    registrar: Option<ProbeViewRegistrar>,
    owners: Option<shell_contract::hosts::OwnedEffectsHost>,
    cleanup_lifetime: shell_contract::runtime::CancelToken,
    startup_issuer: Option<Arc<shell_contract::ipc::NativeInvocationIssuer>>,
    startup_generation: Option<shell_contract::runtime::RendererGeneration>,
    startup_error: Option<String>,
    next_view: u32,
    next_window: u32,
    next_uuid: u32,
    views: HashMap<u32, ViewState>,
    kinds: HashMap<u32, TabKind>,
    saved_barriers: HashMap<ViewId, (shell_contract::ipc::NativeSavedTarget, Arc<()>)>,
    closures: HashMap<ViewId, Arc<FixtureClosure>>,
    generated: HashMap<ViewId, main_core::shell::tab_manager::EditorSeed>,
    generated_tokens: HashMap<ViewId, Arc<()>>,
    generated_epochs: HashMap<ViewId, shell_contract::runtime::RendererGeneration>,
    generated_epoch_tokens: HashMap<ViewId, Arc<()>>,
    generated_placements:
        HashMap<ViewId, Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
    generated_placement_tokens: HashMap<ViewId, Arc<()>>,
    windows: HashMap<u32, WinState>,
    close_answers: VecDeque<Value>,
    generated_retry_answers: VecDeque<Value>,
    saved_load_answers: VecDeque<Value>,
    menus: VecDeque<Value>,
    app_labels: Vec<String>,
    active_menu: Option<(TabKind, Option<ViewId>)>,
    suggested: HashSet<String>,
    shell: ShellWindow,
}

fn rect_json(r: &Rect) -> Value {
    json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height })
}

fn rect_of(v: &Value, default: Rect) -> Rect {
    let f = |key: &str, d: f64| v.get(key).and_then(Value::as_f64).unwrap_or(d);
    Rect {
        x: f("x", default.x),
        y: f("y", default.y),
        width: f("width", default.width),
        height: f("height", default.height),
    }
}

fn opt_str(text: Option<&str>) -> Value {
    text.map_or(Value::Null, Value::from)
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok())
        .collect()
}

impl World {
    fn new(case: &Value, log: ProbeLog) -> World {
        let shell = &case["shell"];
        let window = shell["window"].as_u64().unwrap_or(1) as u32;
        let view = shell["view"].as_u64().unwrap_or(1) as u32;
        let mut windows = HashMap::new();
        windows.insert(
            window,
            WinState {
                destroyed: false,
                minimized: shell["minimized"].as_bool().unwrap_or(false),
                focused: shell["focused"].as_bool().unwrap_or(true),
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: shell["width"].as_f64().unwrap_or(1200.0),
                    height: shell["height"].as_f64().unwrap_or(800.0),
                },
                position: (0.0, 0.0),
                proof: FixtureWindowProof::new(WindowId(window), String::new()),
            },
        );
        let queue = |key: &str| -> VecDeque<Value> {
            case[key]
                .as_array()
                .map(|a| a.iter().cloned().collect())
                .unwrap_or_default()
        };
        let mut views = HashMap::new();
        if let Some(map) = case["views"].as_object() {
            for (id, v) in map {
                if let Ok(id) = id.parse::<u32>() {
                    views.insert(
                        id,
                        ViewState {
                            dirty: v["dirty"].as_bool().unwrap_or(false),
                            pending: v["pending"].as_f64().unwrap_or(0.0),
                            memo: v["memo"].as_bool().unwrap_or(false),
                            destroyed: v["destroyed"].as_bool().unwrap_or(false),
                            query_fails: v["queryFails"].as_bool().unwrap_or(false),
                        },
                    );
                }
            }
        }
        World {
            case: case.clone(),
            log,
            muted: false,
            events: None,
            registrar: None,
            owners: None,
            cleanup_lifetime: shell_contract::runtime::CancelToken::default(),
            startup_issuer: None,
            startup_generation: None,
            startup_error: None,
            next_view: 100,
            next_window: case["firstWindow"].as_u64().unwrap_or(2) as u32,
            next_uuid: 1,
            views,
            kinds: HashMap::new(),
            saved_barriers: HashMap::new(),
            closures: HashMap::new(),
            generated: HashMap::new(),
            generated_tokens: HashMap::new(),
            generated_epochs: HashMap::new(),
            generated_epoch_tokens: HashMap::new(),
            generated_placements: HashMap::new(),
            generated_placement_tokens: HashMap::new(),
            windows,
            close_answers: queue("closeAnswers"),
            generated_retry_answers: queue("generatedRetryAnswers"),
            saved_load_answers: queue("savedLoadAnswers"),
            menus: queue("menus"),
            app_labels: Vec::new(),
            active_menu: None,
            suggested: HashSet::new(),
            shell: ShellWindow {
                window: WindowId(window),
                view: ViewId(view),
            },
        }
    }

    fn log(&self, line: Vec<Value>) {
        if !self.muted {
            self.log.lock().unwrap().push(Value::Array(line));
        }
    }

    fn view(&mut self, id: u32) -> &mut ViewState {
        self.views.entry(id).or_default()
    }

    fn send(&self, event: HostEvent) {
        if let Some(events) = &self.events {
            events.send(event);
        }
    }

    fn make_view(&mut self) -> ViewId {
        let id = self.next_view;
        self.next_view += 1;
        ViewId(id)
    }

    fn label_of(&self, window: Option<WindowId>) -> Value {
        match window {
            Some(w) if w == self.shell.window => Value::from("shell"),
            Some(w) => Value::from(format!("window:{}", w.0)),
            None => Value::Null,
        }
    }

    fn string(&self, key: &str) -> String {
        self.case["strings"][key].as_str().unwrap_or(key).to_owned()
    }
}

/// The world, behind the traits
#[derive(Clone)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn w(&self) -> MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }
}

fn fixture_reserve(
    fake: &Fake,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
) -> Arc<FixtureClosure> {
    let closure = Arc::new(FixtureClosure::new(view, created));
    fake.w().closures.insert(view, closure.clone());
    closure
}

fn fixture_close_state(fake: &Fake, view: ViewId) -> Arc<FixtureClosure> {
    let mut world = fake.w();
    world
        .closures
        .entry(view)
        .or_insert_with(|| {
            Arc::new(FixtureClosure::new(
                view,
                shell_contract::runtime::RendererGeneration::new(),
            ))
        })
        .clone()
}

struct FixtureCloseProducer {
    fake: Fake,
    view: ViewId,
    closure: Arc<FixtureClosure>,
}

impl super::host::NativeBrowserCloseProducer for FixtureCloseProducer {
    fn watch(
        &self,
        done: Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>,
    ) -> Result<(), String> {
        self.closure.subscribe(done);
        Ok(())
    }

    fn dispatch_close(
        &self,
        mut permit: shell_contract::ipc::NativeBrowserClosePermit,
    ) -> Result<
        shell_contract::ipc::NativeBrowserClosePermit,
        (String, shell_contract::ipc::NativeBrowserClosePermit),
    > {
        if !permit
            .reservation()
            .same_reservation(&self.closure.reservation)
        {
            return Err((
                "The synthetic close permit belongs to another browser".into(),
                permit,
            ));
        }
        if let Err(error) = permit.begin_dispatch() {
            return Err((error, permit));
        }
        let mut world = self.fake.w();
        if !world
            .closures
            .get(&self.view)
            .is_some_and(|current| Arc::ptr_eq(current, &self.closure))
        {
            drop(world);
            let error = match permit.dispatch_failed() {
                Ok(()) => "The synthetic browser reservation was replaced".to_owned(),
                Err(reset) => format!("The synthetic browser reservation was replaced; {reset}"),
            };
            return Err((error, permit));
        }
        let entry = world.view(self.view.0);
        if !entry.destroyed {
            entry.destroyed = true;
            world.log(vec![json!("close"), json!(self.view.0)]);
        }
        drop(world);
        self.closure.close();
        Ok(permit)
    }
}

fn fixture_cleanup(fake: &Fake, view: ViewId) -> ViewCreationCleanup {
    let closure = fixture_close_state(fake, view);
    fixture_cleanup_for(fake, view, closure.owner.clone())
}

fn fixture_cleanup_for(
    fake: &Fake,
    view: ViewId,
    owner: shell_contract::ipc::NativeBrowserOwner,
) -> ViewCreationCleanup {
    let closure = fixture_close_state(fake, view);
    let producer = Arc::new(FixtureCloseProducer {
        fake: fake.clone(),
        view,
        closure: closure.clone(),
    });
    Arc::new(move |done| {
        use super::host::NativeBrowserCloseProducer;
        use shell_contract::ipc::{NativeBrowserCloseClaim, NativeBrowserRetirement};
        let completed = closure.state.lock().unwrap().receipt.clone();
        if let Some(receipt) = completed {
            return done(Ok(NativeBrowserRetirement::Closed(receipt)));
        }
        let retry = closure.state.lock().unwrap().retry_permit.take();
        let claim = match retry {
            Some(permit) => Ok(NativeBrowserCloseClaim::Won { permit, value: () }),
            None => closure.reservation.claim_close(&owner, |_| Ok(())),
        };
        match claim {
            Ok(NativeBrowserCloseClaim::Superseded(proof)) => {
                done(Ok(NativeBrowserRetirement::Superseded(proof)))
            }
            Ok(NativeBrowserCloseClaim::Won { permit, .. }) => {
                let result = Arc::new(Mutex::new(Some(done)));
                let callback = result.clone();
                if let Err(error) = producer.watch(Box::new(move |receipt| {
                    let done = callback.lock().unwrap().take();
                    if let Some(done) = done {
                        done(Ok(NativeBrowserRetirement::Closed(receipt)));
                    }
                })) {
                    closure.state.lock().unwrap().retry_permit = Some(permit);
                    let done = result.lock().unwrap().take();
                    if let Some(done) = done {
                        done(Err(error));
                    }
                    return;
                }
                if let Err((error, permit)) = producer.dispatch_close(permit) {
                    closure.state.lock().unwrap().retry_permit = Some(permit);
                    let done = result.lock().unwrap().take();
                    if let Some(done) = done {
                        done(Err(error));
                    }
                }
            }
            Err(error) => done(Err(error)),
        }
    })
}

fn fixture_ready<T>(fake: &Fake, view: ViewId, value: T) -> ViewCreationReady<T> {
    let state = fixture_close_state(fake, view);
    let producer: Arc<dyn super::host::NativeBrowserCloseProducer> =
        Arc::new(FixtureCloseProducer {
            fake: fake.clone(),
            view,
            closure: state.clone(),
        });
    let watched = state.clone();
    let closure: main_core::shell::tab_manager::ViewClosureWatch<
        shell_contract::ipc::NativeBrowserRetirement,
    > = Arc::new(move |watcher| {
        watched.subscribe(Box::new(move |receipt| {
            watcher(shell_contract::ipc::NativeBrowserRetirement::Closed(
                receipt,
            ))
        }));
        Ok(())
    });
    let reservation = super::host::NativeBrowserBinding {
        reservation: state.reservation.clone(),
        owner: state.owner.clone(),
        producer,
        closure: closure.clone(),
        registration: super::host::NativeBrowserRegistration::Unregistered,
    };
    ViewCreationReady {
        view: value,
        cleanup: fixture_cleanup(fake, view),
        closure,
        reservation,
    }
}

fn fixture_creation<T>(fake: &Fake, result: TabResult<(ViewId, T)>) -> ViewCreationResult<T> {
    match result {
        Ok((view, value)) => Ok(fixture_ready(fake, view, value)),
        Err(error) => Err(ViewCreationFailure {
            error,
            cleanup: ViewCreationCleanupState::Complete,
        }),
    }
}

fn fixture_surface(kind: TabKind) -> shell_contract::Surface {
    use shell_contract::Surface;
    match kind {
        TabKind::Home | TabKind::Help => Surface::Shell,
        TabKind::Docs => Surface::Docs,
        TabKind::Sheets => Surface::Sheets,
        TabKind::Slides => Surface::Slides,
        TabKind::Pdf => Surface::Pdf,
        TabKind::Markdown => Surface::Markdown,
        TabKind::Html => Surface::Html,
        TabKind::Mindmap => Surface::Mindmap,
        TabKind::Hwp => Surface::Hwp,
        TabKind::Epub => Surface::Epub,
        TabKind::Diagram => Surface::Diagram,
    }
}

fn fixture_register_owner(
    fake: &Fake,
    view: ViewId,
    generation: &shell_contract::runtime::RendererGeneration,
    surface: shell_contract::Surface,
) -> Result<shell_contract::ipc::NativeRequestOwner, String> {
    let (registrar, owners, window) = {
        let world = fake.w();
        (
            world
                .registrar
                .clone()
                .ok_or("Synthetic view registrar unavailable")?,
            world
                .owners
                .clone()
                .ok_or("Synthetic ownership unavailable")?,
            world.shell.window,
        )
    };
    registrar(
        shell_contract::ipc::Sender {
            view,
            window,
            surface,
        },
        generation.clone(),
    )?;
    owners.capture_owner(view, generation)
}

#[allow(clippy::too_many_arguments)]
fn fixture_native_command(
    fake: &Fake,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    created: &shell_contract::runtime::RendererGeneration,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    create: impl FnOnce() -> TabResult<ViewId>,
    initialize: super::host::EditorInitializer,
) -> ViewCreationResult<shell_contract::ipc::NativeInvocationDocument> {
    use shell_contract::hosts::synthetic::{
        register_native_invocation_fixture, with_native_command_creation,
        with_native_command_target,
    };
    let mut create = Some(create);
    let mut view = None;
    let admitted = with_native_command_creation(
        operation,
        expected,
        created,
        participants,
        cancel,
        pending,
        &mut || {
            let created_view = create.take().ok_or("Synthetic creation already ran")?()
                .map_err(|error| error.0)?;
            fixture_reserve(fake, created_view, created.clone());
            view = Some(created_view);
            Ok(())
        },
    );
    if let Err(error) = admitted {
        return match view {
            Some(view) => {
                let close = fixture_cleanup(fake, view);
                let generation = created.clone();
                let cleanup = Arc::new(move |done| {
                    generation.revoke();
                    close(done);
                });
                Err(ViewCreationFailure {
                    error: TabError(error),
                    cleanup: ViewCreationCleanupState::Pending(cleanup),
                })
            }
            None => fixture_creation(fake, Err(TabError(error))),
        };
    }
    let Some(view) = view else {
        return fixture_creation(fake, Err(TabError("Synthetic creation did not run".into())));
    };
    let close = fixture_cleanup(fake, view);
    let generation = created.clone();
    let cleanup: ViewCreationCleanup = Arc::new(move |done| {
        generation.revoke();
        close(done);
    });
    let state = fixture_close_state(fake, view);
    if let Err(error) = initialize(view, created, &state.reservation, &state.owner) {
        return Err(ViewCreationFailure {
            error,
            cleanup: ViewCreationCleanupState::Pending(cleanup),
        });
    }
    let result = (|| {
        let owner = fixture_register_owner(fake, view, created, operation.selection().surface())?;
        let target = operation.reserve_target(view, created.clone())?;
        let document = register_native_invocation_fixture(&target, owner.clone())?;
        with_native_command_target(
            operation,
            expected,
            &document,
            &owner,
            participants,
            cancel,
            pending,
            &mut || Ok(()),
        )?;
        Ok(document)
    })();
    match result {
        Ok(document) => {
            let owner = document.owner().clone();
            let mut ready = fixture_ready(fake, view, document);
            ready.reservation.registration =
                super::host::NativeBrowserRegistration::Registered(owner);
            ready.cleanup = cleanup;
            Ok(ready)
        }
        Err(error) => Err(ViewCreationFailure {
            error: TabError(error),
            cleanup: ViewCreationCleanupState::Pending(cleanup),
        }),
    }
}

fn fixture_command_matches(
    spec: &ViewSpec<'_>,
    command: &shell_contract::ipc::NativeEditorCommand,
) -> bool {
    use shell_contract::{Surface, ipc::NativeEditorCommand};
    let (surface, path, sheets_opening) = match spec {
        ViewSpec::Docs { path } => (Surface::Docs, *path, None),
        ViewSpec::Sheets { opening_workbook } => (Surface::Sheets, None, *opening_workbook),
        ViewSpec::Slides { path } => (Surface::Slides, *path, None),
        ViewSpec::Pdf { path } => (Surface::Pdf, Some(*path), None),
        ViewSpec::Markdown { path } => (Surface::Markdown, *path, None),
        ViewSpec::Html { path } => (Surface::Html, *path, None),
        ViewSpec::Mindmap { path } => (Surface::Mindmap, *path, None),
        ViewSpec::Epub { path } => (Surface::Epub, Some(*path), None),
        ViewSpec::Diagram { path } => (Surface::Diagram, *path, None),
        ViewSpec::Hwp {
            path,
            untitled_copy: HwpUntitledCopy::Flag(false),
        } => (Surface::Hwp, *path, None),
        _ => return false,
    };
    match command {
        NativeEditorCommand::OpenFile {
            surface: selected,
            path: selected_path,
        } => {
            *selected == surface
                && if surface == Surface::Sheets {
                    sheets_opening == Some(true)
                } else {
                    path.is_some_and(|path| std::path::Path::new(path) == selected_path)
                }
        }
        NativeEditorCommand::NewDocument {
            surface: selected, ..
        } => {
            *selected == surface
                && path.is_none()
                && (surface != Surface::Sheets || sheets_opening != Some(true))
        }
        _ => false,
    }
}

/// The log line of a popup entry as `Menu.buildFromTemplate` sees it: absent
/// and default fields left out (world.ts writes it the same way)
fn entry_json(e: &MenuEntry) -> Value {
    let mut map = Map::new();
    match e.kind {
        MenuEntryKind::Separator => {
            map.insert("type".into(), json!("separator"));
        }
        MenuEntryKind::Checkbox => {
            map.insert("label".into(), json!(e.label));
            map.insert("type".into(), json!("checkbox"));
        }
        MenuEntryKind::Header => {
            map.insert("label".into(), json!(e.label));
            map.insert("type".into(), json!("header"));
        }
        MenuEntryKind::Normal => {
            map.insert("label".into(), json!(e.label));
        }
    }
    if !e.enabled {
        map.insert("enabled".into(), json!(false));
    }
    if e.checked {
        map.insert("checked".into(), json!(true));
    }
    if let Some(icon) = e.icon {
        map.insert("icon".into(), json!(icon));
    }
    if !e.submenu.is_empty() {
        map.insert(
            "submenu".into(),
            Value::Array(e.submenu.iter().map(entry_json).collect()),
        );
    }
    Value::Object(map)
}

/// The entry with this label, depth first
fn pick(entries: &[MenuEntry], label: &str) -> Option<u32> {
    for e in entries {
        if e.kind != MenuEntryKind::Separator && e.label == label {
            return Some(e.id);
        }
        if let Some(id) = pick(&e.submenu, label) {
            return Some(id);
        }
    }
    None
}

impl WindowsHost for Fake {
    fn attach(&self, events: Arc<dyn HostEvents>) {
        self.w().events = Some(events);
    }

    fn shell_window(&self) -> Option<ShellWindow> {
        let w = self.w();
        (w.case["noShell"].as_bool() != Some(true)).then_some(w.shell)
    }

    fn create_shell_window(&self) -> ShellWindow {
        self.w().shell
    }

    fn window_state(&self, window: WindowId) -> WindowState {
        let w = self.w();
        match w.windows.get(&window.0) {
            Some(s) => WindowState {
                destroyed: s.destroyed,
                minimized: s.minimized,
                focused: s.focused,
                visible: true,
                content_bounds: s.bounds,
                position: s.position,
                title: s.proof.state.lock().unwrap().title.clone(),
            },
            None => WindowState {
                destroyed: true,
                minimized: false,
                focused: false,
                visible: false,
                content_bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                },
                position: (0.0, 0.0),
                title: String::new(),
            },
        }
    }

    fn create_window(&self, options: &WindowOptions) -> super::window_snapshot::WindowReservation {
        let mut w = self.w();
        let id = w.next_window;
        w.next_window += 1;
        let mut opts = Map::new();
        opts.insert("width".into(), json!(options.width));
        opts.insert("height".into(), json!(options.height));
        if let Some((x, y)) = options.origin {
            opts.insert("x".into(), json!(x));
            opts.insert("y".into(), json!(y));
        }
        opts.insert("minWidth".into(), json!(options.min_width));
        opts.insert("minHeight".into(), json!(options.min_height));
        opts.insert("title".into(), json!(options.title));
        opts.insert("show".into(), json!(options.show));
        opts.insert("autoHideMenuBar".into(), json!(options.auto_hide_menu_bar));
        w.log(vec![json!("newBrowserWindow"), Value::Object(opts)]);
        let proof = FixtureWindowProof::new(WindowId(id), options.title.clone());
        w.windows.insert(
            id,
            WinState {
                destroyed: false,
                minimized: false,
                focused: false,
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: options.width,
                    height: options.height,
                },
                position: options.origin.unwrap_or((0.0, 0.0)),
                proof: proof.clone(),
            },
        );
        super::window_snapshot::WindowReservation::new(WindowId(id), proof)
    }

    fn window_restore(&self, window: WindowId) {
        self.w()
            .log(vec![json!("win"), json!(window.0), json!("restore")]);
    }
    fn window_show(&self, window: WindowId) {
        self.w()
            .log(vec![json!("win"), json!(window.0), json!("show")]);
    }
    fn window_show_inactive(&self, window: WindowId) {
        self.w()
            .log(vec![json!("win"), json!(window.0), json!("showInactive")]);
    }
    fn window_focus(&self, window: WindowId) {
        self.w()
            .log(vec![json!("win"), json!(window.0), json!("focus")]);
    }
    fn window_destroy(&self, window: WindowId) {
        let mut w = self.w();
        w.log(vec![json!("win"), json!(window.0), json!("destroy")]);
        if let Some(s) = w.windows.get_mut(&window.0) {
            s.destroyed = true;
            s.proof.update(|state| state.destroyed = true);
        }
        w.send(HostEvent::Window(window, WindowEvent::Closed));
    }
    fn window_set_title(&self, window: &super::window_snapshot::WindowReservation, title: &str) {
        if window.set_title(title).is_ok() {
            self.w().log(vec![
                json!("win"),
                json!(window.window().0),
                json!("setTitle"),
                json!(title),
            ]);
        }
    }
    fn window_set_position(&self, window: WindowId, x: f64, y: f64) {
        let mut w = self.w();
        w.log(vec![
            json!("win"),
            json!(window.0),
            json!("setPosition"),
            json!(x),
            json!(y),
        ]);
        if let Some(s) = w.windows.get_mut(&window.0) {
            s.position = (x, y);
        }
        w.send(HostEvent::Window(window, WindowEvent::Move));
    }
    fn window_set_frame(&self, window: WindowId, frame: &Rect) {
        let w = self.w();
        w.log(vec![
            json!("win"),
            json!(window.0),
            json!("setBounds"),
            rect_json(frame),
        ]);
        if window != w.shell.window {
            w.send(HostEvent::Window(window, WindowEvent::Move));
            w.send(HostEvent::Window(window, WindowEvent::Resize));
        }
    }
    fn window_work_area(&self, _window: WindowId) -> Rect {
        let w = self.w();
        rect_of(
            &w.case["workArea"],
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
        )
    }
    fn add_child_view(&self, window: WindowId, view: ViewId) {
        self.w().log(vec![
            json!("win"),
            json!(window.0),
            json!("addChildView"),
            json!(view.0),
        ]);
    }
    fn remove_child_view(&self, window: WindowId, view: ViewId) {
        self.w().log(vec![
            json!("win"),
            json!(window.0),
            json!("removeChildView"),
            json!(view.0),
        ]);
    }

    fn view_destroyed(&self, view: ViewId) -> bool {
        self.w().view(view.0).destroyed
    }
    fn view_set_visible(&self, view: ViewId, visible: bool) {
        self.w()
            .log(vec![json!("setVisible"), json!(view.0), json!(visible)]);
    }
    fn view_set_bounds(&self, view: ViewId, bounds: &Rect) {
        self.w()
            .log(vec![json!("setBounds"), json!(view.0), rect_json(bounds)]);
    }
    fn view_focus(&self, view: ViewId) {
        self.w()
            .log(vec![json!("webContents.focus"), json!(view.0)]);
    }
    fn view_close(&self, view: ViewId) {
        let closure = {
            let mut world = self.w();
            world.log(vec![json!("close"), json!(view.0)]);
            world.view(view.0).destroyed = true;
            world.closures.get(&view).cloned()
        };
        if let Some(closure) = closure {
            closure.close();
        }
    }
    fn view_reload(&self, view: ViewId) {
        self.w().log(vec![json!("reload"), json!(view.0)]);
    }
    fn view_load_url(&self, view: ViewId, url: &str) {
        self.w()
            .log(vec![json!("loadURL"), json!(view.0), json!(url)]);
    }
    fn view_listen(&self, view: ViewId, listen: ViewListen) {
        let w = self.w();
        let (how, events): (&str, &[&str]) = match listen {
            ViewListen::HtmlFullScreen => {
                ("on", &["enter-html-full-screen", "leave-html-full-screen"])
            }
            ViewListen::PaneFocus => ("on", &["focus"]),
            ViewListen::RenderProcessGone => ("once", &["render-process-gone"]),
        };
        for event in events {
            w.log(vec![json!(how), json!(view.0), json!(event)]);
        }
    }
    fn watch_page_load(&self, view: ViewId, _token: u64) {
        let w = self.w();
        for event in ["did-finish-load", "destroyed"] {
            w.log(vec![json!("once"), json!(view.0), json!(event)]);
        }
    }
    fn create_help_view(&self, done: Box<dyn FnOnce(ViewCreationResult<ViewId>)>) {
        let mut w = self.w();
        let view = w.make_view();
        w.log(vec![
            json!("newWebContentsView"),
            json!(view.0),
            json!({
                "contextIsolation": true,
                "nodeIntegration": false,
                "sandbox": true,
                "backgroundThrottling": false
            }),
        ]);
        w.log(vec![json!("setWindowOpenHandler"), json!(view.0)]);
        drop(w);
        done(fixture_creation(self, Ok((view, view))));
    }

    fn create_help_native_command(
        &self,
        operation: shell_contract::ipc::NativeInvocationOperation,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    ) {
        let created = shell_contract::runtime::RendererGeneration::new();
        let expected = match operation.selection() {
            shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                command @ shell_contract::ipc::NativeEditorCommand::OpenHelp { .. },
            ) => command.clone(),
            _ => {
                return done(fixture_creation(
                    self,
                    Err(TabError("Synthetic help command does not match".into())),
                ));
            }
        };
        done(fixture_native_command(
            self,
            &operation,
            &expected,
            &created,
            &participants,
            &cancel,
            &pending,
            || {
                let mut world = self.w();
                let view = world.make_view();
                world.log(vec![json!("newWebContentsView"), json!(view.0), json!({
                "contextIsolation": true, "nodeIntegration": false, "sandbox": true, "backgroundThrottling": false
            })]);
                world.log(vec![json!("setWindowOpenHandler"), json!(view.0)]);
                Ok(view)
            },
            Box::new(|_, _, _, _| Ok(())),
        ));
    }

    fn cursor_point(&self) -> (f64, f64) {
        let w = self.w();
        (
            w.case["cursor"]["x"].as_f64().unwrap_or(0.0),
            w.case["cursor"]["y"].as_f64().unwrap_or(0.0),
        )
    }
    fn display_work_area(&self, _at: (f64, f64)) -> Rect {
        self.window_work_area(WindowId(0))
    }

    fn set_timer(&self, _key: TimerKey, delay_ms: f64) {
        let w = self.w();
        if delay_ms == 0.0 {
            w.log(vec![json!("setImmediate")]);
        } else {
            w.log(vec![json!("setTimeout"), json!(delay_ms)]);
        }
    }
    fn clear_timer(&self, _key: TimerKey) {
        self.w().log(vec![json!("clearTimeout")]);
    }

    fn popup_menu(
        &self,
        window: WindowId,
        entries: &[MenuEntry],
        at: Option<(f64, f64)>,
        token: u64,
    ) {
        let mut w = self.w();
        let mut opts = Map::new();
        opts.insert("window".into(), json!(window.0));
        if let Some((x, y)) = at {
            opts.insert("x".into(), json!(x));
            opts.insert("y".into(), json!(y));
        }
        w.log(vec![
            json!("menu.popup"),
            Value::Object(opts),
            Value::Array(entries.iter().map(entry_json).collect()),
        ]);
        let chosen = w
            .menus
            .pop_front()
            .and_then(|label| label.as_str().and_then(|l| pick(entries, l)));
        w.send(HostEvent::Menu { token, chosen });
    }
}

// ---- the hooks ----

/// `createDocsView`, ... the TypeScript name and arguments of a view spec
fn create_call(spec: &ViewSpec<'_>) -> (&'static str, Vec<Value>, TabKind) {
    match spec {
        ViewSpec::Docs { path } => ("createDocsView", vec![opt_str(*path)], TabKind::Docs),
        ViewSpec::Sheets { opening_workbook } => {
            let mut opts = Map::new();
            opts.insert("includeAiHandlers".into(), Value::Bool(false));
            if let Some(opening) = opening_workbook {
                opts.insert("openingWorkbook".into(), Value::Bool(*opening));
            }
            (
                "createSheetsView",
                vec![Value::Object(opts)],
                TabKind::Sheets,
            )
        }
        ViewSpec::Slides { path } => ("createSlidesView", vec![opt_str(*path)], TabKind::Slides),
        ViewSpec::Pdf { path } => ("createPdfView", vec![Value::from(*path)], TabKind::Pdf),
        ViewSpec::Markdown { path } => (
            "createMarkdownView",
            vec![opt_str(*path)],
            TabKind::Markdown,
        ),
        ViewSpec::Html { path } => ("createHtmlView", vec![opt_str(*path)], TabKind::Html),
        ViewSpec::HtmlPresent { owner, title } => (
            "createHtmlPresentView",
            vec![Value::from(*owner), Value::from(*title)],
            TabKind::Html,
        ),
        ViewSpec::Mindmap { path } => ("createMindmapView", vec![opt_str(*path)], TabKind::Mindmap),
        ViewSpec::Epub { path } => ("createEpubView", vec![Value::from(*path)], TabKind::Epub),
        ViewSpec::Diagram { path } => ("createDiagramView", vec![opt_str(*path)], TabKind::Diagram),
        ViewSpec::Hwp {
            path,
            untitled_copy,
        } => {
            let copy = match untitled_copy {
                HwpUntitledCopy::Flag(flag) => Value::Bool(*flag),
                HwpUntitledCopy::BaseName(name) => json!({ "baseName": name }),
            };
            (
                "createHwpView",
                vec![opt_str(*path), json!({ "untitledCopy": copy })],
                TabKind::Hwp,
            )
        }
    }
}

struct ProbeEpochRollback {
    world: Fake,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    token: Arc<()>,
    previous:
        std::cell::RefCell<Option<(shell_contract::runtime::RendererGeneration, Option<Arc<()>>)>>,
    completed: std::cell::Cell<bool>,
}

impl ProbeEpochRollback {
    fn remove(&self, restore: bool) -> Result<(), String> {
        if self.completed.get() {
            return Ok(());
        }
        let mut world = self.world.w();
        if !world
            .generated_epoch_tokens
            .get(&self.view)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("Synthetic generated epoch was replaced".into());
        }
        world.generated_epochs.remove(&self.view);
        world.generated_epoch_tokens.remove(&self.view);
        if restore && let Some((epoch, token)) = self.previous.borrow_mut().take() {
            world.generated_epochs.insert(self.view, epoch);
            if let Some(token) = token {
                world.generated_epoch_tokens.insert(self.view, token);
            }
        }
        self.completed.set(true);
        Ok(())
    }
    fn validate(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if receipt.matches(&self.reservation, self.view, &self.created) {
            Ok(())
        } else {
            Err("Synthetic generated epoch closure mismatch".into())
        }
    }
}

impl super::host::EditorSeedRollback for ProbeEpochRollback {
    fn retire_closed(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(self.validate(&receipt));
    }
    fn retire_adopted(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(self.validate(&receipt).and_then(|()| {
            if receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
                return Err("An adopted epoch requires a closed browser".into());
            }
            self.remove(false)
        }));
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !proof.matches(&self.reservation, &self.owner) {
            return done(Err("Synthetic epoch replacement mismatch".into()));
        }
        self.completed.set(true);
        done(Ok(()));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.remove(true));
    }
}

fn fixture_install_epoch(
    fake: &Fake,
    view: ViewId,
    created: &shell_contract::runtime::RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    owner: &shell_contract::ipc::NativeBrowserOwner,
    rollback: &super::host::EditorSeedRollbackStore,
) -> Arc<()> {
    let token = Arc::new(());
    let previous = {
        let mut world = fake.w();
        let prior = world.generated_epochs.insert(view, created.clone());
        let prior_token = world.generated_epoch_tokens.insert(view, token.clone());
        prior.map(|epoch| (epoch, prior_token))
    };
    rollback.borrow_mut().push(Rc::new(ProbeEpochRollback {
        world: fake.clone(),
        view,
        created: created.clone(),
        reservation: reservation.clone(),
        owner: owner.clone(),
        token: token.clone(),
        previous: std::cell::RefCell::new(previous),
        completed: std::cell::Cell::new(false),
    }));
    token
}

struct ProbeSpareState {
    owner: shell_contract::ipc::NativeBrowserOwner,
    closing: bool,
}

#[derive(Clone)]
struct ProbeSpareActor {
    fake: Fake,
    view: ViewId,
    reservation: shell_contract::ipc::NativeClosureReservation,
    epoch: shell_contract::runtime::RendererGeneration,
    token: Arc<()>,
    owner: Rc<std::cell::RefCell<Option<shell_contract::ipc::NativeBrowserOwner>>>,
    state: Rc<std::cell::RefCell<ProbeSpareState>>,
}

impl ProbeSpareActor {
    fn validate(
        &self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
    ) -> Result<(), String> {
        let held = self.owner.borrow();
        let owner = held
            .as_ref()
            .ok_or("The synthetic spare owner is not committed")?;
        let state = self.state.borrow();
        if state.closing
            || !state.owner.same_owner(owner)
            || !transition.matches(&self.reservation)
            || !transition.matches_owner(owner)
        {
            return Err("The synthetic spare actor owner changed".into());
        }
        let world = self.fake.w();
        if !self.epoch.is_active()
            || !world
                .generated_epochs
                .get(&self.view)
                .is_some_and(|epoch| epoch.same_generation(&self.epoch))
            || !world
                .generated_epoch_tokens
                .get(&self.view)
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("The synthetic spare actor registration changed".into());
        }
        Ok(())
    }
}

struct ProbeSpareReceipt {
    actor: Rc<ProbeSpareActor>,
    completed: std::cell::Cell<bool>,
}

impl super::host::EditorSeedRollback for ProbeSpareReceipt {
    fn retire_closed(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let result = if receipt.matches(&self.actor.reservation, self.actor.view, &self.actor.epoch)
        {
            Ok(())
        } else {
            Err("The synthetic spare close belongs to another reservation".into())
        };
        done(result);
    }

    fn retire_adopted(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let result = (|| {
            if !receipt.matches(&self.actor.reservation, self.actor.view, &self.actor.epoch)
                || receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
            {
                return Err("The synthetic adopted spare has another close receipt".into());
            }
            if self.completed.get() {
                return Ok(());
            }
            let held = self.actor.owner.borrow();
            let owner = held
                .as_ref()
                .ok_or("The synthetic spare owner is not committed")?;
            if !self.actor.state.borrow().owner.same_owner(owner) {
                return Err("The synthetic spare owner was replaced".into());
            }
            let mut world = self.actor.fake.w();
            if !world
                .generated_epoch_tokens
                .get(&self.actor.view)
                .is_some_and(|token| Arc::ptr_eq(token, &self.actor.token))
            {
                return Err("The synthetic spare registration was replaced".into());
            }
            world.generated_epoch_tokens.remove(&self.actor.view);
            world.generated_epochs.remove(&self.actor.view);
            self.completed.set(true);
            Ok(())
        })();
        done(result);
    }

    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let result = (|| {
            let held = self.actor.owner.borrow();
            let owner = held
                .as_ref()
                .ok_or("The synthetic spare owner is not committed")?;
            if !proof.matches(&self.actor.reservation, owner)
                || !proof.successor_matches(&self.actor.state.borrow().owner)
            {
                return Err("The synthetic spare replacement does not match".into());
            }
            self.completed.set(true);
            Ok(())
        })();
        done(result);
    }

    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        let uncommitted = self.actor.owner.borrow().is_none();
        if uncommitted {
            self.completed.set(true);
            done(Ok(()));
        } else {
            done(Err(
                "An adopted synthetic spare cannot be rolled back".into()
            ));
        }
    }
}

struct ProbeSpareClosePlan<'a>(&'a ProbeSpareActor);

impl super::host::SpareClosePlan for ProbeSpareClosePlan<'_> {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.0.validate(transition)?;
        next()?;
        self.0.state.borrow_mut().closing = true;
        Ok(())
    }
}

struct ProbeSpareTransferPlan {
    original: ProbeSpareActor,
    successor: Rc<ProbeSpareActor>,
    receipt: Rc<ProbeSpareReceipt>,
}

impl super::host::SpareTransferPlan for ProbeSpareTransferPlan {
    fn successor(&self) -> Rc<dyn super::host::SpareActor> {
        self.successor.clone()
    }
    fn successor_receipt(&self) -> Rc<dyn super::host::EditorSeedRollback> {
        self.receipt.clone()
    }

    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        successor: &shell_contract::ipc::NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.receipt.completed.get() {
            return Err("The synthetic spare transfer was cancelled".into());
        }
        self.original.validate(transition)?;
        if !transition.proposes_owner(successor) {
            return Err("The synthetic spare successor changed".into());
        }
        next()?;
        self.original.state.borrow_mut().owner = successor.clone();
        *self.successor.owner.borrow_mut() = Some(successor.clone());
        Ok(())
    }
}
struct ProbeSpareSavedBarrier {
    original: ProbeSpareActor,
    target: shell_contract::ipc::NativeSavedTarget,
    token: Arc<()>,
    committed: bool,
}
impl super::host::SpareSavedBarrier for ProbeSpareSavedBarrier {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.original.validate(transition)?;
        if self.committed
            || !transition.proposes_owner(self.target.browser())
            || !transition.matches(self.target.reservation())
        {
            return Err("The synthetic saved spare transition changed".into());
        }
        if self
            .original
            .fake
            .w()
            .saved_barriers
            .contains_key(&self.original.view)
        {
            return Err("The synthetic saved spare already has a barrier".into());
        }
        next()?;
        self.original.fake.w().saved_barriers.insert(
            self.original.view,
            (self.target.clone(), self.token.clone()),
        );
        self.committed = true;
        Ok(())
    }
}

impl super::host::SpareActor for ProbeSpareActor {
    fn prepare_close(&self) -> Result<Box<dyn super::host::SpareClosePlan + '_>, String> {
        Ok(Box::new(ProbeSpareClosePlan(self)))
    }

    fn prepare_transfer(&self) -> Result<Box<dyn super::host::SpareTransferPlan>, String> {
        let successor = Rc::new(ProbeSpareActor {
            fake: self.fake.clone(),
            view: self.view,
            reservation: self.reservation.clone(),
            epoch: self.epoch.clone(),
            token: self.token.clone(),
            owner: Rc::new(std::cell::RefCell::new(None)),
            state: self.state.clone(),
        });
        let receipt = Rc::new(ProbeSpareReceipt {
            actor: successor.clone(),
            completed: std::cell::Cell::new(false),
        });
        Ok(Box::new(ProbeSpareTransferPlan {
            original: self.clone(),
            successor,
            receipt,
        }))
    }
    fn prepare_saved_barrier(
        &self,
        target: &shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn super::host::SpareSavedBarrier>, String> {
        if target.surface() != shell_contract::Surface::Sheets
            || target.view() != self.view
            || !target.created().same_generation(&self.epoch)
            || !target.reservation().same_reservation(&self.reservation)
        {
            return Err("The synthetic saved spare target does not match".into());
        }
        let mut world = self.fake.w();
        if world.saved_barriers.contains_key(&self.view) {
            return Err("The synthetic saved spare already has a barrier".into());
        }
        world
            .saved_barriers
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        drop(world);
        Ok(Box::new(ProbeSpareSavedBarrier {
            original: self.clone(),
            target: target.clone(),
            token: Arc::new(()),
            committed: false,
        }))
    }
}

struct ProbeSeedRollback {
    world: Fake,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    token: Arc<()>,
    displaced:
        std::cell::RefCell<Option<(main_core::shell::tab_manager::EditorSeed, Option<Arc<()>>)>>,
    completed: std::cell::Cell<bool>,
}

impl ProbeSeedRollback {
    fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if proof.matches(&self.reservation, self.view, &self.created) {
            Ok(())
        } else {
            Err("Synthetic seed closure mismatch".into())
        }
    }
    fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        self.retire_closed(proof)?;
        if proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return Err("An adopted seed requires a closed browser".into());
        }
        if self.completed.get() {
            return Ok(());
        }
        let mut world = self.world.w();
        if !world
            .generated_tokens
            .get(&self.view)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("Synthetic editor seed was replaced".into());
        }
        world.generated.remove(&self.view);
        world.generated_tokens.remove(&self.view);
        self.completed.set(true);
        Ok(())
    }
    fn rollback(&self) -> Result<(), String> {
        if self.completed.get() {
            return Ok(());
        }
        let mut world = self.world.w();
        if !world
            .generated_tokens
            .get(&self.view)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("Synthetic editor seed was replaced".into());
        }
        world.generated.remove(&self.view);
        world.generated_tokens.remove(&self.view);
        if let Some((seed, token)) = self.displaced.borrow_mut().take() {
            world.generated.insert(self.view, seed);
            if let Some(token) = token {
                world.generated_tokens.insert(self.view, token);
            }
        }
        self.completed.set(true);
        Ok(())
    }
}
impl super::host::EditorSeedRollback for ProbeSeedRollback {
    fn retire_closed(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(ProbeSeedRollback::retire_closed(self, &proof));
    }
    fn retire_adopted(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(ProbeSeedRollback::retire_adopted(self, &proof));
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !proof.matches(&self.reservation, &self.owner) {
            return done(Err("Synthetic seed replacement mismatch".into()));
        }
        if !self.completed.get() {
            let mut world = self.world.w();
            if world
                .generated_tokens
                .get(&self.view)
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
            {
                world.generated.remove(&self.view);
                world.generated_tokens.remove(&self.view);
            }
            self.displaced.borrow_mut().take();
            self.completed.set(true);
        }
        done(Ok(()));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(ProbeSeedRollback::rollback(self));
    }
}

struct ProbePlacementRollback {
    world: Fake,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    token: Arc<()>,
    #[allow(clippy::type_complexity)]
    displaced: std::cell::RefCell<
        Option<(
            Option<main_core::shell::projects::new_documents::NewDocumentPlacement>,
            Option<Arc<()>>,
        )>,
    >,
    completed: std::cell::Cell<bool>,
}
impl ProbePlacementRollback {
    fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if proof.matches(&self.reservation, self.view, &self.created) {
            Ok(())
        } else {
            Err("Synthetic placement closure mismatch".into())
        }
    }
    fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        self.retire_closed(proof)?;
        if proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return Err("An adopted placement requires a closed browser".into());
        }
        if self.completed.get() {
            return Ok(());
        }
        let mut world = self.world.w();
        if !world
            .generated_placement_tokens
            .get(&self.view)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("Synthetic editor placement was replaced".into());
        }
        world.generated_placements.remove(&self.view);
        world.generated_placement_tokens.remove(&self.view);
        self.completed.set(true);
        Ok(())
    }
    fn rollback(&self) -> Result<(), String> {
        if self.completed.get() {
            return Ok(());
        }
        let mut world = self.world.w();
        if !world
            .generated_placement_tokens
            .get(&self.view)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            return Err("Synthetic editor placement was replaced".into());
        }
        world.generated_placements.remove(&self.view);
        world.generated_placement_tokens.remove(&self.view);
        if let Some((placement, token)) = self.displaced.borrow_mut().take() {
            world.generated_placements.insert(self.view, placement);
            if let Some(token) = token {
                world.generated_placement_tokens.insert(self.view, token);
            }
        }
        self.completed.set(true);
        Ok(())
    }
}
impl super::host::EditorSeedRollback for ProbePlacementRollback {
    fn retire_closed(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(ProbePlacementRollback::retire_closed(self, &proof));
    }
    fn retire_adopted(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done(ProbePlacementRollback::retire_adopted(self, &proof));
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !proof.matches(&self.reservation, &self.owner) {
            return done(Err("Synthetic placement replacement mismatch".into()));
        }
        if !self.completed.get() {
            let mut world = self.world.w();
            if world
                .generated_placement_tokens
                .get(&self.view)
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
            {
                world.generated_placements.remove(&self.view);
                world.generated_placement_tokens.remove(&self.view);
            }
            self.displaced.borrow_mut().take();
            self.completed.set(true);
        }
        done(Ok(()));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(ProbePlacementRollback::rollback(self));
    }
}

struct ProbePreparedSeed {
    fake: Fake,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    seed: Option<main_core::shell::tab_manager::EditorSeed>,
    placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
    expected_seed: Option<Arc<()>>,
    expected_placement: Option<Arc<()>>,
    expected_seed_present: bool,
    expected_placement_present: bool,
    seed_token: Option<Arc<()>>,
    placement_token: Option<Arc<()>>,
    previous_seed: Option<(main_core::shell::tab_manager::EditorSeed, Option<Arc<()>>)>,
    previous_placement: Option<(
        Option<main_core::shell::projects::new_documents::NewDocumentPlacement>,
        Option<Arc<()>>,
    )>,
    owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    rollback: super::host::EditorSeedRollbackStore,
    logs: Vec<Vec<Value>>,
    nudge_target: Option<shell_contract::ipc::NativeRequestOwner>,
    committed: bool,
}

fn probe_token_matches(current: Option<&Arc<()>>, expected: &Option<Arc<()>>) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_probe_seed(
    fake: &Fake,
    view: ViewId,
    created: &shell_contract::runtime::RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    seed: Option<main_core::shell::tab_manager::EditorSeed>,
    placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
    docs_open_path: Option<String>,
    workbook: Option<String>,
    rollback: &super::host::EditorSeedRollbackStore,
) -> TabResult<ProbePreparedSeed> {
    let mut logs = Vec::new();
    if let Some(path) = docs_open_path {
        logs.push(vec![json!("queueDocsOpen"), json!(view.0), json!(path)]);
    }
    if let Some(path) = workbook {
        logs.push(vec![
            json!("queueWorkbookForView"),
            json!(view.0),
            json!(path),
        ]);
    }
    if let Some(seed) = &seed {
        logs.push(match seed {
            main_core::shell::tab_manager::EditorSeed::Generate(value) => vec![
                json!("queueGenerateHandoffFor"),
                json!(view.0),
                value.clone(),
            ],
            main_core::shell::tab_manager::EditorSeed::Conversion(value) => {
                vec![json!("queueConvertedContent"), json!(view.0), value.clone()]
            }
            main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content } => vec![
                json!("queueDocsAiSeed"),
                json!(view.0),
                json!(new_blank),
                json!(content),
            ],
        });
    }
    let mut world = fake.w();
    world
        .generated
        .try_reserve(1)
        .map_err(|error| TabError(error.to_string()))?;
    world
        .generated_tokens
        .try_reserve(1)
        .map_err(|error| TabError(error.to_string()))?;
    world
        .generated_placements
        .try_reserve(1)
        .map_err(|error| TabError(error.to_string()))?;
    world
        .generated_placement_tokens
        .try_reserve(1)
        .map_err(|error| TabError(error.to_string()))?;
    rollback
        .borrow_mut()
        .try_reserve(2)
        .map_err(|error| TabError(error.to_string()))?;
    Ok(ProbePreparedSeed {
        fake: fake.clone(),
        view,
        created: created.clone(),
        reservation: reservation.clone(),
        expected_seed: world.generated_tokens.get(&view).cloned(),
        expected_placement: world.generated_placement_tokens.get(&view).cloned(),
        expected_seed_present: world.generated.contains_key(&view),
        expected_placement_present: world.generated_placements.contains_key(&view),
        seed_token: seed.as_ref().map(|_| Arc::new(())),
        placement_token: placement.as_ref().map(|_| Arc::new(())),
        seed,
        placement,
        previous_seed: None,
        previous_placement: None,
        owner: None,
        rollback: rollback.clone(),
        logs,
        nudge_target: None,
        committed: false,
    })
}

impl super::host::SpareSeedPlan for ProbePreparedSeed {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        successor: &shell_contract::ipc::NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.committed
            || !transition.matches(&self.reservation)
            || !(transition.matches_owner(successor) || transition.proposes_owner(successor))
        {
            return Err("Synthetic seed transition changed".into());
        }
        {
            let world = self.fake.w();
            if world.generated.contains_key(&self.view) != self.expected_seed_present
                || world.generated_placements.contains_key(&self.view)
                    != self.expected_placement_present
                || !probe_token_matches(world.generated_tokens.get(&self.view), &self.expected_seed)
                || !probe_token_matches(
                    world.generated_placement_tokens.get(&self.view),
                    &self.expected_placement,
                )
            {
                return Err("Synthetic seed state changed".into());
            }
        }
        next()?;
        let mut world = self.fake.w();
        if let Some(seed) = self.seed.take() {
            let previous = world.generated.insert(self.view, seed);
            let previous_token = world.generated_tokens.insert(
                self.view,
                self.seed_token
                    .as_ref()
                    .expect("prepared seed token")
                    .clone(),
            );
            self.previous_seed = previous.map(|seed| (seed, previous_token));
        }
        if let Some(placement) = self.placement.take() {
            let previous = world.generated_placements.insert(self.view, placement);
            let previous_token = world.generated_placement_tokens.insert(
                self.view,
                self.placement_token
                    .as_ref()
                    .expect("prepared placement token")
                    .clone(),
            );
            self.previous_placement = previous.map(|placement| (placement, previous_token));
        }
        self.owner = Some(successor.clone());
        self.committed = true;
        Ok(())
    }
    fn after_commit(
        &mut self,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<(), String> {
        let owner = self
            .owner
            .take()
            .ok_or("Synthetic seed was not committed")?;
        if let Some(token) = self.seed_token.take() {
            self.rollback.borrow_mut().push(Rc::new(ProbeSeedRollback {
                world: self.fake.clone(),
                view: self.view,
                created: self.created.clone(),
                reservation: self.reservation.clone(),
                owner: owner.clone(),
                token,
                displaced: std::cell::RefCell::new(self.previous_seed.take()),
                completed: std::cell::Cell::new(false),
            }));
        }
        if let Some(token) = self.placement_token.take() {
            self.rollback
                .borrow_mut()
                .push(Rc::new(ProbePlacementRollback {
                    world: self.fake.clone(),
                    view: self.view,
                    created: self.created.clone(),
                    reservation: self.reservation.clone(),
                    owner,
                    token,
                    displaced: std::cell::RefCell::new(self.previous_placement.take()),
                    completed: std::cell::Cell::new(false),
                }));
        }
        if cancel.is_cancelled() {
            return Err("Synthetic seed request was cancelled".into());
        }
        {
            let world = self.fake.w();
            for line in self.logs.drain(..) {
                world.log(line);
            }
        }
        if let Some(target) = self.nudge_target.take() {
            shell_contract::hosts::synthetic::admit_many(&target, &[], cancel, cancel, || {
                self.fake
                    .w()
                    .log(vec![json!("nudgeQueuedWorkbook"), json!(self.view.0)]);
            })?;
        }
        Ok(())
    }
}

struct ProbeSupersededDrain {
    state: Rc<std::cell::RefCell<ProbeSupersededState>>,
}
struct ProbeSupersededState {
    receipts: Vec<Rc<dyn super::host::EditorSeedRollback>>,
    proof: Option<shell_contract::ipc::NativeBrowserSuperseded>,
    running: bool,
}
impl ProbeSupersededDrain {
    fn begin(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        let ready = {
            let mut state = self.state.borrow_mut();
            if state.running {
                Err("Synthetic superseded cleanup is already running".to_owned())
            } else if state.proof.is_none() {
                Err("Synthetic superseded proof is unavailable".to_owned())
            } else {
                state.running = true;
                Ok(())
            }
        };
        if let Err(error) = ready {
            return done(Err(error));
        }
        Self::advance(self.state.clone(), done);
    }
    fn advance(
        state: Rc<std::cell::RefCell<ProbeSupersededState>>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let next = {
            let mut current = state.borrow_mut();
            match current.receipts.last().cloned() {
                Some(receipt) => Some((
                    receipt,
                    current
                        .proof
                        .as_ref()
                        .expect("installed superseded proof")
                        .clone(),
                )),
                None => {
                    current.running = false;
                    None
                }
            }
        };
        let Some((receipt, proof)) = next else {
            return done(Ok(()));
        };
        receipt.retire_superseded(
            proof,
            Box::new(move |result| {
                if let Err(error) = result {
                    state.borrow_mut().running = false;
                    return done(Err(error));
                }
                state.borrow_mut().receipts.pop();
                Self::advance(state, done);
            }),
        );
    }
}
impl super::host::EditorSeedRollback for ProbeSupersededDrain {
    fn retire_closed(
        &self,
        _: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.begin(done);
    }
    fn retire_adopted(
        &self,
        _: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.begin(done);
    }
    fn retire_superseded(
        &self,
        _: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.begin(done);
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.begin(done);
    }
}

struct Hooks(Fake, Services);

struct ProbeGeneratedRetryPrompt {
    fake: Fake,
    services: Services,
    shell: ShellWindow,
    window: Arc<FixtureWindowProof>,
    output: shell_contract::ipc::SavedPublication,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
}

impl super::host::CapturedGeneratedRetryPrompt for ProbeGeneratedRetryPrompt {
    fn show(
        &self,
        error: String,
        done: Box<dyn FnOnce(Result<super::host::GeneratedRetryChoice, String>)>,
    ) {
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Synthetic retry authority unavailable".into()));
        };
        let fake = self.fake.clone();
        let output = self.output.clone();
        let participants = self.participants.clone();
        let cancel = self.cancel.clone();
        let pending = self.pending.clone();
        let additional = self.additional.clone();
        let ui_effects = effects.clone();
        let ui_output = output.clone();
        let ui_participants = participants.clone();
        let ui_cancel = cancel.clone();
        let ui_pending = pending.clone();
        let ui_additional = additional.clone();
        let shell = self.shell;
        let window = self.window.clone();
        let reply = crate::home_files::back_to_core(
            self.services.core.clone(),
            move |answer: Result<String, String>| {
                let extra: Vec<_> = additional.iter().collect();
                let admitted = output.with_effect(
                    effects.as_ref(),
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || Ok(()),
                );
                done(admitted.and_then(|()| match answer?.as_str() {
                    "retry" => Ok(super::host::GeneratedRetryChoice::Retry),
                    "cancel" => Ok(super::host::GeneratedRetryChoice::Cancel),
                    _ => Err("Unknown synthetic retry prompt choice".into()),
                }));
            },
        );
        self.services.ui.post(Box::new(move || {
            let mut answer = None;
            let extra: Vec<_> = ui_additional.iter().collect();
            let admitted = ui_output.with_effect(
                ui_effects.as_ref(),
                &ui_participants,
                &ui_cancel,
                &ui_pending,
                &extra,
                &mut || {
                    let mut world = fake.w();
                    if !world.windows.get(&shell.window.0).is_some_and(|current| {
                        !current.destroyed && Arc::ptr_eq(&current.proof, &window)
                    }) {
                        return Err("The synthetic retry parent was replaced or closed".into());
                    }
                    world.log(vec![json!("generatedRetryPrompt"), json!(error)]);
                    answer = Some(match world.generated_retry_answers.pop_front() {
                        Some(Value::String(answer)) if answer == "retry" || answer == "cancel" => {
                            Ok(answer)
                        }
                        Some(Value::String(answer)) if answer == "throws" => {
                            Err("Synthetic retry prompt failed".into())
                        }
                        _ => Err("Synthetic retry prompt requires a scripted choice".into()),
                    });
                    Ok(())
                },
            );
            let answer = admitted.and_then(|()| {
                answer.ok_or_else(|| "Synthetic retry prompt was not admitted".to_owned())?
            });
            reply(answer);
        }));
    }
}

struct ProbeUnstartedSaved {
    fake: Fake,
    view: ViewId,
    closure: Arc<FixtureClosure>,
    armed: bool,
}

impl Drop for ProbeUnstartedSaved {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let removed = {
            let mut world = self.fake.w();
            if world
                .closures
                .get(&self.view)
                .is_some_and(|current| Arc::ptr_eq(current, &self.closure))
                && !world.kinds.contains_key(&self.view.0)
            {
                world.closures.remove(&self.view);
                world.views.remove(&self.view.0);
                true
            } else {
                false
            }
        };
        if removed {
            self.closure.close();
        }
    }
}

struct ProbeCapturedSavedShell {
    fake: Fake,
    services: Services,
    shell: ShellWindow,
    owner: shell_contract::ipc::NativeRequestOwner,
    window: Arc<FixtureWindowProof>,
}

impl super::host::CapturedSavedShell for ProbeCapturedSavedShell {
    fn prepare_fresh(
        &self,
        spec: &ViewSpec<'_>,
        input: shell_contract::ipc::NativeSavedInput,
        path: shell_contract::ipc::NativeSavedPath,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        rollback: super::host::EditorSeedRollbackStore,
    ) -> TabResult<super::host::PreparedSavedView> {
        let effects = self
            .services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .cloned()
            .ok_or_else(|| TabError("Synthetic saved authority unavailable".into()))?;
        if self.owner.view != self.shell.view
            || !self
                .owner
                .context
                .renderer()
                .is_some_and(|generation| generation.is_active())
        {
            return Err(TabError("Synthetic saved shell was replaced".into()));
        }
        {
            let world = self.fake.w();
            if !world
                .windows
                .get(&self.shell.window.0)
                .is_some_and(|window| !window.destroyed && Arc::ptr_eq(&window.proof, &self.window))
            {
                return Err(TabError("Synthetic saved shell window was replaced".into()));
            }
        }
        participants.push(self.owner.clone());
        let (name, args, kind) = create_call(spec);
        let view = self.fake.w().make_view();
        let generation = shell_contract::runtime::RendererGeneration::new();
        let closure = fixture_reserve(&self.fake, view, generation.clone());
        let mut unstarted = ProbeUnstartedSaved {
            fake: self.fake.clone(),
            view,
            closure: closure.clone(),
            armed: true,
        };
        let target = shell_contract::ipc::NativeSavedTarget::prepare(
            input,
            path,
            fixture_surface(kind),
            view,
            generation.clone(),
            closure.reservation.clone(),
            closure.owner.clone(),
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            additional.clone(),
        )
        .map_err(TabError)?;
        let actor = Rc::new(Editors(self.fake.clone(), fixture_surface(kind)));
        let mut prepared = actor
            .prepare_saved_document(target.clone())
            .map_err(|error| TabError(error.message))?;
        rollback
            .borrow_mut()
            .try_reserve(1)
            .map_err(|error| TabError(error.to_string()))?;
        let ready = fixture_ready(&self.fake, view, view);
        let binding = ready.reservation.clone();
        let start_target = target.clone();
        let fake = self.fake.clone();
        let start = Box::new(move |adopt: super::host::SavedManagerAdoption,
            registration: super::host::SavedManagerRegistration,
            _cleanup: ViewCreationCleanup,
            done: Box<dyn FnOnce(ViewCreationResult<super::host::SavedViewLoaded>)>| {
            let unstarted = &mut unstarted;
            unstarted.armed = false;
            let extra: Vec<_> = additional.iter().collect();
            let mut adopt = Some(adopt);
            let result = effects.with_saved_target_creation(&start_target, &participants, &cancel, &pending, &extra,
                &mut |transition| {
                    let mut commit_actor = |next: &mut dyn FnMut() -> Result<(), String>| {
                        prepared.with_validated(transition, &mut || next().map_err(probe_saved_error))
                            .map_err(|error| error.message)
                    };
                    adopt.take().ok_or("Synthetic saved adoption was already consumed")?(&start_target, transition, &mut commit_actor)
                });
            let receipt = prepared.finish();
            rollback.borrow_mut().push(Rc::new(ProbeSavedActorRollback(receipt)));
            let fail = |error: String, done: Box<dyn FnOnce(ViewCreationResult<super::host::SavedViewLoaded>)>| {
                done(Err(ViewCreationFailure { error: TabError(error), cleanup: ViewCreationCleanupState::Pending(ready.cleanup.clone()) }));
            };
            if let Err(error) = result { return fail(error, done); }
            {
                let world = fake.w();
                let mut line = vec![Value::from(name)];
                line.extend(args);
                world.log(line);
                if world.case["fail"].as_array().is_some_and(|fail| fail.iter().any(|item| item.as_str() == Some(name))) {
                    drop(world);
                    return fail("Synthetic saved view construction failed".into(), done);
                }
            }
            let owner = match fixture_register_owner(&fake, view, &generation, fixture_surface(kind)) {
                Ok(owner) => owner,
                Err(error) => return fail(error, done),
            };
            let mut document = None;
            let mut registration = Some(registration);
            let result = effects.with_saved_target_registration(&start_target, &owner, &participants, &cancel, &pending, &extra,
                &mut |transition| {
                    let mut register = || {
                        document = Some(start_target.register_from_native_admitted(owner.clone())?);
                        Ok(())
                    };
                    registration.take().ok_or("Synthetic saved registration was already consumed")?(&start_target, &owner, transition, &mut register)
                });
            if let Err(error) = result { return fail(error, done); }
            let Some(document) = document else { return fail("Synthetic saved registration did not commit".into(), done); };
            let mut binding = ready.reservation;
            binding.registration = super::host::NativeBrowserRegistration::Registered(owner);
            let saved_document = document.clone();
            actor.load_saved_document(document, Box::new(move |outcome| match outcome {
                Ok(outcome) => done(Ok(ViewCreationReady {
                    view: super::host::SavedViewLoaded { document: saved_document, outcome },
                    cleanup: ready.cleanup, closure: ready.closure, reservation: binding,
                })),
                Err(error) => done(Err(ViewCreationFailure { error: TabError(error.message), cleanup: ViewCreationCleanupState::Pending(ready.cleanup) })),
            }));
        });
        Ok(super::host::PreparedSavedView {
            target,
            binding,
            spare_actor: None,
            start,
        })
    }

    fn prepare_spare(
        &self,
        spare: &super::host::RetainedSpareBinding,
        input: shell_contract::ipc::NativeSavedInput,
        path: shell_contract::ipc::NativeSavedPath,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        rollback: super::host::EditorSeedRollbackStore,
    ) -> TabResult<super::host::PreparedSavedView> {
        let effects = self
            .services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .cloned()
            .ok_or_else(|| TabError("Synthetic saved authority unavailable".into()))?;
        if self.owner.view != self.shell.view
            || !self
                .owner
                .context
                .renderer()
                .is_some_and(|generation| generation.is_active())
        {
            return Err(TabError("Synthetic saved shell was replaced".into()));
        }
        let super::host::NativeBrowserRegistration::Registered(original) =
            &spare.browser.registration
        else {
            return Err(TabError("Synthetic saved spare is not registered".into()));
        };
        let view = original.view;
        let expected = original
            .context
            .renderer()
            .cloned()
            .ok_or_else(|| TabError("Synthetic saved spare renderer unavailable".into()))?;
        let closure = self
            .fake
            .w()
            .closures
            .get(&view)
            .cloned()
            .ok_or_else(|| TabError("Synthetic saved spare reservation unavailable".into()))?;
        if !closure.reservation.matches(view, &expected)
            || !closure
                .reservation
                .same_reservation(&spare.browser.reservation)
        {
            return Err(TabError("Synthetic saved spare reservation changed".into()));
        }
        let actor = spare
            .spare_actor
            .as_ref()
            .ok_or_else(|| TabError("Synthetic saved spare actor unavailable".into()))?;
        participants.push(self.owner.clone());
        participants.push(original.clone());
        let target = shell_contract::ipc::NativeSavedTarget::prepare(
            input,
            path,
            shell_contract::Surface::Sheets,
            view,
            expected.clone(),
            closure.reservation.clone(),
            spare.browser.owner.clone(),
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            additional.clone(),
        )
        .map_err(TabError)?;
        let replacement = closure
            .reservation
            .prepare_replacement(&spare.browser.owner)
            .map_err(TabError)?;
        let successor_target = target.prepare_successor(&replacement).map_err(TabError)?;
        let mut transfer = actor.prepare_transfer().map_err(TabError)?;
        let successor_actor = transfer.successor();
        let successor_receipt = transfer.successor_receipt();
        let mut barrier = actor
            .prepare_saved_barrier(&successor_target)
            .map_err(TabError)?;
        rollback
            .borrow_mut()
            .try_reserve(2)
            .map_err(|error| TabError(error.to_string()))?;
        let prior = Rc::new(ProbeSupersededDrain {
            state: Rc::new(std::cell::RefCell::new(ProbeSupersededState {
                receipts: spare.rollback.borrow().clone(),
                proof: None,
                running: false,
            })),
        });
        let mut proposed = spare.browser.clone();
        proposed.owner = replacement.successor().clone();
        let editors = Rc::new(Editors(self.fake.clone(), shell_contract::Surface::Sheets));
        let source = self.owner.clone();
        let original = original.clone();
        let start_target = successor_target.clone();
        let ready_binding = proposed.clone();
        let start = Box::new(
            move |adopt: super::host::SavedManagerAdoption,
                  registration: super::host::SavedManagerRegistration,
                  cleanup: ViewCreationCleanup,
                  done: Box<dyn FnOnce(ViewCreationResult<super::host::SavedViewLoaded>)>| {
                let extra: Vec<_> = additional.iter().collect();
                let mut adopt = Some(adopt);
                let committed = synthetic_created_effect(
                    &source,
                    &participants,
                    &expected,
                    &cancel,
                    &pending,
                    || {
                        replacement.commit(|transition, next_owner| {
                            let adoption = start_target.validate_in_transition(transition)?;
                            let mut actor = |next: &mut dyn FnMut() -> Result<(), String>| {
                                transfer.with_validated(transition, next_owner, &mut || {
                                    barrier.with_validated(transition, next)
                                })
                            };
                            adopt
                                .take()
                                .ok_or("Synthetic saved spare adoption was consumed")?(
                                &start_target,
                                transition,
                                &mut actor,
                            )?;
                            adoption.commit();
                            Ok(())
                        })
                    },
                )
                .and_then(|result| result.map_err(TabError));
                let (_, superseded, ()) = match committed {
                    Ok(committed) => committed,
                    Err(error) => {
                        return done(Err(ViewCreationFailure {
                            error,
                            cleanup: ViewCreationCleanupState::Complete,
                        }));
                    }
                };
                prior.state.borrow_mut().proof = Some(superseded);
                rollback.borrow_mut().push(prior.clone());
                rollback.borrow_mut().push(successor_receipt);
                let fail = |error: String,
                            cleanup: ViewCreationCleanup,
                            done: Box<
                    dyn FnOnce(ViewCreationResult<super::host::SavedViewLoaded>),
                >| {
                    done(Err(ViewCreationFailure {
                        error: TabError(error),
                        cleanup: ViewCreationCleanupState::Pending(cleanup),
                    }));
                };
                let mut document = None;
                let mut registration = Some(registration);
                let registered = effects.with_saved_target_registration(
                    &start_target,
                    &original,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut |transition| {
                        let mut register = || {
                            document =
                                Some(start_target.register_from_native_admitted(original.clone())?);
                            Ok(())
                        };
                        registration
                            .take()
                            .ok_or("Synthetic saved spare registration was consumed")?(
                            &start_target,
                            &original,
                            transition,
                            &mut register,
                        )
                    },
                );
                if let Err(error) = registered {
                    return fail(error, cleanup, done);
                }
                let Some(document) = document else {
                    return fail(
                        "Synthetic saved spare registration did not commit".into(),
                        cleanup,
                        done,
                    );
                };
                let saved_document = document.clone();
                editors.load_saved_document(
                    document,
                    Box::new(move |outcome| match outcome {
                        Ok(outcome) => prior.begin(Box::new(move |retired| match retired {
                            Ok(()) => done(Ok(ViewCreationReady {
                                view: super::host::SavedViewLoaded {
                                    document: saved_document,
                                    outcome,
                                },
                                cleanup,
                                closure: ready_binding.closure.clone(),
                                reservation: super::host::NativeBrowserBinding {
                                    registration: super::host::NativeBrowserRegistration::Registered(
                                        original,
                                    ),
                                    ..ready_binding
                                },
                            })),
                            Err(error) => fail(error, cleanup, done),
                        })),
                        Err(error) => fail(error.message, cleanup, done),
                    }),
                );
            },
        );
        Ok(super::host::PreparedSavedView {
            target: successor_target,
            binding: proposed,
            spare_actor: Some(successor_actor),
            start,
        })
    }
}

struct ProbeDocsDisposal {
    fake: Fake,
    core: Arc<dyn shell_contract::runtime::Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    origin: shell_contract::ipc::NativeRequestOwner,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    target: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    running: std::cell::Cell<bool>,
    closed: std::cell::Cell<bool>,
}

impl super::activation::DocsDisposalOperation for ProbeDocsDisposal {
    fn run(self: Rc<Self>, done: Box<dyn FnOnce(Result<super::activation::DocsDisposal, String>)>) {
        if self.closed.get() || self.running.replace(true) {
            return done(Err("synthetic disposal is already running or closed".into()));
        }
        let view = self.target.view;
        let generation = self
            .target
            .context
            .renderer()
            .expect("captured disposal renderer")
            .clone();
        let fake = self.fake.clone();
        let completion = self.clone();
        let reply = crate::home_files::back_to_core(
            self.core.clone(),
            move |result: Result<(), String>| {
                completion.running.set(false);
                if result.is_ok() {
                    completion.closed.set(true);
                }
                done(result.and_then(|()| {
                    super::activation::DocsDisposal::closed(completion.target.clone())
                }));
            },
        );
        self.effects.post_ui_owned_many(
            self.origin.clone(),
            self.participants.clone(),
            self.cancel.clone(),
            self.pending.clone(),
            Box::new(move || {
                let mut world = fake.w();
                if world.view(view.0).destroyed {
                    return Err("synthetic disposal target was already closed".into());
                }
                if world.case["fail"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item.as_str() == Some("disposeDocs"))
                }) {
                    return Err("synthetic document disposal failed".into());
                }
                world.log(vec![json!("teardownDocsRenderer"), json!(view.0)]);
                world.log(vec![json!("close"), json!(view.0)]);
                world.view(view.0).destroyed = true;
                world.log(vec![json!("releaseClosedRenderer"), json!(view.0)]);
                drop(world);
                generation.revoke();
                Ok(())
            }),
            reply,
        );
    }
}

impl Hooks {
    fn log(&self, name: &str, args: Vec<Value>) {
        let mut line = vec![Value::from(name)];
        line.extend(args);
        self.0.w().log(line);
    }
    fn create_prepared(&self, name: &str, args: Vec<Value>, kind: TabKind) -> TabResult<ViewId> {
        let mut w = self.0.w();
        let mut line = vec![Value::from(name)];
        line.extend(args);
        w.log(line);
        if w.case["fail"]
            .as_array()
            .is_some_and(|fail| fail.iter().any(|item| item.as_str() == Some(name)))
        {
            return Err(TabError("create failed".into()));
        }
        let view = w.make_view();
        w.kinds.insert(view.0, kind);
        Ok(view)
    }
}

fn synthetic_created_effect<T>(
    source: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    created: &shell_contract::runtime::RendererGeneration,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    effect: impl FnOnce() -> T,
) -> TabResult<T> {
    let mut generations = vec![created];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in std::iter::once(source).chain(participants) {
        generations.push(
            owner
                .context
                .renderer()
                .ok_or_else(|| TabError("Synthetic owner has no epoch".into()))?,
        );
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
        }
        if let Some(document) = owner.pdf_document() {
            pdf.push(document);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
        losses.push(
            owner
                .loss_token()
                .ok_or_else(|| TabError("Synthetic owner has no lifetime".into()))?,
        );
    }
    let mut tokens = vec![cancel, pending];
    tokens.extend(losses.iter());
    let mut generations =
        shell_contract::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = shell_contract::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = shell_contract::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations
        .with_active(|| pdf.with_live(|| tokens.with_active(effect)))
        .flatten()
        .flatten()
        .ok_or_else(|| TabError("Synthetic created operation lost a captured owner".into()))
}

fn probe_activation_effect(
    effects: &shell_contract::hosts::OwnedEffectsHost,
    authority: &super::activation::TabActivationAuthority,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    match authority {
        super::activation::TabActivationAuthority::Interactive(origin) => {
            effects.with_owned_effect_many(origin, participants, cancel, pending, effect)
        }
        super::activation::TabActivationAuthority::NativeInvocation { document, owner } => {
            shell_contract::hosts::synthetic::with_native_invocation(
                document,
                owner,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        super::activation::TabActivationAuthority::NativeOperation { operation, owner } => effects
            .with_native_operation_effect(operation, owner, participants, cancel, pending, effect),
        super::activation::TabActivationAuthority::StartupRecovery {
            startup,
            shell,
            child,
            owner,
        } => effects.with_startup_recovery_effect(
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            effect,
        ),
        super::activation::TabActivationAuthority::CommandRecovery {
            command,
            child,
            owner,
        } => effects.with_command_recovery_target_effect(
            command,
            child,
            owner,
            participants,
            cancel,
            pending,
            effect,
        ),
        super::activation::TabActivationAuthority::ExistingDocumentSave {
            path,
            owner,
            additional,
        } => {
            let mut captured = vec![owner.clone()];
            captured.extend_from_slice(participants);
            path.with_effect(effects, &captured, cancel, pending, additional, effect)
        }
        super::activation::TabActivationAuthority::SavedDocument {
            document,
            owner,
            additional,
        } => {
            let extra: Vec<_> = additional.iter().collect();
            effects.with_native_saved_document_same_origin(
                document,
                owner,
                cancel,
                participants,
                cancel,
                pending,
                &extra,
                &mut |_| effect(),
            )
        }
        super::activation::TabActivationAuthority::NativeSaveReload { receipt, owner } => effects
            .with_native_save_reload_many_effect(
                receipt,
                owner,
                participants,
                cancel,
                pending,
                effect,
            ),
        super::activation::TabActivationAuthority::NativeCommand {
            operation,
            expected,
            owner,
        } => {
            let mut captured = vec![owner.clone()];
            captured.extend_from_slice(participants);
            effects.with_native_command_preparation_effect(
                operation, expected, &captured, cancel, pending, effect,
            )
        }
        super::activation::TabActivationAuthority::NativeCommandTarget {
            document,
            expected,
            owner,
        } => effects.with_native_command_target_effect(
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            effect,
        ),
        super::activation::TabActivationAuthority::StartupCommand {
            startup,
            shell,
            child,
            expected,
            owner,
        } => {
            let mut captured = vec![owner.clone()];
            captured.extend_from_slice(participants);
            effects.with_startup_command_preparation_effect(
                startup, shell, child, expected, &captured, cancel, pending, effect,
            )
        }
        super::activation::TabActivationAuthority::StartupCommandTarget {
            startup,
            shell,
            document,
            expected,
            owner,
        } => effects.with_startup_command_target_effect(
            startup,
            shell,
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            effect,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_activation_emit(
    events: &shell_contract::hosts::synthetic::SyntheticOwnedEffects,
    revision: &super::activation::SplitMemoryRevision,
    effects: &shell_contract::hosts::OwnedEffectsHost,
    authority: &super::activation::TabActivationAuthority,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    target: usize,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    channel: &'static str,
    args: Vec<shell_contract::IpcValue>,
) -> Result<(), String> {
    let recipient = participants
        .get(target)
        .ok_or("Synthetic event recipient unavailable")?;
    let mut event = events.prepare_event(recipient, channel, args)?;
    probe_activation_effect(
        effects,
        authority,
        participants,
        cancel,
        pending,
        &mut || {
            revision
                .with_current_commit(|| event.enqueue())
                .ok_or("Synthetic split state was superseded")?
        },
    )?;
    event.drain();
    Ok(())
}

fn probe_activation_ui(
    effects: &shell_contract::hosts::OwnedEffectsHost,
    authority: &super::activation::TabActivationAuthority,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    action: Box<dyn FnOnce() -> Result<(), String> + Send>,
) -> Result<(), String> {
    let mut action = Some(action);
    let mut accepted = None;
    probe_activation_effect(
        effects,
        authority,
        participants,
        cancel,
        pending,
        &mut || {
            accepted = action.take();
            Ok(())
        },
    )?;
    accepted.ok_or("Synthetic activation action was not admitted")?()
}

#[derive(Clone)]
enum ProbePermitAction {
    Remove {
        fake: Fake,
        window: WindowId,
        view: ViewId,
        reservation: shell_contract::ipc::NativeClosureReservation,
        owner: shell_contract::ipc::NativeBrowserOwner,
    },
    Close(Arc<dyn super::host::NativeBrowserCloseProducer>),
}

struct ProbeCloseRetry {
    ui: Arc<dyn shell_contract::runtime::Thread>,
    core: Arc<dyn shell_contract::runtime::Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    authority: super::activation::TabActivationAuthority,
    participants: Arc<[shell_contract::ipc::NativeRequestOwner]>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    action: ProbePermitAction,
    permit: std::cell::RefCell<Option<shell_contract::ipc::NativeBrowserClosePermit>>,
}

impl ProbeCloseRetry {
    fn start(self: Rc<Self>, done: super::activation::TabUiStepDone) {
        let permit = self.permit.borrow_mut().take();
        let Some(permit) = permit else {
            return done(Err("The synthetic close permit is already in flight".into()));
        };
        let state = self.clone();
        let reply = crate::home_files::back_to_core(
            self.core.clone(),
            move |result: Result<
                Option<shell_contract::ipc::NativeBrowserClosePermit>,
                (String, shell_contract::ipc::NativeBrowserClosePermit),
            >| {
                match result {
                    Ok(Some(permit)) => done(Ok(
                        super::activation::TabUiStepOutcome::AppliedWithClosePermit(permit),
                    )),
                    Ok(None) => done(Ok(super::activation::TabUiStepOutcome::Applied)),
                    Err((error, permit)) => {
                        *state.permit.borrow_mut() = Some(permit);
                        let retry_state = state.clone();
                        let retry = Rc::new(move |done| retry_state.clone().start(done));
                        done(Ok(
                            super::activation::TabUiStepOutcome::CommittedCleanupPending {
                                error,
                                retry,
                            },
                        ));
                    }
                }
            },
        );
        let effects = self.effects.clone();
        let authority = self.authority.clone();
        let participants = self.participants.clone();
        let cancel = self.cancel.clone();
        let pending = self.pending.clone();
        let action = self.action.clone();
        self.ui.post(Box::new(move || {
            let admission = probe_activation_effect(
                &effects,
                &authority,
                &participants,
                &cancel,
                &pending,
                &mut || Ok(()),
            );
            let result = match admission {
                Ok(()) => match action {
                    ProbePermitAction::Close(producer) => {
                        producer.dispatch_close(permit).map(|_| None)
                    }
                    ProbePermitAction::Remove {
                        fake,
                        window,
                        view,
                        reservation,
                        owner,
                    } => {
                        let mut permit = permit;
                        match permit.with_closing(|transition| {
                            if !transition.matches(&reservation)
                                || !transition.matches_owner(&owner)
                            {
                                return Err(
                                    "The synthetic removal permit belongs to another browser"
                                        .into(),
                                );
                            }
                            fake.remove_child_view(window, view);
                            Ok(())
                        }) {
                            Ok(()) => Ok(Some(permit)),
                            Err(error) => Err((error, permit)),
                        }
                    }
                },
                Err(error) => Err((error, permit)),
            };
            reply(result);
        }));
    }
}

impl TabHooks for Hooks {
    fn cleanup_lifetime(&self) -> shell_contract::runtime::CancelToken {
        self.0.w().cleanup_lifetime.clone()
    }
    fn register_generated_path_task(
        &self,
        task: std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        self.1.hosts.get::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticGeneratedCleanupRegistry>>()
            .ok_or_else(|| "The synthetic generated cleanup registry is unavailable".to_owned())?.register(task)
    }
    fn capture_generated_retry_prompt(
        &self,
        shell: ShellWindow,
        owner: shell_contract::ipc::NativeRequestOwner,
        output: shell_contract::ipc::SavedPublication,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
    ) -> Result<Rc<dyn super::host::CapturedGeneratedRetryPrompt>, String> {
        if owner.view != shell.view {
            return Err("The synthetic retry owner does not match its shell".into());
        }
        let effects = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Synthetic retry authority unavailable")?;
        participants.push(owner);
        let extra: Vec<_> = additional.iter().collect();
        let mut captured = None;
        output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                let world = self.0.w();
                if world.shell.view != shell.view || world.shell.window != shell.window {
                    return Err("The synthetic retry shell changed".into());
                }
                let window = world
                    .windows
                    .get(&shell.window.0)
                    .filter(|window| !window.destroyed)
                    .ok_or("The synthetic retry parent is unavailable")?;
                captured = Some(window.proof.clone());
                Ok(())
            },
        )?;
        Ok(Rc::new(ProbeGeneratedRetryPrompt {
            fake: self.0.clone(),
            services: self.1.clone(),
            shell,
            window: captured.ok_or("The synthetic retry parent was not captured")?,
            output,
            participants,
            cancel,
            pending,
            additional,
        }))
    }
    fn capture_saved_shell(
        &self,
        shell: ShellWindow,
        owner: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<Rc<dyn super::host::CapturedSavedShell>, String> {
        if owner.view != shell.view {
            return Err("Synthetic saved shell owner does not match".into());
        }
        let effects = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Synthetic saved shell authority unavailable")?;
        let loss = owner
            .loss_token()
            .ok_or("Synthetic saved shell lifetime unavailable")?;
        let mut captured = None;
        effects.with_owned_effect(&owner, &owner, &loss, &loss, &mut || {
            let world = self.0.w();
            if world.shell != shell {
                return Err("Synthetic saved shell changed".into());
            }
            captured = Some(
                world
                    .windows
                    .get(&shell.window.0)
                    .filter(|window| !window.destroyed)
                    .ok_or("Synthetic saved shell window unavailable")?
                    .proof
                    .clone(),
            );
            Ok(())
        })?;
        Ok(Rc::new(ProbeCapturedSavedShell {
            fake: self.0.clone(),
            services: self.1.clone(),
            shell,
            owner,
            window: captured.ok_or("Synthetic saved shell window was not captured")?,
        }))
    }
    fn prepare_activation_ui(
        &self,
        shell: ShellWindow,
        shell_owner: &shell_contract::ipc::NativeRequestOwner,
        target: Option<ViewId>,
        effects: &[main_core::shell::tab_manager::TabActivationEffect<
            ViewId,
            super::host::RetainedSpareBinding,
        >],
    ) -> Result<super::activation::PreparedTabUiBatch, String> {
        use main_core::shell::tab_manager::TabActivationEffect;
        let owners = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("synthetic ownership service unavailable")?;
        let capture = |view: ViewId| {
            let generation = self
                .1
                .views
                .renderer_generation(view)
                .ok_or("synthetic activation view unavailable")?;
            owners.capture_owner(view, &generation)
        };
        let generation = shell_owner
            .context
            .renderer()
            .ok_or("synthetic activation shell generation unavailable")?;
        if shell_owner.view != shell.view
            || self
                .1
                .views
                .sender_of(shell.view)
                .is_none_or(|sender| sender.window != shell.window)
            || !self
                .1
                .views
                .renderer_generation(shell.view)
                .is_some_and(|current| current.same_generation(generation))
            || self.0.view_destroyed(shell.view)
        {
            return Err("synthetic activation shell unavailable".into());
        }
        let loss = shell_owner
            .loss_token()
            .ok_or("synthetic activation shell lifetime unavailable")?;
        owners.with_owned_effect(shell_owner, shell_owner, &loss, &loss, &mut || Ok(()))?;
        let shell_owner = shell_owner.clone();
        let mut participants = Vec::new();
        for effect in effects {
            if let Some(view) = effect.view().copied()
                && view != shell.view
                && !participants
                    .iter()
                    .any(|owner: &shell_contract::ipc::NativeRequestOwner| owner.view == view)
            {
                match effect {
                    TabActivationEffect::RemoveChild(_, binding)
                    | TabActivationEffect::Close(_, binding) => {
                        if let super::host::NativeBrowserRegistration::Registered(owner) =
                            &binding.browser.registration
                        {
                            if owner.view != view {
                                return Err("The synthetic browser binding has another view".into());
                            }
                            participants.push(owner.clone());
                        }
                    }
                    _ => {
                        if self.0.view_destroyed(view) {
                            return Err("synthetic activation view destroyed".into());
                        }
                        participants.push(capture(view)?);
                    }
                }
            }
        }
        let mut steps = Vec::with_capacity(effects.len());
        for effect in effects {
            let action: Option<Box<dyn FnOnce() -> Result<(), String> + Send>> = match effect {
                TabActivationEffect::Visible(view, visible) => {
                    let (fake, view, visible) = (self.0.clone(), *view, *visible);
                    Some(Box::new(move || {
                        fake.view_set_visible(view, visible);
                        Ok(())
                    }))
                }
                TabActivationEffect::Bounds(view, bounds) => {
                    let (fake, view, bounds) = (self.0.clone(), *view, *bounds);
                    Some(Box::new(move || {
                        fake.view_set_bounds(view, &bounds);
                        Ok(())
                    }))
                }
                TabActivationEffect::Focus(view) => {
                    let (fake, view) = (self.0.clone(), *view);
                    Some(Box::new(move || {
                        fake.view_focus(view);
                        Ok(())
                    }))
                }
                TabActivationEffect::FocusShell => {
                    let fake = self.0.clone();
                    Some(Box::new(move || {
                        fake.view_focus(shell.view);
                        Ok(())
                    }))
                }
                TabActivationEffect::Menu(kind) => {
                    let (fake, kind) = (self.0.clone(), *kind);
                    Some(Box::new(move || {
                        let mut world = fake.w();
                        world.active_menu = Some((kind, target));
                        world.log(vec![
                            json!("applyMenuFor"),
                            json!(kind.as_str()),
                            target.map_or(Value::Null, |view| json!(view.0)),
                        ]);
                        Ok(())
                    }))
                }
                TabActivationEffect::RemoveChild(view, binding) => {
                    let action = ProbePermitAction::Remove {
                        fake: self.0.clone(),
                        window: shell.window,
                        view: *view,
                        reservation: binding.browser.reservation.clone(),
                        owner: binding.browser.owner.clone(),
                    };
                    let effects = owners.clone();
                    let core = self.1.core.clone();
                    let ui = self.1.ui.clone();
                    steps.push(Some(super::activation::PreparedTabUiStep::new(Box::new(
                        move |authority, participants, cancel, pending, permit, done| {
                            let Some(permit) = permit else {
                                return done(Err(
                                    "The synthetic removal permit is unavailable".into()
                                ));
                            };
                            Rc::new(ProbeCloseRetry {
                                ui,
                                core,
                                effects,
                                authority,
                                participants,
                                cancel,
                                pending,
                                action,
                                permit: std::cell::RefCell::new(Some(permit)),
                            })
                            .start(done);
                        },
                    ))));
                    continue;
                }
                TabActivationEffect::Close(_, binding) => {
                    let action = ProbePermitAction::Close(binding.browser.producer.clone());
                    let effects = owners.clone();
                    let core = self.1.core.clone();
                    let ui = self.1.ui.clone();
                    steps.push(Some(super::activation::PreparedTabUiStep::new(Box::new(
                        move |authority, participants, cancel, pending, permit, done| {
                            let Some(permit) = permit else {
                                return done(Err(
                                    "The synthetic close permit is unavailable".into()
                                ));
                            };
                            Rc::new(ProbeCloseRetry {
                                ui,
                                core,
                                effects,
                                authority,
                                participants,
                                cancel,
                                pending,
                                action,
                                permit: std::cell::RefCell::new(Some(permit)),
                            })
                            .start(done);
                        },
                    ))));
                    continue;
                }
                TabActivationEffect::SetSpareTimer(_)
                | TabActivationEffect::ClearSpareTimer
                | TabActivationEffect::ActiveDocs(_)
                | TabActivationEffect::ActiveSheets(_)
                | TabActivationEffect::ActiveSlides(_)
                | TabActivationEffect::Changed
                | TabActivationEffect::SplitChanged(_) => None,
            };
            steps.push(action.map(|action| {
                let effects = owners.clone();
                let core = self.1.core.clone();
                let ui = self.1.ui.clone();
                let fake = self.0.clone();
                super::activation::PreparedTabUiStep::new(Box::new(
                    move |origin, participants, cancel, pending, permit, done| {
                        if permit.is_some() {
                            return done(Err("Unexpected synthetic close permit".into()));
                        }
                        let reply = crate::home_files::back_to_core(
                            core,
                            move |result: Result<(), String>| {
                                done(result.map(|()| super::activation::TabUiStepOutcome::Applied))
                            },
                        );
                        ui.post(Box::new(move || {
                            let action = Box::new(move || {
                                if fake.w().case["fail"].as_array().is_some_and(|items| {
                                    items
                                        .iter()
                                        .any(|item| item.as_str() == Some("activationUi"))
                                }) {
                                    return Err("synthetic activation UI failed".into());
                                }
                                action()
                            });
                            reply(probe_activation_ui(
                                &effects,
                                &origin,
                                &participants,
                                &cancel,
                                &pending,
                                action,
                            ));
                        }));
                    },
                ))
            }));
        }
        Ok(super::activation::PreparedTabUiBatch {
            shell: shell_owner,
            participants,
            steps,
        })
    }

    fn prepare_docs_disposal_owned(
        &self,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        target: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    ) -> Result<Rc<dyn super::activation::DocsDisposalOperation>, String> {
        if target.view != view || self.0.view_destroyed(view) {
            return Err("synthetic disposal target unavailable".into());
        }
        target
            .context
            .renderer()
            .ok_or("synthetic disposal renderer unavailable")?;
        let effects = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("synthetic ownership service unavailable")?;
        participants.push(destination);
        participants.push(target.clone());
        effects.with_owned_effect_many(
            &origin,
            &participants,
            &cancel,
            &pending,
            &mut || Ok(()),
        )?;
        Ok(Rc::new(ProbeDocsDisposal {
            fake: self.0.clone(),
            core: self.1.core.clone(),
            effects,
            origin,
            participants,
            target,
            cancel,
            pending,
            running: std::cell::Cell::new(false),
            closed: std::cell::Cell::new(false),
        }))
    }

    fn prepare_activation_publication_owned(
        &self,
        origin: super::activation::TabActivationAuthority,
        shell: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        snapshot: super::activation::TabActivationPublicationSnapshot,
    ) -> Result<super::activation::PreparedTabActivationPublication, String> {
        participants.push(shell);
        let shell_index = participants.len() - 1;
        probe_activation_effect(
            &effects,
            &origin,
            &participants,
            &cancel,
            &pending,
            &mut || Ok(()),
        )?;
        let mut recipients = Vec::new();
        for tab in &snapshot.changed.tabs {
            if let Some((_, view)) = snapshot.tab_views.iter().find(|(id, _)| id == &tab.id) {
                let index = participants
                    .iter()
                    .position(|owner| owner.view == *view)
                    .ok_or("probe tab owner unavailable")?;
                recipients.push((tab.id.clone(), index));
            }
        }
        let changed_args = vec![shell_contract::IpcValue::Array(
            snapshot
                .changed
                .tabs
                .iter()
                .map(|tab| super::wire::to_ipc(&tab.to_value()))
                .collect(),
        )];
        let split_args = vec![
            snapshot
                .split
                .as_ref()
                .map_or(shell_contract::IpcValue::Null, |state| {
                    super::wire::to_ipc(&state.to_value())
                }),
        ];
        let split_state = snapshot.split.clone();
        let persisted = snapshot.split.as_ref().map(|state| state.to_value());
        let changed_revision = snapshot.split_memory_revision.clone();
        let split_revision = snapshot.split_memory_revision.clone();
        let events = self
            .1
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .cloned()
            .ok_or("synthetic event publication unavailable")?;
        let changed_events = events.clone();
        let split_events = events;
        let changed_origin = origin.clone();
        let changed_participants = participants.clone();
        let changed_cancel = cancel.clone();
        let changed_pending = pending.clone();
        let changed_effects = effects.clone();
        let changed = Box::new(move |done: super::activation::TabUiStepDone| {
            done(
                probe_activation_emit(
                    changed_events.as_ref(),
                    &changed_revision,
                    &changed_effects,
                    &changed_origin,
                    &changed_participants,
                    shell_index,
                    &changed_cancel,
                    &changed_pending,
                    "tabs:changed",
                    changed_args,
                )
                .map(|()| super::activation::TabUiStepOutcome::Applied),
            );
        });
        let split_origin = origin.clone();
        let split_participants = participants.clone();
        let split_cancel = cancel.clone();
        let split_pending = pending.clone();
        let split_effects = effects.clone();
        let split = Box::new(
            move |ratio_only: bool, done: super::activation::TabUiStepDone| {
                let result = (|| -> Result<(), String> {
                    if !ratio_only {
                        for (id, index) in &recipients {
                            let role = split_state.as_ref().and_then(|state| {
                                let pane = if id == &state.a_tab_id {
                                    "a"
                                } else if id == &state.b_tab_id {
                                    "b"
                                } else {
                                    return None;
                                };
                                Some(if state.focus.as_str() == pane {
                                    "focus"
                                } else {
                                    "secondary"
                                })
                            });
                            probe_activation_emit(
                                split_events.as_ref(),
                                &split_revision,
                                &split_effects,
                                &split_origin,
                                &split_participants,
                                *index,
                                &split_cancel,
                                &split_pending,
                                "app:split-role-changed",
                                vec![role.map_or(shell_contract::IpcValue::Null, |role| {
                                    shell_contract::IpcValue::String(role.into())
                                })],
                            )?;
                        }
                    }
                    probe_activation_emit(
                        split_events.as_ref(),
                        &split_revision,
                        &split_effects,
                        &split_origin,
                        &split_participants,
                        shell_index,
                        &split_cancel,
                        &split_pending,
                        "tabs:split-changed",
                        split_args,
                    )
                })();
                done(result.map(|()| super::activation::TabUiStepOutcome::Applied));
            },
        );
        let log = self.0.clone();
        let revision = snapshot.split_memory_revision;
        let persist_split = Box::new(move |done: super::activation::TabUiStepDone| {
            let result = probe_activation_effect(
                &effects,
                &origin,
                &participants,
                &cancel,
                &pending,
                &mut || {
                    revision
                        .with_current_commit(|| {
                            log.w().log(vec![
                                json!("persistSplitMemory"),
                                persisted.clone().unwrap_or(Value::Null),
                            ])
                        })
                        .ok_or("split memory was superseded")?;
                    Ok(())
                },
            );
            done(result.map(|()| super::activation::TabUiStepOutcome::Applied));
        });
        Ok(super::activation::PreparedTabActivationPublication {
            changed,
            split,
            persist_split,
        })
    }
    fn create_html_present_view(
        &self,
        owner: i64,
        title: &str,
        done: Box<dyn FnOnce(ViewCreationResult<ViewId>)>,
    ) {
        let (name, args, kind) = create_call(&ViewSpec::HtmlPresent { owner, title });
        done(fixture_creation(
            &self.0,
            self.create_prepared(name, args, kind)
                .map(|view| (view, view)),
        ));
    }
    fn create_spare_view_owned(
        &self,
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: shell_contract::ipc::NativeRequestOwner,
        cleanup_token: shell_contract::runtime::CancelToken,
        rollback: super::host::EditorSeedRollbackStore,
        spare_actor: super::host::SpareActorSlot,
        done: Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    ) {
        use super::host::NativeBrowserRegistration;
        use shell_contract::ipc::NativeEditorCommand;
        let (issuer, startup_error) = {
            let world = self.0.w();
            (world.startup_issuer.clone(), world.startup_error.clone())
        };
        if let Some(error) = startup_error {
            return done(fixture_creation(&self.0, Err(TabError(error))));
        }
        let Some(issuer) = issuer else {
            return done(fixture_creation(
                &self.0,
                Err(TabError("Synthetic startup issuer unavailable".into())),
            ));
        };
        let command = NativeEditorCommand::NewDocument {
            surface: shell_contract::Surface::Sheets,
            directory: None,
        };
        let child =
            match issuer.issue_command_child(&startup, command.clone(), cleanup_token.clone()) {
                Ok(child) => child,
                Err(error) => return done(fixture_creation(&self.0, Err(TabError(error)))),
            };
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(fixture_creation(
                &self.0,
                Err(TabError("Synthetic ownership unavailable".into())),
            ));
        };
        let created = shell_contract::runtime::RendererGeneration::new();
        let pending = shell_contract::runtime::CancelToken::default();
        let mut view = None;
        let mut actor_token = None;
        let (name, args, kind) = create_call(&ViewSpec::Sheets {
            opening_workbook: None,
        });
        let admitted = effects.with_startup_command_creation_effect(
            &startup,
            &shell,
            &child,
            &command,
            &created,
            &[],
            &cleanup_token,
            &pending,
            &mut || {
                let made = self
                    .create_prepared(name, args.clone(), kind)
                    .map_err(|error| error.0)?;
                let closure = fixture_reserve(&self.0, made, created.clone());
                actor_token = Some(fixture_install_epoch(
                    &self.0,
                    made,
                    &created,
                    &closure.reservation,
                    &closure.owner,
                    &rollback,
                ));
                view = Some(made);
                Ok(())
            },
        );
        let Some(view) = view else {
            return done(fixture_creation(
                &self.0,
                Err(TabError(admitted.err().unwrap_or_else(|| {
                    "Synthetic spare creation did not run".into()
                }))),
            ));
        };
        let close = fixture_cleanup(&self.0, view);
        let generation = created.clone();
        let cleanup: ViewCreationCleanup = Arc::new(move |done| {
            generation.revoke();
            close(done);
        });
        if let Err(error) = admitted {
            return done(Err(ViewCreationFailure {
                error: TabError(error),
                cleanup: ViewCreationCleanupState::Pending(cleanup),
            }));
        }
        let Some(token) = actor_token else {
            return done(Err(ViewCreationFailure {
                error: TabError("Synthetic spare actor registration did not complete".into()),
                cleanup: ViewCreationCleanupState::Pending(cleanup),
            }));
        };
        let browser = fixture_close_state(&self.0, view);
        let owner = browser.owner.clone();
        let actor: Rc<dyn super::host::SpareActor> = Rc::new(ProbeSpareActor {
            fake: self.0.clone(),
            view,
            reservation: browser.reservation.clone(),
            epoch: created.clone(),
            token,
            owner: Rc::new(std::cell::RefCell::new(Some(owner.clone()))),
            state: Rc::new(std::cell::RefCell::new(ProbeSpareState {
                owner,
                closing: false,
            })),
        });
        *spare_actor.borrow_mut() = Some(actor);
        let registered = (|| {
            let owner =
                fixture_register_owner(&self.0, view, &created, shell_contract::Surface::Sheets)?;
            let target = child.reserve_target(view, created.clone())?;
            shell_contract::hosts::synthetic::register_native_invocation_fixture(&target, owner)
        })();
        let document = match registered {
            Ok(document) => document,
            Err(error) => {
                return done(Err(ViewCreationFailure {
                    error: TabError(error),
                    cleanup: ViewCreationCleanupState::Pending(cleanup),
                }));
            }
        };
        let fake = self.0.clone();
        let ui_fake = fake.clone();
        let ui_document = document.clone();
        let ui_startup = startup.clone();
        let ui_shell = shell.clone();
        let shell_window = self.0.w().shell.window;
        let finish = crate::home_files::back_to_core(
            self.1.core.clone(),
            Box::new(move |result: Result<(), String>| match result {
                Ok(()) => {
                    let owner = document.owner().clone();
                    let mut ready = fixture_ready(&fake, view, document);
                    ready.reservation.registration = NativeBrowserRegistration::Registered(owner);
                    ready.cleanup = cleanup;
                    done(Ok(ready));
                }
                Err(error) => done(Err(ViewCreationFailure {
                    error: TabError(error),
                    cleanup: ViewCreationCleanupState::Pending(cleanup),
                })),
            }),
        );
        self.1.ui.post(Box::new(move || {
            let result = effects.with_startup_command_target_effect(
                &ui_startup,
                &ui_shell,
                &child,
                &command,
                &ui_document,
                ui_document.owner(),
                &[],
                &cleanup_token,
                &pending,
                &mut || {
                    if ui_fake.view_destroyed(view) {
                        return Err("The synthetic spare was closed before attachment".into());
                    }
                    ui_fake.add_child_view(shell_window, view);
                    Ok(())
                },
            );
            finish(result);
        }));
    }
    fn prepare_view_native_command(
        &self,
        spec: &ViewSpec<'_>,
        operation: shell_contract::ipc::NativeInvocationOperation,
        _rollback: super::host::EditorSeedRollbackStore,
        initialize: super::host::EditorInitializer,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    ) -> TabResult<(
        shell_contract::runtime::RendererGeneration,
        Box<
            dyn FnOnce(
                Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
            ),
        >,
    )> {
        let expected = match operation.selection() {
            shell_contract::ipc::NativeInvocationSelection::NativeCommand(command)
                if fixture_command_matches(spec, command) =>
            {
                command.clone()
            }
            _ => {
                return Err(TabError(
                    "Synthetic native command does not match the view".into(),
                ));
            }
        };
        let (name, args, kind) = create_call(spec);
        let hooks = Hooks(self.0.clone(), self.1.clone());
        let created = shell_contract::runtime::RendererGeneration::new();
        let generation = created.clone();
        Ok((
            created,
            Box::new(move |done| {
                done(fixture_native_command(
                    &hooks.0,
                    &operation,
                    &expected,
                    &generation,
                    &participants,
                    &cancel,
                    &pending,
                    || hooks.create_prepared(name, args, kind),
                    initialize,
                ));
            }),
        ))
    }
    fn prepare_view_owned(
        &self,
        spec: &ViewSpec<'_>,
        _rollback: super::host::EditorSeedRollbackStore,
        initialize: super::host::EditorInitializer,
    ) -> TabResult<(
        shell_contract::runtime::RendererGeneration,
        Box<
            dyn FnOnce(
                shell_contract::ipc::NativeRequestOwner,
                shell_contract::runtime::CancelToken,
                shell_contract::runtime::CancelToken,
                Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeRequestOwner>)>,
            ),
        >,
    )> {
        let (name, args, kind) = create_call(spec);
        let hooks = Hooks(self.0.clone(), self.1.clone());
        let created = shell_contract::runtime::RendererGeneration::new();
        let generation = created.clone();
        Ok((
            created,
            Box::new(move |owner, cancel, pending, then| {
                let made =
                    synthetic_created_effect(&owner, &[], &generation, &cancel, &pending, || {
                        hooks.create_prepared(name, args, kind)
                    });
                let view = match made.and_then(|result| result) {
                    Ok(view) => view,
                    Err(error) => return then(fixture_creation(&hooks.0, Err(error))),
                };
                let closure = fixture_reserve(&hooks.0, view, generation.clone());
                let close = fixture_cleanup(&hooks.0, view);
                let retired = generation.clone();
                let cleanup: ViewCreationCleanup = Arc::new(move |done| {
                    retired.revoke();
                    close(done);
                });
                if let Err(error) =
                    initialize(view, &generation, &closure.reservation, &closure.owner)
                {
                    return then(Err(ViewCreationFailure {
                        error,
                        cleanup: ViewCreationCleanupState::Pending(cleanup),
                    }));
                }
                let captured =
                    fixture_register_owner(&hooks.0, view, &generation, fixture_surface(kind))
                        .and_then(|target| {
                            shell_contract::hosts::synthetic::admit_many(
                                &owner,
                                std::slice::from_ref(&target),
                                &cancel,
                                &pending,
                                || target.clone(),
                            )
                        });
                match captured {
                    Ok(target) => {
                        let registered = target.clone();
                        let mut ready = fixture_ready(&hooks.0, view, target);
                        ready.reservation.registration =
                            super::host::NativeBrowserRegistration::Registered(registered);
                        ready.cleanup = cleanup;
                        then(Ok(ready))
                    }
                    Err(error) => then(Err(ViewCreationFailure {
                        error: TabError(error),
                        cleanup: ViewCreationCleanupState::Pending(cleanup),
                    })),
                }
            }),
        ))
    }
    fn prepare_generated_view_owned(
        &self,
        spec: &ViewSpec<'_>,
        source: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: super::host::EditorSeedRollbackStore,
    ) -> TabResult<super::host::PreparedGeneratedView> {
        use super::host::SpareSeedPlan;
        let (name, args, kind) = create_call(spec);
        let view = self.0.w().make_view();
        let created = shell_contract::runtime::RendererGeneration::new();
        let closure = fixture_reserve(&self.0, view, created.clone());
        let mut unstarted = ProbeUnstartedSaved {
            fake: self.0.clone(),
            view,
            closure: closure.clone(),
            armed: true,
        };
        let mut seed_plan = prepare_probe_seed(
            &self.0,
            view,
            &created,
            &closure.reservation,
            seed,
            placement,
            None,
            None,
            &rollback,
        )?;
        let epoch_token = Arc::new(());
        {
            let mut world = self.0.w();
            world
                .kinds
                .try_reserve(1)
                .map_err(|error| TabError(error.to_string()))?;
            world
                .generated_epochs
                .try_reserve(1)
                .map_err(|error| TabError(error.to_string()))?;
            world
                .generated_epoch_tokens
                .try_reserve(1)
                .map_err(|error| TabError(error.to_string()))?;
        }
        rollback
            .borrow_mut()
            .try_reserve(1)
            .map_err(|error| TabError(error.to_string()))?;
        let epoch_receipt = Rc::new(ProbeEpochRollback {
            world: self.0.clone(),
            view,
            created: created.clone(),
            reservation: closure.reservation.clone(),
            owner: closure.owner.clone(),
            token: epoch_token.clone(),
            previous: std::cell::RefCell::new(None),
            completed: std::cell::Cell::new(false),
        });
        let ready = fixture_ready(&self.0, view, view);
        let binding = ready.reservation.clone();
        let gate = Arc::new(FixturePreclose::default());
        let start_gate = gate.clone();
        let close = fixture_cleanup(&self.0, view);
        let installer: super::host::GeneratedPrecloseInstaller =
            Box::new(move |barrier, failure| gate.install(close, barrier, failure));
        let fake = self.0.clone();
        let generation = created.clone();
        let start =
            Box::new(
                move |adopt: super::host::GeneratedManagerAdoption,
                      register: super::host::GeneratedManagerRegistration,
                      cleanup: ViewCreationCleanup,
                      done: Box<
                    dyn FnOnce(ViewCreationResult<super::host::GeneratedViewCommit>),
                >| {
                    let cleanup = gated_cleanup(start_gate, cleanup);
                    let unstarted = &mut unstarted;
                    unstarted.armed = false;
                    let mut adoption = Some(adopt);
                    let mut epoch_committed = false;
                    let result = synthetic_created_effect(
                        &source,
                        &participants,
                        &generation,
                        &cancel,
                        &pending,
                        || {
                            closure
                                .reservation
                                .with_initial_owner(&closure.owner, |transition| {
                                    {
                                        let world = fake.w();
                                        if world.kinds.contains_key(&view.0)
                                            || world.generated_epochs.contains_key(&view)
                                            || !world.closures.get(&view).is_some_and(|current| {
                                                Arc::ptr_eq(current, &closure)
                                            })
                                        {
                                            return Err(
                                                "Synthetic generated reservation changed".into()
                                            );
                                        }
                                    }
                                    let mut actor =
                                        |next: &mut dyn FnMut() -> Result<(), String>| {
                                            seed_plan.with_validated(
                                                transition,
                                                &closure.owner,
                                                &mut || {
                                                    next()?;
                                                    let mut world = fake.w();
                                                    world.kinds.insert(view.0, kind);
                                                    world
                                                        .generated_epochs
                                                        .insert(view, generation.clone());
                                                    world
                                                        .generated_epoch_tokens
                                                        .insert(view, epoch_token.clone());
                                                    epoch_committed = true;
                                                    Ok(())
                                                },
                                            )
                                        };
                                    adoption
                                        .take()
                                        .ok_or("Synthetic generated adoption was consumed")?(
                                        transition,
                                        &closure.owner,
                                        &mut actor,
                                    )
                                })
                        },
                    )
                    .and_then(|result| result.map_err(TabError));
                    if epoch_committed {
                        rollback.borrow_mut().push(epoch_receipt);
                    }
                    let seeded = if seed_plan.committed {
                        seed_plan.after_commit(&cancel).map_err(TabError)
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result.and(seeded) {
                        return done(Err(ViewCreationFailure {
                            error,
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    }
                    {
                        let world = fake.w();
                        let mut line = vec![Value::from(name)];
                        line.extend(args);
                        world.log(line);
                        if world.case["fail"].as_array().is_some_and(|items| items.iter().any(|item| matches!(item.as_str(), Some(value) if value == name || value == "loadGeneratedView"))) {
                    drop(world);
                    return done(Err(ViewCreationFailure { error: TabError("Synthetic generated load failed".into()), cleanup: ViewCreationCleanupState::Pending(cleanup) }));
                }
                        world.log(vec![json!("loadGeneratedView"), json!(view.0)]);
                    }
                    let target = match fixture_register_owner(
                        &fake,
                        view,
                        &generation,
                        fixture_surface(kind),
                    )
                    .and_then(|target| {
                        shell_contract::hosts::synthetic::admit_many(
                            &source,
                            std::slice::from_ref(&target),
                            &cancel,
                            &pending,
                            || target.clone(),
                        )
                    }) {
                        Ok(target) => target,
                        Err(error) => {
                            return done(Err(ViewCreationFailure {
                                error: TabError(error),
                                cleanup: ViewCreationCleanupState::Pending(cleanup),
                            }));
                        }
                    };
                    let mut published = None;
                    let registered = synthetic_created_effect(
                        &source,
                        std::slice::from_ref(&target),
                        &generation,
                        &cancel,
                        &pending,
                        || {
                            closure
                                .reservation
                                .with_owner(&closure.owner, |transition| {
                                    register(&target, transition, &mut || {
                                        published = Some(target.clone());
                                        Ok(())
                                    })
                                })
                        },
                    )
                    .and_then(|result| result.map_err(TabError));
                    if let Err(error) = registered {
                        return done(Err(ViewCreationFailure {
                            error,
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    }
                    let Some(target) = published else {
                        return done(Err(ViewCreationFailure {
                            error: TabError(
                                "Synthetic generated registration was not published".into(),
                            ),
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    };
                    match super::host::GeneratedViewCommit::loaded(target.clone(), generation) {
                        Ok(commit) => {
                            let mut reservation = ready.reservation;
                            reservation.registration =
                                super::host::NativeBrowserRegistration::Registered(target);
                            done(Ok(ViewCreationReady {
                                view: commit,
                                cleanup,
                                closure: ready.closure,
                                reservation,
                            }));
                        }
                        Err(error) => done(Err(ViewCreationFailure {
                            error,
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        })),
                    }
                },
            );
        Ok(super::host::PreparedGeneratedView {
            view,
            created,
            binding,
            spare_actor: None,
            installer,
            start,
        })
    }
    fn prepare_initialized_spare_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
        binding: super::host::RetainedSpareBinding,
        source: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: super::host::EditorSeedRollbackStore,
    ) -> TabResult<super::host::PreparedGeneratedView> {
        use super::host::SpareSeedPlan;
        let closure = self
            .0
            .w()
            .closures
            .get(&view)
            .cloned()
            .ok_or_else(|| TabError("Synthetic spare reservation unavailable".into()))?;
        if !closure.reservation.matches(view, expected)
            || !closure
                .reservation
                .same_reservation(&binding.browser.reservation)
        {
            return Err(TabError("Synthetic spare reservation changed".into()));
        }
        let target = match &binding.browser.registration {
            super::host::NativeBrowserRegistration::Registered(owner)
                if owner.view == view
                    && owner
                        .context
                        .renderer()
                        .is_some_and(|renderer| renderer.same_generation(expected)) =>
            {
                owner.clone()
            }
            _ => return Err(TabError("Synthetic spare registration changed".into())),
        };
        let actor = binding
            .spare_actor
            .as_ref()
            .ok_or_else(|| TabError("Synthetic spare actor unavailable".into()))?;
        let mut transfer = actor.prepare_transfer().map_err(TabError)?;
        let successor_receipt = transfer.successor_receipt();
        let successor_actor = transfer.successor();
        let replacement = closure
            .reservation
            .prepare_replacement(&binding.browser.owner)
            .map_err(TabError)?;
        let successor = replacement.successor().clone();
        let mut proposed = binding.browser.clone();
        proposed.owner = successor.clone();
        let mut seed_plan = prepare_probe_seed(
            &self.0,
            view,
            expected,
            &closure.reservation,
            seed,
            placement,
            None,
            None,
            &rollback,
        )?;
        rollback
            .borrow_mut()
            .try_reserve(2)
            .map_err(|error| TabError(error.to_string()))?;
        let prior = Rc::new(ProbeSupersededDrain {
            state: Rc::new(std::cell::RefCell::new(ProbeSupersededState {
                receipts: binding.rollback.borrow().clone(),
                proof: None,
                running: false,
            })),
        });
        let gate = Arc::new(FixturePreclose::default());
        let close = fixture_cleanup_for(&self.0, view, successor.clone());
        let installer: super::host::GeneratedPrecloseInstaller =
            Box::new(move |barrier, failure| gate.install(close, barrier, failure));
        let created = expected.clone();
        let generation = created.clone();
        let ready_binding = proposed.clone();
        participants.push(target.clone());
        let start =
            Box::new(
                move |adopt: super::host::GeneratedManagerAdoption,
                      register: super::host::GeneratedManagerRegistration,
                      cleanup: ViewCreationCleanup,
                      done: Box<
                    dyn FnOnce(ViewCreationResult<super::host::GeneratedViewCommit>),
                >| {
                    let mut adopt = Some(adopt);
                    let result = synthetic_created_effect(
                        &source,
                        &participants,
                        &generation,
                        &cancel,
                        &pending,
                        || {
                            replacement.commit(|transition, next_owner| {
                                let mut actor = |next: &mut dyn FnMut() -> Result<(), String>| {
                                    transfer.with_validated(transition, next_owner, &mut || {
                                        seed_plan.with_validated(transition, next_owner, next)
                                    })
                                };
                                adopt
                                    .take()
                                    .ok_or("Synthetic spare adoption was consumed")?(
                                    transition, next_owner, &mut actor,
                                )
                            })
                        },
                    )
                    .and_then(|result| result.map_err(TabError));
                    let (_, proof, ()) = match result {
                        Ok(value) => value,
                        Err(error) => {
                            return done(Err(ViewCreationFailure {
                                error,
                                cleanup: ViewCreationCleanupState::Complete,
                            }));
                        }
                    };
                    prior.state.borrow_mut().proof = Some(proof);
                    rollback.borrow_mut().push(prior.clone());
                    rollback.borrow_mut().push(successor_receipt);
                    let mut published = None;
                    let registered = synthetic_created_effect(
                        &source,
                        &participants,
                        &generation,
                        &cancel,
                        &pending,
                        || {
                            closure.reservation.with_owner(&successor, |transition| {
                                register(&target, transition, &mut || {
                                    published = Some(target.clone());
                                    Ok(())
                                })
                            })
                        },
                    )
                    .and_then(|result| result.map_err(TabError));
                    if let Err(error) = registered {
                        return done(Err(ViewCreationFailure {
                            error,
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    }
                    let Some(target) = published else {
                        return done(Err(ViewCreationFailure {
                            error: TabError(
                                "Synthetic spare registration was not published".into(),
                            ),
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    };
                    if let Err(error) = seed_plan.after_commit(&cancel) {
                        return done(Err(ViewCreationFailure {
                            error: TabError(error),
                            cleanup: ViewCreationCleanupState::Pending(cleanup),
                        }));
                    }
                    prior.begin(Box::new(move |retired| {
                        if let Err(error) = retired {
                            return done(Err(ViewCreationFailure {
                                error: TabError(error),
                                cleanup: ViewCreationCleanupState::Pending(cleanup),
                            }));
                        }
                        match super::host::GeneratedViewCommit::loaded(target, generation) {
                            Ok(commit) => done(Ok(ViewCreationReady {
                                view: commit,
                                cleanup,
                                closure: ready_binding.closure.clone(),
                                reservation: ready_binding,
                            })),
                            Err(error) => done(Err(ViewCreationFailure {
                                error,
                                cleanup: ViewCreationCleanupState::Pending(cleanup),
                            })),
                        }
                    }));
                },
            );
        Ok(super::host::PreparedGeneratedView {
            view,
            created,
            binding: proposed,
            spare_actor: Some(successor_actor),
            installer,
            start,
        })
    }
    fn commit_generated_seed_owned(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        source: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        rollback: &super::host::EditorSeedRollbackStore,
    ) -> TabResult<()> {
        let seed_token = seed.as_ref().map(|_| Arc::new(()));
        let placement_token = placement.as_ref().map(|_| Arc::new(()));
        let mut previous_seed = None;
        let mut previous_placement = None;
        synthetic_created_effect(source, participants, created, cancel, pending, || {
            let mut world = self.0.w();
            if !world
                .generated_epochs
                .get(&view)
                .is_some_and(|epoch| epoch.same_generation(created))
            {
                return Err(TabError(
                    "Synthetic generated transfer has another epoch".into(),
                ));
            }
            if let Some(placement) = placement {
                let token = placement_token.as_ref().expect("prepared placement token");
                let prior = world.generated_placements.insert(view, placement);
                let prior_token = world.generated_placement_tokens.insert(view, token.clone());
                previous_placement = prior.map(|placement| (placement, prior_token));
            }
            if let Some(seed) = seed {
                match &seed {
                    main_core::shell::tab_manager::EditorSeed::Generate(value) => world.log(vec![
                        json!("queueGenerateHandoffFor"),
                        json!(view.0),
                        value.clone(),
                    ]),
                    main_core::shell::tab_manager::EditorSeed::Conversion(value) => {
                        world.log(vec![
                            json!("queueConvertedContent"),
                            json!(view.0),
                            value.clone(),
                        ])
                    }
                    main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content } => {
                        world.log(vec![
                            json!("queueDocsAiSeed"),
                            json!(view.0),
                            json!(new_blank),
                            json!(content),
                        ])
                    }
                }
                let token = seed_token.as_ref().expect("prepared seed token");
                let prior = world.generated.insert(view, seed);
                let prior_token = world.generated_tokens.insert(view, token.clone());
                previous_seed = prior.map(|seed| (seed, prior_token));
            }
            Ok(())
        })??;
        if let Some(token) = placement_token {
            rollback
                .borrow_mut()
                .push(std::rc::Rc::new(ProbePlacementRollback {
                    world: self.0.clone(),
                    view,
                    token,
                    created: created.clone(),
                    reservation: reservation.clone(),
                    owner: owner.clone(),
                    displaced: std::cell::RefCell::new(previous_placement),
                    completed: std::cell::Cell::new(false),
                }));
        }
        if let Some(token) = seed_token {
            rollback
                .borrow_mut()
                .push(std::rc::Rc::new(ProbeSeedRollback {
                    world: self.0.clone(),
                    view,
                    token,
                    created: created.clone(),
                    reservation: reservation.clone(),
                    owner: owner.clone(),
                    displaced: std::cell::RefCell::new(previous_seed),
                    completed: std::cell::Cell::new(false),
                }));
        }
        Ok(())
    }
    fn prepare_spare_editor_seed(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        new_blank: bool,
        target: &shell_contract::ipc::NativeRequestOwner,
        binding: &super::host::RetainedSpareBinding,
        rollback: &super::host::EditorSeedRollbackStore,
    ) -> TabResult<Box<dyn super::host::SpareSeedPlan>> {
        let created = target
            .context
            .renderer()
            .ok_or_else(|| TabError("Synthetic spare renderer unavailable".into()))?;
        if target.view != view
            || !binding.browser.reservation.matches(view, created)
            || !matches!(&binding.browser.registration, super::host::NativeBrowserRegistration::Registered(registered)
                if registered.view == view && registered.context.renderer().is_some_and(|generation| generation.same_generation(created)))
        {
            return Err(TabError("Synthetic spare seed target changed".into()));
        }
        let nudge = workbook.is_some();
        let mut plan = prepare_probe_seed(
            &self.0,
            view,
            created,
            &binding.browser.reservation,
            seed,
            None,
            docs_open_path,
            workbook,
            rollback,
        )?;
        if new_blank {
            plan.logs.push(vec![json!("setSheetsNewBlank")]);
        }
        if nudge {
            plan.nudge_target = Some(target.clone());
        }
        Ok(Box::new(plan))
    }
    fn commit_editor_seed_owned(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        source: &shell_contract::ipc::NativeRequestOwner,
        rollback: &super::host::EditorSeedRollbackStore,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()> {
        let nudge = workbook.is_some() && reused_target.is_some();
        synthetic_created_effect(source, &[], created, cancel, pending, || {
            let mut world = self.0.w();
            if let Some(path) = docs_open_path {
                world.log(vec![json!("queueDocsOpen"), json!(view.0), json!(path)]);
            }
            if let Some(path) = workbook {
                world.log(vec![
                    json!("queueWorkbookForView"),
                    json!(view.0),
                    json!(path),
                ]);
            }
            if let Some(seed) = seed {
                if let main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content } =
                    &seed
                {
                    world.log(vec![
                        json!("queueDocsAiSeed"),
                        json!(view.0),
                        json!(new_blank),
                        json!(content),
                    ]);
                }
                let token = Arc::new(());
                let prior = world.generated.insert(view, seed);
                let prior_token = world.generated_tokens.insert(view, token.clone());
                let displaced = prior.map(|seed| (seed, prior_token));
                rollback
                    .borrow_mut()
                    .push(std::rc::Rc::new(ProbeSeedRollback {
                        world: self.0.clone(),
                        view,
                        token,
                        created: created.clone(),
                        reservation: reservation.clone(),
                        owner: owner.clone(),
                        displaced: std::cell::RefCell::new(displaced),
                        completed: std::cell::Cell::new(false),
                    }));
            }
            Ok::<(), TabError>(())
        })??;
        if nudge {
            let target = reused_target
                .ok_or_else(|| TabError("Synthetic spare target is unavailable".into()))?;
            if target.view != view {
                return Err(TabError("Synthetic spare target changed".into()));
            }
            shell_contract::hosts::synthetic::admit_many(
                source,
                std::slice::from_ref(target),
                cancel,
                pending,
                || {
                    self.log("nudgeQueuedWorkbook", vec![json!(view.0)]);
                },
            )
            .map_err(TabError)?;
        }
        Ok(())
    }
    fn commit_native_editor_seed(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        rollback: &super::host::EditorSeedRollbackStore,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()> {
        use main_core::shell::tab_manager::EditorSeed;
        use shell_contract::{Surface, ipc::NativeEditorCommand};
        if reused_target.is_some_and(|owner| {
            owner.view != view
                || owner
                    .context
                    .renderer()
                    .is_none_or(|generation| !generation.same_generation(created))
        }) {
            return Err(TabError("Synthetic spare target changed".into()));
        }
        match (expected, &seed) {
            (NativeEditorCommand::OpenFile { surface, path }, None) => {
                let selected = path
                    .to_str()
                    .ok_or_else(|| TabError("Selected path is not UTF-8".into()))?;
                let valid = match surface {
                    Surface::Docs => {
                        docs_open_path.as_deref() == Some(selected) && workbook.is_none()
                    }
                    Surface::Sheets => {
                        workbook.as_deref() == Some(selected) && docs_open_path.is_none()
                    }
                    _ => docs_open_path.is_none() && workbook.is_none(),
                };
                if !valid {
                    return Err(TabError("Synthetic native editor input changed".into()));
                }
            }
            (
                NativeEditorCommand::NewDocument {
                    surface: Surface::Docs,
                    ..
                },
                Some(EditorSeed::DocsAi {
                    new_blank: true,
                    content: None,
                }),
            ) if docs_open_path.is_none() && workbook.is_none() => {}
            (NativeEditorCommand::NewDocument { surface, .. }, None)
                if *surface != Surface::Docs && docs_open_path.is_none() && workbook.is_none() => {}
            _ => {
                return Err(TabError(
                    "Synthetic native seed does not match command".into(),
                ));
            }
        }
        let participants = reused_target.map(std::slice::from_ref).unwrap_or(&[]);
        let effects = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| TabError("Synthetic owned effects are unavailable".into()))?;
        let nudge = workbook.is_some() && reused_target.is_some();
        let mut seed = seed;
        let token = Arc::new(());
        let mut displaced = None;
        effects
            .with_native_command_creation_effect(
                operation,
                expected,
                created,
                participants,
                cancel,
                pending,
                &mut || {
                    let mut world = self.0.w();
                    if let Some(path) = &docs_open_path {
                        world.log(vec![json!("queueDocsOpen"), json!(view.0), json!(path)]);
                    }
                    if let Some(path) = &workbook {
                        world.log(vec![
                            json!("queueWorkbookForView"),
                            json!(view.0),
                            json!(path),
                        ]);
                    }
                    if let Some(seed) = seed.take() {
                        let prior = world.generated.insert(view, seed);
                        let prior_token = world.generated_tokens.insert(view, token.clone());
                        displaced = Some(prior.map(|seed| (seed, prior_token)));
                    }
                    Ok(())
                },
            )
            .map_err(TabError)?;
        if let Some(displaced) = displaced {
            rollback
                .borrow_mut()
                .push(std::rc::Rc::new(ProbeSeedRollback {
                    world: self.0.clone(),
                    view,
                    token,
                    created: created.clone(),
                    reservation: reservation.clone(),
                    owner: owner.clone(),
                    displaced: std::cell::RefCell::new(displaced),
                    completed: std::cell::Cell::new(false),
                }));
        }
        if nudge {
            effects
                .emit_native_command_creation_many(
                    operation,
                    expected,
                    created,
                    participants,
                    0,
                    cancel,
                    pending,
                    "menu:action",
                    vec![shell_contract::IpcValue::String("open".into())],
                )
                .map_err(TabError)?;
        }
        Ok(())
    }
    fn reveal_written_export_owned(
        &self,
        path: std::path::PathBuf,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || [origin, destination]
                .iter()
                .any(|owner| owner.loss_token().is_none_or(|loss| loss.is_cancelled()))
        {
            return done(Err("The captured export reveal owner was released".into()));
        }
        self.log("showItemInFolder", vec![json!(path.to_string_lossy())]);
        done(Ok(()));
    }
    fn reveal_written_export_existing_document(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Owned export reveal effects are unavailable".into()));
        };
        let extra: Vec<_> = additional.iter().collect();
        let result = effects.with_existing_document_destination_effect(
            &selection,
            selection.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                self.log(
                    "showItemInFolder",
                    vec![json!(selection.path().to_string_lossy())],
                );
                Ok(())
            },
        );
        done(result);
    }
    fn reveal_written_publication(
        &self,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic export reveal authority unavailable".into()));
        };
        let extra: Vec<_> = additional.iter().collect();
        let mut accepted = false;
        let result = output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                accepted = true;
                Ok(())
            },
        );
        if let Err(error) = result {
            return done(Err(error));
        }
        if !accepted {
            return done(Err("Synthetic export reveal was not admitted".into()));
        }
        let world = self.0.w();
        if world.case["fail"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str() == Some("revealWritten"))
        }) {
            drop(world);
            return done(Err("Synthetic export reveal failed".into()));
        }
        world.log(vec![
            json!("showItemInFolder"),
            json!(output.path().to_string_lossy()),
        ]);
        drop(world);
        done(Ok(()));
    }
    fn rollback_generated_seed_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
    ) {
        let mut world = self.0.w();
        if world
            .generated_epochs
            .get(&view)
            .is_some_and(|epoch| epoch.same_generation(expected))
        {
            world.generated.remove(&view);
            world.generated_tokens.remove(&view);
            world.generated_epochs.remove(&view);
            world.generated_epoch_tokens.remove(&view);
            world.generated_placements.remove(&view);
            world.log(vec![json!("dropGenerateHandoff"), json!(view.0)]);
        }
    }
    fn open_opener_owned(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
        _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    ) {
        done(Err("no opener in the probe".into()));
    }
    fn open_opener_startup(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
        _: super::activation::StartupRecoveryConstraint,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    ) {
        done(Err("no opener in the probe".into()));
    }
    fn open_opener_native_command(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
        _: shell_contract::ipc::NativeInvocationOperation,
        _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    ) {
        done(Err("no opener in the probe".into()));
    }
    fn opener_needs_user_owned(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
        _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        done(Err("no opener in the probe".into()));
    }
    fn reveal_view_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Box<dyn FnOnce(TabResult<()>)>,
    ) {
        if !expected.is_active()
            || cancel.is_cancelled()
            || owner
                .context
                .renderer()
                .is_none_or(|renderer| !renderer.is_active())
        {
            then(Err(TabError(
                "Renderer document is no longer available.".into(),
            )));
        } else {
            self.log("ownedReveal", vec![json!(view.0)]);
            then(Ok(()));
        }
    }
    fn reveal_captured_many(
        &self,
        authority: super::activation::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: super::activation::TabUiDone,
    ) {
        if self.0.view_destroyed(target.view) {
            done(Err("Synthetic target view is unavailable".into()));
            return;
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            done(Err("Synthetic ownership service unavailable".into()));
            return;
        };
        let view = target.view;
        participants.push(target);
        let fake = self.0.clone();
        let reply = crate::home_files::back_to_core(self.1.core.clone(), done);
        self.1.ui.post(Box::new(move || {
            let action = Box::new(move || {
                fake.w().log(vec![json!("ownedReveal"), json!(view.0)]);
                Ok(())
            });
            reply(probe_activation_ui(
                &effects,
                &authority,
                &participants,
                &cancel,
                &pending,
                action,
            ));
        }));
    }
    fn adopt_recovery_view_owned(
        &self,
        shell: ShellWindow,
        authority: super::activation::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: super::activation::TabUiDone,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            done(Err("Synthetic ownership service unavailable".into()));
            return;
        };
        if self.0.view_destroyed(target.view)
            || self
                .1
                .views
                .sender_of(shell.view)
                .is_none_or(|sender| sender.window != shell.window)
        {
            done(Err("Synthetic recovery view or shell is unavailable".into()));
            return;
        }
        let shell_owner = self
            .1
            .views
            .renderer_generation(shell.view)
            .ok_or_else(|| "Synthetic recovery shell is unavailable".to_owned())
            .and_then(|generation| effects.capture_owner(shell.view, &generation));
        let shell_owner = match shell_owner {
            Ok(owner) => owner,
            Err(error) => {
                done(Err(error));
                return;
            }
        };
        let view = target.view;
        participants.extend([target, shell_owner]);
        let fake = self.0.clone();
        let reply = crate::home_files::back_to_core(self.1.core.clone(), done);
        self.1.ui.post(Box::new(move || {
            let action = Box::new(move || {
                fake.view_set_visible(view, false);
                fake.add_child_view(shell.window, view);
                fake.view_listen(view, ViewListen::HtmlFullScreen);
                fake.view_listen(view, ViewListen::PaneFocus);
                Ok(())
            });
            reply(probe_activation_ui(
                &effects,
                &authority,
                &participants,
                &cancel,
                &pending,
                action,
            ));
        }));
    }
    fn prepare_reopen_owned(
        &self,
        path: String,
        owner: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Box<
            dyn FnOnce(
                Result<
                    (
                        String,
                        Option<String>,
                        shell_contract::ipc::NativeRequestOwner,
                        shell_contract::runtime::CancelToken,
                    ),
                    String,
                >,
            ),
        >,
    ) {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || destination
                .loss_token()
                .is_none_or(|loss| loss.is_cancelled())
        {
            then(Err("Renderer document is no longer available.".into()));
        } else {
            then(Ok((path, None, owner, cancel)));
        }
    }
    fn prepare_reopen_existing_document(
        &self,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeDriveReopen, String>)>,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return then(Err("Native reopen effects are unavailable".into()));
        };
        let extra: Vec<_> = additional.iter().collect();
        let result = output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                if output.commit().path() != output.path() {
                    return Err("The committed output does not match its selection".into());
                }
                Ok(())
            },
        );
        then(result.map(|()| shell_contract::ipc::NativeDriveReopen::Unchanged(output)));
    }
    fn read_saved_input(
        &self,
        output: shell_contract::ipc::SavedPublication,
        path: shell_contract::ipc::NativeSavedPath,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeSavedInput, String>)>,
    ) {
        let done = crate::home_files::back_to_core(self.1.core.clone(), then);
        self.1.workers.spawn(Box::new(move || {
            use shell_contract::ipc::{NativeSavedInput, NativeSavedPath};
            let result = (|| -> Result<NativeSavedInput, String> {
                if !path.matches_publication(&output) {
                    return Err("The saved source does not match the committed output".into());
                }
                match &path {
                    NativeSavedPath::Selected(_) => output
                        .read_input(usize::MAX, &participants, &cancel, &pending, &additional)
                        .map_err(|error| error.to_string()),
                    NativeSavedPath::DriveMoved(receipt)
                    | NativeSavedPath::DriveFallback(receipt) => {
                        let mut controls = vec![cancel, pending];
                        controls.extend(additional);
                        controls.extend(output.loss_tokens()?);
                        for owner in &participants {
                            controls.push(
                                owner
                                    .loss_token()
                                    .ok_or("The saved-source participant is unavailable")?,
                            );
                            controls.push(
                                owner
                                    .context
                                    .renderer()
                                    .ok_or("The saved-source renderer is unavailable")?
                                    .loss_token(),
                            );
                        }
                        let read = receipt
                            .read_committed_reopen(
                                matches!(&path, NativeSavedPath::DriveMoved(_)),
                                usize::MAX,
                                &controls,
                            )
                            .map_err(|error| error.to_string())?;
                        Ok(NativeSavedInput::from_drive_committed_read(output, read))
                    }
                }
            })();
            done(result);
        }));
    }
    fn sheets_pending_edit_count(&self, wc_id: i64) -> f64 {
        self.log("sheetsPendingEditCount", vec![json!(wc_id)]);
        self.0.w().view(wc_id as u32).pending
    }
    fn family_is_dirty(&self, kind: TabKind, wc_id: i64) -> TabResult<bool> {
        self.log("isDirty", vec![json!(kind.as_str()), json!(wc_id)]);
        let mut world = self.0.w();
        let view = world.view(wc_id as u32);
        if view.query_fails {
            return Err(TabError("dirty query failed".into()));
        }
        Ok(view.dirty)
    }
    fn hwp_memo_modal_open(&self, wc_id: i64) -> bool {
        self.log("hwpMemoModalOpen", vec![json!(wc_id)]);
        self.0.w().view(wc_id as u32).memo
    }
    fn docs_query_dirty(&self, view: ViewId, then: Then<bool>) {
        self.log("docsQueryDirty", vec![json!(view.0)]);
        let answer = {
            let mut w = self.0.w();
            let state = w.view(view.0);
            if state.query_fails {
                Err(IpcError {
                    name: Some("Error".into()),
                    message: "query failed".into(),
                })
            } else {
                Ok(state.dirty)
            }
        };
        then(answer);
    }
    fn docs_query_dirty_owned(
        &self,
        view: ViewId,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        if let Err(error) = self.with_request_owner(&owner, &cancel, &mut || Ok(())) {
            then(Err(error));
            return;
        }
        self.docs_query_dirty(view, then);
    }
    fn docs_query_dirty_captured_many(
        &self,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        let error = |message: String| IpcError {
            name: Some("Error".into()),
            message,
        };
        if destination.view != view {
            return then(Err(error("synthetic Docs owner mismatch".into())));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return then(Err(error("synthetic ownership service unavailable".into())));
        };
        participants.push(destination);
        let mut answer = None;
        let admitted =
            effects.with_owned_effect_many(&origin, &participants, &cancel, &pending, &mut || {
                let mut world = self.0.w();
                world.log(vec![json!("docsQueryDirty"), json!(view.0)]);
                let state = world.view(view.0);
                answer = Some(if state.destroyed {
                    Err(error("synthetic Docs view destroyed".into()))
                } else if state.query_fails {
                    Err(error("query failed".into()))
                } else {
                    Ok(state.dirty)
                });
                Ok(())
            });
        then(match admitted {
            Err(message) => Err(error(message)),
            Ok(()) => answer.unwrap_or_else(|| Err(error("synthetic query not admitted".into()))),
        });
    }

    fn docs_request_close_captured_many(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        let error = |message: String| IpcError {
            name: Some("Error".into()),
            message,
        };
        if destination.view != view {
            return then(Err(error("synthetic Docs owner mismatch".into())));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return then(Err(error("synthetic ownership service unavailable".into())));
        };
        participants.push(destination);
        let mut answer = None;
        let admitted =
            effects.with_owned_effect_many(&origin, &participants, &cancel, &pending, &mut || {
                let mut world = self.0.w();
                if world.view(view.0).destroyed {
                    answer = Some(Err(error("synthetic Docs view destroyed".into())));
                } else {
                    let kind = world
                        .kinds
                        .get(&view.0)
                        .map_or("docs", |kind| kind.as_str());
                    let label = world.label_of(parent);
                    world.log(vec![
                        json!("requestClose"),
                        json!(kind),
                        json!(view.0),
                        label,
                    ]);
                    answer = Some(match world.close_answers.pop_front() {
                        Some(Value::String(value)) if value == "throws" => {
                            Err(error("prompt failed".into()))
                        }
                        Some(Value::Bool(value)) => Ok(value),
                        _ => Ok(true),
                    });
                }
                Ok(())
            });
        then(match admitted {
            Err(message) => Err(error(message)),
            Ok(()) => answer.unwrap_or_else(|| Err(error("synthetic close not admitted".into()))),
        });
    }
    fn with_request_owner(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        let loss = owner.loss_token();
        let generation = owner.context.renderer().ok_or_else(|| IpcError {
            name: Some("Error".into()),
            message: "synthetic request owner is unbound".into(),
        })?;
        shell_contract::runtime::RendererGeneration::with_active_all(
            [Some(generation), owner.context.document_lease()],
            || {
                shell_contract::runtime::CancelToken::with_active_all(
                    [Some(cancel), loss.as_ref()],
                    effect,
                )
            },
        )
        .flatten()
        .ok_or_else(|| IpcError {
            name: Some("Error".into()),
            message: "Renderer document is no longer available.".into(),
        })?
    }
    fn set_active_docs_resolver(&self, view: Option<ViewId>, null_if_destroyed: bool) {
        let id = match view {
            Some(v) if !(null_if_destroyed && self.0.w().view(v.0).destroyed) => json!(v.0),
            _ => Value::Null,
        };
        self.log("setActiveDocsResolver", vec![id]);
    }
    fn set_active_sheets_web_contents(&self, view: Option<ViewId>) {
        self.log(
            "setActiveSheetsWebContents",
            vec![view.map_or(Value::Null, |v| json!(v.0))],
        );
    }
    fn set_active_slides_web_contents(&self, view: ViewId) {
        self.log("setActiveSlidesWebContents", vec![json!(view.0)]);
    }
    fn apply_menu_for(&self, _kind: TabKind) {}
    fn teardown_docs_renderer(&self, view: ViewId) {
        self.log("teardownDocsRenderer", vec![json!(view.0)]);
    }
    fn release_closed_renderer(&self, view: ViewId) {
        self.log("releaseClosedRenderer", vec![json!(view.0)]);
    }
    fn mark_docs_new_blank(&self, wc_id: i64) {
        self.log("markDocsNewBlank", vec![json!(wc_id)]);
    }
    fn queue_docs_ai_content(
        &self,
        wc_id: i64,
        content: &Value,
    ) -> main_core::shell::tab_manager::TabResult<()> {
        self.log("queueDocsAiContent", vec![json!(wc_id), content.clone()]);
        Ok(())
    }
    fn queue_docs_copy(&self, wc_id: i64, path: &str, name: &str) {
        self.log(
            "queueDocsCopy",
            vec![json!(wc_id), json!(path), json!(name)],
        );
    }
    fn set_sheets_new_blank(&self) {
        self.log("setSheetsNewBlank", vec![]);
    }
    fn queue_workbook_for_view(&self, view: ViewId, path: &str) {
        self.log("queueWorkbookForView", vec![json!(view.0), json!(path)]);
    }
    fn nudge_queued_workbook(&self, view: ViewId) {
        self.log("nudgeQueuedWorkbook", vec![json!(view.0)]);
    }
    fn mark_pdf_untitled_copy(&self, path: &str) {
        self.log("markPdfUntitledCopy", vec![json!(path)]);
    }
    fn clear_pdf_dirty(&self, wc_id: i64) {
        self.log("clearPdfDirty", vec![json!(wc_id)]);
    }
    fn queue_markdown_seed_for(&self, view: ViewId, text: &str) {
        self.log("queueMarkdownSeedFor", vec![json!(view.0), json!(text)]);
    }
    fn queue_html_seed_for(&self, view: ViewId, text: &str) {
        self.log("queueHtmlSeedFor", vec![json!(view.0), json!(text)]);
    }
    fn queue_mindmap_seed_for(&self, view: ViewId, bytes: &[u8]) {
        self.log(
            "queueMindmapSeedFor",
            vec![json!(view.0), json!(hex(bytes))],
        );
    }
    fn queue_slides_seed_for(&self, view: ViewId, bytes: &[u8], suggested_name: &str) {
        self.log(
            "queueSlidesSeedFor",
            vec![
                json!(view.0),
                json!(hex(bytes)),
                json!({ "askPathOnFirstSave": true, "suggestedName": suggested_name }),
            ],
        );
    }
    fn queue_generate_handoff_for(&self, view: ViewId, handoff: &Value) {
        self.log(
            "queueGenerateHandoffFor",
            vec![json!(view.0), handoff.clone()],
        );
    }
    fn queue_converted_content(&self, view: ViewId, doc: &Value) {
        self.log("queueConvertedContent", vec![json!(view.0), doc.clone()]);
    }
    fn drop_generate_handoff(&self, wc_id: i64) {
        self.log("dropGenerateHandoff", vec![json!(wc_id)]);
    }
    fn diagram_tab_identity(&self, wc_id: i64) -> Option<DiagramIdentity> {
        self.log("diagramTabIdentity", vec![json!(wc_id)]);
        None
    }
    fn reset_sheets_shutting_down(&self) {
        self.log("resetSheetsShuttingDown", vec![]);
    }
    fn cancel_update_restart(&self) {
        self.log("cancelUpdateRestart", vec![]);
    }
    fn resume_discarded_copies(&self) {
        self.log("resumeDiscardedCopies", vec![]);
    }
    fn pdf_close_cancelled(&self, view: ViewId) {
        self.log("pdfCloseCancelled", vec![json!(view.0)]);
    }

    fn ui_scale(&self) -> f64 {
        self.0.w().case["shell"]["scale"].as_f64().unwrap_or(1.0)
    }
    fn no_spare_view_env(&self) -> bool {
        false
    }
    fn renderer_dev_url(&self) -> Option<String> {
        None
    }
    fn tm(&self, key: &str) -> String {
        self.0.w().string(key)
    }
    fn tm_params(&self, key: &str, params: &[(&str, &str)]) -> String {
        let mut text = self.0.w().string(key);
        for (name, value) in params {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }
    fn random_uuid(&self) -> String {
        let mut w = self.0.w();
        let n = w.next_uuid;
        w.next_uuid += 1;
        format!("00000000-0000-4000-8000-{n:012}")
    }
    fn warn(&self, message: &str, error: &str) {
        self.log("console", vec![json!(message), json!(error)]);
    }

    fn tabs_changed(&self, _changed: &TabsChanged) {}
    fn prepare_saved_publication_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String> {
        participants.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        effects.with_owned_effect_many_extra(
            &origin,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )?;
        let windows: HashMap<WindowId, super::window_snapshot::WindowReservation> = {
            let world = self.0.w();
            world
                .windows
                .iter()
                .map(|(id, window)| {
                    let id = WindowId(*id);
                    (
                        id,
                        super::window_snapshot::WindowReservation::new(id, window.proof.clone()),
                    )
                })
                .collect()
        };
        let core = self.1.core.clone();
        let session_origin = origin.clone();
        let session_participants = participants.clone();
        let session_cancel = cancel.clone();
        let session_pending = pending.clone();
        let session_additional = additional.clone();
        let session_effects = effects.clone();
        // This probe's existing hooks have no session, menus or recovery publishers.
        Ok(super::owned_saved::SavedPublication {
            publish: Rc::new(move |ack, done| {
                let extra = additional.iter().collect::<Vec<_>>();
                let result = ack.event_args().map_or_else(
                    || {
                        effects.with_owned_effect_many_extra(
                            &origin,
                            &participants,
                            &cancel,
                            &pending,
                            &extra,
                            &mut || Ok(()),
                        )
                    },
                    |args| {
                        effects.emit_owned_many_extra(
                            &origin,
                            &participants,
                            participants.len() - 1,
                            &cancel,
                            &pending,
                            &extra,
                            "tabs:changed",
                            args,
                        )
                    },
                );
                if let Err(error) = result {
                    return done(Err(error));
                }
                match ack.detached_title {
                    None => done(Ok(())),
                    Some((reservation, title)) => {
                        if !windows
                            .get(&reservation.window())
                            .is_some_and(|captured| captured.same_reservation(&reservation))
                        {
                            return done(Err("The synthetic detached window was replaced".into()));
                        }
                        let done = crate::home_files::back_to_core(core.clone(), done);
                        effects.post_ui_owned_many_extra(
                            origin.clone(),
                            participants.clone(),
                            cancel.clone(),
                            pending.clone(),
                            additional.clone(),
                            Box::new(move || reservation.set_title(&title)),
                            done,
                        );
                    }
                }
            }),
            session: Box::new(move |_| {
                let extra = session_additional.iter().collect::<Vec<_>>();
                session_effects.with_owned_effect_many_extra(
                    &session_origin,
                    &session_participants,
                    &session_cancel,
                    &session_pending,
                    &extra,
                    &mut || Ok(()),
                )?;
                Ok(shell_contract::hosts::SavedSessionReceipt::NoPublication)
            }),
        })
    }
    fn prepare_saved_publication_existing_document(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String> {
        participants.push(destination.binding().owner().clone());
        let extra = additional.iter().collect::<Vec<_>>();
        effects.with_existing_document_destination_effect(
            &destination,
            destination.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )?;
        let windows: HashMap<WindowId, super::window_snapshot::WindowReservation> = {
            let world = self.0.w();
            world
                .windows
                .iter()
                .map(|(id, window)| {
                    let id = WindowId(*id);
                    (
                        id,
                        super::window_snapshot::WindowReservation::new(id, window.proof.clone()),
                    )
                })
                .collect()
        };
        let core = self.1.core.clone();
        let session_destination = destination.clone();
        let session_participants = participants.clone();
        let session_cancel = cancel.clone();
        let session_pending = pending.clone();
        let session_additional = additional.clone();
        let session_effects = effects.clone();
        Ok(super::owned_saved::SavedPublication {
            publish: Rc::new(move |ack, done| {
                let extra = additional.iter().collect::<Vec<_>>();
                let result = ack.event_args().map_or_else(
                    || {
                        effects.with_existing_document_destination_effect(
                            &destination,
                            destination.path(),
                            &participants,
                            &cancel,
                            &pending,
                            &extra,
                            &mut || Ok(()),
                        )
                    },
                    |args| {
                        effects.emit_existing_document_save_many_extra(
                            destination.binding(),
                            &participants,
                            participants.len() - 1,
                            &cancel,
                            &pending,
                            &extra,
                            "tabs:changed",
                            args,
                        )
                    },
                );
                if let Err(error) = result {
                    return done(Err(error));
                }
                match ack.detached_title {
                    None => done(Ok(())),
                    Some((reservation, title)) => {
                        if !windows
                            .get(&reservation.window())
                            .is_some_and(|captured| captured.same_reservation(&reservation))
                        {
                            return done(Err("The synthetic detached window was replaced".into()));
                        }
                        let done = crate::home_files::back_to_core(core.clone(), done);
                        effects.post_ui_existing_document_save_extra(
                            destination.binding().clone(),
                            participants.clone(),
                            cancel.clone(),
                            pending.clone(),
                            additional.clone(),
                            Box::new(move || reservation.set_title(&title)),
                            done,
                        );
                    }
                }
            }),
            session: Box::new(move |_| {
                let extra = session_additional.iter().collect::<Vec<_>>();
                session_effects.with_existing_document_destination_effect(
                    &session_destination,
                    session_destination.path(),
                    &session_participants,
                    &session_cancel,
                    &session_pending,
                    &extra,
                    &mut || Ok(()),
                )?;
                Ok(shell_contract::hosts::SavedSessionReceipt::NoPublication)
            }),
        })
    }
    fn prepare_saved_publication_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String> {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_native_saved_document_input(
            &document,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut |_| Ok(()),
        )?;
        let windows: HashMap<WindowId, super::window_snapshot::WindowReservation> = {
            let world = self.0.w();
            world
                .windows
                .iter()
                .map(|(id, window)| {
                    let id = WindowId(*id);
                    (
                        id,
                        super::window_snapshot::WindowReservation::new(id, window.proof.clone()),
                    )
                })
                .collect()
        };
        let core = self.1.core.clone();
        let session_document = document.clone();
        let session_participants = participants.clone();
        let session_cancel = cancel.clone();
        let session_pending = pending.clone();
        let session_additional = additional.clone();
        let session_effects = effects.clone();
        Ok(super::owned_saved::SavedPublication {
            publish: Rc::new(move |ack, done| {
                let extra: Vec<_> = additional.iter().collect();
                let result = match (ack.event_args(), ack.shell) {
                    (Some(args), Some(shell)) => match participants
                        .iter()
                        .position(|owner| owner.view == shell.view)
                    {
                        Some(index) => effects.emit_native_saved_document_input(
                            &document,
                            &participants,
                            index,
                            &cancel,
                            &pending,
                            &extra,
                            "tabs:changed",
                            args,
                        ),
                        None => Err("Synthetic saved event recipient unavailable".into()),
                    },
                    _ => effects.with_native_saved_document_input(
                        &document,
                        &participants,
                        &cancel,
                        &pending,
                        &extra,
                        &mut |_| Ok(()),
                    ),
                };
                if let Err(error) = result {
                    return done(Err(error));
                }
                match ack.detached_title {
                    None => done(Ok(())),
                    Some((reservation, title)) => {
                        if !windows
                            .get(&reservation.window())
                            .is_some_and(|captured| captured.same_reservation(&reservation))
                        {
                            return done(Err("The synthetic detached window was replaced".into()));
                        }
                        let done = crate::home_files::back_to_core(core.clone(), done);
                        effects.post_ui_native_saved_document_input(
                            document.clone(),
                            participants.clone(),
                            cancel.clone(),
                            pending.clone(),
                            additional.clone(),
                            Box::new(move || reservation.set_title(&title)),
                            done,
                        );
                    }
                }
            }),
            session: Box::new(move |_| {
                let extra: Vec<_> = session_additional.iter().collect();
                session_effects.with_native_saved_document_input(
                    &session_document,
                    &session_participants,
                    &session_cancel,
                    &session_pending,
                    &extra,
                    &mut |_| Ok(()),
                )?;
                Ok(shell_contract::hosts::SavedSessionReceipt::NoPublication)
            }),
        })
    }
    fn prepare_saved_publication_recovery(
        &self,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String> {
        effects.with_recovery_open_effect_many(
            &receipt,
            &receiver,
            &[],
            &cancel,
            &pending,
            &mut || Ok(()),
        )?;
        let session_receipt = receipt.clone();
        let session_receiver = receiver.clone();
        let session_cancel = cancel.clone();
        let session_pending = pending.clone();
        let session_effects = effects.clone();
        Ok(super::owned_saved::SavedPublication {
            publish: Rc::new(move |ack, done| {
                let result = ack.event_args().map_or_else(
                    || {
                        effects.with_recovery_open_effect_many(
                            &receipt,
                            &receiver,
                            &[],
                            &cancel,
                            &pending,
                            &mut || Ok(()),
                        )
                    },
                    |args| {
                        effects.emit_recovery_open_many(
                            &receipt,
                            &receiver,
                            std::slice::from_ref(&receiver),
                            0,
                            &cancel,
                            &pending,
                            "tabs:changed",
                            args,
                        )
                    },
                );
                done(result);
            }),
            session: Box::new(move |_| {
                session_effects.with_recovery_open_effect_many(
                    &session_receipt,
                    &session_receiver,
                    &[],
                    &session_cancel,
                    &session_pending,
                    &mut || Ok(()),
                )?;
                Ok(shell_contract::hosts::SavedSessionReceipt::NoPublication)
            }),
        })
    }
    fn split_changed(&self, _state: Option<&SplitViewState>) {}
    fn detached_changed(&self) {}

    fn app_menu_popup(&self) -> Option<Vec<MenuEntry>> {
        let mut w = self.0.w();
        let menu = w.case["appMenu"].as_array()?.clone();
        w.app_labels.clear();
        let mut labels = Vec::new();
        fn build(items: &[Value], top: bool, labels: &mut Vec<String>) -> Vec<MenuEntry> {
            items
                .iter()
                .map(|item| {
                    let id = labels.len() as u32;
                    let label = item["label"].as_str().unwrap_or("").to_owned();
                    labels.push(label.clone());
                    let separator = item["type"].as_str() == Some("separator");
                    let sub = item["submenu"].as_array();
                    let submenu = sub.map(|s| build(s, false, labels)).unwrap_or_default();
                    MenuEntry {
                        id,
                        kind: if separator {
                            MenuEntryKind::Separator
                        } else {
                            MenuEntryKind::Normal
                        },
                        label,
                        // a menu that has a submenu is listed by its label alone
                        enabled: if top && sub.is_some() {
                            true
                        } else {
                            item["enabled"].as_bool().unwrap_or(true)
                        },
                        checked: false,
                        icon: None,
                        submenu,
                    }
                })
                .collect()
        }
        let mut entries = build(&menu, true, &mut labels);
        let id = labels.len() as u32;
        labels.push(String::new());
        entries.push(MenuEntry {
            id,
            kind: MenuEntryKind::Separator,
            label: String::new(),
            enabled: true,
            checked: false,
            icon: None,
            submenu: Vec::new(),
        });
        let id = labels.len() as u32;
        let check = w.string("menuCheckUpdates");
        labels.push(check.clone());
        entries.push(MenuEntry {
            id,
            kind: MenuEntryKind::Normal,
            label: check,
            enabled: true,
            checked: false,
            icon: None,
            submenu: Vec::new(),
        });
        w.app_labels = labels;
        Some(entries)
    }
    fn app_menu_activate(&self, id: u32, _cx: shell_contract::ipc::CallContext) {
        let label = self.0.w().app_labels.get(id as usize).cloned();
        if let Some(label) = label {
            self.log("appMenu.click", vec![json!(label)]);
        }
    }
    fn visible_home_kinds(&self) -> Vec<String> {
        self.0.w().case["homeKinds"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn drive_reopen(&self, _path: &str) -> bool {
        false
    }
    fn route_path_native(&self, path: String, done: Box<dyn FnOnce(Result<bool, String>)>) {
        self.log("routeDocumentPath", vec![json!(path)]);
        done(Ok(true));
    }
    fn issue_native_command(
        &self,
        _: shell_contract::ipc::NativeEditorCommand,
        _: shell_contract::runtime::CancelToken,
    ) -> Result<shell_contract::ipc::NativeInvocationOperation, String> {
        Err("Native commands are unavailable in the probe host".into())
    }
    fn record_recent_file(&self, path: &str) {
        self.log("recordRecentFile", vec![json!(path)]);
    }
    fn record_recent_file_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let matches_path = operation.is_root_native_command()
            && matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { path: selected, .. } if selected == std::path::Path::new(&path));
        if !matches_path {
            return done(Err(
                "Synthetic recent path does not match the native command".into(),
            ));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let result = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || {
                self.log("recordRecentFile", vec![json!(path)]);
                Ok(())
            },
        );
        done(result);
    }
    fn notify_unsupported_file(&self, path: &str) {
        let ext = std::path::Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let ext = if ext.is_empty() {
            path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
        } else {
            ext
        };
        let message = self.tm_params("errUnsupportedExt", &[("ext", &ext)]);
        self.show_app_warning(&message, None);
    }
    fn notify_unsupported_file_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !operation.is_root_native_command()
            || !matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { path: selected, .. } if selected == std::path::Path::new(&path))
        {
            return done(Err(
                "Synthetic warning path does not match the native command".into(),
            ));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let result = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || {
                self.notify_unsupported_file(&path);
                Ok(())
            },
        );
        done(result);
    }
    fn show_app_warning(&self, message: &str, detail: Option<&str>) {
        let w = self.0.w();
        let shell = w.shell.window.0;
        let mut options = Map::new();
        options.insert("type".into(), json!("warning"));
        options.insert("message".into(), json!(message));
        if let Some(detail) = detail.filter(|d| !d.is_empty()) {
            options.insert("detail".into(), json!(detail));
        }
        w.log(vec![json!("win"), json!(shell), json!("show")]);
        w.log(vec![json!("win"), json!(shell), json!("focus")]);
        w.log(vec![
            json!("dialog.showMessageBox"),
            json!(shell),
            Value::Object(options),
        ]);
    }
    fn show_app_warning_owned(
        &self,
        message: String,
        detail: Option<String>,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let result =
            effects.with_owned_effect_many(&origin, &participants, &cancel, &pending, &mut || {
                self.show_app_warning(&message, detail.as_deref());
                Ok(())
            });
        done(result);
    }
    fn show_app_warning_native(
        &self,
        message: String,
        detail: Option<String>,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !operation.is_root_native_command()
            || !matches!(
                &expected,
                shell_contract::ipc::NativeEditorCommand::OpenFile { .. }
            )
        {
            return done(Err(
                "Synthetic warning does not match the native command".into()
            ));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let result = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || {
                self.show_app_warning(&message, detail.as_deref());
                Ok(())
            },
        );
        done(result);
    }
    fn diagram_open_refusal_owned(
        &self,
        path: String,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let mut answer = None;
        let result =
            effects.with_owned_effect_many(&origin, &participants, &cancel, &pending, &mut || {
                let world = self.0.w();
                answer = Some(
                    world.case["refuse"]
                        .as_array()
                        .and_then(|refused| {
                            refused
                                .iter()
                                .find(|item| item["path"].as_str() == Some(path.as_str()))
                        })
                        .and_then(|item| item["reason"].as_str().map(str::to_owned)),
                );
                Ok(())
            });
        done(
            result.and_then(|()| answer.ok_or_else(|| "Synthetic refusal was not admitted".into())),
        );
    }
    fn diagram_open_refusal_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        if !operation.is_root_native_command()
            || !matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { path: selected, .. } if selected == std::path::Path::new(&path))
        {
            return done(Err(
                "Synthetic diagram path does not match the native command".into(),
            ));
        }
        let Some(effects) = self
            .1
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Synthetic ownership service unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let mut answer = None;
        let result = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || {
                let world = self.0.w();
                answer = Some(
                    world.case["refuse"]
                        .as_array()
                        .and_then(|refused| {
                            refused
                                .iter()
                                .find(|item| item["path"].as_str() == Some(path.as_str()))
                        })
                        .and_then(|item| item["reason"].as_str().map(str::to_owned)),
                );
                Ok(())
            },
        );
        done(
            result.and_then(|()| answer.ok_or_else(|| "Synthetic refusal was not admitted".into())),
        );
    }
    fn start_queued_workbook_nudge(&self) {}
    fn default_save_dir(&self) -> String {
        self.0.w().case["defaultSaveDir"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn blank_xlsx(&self) -> Result<Vec<u8>, String> {
        Ok(unhex(self.0.w().case["blankXlsx"].as_str().unwrap_or("")))
    }
    fn blank_pdf(&self) -> Vec<u8> {
        unhex(self.0.w().case["blankPdf"].as_str().unwrap_or(""))
    }
    fn unique_path_in(&self, dir: &str, name: &str) -> String {
        // docs-main's `uniquePathIn`
        let dot = name.rfind('.').filter(|i| *i > 0);
        let (base, ext) = match dot {
            Some(i) => (&name[..i], &name[i..]),
            None => (name, ""),
        };
        let mut candidate = format!("{dir}/{name}");
        let mut i = 2;
        while std::path::Path::new(&candidate).exists() {
            candidate = format!("{dir}/{base}-{i}{ext}");
            i += 1;
        }
        candidate
    }
    fn sheets_suggested_path_taken(&self, path: &str) -> bool {
        self.0.w().suggested.contains(path)
    }
    fn mark_sheets_unsaved_new(&self, _backing: &str, suggested: &str, _temp_dir: &str) {
        self.0.w().suggested.insert(suggested.to_owned());
    }
    fn unmark_sheets_unsaved_new(&self, _backing: &str) {}
    fn mark_sheets_untitled_path(&self, _path: &str) {}
    fn mark_pdf_untitled_path(&self, _path: &str) {}
    fn show_error_dialog(&self, title: &str, error: &str) {
        self.log("showErrorDialog", vec![json!(title), json!(error)]);
    }
    fn open_agent_window(&self) {
        self.log("openAgentWindow", vec![]);
    }
    fn drop_skill_file(&self, _path: &str, then: Box<dyn FnOnce(bool)>) {
        then(false);
    }
    fn drop_no_skills_found(&self) {}
    fn is_inside_root(&self, root: &str, path: &str) -> bool {
        path == root || path.starts_with(&format!("{root}/"))
    }
    fn open_file_via_dialog(&self) {
        self.log("openFileViaDialog", vec![]);
    }
    fn tab_document_action(
        &self,
        kind: TabKind,
        view: ViewId,
        tab_id: &str,
        file_path: Option<&str>,
        action: DocAction,
    ) {
        self.log(
            "tabDocumentAction",
            vec![
                json!(kind.as_str()),
                json!(view.0),
                json!(tab_id),
                opt_str(file_path),
                json!(format!("{action:?}")),
            ],
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The editors of the world: every view made by a `create*View` call
struct Editors(Fake, shell_contract::Surface);

fn probe_saved_error(message: impl Into<String>) -> IpcError {
    IpcError {
        name: Some("Error".into()),
        message: message.into(),
    }
}

struct ProbeSavedActor {
    fake: Fake,
    target: shell_contract::ipc::NativeSavedTarget,
    token: Arc<()>,
    committed: std::cell::Cell<bool>,
    retired: std::cell::Cell<bool>,
}

impl ProbeSavedActor {
    fn retire(&self) -> Result<(), IpcError> {
        if !self.committed.get() || self.retired.get() {
            return Err(probe_saved_error("Synthetic saved actor is not live"));
        }
        let mut world = self.fake.w();
        let view = self.target.view();
        if !world
            .saved_barriers
            .get(&view)
            .is_some_and(|(target, token)| {
                target.same_target(&self.target) && Arc::ptr_eq(token, &self.token)
            })
        {
            return Err(probe_saved_error("Synthetic saved actor was replaced"));
        }
        world.saved_barriers.remove(&view);
        world.kinds.remove(&view.0);
        self.retired.set(true);
        Ok(())
    }
}

impl shell_contract::editors::SavedActorRegistration for ProbeSavedActor {
    fn retire_closed(&self, receipt: shell_contract::ipc::NativeClosureReceipt, then: Then<()>) {
        if !receipt.matches(
            self.target.reservation(),
            self.target.view(),
            self.target.created(),
        ) {
            return then(Err(probe_saved_error(
                "Synthetic saved actor close proof does not match",
            )));
        }
        then(self.retire());
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        then: Then<()>,
    ) {
        if !proof.matches(self.target.reservation(), self.target.browser()) {
            return then(Err(probe_saved_error(
                "Synthetic saved actor replacement proof does not match",
            )));
        }
        then(self.retire());
    }
    fn rollback(&self, then: Then<()>) {
        if self.committed.get() {
            return then(Err(probe_saved_error(
                "Committed synthetic saved actor requires retirement proof",
            )));
        }
        self.retired.set(true);
        then(Ok(()));
    }
}

struct ProbePreparedSavedActor {
    receipt: Rc<ProbeSavedActor>,
    kind: TabKind,
}

impl Drop for ProbePreparedSavedActor {
    fn drop(&mut self) {
        if !self.receipt.committed.get() {
            self.receipt.retired.set(true);
        }
    }
}

impl shell_contract::editors::PreparedSavedDocument for ProbePreparedSavedActor {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        let receipt = &self.receipt;
        if receipt.committed.get()
            || receipt.retired.get()
            || !transition.matches(receipt.target.reservation())
            || !transition.matches_owner(receipt.target.browser())
        {
            return Err(probe_saved_error(
                "Synthetic saved actor transition changed",
            ));
        }
        {
            let world = receipt.fake.w();
            if world.kinds.contains_key(&receipt.target.view().0)
                || world.saved_barriers.contains_key(&receipt.target.view())
            {
                return Err(probe_saved_error(
                    "Synthetic saved actor is already installed",
                ));
            }
        }
        next()?;
        let mut world = receipt.fake.w();
        world.kinds.insert(receipt.target.view().0, self.kind);
        world.saved_barriers.insert(
            receipt.target.view(),
            (receipt.target.clone(), receipt.token.clone()),
        );
        receipt.committed.set(true);
        Ok(())
    }
    fn finish(self: Box<Self>) -> Rc<dyn shell_contract::editors::SavedActorRegistration> {
        self.receipt.clone()
    }
}

struct ProbeSavedActorRollback(Rc<dyn shell_contract::editors::SavedActorRegistration>);
impl super::host::EditorSeedRollback for ProbeSavedActorRollback {
    fn retire_closed(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0.retire_closed(
            receipt,
            Box::new(move |result| done(result.map_err(|error| error.message))),
        );
    }
    fn retire_adopted(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.retire_closed(receipt, done);
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0.retire_superseded(
            proof,
            Box::new(move |result| done(result.map_err(|error| error.message))),
        );
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.0.rollback(Box::new(move |result| {
            done(result.map_err(|error| error.message))
        }));
    }
}

impl DocumentEditors for Editors {
    fn saved_surface(&self) -> shell_contract::Surface {
        self.1
    }
    fn owns(&self, view: ViewId) -> bool {
        self.0
            .w()
            .kinds
            .get(&view.0)
            .is_some_and(|kind| fixture_surface(*kind) == self.1)
    }
    fn capture_document_owner(
        &self,
        _view: ViewId,
        native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, IpcError> {
        Ok(native)
    }
    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, IpcError> {
        if target.surface() != self.1 {
            return Err(probe_saved_error("Synthetic saved actor surface changed"));
        }
        let kind = match target.surface() {
            shell_contract::Surface::Docs => TabKind::Docs,
            shell_contract::Surface::Sheets => TabKind::Sheets,
            shell_contract::Surface::Slides => TabKind::Slides,
            shell_contract::Surface::Pdf => TabKind::Pdf,
            shell_contract::Surface::Markdown => TabKind::Markdown,
            shell_contract::Surface::Html => TabKind::Html,
            shell_contract::Surface::Mindmap => TabKind::Mindmap,
            shell_contract::Surface::Hwp => TabKind::Hwp,
            shell_contract::Surface::Epub => TabKind::Epub,
            shell_contract::Surface::Diagram => TabKind::Diagram,
            _ => {
                return Err(probe_saved_error(
                    "Unsupported synthetic saved actor surface",
                ));
            }
        };
        {
            let mut world = self.0.w();
            if world.kinds.contains_key(&target.view().0)
                || world.saved_barriers.contains_key(&target.view())
            {
                return Err(probe_saved_error("Synthetic saved actor already exists"));
            }
            world
                .kinds
                .try_reserve(1)
                .map_err(|error| probe_saved_error(error.to_string()))?;
            world
                .saved_barriers
                .try_reserve(1)
                .map_err(|error| probe_saved_error(error.to_string()))?;
        }
        let receipt = Rc::new(ProbeSavedActor {
            fake: self.0.clone(),
            target,
            token: Arc::new(()),
            committed: std::cell::Cell::new(false),
            retired: std::cell::Cell::new(false),
        });
        Ok(Box::new(ProbePreparedSavedActor { receipt, kind }))
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        let effects = self.0.w().owners.clone();
        let Some(effects) = effects else {
            return then(Err(probe_saved_error(
                "Synthetic saved authority unavailable",
            )));
        };
        let target = document.target();
        let additional: Vec<_> = target.additional().iter().collect();
        let mut outcome = None;
        let result = effects.with_native_saved_document_input(
            &document,
            &[],
            target.cancel(),
            target.pending(),
            &additional,
            &mut |input| {
                let mut world = self.0.w();
                if !world
                    .saved_barriers
                    .get(&target.view())
                    .is_some_and(|(captured, _)| captured.same_target(target))
                {
                    return Err("Synthetic saved input barrier was replaced".into());
                }
                let answer = world.saved_load_answers.pop_front();
                world.log(vec![
                    json!("savedInput"),
                    json!(target.view().0),
                    json!(input.bytes().len()),
                ]);
                outcome = Some(match answer {
                    Some(Value::String(answer)) if answer == "applied" => {
                        Ok(shell_contract::editors::SavedDocumentLoadOutcome::Applied)
                    }
                    Some(Value::String(answer)) if answer == "recovery" => {
                        Ok(shell_contract::editors::SavedDocumentLoadOutcome::RecoveryApplied)
                    }
                    Some(Value::String(answer)) if answer == "throws" => {
                        Err("Synthetic saved input failed".to_owned())
                    }
                    _ => Err("Synthetic saved input requires a scripted completion".to_owned()),
                });
                Ok(())
            },
        );
        then(
            result
                .and_then(|()| {
                    outcome.ok_or_else(|| "Synthetic saved input was not admitted".to_owned())?
                })
                .map_err(probe_saved_error),
        );
    }
    fn request_save(&self, _view: ViewId, _mode: SaveMode, then: Then<bool>) {
        then(Ok(true))
    }
    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.request_save(view, mode, then)
    }
    fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        let effects = self.0.w().owners.clone();
        let result = effects.ok_or_else(|| "Synthetic ownership service unavailable".to_owned()).and_then(|effects| {
            effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
                if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                    shell_contract::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: shell_contract::ipc::NativeSaveSelection::Mode(selected_mode), .. })
                    if *selected == view && *selected_mode == mode) || !self.owns(view) {
                    return Err("Synthetic editor save selection changed".into());
                }
                Ok(())
            })
        });
        then(result.map(|()| true).map_err(|message| IpcError {
            name: Some("Error".into()),
            message,
        }));
    }
    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        let answer = {
            let mut w = self.0.w();
            let kind = w.kinds.get(&view.0).map_or("docs", |k| k.as_str());
            let label = w.label_of(parent);
            w.log(vec![
                json!("requestClose"),
                json!(kind),
                json!(view.0),
                label,
            ]);
            w.close_answers.pop_front()
        };
        match answer {
            Some(Value::String(s)) if s == "throws" => then(Err(IpcError {
                name: Some("Error".into()),
                message: "prompt failed".into(),
            })),
            Some(Value::Bool(b)) => then(Ok(b)),
            _ => then(Ok(true)),
        }
    }
    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.request_close(view, parent, then)
    }
    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Ok(String::new()))
    }
    fn save_to_path(&self, _view: ViewId, _path: std::path::PathBuf, then: Then<()>) {
        then(Ok(()))
    }
    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: std::path::PathBuf,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        self.save_to_path(view, path, then)
    }
    fn save_to_path_native(
        &self,
        view: ViewId,
        path: std::path::PathBuf,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        let effects = self.0.w().owners.clone();
        let result = effects.ok_or_else(|| "Synthetic ownership service unavailable".to_owned()).and_then(|effects| {
            effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
                if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                    shell_contract::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: shell_contract::ipc::NativeSaveSelection::ExactPath(selected_path), .. })
                    if *selected == view && selected_path == &path) || !self.owns(view) {
                    return Err("Synthetic editor save selection changed".into());
                }
                Ok(())
            })
        });
        then(result.map_err(|message| IpcError {
            name: Some("Error".into()),
            message,
        }));
    }
    fn file_renamed(&self, _view: ViewId, _old: &std::path::Path, _new: &std::path::Path) {}
    fn file_renamed_owned(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        _destination: &shell_contract::ipc::NativeRequestOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        self.file_renamed(view, old_path, new_path);
        then(Ok(()))
    }
    fn file_renamed_many_owned_extra(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<()>,
    ) {
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(destination.clone());
        captured.extend_from_slice(participants);
        let extra = additional.iter().collect::<Vec<_>>();
        let effects = self.0.w().owners.clone();
        let result = effects
            .ok_or_else(|| "Synthetic ownership service unavailable".to_owned())
            .and_then(|effects| {
                effects.with_owned_effect_many_extra(
                    origin,
                    &captured,
                    cancel,
                    pending,
                    &extra,
                    &mut || {
                        self.file_renamed(view, old_path, new_path);
                        Ok(())
                    },
                )
            });
        then(result.map_err(|message| shell_contract::IpcError {
            name: Some("Error".into()),
            message,
        }));
    }
    fn file_renamed_existing_document(
        &self,
        view: ViewId,
        old: &std::path::Path,
        new: &std::path::Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: &std::path::Path,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        let mut captured = participants.to_vec();
        captured.push(destination.clone());
        let additional = additional.iter().collect::<Vec<_>>();
        let effects = self.0.w().owners.clone();
        let result = authority.snapshot().and_then(|snapshot| {
            effects
                .ok_or_else(|| "Synthetic ownership service unavailable".to_owned())
                .and_then(|effects| {
                    if snapshot.selection().path() != expected_path {
                        return Err("The native save path changed".into());
                    }
                    effects.with_native_save_revision_snapshot_effect(
                        &snapshot,
                        &captured,
                        cancel,
                        pending,
                        &additional,
                        &mut || {
                            if destination.view != view || !self.owns(view) {
                                return Err("Synthetic editor was replaced".into());
                            }
                            self.file_renamed(view, old, new);
                            Ok(())
                        },
                    )
                })
        });
        then(result.map(|()| None).map_err(|message| IpcError {
            name: Some("Error".into()),
            message,
        }));
    }
    fn file_renamed_recovery(
        &self,
        view: ViewId,
        old: &std::path::Path,
        new: &std::path::Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        let mut captured = participants.to_vec();
        captured.push(destination.clone());
        let result = shell_contract::hosts::synthetic::with_recovery_open_many(
            receipt,
            receiver,
            &captured,
            cancel,
            pending,
            &mut || {
                self.file_renamed(view, old, new);
                Ok(())
            },
        );
        then(result.map_err(|message| shell_contract::IpcError {
            name: Some("Error".into()),
            message,
        }));
    }
    fn export_request(&self, _view: ViewId, _format: &str) {}
    fn document_path(&self, _view: ViewId) -> Option<std::path::PathBuf> {
        None
    }
}

/// What `probe_hosts` leaves for `register`: the world of this case
struct WorldBox(Fake);

/// Inserts the world as the batch's [`WindowsHost`], answering from `case`
pub fn probe_hosts(hosts: &mut Hosts, case: &Value) {
    let log = hosts.get::<ProbeLog>().cloned().unwrap_or_default();
    let fake = Fake(Arc::new(Mutex::new(World::new(case, log))));
    let windows: WindowsHostRef = Arc::new(fake.clone());
    hosts.insert(windows);
    hosts.insert(Arc::new(WorldBox(fake)));
}

/// What `register` adds for the shell: the hooks and the editors of the world
pub(super) fn provide_hooks(provided: &Rc<Provided>, services: &Services) {
    if !provided.all::<dyn TabHooks>().is_empty() {
        return;
    }
    let Some(world) = services.hosts.get::<Arc<WorldBox>>() else {
        return;
    };
    {
        let mut fake = world.0.w();
        fake.registrar = services.hosts.get::<ProbeViewRegistrar>().cloned();
        fake.owners = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned();
    }
    provided.add::<dyn TabHooks>(Rc::new(Hooks(world.0.clone(), services.clone())));
    for surface in [
        shell_contract::Surface::Shell,
        shell_contract::Surface::Docs,
        shell_contract::Surface::Sheets,
        shell_contract::Surface::Slides,
        shell_contract::Surface::Pdf,
        shell_contract::Surface::Markdown,
        shell_contract::Surface::Html,
        shell_contract::Surface::Mindmap,
        shell_contract::Surface::Hwp,
        shell_contract::Surface::Epub,
        shell_contract::Surface::Diagram,
    ] {
        provided.add::<dyn DocumentEditors>(Rc::new(Editors(world.0.clone(), surface)));
    }
}

fn install_registered_shell(
    state: &super::TabsState,
    views: &dyn shell_contract::runtime::Views,
    effects: &shell_contract::hosts::OwnedEffectsHost,
    world: &WorldBox,
    sender: shell_contract::ipc::Sender,
    generation: shell_contract::runtime::RendererGeneration,
) -> Result<(), String> {
    let (shell, shutdown, installed) = {
        let fake = world.0.w();
        (
            fake.shell,
            fake.cleanup_lifetime.clone(),
            fake.startup_generation.clone(),
        )
    };
    if sender.view != shell.view
        || sender.window != shell.window
        || sender.surface != shell_contract::Surface::Shell
    {
        return Err("The synthetic first shell registration does not match".into());
    }
    let current = views
        .sender_of(sender.view)
        .ok_or("The synthetic shell registration was removed")?;
    if current.view != sender.view
        || current.window != sender.window
        || current.surface != sender.surface
        || !views
            .renderer_generation(sender.view)
            .is_some_and(|current| current.same_generation(&generation))
    {
        return Err("The synthetic first shell registration was replaced".into());
    }
    if let Some(installed) = installed {
        return if installed.same_generation(&generation) {
            Ok(())
        } else {
            Err("The synthetic first shell generation changed".into())
        };
    }
    let owner = effects.capture_owner(sender.view, &generation)?;
    let issuer = Arc::new(shell_contract::ipc::NativeInvocationIssuer::new(shutdown));
    let root = issuer.issue_startup(
        Vec::new(),
        false,
        owner,
        shell_contract::runtime::CancelToken::default(),
    );
    state.install_spare_creator_authority(root)?;
    let mut fake = world.0.w();
    fake.startup_issuer = Some(issuer);
    fake.startup_generation = Some(generation);
    Ok(())
}

pub(super) fn install_first_shell(
    state: &Rc<super::TabsState>,
    services: &Services,
) -> Result<(), String> {
    let Some(world) = services.hosts.get::<Arc<WorldBox>>().cloned() else {
        return Ok(());
    };
    let shell = world.0.w().shell;
    if let Some(sender) = services.views.sender_of(shell.view)
        && sender.window == shell.window
        && sender.surface == shell_contract::Surface::Shell
    {
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Synthetic ownership unavailable")?;
        let generation = services
            .views
            .renderer_generation(shell.view)
            .ok_or("The synthetic shell generation is unavailable")?;
        return install_registered_shell(
            state,
            services.views.as_ref(),
            &effects,
            &world,
            sender,
            generation,
        );
    }
    let Some(events) = services.hosts.get::<Arc<ProbeShellRegistration>>().cloned() else {
        return Ok(());
    };
    let effects = services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
        .ok_or("Synthetic ownership unavailable")?;
    let weak = Rc::downgrade(state);
    let views = services.views.clone();
    let weak_world = Arc::downgrade(&world);
    let callback = crate::home_files::back_to_core(
        services.core.clone(),
        move |event: Option<ProbeShellEvent>| {
            let Some((sender, generation)) = event else {
                return;
            };
            if let (Some(state), Some(world)) = (weak.upgrade(), weak_world.upgrade())
                && let Err(error) = install_registered_shell(
                    &state,
                    views.as_ref(),
                    &effects,
                    &world,
                    sender,
                    generation,
                )
            {
                world.0.w().startup_error = Some(error);
            }
        },
    );
    events.arm(shell.view, shell.window, callback)?;
    if let Some(sender) = services.views.sender_of(shell.view)
        && sender.window == shell.window
        && sender.surface == shell_contract::Surface::Shell
        && let Some(generation) = services.views.renderer_generation(shell.view)
    {
        events.registered(sender, generation);
    }
    Ok(())
}

pub fn probe_state(
    services: &Services,
    provided: Rc<Provided>,
) -> Result<Rc<super::TabsState>, shell_contract::ipc::RegisterError> {
    provide_hooks(&provided, services);
    let windows = services
        .hosts
        .need::<WindowsHostRef>(shell_contract::Batch::W)?;
    let hooks = provided.all::<dyn TabHooks>().into_iter().next().ok_or(
        shell_contract::ipc::RegisterError::MissingHost {
            batch: shell_contract::Batch::W,
            host: "shell_ipc_w::tabs::TabHooks",
        },
    )?;
    let state = super::TabsState::new(services, windows, hooks, provided.clone());
    install_first_shell(&state, services).map_err(|message| {
        shell_contract::ipc::RegisterError::Initialization {
            batch: shell_contract::Batch::W,
            host: "shell_ipc_w::tabs::probe::first_shell",
            message,
        }
    })?;
    provided.add::<super::TabsState>(state.clone());
    Ok(state)
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use shell_contract::{Surface, ipc::NativeEditorCommand};

    /// A retirement is a terminal close of exactly this reservation, view and generation.
    fn closed_matches(
        retirement: &shell_contract::ipc::NativeBrowserRetirement,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        view: ViewId,
        generation: &shell_contract::runtime::RendererGeneration,
    ) -> bool {
        matches!(
            retirement,
            shell_contract::ipc::NativeBrowserRetirement::Closed(receipt)
                if receipt.matches(reservation, view, generation)
        )
    }

    #[test]
    fn first_shell_registration_delivers_only_the_exact_event_once() {
        let events = ProbeShellRegistration::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let result = seen.clone();
        events
            .arm(
                ViewId(7),
                WindowId(3),
                Box::new(move |event| {
                    if let Some(event) = event {
                        result.lock().unwrap().push(event);
                    }
                }),
            )
            .unwrap();
        let generation = shell_contract::runtime::RendererGeneration::new();
        events.registered(
            shell_contract::ipc::Sender {
                view: ViewId(7),
                window: WindowId(3),
                surface: Surface::Docs,
            },
            generation.clone(),
        );
        events.registered(
            shell_contract::ipc::Sender {
                view: ViewId(8),
                window: WindowId(3),
                surface: Surface::Shell,
            },
            generation.clone(),
        );
        assert!(seen.lock().unwrap().is_empty());
        let sender = shell_contract::ipc::Sender {
            view: ViewId(7),
            window: WindowId(3),
            surface: Surface::Shell,
        };
        events.registered(sender, generation.clone());
        events.registered(sender, shell_contract::runtime::RendererGeneration::new());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0.view, sender.view);
        assert!(seen[0].1.same_generation(&generation));
    }

    struct TestThread;
    impl shell_contract::runtime::Thread for TestThread {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }
        fn is_current(&self) -> bool {
            true
        }
    }
    #[derive(Default)]
    struct QueuedUi(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
    impl shell_contract::runtime::Thread for QueuedUi {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            self.0.lock().unwrap().push(job);
        }
        fn is_current(&self) -> bool {
            false
        }
    }
    impl QueuedUi {
        fn run(&self) {
            let jobs = std::mem::take(&mut *self.0.lock().unwrap());
            for job in jobs {
                job();
            }
        }
    }
    struct TestWorkers;
    impl shell_contract::runtime::Workers for TestWorkers {
        fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }
    }
    #[test]
    fn generated_open_cancelled_before_admission_starts_no_view() {
        use main_core::packages::electron_utils::generate_handoff::GenerateHandoffTarget;
        use shell_contract::runtime::{CancelToken, RendererGeneration};

        let views = Arc::new(RegisteredViews::default());
        let shell = shell_contract::ipc::Sender {
            view: ViewId(1),
            window: WindowId(1),
            surface: Surface::Shell,
        };
        let generation = RendererGeneration::new();
        views
            .0
            .lock()
            .unwrap()
            .insert(shell.view, (shell, generation.clone()));
        let registrar_views = views.clone();
        let registrar: ProbeViewRegistrar = Arc::new(move |sender, generation| {
            registrar_views
                .0
                .lock()
                .unwrap()
                .insert(sender.view, (sender, generation));
            Ok(())
        });
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({}));
        hosts.insert(registrar);
        let ui = Arc::new(QueuedUi::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: ui.clone(),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views,
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        let fake = services.hosts.get::<Arc<WorldBox>>().unwrap().0.clone();
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
        let owner = effects.capture_owner(shell.view, &generation).unwrap();
        let next_view = fake.w().next_view;
        let cancel = CancelToken::default();
        cancel.cancel();
        let result = Rc::new(std::cell::RefCell::new(None));
        let observed = result.clone();
        state.open_generated_editor_owned(
            super::super::open::GeneratedEditorOpen::Generate {
                target: GenerateHandoffTarget::Slides,
                handoff: json!({"text": "fixture"}),
                opener: Some(main_core::shell::projects::new_documents::NewDocumentOpener::Home),
            },
            owner.clone(),
            cancel,
            Box::new(move |reply| *observed.borrow_mut() = Some(reply.map(|tab| tab.view()))),
        );
        ui.run();
        assert!(
            matches!(&*result.borrow(), Some(Err(error)) if error == "The initialized tab operation is no longer active")
        );
        assert_eq!(fake.w().next_view, next_view);
        assert!(fake.w().generated.is_empty());

        let pending = CancelToken::default();
        pending.cancel();
        let result = Rc::new(std::cell::RefCell::new(None));
        let observed = result.clone();
        state.open_initialized_editor_owned(
            main_core::shell::tab_manager::EditorOpen::Slides { path: None },
            owner,
            Vec::new(),
            CancelToken::default(),
            pending,
            Some(main_core::shell::projects::new_documents::NewDocumentOpener::Home),
            Box::new(move |reply| *observed.borrow_mut() = Some(reply.map(|tab| tab.view()))),
        );
        ui.run();
        assert!(
            matches!(&*result.borrow(), Some(Err(error)) if error == "The initialized tab operation is no longer active")
        );
        assert_eq!(fake.w().next_view, next_view);
        assert!(fake.w().generated.is_empty());
    }

    #[test]
    fn canceled_generated_open_retires_its_view_and_preserves_existing_tab() {
        use main_core::packages::electron_utils::generate_handoff::GenerateHandoffTarget;
        use shell_contract::runtime::{CancelToken, RendererGeneration};

        let views = Arc::new(RegisteredViews::default());
        let shell = shell_contract::ipc::Sender {
            view: ViewId(1),
            window: WindowId(1),
            surface: Surface::Shell,
        };
        let shell_generation = RendererGeneration::new();
        views
            .0
            .lock()
            .unwrap()
            .insert(shell.view, (shell, shell_generation.clone()));
        let registrar_views = views.clone();
        let registrar: ProbeViewRegistrar = Arc::new(move |sender, generation| {
            registrar_views
                .0
                .lock()
                .unwrap()
                .insert(sender.view, (sender, generation));
            Ok(())
        });
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({}));
        hosts.insert(registrar);
        let ui = Arc::new(QueuedUi::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: ui.clone(),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views,
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        let fake = services.hosts.get::<Arc<WorldBox>>().unwrap().0.clone();
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
        let owner = effects
            .capture_owner(shell.view, &shell_generation)
            .unwrap();
        let request = || super::super::open::GeneratedEditorOpen::Generate {
            target: GenerateHandoffTarget::Slides,
            handoff: json!({"text": "fixture"}),
            opener: None,
        };
        let first = Rc::new(std::cell::RefCell::new(None));
        let first_result = first.clone();
        state.open_generated_editor_owned(
            request(),
            owner.clone(),
            CancelToken::default(),
            Box::new(move |result| {
                *first_result.borrow_mut() =
                    Some(result.map(|tab| (tab.tab_id().to_owned(), tab.view())));
            }),
        );
        for _ in 0..64 {
            if first.borrow().is_some() {
                break;
            }
            ui.run();
        }
        let (existing_id, existing_view) = first.borrow_mut().take().unwrap().unwrap();
        let existing_generation = services.views.renderer_generation(existing_view).unwrap();
        let cancel = CancelToken::default();
        let second = Rc::new(std::cell::RefCell::new(None));
        let second_result = second.clone();
        state.open_generated_editor_owned(
            request(),
            owner,
            cancel.clone(),
            Box::new(move |result| {
                *second_result.borrow_mut() = Some(result.map(|tab| tab.view()))
            }),
        );
        let generated = {
            let inner = state.inner.borrow();
            let manager = inner.manager.as_ref().unwrap();
            let active = manager.list().into_iter().find(|tab| tab.active).unwrap();
            assert_ne!(active.id, existing_id);
            manager.web_contents_for_tab(&active.id).unwrap()
        };
        assert!(second.borrow().is_none());
        cancel.cancel();
        for _ in 0..64 {
            ui.run();
            state.drain_creation_cleanups();
        }
        assert!(matches!(&*second.borrow(), Some(Err(_))));
        assert!(fake.view_destroyed(generated));
        assert!(!fake.w().generated.contains_key(&generated));
        let inner = state.inner.borrow();
        let manager = inner.manager.as_ref().unwrap();
        assert!(
            manager
                .list()
                .iter()
                .all(|tab| manager.web_contents_for_tab(&tab.id) != Some(generated))
        );
        assert_eq!(
            manager.web_contents_for_tab(&existing_id),
            Some(existing_view)
        );
        assert!(existing_generation.is_active());
        assert!(!fake.view_destroyed(existing_view));
    }

    #[test]
    fn help_open_cancelled_before_admission_starts_no_view() {
        use shell_contract::runtime::{CancelToken, RendererGeneration};

        let views = Arc::new(RegisteredViews::default());
        let shell = shell_contract::ipc::Sender {
            view: ViewId(1),
            window: WindowId(1),
            surface: Surface::Shell,
        };
        let generation = RendererGeneration::new();
        views
            .0
            .lock()
            .unwrap()
            .insert(shell.view, (shell, generation.clone()));
        let registrar_views = views.clone();
        let registrar: ProbeViewRegistrar = Arc::new(move |sender, generation| {
            registrar_views
                .0
                .lock()
                .unwrap()
                .insert(sender.view, (sender, generation));
            Ok(())
        });
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({}));
        hosts.insert(registrar);
        let ui = Arc::new(QueuedUi::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: ui.clone(),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views,
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        let fake = services.hosts.get::<Arc<WorldBox>>().unwrap().0.clone();
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
        let owner = effects.capture_owner(shell.view, &generation).unwrap();
        let next_view = fake.w().next_view;
        let cancel = CancelToken::default();
        cancel.cancel();
        let result = Rc::new(std::cell::RefCell::new(None));
        let observed = result.clone();
        state.open_help_owned(
            Some("diagram".into()),
            owner,
            cancel,
            CancelToken::default(),
            Box::new(move |reply| *observed.borrow_mut() = Some(reply.map_err(|error| error.0))),
        );
        ui.run();
        assert!(matches!(&*result.borrow(), Some(Err(_))));
        assert_eq!(fake.w().next_view, next_view);
    }

    struct TestEvents;
    impl shell_contract::ipc::EventSink for TestEvents {
        fn emit(
            &self,
            _: shell_contract::ipc::Target,
            _: &'static str,
            _: Vec<shell_contract::IpcValue>,
        ) {
        }
    }
    struct TestViews;
    impl shell_contract::runtime::Views for TestViews {
        fn sender_of(&self, _: ViewId) -> Option<shell_contract::ipc::Sender> {
            None
        }
        fn all(&self) -> Vec<shell_contract::ipc::Sender> {
            Vec::new()
        }
        fn on_destroyed(&self, _: Box<dyn Fn(ViewId) + Send + Sync>) {}
        fn renderer_generation(
            &self,
            _: ViewId,
        ) -> Option<shell_contract::runtime::RendererGeneration> {
            None
        }
        fn on_renderer_replaced(
            &self,
            _: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
        ) {
        }
    }
    struct BoundViews(
        shell_contract::ipc::Sender,
        shell_contract::runtime::RendererGeneration,
    );
    impl shell_contract::runtime::Views for BoundViews {
        fn sender_of(&self, view: ViewId) -> Option<shell_contract::ipc::Sender> {
            (view == self.0.view).then_some(self.0)
        }
        fn all(&self) -> Vec<shell_contract::ipc::Sender> {
            vec![self.0]
        }
        fn on_destroyed(&self, _: Box<dyn Fn(ViewId) + Send + Sync>) {}
        fn renderer_generation(
            &self,
            view: ViewId,
        ) -> Option<shell_contract::runtime::RendererGeneration> {
            (view == self.0.view).then(|| self.1.clone())
        }
        fn on_renderer_replaced(
            &self,
            _: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
        ) {
        }
    }

    #[derive(Default)]
    struct RegisteredViews(
        Mutex<
            HashMap<
                ViewId,
                (
                    shell_contract::ipc::Sender,
                    shell_contract::runtime::RendererGeneration,
                ),
            >,
        >,
    );
    impl shell_contract::runtime::Views for RegisteredViews {
        fn sender_of(&self, view: ViewId) -> Option<shell_contract::ipc::Sender> {
            self.0.lock().unwrap().get(&view).map(|(sender, _)| *sender)
        }
        fn all(&self) -> Vec<shell_contract::ipc::Sender> {
            self.0
                .lock()
                .unwrap()
                .values()
                .map(|(sender, _)| *sender)
                .collect()
        }
        fn renderer_generation(
            &self,
            view: ViewId,
        ) -> Option<shell_contract::runtime::RendererGeneration> {
            self.0
                .lock()
                .unwrap()
                .get(&view)
                .map(|(_, generation)| generation.clone())
        }
        fn on_destroyed(&self, _: Box<dyn Fn(ViewId) + Send + Sync>) {}
        fn on_renderer_replaced(
            &self,
            _: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
        ) {
        }
    }
    struct TestPaths;
    impl shell_contract::runtime::AppPaths for TestPaths {
        fn path(&self, _: shell_contract::runtime::PathName) -> std::path::PathBuf {
            std::path::PathBuf::from("/synthetic")
        }
    }

    #[cfg(unix)]
    #[test]
    fn saved_creator_adopts_then_registers_before_scripted_input_completion() {
        use shell_contract::ipc::{
            NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection, NativeSavedInput,
            NativeSavedPath,
        };
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        use std::{
            fs::{self, File},
            time::{SystemTime, UNIX_EPOCH},
        };
        let dir = std::env::temp_dir().join(format!(
            "probe-saved-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("saved.docx");
        let views = Arc::new(RegisteredViews::default());
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({"savedLoadAnswers":["applied"]}));
        let registrar_views = views.clone();
        let registrar: ProbeViewRegistrar = Arc::new(move |sender, generation| {
            registrar_views
                .0
                .lock()
                .unwrap()
                .insert(sender.view, (sender, generation));
            Ok(())
        });
        hosts.insert(registrar);
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: Arc::new(TestThread),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views: views.clone(),
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let provided = Rc::new(Provided::default());
        provide_hooks(&provided, &services);
        let fake = services.hosts.get::<Arc<WorldBox>>().unwrap().0.clone();
        let shell = fake.w().shell;
        let shell_generation = RendererGeneration::new();
        views.0.lock().unwrap().insert(
            shell.view,
            (
                shell_contract::ipc::Sender {
                    view: shell.view,
                    window: shell.window,
                    surface: Surface::Shell,
                },
                shell_generation.clone(),
            ),
        );
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
        let shell_owner = effects
            .capture_owner(shell.view, &shell_generation)
            .unwrap();
        let mut document_owner = shell_owner.clone();
        document_owner.context = document_owner
            .context
            .with_document_lease(RendererGeneration::new());
        let binding = NativeInvocationIssuer::new(CancelToken::default())
            .issue_existing_document_save(
                Surface::Docs,
                shell.view,
                NativeSaveSelection::ExactPath(path.clone()),
                document_owner,
                CancelToken::default(),
            )
            .unwrap();
        let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let parent = File::open(&dir).unwrap();
        let write = effects
            .begin_native_direct_write(
                &selection,
                &[],
                &cancel,
                &pending,
                &[],
                file_commit::prepare_direct(&parent, &path).unwrap(),
            )
            .unwrap();
        let written = match write.write_owned(b"synthetic saved input".to_vec()) {
            Ok(written) => written,
            Err(_) => panic!("fixture write failed"),
        };
        let (output, snapshot) = match written.finish(effects.as_ref()) {
            shell_contract::ipc::DirectWriteCompletion::Committed {
                publication,
                snapshot,
            } => (publication, snapshot),
            _ => panic!("fixture publication failed"),
        };
        let input = NativeSavedInput::from_written_snapshot(output.clone(), snapshot).unwrap();
        let hooks = provided.all::<dyn TabHooks>().into_iter().next().unwrap();
        let captured = hooks.capture_saved_shell(shell, shell_owner).unwrap();
        let rollback: super::super::host::EditorSeedRollbackStore =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let prepared = captured
            .prepare_fresh(
                &ViewSpec::Docs { path: None },
                input.clone(),
                NativeSavedPath::Selected(output.clone()),
                vec![],
                cancel.clone(),
                pending.clone(),
                vec![],
                rollback.clone(),
            )
            .unwrap();
        let view = prepared.target.view();
        assert!(!fake.w().kinds.contains_key(&view.0));
        let adopted = Rc::new(std::cell::Cell::new(false));
        let registered = Rc::new(std::cell::Cell::new(false));
        let adopted_callback = adopted.clone();
        let registered_callback = registered.clone();
        let fake_at_adoption = fake.clone();
        let fake_at_registration = fake.clone();
        let result = Rc::new(std::cell::RefCell::new(None));
        let result_slot = result.clone();
        (prepared.start)(
            Box::new(move |target, transition, actor| {
                assert_eq!(target.view(), view);
                assert!(transition.matches(target.reservation()));
                actor(&mut || {
                    assert!(!fake_at_adoption.w().kinds.contains_key(&view.0));
                    adopted_callback.set(true);
                    Ok(())
                })
            }),
            Box::new(move |target, owner, transition, register| {
                assert_eq!(target.view(), owner.view);
                assert!(transition.matches(target.reservation()));
                assert!(fake_at_registration.w().kinds.contains_key(&view.0));
                registered_callback.set(true);
                register()
            }),
            Arc::new(|done| done(Err("unused cleanup".into()))),
            Box::new(move |value| *result_slot.borrow_mut() = Some(value)),
        );
        assert!(adopted.get() && registered.get());
        let ready = match result.borrow_mut().take() {
            Some(Ok(ready)) => ready,
            _ => panic!("fixture saved load did not complete"),
        };
        assert!(matches!(
            ready.view.outcome,
            shell_contract::editors::SavedDocumentLoadOutcome::Applied
        ));
        assert_eq!(ready.view.document.target().view(), view);
        assert_eq!(rollback.borrow().len(), 1);
        let cancelled = CancelToken::default();
        let refused = captured
            .prepare_fresh(
                &ViewSpec::Docs { path: None },
                input,
                NativeSavedPath::Selected(output),
                vec![],
                cancelled.clone(),
                pending,
                vec![],
                rollback,
            )
            .unwrap();
        let refused_view = refused.target.view();
        cancelled.cancel();
        let denied = Rc::new(std::cell::Cell::new(false));
        let denied_slot = denied.clone();
        (refused.start)(
            Box::new(|_, _, _| panic!("cancelled adoption must not run")),
            Box::new(|_, _, _, _| panic!("cancelled registration must not run")),
            Arc::new(|done| done(Err("unused cleanup".into()))),
            Box::new(move |value| denied_slot.set(value.is_err())),
        );
        assert!(denied.get());
        assert!(!fake.w().kinds.contains_key(&refused_view.0));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_spare_replaces_its_browser_owner_and_installs_the_barrier_before_load() {
        use shell_contract::ipc::{
            NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection, NativeSavedInput,
            NativeSavedPath,
        };
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        use std::{
            fs::{self, File},
            time::{SystemTime, UNIX_EPOCH},
        };
        let dir = std::env::temp_dir().join(format!(
            "probe-saved-spare-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("saved.xlsx");
        let views = Arc::new(RegisteredViews::default());
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({"savedLoadAnswers":["applied"]}));
        let registrar_views = views.clone();
        let registrar: ProbeViewRegistrar = Arc::new(move |sender, generation| {
            registrar_views
                .0
                .lock()
                .unwrap()
                .insert(sender.view, (sender, generation));
            Ok(())
        });
        hosts.insert(registrar);
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: Arc::new(TestThread),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views: views.clone(),
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let provided = Rc::new(Provided::default());
        provide_hooks(&provided, &services);
        let fake = services.hosts.get::<Arc<WorldBox>>().unwrap().0.clone();
        let shell = fake.w().shell;
        let shell_generation = RendererGeneration::new();
        views.0.lock().unwrap().insert(
            shell.view,
            (
                shell_contract::ipc::Sender {
                    view: shell.view,
                    window: shell.window,
                    surface: Surface::Shell,
                },
                shell_generation.clone(),
            ),
        );
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
        let shell_owner = effects
            .capture_owner(shell.view, &shell_generation)
            .unwrap();
        let mut document_owner = shell_owner.clone();
        document_owner.context = document_owner
            .context
            .with_document_lease(RendererGeneration::new());
        let binding = NativeInvocationIssuer::new(CancelToken::default())
            .issue_existing_document_save(
                Surface::Sheets,
                shell.view,
                NativeSaveSelection::ExactPath(path.clone()),
                document_owner,
                CancelToken::default(),
            )
            .unwrap();
        let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let parent = File::open(&dir).unwrap();
        let write = effects
            .begin_native_direct_write(
                &selection,
                &[],
                &cancel,
                &pending,
                &[],
                file_commit::prepare_direct(&parent, &path).unwrap(),
            )
            .unwrap();
        let written = match write.write_owned(b"synthetic saved workbook".to_vec()) {
            Ok(written) => written,
            Err(_) => panic!("fixture write failed"),
        };
        let (output, snapshot) = match written.finish(effects.as_ref()) {
            shell_contract::ipc::DirectWriteCompletion::Committed {
                publication,
                snapshot,
            } => (publication, snapshot),
            _ => panic!("fixture publication failed"),
        };
        let input = NativeSavedInput::from_written_snapshot(output.clone(), snapshot).unwrap();
        let hooks = provided.all::<dyn TabHooks>().into_iter().next().unwrap();
        let captured = hooks.capture_saved_shell(shell, shell_owner).unwrap();
        let make_spare = || {
            let view = fake.w().make_view();
            let generation = RendererGeneration::new();
            let closure = fixture_reserve(&fake, view, generation.clone());
            closure
                .reservation
                .with_initial_owner(&closure.owner, |_| Ok(()))
                .unwrap();
            let owner = fixture_register_owner(&fake, view, &generation, Surface::Sheets).unwrap();
            let token = Arc::new(());
            {
                let mut world = fake.w();
                world.generated_epochs.insert(view, generation.clone());
                world.generated_epoch_tokens.insert(view, token.clone());
            }
            let actor = Rc::new(ProbeSpareActor {
                fake: fake.clone(),
                view,
                reservation: closure.reservation.clone(),
                epoch: generation,
                token,
                owner: Rc::new(std::cell::RefCell::new(Some(closure.owner.clone()))),
                state: Rc::new(std::cell::RefCell::new(ProbeSpareState {
                    owner: closure.owner.clone(),
                    closing: false,
                })),
            });
            let shared: Rc<dyn super::super::host::SpareActor> = actor.clone();
            let mut ready = fixture_ready(&fake, view, ());
            ready.reservation.registration =
                super::super::host::NativeBrowserRegistration::Registered(owner);
            let spare = super::super::host::RetainedSpareBinding::new(
                ready.reservation,
                Rc::default(),
                Some(shared),
            );
            (spare, view, closure, actor)
        };
        let unused_cleanup =
            || -> ViewCreationCleanup { Arc::new(|done| done(Err("unused cleanup".into()))) };

        let (spare, view, closure, actor) = make_spare();
        let original_browser = closure.owner.clone();
        let rollback: super::super::host::EditorSeedRollbackStore =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let prepared = captured
            .prepare_spare(
                &spare,
                input.clone(),
                NativeSavedPath::Selected(output.clone()),
                vec![],
                cancel.clone(),
                pending.clone(),
                vec![],
                rollback.clone(),
            )
            .unwrap();
        let target = prepared.target.clone();
        assert_eq!(target.view(), view);
        assert!(prepared.spare_actor.is_some());
        assert!(!target.browser().same_owner(&original_browser));
        assert!(prepared.binding.owner.same_owner(target.browser()));
        assert!(!fake.w().saved_barriers.contains_key(&view));
        assert!(rollback.borrow().is_empty());
        let adopted = Rc::new(std::cell::Cell::new(false));
        let registered = Rc::new(std::cell::Cell::new(false));
        let adopted_callback = adopted.clone();
        let registered_callback = registered.clone();
        let fake_at_adoption = fake.clone();
        let fake_at_registration = fake.clone();
        let transition_original = original_browser.clone();
        let result = Rc::new(std::cell::RefCell::new(None));
        let result_slot = result.clone();
        (prepared.start)(
            Box::new(move |target, transition, actor| {
                assert!(transition.matches_owner(&transition_original));
                assert!(transition.proposes_owner(target.browser()));
                assert!(!fake_at_adoption.w().saved_barriers.contains_key(&view));
                actor(&mut || {
                    adopted_callback.set(true);
                    Ok(())
                })
            }),
            Box::new(move |target, owner, transition, register| {
                assert_eq!(owner.view, view);
                assert!(transition.matches_owner(target.browser()));
                assert!(fake_at_registration.w().saved_barriers.contains_key(&view));
                registered_callback.set(true);
                register()
            }),
            unused_cleanup(),
            Box::new(move |value| *result_slot.borrow_mut() = Some(value)),
        );
        assert!(adopted.get() && registered.get());
        let ready = match result.borrow_mut().take() {
            Some(Ok(ready)) => ready,
            _ => panic!("fixture saved spare load did not complete"),
        };
        assert!(matches!(
            ready.view.outcome,
            shell_contract::editors::SavedDocumentLoadOutcome::Applied
        ));
        assert!(ready.view.document.target().same_target(&target));
        assert!(ready.reservation.owner.same_owner(target.browser()));
        assert!(matches!(
            ready.reservation.registration,
            super::super::host::NativeBrowserRegistration::Registered(_)
        ));
        assert!(actor.state.borrow().owner.same_owner(target.browser()));
        assert!(fake.w().saved_barriers.contains_key(&view));
        assert_eq!(rollback.borrow().len(), 2);

        let (spare, refused_view, refused_closure, refused_actor) = make_spare();
        let cancelled = CancelToken::default();
        let refused_rollback: super::super::host::EditorSeedRollbackStore =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let refused = captured
            .prepare_spare(
                &spare,
                input,
                NativeSavedPath::Selected(output),
                vec![],
                cancelled.clone(),
                pending,
                vec![],
                refused_rollback.clone(),
            )
            .unwrap();
        cancelled.cancel();
        let denied = Rc::new(std::cell::RefCell::new(None));
        let denied_slot = denied.clone();
        (refused.start)(
            Box::new(|_, _, _| panic!("cancelled spare adoption must not run")),
            Box::new(|_, _, _, _| panic!("cancelled spare registration must not run")),
            unused_cleanup(),
            Box::new(move |value| *denied_slot.borrow_mut() = Some(value)),
        );
        match denied.borrow_mut().take() {
            Some(Err(failure)) => assert!(matches!(
                failure.cleanup,
                ViewCreationCleanupState::Complete
            )),
            _ => panic!("cancelled spare must fail without cleanup"),
        }
        assert!(!fake.w().saved_barriers.contains_key(&refused_view));
        assert!(refused_rollback.borrow().is_empty());
        assert!(
            refused_actor
                .state
                .borrow()
                .owner
                .same_owner(&refused_closure.owner)
        );
        assert!(
            refused_closure
                .reservation
                .with_owner(&refused_closure.owner, |_| Ok(()))
                .is_ok()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Default)]
    struct LateViews(Mutex<Option<ProbeShellEvent>>);
    impl shell_contract::runtime::Views for LateViews {
        fn sender_of(&self, view: ViewId) -> Option<shell_contract::ipc::Sender> {
            self.0
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|(sender, _)| (sender.view == view).then_some(*sender))
        }
        fn all(&self) -> Vec<shell_contract::ipc::Sender> {
            self.0
                .lock()
                .unwrap()
                .as_ref()
                .map_or_else(Vec::new, |(sender, _)| vec![*sender])
        }
        fn on_destroyed(&self, _: Box<dyn Fn(ViewId) + Send + Sync>) {}
        fn renderer_generation(
            &self,
            view: ViewId,
        ) -> Option<shell_contract::runtime::RendererGeneration> {
            self.0
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|(sender, generation)| (sender.view == view).then(|| generation.clone()))
        }
        fn on_renderer_replaced(
            &self,
            _: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
        ) {
        }
    }

    #[test]
    fn late_registered_shell_installs_only_its_original_startup() {
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({}));
        let registration = Arc::new(ProbeShellRegistration::default());
        hosts.insert(registration.clone());
        let views = Arc::new(LateViews::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: Arc::new(TestThread),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views: views.clone(),
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        assert!(state.spare_creator_authority.borrow().is_none());
        let sender = shell_contract::ipc::Sender {
            view: ViewId(1),
            window: WindowId(1),
            surface: Surface::Shell,
        };
        let generation = shell_contract::runtime::RendererGeneration::new();
        *views.0.lock().unwrap() = Some((sender, generation.clone()));
        registration.registered(sender, generation.clone());
        let root = state.spare_creator_authority.borrow().clone().unwrap();
        assert!(
            root.startup_owner()
                .unwrap()
                .context
                .renderer()
                .unwrap()
                .same_generation(&generation)
        );
        let world = services.hosts.get::<Arc<WorldBox>>().unwrap();
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
        assert!(
            install_registered_shell(
                &state,
                views.as_ref(),
                effects,
                world,
                sender,
                generation.clone()
            )
            .is_ok()
        );
        assert!(
            install_registered_shell(
                &state,
                views.as_ref(),
                effects,
                world,
                sender,
                shell_contract::runtime::RendererGeneration::new()
            )
            .is_err()
        );
        assert!(
            root.startup_owner()
                .unwrap()
                .context
                .renderer()
                .unwrap()
                .same_generation(&generation)
        );
    }

    #[test]
    fn real_tabs_cleanup_waits_for_async_retry_and_reports_each_drain() {
        let mut hosts = Hosts::default();
        probe_hosts(&mut hosts, &json!({}));
        let services = Services {
            ui: Arc::new(TestThread),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views: Arc::new(TestViews),
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        };
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        type CloseDone =
            Box<dyn FnOnce(Result<shell_contract::ipc::NativeBrowserRetirement, String>) + Send>;
        type RollbackDone = Box<dyn FnOnce(Result<(), String>)>;
        let close = Arc::new(Mutex::new(None::<CloseDone>));
        let close_slot = close.clone();
        let cleanup: ViewCreationCleanup =
            Arc::new(move |done| *close_slot.lock().unwrap() = Some(done));
        let generation = shell_contract::runtime::RendererGeneration::new();
        let (issuer, reservation) =
            shell_contract::ipc::NativeClosureIssuer::new(ViewId(88), generation.clone());
        let rollback = Rc::new(std::cell::RefCell::new(None::<RollbackDone>));
        let rollback_slot = rollback.clone();
        let rollback_fn = Rc::new(
            move |proof: Option<shell_contract::ipc::NativeBrowserRetirement>,
                  done: RollbackDone| {
                assert!(proof.as_ref().is_some_and(|proof| closed_matches(
                    proof,
                    &reservation,
                    ViewId(88),
                    &generation
                )));
                *rollback_slot.borrow_mut() = Some(done);
            },
        );
        let replies = Rc::new(std::cell::Cell::new(0));
        let replied = replies.clone();
        let failure = Box::new(move |_: &str| {
            if replied.get() == 0 {
                replied.set(1);
            }
        });
        let completed = replies.clone();
        state.retain_creation_cleanup_with_rollback(
            Some(cleanup),
            None,
            Some(rollback_fn),
            failure,
            Box::new(move || {
                if completed.get() == 0 {
                    completed.set(1);
                }
            }),
        );
        let first = Rc::new(std::cell::RefCell::new(None));
        let sink = first.clone();
        state.cancel_creations_and_drain(Box::new(move |result| *sink.borrow_mut() = Some(result)));
        assert!(first.borrow().is_none());
        close.lock().unwrap().take().unwrap()(Err("close failed".into()));
        assert!(matches!(&*first.borrow(), Some(Err(_))));
        assert_eq!(replies.get(), 1);
        let second = Rc::new(std::cell::RefCell::new(None));
        let sink = second.clone();
        state.cancel_creations_and_drain(Box::new(move |result| *sink.borrow_mut() = Some(result)));
        close.lock().unwrap().take().unwrap()(Ok(
            shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed()),
        ));
        assert!(second.borrow().is_none());
        rollback.borrow_mut().take().unwrap()(Err("rollback failed".into()));
        assert!(matches!(&*second.borrow(), Some(Err(_))));
        let third = Rc::new(std::cell::RefCell::new(None));
        let sink = third.clone();
        state.cancel_creations_and_drain(Box::new(move |result| *sink.borrow_mut() = Some(result)));
        assert!(third.borrow().is_none());
        rollback.borrow_mut().take().unwrap()(Ok(()));
        assert!(matches!(&*third.borrow(), Some(Ok(()))));
        assert_eq!(replies.get(), 1);
    }

    #[test]
    fn generated_preclose_joins_callers_and_retries_after_failure() {
        type BarrierDone = Box<dyn FnOnce(Result<(), String>) + Send>;
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let view = ViewId(97);
        let generation = shell_contract::runtime::RendererGeneration::new();
        let created = fixture_reserve(&fake, view, generation.clone());
        let gate = Arc::new(FixturePreclose::default());
        let cleanup = fixture_preclose_cleanup(&fake, view, gate.clone());
        let pending = Arc::new(Mutex::new(None::<BarrierDone>));
        let slot = pending.clone();
        let barrier: super::super::host::GeneratedPrecloseBarrier =
            Arc::new(move |done| *slot.lock().unwrap() = Some(done));
        let failures = Arc::new(Mutex::new(Vec::new()));
        let observed = failures.clone();
        let retry = gate
            .install(
                fixture_cleanup(&fake, view),
                barrier,
                Arc::new(move |error| observed.lock().unwrap().push(error)),
            )
            .unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let results = results.clone();
            cleanup(Box::new(move |result| results.lock().unwrap().push(result)));
        }
        assert!(results.lock().unwrap().is_empty());
        let barrier_done = pending.lock().unwrap().take().unwrap();
        barrier_done(Err("drain failed".into()));
        assert_eq!(results.lock().unwrap().len(), 2);
        assert!(results.lock().unwrap().iter().all(Result::is_err));
        assert_eq!(failures.lock().unwrap().as_slice(), &["drain failed"]);
        let observed = Arc::new(Mutex::new(None));
        let sink = observed.clone();
        created.subscribe(Box::new(move |receipt| {
            *sink.lock().unwrap() = Some(receipt)
        }));
        retry();
        assert!(observed.lock().unwrap().is_none());
        let barrier_done = pending.lock().unwrap().take().unwrap();
        barrier_done(Ok(()));
        assert!(observed.lock().unwrap().as_ref().unwrap().matches(
            &created.reservation,
            view,
            &generation
        ));
    }

    #[test]
    fn fixture_close_replays_original_receipt_after_terminal() {
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let view = ViewId(99);
        let generation = shell_contract::runtime::RendererGeneration::new();
        let state = fixture_reserve(&fake, view, generation.clone());
        let ready = fixture_ready(&fake, view, view);
        let received = Arc::new(Mutex::new(None));
        let sink = received.clone();
        (ready.cleanup)(Box::new(move |result| {
            *sink.lock().unwrap() = Some(result.unwrap())
        }));
        let replay = Arc::new(Mutex::new(None));
        let sink = replay.clone();
        (ready.closure)(Box::new(move |receipt| {
            *sink.lock().unwrap() = Some(receipt)
        }))
        .unwrap();
        let original = received.lock().unwrap();
        let later = replay.lock().unwrap();
        assert!(closed_matches(
            original.as_ref().unwrap(),
            &state.reservation,
            view,
            &generation
        ));
        assert!(closed_matches(
            later.as_ref().unwrap(),
            &state.reservation,
            view,
            &generation
        ));
        let (_, forged) = shell_contract::ipc::NativeClosureIssuer::new(view, generation.clone());
        assert!(!closed_matches(
            later.as_ref().unwrap(),
            &forged,
            view,
            &generation
        ));
    }

    #[test]
    fn unstarted_saved_reservation_closes_only_its_own_fixture() {
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let view = fake.w().make_view();
        let generation = shell_contract::runtime::RendererGeneration::new();
        let closure = fixture_reserve(&fake, view, generation.clone());
        let observed = Arc::new(Mutex::new(None));
        let sink = observed.clone();
        closure.subscribe(Box::new(move |receipt| {
            *sink.lock().unwrap() = Some(receipt)
        }));
        drop(ProbeUnstartedSaved {
            fake: fake.clone(),
            view,
            closure: closure.clone(),
            armed: true,
        });
        assert!(!generation.is_active());
        assert!(observed.lock().unwrap().as_ref().unwrap().matches(
            &closure.reservation,
            view,
            &generation
        ));
        assert!(!fake.w().closures.contains_key(&view));

        let reused = fake.w().make_view();
        let stale = fixture_reserve(
            &fake,
            reused,
            shell_contract::runtime::RendererGeneration::new(),
        );
        let successor = fixture_reserve(
            &fake,
            reused,
            shell_contract::runtime::RendererGeneration::new(),
        );
        drop(ProbeUnstartedSaved {
            fake: fake.clone(),
            view: reused,
            closure: stale,
            armed: true,
        });
        assert!(
            fake.w()
                .closures
                .get(&reused)
                .is_some_and(|current| Arc::ptr_eq(current, &successor))
        );
        assert!(successor.created.is_active());
    }

    #[test]
    fn generated_seed_commits_only_after_manager_and_rolls_back_exact_token() {
        use super::super::host::SpareSeedPlan;
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let view = fake.w().make_view();
        let generation = shell_contract::runtime::RendererGeneration::new();
        let closure = fixture_reserve(&fake, view, generation.clone());
        let rollback: super::super::host::EditorSeedRollbackStore =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut plan = prepare_probe_seed(
            &fake,
            view,
            &generation,
            &closure.reservation,
            Some(main_core::shell::tab_manager::EditorSeed::Generate(
                json!({"fixture": true}),
            )),
            None,
            None,
            None,
            &rollback,
        )
        .unwrap();
        assert!(
            closure
                .reservation
                .with_initial_owner(&closure.owner, |transition| {
                    plan.with_validated(transition, &closure.owner, &mut || {
                        Err("manager rejected".into())
                    })
                })
                .is_err()
        );
        assert!(!fake.w().generated.contains_key(&view));
        closure
            .reservation
            .with_initial_owner(&closure.owner, |transition| {
                plan.with_validated(transition, &closure.owner, &mut || {
                    assert!(!fake.w().generated.contains_key(&view));
                    Ok(())
                })
            })
            .unwrap();
        plan.after_commit(&shell_contract::runtime::CancelToken::default())
            .unwrap();
        assert!(fake.w().generated.contains_key(&view));
        let done = Rc::new(std::cell::RefCell::new(None));
        let observed = done.clone();
        rollback.borrow()[0].rollback(Box::new(move |result| {
            *observed.borrow_mut() = Some(result)
        }));
        assert!(matches!(&*done.borrow(), Some(Ok(()))));
        assert!(!fake.w().generated.contains_key(&view));
    }

    #[test]
    fn superseded_cleanup_retains_failed_receipt_for_retry() {
        struct Receipt {
            reservation: shell_contract::ipc::NativeClosureReservation,
            owner: shell_contract::ipc::NativeBrowserOwner,
            calls: Rc<std::cell::Cell<usize>>,
        }
        impl super::super::host::EditorSeedRollback for Receipt {
            fn retire_closed(
                &self,
                _: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("wrong path".into()));
            }
            fn retire_adopted(
                &self,
                _: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("wrong path".into()));
            }
            fn retire_superseded(
                &self,
                proof: shell_contract::ipc::NativeBrowserSuperseded,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                if !proof.matches(&self.reservation, &self.owner) {
                    return done(Err("wrong proof".into()));
                }
                self.calls.set(self.calls.get() + 1);
                done(if self.calls.get() == 1 {
                    Err("retry".into())
                } else {
                    Ok(())
                });
            }
            fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
                done(Err("wrong path".into()));
            }
        }
        let generation = shell_contract::runtime::RendererGeneration::new();
        let (_issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(77), generation);
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let (_, proof, ()) = reservation
            .prepare_replacement(&owner)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let receipt = Rc::new(Receipt {
            reservation,
            owner,
            calls: calls.clone(),
        });
        let drain = ProbeSupersededDrain {
            state: Rc::new(std::cell::RefCell::new(ProbeSupersededState {
                receipts: vec![receipt],
                proof: Some(proof),
                running: false,
            })),
        };
        let first = Rc::new(std::cell::RefCell::new(None));
        let first_slot = first.clone();
        drain.begin(Box::new(move |result| {
            *first_slot.borrow_mut() = Some(result)
        }));
        assert!(matches!(&*first.borrow(), Some(Err(_))));
        assert_eq!(drain.state.borrow().receipts.len(), 1);
        let second = Rc::new(std::cell::RefCell::new(None));
        let second_slot = second.clone();
        drain.begin(Box::new(move |result| {
            *second_slot.borrow_mut() = Some(result)
        }));
        assert!(matches!(&*second.borrow(), Some(Ok(()))));
        assert!(drain.state.borrow().receipts.is_empty());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn fixture_old_cleanup_cannot_close_a_replaced_view() {
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let view = ViewId(98);
        fixture_reserve(
            &fake,
            view,
            shell_contract::runtime::RendererGeneration::new(),
        );
        let old_close = fixture_cleanup(&fake, view);
        let successor = fixture_reserve(
            &fake,
            view,
            shell_contract::runtime::RendererGeneration::new(),
        );
        let result = Arc::new(Mutex::new(None));
        let sink = result.clone();
        old_close(Box::new(move |value| *sink.lock().unwrap() = Some(value)));
        assert!(matches!(&*result.lock().unwrap(), Some(Err(_))));
        assert!(!fake.view_destroyed(view));
        assert!(successor.state.lock().unwrap().receipt.is_none());
    }

    #[test]
    fn fixture_window_observation_rejects_changed_revision() {
        let proof = FixtureWindowProof::new(WindowId(7), "before".into());
        let reservation =
            super::super::window_snapshot::WindowReservation::new(WindowId(7), proof.clone());
        let observed = Arc::new(Mutex::new(None));
        let target = observed.clone();
        reservation.observe(Box::new(move |result| {
            *target.lock().unwrap() = Some(result.unwrap())
        }));
        let observed = observed.lock().unwrap().take().unwrap();
        assert!(
            super::super::window_snapshot::with_window_observations(
                std::slice::from_ref(&observed),
                || ()
            )
            .is_ok()
        );
        reservation.set_title("after").unwrap();
        assert!(
            super::super::window_snapshot::with_window_observations(&[observed], || ()).is_err()
        );
    }

    #[test]
    fn saved_detached_slides_title_uses_the_slides_policy() {
        use main_core::shell::detached_windows::DetachedWindowOptions;
        use shell_contract::runtime::CancelToken;
        let sender = shell_contract::ipc::Sender {
            view: ViewId(42),
            window: WindowId(2),
            surface: Surface::Slides,
        };
        let generation = shell_contract::runtime::RendererGeneration::new();
        let mut hosts = Hosts::default();
        let ui = Arc::new(QueuedUi::default());
        probe_hosts(&mut hosts, &json!({}));
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: ui.clone(),
            core: Arc::new(TestThread),
            workers: Arc::new(TestWorkers),
            events: Arc::new(TestEvents),
            views: Arc::new(BoundViews(sender, generation.clone())),
            paths: Arc::new(TestPaths),
            hosts: Arc::new(hosts),
        });
        let state = probe_state(&services, Rc::new(Provided::default())).unwrap();
        {
            let mut inner = state.inner.borrow_mut();
            let super::super::state::Inner {
                detached, bridge, ..
            } = &mut *inner;
            detached.create(
                bridge,
                DetachedWindowOptions {
                    view: sender.view,
                    binding: super::super::host::RetainedSpareBinding::new(
                        fixture_ready(
                            &Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default())))),
                            sender.view,
                            (),
                        )
                        .reservation,
                        Rc::default(),
                        None,
                    ),
                    kind: TabKind::Slides,
                    title: "Before".into(),
                    file_path: Some("/saved.pptx".into()),
                    tear_off_at: None,
                },
            );
        }
        let windows = services.hosts.get::<WindowsHostRef>().unwrap().clone();
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
        let owner = effects.capture_owner(sender.view, &generation).unwrap();
        let result = Rc::new(std::cell::RefCell::new(None));
        let sink = result.clone();
        state.set_document_title_owned(
            "Regular".into(),
            owner.clone(),
            owner.clone(),
            CancelToken::default(),
            CancelToken::default(),
            effects.clone(),
            Box::new(move |value| *sink.borrow_mut() = Some(value)),
        );
        assert!(matches!(&*result.borrow(), Some(Ok(()))));
        assert_eq!(windows.window_state(WindowId(2)).title, "Before");
        let result = Rc::new(std::cell::RefCell::new(None));
        let sink = result.clone();
        state.set_slides_document_title_owned(
            "Slides".into(),
            owner.clone(),
            owner.clone(),
            CancelToken::default(),
            CancelToken::default(),
            effects.clone(),
            Box::new(move |value| *sink.borrow_mut() = Some(value)),
        );
        assert!(result.borrow().is_none());
        ui.run();
        assert!(matches!(&*result.borrow(), Some(Ok(()))));
        assert_eq!(windows.window_state(WindowId(2)).title, "Slides");
        let result = Rc::new(std::cell::RefCell::new(None));
        let sink = result.clone();
        state.set_slides_document_title_owned(
            "Stale".into(),
            owner.clone(),
            owner,
            CancelToken::default(),
            CancelToken::default(),
            effects,
            Box::new(move |value| *sink.borrow_mut() = Some(value)),
        );
        let world = services.hosts.get::<Arc<WorldBox>>().unwrap();
        {
            let mut world = world.0.w();
            let window = world.windows.get_mut(&2).unwrap();
            window.proof.update(|state| state.destroyed = true);
            window.proof = FixtureWindowProof::new(WindowId(2), "Successor".into());
        }
        ui.run();
        assert!(matches!(&*result.borrow(), Some(Err(_))));
        assert_eq!(windows.window_state(WindowId(2)).title, "Successor");
    }

    #[test]
    fn fixture_command_requires_the_selected_surface_and_path() {
        let spec = ViewSpec::Docs {
            path: Some("/selected.docx"),
        };
        let selected = NativeEditorCommand::OpenFile {
            surface: Surface::Docs,
            path: "/selected.docx".into(),
        };
        let wrong_path = NativeEditorCommand::OpenFile {
            surface: Surface::Docs,
            path: "/other.docx".into(),
        };
        let wrong_surface = NativeEditorCommand::OpenFile {
            surface: Surface::Slides,
            path: "/selected.docx".into(),
        };
        assert!(fixture_command_matches(&spec, &selected));
        assert!(!fixture_command_matches(&spec, &wrong_path));
        assert!(!fixture_command_matches(&spec, &wrong_surface));
    }

    #[test]
    fn sheets_creation_does_not_guess_the_workbook_path() {
        let spec = ViewSpec::Sheets {
            opening_workbook: Some(true),
        };
        assert!(fixture_command_matches(
            &spec,
            &NativeEditorCommand::OpenFile {
                surface: Surface::Sheets,
                path: "/selected.xlsx".into(),
            }
        ));
        assert!(!fixture_command_matches(
            &spec,
            &NativeEditorCommand::NewDocument {
                surface: Surface::Sheets,
                directory: None,
            }
        ));
    }

    #[test]
    fn canceled_native_command_does_not_create_a_probe_view() {
        use shell_contract::{
            ipc::NativeInvocationIssuer,
            runtime::{CancelToken, RendererGeneration},
        };
        let fake = Fake(Arc::new(Mutex::new(World::new(&json!({}), Arc::default()))));
        let shutdown = CancelToken::default();
        let cancel = CancelToken::default();
        let command = NativeEditorCommand::NewDocument {
            surface: Surface::Docs,
            directory: None,
        };
        let operation =
            NativeInvocationIssuer::new(shutdown).issue_command(command.clone(), cancel.clone());
        cancel.cancel();
        let called = std::cell::Cell::new(false);
        let result = fixture_native_command(
            &fake,
            &operation,
            &command,
            &RendererGeneration::new(),
            &[],
            &cancel,
            &CancelToken::default(),
            || {
                called.set(true);
                Ok(ViewId(99))
            },
            Box::new(|_, _, _, _| Ok(())),
        );
        assert!(result.is_err());
        assert!(!called.get());
    }
}
