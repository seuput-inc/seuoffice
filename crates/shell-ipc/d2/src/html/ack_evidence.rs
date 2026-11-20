//! Drivers of the renderer acknowledgements the text editors wait for
//! (`<app>:recovery-applied`, `<app>:saved-input-applied`). HTML and Markdown
//! run the same `TextDocs` and `TextRecovery`, so one driver per channel
//! serves both apps' evidence tests; an [`App`] names the app's channels,
//! fakes and file layout.
//!
//! Both acknowledgements carry one `result` object. The native wait keys on:
//! - recovery: `result.token` (the `recoveryToken` of the seed the renderer
//!   consumed) and `result.ok` / `result.error`, from the renderer registered
//!   for that seed;
//! - saved input: `result.token` (the `savedInputToken` of the text the
//!   renderer was handed) and `result.outcome` / `result.error`, from the
//!   renderer the saved document is registered to.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::json;
use shell_contract::ipc::{NativeContext, NativeRequestOwner, Reply, Sender};
use shell_contract::runtime::CancelToken;
use shell_contract::{IpcValue, Surface, ViewId};

use crate::kit::{IntoIpc, field, obj};
use crate::testing::Rt;

/// What differs between the apps that share the text-document state
pub(crate) struct App {
    /// `html` or `markdown`: the channel prefix and the recovery kind
    pub name: &'static str,
    pub surface: Surface,
    /// a surface that is not this app's and not Docs, for the dispatcher
    pub sibling: Surface,
    pub hosts: fn(&mut shell_contract::Hosts),
    pub entries: fn(
        &shell_contract::runtime::Services,
        &Rc<shell_contract::provided::Provided>,
    ) -> Result<Vec<crate::kit::Entry>, shell_contract::ipc::RegisterError>,
    /// a view the editors track, on a path or untitled
    pub track: fn(&Rt, u32, Option<&str>) -> Sender,
    /// a file in the folder the app opens documents from
    pub file: fn(&Rt, &str, &str) -> String,
}

fn rt(app: &App) -> Rt {
    let mut rt = Rt::new(app.hosts);
    rt.register(app.entries).unwrap();
    rt
}

fn channel(app: &App, name: &str) -> String {
    format!("{}:{name}", app.name)
}

/// The scenarios every renderer acknowledgement contract requires
const SCENARIOS: [&str; 7] = [
    "successful-ack",
    "failed-apply",
    "missing-receipt",
    "foreign-receipt",
    "foreign-renderer",
    "duplicate-ack",
    "unauthorized-surface",
];

/// The receipt of a driver that returned: every assertion of it held
pub(crate) fn record(app: &App, name: &str, observed: &serde_json::Value) {
    shell_contract::native_evidence::record(&channel(app, name), &SCENARIOS, &observed.to_string());
}

fn s(text: &str) -> IpcValue {
    text.into_ipc()
}

/// An invoke the way the native shell delivers it: with the caller's own native context
fn call(
    rt: &mut Rt,
    channel: &str,
    from: Sender,
    args: Vec<IpcValue>,
    native: &NativeContext,
) -> Reply {
    rt.invoke_native(channel, from, args, native.clone(), CancelToken::default())
        .take()
        .unwrap_or_else(|| panic!("{channel} did not answer"))
}

fn refusal(reply: Reply) -> String {
    reply.expect_err("the acknowledgement is refused").message
}

/// A recovery acknowledgement from `from`'s renderer under a native origin
/// that no seed of that view was handed to
fn ack_under_a_fresh_origin(rt: &mut Rt, app: &App, from: Sender, result: IpcValue) -> Reply {
    let native = rt.native_root(from.view).context;
    call(
        rt,
        &channel(app, "recovery-applied"),
        from,
        vec![result],
        &native,
    )
}

/// The call a renderer from a view of `surface` makes, through the real dispatcher
fn dispatched_from(app: &App, surface: Surface, channel: &str, args: Vec<IpcValue>) -> String {
    let rt = Rt::new(app.hosts);
    let from = rt.view(90, surface);
    let mut table = shell_core::table::Table::default();
    let provided = shell_contract::ipc::Registry::provided(&table);
    for entry in (app.entries)(&rt.services, &provided).unwrap() {
        shell_contract::ipc::Registry::register(
            &mut table,
            shell_contract::Batch::D2,
            entry.channel,
            entry.kind,
            entry.handler,
        )
        .unwrap();
    }
    let dispatcher = shell_core::dispatch::Dispatcher::new(table);
    let (send, receive) = std::sync::mpsc::channel();
    dispatcher.dispatch(
        from,
        shell_contract::ipc::ChannelKind::Invoke,
        Some(shell_contract::RequestId(1)),
        None,
        channel,
        args,
        Box::new(move |result| send.send(result).unwrap()),
    );
    let message = receive.try_recv().unwrap().unwrap_err().message;
    assert!(
        message.contains(&format!("not open to {surface:?}")),
        "{channel} from {surface:?}: {message}"
    );
    message
}

/// `unauthorized-surface`: views of Docs and of the sibling text app are
/// refused before any handler runs
fn refused_on_other_surfaces(app: &App, name: &str, args: Vec<IpcValue>) -> serde_json::Value {
    let channel = channel(app, name);
    json!({
        "docs": dispatched_from(app, Surface::Docs, &channel, args.clone()),
        "sibling": dispatched_from(app, app.sibling, &channel, args),
    })
}

// ---- recovery ----

/// A recovery copy the native shell restored into a fresh view: the renderer
/// has consumed the seed and owes the `recovery-applied` acknowledgement
struct Seed {
    sender: Sender,
    receiver: NativeRequestOwner,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    token: IpcValue,
    text: IpcValue,
    _authority: shell_contract::ipc::RecoveryOpenTarget,
}

impl Seed {
    fn ack(&self, rt: &mut Rt, app: &App, result: IpcValue) -> Reply {
        call(
            rt,
            &channel(app, "recovery-applied"),
            self.sender,
            vec![result],
            &self.receiver.context,
        )
    }

    fn exact(&self, ok: bool, error: Option<&str>) -> IpcValue {
        match error {
            Some(error) => obj([
                ("token", self.token.clone()),
                ("ok", ok.into_ipc()),
                ("error", s(error)),
            ]),
            None => obj([("token", self.token.clone()), ("ok", ok.into_ipc())]),
        }
    }
}

/// Restores `name`'s unsaved copy into view `first + 1` (the copy is written
/// by view `first`, the restore is requested through view `first + 2`) and
/// has the renderer of the target consume the seed
fn seed(rt: &mut Rt, app: &App, first: u32, name: &str) -> Seed {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    let path = (app.file)(rt, name, "saved");
    let writer = (app.track)(rt, first, Some(&path));
    rt.send(
        &channel(app, "dirty-changed"),
        writer,
        vec![true.into_ipc()],
    );
    assert_eq!(
        rt.invoke_now(
            &channel(app, "recovery-write"),
            writer,
            vec![s("recovered")]
        ),
        Ok(s("written"))
    );
    let target = (app.track)(rt, first + 1, None);
    let issuer = (app.track)(rt, first + 2, None);
    let effects = rt
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .clone();
    let created = rt.services.views.renderer_generation(target.view).unwrap();
    let (_closure_issuer, closure, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(target.view, created.clone());
    let source = effects
        .capture_owner(
            issuer.view,
            &rt.services.views.renderer_generation(issuer.view).unwrap(),
        )
        .unwrap();
    let cancel = CancelToken::default();
    let authority = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source,
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: cancel.clone(),
        },
        target.view,
        created,
        closure,
        browser,
    )
    .unwrap();
    let contract = rt
        .provided()
        .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
        .into_iter()
        .find(|editor| editor.kind() == app.name)
        .unwrap();
    let opened: Rc<RefCell<Option<Result<(), String>>>> = Rc::default();
    let done = Rc::clone(&opened);
    contract.open_file_with_decision(
        &target.view,
        &RecoveryOpenFileRequest {
            file_path: path,
            decision: RecoveryOpenDecision::Restore,
        },
        &authority,
        Box::new(move |result| *done.borrow_mut() = Some(result.map_err(|error| error.0))),
    );
    rt.drain();
    assert_eq!(*opened.borrow(), Some(Ok(())));
    cancel.cancel();
    let receiver = rt.native_root(target.view);
    authority
        .retain_native_registration(receiver.clone())
        .unwrap();
    let receipt = authority.committed_seed().unwrap();
    let handed = call(
        rt,
        &channel(app, "recovery-open"),
        target,
        vec![],
        &receiver.context,
    )
    .unwrap();
    assert!(receipt.settlement().is_none(), "the seed waits for its ack");
    Seed {
        sender: target,
        receiver,
        receipt,
        token: field(&handed, "recoveryToken").clone(),
        text: field(&handed, "text").clone(),
        _authority: authority,
    }
}

/// Drives `<app>:recovery-applied` through every required scenario and
/// returns what it observed
pub(crate) fn recovery_acks(app: &App) -> serde_json::Value {
    let mut rt = rt(app);
    let settled = IpcValue::Undefined;

    // successful-ack and failed-apply: the exact acknowledgement settles the wait
    let applied = seed(&mut rt, app, 10, "applied.copy");
    assert_eq!(applied.text, s("recovered"));
    assert_eq!(
        applied.ack(&mut rt, app, applied.exact(true, None)),
        Ok(settled.clone())
    );
    assert_eq!(applied.receipt.settlement(), Some(Ok(())));
    let failed = seed(&mut rt, app, 20, "failed.copy");
    assert_eq!(
        failed.ack(&mut rt, app, failed.exact(false, Some("apply failed"))),
        Ok(settled.clone())
    );
    assert_eq!(
        failed.receipt.settlement(),
        Some(Err("apply failed".into()))
    );

    // the rest are refused while this one wait stays open
    let pending = seed(&mut rt, app, 30, "pending.copy");
    let other = seed(&mut rt, app, 40, "other.copy");

    // missing-receipt: no token, no verdict, or nothing pending in that view
    let untokened = refusal(pending.ack(&mut rt, app, obj([("ok", true.into_ipc())])));
    assert!(untokened.contains("no token"), "{untokened}");
    let unjudged = refusal(pending.ack(&mut rt, app, obj([("token", pending.token.clone())])));
    assert!(unjudged.contains("no result"), "{unjudged}");
    let idle = (app.track)(&rt, 50, None);
    let nothing_pending = refusal(ack_under_a_fresh_origin(
        &mut rt,
        app,
        idle,
        obj([("token", pending.token.clone()), ("ok", true.into_ipc())]),
    ));
    assert!(nothing_pending.contains("unavailable"), "{nothing_pending}");
    assert!(pending.receipt.settlement().is_none());

    // foreign-receipt: a token of no wait, and the token of another document's wait
    let unknown = refusal(pending.ack(
        &mut rt,
        app,
        obj([("token", s("recovery:unknown")), ("ok", true.into_ipc())]),
    ));
    assert!(unknown.contains("does not match"), "{unknown}");
    let other_documents = refusal(pending.ack(&mut rt, app, other.exact(true, None)));
    assert!(
        other_documents.contains("does not match"),
        "{other_documents}"
    );
    let on_the_other_view = refusal(other.ack(&mut rt, app, pending.exact(true, None)));
    assert!(
        on_the_other_view.contains("does not match"),
        "{on_the_other_view}"
    );
    assert!(pending.receipt.settlement().is_none());
    assert!(other.receipt.settlement().is_none());

    // foreign-renderer: the exact token from a view that has no such seed, and
    // from the seed's own view under another renderer origin
    let from_other_view = refusal(ack_under_a_fresh_origin(
        &mut rt,
        app,
        idle,
        pending.exact(true, None),
    ));
    assert!(from_other_view.contains("unavailable"), "{from_other_view}");
    let from_other_origin = refusal(ack_under_a_fresh_origin(
        &mut rt,
        app,
        pending.sender,
        pending.exact(true, None),
    ));
    assert!(pending.receipt.settlement().is_none());
    assert!(other.receipt.settlement().is_none());

    // the refusals consumed nothing: the exact acknowledgements still settle both waits
    assert_eq!(
        pending.ack(&mut rt, app, pending.exact(true, None)),
        Ok(settled.clone())
    );
    assert_eq!(pending.receipt.settlement(), Some(Ok(())));
    assert_eq!(
        other.ack(&mut rt, app, other.exact(false, Some("other failed"))),
        Ok(settled)
    );
    assert_eq!(other.receipt.settlement(), Some(Err("other failed".into())));

    // duplicate-ack: a second acknowledgement changes neither settled wait
    let twice = refusal(applied.ack(&mut rt, app, applied.exact(false, Some("late"))));
    assert_eq!(applied.receipt.settlement(), Some(Ok(())));
    let twice_after_failure = refusal(failed.ack(&mut rt, app, failed.exact(true, None)));
    assert_eq!(
        failed.receipt.settlement(),
        Some(Err("apply failed".into()))
    );

    // unauthorized-surface
    let surfaces = refused_on_other_surfaces(
        app,
        "recovery-applied",
        vec![obj([("token", s("recovery:x")), ("ok", true.into_ipc())])],
    );

    json!({
        "successful-ack": {"answer": "undefined", "settlement": "ok"},
        "failed-apply": {"answer": "undefined", "settlement": "apply failed"},
        "missing-receipt": {
            "no token": untokened,
            "no verdict": unjudged,
            "nothing pending": nothing_pending,
        },
        "foreign-receipt": {
            "unknown token": unknown,
            "token of another document": other_documents,
            "other document's view": on_the_other_view,
        },
        "foreign-renderer": {
            "view without the seed": from_other_view,
            "other renderer origin": from_other_origin,
        },
        "duplicate-ack": {"after success": twice, "after failure": twice_after_failure},
        "unauthorized-surface": surfaces,
    })
}

// ---- saved input ----

/// What the native load of a saved document was told, in the order it was told
#[cfg(unix)]
type Outcomes = Rc<RefCell<Vec<Result<shell_contract::editors::SavedDocumentLoadOutcome, String>>>>;

#[cfg(unix)]
const SAVED_TEXT: &str = "# saved";

/// A saved document the native shell opened in a fresh view: the renderer has
/// been handed the retained text and owes the `saved-input-applied` acknowledgement
#[cfg(unix)]
struct Saved {
    sender: Sender,
    owner: NativeRequestOwner,
    token: IpcValue,
    outcomes: Outcomes,
    _issuer: shell_contract::ipc::NativeClosureIssuer,
    _registration: Rc<dyn shell_contract::editors::SavedActorRegistration>,
}

#[cfg(unix)]
impl Saved {
    fn ack_as(
        &self,
        rt: &mut Rt,
        app: &App,
        from: Sender,
        native: &NativeContext,
        result: IpcValue,
    ) -> Reply {
        call(
            rt,
            &channel(app, "saved-input-applied"),
            from,
            vec![result],
            native,
        )
    }

    fn ack(&self, rt: &mut Rt, app: &App, result: IpcValue) -> Reply {
        self.ack_as(rt, app, self.sender, &self.owner.context, result)
    }

    fn exact(&self, outcome: &str, error: Option<&str>) -> IpcValue {
        match error {
            Some(error) => obj([
                ("token", self.token.clone()),
                ("outcome", s(outcome)),
                ("error", s(error)),
            ]),
            None => obj([("token", self.token.clone()), ("outcome", s(outcome))]),
        }
    }
}

/// The bytes a committed save of `existing`, a document of `surface`, retained,
/// as the native shell hands them to the load
#[cfg(unix)]
pub(crate) fn saved_input(
    rt: &Rt,
    surface: Surface,
    existing: &NativeRequestOwner,
    path: &std::path::Path,
) -> (
    shell_contract::ipc::NativeSavedInput,
    shell_contract::ipc::NativeSavedPath,
) {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection, NativeSavedInput,
        NativeSavedPath,
    };
    use std::ffi::CString;
    let effects = rt
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer
        .issue_existing_document_save(
            surface,
            existing.view,
            NativeSaveSelection::ExactPath(path.to_owned()),
            existing.clone(),
            CancelToken::default(),
        )
        .unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let parent = std::fs::File::open(path.parent().unwrap()).unwrap();
    let directory = file_commit::PrivateDirectory::create(&parent).unwrap();
    let stage = directory
        .create_stage(0o600)
        .unwrap()
        .quarantine()
        .ok()
        .unwrap();
    let stage = stage
        .write_owned(SAVED_TEXT.as_bytes().to_vec())
        .ok()
        .unwrap();
    let name = CString::new(path.file_name().unwrap().to_str().unwrap()).unwrap();
    let stage = stage
        .prepare(&parent, &name, path, file_commit::PublicationMode::Replace)
        .ok()
        .unwrap();
    let (publication, snapshot) = effects
        .publish_native_staged_write(
            &selection,
            &[],
            &CancelToken::default(),
            &CancelToken::default(),
            &[],
            stage,
        )
        .ok()
        .unwrap();
    let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot).unwrap();
    (input, NativeSavedPath::Selected(publication))
}

/// Opens `name`'s saved output in the new view `id`: the editor registers its
/// barrier, the load registers the document, the renderer's first request
/// receives the retained text
#[cfg(unix)]
fn saved(rt: &mut Rt, app: &App, id: u32, name: &str) -> Saved {
    use shell_contract::hosts::synthetic::SyntheticOwnedEffects;
    use shell_contract::ipc::{NativeClosureIssuer, NativeSavedTarget};
    use std::sync::Arc;
    let path = (app.file)(rt, name, "old");
    let view = (app.track)(rt, id, None);
    let editor = shell_contract::editor_for(&rt.provided(), view.view).unwrap();
    let existing = editor
        .capture_document_owner(view.view, rt.native_root(view.view))
        .unwrap();
    let (input, saved_path) = saved_input(rt, app.surface, &existing, std::path::Path::new(&path));
    let generation = rt.services.views.renderer_generation(view.view).unwrap();
    let (issuer, reservation, browser) =
        NativeClosureIssuer::new_with_owner(view.view, generation.clone());
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let target = NativeSavedTarget::prepare(
        input,
        saved_path,
        app.surface,
        view.view,
        generation,
        reservation,
        browser,
        vec![],
        cancel.clone(),
        pending.clone(),
        vec![],
    )
    .unwrap();
    let mut prepared = editor.prepare_saved_document(target.clone()).unwrap();
    let effects = rt
        .services
        .hosts
        .get::<Arc<SyntheticOwnedEffects>>()
        .unwrap()
        .clone();
    effects
        .with_saved_target_creation(&target, &[], &cancel, &pending, &[], &mut |transition| {
            prepared
                .with_validated(transition, &mut || Ok(()))
                .map_err(|error| error.message)
        })
        .unwrap();
    let registration = prepared.finish();
    let owner = rt.native_root(view.view);
    let mut document = None;
    effects
        .with_saved_target_registration(&target, &owner, &[], &cancel, &pending, &[], &mut |_| {
            document = Some(target.register_from_native_admitted(owner.clone())?);
            Ok(())
        })
        .unwrap();
    let outcomes: Outcomes = Rc::default();
    let seen = Rc::clone(&outcomes);
    editor.load_saved_document(
        document.unwrap(),
        Box::new(move |result| {
            seen.borrow_mut()
                .push(result.map_err(|error| error.message))
        }),
    );
    rt.drain();
    let handed = call(
        rt,
        &channel(app, "consume-pending"),
        view,
        vec![],
        &owner.context,
    )
    .unwrap();
    assert_eq!(field(&handed, "text"), &s(SAVED_TEXT));
    assert_eq!(field(&handed, "recovered"), &false.into_ipc());
    assert!(outcomes.borrow().is_empty(), "the load waits for its ack");
    Saved {
        sender: view,
        owner,
        token: field(&handed, "savedInputToken").clone(),
        outcomes,
        _issuer: issuer,
        _registration: registration,
    }
}

/// Drives `<app>:saved-input-applied` through every required scenario and
/// returns what it observed
#[cfg(unix)]
pub(crate) fn saved_input_acks(app: &App) -> serde_json::Value {
    use shell_contract::editors::SavedDocumentLoadOutcome::{Applied, RecoveryApplied};
    let mut rt = rt(app);
    let settled = IpcValue::Undefined;

    // successful-ack: `applied` and `recovered` both settle the load as a success
    let applied = saved(&mut rt, app, 10, "applied.copy");
    assert_eq!(
        applied.ack(&mut rt, app, applied.exact("applied", None)),
        Ok(settled.clone())
    );
    assert_eq!(*applied.outcomes.borrow(), vec![Ok(Applied)]);
    let recovered = saved(&mut rt, app, 20, "recovered.copy");
    assert_eq!(
        recovered.ack(&mut rt, app, recovered.exact("recovered", None)),
        Ok(settled.clone())
    );
    assert_eq!(*recovered.outcomes.borrow(), vec![Ok(RecoveryApplied)]);

    // failed-apply: the load fails with the renderer's error
    let failed = saved(&mut rt, app, 30, "failed.copy");
    assert_eq!(
        failed.ack(&mut rt, app, failed.exact("failed", Some("apply failed"))),
        Ok(settled.clone())
    );
    assert_eq!(*failed.outcomes.borrow(), vec![Err("apply failed".into())]);

    // the rest are refused while these two waits stay open
    let pending = saved(&mut rt, app, 40, "pending.copy");
    let other = saved(&mut rt, app, 50, "other.copy");
    let idle = (app.track)(&rt, 60, None);
    let idle_native = rt.native_root(idle.view).context;
    let invalid = "The saved document receipt is invalid";

    // missing-receipt: no token, no result, or no saved wait in that view
    let untokened = refusal(pending.ack(&mut rt, app, obj([("outcome", s("applied"))])));
    let resultless = refusal(call(
        &mut rt,
        &channel(app, "saved-input-applied"),
        pending.sender,
        vec![],
        &pending.owner.context,
    ));
    let nothing_pending = refusal(pending.ack_as(
        &mut rt,
        app,
        idle,
        &idle_native,
        pending.exact("applied", None),
    ));
    for message in [&untokened, &resultless, &nothing_pending] {
        assert_eq!(message, invalid);
    }

    // foreign-receipt: a token of no wait, and the token of another document's wait
    let unknown = refusal(pending.ack(
        &mut rt,
        app,
        obj([("token", s("unknown")), ("outcome", s("applied"))]),
    ));
    let other_documents = refusal(pending.ack(&mut rt, app, other.exact("applied", None)));
    for message in [&unknown, &other_documents] {
        assert_eq!(message, invalid);
    }

    // foreign-renderer: the exact token from another document's view, and from
    // the document's own view under another renderer origin
    let from_other_view = refusal(pending.ack_as(
        &mut rt,
        app,
        other.sender,
        &other.owner.context,
        pending.exact("applied", None),
    ));
    assert_eq!(from_other_view, invalid);
    let fresh_origin = rt.native_root(pending.sender.view).context;
    let from_other_origin = refusal(pending.ack_as(
        &mut rt,
        app,
        pending.sender,
        &fresh_origin,
        pending.exact("applied", None),
    ));
    assert!(pending.outcomes.borrow().is_empty());
    assert!(other.outcomes.borrow().is_empty());

    // the refusals consumed nothing: the exact acknowledgements still settle both waits
    assert_eq!(
        pending.ack(&mut rt, app, pending.exact("applied", None)),
        Ok(settled.clone())
    );
    assert_eq!(*pending.outcomes.borrow(), vec![Ok(Applied)]);
    assert_eq!(
        other.ack(&mut rt, app, other.exact("failed", Some("other failed"))),
        Ok(settled)
    );
    assert_eq!(*other.outcomes.borrow(), vec![Err("other failed".into())]);

    // duplicate-ack: a second acknowledgement is refused and reports nothing again
    let twice = refusal(applied.ack(&mut rt, app, applied.exact("failed", Some("late"))));
    assert_eq!(*applied.outcomes.borrow(), vec![Ok(Applied)]);
    let twice_after_failure = refusal(failed.ack(&mut rt, app, failed.exact("applied", None)));
    assert_eq!(*failed.outcomes.borrow(), vec![Err("apply failed".into())]);
    for message in [&twice, &twice_after_failure] {
        assert_eq!(message, invalid);
    }

    // unauthorized-surface
    let surfaces = refused_on_other_surfaces(
        app,
        "saved-input-applied",
        vec![obj([("token", s("t")), ("outcome", s("applied"))])],
    );

    json!({
        "successful-ack": {
            "answer": "undefined",
            "applied": "Applied",
            "recovered": "RecoveryApplied",
        },
        "failed-apply": {"answer": "undefined", "load": "apply failed"},
        "missing-receipt": {
            "no token": untokened,
            "no result": resultless,
            "nothing pending": nothing_pending,
        },
        "foreign-receipt": {
            "unknown token": unknown,
            "token of another document": other_documents,
        },
        "foreign-renderer": {
            "other document's view": from_other_view,
            "other renderer origin": from_other_origin,
        },
        "duplicate-ack": {"after success": twice, "after failure": twice_after_failure},
        "unauthorized-surface": surfaces,
    })
}
