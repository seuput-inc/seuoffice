//! The Rust side of tools/ipc-diff (the stage 3 shell contract, section 8).
//! Reads one request per line on stdin, `{"dir": folder, "case": case}`, and
//! answers each with one line on stdout. Every case gets a fresh world: a
//! real core thread, a fake UI thread, workers it can wait for, the case's
//! views, `app.getPath` under the case's folder, and hosts faked from the
//! case (the shared ones here, each batch's through its `probe_hosts`). All
//! six batches register as they stand; then the case's calls run in order
//! through the shell's own dispatch. The shapes are in
//! tools/ipc-diff/README.md.

mod fakes;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shell_contract::editors::Then;
use shell_contract::ids::{RequestId, ViewId, WindowId};
use shell_contract::ipc::{ChannelKind, EventSink, RegisterError, Registry, Reply, Sender, Target};
use shell_contract::provided::Provided;
use shell_contract::runtime::{AppPaths, PathName, Services, Thread, Views, Workers, run_on};
use shell_contract::value::IpcValue;
use shell_contract::{Hosts, PRELOADS, Surface};
use shell_core::core_thread::{CoreThread, ThreadWorkers};
use shell_core::dispatch::{CatchingWorkers, Dispatcher};
use shell_core::outlet::resolve;
use shell_core::table::{StartError, Table};
use shell_core::{BATCHES, wire};

/// The call log a batch's fakes write to: take it from the hosts with
/// `hosts.get::<ProbeLog>()`; one JSON line per call, named after the
/// Electron call it fakes (tools/ipc-diff/README.md)
pub type ProbeLog = Arc<Mutex<Vec<Value>>>;

/// The batch of each entry of [`BATCHES`], as a group names it
const BATCH_NAMES: [&str; 6] = ["U", "A", "M", "W", "D1", "D2"];
const _: () = assert!(BATCHES.len() == BATCH_NAMES.len());

/// How long one call may take before the case fails
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok())
        .collect()
}

static SURFACES: LazyLock<HashMap<String, Surface>> = LazyLock::new(|| {
    PRELOADS
        .iter()
        .map(|p| (format!("{:?}", p.surface), p.surface))
        .collect()
});

type Replaced = Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>;

/// The views of the case, in the order `Views::all` lists them
struct CaseViews {
    views: Mutex<Vec<Sender>>,
    generations: Mutex<HashMap<ViewId, shell_contract::runtime::RendererGeneration>>,
    replaced: Mutex<Vec<Replaced>>,
}

impl Views for CaseViews {
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

    fn on_destroyed(&self, _listener: Box<dyn Fn(ViewId) + Send + Sync>) {}

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
        self.replaced.lock().unwrap().push(listener);
    }
}

/// `app.getPath(name)` is `<folder>/<name>`, as fakes.ts answers it
struct CasePaths(PathBuf);

impl AppPaths for CasePaths {
    fn path(&self, name: PathName) -> PathBuf {
        let name = match name {
            PathName::Home => "home",
            PathName::AppData => "appData",
            PathName::UserData => "userData",
            PathName::Temp => "temp",
            PathName::Exe => "exe",
            PathName::Desktop => "desktop",
            PathName::Documents => "documents",
            PathName::Downloads => "downloads",
            PathName::Logs => "logs",
        };
        self.0.join(name)
    }
}

/// Every event, one entry per view it reaches
struct Recorder {
    views: Arc<CaseViews>,
    events: Events,
}

/// The events of a case so far. While `muted` names a thread, what that
/// thread sends is not kept ([`register_keeping_own`]); other threads' sends
/// (a worker a batch started) are.
#[derive(Default)]
struct Events {
    list: Mutex<Vec<Value>>,
    muted: Mutex<Option<std::thread::ThreadId>>,
}

impl Events {
    fn push(&self, event: Value) {
        if *self.muted.lock().unwrap() != Some(std::thread::current().id()) {
            self.list.lock().unwrap().push(event);
        }
    }

    fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.list.lock().unwrap())
    }
}

/// A case-local reference to the actual argument one side emitted. The
/// comparison normalizes only the referenced identity, never the remaining
/// event fields or the argument used to answer the real handler.
#[derive(Clone)]
struct EventArgument {
    step: usize,
    channel: String,
    view: u64,
    arg: usize,
    occurrence: usize,
    /// a token only this side's shell adds as the event's last argument; it is
    /// left out of the compared event (tools/ipc-diff/event-refs.ts, `nativeEventArg`)
    native: bool,
}
impl EventArgument {
    fn parse(value: &Value) -> Result<Option<Self>, String> {
        let Some(object) = value.as_object() else {
            return Ok(None);
        };
        if object.len() != 1 || !object.contains_key("$eventArg") {
            return Ok(None);
        }
        let reference = object["$eventArg"]
            .as_object()
            .ok_or("invalid event argument reference")?;
        let number = |name: &str| -> Result<u64, String> {
            reference
                .get(name)
                .and_then(Value::as_u64)
                .filter(|n| *n <= 9_007_199_254_740_991)
                .ok_or_else(|| "invalid event argument reference".to_owned())
        };
        Ok(Some(Self {
            step: usize::try_from(number("step")?).map_err(|e| e.to_string())?,
            channel: reference
                .get("channel")
                .and_then(Value::as_str)
                .ok_or("invalid event argument reference")?
                .to_owned(),
            view: number("view")?,
            arg: usize::try_from(number("arg")?).map_err(|e| e.to_string())?,
            occurrence: usize::try_from(number("occurrence")?).map_err(|e| e.to_string())?,
            native: reference.get("native").and_then(Value::as_bool) == Some(true),
        }))
    }
    fn key(&self) -> String {
        json!([
            self.step,
            self.channel.as_str(),
            self.view,
            self.arg,
            self.occurrence
        ])
        .to_string()
    }
}

#[derive(Default)]
struct EventReferences {
    wanted: HashMap<String, EventArgument>,
    values: HashMap<String, Value>,
}
impl EventReferences {
    fn collect(&mut self, value: &Value) -> Result<(), String> {
        if let Some(reference) = EventArgument::parse(value)? {
            self.wanted.insert(reference.key(), reference);
        } else {
            match value {
                Value::Array(values) => {
                    for value in values {
                        self.collect(value)?;
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        self.collect(value)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn resolve(&self, value: &Value) -> Result<Value, String> {
        if let Some(reference) = EventArgument::parse(value)? {
            let key = reference.key();
            return self
                .values
                .get(&key)
                .cloned()
                .ok_or_else(|| format!("event argument not captured: {key}"));
        }
        match value {
            Value::Array(values) => values
                .iter()
                .map(|value| self.resolve(value))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Value::Object(values) => values
                .iter()
                .map(|(name, value)| Ok((name.clone(), self.resolve(value)?)))
                .collect::<Result<serde_json::Map<_, _>, String>>()
                .map(Value::Object),
            _ => Ok(value.clone()),
        }
    }
    fn capture(&mut self, step: usize, mut events: Vec<Value>) -> Result<Vec<Value>, String> {
        for (key, reference) in &self.wanted {
            if reference.step != step {
                continue;
            }
            let index = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event["channel"].as_str() == Some(reference.channel.as_str())
                        && event["view"].as_u64() == Some(reference.view)
                })
                .nth(reference.occurrence)
                .map(|(index, _)| index)
                .ok_or_else(|| format!("event argument not found: {key}"))?;
            let args = events[index]["args"]
                .as_array_mut()
                .ok_or_else(|| format!("event argument not found: {key}"))?;
            let last = args.len().checked_sub(1);
            let value = args
                .get_mut(reference.arg)
                .ok_or_else(|| format!("event argument not found: {key}"))?;
            let kind = if value.is_number() {
                "number"
            } else if value.is_string() {
                "string"
            } else {
                return Err(format!("event identity is not a number or string: {key}"));
            };
            self.values.insert(key.clone(), value.clone());
            if reference.native {
                if Some(reference.arg) != last {
                    return Err(format!(
                        "native event argument is not the last argument: {key}"
                    ));
                }
                args.pop();
            } else {
                *value = Value::String(format!("$EVENT:{key}:{kind}"));
            }
        }
        Ok(events)
    }
}

impl EventSink for Recorder {
    fn emit(&self, target: Target, channel: &'static str, args: Vec<IpcValue>) {
        let mut e = wire::Encoder::default();
        let header = e.values(&args);
        let buffers: Vec<String> = e.buffers.iter().map(|b| hex(b)).collect();
        for view in resolve(self.views.as_ref(), target) {
            self.events.push(json!({
                "view": view.0,
                "channel": channel,
                "args": header,
                "buffers": buffers,
            }));
        }
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
        if self.views.sender_of(owner.view).is_none()
            || !self
                .views
                .renderer_generation(owner.view)
                .is_some_and(|current| current.is_active() && current.same_generation(generation))
        {
            return Err(format!("renderer does not own view {}", owner.view.0));
        }
        self.emit(Target::View(owner.view), channel, args);
        Ok(())
    }
}

/// Workers the probe can wait for
#[derive(Default)]
struct Tracked {
    active: Mutex<usize>,
    idle: Condvar,
    spawned: AtomicUsize,
}

struct TrackedWorkers(Arc<Tracked>);

impl Workers for TrackedWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        let tracked = self.0.clone();
        *tracked.active.lock().unwrap() += 1;
        tracked.spawned.fetch_add(1, Ordering::SeqCst);
        ThreadWorkers.spawn(Box::new(move || {
            // counted down even when the job panics
            struct Done(Arc<Tracked>);
            impl Drop for Done {
                fn drop(&mut self) {
                    *self.0.active.lock().unwrap() -= 1;
                    self.0.idle.notify_all();
                }
            }
            let _done = Done(tracked);
            job();
        }));
    }
}

impl Tracked {
    fn wait_idle(&self, until: Instant) -> bool {
        let mut active = self.active.lock().unwrap();
        while *active > 0 {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            active = self.idle.wait_timeout(active, left).unwrap().0;
        }
        true
    }
}

thread_local! {
    /// The case's dispatch, on its core thread
    static DISPATCHER: RefCell<Option<Dispatcher>> = const { RefCell::new(None) };
    /// What the batches provide each other, for the `trigger` calls
    static PROVIDED: RefCell<Option<Rc<Provided>>> = const { RefCell::new(None) };
}

fn sender_of(view: &Value) -> Result<Sender, String> {
    let surface = view["surface"].as_str().unwrap_or("Shell");
    let surface = *SURFACES
        .get(surface)
        .ok_or_else(|| format!("no such surface: {surface}"))?;
    let id = view["view"].as_u64().ok_or("a view without an id")? as u32;
    let window = view["window"].as_u64().unwrap_or(id as u64) as u32;
    Ok(Sender {
        view: ViewId(id),
        window: WindowId(window),
        surface,
    })
}

fn kind_of(text: &str) -> Result<ChannelKind, String> {
    match text {
        "invoke" => Ok(ChannelKind::Invoke),
        "send" => Ok(ChannelKind::Send),
        "sendSync" => Ok(ChannelKind::SendSync),
        other => Err(format!("no such call kind: {other}")),
    }
}

fn answer(kind: ChannelKind, channel: &str, reply: Option<Reply>) -> Value {
    match (kind, reply) {
        (_, None) => json!({ "timeout": true }),
        (ChannelKind::Send, Some(Ok(_))) => json!({}),
        (ChannelKind::Send, Some(Err(e))) => json!({ "thrown": e.to_string() }),
        (_, Some(Ok(value))) => {
            let (header, buffers) = wire::encode(&value);
            let buffers: Vec<String> = buffers.iter().map(|b| hex(b)).collect();
            json!({ "value": header, "buffers": buffers })
        }
        (_, Some(Err(e))) => json!({ "error": e.remote_message(channel) }),
    }
}

/// A deferred step until an `await` step takes its reply
enum Pending {
    Call(ChannelKind, String, mpsc::Receiver<Reply>),
    Trigger(mpsc::Receiver<Reply>),
}

/// A trigger's outcome: the value, or `String(err)` of its rejection
fn trigger_answer(reply: Option<Reply>) -> Value {
    match reply {
        None => json!({ "timeout": true }),
        Some(Ok(value)) => {
            let (header, buffers) = wire::encode(&value);
            let buffers: Vec<String> = buffers.iter().map(|b| hex(b)).collect();
            json!({ "value": header, "buffers": buffers })
        }
        Some(Err(e)) => json!({ "error": e.to_string() }),
    }
}

/// Registers `batches` in order on the calling thread and keeps, of what
/// they send while they register, only what the batch `own` sends. The
/// TypeScript side loads only the module of the group's batch and keeps what
/// it sends while loading; what another batch sends while it registers (A's
/// hub load tells every window) belongs to no call. Only the registering
/// thread's own sends are dropped: a worker that sends meanwhile (one the
/// group's batch started) is kept. Without `own`, everything is kept.
fn register_keeping_own<F>(
    table: &mut Table,
    batches: impl IntoIterator<Item = (&'static str, F)>,
    own: Option<&str>,
    events: &Events,
) -> Result<(), RegisterError>
where
    F: FnOnce(&mut Table) -> Result<(), RegisterError>,
{
    for (name, register) in batches {
        let other = own.is_some_and(|own| own != name);
        if other {
            *events.muted.lock().unwrap() = Some(std::thread::current().id());
        }
        let registered = register(table);
        *events.muted.lock().unwrap() = None;
        registered?;
    }
    Ok(())
}

fn run_case(dir: &str, case: &Value) -> Result<Value, String> {
    let views = match case["views"].as_array() {
        Some(views) => views.iter().map(sender_of).collect::<Result<Vec<_>, _>>()?,
        None => vec![sender_of(&json!({ "view": 1, "window": 1 }))?],
    };
    let core = CoreThread::spawn("core");
    let ui = CoreThread::spawn("ui");
    let tracked = Arc::new(Tracked::default());
    let case_views = Arc::new(CaseViews {
        views: Mutex::new(views.clone()),
        generations: Mutex::new(
            views
                .iter()
                .map(|sender| {
                    (
                        sender.view,
                        shell_contract::runtime::RendererGeneration::new(),
                    )
                })
                .collect(),
        ),
        replaced: Mutex::new(Vec::new()),
    });
    // The slides presenter's audience window is made by the shell, not by a call:
    // its probe-only synthetic render owner is not native authentication.
    let case_owners: HashMap<ViewId, shell_contract::ipc::NativeRequestOwner> = views
        .iter()
        .filter(|sender| sender.surface == Surface::Slides)
        .filter_map(|sender| {
            case_views
                .generations
                .lock()
                .unwrap()
                .get(&sender.view)
                .map(|generation| {
                    (
                        sender.view,
                        shell_contract::ipc::NativeRequestOwner {
                            view: sender.view,
                            context: shell_contract::ipc::NativeContext::with_renderer(
                                (),
                                generation.clone(),
                            ),
                        },
                    )
                })
        })
        .collect();
    let recorder = Arc::new(Recorder {
        views: case_views.clone(),
        events: Events::default(),
    });
    let log: ProbeLog = Arc::default();
    let mut hosts = Hosts::default();
    hosts.insert(log.clone());
    let register_views = case_views.clone();
    let shell_registration = Arc::new(shell_ipc_w::tabs::ProbeShellRegistration::default());
    let register_shell = shell_registration.clone();
    let registrar: shell_ipc_w::tabs::ProbeViewRegistrar = Arc::new(move |sender, generation| {
        let mut views = register_views
            .views
            .lock()
            .map_err(|error| error.to_string())?;
        let mut generations = register_views
            .generations
            .lock()
            .map_err(|error| error.to_string())?;
        if views.iter().any(|existing| existing.view == sender.view)
            || generations.contains_key(&sender.view)
        {
            return Err("Synthetic view was already registered".into());
        }
        views.push(sender);
        generations.insert(sender.view, generation.clone());
        drop(generations);
        drop(views);
        register_shell.registered(sender, generation);
        Ok(())
    });
    hosts.insert(shell_registration);
    hosts.insert(registrar);
    let given = &case["hosts"];
    fakes::insert_shared(&mut hosts, &given["shared"], &views, &log);
    shell_ipc_u::probe_hosts(&mut hosts, &given["u"]);
    shell_ipc_a::probe_hosts(&mut hosts, &given["a"]);
    shell_ipc_m::probe_hosts(&mut hosts, &given["m"]);
    shell_ipc_w::probe_hosts(&mut hosts, &given["w"]);
    shell_ipc_d1::probe_hosts(&mut hosts, &given["d1"]);
    if let Some(owner) = case_owners.get(&ViewId(100))
        && let Some(shell) = hosts.get::<Arc<shell_ipc_d1::slides::fake::FakeShell>>()
    {
        *shell
            .audience_owner
            .lock()
            .map_err(|error| error.to_string())? = Some(owner.clone());
    }
    shell_ipc_d2::probe_hosts(&mut hosts, &given["d2"]);
    let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
        ui: ui.clone(),
        core: core.clone(),
        workers: Arc::new(CatchingWorkers(Arc::new(TrackedWorkers(tracked.clone())))),
        events: recorder.clone(),
        views: case_views.clone(),
        paths: Arc::new(CasePaths(PathBuf::from(dir))),
        hosts: Arc::new(hosts),
    });

    let trigger_services = services.clone();
    // Synthetic captures stand in for the native ingress, never authenticate. The
    // native host gives every capture of one renderer the same origin, which the
    // D1 editors compare their saves against (as the batch crates' own tests
    // arrange); the other batches keep each capture its own origin.
    let effects = trigger_services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "the probe has no synthetic owned-effects fixture".to_owned())?;
    let d1 = case["batch"].as_str() == Some("D1");
    if d1 {
        trigger_services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .ok_or_else(|| "the probe has no synthetic capture origins".to_owned())?
            .share_capture_origins();
    }
    // what the shell attaches to a call of a document view: the context its owned
    // effects capture for the view's renderer; none once that renderer is gone
    let capture_native = |sender: Sender| {
        case_views
            .renderer_generation(sender.view)
            .and_then(|generation| effects.capture_owner(sender.view, &generation).ok())
    };
    let own = case["batch"].as_str().map(str::to_owned);
    let registering = recorder.clone();
    let registered = run_on(core.as_ref(), move || {
        let mut table = Table::default();
        let services = &services;
        let batches = BATCH_NAMES
            .iter()
            .zip(BATCHES)
            .map(|(name, register)| (*name, move |table: &mut Table| register(table, services)));
        if let Err(e) =
            register_keeping_own(&mut table, batches, own.as_deref(), &registering.events)
        {
            return Some(format!("{:?}", StartError::Register(e)));
        }
        PROVIDED.with(|p| *p.borrow_mut() = Some(table.provided()));
        DISPATCHER.with(|d| *d.borrow_mut() = Some(Dispatcher::new(table)));
        None
    });
    if let Some(error) = registered {
        core.stop();
        ui.stop();
        return Ok(json!({ "register": error }));
    }
    // the shell registers a Docs view's media session when it creates the view;
    // a case's Docs view is created here, before its first call
    let d2 = case["batch"].as_str() == Some("D2");
    let docs_views: Vec<ViewId> = views
        .iter()
        .filter(|sender| d2 && sender.surface == Surface::Docs)
        .map(|sender| sender.view)
        .collect();
    if !docs_views.is_empty() {
        let attached = run_on(core.as_ref(), move || {
            PROVIDED.with(|p| {
                let provided = p.borrow();
                let provided = provided.as_ref().expect("the case's provided");
                let docs = provided
                    .all::<shell_ipc_d2::docs::editors::DocsEditors>()
                    .into_iter()
                    .next()
                    .ok_or_else(|| "the Docs editors were not provided".to_owned())?;
                docs_views
                    .into_iter()
                    .try_for_each(|view| docs.register_view(view).map(drop))
            })
        });
        if let Err(error) = attached {
            core.stop();
            ui.stop();
            return Ok(json!({ "register": error }));
        }
    }

    let mut calls = Vec::new();
    let mut references = EventReferences::default();
    for call in case["calls"].as_array().into_iter().flatten() {
        references.collect(&call["args"])?;
        references.collect(&call["capture"])?;
    }
    // deferred invokes by the index of their call, until an `await` step takes them
    let mut deferred: HashMap<u64, Pending> = HashMap::new();
    for (i, call) in case["calls"].as_array().into_iter().flatten().enumerate() {
        if let Some(at) = call["await"].as_u64() {
            let answer = match deferred.remove(&at) {
                Some(Pending::Call(kind, channel, rx)) => {
                    answer(kind, &channel, rx.recv_timeout(CALL_TIMEOUT).ok())
                }
                Some(Pending::Trigger(rx)) => trigger_answer(rx.recv_timeout(CALL_TIMEOUT).ok()),
                None => json!({ "thrown": format!("no deferred call {at}") }),
            };
            let drained = drain(&core, &ui, &tracked);
            let mut out = json!({
                "answer": answer,
                "events": references.capture(i, recorder.events.take())?,
            });
            if !drained {
                out["undrained"] = json!(true);
            }
            calls.push(out);
            continue;
        }
        let channel = call["channel"].as_str().ok_or("a call without a channel")?;
        let view = call["view"].as_u64().unwrap_or(views[0].view.0 as u64) as u32;
        let sender = *views
            .iter()
            .find(|s| s.view.0 == view)
            .ok_or_else(|| format!("call {i}: no view {view}"))?;
        if call["kind"].as_str() == Some("trigger") {
            let services = trigger_services.clone();
            let name = call["name"].as_str().unwrap_or("").to_string();
            let unknown = format!("unknown trigger {name}");
            let args = references.resolve(&call["args"])?;
            let owner = if d1 {
                capture_native(sender)
            } else {
                case_owners.get(&sender.view).cloned()
            };
            let (tx, rx) = mpsc::channel::<Reply>();
            let known = run_on(core.as_ref(), move || {
                // answered on the core thread or later, after a timer, a
                // worker or a dialog: the reply goes through the channel
                let then: Then<IpcValue> = Box::new(move |r| {
                    let _ = tx.send(r);
                });
                PROVIDED.with(|p| {
                    let p = p.borrow();
                    let provided = p.as_ref().expect("the case's provided");
                    // one arm per batch crate that exports `probe_trigger`,
                    // matched on the name's prefix; the batch adds its own
                    match name.split_once('/').map(|(batch, _)| batch) {
                        Some("d1") => shell_ipc_d1::probe_trigger(
                            &name,
                            ViewId(view),
                            owner,
                            &args,
                            &services,
                            provided,
                            then,
                        ),
                        Some("a") => shell_ipc_a::probe_trigger(
                            &name,
                            ViewId(view),
                            &args,
                            &services,
                            provided,
                            then,
                        ),
                        Some("m") => shell_ipc_m::probe_trigger(
                            &name,
                            ViewId(view),
                            &args,
                            &services,
                            provided,
                            then,
                        ),
                        Some("w") => shell_ipc_w::projects::probe_trigger(
                            &name,
                            ViewId(view),
                            &args,
                            &services,
                            provided,
                            then,
                        ),
                        Some("d2") => shell_ipc_d2::probe_trigger(
                            &name,
                            ViewId(view),
                            &args,
                            &services,
                            provided,
                            then,
                        ),
                        _ => false,
                    }
                })
            });
            let answer = if !known {
                json!({ "thrown": unknown })
            } else if call["defer"].as_bool() == Some(true) {
                deferred.insert(i as u64, Pending::Trigger(rx));
                json!({ "deferred": true })
            } else {
                trigger_answer(rx.recv_timeout(CALL_TIMEOUT).ok())
            };
            let drained = drain(&core, &ui, &tracked);
            let mut out = json!({
                "answer": answer,
                "events": references.capture(i, recorder.events.take())?,
            });
            if !drained {
                out["undrained"] = json!(true);
            }
            calls.push(out);
            continue;
        }
        let kind = kind_of(call["kind"].as_str().unwrap_or("invoke"))?;
        let buffers: Vec<Vec<u8>> = call["buffers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| unhex(b.as_str().unwrap_or("")))
            .collect();
        let resolved = references.resolve(&call["args"])?;
        let args = wire::decode_list(&resolved, &buffers).map_err(|e| format!("call {i}: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let name = channel.to_string();
        let request = (kind == ChannelKind::Invoke).then_some(RequestId(i as u64 + 1));
        let native = if d1
            && matches!(
                sender.surface,
                Surface::Slides | Surface::Sheets | Surface::Mindmap | Surface::Diagram
            ) {
            capture_native(sender).map(|owner| owner.context)
        } else if sender.surface == Surface::Slides {
            case_owners
                .get(&sender.view)
                .map(|owner| owner.context.clone())
        } else if sender.surface == Surface::Sheets
            && matches!(
                channel,
                "workbook:create-document"
                    | "sheets:capture-screen-sources"
                    | "sheets:capture-screen-source"
            )
        {
            capture_native(sender).map(|owner| owner.context)
        } else if matches!(
            channel,
            "docs:write-recovery-now-done" | "hwp:recovery-flushed"
        ) || (channel == "docs:close-check-result"
            && matches!(args.get(1), Some(IpcValue::String(_))))
        {
            case_views
                .renderer_generation(sender.view)
                .map(|generation| shell_contract::ipc::NativeContext::with_renderer((), generation))
        } else if d2 && (sender.surface == Surface::Docs || channel.starts_with("pdf:")) {
            // a Docs call carries its document's owner; a view without one has none
            capture_native(sender)
                .filter(|owner| {
                    sender.surface != Surface::Docs
                        || effects.docs_document_identity(owner).is_some()
                })
                .map(|owner| owner.context)
        } else if d2 && matches!(channel, "html:handoff-take" | "html:handoff-run-started") {
            case_views
                .renderer_generation(sender.view)
                .map(|generation| {
                    shell_contract::ipc::NativeContext::with_owner_loss(
                        (),
                        generation.clone(),
                        generation.loss_token(),
                    )
                })
        } else {
            None
        };
        // the one reply that answers under the successor its own request creates
        let mut reply_authority = None;
        let native = match native {
            Some(context)
                if channel == "docs:consume-pending-open" && kind == ChannelKind::Invoke =>
            {
                let synthetic = trigger_services
                    .hosts
                    .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
                    .cloned()
                    .ok_or_else(|| "the probe has no synthetic reply authority".to_owned())?;
                let (owner, authority) =
                    shell_contract::hosts::synthetic::SyntheticReplyAuthority::attach(
                        shell_contract::ipc::NativeRequestOwner {
                            view: sender.view,
                            context,
                        },
                        &shell_contract::runtime::CancelToken::default(),
                    );
                reply_authority = Some((synthetic, authority));
                Some(owner.context)
            }
            other => other,
        };
        core.post(Box::new(move || {
            DISPATCHER.with(|d| {
                let d = d.borrow();
                let d = d.as_ref().expect("the case's dispatcher");
                d.dispatch(
                    sender,
                    kind,
                    request,
                    native,
                    &name,
                    args,
                    Box::new(move |r| match reply_authority {
                        Some((effects, authority)) => {
                            let _ = authority.deliver(&effects, move || {
                                let _ = tx.send(r);
                            });
                        }
                        None => {
                            let _ = tx.send(r);
                        }
                    }),
                );
            })
        }));
        let answer = if call["defer"].as_bool() == Some(true) {
            deferred.insert(i as u64, Pending::Call(kind, channel.to_string(), rx));
            json!({ "deferred": true })
        } else {
            answer(kind, channel, rx.recv_timeout(CALL_TIMEOUT).ok())
        };
        let drained = drain(&core, &ui, &tracked);
        let mut out = json!({
            "answer": answer,
            "events": references.capture(i, recorder.events.take())?,
        });
        if !drained {
            out["undrained"] = json!(true);
        }
        calls.push(out);
    }
    run_on(core.as_ref(), || {
        DISPATCHER.with(|d| drop(d.borrow_mut().take()));
        PROVIDED.with(|p| drop(p.borrow_mut().take()));
    });
    core.stop();
    ui.stop();
    let log = std::mem::take(&mut *log.lock().unwrap());
    Ok(json!({ "calls": calls, "log": log }))
}

/// Waits until the workers are idle and the core and UI queues hold nothing
/// they posted; false when that did not happen in time
fn drain(core: &CoreThread, ui: &CoreThread, tracked: &Tracked) -> bool {
    let until = Instant::now() + CALL_TIMEOUT;
    loop {
        if !tracked.wait_idle(until) {
            return false;
        }
        let spawned = tracked.spawned.load(Ordering::SeqCst);
        run_on(ui, || ());
        run_on(core, || ());
        if *tracked.active.lock().unwrap() == 0 && tracked.spawned.load(Ordering::SeqCst) == spawned
        {
            return true;
        }
    }
}

// The probe runs on the developers' machines and the runners: batch A's
// safeStorage must never reach the person's login keychain or DPAPI key here
// (`feature = "probe"` compiles its OS key store out)
const _: () = assert!(!shell_ipc_a::secrets::OS_KEY_STORE);

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.expect("reading stdin");
        if line.trim().is_empty() {
            continue;
        }
        let out = match serde_json::from_str::<Value>(&line) {
            Ok(request) => {
                let dir = request["dir"].as_str().unwrap_or(".").to_string();
                run_case(&dir, &request["case"]).unwrap_or_else(|e| json!({ "bad": e }))
            }
            Err(e) => json!({ "bad": e.to_string() }),
        };
        writeln!(stdout, "{out}").expect("writing stdout");
        stdout.flush().expect("writing stdout");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(channel: &str) -> Value {
        json!({ "view": 1, "channel": channel })
    }

    fn channels(events: &Events) -> Vec<String> {
        events
            .list
            .lock()
            .unwrap()
            .iter()
            .map(|e| e["channel"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn replay_uses_each_actual_event_identity_and_retains_stale_values() {
        let old =
            json!({"$eventArg":{"step":0,"channel":"copy-now","view":1,"arg":0,"occurrence":0}});
        let new =
            json!({"$eventArg":{"step":1,"channel":"copy-now","view":1,"arg":0,"occurrence":0}});
        let args = json!([old, new, null]);
        let mut refs = EventReferences::default();
        refs.collect(&args).unwrap();
        let first = refs
            .capture(
                0,
                vec![json!({"view":1,"channel":"copy-now","args":[91],"buffers":[]})],
            )
            .unwrap();
        refs.capture(
            1,
            vec![json!({"view":1,"channel":"copy-now","args":[92],"buffers":[]})],
        )
        .unwrap();
        assert_eq!(refs.resolve(&args).unwrap(), json!([91, 92, null]));
        assert_eq!(first[0]["args"][0], "$EVENT:[0,\"copy-now\",1,0,0]:number");
    }

    #[test]
    fn replay_rejects_missing_or_wrong_view_event() {
        let reference =
            json!({"$eventArg":{"step":0,"channel":"copy-now","view":2,"arg":0,"occurrence":0}});
        let mut refs = EventReferences::default();
        refs.collect(&reference).unwrap();
        assert!(refs.resolve(&reference).is_err());
        assert!(
            refs.capture(
                0,
                vec![json!({"view":1,"channel":"copy-now","args":[9],"buffers":[]})]
            )
            .is_err()
        );
    }

    /// A batch whose registration sends `channels`
    fn sending<'a>(
        events: &'a Events,
        channels: &'a [&'a str],
    ) -> impl FnOnce(&mut Table) -> Result<(), RegisterError> + 'a {
        move |_| {
            for channel in channels {
                events.push(sent(channel));
            }
            Ok(())
        }
    }

    #[test]
    fn only_the_groups_own_batch_keeps_what_it_sends_while_registering() {
        let events = Events::default();
        let batches = [
            ("A", sending(&events, &["mcp-tools:changed"])),
            // W's quick-look sends M's channel: the sender's batch counts, not the channel's
            (
                "W",
                sending(
                    &events,
                    &["quick-look:will-show", "quick-look:theme-changed"],
                ),
            ),
            ("M", sending(&events, &["home:open-settings"])),
        ];
        register_keeping_own(&mut Table::default(), batches, Some("W"), &events).unwrap();
        assert_eq!(
            channels(&events),
            ["quick-look:will-show", "quick-look:theme-changed"]
        );
        // what another batch sends once every batch registered is a call's
        events.push(sent("mcp-tools:changed"));
        assert_eq!(channels(&events).last().unwrap(), "mcp-tools:changed");
    }

    #[test]
    fn without_a_batch_everything_is_kept() {
        let events = Events::default();
        let batches = [
            ("A", sending(&events, &["mcp-tools:changed"])),
            ("W", sending(&events, &["quick-look:will-show"])),
        ];
        register_keeping_own(&mut Table::default(), batches, None, &events).unwrap();
        assert_eq!(
            channels(&events),
            ["mcp-tools:changed", "quick-look:will-show"]
        );
    }

    type Register<'a> = dyn FnOnce(&mut Table) -> Result<(), RegisterError> + 'a;

    #[test]
    fn a_worker_sending_while_another_batch_registers_is_kept() {
        let events = Events::default();
        // A's drive port started a worker at A's registration; it reports
        // while M registers, and M's own registration send is still dropped
        let batches: [(&str, Box<Register<'_>>); 2] = [
            ("A", Box::new(|_| Ok(()))),
            (
                "M",
                Box::new(|_| {
                    events.push(sent("home:open-settings"));
                    std::thread::scope(|s| {
                        s.spawn(|| events.push(sent("drive:state-changed")));
                    });
                    Ok(())
                }),
            ),
        ];
        register_keeping_own(&mut Table::default(), batches, Some("A"), &events).unwrap();
        assert_eq!(channels(&events), ["drive:state-changed"]);
    }
}
