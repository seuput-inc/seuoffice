//! Behaviour a renderer can observe: open, edit, undo and save on the fake
//! deck, the save-as dialog, the close round trip that never blocks, and the
//! cleanup when a view goes.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use std::sync::Arc;

use shell_contract::editors::{DocumentEditors, SaveMode};
use shell_contract::ipc::Target;
use shell_contract::runtime::Thread;
use shell_contract::{IpcValue, Surface, ViewId};

use crate::testkit::{Kit, TestRegistry, registered, temp_dir};
use crate::value::{field, num, obj, string};

type Job = Box<dyn FnOnce() + Send>;

enum CoreMessage {
    Run(Job),
    Done,
}

/// The test's own thread is the core thread, the only one that can see the
/// slides app. A job posted from any other thread (a worker, the recent-list
/// queue) is handed to the core thread while it waits for that worker.
struct CoreThread {
    owner: std::thread::ThreadId,
    pump: std::sync::Mutex<Option<std::sync::mpsc::Sender<CoreMessage>>>,
}

impl shell_contract::runtime::Thread for CoreThread {
    fn post(&self, job: Job) {
        if self.is_current() {
            return job();
        }
        let pump = self.pump.lock().unwrap().clone();
        match pump {
            Some(pump) => {
                if let Err(rejected) = pump.send(CoreMessage::Run(job))
                    && let CoreMessage::Run(job) = rejected.0
                {
                    job()
                }
            }
            None => job(),
        }
    }

    fn is_current(&self) -> bool {
        std::thread::current().id() == self.owner
    }
}

impl CoreThread {
    /// Runs `job` as a worker thread would run it, and serves the core jobs it
    /// causes until it finishes, so the caller still sees it done on return.
    fn run_as_worker(&self, job: Job) {
        if !self.is_current() {
            return job();
        }
        struct Finish(std::sync::mpsc::Sender<CoreMessage>);
        impl Drop for Finish {
            fn drop(&mut self) {
                let _ = self.0.send(CoreMessage::Done);
            }
        }
        let (send, receive) = std::sync::mpsc::channel();
        let previous = self.pump.lock().unwrap().replace(send.clone());
        let finish = Finish(send);
        let worker = std::thread::spawn(move || {
            let _finish = finish;
            job()
        });
        while let CoreMessage::Run(job) = receive.recv().expect("the worker keeps its pump open") {
            job()
        }
        *self.pump.lock().unwrap() = previous;
        while let Ok(CoreMessage::Run(job)) = receive.try_recv() {
            job()
        }
        if let Err(panic) = worker.join() {
            std::panic::resume_unwind(panic)
        }
    }
}

struct WorkerThreads {
    core: Arc<CoreThread>,
    held: Arc<crate::testkit::HeldWorkers>,
}

impl shell_contract::runtime::Workers for WorkerThreads {
    fn spawn(&self, job: Job) {
        let core = self.core.clone();
        self.held.spawn(Box::new(move || core.run_as_worker(job)));
    }
}

fn setup(name: &str) -> (Kit, TestRegistry, shell_contract::ipc::Sender, PathBuf) {
    let dir = temp_dir(name);
    // userData exists in a running app: the recent list and the recovery store write there
    std::fs::create_dir_all(dir.join("userdata")).unwrap();
    let mut kit = Kit::new(&dir, crate::insert_fakes);
    // production runs workers off the core thread; the recent-list queue and the
    // save publication reach the app only through core posts
    let core = Arc::new(CoreThread {
        owner: std::thread::current().id(),
        pump: std::sync::Mutex::new(None),
    });
    kit.services.core = core.clone();
    kit.services.workers = Arc::new(WorkerThreads {
        core,
        held: kit.workers.clone(),
    });
    let registry = registered(&kit);
    // the native host gives every capture of one renderer the same origin
    kit.host::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
        .share_capture_origins();
    let sender = kit.view(1, Surface::Slides);
    (kit, registry, sender, dir)
}

/// `testkit::call` with the native context the shell's owned effects capture for
/// the view's renderer: its owner-loss token and the origin its saves compare against.
fn call(
    registry: &TestRegistry,
    sender: shell_contract::ipc::Sender,
    channel: &str,
    args: Vec<IpcValue>,
) -> Result<IpcValue, shell_contract::IpcError> {
    let (kind, handler) = registry
        .handlers
        .get(channel)
        .unwrap_or_else(|| panic!("no handler for {channel}"));
    let native = super::views().and_then(|slides| {
        let generation = slides.0.services.views.renderer_generation(sender.view)?;
        slides
            .0
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()?
            .capture_owner(sender.view, &generation)
            .ok()
            .map(|owner| owner.context)
    });
    let cx = shell_contract::ipc::CallContext {
        sender,
        request: None,
        cancel: shell_contract::runtime::CancelToken::default(),
        native,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    handler.call(
        &cx,
        args,
        shell_contract::ipc::Responder::new(move |reply| {
            let _ = tx.send(reply);
        }),
    );
    let reply = rx
        .try_recv()
        .unwrap_or_else(|_| panic!("{channel} did not answer"));
    if *kind == shell_contract::ipc::ChannelKind::Send {
        assert_eq!(reply.as_ref().ok(), Some(&IpcValue::Undefined));
    }
    reply
}

fn invoke(
    registry: &TestRegistry,
    sender: shell_contract::ipc::Sender,
    channel: &str,
    args: Vec<IpcValue>,
) -> IpcValue {
    call(registry, sender, channel, args).unwrap_or_else(|e| panic!("{channel}: {e}"))
}

/// `addElement` of a text box carrying `text`
fn add_text(registry: &TestRegistry, sender: shell_contract::ipc::Sender, text: &str) -> IpcValue {
    invoke(
        registry,
        sender,
        "slides:add-element",
        vec![obj([
            ("slideIndex", num(0.0)),
            ("fitWidthPx", num(960.0)),
            ("kind", string("textbox")),
            ("xPx", num(10.0)),
            ("yPx", num(10.0)),
            ("wPx", num(100.0)),
            ("hPx", num(40.0)),
            ("text", string(text)),
        ])],
    )
}

fn history_events(kit: &Kit) -> Vec<(bool, bool)> {
    kit.take_events()
        .into_iter()
        .filter(|(_, channel, _)| *channel == "slides:history-changed")
        .map(|(_, _, args)| {
            let state = &args[0];
            (
                matches!(field(state, "canUndo"), IpcValue::Bool(true)),
                matches!(field(state, "canRedo"), IpcValue::Bool(true)),
            )
        })
        .collect()
}

#[test]
fn generated_seed_rollback_preserves_replaced_insertions() {
    use shell_contract::runtime::CancelToken;
    let (kit, _, issuer, _) = setup("slides-seed-rollback");
    let target = kit.view(2, Surface::Slides);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let slides = super::views().unwrap();
    let (_, reservation) =
        shell_contract::ipc::NativeClosureIssuer::new(target.view, created.clone());
    let queue = |name: &str| {
        slides
            .queue_seed_owned(
                target.view,
                name.as_bytes().to_vec(),
                name.into(),
                source.clone(),
                &[],
                CancelToken::default(),
                CancelToken::default(),
                created.clone(),
                reservation.clone(),
                &mut || Ok(()),
            )
            .unwrap()
    };
    let first = queue("first");
    let second = queue("second");
    assert!(first.rollback().is_err());
    assert!(!first.was_consumed());
    second.rollback().unwrap();
    second.rollback().unwrap();
    first.rollback().unwrap();
    assert!(!first.was_consumed());
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
}

#[test]
fn staged_generated_seed_is_inserted_only_after_initializer_succeeds() {
    use shell_contract::{ipc::NativeClosureIssuer, runtime::CancelToken};
    let (kit, _, issuer, _) = setup("slides-seed-staged");
    let target = kit.view(2, Surface::Slides);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let (_, reservation, browser) =
        NativeClosureIssuer::new_with_owner(target.view, created.clone());
    let slides = super::views().unwrap();
    let prepare = || {
        slides
            .prepare_seed_in_transition(
                target.view,
                vec![1],
                "deck".into(),
                source.clone(),
                CancelToken::default(),
                created.clone(),
                reservation.clone(),
                browser.clone(),
            )
            .unwrap()
    };
    let rejected = prepare();
    let failed = reservation.with_initial_owner(&browser, |transition| {
        rejected.commit(transition, &mut || Err("rejected".into()))
    });
    assert!(failed.is_err());
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
    let cancelled = prepare();
    cancelled.receipt().rollback().unwrap();
    assert!(
        reservation
            .with_initial_owner(&browser, |transition| cancelled
                .commit(transition, &mut || Ok(())))
            .is_err()
    );
    let prepared = prepare();
    let preallocated = prepared.receipt();
    let receipt = reservation
        .with_initial_owner(&browser, |transition| {
            prepared.commit(transition, &mut || Ok(()))
        })
        .unwrap();
    assert!(Rc::ptr_eq(&preallocated, &receipt));
    assert!(
        slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
    receipt.rollback().unwrap();
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
}

#[test]
fn superseded_seed_retirement_preserves_successor() {
    use shell_contract::{ipc::NativeClosureIssuer, runtime::CancelToken};
    let (kit, _, issuer, _) = setup("slides-seed-superseded");
    let target = kit.view(2, Surface::Slides);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let (close, reservation, first_browser) =
        NativeClosureIssuer::new_with_owner(target.view, created.clone());
    let slides = super::views().unwrap();
    let first = slides
        .prepare_seed_in_transition(
            target.view,
            vec![1],
            "first".into(),
            source.clone(),
            CancelToken::default(),
            created.clone(),
            reservation.clone(),
            first_browser.clone(),
        )
        .unwrap();
    let first = reservation
        .with_initial_owner(&first_browser, |transition| {
            first.commit(transition, &mut || Ok(()))
        })
        .unwrap();
    let replacement = reservation.prepare_replacement(&first_browser).unwrap();
    let second = slides
        .prepare_seed_in_transition(
            target.view,
            vec![2],
            "second".into(),
            source,
            CancelToken::default(),
            created,
            reservation.clone(),
            replacement.successor().clone(),
        )
        .unwrap();
    let (_, proof, second) = replacement
        .commit(|transition, _| second.commit(transition, &mut || Ok(())))
        .unwrap();
    let (_, foreign_reservation, foreign_owner) = NativeClosureIssuer::new_with_owner(
        ViewId(99),
        shell_contract::runtime::RendererGeneration::new(),
    );
    foreign_reservation
        .with_initial_owner(&foreign_owner, |_| Ok(()))
        .unwrap();
    let (_, foreign_proof, ()) = foreign_reservation
        .prepare_replacement(&foreign_owner)
        .unwrap()
        .commit(|_, _| Ok(()))
        .unwrap();
    assert!(first.retire_superseded(&foreign_proof).is_err());
    first.retire_superseded(&proof).unwrap();
    first.retire_superseded(&proof).unwrap();
    let closed_successor = close.closed();
    assert!(first.retire_closed(&closed_successor).is_err());
    assert!(
        slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
    second.rollback().unwrap();
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
}

#[test]
fn generated_seed_requires_the_exact_native_close_receipt() {
    use shell_contract::{ipc::NativeClosureIssuer, runtime::CancelToken};
    let (kit, _, issuer, _) = setup("slides-seed-close-proof");
    let target = kit.view(2, Surface::Slides);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let (close_issuer, reservation) = NativeClosureIssuer::new(target.view, created.clone());
    let slides = super::views().unwrap();
    let receipt = slides
        .queue_seed_owned(
            target.view,
            vec![1],
            "deck".into(),
            source,
            &[],
            CancelToken::default(),
            CancelToken::default(),
            created.clone(),
            reservation,
            &mut || Ok(()),
        )
        .unwrap();
    created.revoke();
    assert!(receipt.rollback().is_err());
    let (wrong, _) = NativeClosureIssuer::new(target.view, created.clone());
    assert!(receipt.retire_closed(&wrong.closed()).is_err());
    receipt.retire_closed(&close_issuer.not_created()).unwrap();
    receipt.rollback().unwrap();
    receipt.rollback().unwrap();
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
}

#[test]
fn closed_native_actor_leaves_no_session_and_no_seed() {
    use shell_contract::{ipc::NativeClosureIssuer, runtime::CancelToken};
    let (kit, registry, issuer, _) = setup("slides-native-actor-close");
    let target = kit.view(2, Surface::Slides);
    invoke(&registry, target, "slides:new-blank", vec![num(960.0)]);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let (close, reservation, browser) =
        NativeClosureIssuer::new_with_owner(target.view, created.clone());
    let slides = super::views().unwrap();
    let mut actor = slides
        .prepare_native_tab(target.view, &created, None, browser.clone())
        .unwrap();
    let seed = slides
        .prepare_seed_in_transition(
            target.view,
            vec![1],
            "deck".into(),
            source,
            CancelToken::default(),
            created.clone(),
            reservation.clone(),
            browser.clone(),
        )
        .unwrap();
    let seed = reservation
        .with_initial_owner(&browser, |transition| {
            actor.commit(transition, &mut || Ok(()))?;
            seed.commit(transition, &mut || Ok(()))
        })
        .unwrap();
    drop(seed);
    assert!(slides.has_session(target.view));
    assert!(
        slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
    let (wrong, _) = NativeClosureIssuer::new(target.view, created.clone());
    assert!(
        slides
            .retire_native_closed(target.view, &wrong.closed())
            .is_err()
    );
    assert!(slides.has_session(target.view));
    let receipt = close.closed();
    slides.retire_native_closed(target.view, &receipt).unwrap();
    assert!(!slides.has_session(target.view));
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
    assert!(slides.retire_native_closed(target.view, &receipt).is_err());
}

#[test]
fn adopted_seed_close_retires_only_the_exact_insertion() {
    use shell_contract::{ipc::NativeClosureIssuer, runtime::CancelToken};
    let (kit, _, issuer, _) = setup("slides-adopted-seed-close");
    let target = kit.view(2, Surface::Slides);
    let effects = kit
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let source = effects
        .capture_owner(
            issuer.view,
            &kit.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let created = kit.services.views.renderer_generation(target.view).unwrap();
    let (close, reservation) = NativeClosureIssuer::new(target.view, created.clone());
    let slides = super::views().unwrap();
    let queue = |name: &str| {
        slides
            .queue_seed_owned(
                target.view,
                name.as_bytes().to_vec(),
                name.into(),
                source.clone(),
                &[],
                CancelToken::default(),
                CancelToken::default(),
                created.clone(),
                reservation.clone(),
                &mut || Ok(()),
            )
            .unwrap()
    };
    let first = queue("first");
    let second = queue("second");
    let proof = close.closed();
    assert!(first.retire_adopted(&proof).is_err());
    second.retire_adopted(&proof).unwrap();
    second.retire_adopted(&proof).unwrap();
    assert!(first.rollback().is_err());
    assert!(
        !slides
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&target.view)
    );
}

#[test]
fn template_cover_runs_off_core_and_rejects_a_destroyed_renderer() {
    let (kit, registry, sender, _) = setup("template-cover-revoked");
    kit.hold_workers();
    let reply = crate::testkit::start(
        &registry,
        sender,
        "slides:template-cover-render",
        vec![string("plain"), string("en")],
    );
    assert!(reply.try_recv().is_err());
    kit.destroy(sender.view);
    kit.release_workers();
    assert!(reply.try_recv().unwrap().is_err());
}

fn presenter_fixture(
    name: &str,
) -> (
    Kit,
    TestRegistry,
    shell_contract::ipc::Sender,
    super::engine::RenderOwner,
) {
    let dir = temp_dir(name);
    std::fs::create_dir_all(dir.join("userdata")).unwrap();
    let kit = Kit::new(&dir, |hosts| {
        crate::insert_fakes(hosts);
        super::insert_fakes(
            hosts,
            &serde_json::json!({"slides": {"externalDisplay": true}}),
        );
    });
    let registry = registered(&kit);
    let sender = kit.view(1, Surface::Slides);
    let audience = kit.view(100, Surface::Slides);
    let generation = kit
        .services
        .views
        .renderer_generation(audience.view)
        .unwrap();
    let owner = super::engine::RenderOwner {
        view: audience.view,
        context: shell_contract::ipc::NativeContext::with_renderer((), generation),
    };
    let shell = kit.host::<std::sync::Arc<super::fake::FakeShell>>();
    *shell.audience_owner.lock().unwrap() = Some(owner.clone());
    (kit, registry, sender, owner)
}

#[test]
fn presenter_closes_window_when_audience_binding_fails() {
    let (kit, registry, sender, audience) = presenter_fixture("slides-audience-bind-failure");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    audience.context.renderer().unwrap().revoke();
    assert!(call(&registry, sender, "slides:presenter-start", vec![]).is_err());
    let slides = super::views().unwrap();
    assert!(slides.has_session(sender.view));
    assert!(!slides.has_session(audience.view));
    let shell = kit.host::<std::sync::Arc<super::fake::FakeShell>>();
    assert_eq!(
        *shell.closed_audiences.lock().unwrap(),
        vec![shell_contract::WindowId(100)]
    );

    let replacement = kit.view(100, Surface::Slides);
    let generation = kit
        .services
        .views
        .renderer_generation(replacement.view)
        .unwrap();
    *shell.audience_owner.lock().unwrap() = Some(super::engine::RenderOwner {
        view: replacement.view,
        context: shell_contract::ipc::NativeContext::with_renderer((), generation),
    });
    let result = invoke(&registry, sender, "slides:presenter-start", vec![]);
    assert_eq!(field(&result, "audience"), &IpcValue::Bool(true));
    assert!(slides.has_session(replacement.view));
    invoke(&registry, sender, "slides:presenter-end", vec![]);
    assert!(!slides.has_session(replacement.view));
}

#[test]
fn revoked_presenter_owner_cannot_open_audience() {
    let (kit, registry, sender, _) = presenter_fixture("slides-presenter-revoked");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let context = shell_contract::ipc::CallContext {
        sender,
        request: None,
        cancel: shell_contract::runtime::CancelToken::default(),
        native: Some(shell_contract::ipc::NativeContext::with_renderer(
            (),
            generation.clone(),
        )),
    };
    generation.revoke();
    let (_, handler) = registry.handlers.get("slides:presenter-start").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    handler.call(
        &context,
        vec![],
        shell_contract::ipc::Responder::new(move |reply| {
            let _ = tx.send(reply);
        }),
    );
    assert!(rx.try_recv().unwrap().is_err());
    let shell = kit.host::<std::sync::Arc<super::fake::FakeShell>>();
    assert!(shell.audience_owner.lock().unwrap().is_some());
    assert!(shell.closed_audiences.lock().unwrap().is_empty());
    assert!(!super::views().unwrap().has_session(ViewId(100)));
}

#[test]
fn dirty_engine_failure_rejects_close_without_discarding_session() {
    let (kit, registry, sender, _) = setup("slides-dirty-engine-error");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let views = super::views().unwrap();
    let deck = views.0.state.borrow().session(sender.view).unwrap().deck();
    let engine = kit.host::<super::engine::SlidesEngineHost>();
    engine.release(deck);
    assert!(views.is_dirty(sender.view).is_err());
    assert!(call(&registry, sender, "slides:is-dirty", vec![]).is_err());
    let answer = Rc::new(RefCell::new(None));
    let result = answer.clone();
    views.0.request_close(
        sender.view,
        None,
        Box::new(move |value| *result.borrow_mut() = Some(value)),
    );
    assert!(answer.borrow().as_ref().unwrap().is_err());
    assert!(views.has_session(sender.view));
}

#[test]
fn private_fonts_work_before_open_and_reject_revoked_owner() {
    let (kit, registry, sender, _) = setup("slides-startup-private-fonts");
    assert!(!super::views().unwrap().has_session(sender.view));
    assert_eq!(
        invoke(&registry, sender, "slides:private-font-faces", vec![]),
        IpcValue::Array(vec![])
    );
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:private-font-data",
            vec![string("absent")]
        ),
        IpcValue::Null
    );
    kit.services
        .views
        .renderer_generation(sender.view)
        .unwrap()
        .revoke();
    assert!(call(&registry, sender, "slides:private-font-faces", vec![]).is_err());
    assert!(
        call(
            &registry,
            sender,
            "slides:private-font-data",
            vec![string("absent")]
        )
        .is_err()
    );
}

#[test]
fn window_open_keeps_the_creation_owner_after_view_replacement() {
    let (kit, _registry, sender, dir) = setup("slides-window-owner");
    let old_generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let old = super::engine::RenderOwner {
        view: sender.view,
        context: shell_contract::ipc::NativeContext::with_owner_loss(
            (),
            old_generation.clone(),
            old_generation.loss_token(),
        ),
    };
    let path = dir.join("window.pptx");
    std::fs::write(
        &path,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let replacement = kit.view(sender.view.0, Surface::Slides);
    super::open_file_in_window(old, path.to_str().unwrap());
    assert!(!super::views().unwrap().has_session(replacement.view));
    let current_generation = kit
        .services
        .views
        .renderer_generation(replacement.view)
        .unwrap();
    let current = super::engine::RenderOwner {
        view: replacement.view,
        context: shell_contract::ipc::NativeContext::with_owner_loss(
            (),
            current_generation.clone(),
            current_generation.loss_token(),
        ),
    };
    super::open_file_in_window(current, path.to_str().unwrap());
    assert!(super::views().unwrap().has_session(replacement.view));
}
#[test]
fn shell_view_seed_opens_once_as_unsaved_and_is_forgotten_on_close() {
    let (kit, registry, sender, _) = setup("slides-shell-seed");
    let slides = super::views().unwrap();
    slides.register_tab(sender.view);
    assert_eq!(slides.active_view(), Some(sender.view));
    let deck = b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n";
    slides.queue_seed(sender.view, deck, "Restored.pptx");
    let opened = invoke(
        &registry,
        sender,
        "slides:consume-pending-open",
        vec![num(960.0)],
    );
    assert_eq!(field(&opened, "path"), &string(""));
    assert!(slides.is_dirty(sender.view).unwrap());
    kit.destroy(sender.view);
    assert_eq!(slides.active_view(), None);
    assert!(!slides.is_dirty(sender.view).unwrap());
}

#[test]
fn shell_transaction_rejects_invalid_ops_and_closed_views() {
    let (kit, registry, sender, _) = setup("slides-shell-transaction");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let slides = super::views().unwrap();
    assert!(slides.has_session(sender.view));
    let owner = shell_contract::ipc::NativeRequestOwner {
        view: sender.view,
        context: shell_contract::ipc::NativeContext::with_renderer(
            (),
            kit.services.views.renderer_generation(sender.view).unwrap(),
        ),
    };
    let result = slides
        .apply_transaction(&owner, &obj([("ops", IpcValue::Array(vec![]))]))
        .unwrap();
    assert_eq!(field(&result, "applied"), &IpcValue::Bool(false));
    assert!(!slides.is_dirty(sender.view).unwrap());
    kit.destroy(sender.view);
    assert!(
        slides
            .apply_transaction(&owner, &obj([("ops", IpcValue::Array(vec![]))]))
            .is_err()
    );
}

#[test]
fn unauthenticated_transaction_cannot_mutate_deck() {
    let (_kit, registry, sender, _) = setup("slides-unauthorized-transaction");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let slides = super::views().unwrap();
    let before = slides.deck_model(sender.view).unwrap();
    let (_, handler) = registry.handlers.get("slides:apply-txn").unwrap();
    let context = shell_contract::ipc::CallContext {
        sender,
        request: None,
        cancel: shell_contract::runtime::CancelToken::default(),
        native: None,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    handler.call(
        &context,
        vec![obj([(
            "ops",
            IpcValue::Array(vec![obj([
                ("op", string("deleteSlide")),
                ("target", obj([("slide", num(0.0))])),
            ])]),
        )])],
        shell_contract::ipc::Responder::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    assert!(rx.try_recv().unwrap().is_err());
    assert_eq!(slides.deck_model(sender.view).unwrap(), before);
}

#[test]
fn channels_without_a_session_answer_null() {
    let (_kit, registry, sender, _dir) = setup("slides-nosession");
    for channel in [
        "slides:add-element",
        "slides:undo",
        "slides:get-render-slides",
    ] {
        let args = if channel == "slides:add-element" {
            vec![obj([("slideIndex", num(0.0))])]
        } else {
            vec![]
        };
        assert_eq!(invoke(&registry, sender, channel, args), IpcValue::Null);
    }
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(false)
    );
    let saved = invoke(&registry, sender, "slides:save", vec![]);
    assert_eq!(field(&saved, "error"), &string("no file open"));
}

#[test]
fn open_edit_undo_save_on_the_fake_deck() {
    let (kit, registry, sender, dir) = setup("slides-flow");
    let opened = invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    assert_eq!(field(&opened, "path"), &string(""));
    assert!(matches!(field(&opened, "slides"), IpcValue::Array(s) if s.len() == 1));
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(false)
    );

    // an edit marks the deck dirty, answers the rebuilt slide and notifies history
    kit.take_events();
    let added = add_text(&registry, sender, "hello");
    assert_eq!(field(&added, "sourceId"), &string("e1"));
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(true)
    );
    let events = history_events(&kit);
    assert_eq!(events.last(), Some(&(true, false)));

    // undo restores the deck and enables redo
    let slides = invoke(&registry, sender, "slides:undo", vec![]);
    let IpcValue::Array(slides) = slides else {
        panic!("undo answers the slides")
    };
    assert_eq!(field(&slides[0], "elements"), &IpcValue::Array(vec![]));
    assert_eq!(history_events(&kit).last(), Some(&(false, true)));
    // nothing left to undo
    assert_eq!(
        invoke(&registry, sender, "slides:undo", vec![]),
        IpcValue::Null
    );
    invoke(&registry, sender, "slides:redo", vec![]);
    assert_eq!(history_events(&kit).last(), Some(&(true, false)));

    // an untitled deck asks where the first save goes
    let target = dir.join("deck.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone()));
    let saved = invoke(&registry, sender, "slides:save", vec![]);
    assert_eq!(field(&saved, "ok"), &IpcValue::Bool(true));
    assert_eq!(field(&saved, "path"), &string(target.to_string_lossy()));
    assert!(std::fs::read(&target).unwrap().starts_with(b"FAKEPPTX"));
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(false)
    );
    // the recent list remembers it
    let recent = invoke(&registry, sender, "slides:recent", vec![]);
    assert_eq!(
        recent,
        IpcValue::Array(vec![string(target.to_string_lossy())])
    );

    // reopening the saved file in another view reads what was written
    let other = kit.view(2, Surface::Slides);
    let reopened = invoke(
        &registry,
        other,
        "slides:open-path",
        vec![string(target.to_string_lossy()), num(960.0)],
    );
    // the same path is already open: the view attaches to the live session
    assert_eq!(field(&reopened, "path"), &string(target.to_string_lossy()));
    let IpcValue::Array(slides) = field(&reopened, "slides") else {
        panic!("open answers slides")
    };
    assert_eq!(
        field(&slides[0], "elements"),
        &IpcValue::Array(vec![obj([("id", string("e1")), ("text", string("hello"))])])
    );
}

#[test]
fn save_as_cancel_writes_nothing_and_a_pick_writes_the_deck() {
    let (kit, registry, sender, dir) = setup("slides-saveas");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);

    // the dialog is cancelled: nothing is written and the deck keeps no path
    let cancelled = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(cancelled, obj([("ok", IpcValue::Bool(false))]));

    // a PDF pick writes no deck; the renderer runs the export
    let pdf = dir.join("deck.pdf");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(pdf.clone()));
    let exported = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(field(&exported, "exportAs"), &string("pdf"));
    assert!(!pdf.exists());

    // a name without an extension becomes a .pptx
    let named = dir.join("named");
    kit.dialogs.saves.lock().unwrap().push_back(Some(named));
    let saved = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(field(&saved, "ok"), &IpcValue::Bool(true));
    assert!(dir.join("named.pptx").exists());
    // the deck now has a path: a plain save writes to it without asking
    let again = invoke(&registry, sender, "slides:save", vec![]);
    assert_eq!(field(&again, "ok"), &IpcValue::Bool(true));
    assert_eq!(kit.dialogs.saves.lock().unwrap().len(), 0);
}

#[test]
fn the_close_round_trip_waits_for_the_renderer_without_blocking() {
    let (kit, registry, sender, _dir) = setup("slides-close");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    add_text(&registry, sender, "x");
    kit.take_events();
    let editor = slides_editor(&registry);
    assert!(editor.owns(ViewId(1)));
    assert_eq!(editor.document_path(ViewId(1)), None);

    // Save (button 0): the renderer is asked, and the close waits for its answer
    let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
    let got = answer.clone();
    kit.dialogs.messages.lock().unwrap().push_back(0);
    editor.request_close(
        ViewId(1),
        None,
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert!(
        answer.borrow().is_none(),
        "the continuation is stored, not awaited"
    );
    let events = kit.take_events();
    assert!(
        events
            .iter()
            .any(|(target, channel, _)| *target == Target::View(ViewId(1))
                && *channel == "slides:close-save-request")
    );
    invoke(
        &registry,
        sender,
        "slides:close-save-result",
        vec![IpcValue::Bool(true)],
    );
    assert_eq!(*answer.borrow(), Some(Ok(true)));

    // Cancel (button 2): the view stays open and no save is requested
    let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
    let got = answer.clone();
    kit.dialogs.messages.lock().unwrap().push_back(2);
    editor.request_close(
        ViewId(1),
        None,
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert_eq!(*answer.borrow(), Some(Ok(false)));
    assert!(
        kit.take_events()
            .iter()
            .all(|(_, c, _)| *c != "slides:close-save-request")
    );

    // a save the renderer reports as failed
    let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
    let got = answer.clone();
    editor.request_save(
        ViewId(1),
        SaveMode::Save,
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert!(answer.borrow().is_none());
    invoke(
        &registry,
        sender,
        "slides:close-save-result",
        vec![IpcValue::Bool(false)],
    );
    assert_eq!(*answer.borrow(), Some(Ok(false)));

    // a clean deck closes without a question
    let clean = kit.view(3, Surface::Slides);
    invoke(&registry, clean, "slides:new-blank", vec![num(960.0)]);
    let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
    let got = answer.clone();
    editor.request_close(
        ViewId(3),
        None,
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert_eq!(*answer.borrow(), Some(Ok(true)));
}

/// An answered save's timer is spent (the TypeScript clears it): when it
/// fires anyway it leaves the view's newer save waiting for its own answer
#[test]
fn an_answered_saves_timer_leaves_the_next_save_waiting() {
    let (_kit, registry, sender, _dir) = setup("slides-save-timer");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    add_text(&registry, sender, "x");
    let editor = slides_editor(&registry);
    let app = super::APPS.with(|apps| apps.borrow().last().cloned().unwrap());
    let save = |editor: &Rc<dyn DocumentEditors>| {
        let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
        let got = answer.clone();
        editor.request_save(
            ViewId(1),
            SaveMode::Save,
            Box::new(move |r| *got.borrow_mut() = Some(r)),
        );
        answer
    };

    let first = save(&editor);
    let first_ticket = app.state.borrow().next_ticket;
    invoke(
        &registry,
        sender,
        "slides:close-save-result",
        vec![IpcValue::Bool(true)],
    );
    assert_eq!(*first.borrow(), Some(Ok(true)));

    let second = save(&editor);
    app.close_save_expired(ViewId(1), first_ticket);
    assert!(second.borrow().is_none(), "the next save still waits");
    invoke(
        &registry,
        sender,
        "slides:close-save-result",
        vec![IpcValue::Bool(true)],
    );
    assert_eq!(*second.borrow(), Some(Ok(true)));
}

/// The menu's Save As waits for the renderer's `slides:save-as` and reports
/// what it came to: cancelled is `false`, a picked file is `true` and exists
#[test]
fn a_menu_save_as_resumes_with_what_the_renderers_save_as_came_to() {
    let (kit, registry, sender, dir) = setup("slides-menu-saveas");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let editor = slides_editor(&registry);
    let ask = |editor: &Rc<dyn DocumentEditors>| {
        let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
        let got = answer.clone();
        editor.request_save(
            ViewId(1),
            SaveMode::SaveAs,
            Box::new(move |r| *got.borrow_mut() = Some(r)),
        );
        answer
    };

    kit.take_events();
    let cancelled = ask(&editor);
    assert!(
        cancelled.borrow().is_none(),
        "the continuation is stored, not awaited"
    );
    assert!(kit.take_events().iter().any(|(target, channel, args)| {
        *target == Target::View(ViewId(1))
            && *channel == "slides:menu"
            && args == &vec![string("save-as")]
    }));
    // the dialog is cancelled
    invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(*cancelled.borrow(), Some(Ok(false)));

    // a picked file: the answer reaches the renderer unchanged and the caller hears true
    let picked = ask(&editor);
    let target = dir.join("menu.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone()));
    let reply = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(field(&reply, "ok"), &IpcValue::Bool(true));
    assert_eq!(*picked.borrow(), Some(Ok(true)));
    assert!(target.exists());

    // a copy format writes no deck: not saved
    let copy = ask(&editor);
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(dir.join("copy.pdf")));
    invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(*copy.borrow(), Some(Ok(false)));

    // a call nobody asked for resumes nothing
    invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
}

/// A newer Save As replaces the older (which resumes `false`); an answered
/// one's timer does nothing; the view going ends a waiting one
#[test]
fn a_waiting_menu_save_as_is_replaced_timed_out_or_ended_with_its_view() {
    let (kit, registry, sender, dir) = setup("slides-menu-saveas-ends");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let editor = slides_editor(&registry);
    let app = super::APPS.with(|apps| apps.borrow().last().cloned().unwrap());
    let ask = |editor: &Rc<dyn DocumentEditors>| {
        let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
        let got = answer.clone();
        editor.request_save(
            ViewId(1),
            SaveMode::SaveAs,
            Box::new(move |r| *got.borrow_mut() = Some(r)),
        );
        answer
    };

    let older = ask(&editor);
    let newer = ask(&editor);
    assert_eq!(*older.borrow(), Some(Ok(false)), "the older resumes false");
    assert!(newer.borrow().is_none());

    // the answered one's timer is spent: it leaves the next request waiting
    let spent = app.state.borrow().next_ticket;
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(dir.join("a.pptx")));
    invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(*newer.borrow(), Some(Ok(true)));
    let waiting = ask(&editor);
    app.save_as_expired(ViewId(1), spent);
    assert!(waiting.borrow().is_none(), "the next request still waits");
    // its own timer ends it
    let own = app.state.borrow().next_ticket;
    app.save_as_expired(ViewId(1), own);
    assert_eq!(*waiting.borrow(), Some(Ok(false)));

    // the view going ends a waiting one
    let last = ask(&editor);
    kit.destroy(ViewId(1));
    assert_eq!(*last.borrow(), Some(Ok(false)));
}

#[test]
fn a_view_that_goes_ends_its_session_and_resumes_a_waiting_close() {
    let (kit, registry, sender, _dir) = setup("slides-gone");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    add_text(&registry, sender, "x");
    let editor = slides_editor(&registry);
    let answer: Rc<RefCell<Option<Result<bool, _>>>> = Rc::default();
    let got = answer.clone();
    editor.request_save(
        ViewId(1),
        SaveMode::Save,
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert!(answer.borrow().is_none());

    kit.destroy(ViewId(1));
    // the renderer is gone: the save is reported as not done
    assert_eq!(*answer.borrow(), Some(Ok(false)));
    assert!(!editor.owns(ViewId(1)));
    assert_eq!(
        invoke(&registry, sender, "slides:get-render-slides", vec![]),
        IpcValue::Null
    );
}

#[test]
fn two_views_share_a_deck_and_both_hear_the_edit() {
    let (kit, registry, sender, dir) = setup("slides-shared");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let target = dir.join("shared.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone()));
    invoke(&registry, sender, "slides:save", vec![]);
    let other = kit.view(2, Surface::Slides);
    invoke(
        &registry,
        other,
        "slides:open-path",
        vec![string(target.to_string_lossy()), num(960.0)],
    );
    kit.take_events();
    add_text(&registry, sender, "shared");
    let deck_events: Vec<_> = kit
        .take_events()
        .into_iter()
        .filter(|(_, channel, _)| *channel == "slides:deck-changed")
        .map(|(target, _, _)| target)
        .collect();
    // a partial update reaches every attached window, the editing one included
    assert_eq!(
        deck_events,
        vec![Target::View(ViewId(1)), Target::View(ViewId(2))]
    );
}

#[test]
fn replacing_a_shared_deck_retires_all_three_documents_before_notifying_observers() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (kit, registry, first, dir) = setup("slides-shared-replace-three");
    invoke(&registry, first, "slides:new-blank", vec![num(960.0)]);
    let path = dir.join("shared.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(path.clone()));
    invoke(&registry, first, "slides:save", vec![]);
    for id in [2, 3] {
        let sender = kit.view(id, Surface::Slides);
        invoke(
            &registry,
            sender,
            "slides:open-path",
            vec![string(path.to_string_lossy()), num(960.0)],
        );
    }
    let app = super::views().unwrap().0;
    let owner = app.state.borrow().owner_of(first.view).unwrap().clone();
    {
        let audience_owner = app.state.borrow().owner_of(ViewId(2)).unwrap().clone();
        let mut state = app.state.borrow_mut();
        state.presenter_shows.insert(
            first.view,
            super::state::PresenterShow {
                audience: Some((ViewId(2), shell_contract::WindowId(100))),
                audience_owner: Some(audience_owner),
                last_sync: Some(string("current slide")),
                owner: owner.clone(),
                ticket: 1,
                starting: false,
                start_waiters: Vec::new(),
            },
        );
        state.audience_presenter.insert(ViewId(2), first.view);
    }
    let authority =
        super::shared_open::SharedOpen::capture(&app, &owner, owner.loss_token().unwrap()).unwrap();
    assert_eq!(authority.participants.len(), 3);
    let old = authority.deck.unwrap();
    let snapshot = app.engine.snapshot(old, false).unwrap();
    let replacement = app
        .engine
        .open(&app.engine.create_blank_pptx().unwrap())
        .unwrap();
    app.engine
        .transfer_history(old, replacement, &authority.participants)
        .unwrap();
    let leases = authority
        .participants
        .iter()
        .map(|owner| owner.context.document_lease().unwrap().clone())
        .collect::<Vec<_>>();
    let notified = Arc::new(AtomicUsize::new(0));
    let index = app.registry_index;
    let subscriptions = leases
        .iter()
        .map(|lease| {
            let notified = notified.clone();
            lease.loss_token().on_cancel(move || {
                assert!(
                    super::with_app(index, |app| {
                        let state = app.state.borrow_mut();
                        for view in [ViewId(1), ViewId(2), ViewId(3)] {
                            assert_eq!(state.session(view).unwrap().deck(), replacement);
                            assert!(state.save_lifetime(view).unwrap().is_active());
                        }
                        let show = state.presenter_shows.get(&ViewId(1)).unwrap();
                        assert!(
                            show.owner
                                .context
                                .document_lease()
                                .unwrap()
                                .same_generation(state.save_lifetime(ViewId(1)).unwrap())
                        );
                        assert!(
                            show.audience_owner
                                .as_ref()
                                .unwrap()
                                .context
                                .document_lease()
                                .unwrap()
                                .same_generation(state.save_lifetime(ViewId(2)).unwrap())
                        );
                        assert_eq!(show.last_sync, Some(string("current slide")));
                    })
                    .is_some()
                );
                notified.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect::<Vec<_>>();
    let session = super::state::SlideSession::new(
        "",
        super::state::DeckHandle::new(replacement, app.engine.clone()),
        960.0,
    );
    let (_, completed) = authority
        .commit(&app, &owner, session, Some(snapshot), |_| {})
        .unwrap();
    assert_eq!(completed.view, first.view);
    assert!(leases.iter().all(|lease| !lease.is_active()));
    assert_eq!(notified.load(Ordering::SeqCst), 3);
    drop(subscriptions);
}

#[test]
fn history_batches_collapse_edits_into_one_undo_step() {
    let (_kit, registry, sender, _dir) = setup("slides-batch");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    assert_eq!(
        invoke(&registry, sender, "slides:history-batch-begin", vec![]),
        IpcValue::Bool(true)
    );
    add_text(&registry, sender, "a");
    add_text(&registry, sender, "b");
    let id = invoke(&registry, sender, "slides:history-batch-end", vec![]);
    assert!(matches!(id, IpcValue::Number(_)));
    let IpcValue::Array(slides) = invoke(&registry, sender, "slides:undo", vec![]) else {
        panic!("one undo takes back both edits")
    };
    assert_eq!(field(&slides[0], "elements"), &IpcValue::Array(vec![]));
    assert_eq!(
        invoke(&registry, sender, "slides:undo", vec![]),
        IpcValue::Null
    );
}

/// The slides editor among the batch's editors: slides registers after the
/// other apps of the batch, and `editor_for` answers the first that owns a
/// view
fn slides_editor(registry: &TestRegistry) -> Rc<dyn DocumentEditors> {
    registry
        .provided
        .all::<dyn DocumentEditors>()
        .last()
        .cloned()
        .expect("the batch provides editors")
}

#[test]
fn save_as_png_authorizes_one_image_export_for_that_tab_only() {
    let (kit, registry, sender, dir) = setup("slides-png");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let png = dir.join("shot.png");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(png.clone()));
    let picked = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    assert_eq!(field(&picked, "exportAs"), &string("png"));

    let export = |target: &std::path::Path, view| {
        invoke(
            &registry,
            view,
            "slides:export-images",
            vec![obj([
                ("format", string("png")),
                ("saveAsTarget", string(target.to_string_lossy())),
                ("saveRequestId", field(&picked, "saveRequestId").clone()),
                (
                    "pngsBase64",
                    IpcValue::Array(vec![string("AQID"), string("BAUG")]),
                ),
            ])],
        )
    };
    // another tab, or another name, is refused
    let other = kit.view(2, Surface::Slides);
    let refused = export(&png, other);
    assert_eq!(
        field(&refused, "error"),
        &string("The save response belongs to another renderer")
    );
    let elsewhere = export(&dir.join("other.png"), sender);
    assert_eq!(field(&elsewhere, "ok"), &IpcValue::Bool(false));
    // the picked name writes one numbered file per slide, once
    let done = export(&png, sender);
    assert_eq!(field(&done, "ok"), &IpcValue::Bool(true));
    assert_eq!(
        std::fs::read(dir.join("shot-01.png")).unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(
        std::fs::read(dir.join("shot-02.png")).unwrap(),
        vec![4, 5, 6]
    );
    let again = export(&png, sender);
    assert_eq!(field(&again, "ok"), &IpcValue::Bool(false));
}

#[test]
fn a_replacement_renderer_cannot_consume_the_previous_save_as_copy() {
    let (kit, registry, sender, dir) = setup("slides-png-replaced");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let target = dir.join("old-owner.png");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone()));
    let picked = invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    let request = field(&picked, "saveRequestId").clone();
    assert!(matches!(request, IpcValue::String(_)));
    kit.destroy(sender.view);
    let replacement = kit.view(sender.view.0, Surface::Slides);
    invoke(&registry, replacement, "slides:new-blank", vec![num(960.0)]);
    let denied = invoke(
        &registry,
        replacement,
        "slides:export-images",
        vec![obj([
            ("format", string("png")),
            ("saveAsTarget", string(target.to_string_lossy())),
            ("saveRequestId", request),
            ("pngsBase64", IpcValue::Array(vec![string("AQID")])),
        ])],
    );
    assert_eq!(field(&denied, "ok"), &IpcValue::Bool(false));
    assert!(!target.exists());
}

#[test]
fn the_microphone_is_let_start_only_for_an_editor_with_a_deck() {
    let (_kit, registry, sender, _dir) = setup("slides-mic");
    assert_eq!(
        invoke(&registry, sender, "slides:microphone-access", vec![]),
        IpcValue::Bool(false)
    );
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    assert_eq!(
        invoke(&registry, sender, "slides:microphone-access", vec![]),
        IpcValue::Bool(true)
    );
    invoke(&registry, sender, "slides:microphone-release", vec![]);
}

#[test]
fn attachments_are_accepted_by_extension_and_size() {
    let (_kit, registry, sender, dir) = setup("slides-files");
    let notes = dir.join("notes.txt");
    std::fs::write(&notes, "abcdef").unwrap();
    let bad = dir.join("tool.exe");
    std::fs::write(&bad, "x").unwrap();
    let answer = invoke(
        &registry,
        sender,
        "slides:files-add",
        vec![IpcValue::Array(vec![
            string(notes.to_string_lossy()),
            string(bad.to_string_lossy()),
        ])],
    );
    let IpcValue::Array(accepted) = field(&answer, "accepted") else {
        panic!("accepted list")
    };
    assert_eq!(accepted.len(), 1);
    assert_eq!(field(&accepted[0], "ext"), &string("txt"));
    let IpcValue::Array(rejected) = field(&answer, "rejected") else {
        panic!("rejected list")
    };
    assert_eq!(
        rejected,
        &vec![string("tool.exe: .exe files are not supported")]
    );
    // the text is read in slices of the characters asked
    let read = invoke(
        &registry,
        sender,
        "slides:files-read",
        vec![string(notes.to_string_lossy()), num(2.0), num(3.0)],
    );
    assert_eq!(field(&read, "text"), &string("cde"));
    assert_eq!(field(&read, "totalChars"), &num(6.0));
}

/// A template-selection hooks provider that records what it is called with
#[derive(Clone, Default)]
struct Hooks(Rc<RefCell<Vec<String>>>);

impl shell_contract::slides::SlidesTemplateSelectionHooks for Hooks {
    fn follow(&self, doc: &shell_contract::slides::SlidesDocument, from: &str, to: &str) {
        self.0.borrow_mut().push(format!(
            "follow {} {from:?} {to:?} at {:?}",
            doc.id, doc.path
        ));
    }

    fn inherit(
        &self,
        previous: Option<&shell_contract::slides::SlidesDocument>,
        replacement: &shell_contract::slides::SlidesDocument,
    ) {
        self.0.borrow_mut().push(format!(
            "inherit {:?} -> {} {:?}",
            previous.map(|p| p.id),
            replacement.id,
            replacement.path
        ));
    }

    fn moved(&self, _old_path: &str, _new_path: &str) {}
    fn prepare_moved_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedTemplateMove, String> {
        self.prepare_moved_many_owned(origin, destination, &[], cancel, pending, effects)
    }
    fn prepare_moved_many_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedTemplateMove, String> {
        let fixture = self.clone();
        let origin = origin.clone();
        let targets = participants
            .iter()
            .cloned()
            .chain(std::iter::once(destination.clone()))
            .collect::<Vec<_>>();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let effects = effects.clone();
        Ok(Box::new(move |old, new, done| {
            done(
                effects.with_owned_effect_many(&origin, &targets, &cancel, &pending, &mut || {
                    fixture.moved(&old, &new);
                    Ok(())
                }),
            )
        }))
    }
    fn prepare_moved_many_owned_extra(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedTemplateMove, String> {
        let fixture = self.clone();
        let origin = origin.clone();
        let targets = participants
            .iter()
            .cloned()
            .chain(std::iter::once(destination.clone()))
            .collect::<Vec<_>>();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let additional = additional.to_vec();
        let effects = effects.clone();
        Ok(Box::new(move |old, new, done| {
            done(effects.with_owned_effect_many_extra(
                &origin,
                &targets,
                &cancel,
                &pending,
                &additional.iter().collect::<Vec<_>>(),
                &mut || {
                    fixture.moved(&old, &new);
                    Ok(())
                },
            ))
        }))
    }
    fn prepare_moved_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedTemplateMove, String> {
        let fixture = self.clone();
        let receipt = receipt.clone();
        let receiver = receiver.clone();
        let targets = participants
            .iter()
            .cloned()
            .chain(std::iter::once(destination.clone()))
            .collect::<Vec<_>>();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let effects = effects.clone();
        let snapshot = snapshot.clone();
        Ok(Box::new(move |old, new, done| {
            done(effects.with_recovery_open_snapshot(
                &receipt,
                &snapshot,
                &receiver,
                &targets,
                &cancel,
                &pending,
                &mut || {
                    fixture.moved(&old, &new);
                    Ok(())
                },
            ))
        }))
    }
    fn prepare_moved_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedTemplateMove, String> {
        let fixture = self.clone();
        let binding = binding.clone();
        let participants = participants.to_vec();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let additional = additional.to_vec();
        let effects = effects.clone();
        Ok(Box::new(move |old, new, done| {
            done(effects.with_existing_document_save_effect_extra(
                &binding,
                &participants,
                &cancel,
                &pending,
                &additional.iter().collect::<Vec<_>>(),
                &mut || {
                    fixture.moved(&old, &new);
                    Ok(())
                },
            ))
        }))
    }
}

#[test]
fn saves_follow_the_template_selection_and_a_replaced_deck_inherits_it() {
    use shell_contract::slides::{SlidesDocument, SlidesDocuments, SlidesTemplateSelectionHooks};
    let (kit, registry, sender, dir) = setup("slides-hooks");
    let hooks = Rc::new(Hooks::default());
    registry
        .provided
        .add::<dyn SlidesTemplateSelectionHooks>(hooks.clone());
    let docs = registry.provided.all::<dyn SlidesDocuments>().remove(0);
    assert_eq!(docs.document(ViewId(1)), None);

    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let first = docs.document(ViewId(1)).expect("a session");
    assert_eq!(first.path, "");

    // the first save of an untitled deck follows from "" to the picked path
    let one = dir.join("one.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(one.clone()));
    invoke(&registry, sender, "slides:save", vec![]);
    let one_text = one.to_string_lossy().into_owned();
    assert_eq!(
        docs.document(ViewId(1)),
        Some(SlidesDocument {
            id: first.id,
            path: one_text.clone()
        })
    );
    // Save As follows from the old path to the new one, same session
    let two = dir.join("two.pptx");
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(two.clone()));
    invoke(
        &registry,
        sender,
        "slides:save-as",
        vec![string("Deck.pptx")],
    );
    let two_text = two.to_string_lossy().into_owned();
    assert_eq!(docs.document(ViewId(1)).unwrap().id, first.id);
    assert_eq!(docs.document(ViewId(1)).unwrap().path, two_text);

    // landing generated pages in replace mode makes a new session object
    let page = invoke(
        &registry,
        sender,
        "slides:local-page-generate",
        vec![obj([("specJson", string("{\"title\":\"x\"}"))])],
    );
    let marker = field(&page, "marker").clone();
    let landed = invoke(
        &registry,
        sender,
        "slides:land-generated-pages",
        vec![IpcValue::Array(vec![marker]), num(960.0)],
    );
    assert!(matches!(field(&landed, "slides"), IpcValue::Array(s) if s.len() == 1));
    let replaced = docs.document(ViewId(1)).unwrap();
    assert_ne!(replaced.id, first.id);
    assert_eq!(replaced.path, "");

    let calls = hooks.0.borrow().clone();
    assert_eq!(
        calls,
        vec![
            format!("follow {} \"\" {one_text:?} at {one_text:?}", first.id),
            format!(
                "follow {} {one_text:?} {two_text:?} at {one_text:?}",
                first.id
            ),
            format!("inherit Some({}) -> {} \"\"", first.id, replaced.id),
        ]
    );
}

#[test]
fn every_slides_channel_of_the_batch_has_a_handler() {
    let (_kit, registry, _sender, _dir) = setup("slides-all");
    let missing: Vec<&str> = shell_contract::CHANNELS
        .iter()
        .filter(|spec| spec.batch == shell_contract::Batch::D1 && spec.name.starts_with("slides:"))
        .filter(|spec| {
            spec.kinds
                .iter()
                .any(|k| *k != shell_contract::ipc::ChannelKind::Event)
        })
        .filter(|spec| !registry.handlers.contains_key(spec.name))
        .map(|spec| spec.name)
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}

#[test]
fn the_master_view_edits_its_part_and_holds_undo_until_it_closes() {
    let (_kit, registry, sender, _dir) = setup("slides-master");
    // no deck, no master
    assert_eq!(
        invoke(&registry, sender, "slides:master-enter", vec![num(960.0)]),
        IpcValue::Null
    );
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    // editing before the view is entered answers null
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:master-delete-element",
            vec![obj([("sourceId", string("m0"))])]
        ),
        IpcValue::Null
    );
    let entered = invoke(&registry, sender, "slides:master-enter", vec![num(960.0)]);
    let IpcValue::Array(items) = field(&entered, "items") else {
        panic!("items")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(field(&items[0], "kind"), &string("master"));

    let text = |value: &str| {
        obj([
            ("sourceId", string("m0")),
            (
                "paragraphs",
                IpcValue::Array(vec![obj([(
                    "runs",
                    IpcValue::Array(vec![obj([("text", string(value))])]),
                )])]),
            ),
        ])
    };
    let edited = invoke(
        &registry,
        sender,
        "slides:master-edit-text",
        vec![text("Hello")],
    );
    assert_eq!(
        field(&edited, "elements"),
        &IpcValue::Array(vec![obj([("id", string("m0")), ("text", string("Hello"))])])
    );
    // a master edit marks the deck changed
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(true)
    );
    // an element that is not there fails the op: null
    let missing = obj([("sourceId", string("nope"))]);
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:master-delete-element",
            vec![missing]
        ),
        IpcValue::Null
    );
    // a new element answers its id
    let added = invoke(
        &registry,
        sender,
        "slides:master-add-element",
        vec![obj([
            ("kind", string("textbox")),
            ("fitWidthPx", num(960.0)),
            ("xPx", num(0.0)),
            ("yPx", num(0.0)),
            ("wPx", num(10.0)),
            ("hPx", num(10.0)),
        ])],
    );
    assert_eq!(field(&added, "sourceId"), &string("m1"));
    // undo is held while the master is open
    assert_eq!(
        invoke(&registry, sender, "slides:undo", vec![]),
        IpcValue::Null
    );
    // closing answers every slide, and undo works again
    let closed = invoke(&registry, sender, "slides:master-close", vec![]);
    assert!(matches!(closed, IpcValue::Array(ref s) if s.len() == 1));
    assert!(matches!(
        invoke(&registry, sender, "slides:undo", vec![]),
        IpcValue::Array(_)
    ));
}

#[test]
fn a_comment_is_added_and_deleted_and_marks_the_deck_changed() {
    let (_kit, registry, sender, _dir) = setup("slides-comments");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let comment =
        |index: f64, text: &str| obj([("slideIndex", num(index)), ("text", string(text))]);
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:add-comment",
            vec![comment(7.0, "x")]
        ),
        IpcValue::Null
    );
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(false)
    );
    let comments = invoke(
        &registry,
        sender,
        "slides:add-comment",
        vec![comment(0.0, "hi")],
    );
    let IpcValue::Array(list) = &comments else {
        panic!("comments")
    };
    assert_eq!(list.len(), 1);
    assert_eq!(field(&list[0], "text"), &string("hi"));
    assert_eq!(
        invoke(&registry, sender, "slides:is-dirty", vec![]),
        IpcValue::Bool(true)
    );
    assert_eq!(
        invoke(&registry, sender, "slides:get-comments", vec![num(0.0)]),
        comments
    );
    // a comment that is not there fails the op: null; the real one goes
    let delete = |idx: f64| {
        obj([
            ("slideIndex", num(0.0)),
            ("authorId", string("a")),
            ("idx", num(idx)),
        ])
    };
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:delete-comment",
            vec![delete(5.0)]
        ),
        IpcValue::Null
    );
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:delete-comment",
            vec![delete(0.0)]
        ),
        IpcValue::Array(vec![])
    );
    let all = invoke(&registry, sender, "slides:get-all-comments", vec![]);
    assert_eq!(all, IpcValue::Array(vec![IpcValue::Array(vec![])]));
}

#[test]
fn insert_image_places_the_file_the_dialog_picked() {
    let (kit, registry, sender, dir) = setup("slides-image");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    // a cancelled dialog inserts nothing
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:insert-image",
            vec![num(0.0), num(960.0)]
        ),
        IpcValue::Null
    );
    let picture = dir.join("pic.png");
    std::fs::write(&picture, [1u8, 2, 3]).unwrap();
    kit.dialogs
        .opens
        .lock()
        .unwrap()
        .push_back(Some(vec![picture]));
    let inserted = invoke(
        &registry,
        sender,
        "slides:insert-image",
        vec![num(0.0), num(960.0)],
    );
    assert_eq!(field(&inserted, "sourceId"), &string("e1"));
    assert_eq!(
        field(field(&inserted, "slide"), "elements"),
        &IpcValue::Array(vec![obj([
            ("id", string("e1")),
            ("text", string("picture:png"))
        ])])
    );
    // the insert is one undo step
    assert!(matches!(
        invoke(&registry, sender, "slides:undo", vec![]),
        IpcValue::Array(_)
    ));
    // a slide that is not there inserts nothing and asks for no file
    assert_eq!(
        invoke(
            &registry,
            sender,
            "slides:insert-image",
            vec![num(9.0), num(960.0)]
        ),
        IpcValue::Null
    );
}

/// every file under `dir`
fn files_under(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn the_recovery_pass_copies_a_changed_deck_once_and_a_save_removes_the_copy() {
    let (kit, registry, sender, dir) = setup("slides-recovery");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    // a clean deck has no copy
    super::recovery::run_recovery_pass(false);
    assert!(files_under(&dir.join("userdata")).is_empty());

    add_text(&registry, sender, "unsaved");
    super::recovery::run_recovery_pass(false);
    let copies: Vec<PathBuf> = files_under(&dir.join("userdata"))
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".pptx"))
        .collect();
    assert_eq!(copies.len(), 1, "one copy of the untitled deck");
    assert!(std::fs::read(&copies[0]).unwrap().starts_with(b"FAKEPPTX"));
    let written = std::fs::metadata(&copies[0]).unwrap().modified().unwrap();
    // nothing changed since: the copy is not taken again
    super::recovery::run_recovery_pass(false);
    assert_eq!(
        std::fs::metadata(&copies[0]).unwrap().modified().unwrap(),
        written
    );

    // a save makes the deck's copies go
    kit.dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(dir.join("saved.pptx")));
    invoke(&registry, sender, "slides:save", vec![]);
    assert!(
        files_under(&dir.join("userdata"))
            .iter()
            .all(|p| !p.to_string_lossy().ends_with(".pptx")),
        "the recovery copy is deleted once the deck is saved"
    );
}

#[test]
fn recovery_identity_tracks_the_live_untitled_deck() {
    use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;

    let (_kit, registry, sender, _dir) = setup("slides-recovery-identity");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let views = super::views().unwrap();
    let first = views.recovery_identity(sender.view).unwrap();
    assert!(matches!(first, RecoveryIdentity::Untitled(_)));
    assert_eq!(views.recovery_identity(sender.view), Some(first));
}

#[test]
fn selected_recovery_decision_opens_the_selected_deck() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use main_core::packages::electron_utils::recovery_store::RecoveryWriteOptions;

    let (kit, registry, sender, dir) = setup("slides-selected-recovery");
    let original = dir.join("original.pptx");
    std::fs::write(
        &original,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let path = original.to_string_lossy().into_owned();
    let views = super::views().unwrap();
    let identity = super::recovery::file_recovery_id(&path);
    views.0.with_recovery_store(|store| {
        store
            .write_copy(
                &identity,
                b"FAKEPPTX 10000000 5143500 1\nppt/slides/slide1.xml 0 \n",
                &RecoveryWriteOptions::default(),
            )
            .unwrap();
    });
    let synthetic =
        kit.host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>();
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let owner = synthetic
        .capture_invocation_root(
            sender.view,
            &generation,
            &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
        )
        .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: owner.clone(),
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        sender.view,
        generation,
        reservation,
        browser_owner,
    )
    .unwrap();
    let finished = Rc::new(RefCell::new(None));
    let outcome = finished.clone();
    views.open_file_with_decision(
        &sender.view,
        &RecoveryOpenFileRequest {
            file_path: path,
            decision: RecoveryOpenDecision::Restore,
        },
        &target,
        Box::new(move |result| *outcome.borrow_mut() = Some(result)),
    );
    assert!(finished.borrow_mut().take().unwrap().is_ok());
    target.retain_native_registration(owner.clone()).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let cx = shell_contract::ipc::CallContext {
        sender,
        request: None,
        cancel: shell_contract::runtime::CancelToken::default(),
        native: Some(owner.context.clone()),
    };
    registry
        .handlers
        .get("slides:consume-pending-open")
        .unwrap()
        .1
        .call(
            &cx,
            vec![num(960.0)],
            shell_contract::ipc::Responder::new(move |result| {
                let _ = send.send(result);
            }),
        );
    let opened = receive.try_recv().unwrap().unwrap();
    assert_eq!(field(field(&opened, "size"), "cx"), &num(10000000.0));
    let token = match field(&opened, "recoveryToken") {
        IpcValue::String(token) => token.clone(),
        other => panic!("missing recovery token: {other:?}"),
    };
    let receipt = target.committed_seed().unwrap();
    assert!(receipt.settlement().is_none());
    let failed = crate::testkit::start_with_owner(
        &registry,
        sender,
        "slides:recovery-initialized",
        vec![string("stale"), IpcValue::Bool(true)],
        &owner,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(failed.try_recv().unwrap().is_err());
    assert!(receipt.settlement().is_none());
    let acknowledged = crate::testkit::start_with_owner(
        &registry,
        sender,
        "slides:recovery-initialized",
        vec![string(token), IpcValue::Bool(true)],
        &owner,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(acknowledged.try_recv().unwrap().is_ok());
    assert!(receipt.settlement().unwrap().is_ok());
}

#[test]
fn recovery_waits_for_worker_and_rejects_a_replaced_target() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    let (kit, _registry, sender, dir) = setup("slides-recovery-pending-replaced");
    let file = dir.join("original.pptx");
    std::fs::write(
        &file,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let effects = kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let owner = effects.capture_owner(sender.view, &generation).unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: owner,
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        sender.view,
        generation,
        reservation,
        browser_owner,
    )
    .unwrap();
    let finished = Rc::new(RefCell::new(None));
    let outcome = finished.clone();
    kit.hold_workers();
    super::views().unwrap().open_file_with_decision(
        &sender.view,
        &RecoveryOpenFileRequest {
            file_path: file.to_string_lossy().into_owned(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| *outcome.borrow_mut() = Some(result)),
    );
    assert!(finished.borrow().is_none());
    kit.view(sender.view.0, Surface::Slides);
    assert!(finished.borrow_mut().take().unwrap().is_err());
    kit.release_workers();
    assert!(finished.borrow().is_none());
    assert!(!target.is_seed_committed());
}

#[test]
fn recovery_preparation_rejects_registration_changed_during_worker() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    let (kit, _registry, sender, dir) = setup("slides-recovery-registration-race");
    let file = dir.join("original.pptx");
    std::fs::write(
        &file,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let effects = kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let owner = effects.capture_owner(sender.view, &generation).unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: owner.clone(),
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        sender.view,
        generation,
        reservation,
        browser_owner,
    )
    .unwrap();
    let finished = Rc::new(RefCell::new(None));
    let outcome = finished.clone();
    kit.hold_workers();
    super::views().unwrap().open_file_with_decision(
        &sender.view,
        &RecoveryOpenFileRequest {
            file_path: file.to_string_lossy().into_owned(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| *outcome.borrow_mut() = Some(result)),
    );
    assert!(finished.borrow().is_none());
    target.retain_native_registration(owner).unwrap();
    kit.release_workers();
    assert!(finished.borrow_mut().take().unwrap().is_err());
    assert!(!target.is_seed_committed());
}

#[test]
fn invalid_recovered_deck_is_marked_later_before_open_error() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryCopyState, RecoveryWriteOptions,
    };
    let (kit, registry, sender, dir) = setup("slides-recovery-invalid-deck");
    let file = dir.join("original.pptx");
    std::fs::write(
        &file,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let path = file.to_string_lossy().into_owned();
    let views = super::views().unwrap();
    let identity = super::recovery::file_recovery_id(&path);
    views.0.with_recovery_store(|store| {
        store
            .write_copy(&identity, b"invalid deck", &RecoveryWriteOptions::default())
            .unwrap();
    });
    let synthetic =
        kit.host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>();
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let owner = synthetic
        .capture_invocation_root(
            sender.view,
            &generation,
            &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
        )
        .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: owner.clone(),
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        sender.view,
        generation,
        reservation,
        browser_owner,
    )
    .unwrap();
    let completed = Rc::new(RefCell::new(None));
    let completion = completed.clone();
    views.open_file_with_decision(
        &sender.view,
        &RecoveryOpenFileRequest {
            file_path: path,
            decision: RecoveryOpenDecision::Restore,
        },
        &target,
        Box::new(move |result| *completion.borrow_mut() = Some(result)),
    );
    assert!(completed.borrow_mut().take().unwrap().is_ok());
    target.retain_native_registration(owner.clone()).unwrap();
    let receipt = target.committed_seed().unwrap();
    kit.hold_workers();
    let response = crate::testkit::start_with_owner(
        &registry,
        sender,
        "slides:consume-pending-open",
        vec![num(960.0)],
        &owner,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(response.try_recv().is_err());
    assert!(receipt.settlement().is_none());
    assert_eq!(
        views
            .0
            .with_recovery_store(|store| store.copy_info(&identity).unwrap().unwrap().state),
        RecoveryCopyState::Undecided
    );
    kit.release_workers();
    assert_eq!(
        response.try_recv().unwrap().unwrap_err().message,
        "not a pptx"
    );
    assert!(receipt.settlement().unwrap().is_err());
    assert_eq!(
        views
            .0
            .with_recovery_store(|store| store.copy_info(&identity).unwrap().unwrap().state),
        RecoveryCopyState::Later
    );
}

#[test]
fn replaced_recovery_target_cannot_mark_a_copy_later() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryCopyState, RecoveryWriteOptions,
    };
    let (kit, registry, sender, dir) = setup("slides-recovery-invalid-replaced");
    let file = dir.join("original.pptx");
    std::fs::write(
        &file,
        b"FAKEPPTX 9144000 5143500 1\nppt/slides/slide1.xml 0 \n",
    )
    .unwrap();
    let path = file.to_string_lossy().into_owned();
    let views = super::views().unwrap();
    let identity = super::recovery::file_recovery_id(&path);
    views.0.with_recovery_store(|store| {
        store
            .write_copy(&identity, b"invalid deck", &RecoveryWriteOptions::default())
            .unwrap();
    });
    let synthetic =
        kit.host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>();
    let generation = kit.services.views.renderer_generation(sender.view).unwrap();
    let owner = synthetic
        .capture_invocation_root(
            sender.view,
            &generation,
            &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
        )
        .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: owner.clone(),
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        sender.view,
        generation,
        reservation,
        browser_owner,
    )
    .unwrap();
    let completed = Rc::new(RefCell::new(None));
    let completion = completed.clone();
    views.open_file_with_decision(
        &sender.view,
        &RecoveryOpenFileRequest {
            file_path: path,
            decision: RecoveryOpenDecision::Restore,
        },
        &target,
        Box::new(move |result| *completion.borrow_mut() = Some(result)),
    );
    assert!(completed.borrow_mut().take().unwrap().is_ok());
    target.retain_native_registration(owner.clone()).unwrap();
    let receipt = target.committed_seed().unwrap();
    kit.hold_workers();
    let response = crate::testkit::start_with_owner(
        &registry,
        sender,
        "slides:consume-pending-open",
        vec![num(960.0)],
        &owner,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(response.try_recv().is_err());
    kit.view(sender.view.0, Surface::Slides);
    kit.release_workers();
    assert_eq!(
        response.try_recv().unwrap().unwrap_err().message,
        "not a pptx"
    );
    assert!(receipt.settlement().unwrap().is_err());
    assert_eq!(
        views
            .0
            .with_recovery_store(|store| store.copy_info(&identity).unwrap().unwrap().state),
        RecoveryCopyState::Undecided
    );
}

#[test]
fn mcp_transaction_applies_under_the_attached_owner_and_pushes_the_render_to_the_tab() {
    let (kit, registry, sender, _) = setup("slides-mcp-transaction");
    invoke(&registry, sender, "slides:new-blank", vec![num(960.0)]);
    let slides = super::views().unwrap();
    let before = slides.deck_model(sender.view).unwrap();
    kit.take_events();
    let request = crate::value::to_json(&obj([(
        "ops",
        IpcValue::Array(vec![obj([
            ("op", string("addElement")),
            ("target", obj([("slide", num(0.0))])),
        ])]),
    )]))
    .unwrap();
    let answer = slides.apply_mcp_transaction(sender.view, &request).unwrap();
    assert_eq!(
        answer.get("applied").as_deref(),
        Some(&jscompat::JsonValue::Bool(true))
    );
    assert!(
        answer.get("slides").is_none(),
        "the render tree stays out of the answer"
    );
    assert_ne!(slides.deck_model(sender.view).unwrap(), before);
    let pushed: Vec<_> = kit
        .take_events()
        .into_iter()
        .filter(|(_, channel, _)| *channel == "slides:deck-changed")
        .map(|(target, _, _)| target)
        .collect();
    assert_eq!(pushed, vec![Target::View(sender.view)]);
    kit.destroy(sender.view);
    assert!(slides.apply_mcp_transaction(sender.view, &request).is_err());
}
