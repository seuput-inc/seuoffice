//! What a renderer of the sheets editor can observe: session flows, the
//! save-edits transfer, dialogs, picking, the close round trip, recovery
//! copies and a view's end. The engine is [`super::fakes::FakeEngine`].

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;
use shell_contract::editors::{SaveMode, editor_for};
use shell_contract::hosts::Hosts;
use shell_contract::ipc::{Sender, Target};
use shell_contract::{Batch, IpcError, IpcValue, Surface, WindowId};

use super::fakes::{FakeEngine, FakeShell};
use super::{insert_fakes, routes, views};
use crate::testkit::{Kit, TestRegistry, start, temp_dir};
use crate::value::{field, num, obj, string};

struct World {
    kit: Kit,
    registry: TestRegistry,
    dir: PathBuf,
}

fn world(name: &str, case: Value) -> World {
    let dir = temp_dir(name);
    let kit = Kit::new(&dir, move |hosts: &mut Hosts| insert_fakes(hosts, &case));
    let mut registry = TestRegistry::default();
    for route in routes(&kit.services, &registry.provided).expect("sheets registers") {
        use shell_contract::ipc::Registry;
        registry
            .register(Batch::D1, route.channel, route.kind, route.handler)
            .expect("the channel is sheets'");
    }
    World { kit, registry, dir }
}

impl World {
    fn tab(&self, id: u32) -> Sender {
        let sender = self.kit.view(id, Surface::Sheets);
        views().unwrap().register_tab(sender.view);
        sender
    }

    fn call(
        &self,
        sender: Sender,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<IpcValue, IpcError> {
        self.start(sender, channel, args).try_recv()
            .unwrap_or_else(|_| panic!("{channel} did not answer"))
    }

    fn start(
        &self, sender: Sender, channel: &str, args: Vec<IpcValue>,
    ) -> std::sync::mpsc::Receiver<Result<IpcValue, IpcError>> {
        let context = shell_contract::ipc::CallContext {
            sender, request: None, cancel: shell_contract::runtime::CancelToken::default(),
            native: self.kit.services.views.renderer_generation(sender.view)
                .map(|_| self.native_owner(sender).context),
        };
        let (_, handler) = self.registry.handlers.get(channel).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        handler.call(&context, args, shell_contract::ipc::Responder::new(move |reply| {
            let _ = tx.send(reply);
        }));
        rx
    }

    /// Synthetic fixture authority, not evidence of native authentication.
    fn native_owner(&self, sender: Sender) -> shell_contract::ipc::NativeRequestOwner {
        let generation = self.kit.services.views.renderer_generation(sender.view).unwrap();
        self.kit.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().unwrap()
            .capture_owner(sender.view, &generation).unwrap()
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.dir.join(name);
        fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// the shell queues a file for the tab, the renderer pulls it
    fn open(&self, sender: Sender, path: &str) -> IpcValue {
        views().unwrap().queue_workbook(sender.view, path);
        self.call(sender, "workbook:select", vec![]).unwrap()
    }

    fn engine(&self) -> Arc<FakeEngine> {
        self.kit.host::<Arc<FakeEngine>>()
    }

    fn shell(&self) -> Arc<FakeShell> {
        self.kit.host::<Arc<FakeShell>>()
    }
}

use std::sync::Arc;

fn text(value: &IpcValue, key: &str) -> String {
    match field(value, key) {
        IpcValue::String(text) => text.clone(),
        other => panic!("{key} is {other:?}"),
    }
}

fn message(error: IpcError) -> String {
    error.message
}

fn save_request(session: &str, mode: &str) -> IpcValue {
    obj([("sessionId", string(session)), ("mode", string(mode))])
}

fn names(events: &[(Target, &'static str, Vec<IpcValue>)]) -> Vec<(Target, &'static str)> {
    events.iter().map(|(t, c, _)| (*t, *c)).collect()
}

#[test]
fn recovery_open_decision_queues_the_requested_workbook_for_that_view() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    let w = world("sheets-recovery-contract", Value::Null);
    let tab = w.tab(1);
    let contract = w.registry.provided.all::<dyn RecoveryEditorContract<shell_contract::ViewId>>()
        .into_iter().find(|editor| editor.kind() == "sheets").unwrap();
    contract.open_file_with_decision(&tab.view, &RecoveryOpenFileRequest {
        file_path: "restored.xlsx".into(),
        decision: RecoveryOpenDecision::Restore,
    });
    assert_eq!(w.call(tab, "sheets:has-queued-workbook", vec![]).unwrap(), IpcValue::Bool(true));
    assert_eq!(names(&w.kit.take_events()), vec![(Target::View(tab.view), "menu:action")]);
}

#[test]
fn recovery_flush_rejects_foreign_ack_and_reports_renderer_failure() {
    let w = world("sheets-flush-result", Value::Null);
    let owner = w.tab(1);
    let foreign = w.kit.view(2, Surface::Sheets);
    let answer = Rc::new(RefCell::new(None));
    let sink = answer.clone();
    views().unwrap().write_copies_now(Box::new(move |result| *sink.borrow_mut() = Some(result)));
    let events = w.kit.take_events();
    let nonce = events.iter().find(|(_, channel, _)| *channel == "workbook:recovery-write-now")
        .unwrap().2[0].clone();
    w.call(foreign, "workbook:recovery-write-now-done", vec![nonce.clone(), IpcValue::Null]).unwrap();
    assert!(answer.borrow().is_none());
    w.call(owner, "workbook:recovery-write-now-done", vec![nonce, string("disk failed")]).unwrap();
    assert_eq!(answer.borrow().as_ref().unwrap().as_ref().unwrap_err().0, "disk failed");
}

#[test]
fn save_to_path_waits_for_the_renderer_and_saved_file() {
    use jscompat::{JsObject, JsonValue};
    use main_core::shell::mcp::tools::document_tools::McpEditorCommandName;
    use main_core::shell::mcp::tools::sheets_tools::SheetsCommandName;
    use shell_contract::editors::Then;
    use shell_ipc_a::mcp::McpBridgeControl;

    #[derive(Default)]
    struct Bridge {
        pending: RefCell<Option<Then<Option<JsonValue>>>>,
        request: RefCell<Option<(i64, SheetsCommandName, JsonValue)>>,
    }
    impl McpBridgeControl for Bridge {
        fn docs_open_blank_tab(&self, _: Then<Option<JsonValue>>) { unreachable!() }
        fn docs_run_command(&self, _: i64, _: McpEditorCommandName, _: Option<JsonValue>, _: Then<Option<JsonValue>>) { unreachable!() }
        fn sheets_open_blank_tab(&self, _: Then<Option<JsonValue>>) { unreachable!() }
        fn sheets_run_command(&self, view: i64, command: SheetsCommandName, payload: Option<JsonValue>, then: Then<Option<JsonValue>>) {
            *self.request.borrow_mut() = Some((view, command, payload.unwrap()));
            *self.pending.borrow_mut() = Some(then);
        }
    }

    let w = world("sheets-save-to-path", Value::Null);
    let sender = w.tab(1);
    let bridge = Rc::new(Bridge::default());
    w.registry.provided.add::<dyn McpBridgeControl>(bridge.clone());
    let editor = editor_for(&w.registry.provided, sender.view).unwrap();
    let path = w.dir.join("saved.xlsx");
    let answer = Rc::new(RefCell::new(None));
    let sink = answer.clone();
    editor.save_to_path(sender.view, path.clone(), Box::new(move |result| *sink.borrow_mut() = Some(result)));
    assert!(answer.borrow().is_none());
    let request = bridge.request.borrow();
    let (view, command, payload) = request.as_ref().unwrap();
    assert_eq!(*view, i64::from(sender.view.0));
    assert_eq!(*command, SheetsCommandName::SaveSheet);
    assert_eq!(payload.get("overwrite").as_deref(), Some(&JsonValue::Bool(true)));
    drop(request);
    fs::write(&path, b"workbook").unwrap();
    let mut saved = JsObject::new();
    saved.set("ok", JsonValue::Bool(true));
    saved.set("path", JsonValue::String(path.to_string_lossy().into_owned()));
    w.kit.hold_workers();
    bridge.pending.borrow_mut().take().unwrap()(Ok(Some(JsonValue::Object(saved))));
    assert!(answer.borrow().is_none());
    w.kit.release_workers();
    assert!(matches!(&*answer.borrow(), Some(Ok(()))));
    let closed = Rc::new(RefCell::new(None));
    let sink = closed.clone();
    editor.save_to_path(sender.view, path.clone(), Box::new(move |result| *sink.borrow_mut() = Some(result)));
    let mut saved = JsObject::new();
    saved.set("ok", JsonValue::Bool(true));
    saved.set("path", JsonValue::String(path.to_string_lossy().into_owned()));
    w.kit.hold_workers();
    bridge.pending.borrow_mut().take().unwrap()(Ok(Some(JsonValue::Object(saved))));
    w.kit.destroy(sender.view);
    assert!(matches!(&*closed.borrow(), Some(Err(_))));
    w.kit.release_workers();
    assert!(matches!(&*closed.borrow(), Some(Err(_))));
}

#[test]
fn queued_workbook_nudges_only_active_view_until_consumed_or_restarted() {
    let w = world("sheets-nudge", Value::Null);
    let first = w.tab(1);
    let second = w.tab(2);
    let sheets = views().unwrap();
    sheets.queue_workbook(first.view, "first.xlsx");
    sheets.queue_workbook(second.view, "second.xlsx");
    sheets.set_active_view(Some(first.view));
    w.shell().set_clock(0.0);
    sheets.start_queued_workbook_nudge();
    assert_eq!(names(&w.kit.take_events()), vec![(Target::View(first.view), "menu:action")]);
    w.shell().fire_oldest(700);
    assert_eq!(names(&w.kit.take_events()), vec![(Target::View(first.view), "menu:action")]);

    sheets.set_active_view(Some(second.view));
    sheets.start_queued_workbook_nudge();
    assert_eq!(names(&w.kit.take_events()), vec![(Target::View(second.view), "menu:action")]);
    w.shell().fire_oldest(700);
    assert!(w.kit.take_events().is_empty());
    w.shell().fire_oldest(700);
    assert_eq!(names(&w.kit.take_events()), vec![(Target::View(second.view), "menu:action")]);
    let _ = w.call(second, "workbook:select", vec![]);
    w.kit.take_events();
    w.shell().fire_oldest(700);
    assert!(w.kit.take_events().is_empty());
    sheets.queue_workbook(second.view, "second.xlsx");
    sheets.start_queued_workbook_nudge();
    w.kit.take_events();


    w.shell().set_clock(30_001.0);
    w.shell().fire_oldest(700);
    assert!(w.kit.take_events().is_empty());
    sheets.start_queued_workbook_nudge();
    w.kit.take_events();
    w.kit.destroy(second.view);
    w.shell().fire_oldest(700);
    assert!(w.kit.take_events().is_empty());
}
fn queue_transition(prepared: super::editors::PreparedWorkbookQueue, reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<super::editors::WorkbookQueueReceipt, String> {
    reservation.with_initial_owner(owner, |_| Ok(()))?;
    reservation.prepare_replacement(owner)?.commit(|transition, successor| {
        Ok(prepared.validate_in_transition(transition, successor)?.commit_in_transition())
    }).map(|(_, _, receipt)| receipt)
}

fn blank_transition(prepared: super::editors::PreparedBlankSeed, reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<super::editors::BlankSeedReceipt, String> {
    reservation.with_initial_owner(owner, |_| Ok(()))?;
    reservation.prepare_replacement(owner)?.commit(|transition, successor| {
        Ok(prepared.validate_in_transition(transition, successor)?.commit_in_transition())
    }).map(|(_, _, receipt)| receipt)
}

fn queue_fixture(sheets: &super::editors::SheetsViews, view: shell_contract::ViewId, generation: &shell_contract::runtime::RendererGeneration, path: &str) -> super::editors::WorkbookQueueReceipt {
    let (_, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let prepared = sheets.prepare_workbook_queue(view, path.into(), generation.clone(), &reservation, &owner).unwrap();
    queue_transition(prepared, &reservation, &owner).unwrap()
}

fn blank_fixture(sheets: &super::editors::SheetsViews, view: shell_contract::ViewId, generation: &shell_contract::runtime::RendererGeneration) -> super::editors::BlankSeedReceipt {
    let (_, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let prepared = sheets.prepare_blank_seed(view, generation.clone(), &reservation, &owner).unwrap();
    blank_transition(prepared, &reservation, &owner).unwrap()
}


#[test]
fn workbook_queue_prepare_rejects_empty_slot_aba() {
    let w = world("sheets-queue-aba", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (_, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let prepared = sheets.prepare_workbook_queue(sender.view, "same.xlsx".into(), generation.clone(), &reservation, &owner).unwrap();
    sheets.queue_workbook(sender.view, "same.xlsx");
    let consumed = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(consumed, "same.xlsx");
    assert!(queue_transition(prepared, &reservation, &owner).is_err());
}

#[test]
fn workbook_queue_receipt_never_removes_replacement_and_retries() {
    let w = world("sheets-queue-receipt", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    sheets.queue_workbook(sender.view, "before.xlsx");
    let first = queue_fixture(&sheets, sender.view, &generation, "same.xlsx");
    let second = queue_fixture(&sheets, sender.view, &generation, "same.xlsx");
    assert!(first.rollback().is_err());
    second.rollback().unwrap();
    first.rollback().unwrap();
    first.rollback().unwrap();
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "before.xlsx");
}

#[test]
fn workbook_receipt_requires_original_closure_reservation() {
    let w = world("sheets-queue-closure-proof", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    sheets.queue_workbook(sender.view, "before.xlsx");
    let prepared = sheets.prepare_workbook_queue(sender.view, "new.xlsx".into(), generation.clone(), &reservation, &owner).unwrap();
    let receipt = queue_transition(prepared, &reservation, &owner).unwrap();
    let (other, _) = shell_contract::ipc::NativeClosureIssuer::new(sender.view, generation.clone());
    assert!(receipt.retire_closed(&other.closed()).is_err());
    generation.revoke();
    assert!(receipt.rollback().is_err());
    receipt.retire_closed(&issuer.closed()).unwrap();
    receipt.rollback().unwrap();
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "before.xlsx");
}

#[test]
fn adopted_workbook_retirement_never_restores_displaced_or_removes_successor() {
    let w = world("sheets-adopted-queue", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    sheets.queue_workbook(sender.view, "before.xlsx");
    let prepared = sheets.prepare_workbook_queue(sender.view, "adopted.xlsx".into(), generation.clone(), &reservation, &owner).unwrap();
    let receipt = queue_transition(prepared, &reservation, &owner).unwrap();
    let (other, _) = shell_contract::ipc::NativeClosureIssuer::new(sender.view, generation);
    assert!(receipt.retire_adopted(&other.closed()).is_err());
    sheets.queue_workbook(sender.view, "successor.xlsx");
    receipt.retire_adopted(&issuer.closed()).unwrap();
    receipt.rollback().unwrap();
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "successor.xlsx");
}

#[test]
fn sheets_transfer_retires_only_predecessor_registration() {
    let w = world("sheets-transfer-owner", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (_issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let mut initial = sheets.prepare_tab_registration(sender.view, generation, &reservation, &owner).unwrap();
    reservation.with_initial_owner(&owner, |transition| initial.commit_admitted(transition)).unwrap();
    let predecessor = initial.finish().unwrap();
    sheets.queue_workbook(sender.view, "successor.xlsx");
    let transfer = predecessor.prepare_transfer().unwrap();
    let (successor, proof, successor_receipt) = reservation.prepare_replacement(&owner).unwrap().commit(|transition, successor| {
        let validated = transfer.validate_in_transition(transition, successor)?;
        Ok(validated.commit_in_transition())
    }).unwrap();
    predecessor.retire_superseded(&proof).unwrap();
    assert!(predecessor.discard().unwrap());
    assert!(sheets.is_tab(sender.view));
    assert_eq!(sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume(), "successor.xlsx");
    let state = sheets.0.state.borrow();
    assert!(state.tab(sender.view).unwrap().browser_owner.as_ref().unwrap().same_owner(&successor));
    drop(state);
    assert!(successor_receipt.prepare_transfer().is_ok());
}

#[test]
fn sheets_close_claim_preserves_actor_until_terminal_receipt() {
    let w = world("sheets-claimed-close", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let mut initial = sheets.prepare_tab_registration(sender.view, generation, &reservation, &owner).unwrap();
    reservation.with_initial_owner(&owner, |transition| initial.commit_admitted(transition)).unwrap();
    let registration = initial.finish().unwrap();
    let close = registration.prepare_close().unwrap();
    let claim = reservation.claim_close(&owner, |transition| {
        let validated = close.validate_in_transition(transition)?;
        validated.commit_in_transition();
        Ok(())
    }).unwrap();
    assert!(matches!(claim, shell_contract::ipc::NativeBrowserCloseClaim::Won { .. }));
    assert!(sheets.is_tab(sender.view));
    assert!(registration.prepare_transfer().is_err());
    registration.retire_adopted(&issuer.closed()).unwrap();
    assert!(!sheets.is_tab(sender.view));
}

#[test]
fn adopted_sheet_close_detaches_before_reentrant_successor_registration() {
    let w = world("sheets-adopted-reentrant-close", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let mut prepared = sheets.prepare_tab_registration(sender.view, generation.clone(), &reservation, &owner).unwrap();
    reservation.with_initial_owner(&owner, |transition| prepared.commit_admitted(transition)).unwrap();
    let registered = prepared.finish().unwrap();
    let callback_sheets = sheets.clone();
    sheets.0.state.borrow_mut().path_save_waiters.insert(1, (sender.view, Box::new(move |result| {
        assert!(result.is_err());
        callback_sheets.register_tab(sender.view);
        callback_sheets.queue_workbook(sender.view, "successor.xlsx");
    })));
    let (other, _) = shell_contract::ipc::NativeClosureIssuer::new(sender.view, generation);
    assert!(registered.retire_adopted(&other.closed()).is_err());
    assert!(sheets.is_tab(sender.view));
    registered.retire_adopted(&issuer.closed()).unwrap();
    assert!(sheets.is_tab(sender.view));
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "successor.xlsx");
}

#[test]
fn workbook_queue_consumption_prevents_restore_of_displaced_item() {
    let w = world("sheets-queue-consumed", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    sheets.queue_workbook(sender.view, "before.xlsx");
    let receipt = queue_fixture(&sheets, sender.view, &generation, "next.xlsx");
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "next.xlsx");
    sheets.queue_workbook(sender.view, "newer.xlsx");
    receipt.rollback().unwrap();
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(sender.view).unwrap().consume();
    assert_eq!(selected, "newer.xlsx");
}

#[test]
fn workbook_queue_receipt_rejects_successor_renderer() {
    let w = world("sheets-queue-successor", Value::Null);
    let original = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(original.view).unwrap();
    let receipt = queue_fixture(&sheets, original.view, &generation, "old.xlsx");
    let successor = w.kit.view(1, Surface::Sheets);
    sheets.queue_workbook(successor.view, "new.xlsx");
    assert!(receipt.rollback().is_err());
    let selected = sheets.0.state.borrow_mut().remove_queued_workbook(successor.view).unwrap().consume();
    assert_eq!(selected, "new.xlsx");
}

#[test]
fn blank_seed_is_bound_to_created_view_and_rollback_is_retryable() {
    let w = world("sheets-blank-receipt", Value::Null);
    let first = w.tab(1);
    let second = w.tab(2);
    let sheets = views().unwrap();
    sheets.set_new_blank();
    let first_generation = w.kit.services.views.renderer_generation(first.view).unwrap();
    let second_generation = w.kit.services.views.renderer_generation(second.view).unwrap();
    let first_seed = blank_fixture(&sheets, first.view, &first_generation);
    let second_seed = blank_fixture(&sheets, second.view, &second_generation);
    assert!(first_seed.rollback().is_err());
    second_seed.rollback().unwrap();
    first_seed.rollback().unwrap();
    first_seed.rollback().unwrap();
    assert_eq!(w.call(second, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(true));
    assert_eq!(w.call(first, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(false));
}

#[test]
fn blank_seed_consumption_cannot_steal_or_undo_a_successor() {
    let w = world("sheets-blank-target", Value::Null);
    let first = w.tab(1);
    let second = w.tab(2);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(first.view).unwrap();
    let receipt = blank_fixture(&sheets, first.view, &generation);
    assert_eq!(w.call(second, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(false));
    assert_eq!(w.call(first, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(true));
    sheets.set_new_blank();
    receipt.rollback().unwrap();
    assert_eq!(w.call(second, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(true));
}

#[test]
fn blank_seed_prepare_rejects_set_then_consume_aba() {
    let w = world("sheets-blank-aba", Value::Null);
    let sender = w.tab(1);
    let sheets = views().unwrap();
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (_, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let prepared = sheets.prepare_blank_seed(sender.view, generation.clone(), &reservation, &owner).unwrap();
    sheets.set_new_blank();
    assert_eq!(w.call(sender, "sheets:consume-new-blank", vec![]).unwrap(), IpcValue::Bool(true));
    assert!(blank_transition(prepared, &reservation, &owner).is_err());
}

#[test]
fn external_links_resolve_only_the_owning_tab_session() {
    let w = world("sheets-external-session", Value::Null);
    let first = w.tab(1);
    let second = w.tab(2);
    let path = w.file("book.xlsx", b"xlsx");
    let opened = w.open(first, &path);
    let id = text(&opened, "sessionId");
    let sheets = views().unwrap();
    assert!(sheets.is_tab(first.view));
    assert_eq!(
        sheets.external_link_session(first.view, &id),
        Some((path, w.dir.to_string_lossy().into_owned()))
    );
    assert_eq!(sheets.external_link_session(second.view, &id), None);
    w.kit.destroy(first.view);
    assert!(!sheets.is_tab(first.view));
}

#[test]
fn unsaved_workbook_name_is_reserved_until_unmarked() {
    let w = world("sheets-unsaved-mark", Value::Null);
    let sheets = views().unwrap();
    sheets.mark_unsaved_new("backing.xlsx", "proposed.xlsm", "temp");
    assert!(sheets.suggested_path_taken("proposed.xlsm"));
    sheets.unmark_unsaved_new("backing.xlsx");
    assert!(!sheets.suggested_path_taken("proposed.xlsm"));
}

#[test]
fn select_and_reopen_keep_what_a_session_remembers() {
    let w = world("sheets-select", Value::Null);
    let one = w.tab(1);
    let a = w.file("a.xlsx", b"xlsx");
    assert_eq!(
        w.call(one, "sheets:has-queued-workbook", vec![]).unwrap(),
        IpcValue::Bool(false)
    );
    views().unwrap().queue_workbook(one.view, &a);
    assert_eq!(
        w.call(one, "sheets:has-queued-workbook", vec![]).unwrap(),
        IpcValue::Bool(true)
    );
    let file = w.call(one, "workbook:select", vec![]).unwrap();
    assert_eq!(text(&file, "name"), "a.xlsx");
    assert_eq!(text(&file, "path"), a);
    assert_eq!(field(&file, "needsSaveAs"), &IpcValue::Bool(false));
    assert_eq!(field(&file, "readOnly"), &IpcValue::Bool(false));
    assert_eq!(text(&file, "sha256").len(), 64);
    assert_eq!(
        w.call(one, "sheets:has-queued-workbook", vec![]).unwrap(),
        IpcValue::Bool(false)
    );
    assert_eq!(
        w.shell().opened.lock().unwrap().as_slice(),
        [(one.view, a.clone())]
    );

    // nothing queued: the picker; cancelled answers null
    assert_eq!(
        w.call(one, "workbook:select", vec![]).unwrap(),
        IpcValue::Null
    );
    w.kit
        .dialogs
        .opens
        .lock()
        .unwrap()
        .push_back(Some(vec![a.clone().into()]));
    let again = w.call(one, "workbook:select", vec![]).unwrap();
    assert_ne!(text(&again, "sessionId"), text(&file, "sessionId"));

    // a plain path re-opens by itself; a missing file and a foreign extension are refused
    let twin = w
        .call(one, "workbook:reopen", vec![string(a.clone())])
        .unwrap();
    assert_eq!(text(&twin, "name"), "a.xlsx");
    let missing = w.dir.join("gone.xlsx").to_string_lossy().into_owned();
    assert_eq!(
        message(
            w.call(one, "workbook:reopen", vec![string(missing)])
                .unwrap_err()
        ),
        "Workbook file not found."
    );
    let doc = w.file("b.docx", b"x");
    assert_eq!(
        message(
            w.call(one, "workbook:reopen", vec![string(doc)])
                .unwrap_err()
        ),
        "Unsupported workbook: docx"
    );

    // a converted .xls is a copy: the first save is a Save As, and re-opening
    // the copy keeps saying so
    let xls = w.file("old.xls", b"xls");
    let converted = w.open(one, &xls);
    assert_eq!(field(&converted, "needsSaveAs"), &IpcValue::Bool(true));
    assert_eq!(text(&converted, "name"), "old.xlsx");
    let copy_path = text(&converted, "path");
    let continued = w
        .call(one, "workbook:reopen", vec![string(copy_path)])
        .unwrap();
    assert_eq!(field(&continued, "needsSaveAs"), &IpcValue::Bool(true));
    assert_eq!(text(&continued, "name"), "old.xlsx");

    // a CSV keeps its identity
    let csv = w.file("t.csv", b"a,b\n1,2\n");
    let opened = w.open(one, &csv);
    assert_eq!(text(&opened, "csvPath"), csv);
    assert_eq!(field(&opened, "emptyCsv"), &IpcValue::Undefined);
}

#[test]
fn reads_check_the_request_and_the_session() {
    let w = world("sheets-reads", Value::Null);
    let one = w.tab(1);
    let a = w.file("a.xlsx", b"xlsx");
    let file = w.open(one, &a);
    let session = text(&file, "sessionId");
    let request = |sheet: &str| {
        obj([
            ("sessionId", string(session.clone())),
            ("sheetId", string(sheet)),
            (
                "range",
                obj([
                    ("startRow", num(0.0)),
                    ("endRow", num(1.0)),
                    ("startColumn", num(0.0)),
                    ("endColumn", num(1.0)),
                ]),
            ),
        ])
    };
    assert!(
        w.call(one, "workbook:read-range", vec![request("s1")])
            .is_ok()
    );
    let bad = w
        .call(
            one,
            "workbook:read-range",
            vec![obj([("sessionId", string("x"))])],
        )
        .unwrap_err();
    assert_eq!(bad.name, None);
    assert!(bad.message.contains("Invalid UUID"), "{}", bad.message);
    let unknown = obj([
        ("sessionId", string("00000000-0000-4000-8000-0000000000ff")),
        ("sheetId", string("s1")),
    ]);
    assert_eq!(
        message(
            w.call(one, "workbook:read-formulas", vec![unknown])
                .unwrap_err()
        ),
        "Unknown workbook session."
    );
    // recalculation resolves sheet ids to the file's names and back
    let recalc = obj([
        ("sessionId", string(session.clone())),
        (
            "edits",
            IpcValue::Array(vec![obj([
                ("sheetId", string("s1")),
                ("row", num(0.0)),
                ("column", num(0.0)),
                ("input", string("=1+1")),
            ])]),
        ),
        ("reads", IpcValue::Array(vec![])),
    ]);
    let answer = w.call(one, "workbook:recalc", vec![recalc]).unwrap();
    let IpcValue::Array(cells) = field(&answer, "cells") else {
        panic!("cells")
    };
    assert_eq!(text(&cells[0], "sheetId"), "s1");
    assert_eq!(field(&cells[0], "isFormula"), &IpcValue::Bool(true));
}

#[test]
fn a_chunked_edit_transfer_feeds_one_save_and_can_be_aborted() {
    let w = world("sheets-transfer", Value::Null);
    let one = w.tab(1);
    let a = w.file("a.xlsx", b"xlsx");
    let file = w.open(one, &a);
    let session = text(&file, "sessionId");
    let begin = |transfer: &str, total: f64| {
        obj([
            ("sessionId", string(session.clone())),
            ("transferId", string(transfer)),
            ("total", num(total)),
        ])
    };
    let chunk = |transfer: &str, seq: f64, json: &str| {
        obj([
            ("sessionId", string(session.clone())),
            ("transferId", string(transfer)),
            ("seq", num(seq)),
            ("editsJson", string(json)),
        ])
    };
    let t1 = jscompat::random_uuid();
    let bad = w
        .call(one, "workbook:save-edits-begin", vec![begin(&t1, 0.0)])
        .unwrap_err();
    assert!(bad.message.contains("Too small: expected number to be >0"));
    w.call(one, "workbook:save-edits-begin", vec![begin(&t1, 3.0)])
        .unwrap();
    assert_eq!(
        message(
            w.call(one, "workbook:save-edits-begin", vec![begin(&t1, 3.0)])
                .unwrap_err()
        ),
        "Save transfer already exists."
    );
    w.call(one, "workbook:save-edits-chunk", vec![chunk(&t1, 0.0, "[{\"sheetId\":\"s1\",\"row\":0,\"column\":0},{\"sheetId\":\"s1\",\"row\":1,\"column\":0}]")]).unwrap();
    // out of order drops the transfer
    assert_eq!(
        message(
            w.call(
                one,
                "workbook:save-edits-chunk",
                vec![chunk(
                    &t1,
                    5.0,
                    "[{\"sheetId\":\"s1\",\"row\":2,\"column\":0}]"
                )]
            )
            .unwrap_err()
        ),
        "Save transfer chunk out of order."
    );
    assert_eq!(
        message(
            w.call(
                one,
                "workbook:save-edits-chunk",
                vec![chunk(
                    &t1,
                    1.0,
                    "[{\"sheetId\":\"s1\",\"row\":2,\"column\":0}]"
                )]
            )
            .unwrap_err()
        ),
        "Unknown save transfer."
    );

    // abort frees an open transfer; saving it afterwards finds nothing
    let t2 = jscompat::random_uuid();
    w.call(one, "workbook:save-edits-begin", vec![begin(&t2, 1.0)])
        .unwrap();
    w.call(
        one,
        "workbook:save-edits-chunk",
        vec![chunk(
            &t2,
            0.0,
            "[{\"sheetId\":\"s1\",\"row\":0,\"column\":0}]",
        )],
    )
    .unwrap();
    let abort = obj([
        ("sessionId", string(session.clone())),
        ("transferId", string(t2.clone())),
    ]);
    assert_eq!(
        w.call(one, "workbook:save-edits-abort", vec![abort.clone()])
            .unwrap(),
        IpcValue::Undefined
    );
    // aborting again is a no-op
    w.call(one, "workbook:save-edits-abort", vec![abort])
        .unwrap();
    let with_transfer = |transfer: &str| {
        let IpcValue::Object(mut entries) = save_request(&session, "save") else {
            unreachable!()
        };
        entries.push(("editsTransferId".into(), string(transfer)));
        IpcValue::Object(entries)
    };
    assert_eq!(
        message(
            w.call(one, "workbook:save", vec![with_transfer(&t2)])
                .unwrap_err()
        ),
        "Unknown save transfer."
    );

    // a complete transfer is consumed by the save that names it
    let t3 = jscompat::random_uuid();
    w.call(one, "workbook:save-edits-begin", vec![begin(&t3, 1.0)])
        .unwrap();
    w.call(
        one,
        "workbook:save-edits-chunk",
        vec![chunk(
            &t3,
            0.0,
            "[{\"sheetId\":\"s1\",\"row\":0,\"column\":0}]",
        )],
    )
    .unwrap();
    let saved = w
        .call(one, "workbook:save", vec![with_transfer(&t3)])
        .unwrap();
    assert_eq!(field(&saved, "canceled"), &IpcValue::Bool(false));
    let new_session = text(field(&saved, "file"), "sessionId");
    assert_ne!(new_session, session);
    assert_eq!(fs::read(&a).unwrap(), b"xlsx+edits");
    // the old session went with the swap
    let gone = obj([
        ("sessionId", string(session.clone())),
        ("sheetId", string("s1")),
    ]);
    assert_eq!(
        message(
            w.call(one, "workbook:read-formulas", vec![gone])
                .unwrap_err()
        ),
        "Unknown workbook session."
    );
    // the same transfer cannot feed a second save
    assert_eq!(
        message(
            w.call(one, "workbook:save", vec![with_transfer(&t3)])
                .unwrap_err()
        ),
        "Unknown save transfer."
    );
}

#[test]
fn a_save_refuses_a_file_changed_underneath_and_clears_the_recovery_copy() {
    let w = world("sheets-save", Value::Null);
    let one = w.tab(1);
    let a = w.file("a.xlsx", b"xlsx");
    let file = w.open(one, &a);
    let session = text(&file, "sessionId");
    // a dirty tab writes its copy
    let written = w
        .call(
            one,
            "workbook:write-recovery",
            vec![save_request(&session, "save")],
        )
        .unwrap();
    assert_eq!(field(&written, "ok"), &IpcValue::Bool(true));
    let copies = w.dir.join("userdata").join("sheets-autosave");
    assert!(
        fs::read_dir(&copies)
            .unwrap()
            .filter_map(Result::ok)
            .count()
            > 0
    );
    // another program changes the file: Save refuses
    fs::write(&a, b"changed").unwrap();
    assert_eq!(
        message(
            w.call(one, "workbook:save", vec![save_request(&session, "save")])
                .unwrap_err()
        ),
        "The workbook changed on disk after it was opened — use Save As instead."
    );
    // Save As asks where; cancelling leaves everything as it was
    let cancelled = w
        .call(
            one,
            "workbook:save",
            vec![save_request(&session, "save-as")],
        )
        .unwrap();
    assert_eq!(cancelled, obj([("canceled", IpcValue::Bool(true))]));
    // a CSV pick is handed back for the renderer to serialize
    let csv_target = w.dir.join("out.csv").to_string_lossy().into_owned();
    w.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(csv_target.clone().into()));
    let csv = w
        .call(
            one,
            "workbook:save",
            vec![save_request(&session, "save-as")],
        )
        .unwrap();
    assert_eq!(text(&csv, "csvSaveAsPath"), csv_target);
    // an .xlsx pick writes there, swaps the session and drops the copy
    let target = w.dir.join("copy.xlsx").to_string_lossy().into_owned();
    w.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone().into()));
    let saved = w
        .call(
            one,
            "workbook:save",
            vec![save_request(&session, "save-as")],
        )
        .unwrap();
    assert_eq!(field(&saved, "canceled"), &IpcValue::Bool(false));
    assert_eq!(text(field(&saved, "file"), "path"), target);
    assert!(fs::read(&target).unwrap().ends_with(b"+edits"));
    assert_eq!(
        fs::read_dir(&copies)
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        0
    );
}

#[test]
fn csv_questions_and_exports() {
    let w = world("sheets-csv", Value::Null);
    let one = w.tab(1);
    let ask = |answer: Option<usize>| {
        if let Some(button) = answer {
            w.kit.dialogs.messages.lock().unwrap().push_back(button);
        }
        w.call(one, "workbook:csv-save-confirm", vec![]).unwrap()
    };
    assert_eq!(ask(Some(0)), string("csv"));
    assert_eq!(ask(Some(1)), string("xlsx"));
    // the dialog closed without an answer: its cancel button
    assert_eq!(ask(None), string("cancel"));
    w.kit.dialogs.messages.lock().unwrap().push_back(1);
    assert_eq!(
        w.call(one, "workbook:withheld-export-choice", vec![string("PDF")])
            .unwrap(),
        string("export")
    );
    assert_eq!(
        w.call(one, "workbook:withheld-export-choice", vec![string("ODS")])
            .unwrap_err()
            .message,
        "Invalid export format."
    );

    let export = |formulas: bool, target: Option<&str>| {
        let mut entries = vec![
            ("fileName", string("t.csv")),
            ("content", string("a,b\n1,2")),
            ("hasFormulas", IpcValue::Bool(formulas)),
        ];
        if let Some(target) = target {
            entries.push(("targetPath", string(target)));
        }
        let request = IpcValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        );
        w.call(one, "workbook:export-csv", vec![request])
    };
    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    assert_eq!(
        export(true, None).unwrap(),
        obj([
            ("canceled", IpcValue::Bool(true)),
            ("saveAsXlsxInstead", IpcValue::Bool(true))
        ])
    );
    // the dialog's name gets .csv, the file starts with a UTF-8 BOM
    let picked = w.dir.join("export").to_string_lossy().into_owned();
    w.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(picked.clone().into()));
    let done = export(false, None).unwrap();
    assert_eq!(text(&done, "path"), format!("{picked}.csv"));
    assert_eq!(
        fs::read(format!("{picked}.csv")).unwrap(),
        b"\xef\xbb\xbfa,b\n1,2"
    );
    assert_eq!(
        export(false, None).unwrap(),
        obj([("canceled", IpcValue::Bool(true))])
    );
}

#[test]
fn picking_a_range_in_another_workbook_relays_events_in_order() {
    let w = world("sheets-pick", Value::Null);
    let (one, two, three) = (w.tab(1), w.tab(2), w.tab(3));
    let b = w.file("b.xlsx", b"xlsx");
    w.open(two, &b);
    let listed = w.call(one, "workbook:list-tabs", vec![]).unwrap();
    assert_eq!(
        field(&listed, "tabs"),
        &IpcValue::Array(vec![obj([
            ("wcId", num(2.0)),
            ("name", string("b.xlsx")),
            ("path", string(b.clone())),
        ])])
    );
    w.kit.take_events();

    let armed = w
        .call(one, "workbook:pick-arm", vec![obj([("wcId", num(2.0))])])
        .unwrap();
    let pick = text(&armed, "pickId");
    assert!(pick.starts_with("pick-"));
    let events = w.kit.take_events();
    assert_eq!(
        names(&events),
        [(Target::View(two.view), "workbook:pick-armed")]
    );
    assert_eq!(events[0].2, vec![obj([("pickId", string(pick.clone()))])]);

    // arming again cancels the first for every other tab
    let second = text(
        &w.call(one, "workbook:pick-arm", vec![obj([("wcId", num(3.0))])])
            .unwrap(),
        "pickId",
    );
    let events = w.kit.take_events();
    assert_eq!(
        names(&events),
        [
            (Target::View(two.view), "workbook:pick-cancelled"),
            (Target::View(three.view), "workbook:pick-cancelled"),
            (Target::View(three.view), "workbook:pick-armed"),
        ]
    );

    // only the armed tab answers
    let selection = |id: &str| {
        obj([
            ("pickId", string(id)),
            ("sheetName", string("Sheet1")),
            ("startRow", num(0.0)),
            ("endRow", num(1.0)),
            ("startColumn", num(2.0)),
            ("endColumn", num(3.0)),
        ])
    };
    w.call(two, "workbook:pick-select", vec![selection(&second)])
        .unwrap();
    w.call(three, "workbook:pick-select", vec![selection(&pick)])
        .unwrap();
    assert!(w.kit.take_events().is_empty());
    w.call(three, "workbook:pick-select", vec![selection(&second)])
        .unwrap();
    let events = w.kit.take_events();
    assert_eq!(
        names(&events),
        [
            (Target::View(one.view), "workbook:pick-selected"),
            (Target::View(two.view), "workbook:pick-cancelled"),
            (Target::View(three.view), "workbook:pick-cancelled"),
        ]
    );
    assert_eq!(events[0].2, vec![selection(&second)]);
    // the pick is spent
    w.call(three, "workbook:pick-select", vec![selection(&second)])
        .unwrap();
    assert!(w.kit.take_events().is_empty());

    // cancelling tells the others, a stale id changes nothing
    let third = text(
        &w.call(two, "workbook:pick-arm", vec![obj([("wcId", num(1.0))])])
            .unwrap(),
        "pickId",
    );
    w.kit.take_events();
    w.call(
        two,
        "workbook:pick-cancel",
        vec![obj([("pickId", string("pick-stale"))])],
    )
    .unwrap();
    assert!(w.kit.take_events().is_empty());
    w.call(
        two,
        "workbook:pick-cancel",
        vec![obj([("pickId", string(third))])],
    )
    .unwrap();
    assert_eq!(
        names(&w.kit.take_events()),
        [
            (Target::View(one.view), "workbook:pick-cancelled"),
            (Target::View(three.view), "workbook:pick-cancelled"),
        ]
    );
}

type Outcome = Rc<RefCell<Option<Result<bool, IpcError>>>>;

fn outcome() -> (Outcome, shell_contract::editors::Then<bool>) {
    let cell: Outcome = Rc::default();
    let then = cell.clone();
    (
        cell,
        Box::new(move |result| *then.borrow_mut() = Some(result)),
    )
}

#[test]
fn a_close_with_unsaved_work_asks_then_waits_for_the_renderer() {
    let w = world("sheets-close", Value::Null);
    let one = w.tab(1);
    let editors = editor_for(&w.registry.provided, one.view).expect("sheets owns the view");
    let parent = Some(WindowId(1));

    // nothing pending: no question
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    assert_eq!(done.borrow_mut().take(), Some(Ok(true)));
    assert!(w.kit.take_log().is_empty());

    // the renderer mirrors its pending count
    w.call(one, "workbook:pending-edits", vec![num(3.0)])
        .unwrap();
    // junk counts are ignored
    w.call(one, "workbook:pending-edits", vec![num(-1.0)])
        .unwrap();
    w.call(one, "workbook:pending-edits", vec![string("many")])
        .unwrap();

    // Cancel
    w.kit.dialogs.messages.lock().unwrap().push_back(2);
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));
    let log = w.kit.take_log();
    assert!(log[0].contains("3 unsaved change(s)"), "{}", log[0]);
    assert!(w.kit.take_events().is_empty());

    // Save: the renderer is asked, and close-save-result resumes the close
    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    assert!(done.borrow().is_none());
    assert_eq!(
        names(&w.kit.take_events()),
        [(Target::View(one.view), "workbook:close-save-request")]
    );
    w.call(
        one,
        "workbook:close-save-result",
        vec![IpcValue::Bool(true)],
    )
    .unwrap();
    assert_eq!(done.borrow_mut().take(), Some(Ok(true)));
    // a result nobody waits for is ignored
    w.call(
        one,
        "workbook:close-save-result",
        vec![IpcValue::Bool(true)],
    )
    .unwrap();

    // a failed save keeps the tab open
    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    w.call(
        one,
        "workbook:close-save-result",
        vec![IpcValue::Bool(false)],
    )
    .unwrap();
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));

    // no answer for two minutes: the close is refused
    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    w.shell().fire(120_000);
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));
    // the late result changes nothing
    w.call(
        one,
        "workbook:close-save-result",
        vec![IpcValue::Bool(true)],
    )
    .unwrap();

    // Don't Save closes without asking the renderer
    w.kit.dialogs.messages.lock().unwrap().push_back(1);
    w.kit.take_events();
    let (done, then) = outcome();
    editors.request_close(one.view, parent, then);
    assert_eq!(done.borrow_mut().take(), Some(Ok(true)));
    assert!(w.kit.take_events().is_empty());
}

#[test]
fn a_save_request_is_resumed_by_the_renderers_answer() {
    let w = world("sheets-request-save", Value::Null);
    let one = w.tab(1);
    let editors = editor_for(&w.registry.provided, one.view).unwrap();
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::Save, then);
    assert_eq!(
        names(&w.kit.take_events()),
        [(Target::View(one.view), "workbook:close-save-request")]
    );
    assert!(done.borrow().is_none());
    w.call(
        one,
        "workbook:close-save-result",
        vec![IpcValue::Bool(true)],
    )
    .unwrap();
    assert_eq!(done.borrow_mut().take(), Some(Ok(true)));
    // the renderer going away resolves a waiting save as not saved
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::Save, then);
    w.call(one, "workbook:pending-edits", vec![num(1.0)])
        .unwrap();
    w.kit.destroy(one.view);
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));
}

/// a workbook opened from a file, saved and clean: the session id
fn clean_workbook(w: &World, one: Sender) -> String {
    let a = w.file("a.xlsx", b"xlsx");
    text(&w.open(one, &a), "sessionId")
}

#[test]
fn a_save_as_the_shell_asks_for_always_asks_for_a_file() {
    let w = world("sheets-request-save-as", Value::Null);
    let one = w.tab(1);
    let session = clean_workbook(&w, one);
    let editors = editor_for(&w.registry.provided, one.view).unwrap();
    w.kit.take_events();

    // a clean, already saved workbook is no reason to answer at once
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    assert!(done.borrow().is_none());
    assert_eq!(
        w.kit.take_events(),
        [(
            Target::View(one.view),
            "menu:action",
            vec![string("save-as")]
        )]
    );
    // the renderer's own plain Save is not the Save As (it swaps the session)
    let plain = w
        .call(one, "workbook:save", vec![save_request(&session, "save")])
        .unwrap();
    assert!(done.borrow().is_none());
    let session = text(field(&plain, "file"), "sessionId");

    // the person cancels the dialog: not saved
    let cancelled = w
        .call(
            one,
            "workbook:save",
            vec![save_request(&session, "save-as")],
        )
        .unwrap();
    assert_eq!(cancelled, obj([("canceled", IpcValue::Bool(true))]));
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));

    // a file is chosen and written: saved
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    let target = w.dir.join("copy.xlsx").to_string_lossy().into_owned();
    w.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone().into()));
    let saved = w
        .call(
            one,
            "workbook:save",
            vec![save_request(&session, "save-as")],
        )
        .unwrap();
    assert_eq!(field(&saved, "canceled"), &IpcValue::Bool(false));
    assert_eq!(done.borrow_mut().take(), Some(Ok(true)));
    // the answer is spent: a later Save As of the renderer (the session
    // swapped to the new file) resumes nothing
    let swapped = text(field(&saved, "file"), "sessionId");
    w.call(
        one,
        "workbook:save",
        vec![save_request(&swapped, "save-as")],
    )
    .unwrap();
    assert!(done.borrow().is_none());
}

#[test]
fn a_save_as_that_fails_or_goes_unanswered_is_not_saved() {
    let w = world("sheets-request-save-as-ends", Value::Null);
    let (one, two) = (w.tab(1), w.tab(2));
    let session = clean_workbook(&w, one);
    let editors = editor_for(&w.registry.provided, one.view).unwrap();

    // a save that rejects (a request of a session that is not open)
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    let unknown = "00000000-0000-4000-8000-0000000000ff";
    w.call(one, "workbook:save", vec![save_request(unknown, "save-as")])
        .unwrap_err();
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));

    // another view's Save As does not resume this one
    let (done, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    w.call(
        two,
        "workbook:save",
        vec![save_request(&session, "save-as")],
    )
    .unwrap_err();
    assert!(done.borrow().is_none());

    // a newer request replaces the older, which is not saved
    let (newer, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));
    assert!(newer.borrow().is_none());

    // two minutes without a Save As: not saved, and the late save changes nothing
    w.shell().fire(120_000);
    assert_eq!(newer.borrow_mut().take(), Some(Ok(false)));
    w.call(
        one,
        "workbook:save",
        vec![save_request(&session, "save-as")],
    )
    .unwrap();
    assert!(newer.borrow().is_none());

    // the timer of an answered Save As does not end the view's next one
    let (first, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    w.call(
        one,
        "workbook:save",
        vec![save_request(&session, "save-as")],
    )
    .unwrap();
    assert_eq!(first.borrow_mut().take(), Some(Ok(false)));
    let (second, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    w.shell().fire_oldest(120_000);
    assert!(second.borrow().is_none());
    w.shell().fire_oldest(120_000);
    assert_eq!(second.borrow_mut().take(), Some(Ok(false)));

    // the view ending
    let (third, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    w.kit.destroy(one.view);
    assert_eq!(third.borrow_mut().take(), Some(Ok(false)));
}

/// A Save As whose save is still on a worker when its wait ends (two
/// minutes, or a newer request) answers no later request: the save keeps
/// the request it arrived for
#[test]
fn a_late_save_as_answers_only_the_request_it_arrived_for() {
    let w = world("sheets-request-save-as-late", Value::Null);
    let one = w.tab(1);
    let session = clean_workbook(&w, one);
    let editors = editor_for(&w.registry.provided, one.view).unwrap();

    let (first, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    let target = w.dir.join("copy.xlsx").to_string_lossy().into_owned();
    w.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.into()));
    w.kit.hold_workers();
    let late = start(
        &w.registry,
        one,
        "workbook:save",
        vec![save_request(&session, "save-as")],
    );
    w.shell().fire(120_000);
    assert_eq!(first.borrow_mut().take(), Some(Ok(false)));

    let (second, then) = outcome();
    editors.request_save(one.view, SaveMode::SaveAs, then);
    w.kit.release_workers();
    let written = late.try_recv().unwrap().unwrap();
    assert_eq!(field(&written, "canceled"), &IpcValue::Bool(false));
    assert!(
        second.borrow().is_none(),
        "the next Save As waits for its own save"
    );

    let swapped = text(field(&written, "file"), "sessionId");
    w.call(
        one,
        "workbook:save",
        vec![save_request(&swapped, "save-as")],
    )
    .unwrap();
    assert_eq!(second.borrow_mut().take(), Some(Ok(false)));
}

#[test]
fn a_view_that_goes_takes_its_sessions_and_state_with_it() {
    let w = world("sheets-gone", Value::Null);
    let (one, two) = (w.tab(1), w.tab(2));
    let a = w.file("a.xlsx", b"xlsx");
    let b = w.file("b.xlsx", b"xlsx");
    w.open(one, &a);
    w.open(two, &b);
    let snapshots = w.dir.join("temp").join("seuoffice-sheets-sessions");
    let count = |dir: &PathBuf| fs::read_dir(dir).map_or(0, |d| d.filter_map(Result::ok).count());
    assert_eq!(count(&snapshots), 2);
    w.call(one, "workbook:pending-edits", vec![num(2.0)])
        .unwrap();
    let (done, then) = outcome();
    editor_for(&w.registry.provided, one.view)
        .unwrap()
        .request_save(one.view, SaveMode::Save, then);
    // a queued file and an armed pick die with the view
    views().unwrap().queue_workbook(one.view, &a);
    w.kit.destroy(one.view);
    assert_eq!(done.borrow_mut().take(), Some(Ok(false)));
    assert_eq!(count(&snapshots), 1);
    assert_eq!(
        w.call(two, "workbook:list-tabs", vec![]).unwrap(),
        obj([("tabs", IpcValue::Array(vec![]))])
    );
    assert!(editor_for(&w.registry.provided, one.view).is_none());
    // the dead view's recovery question is gone with it
    assert!(
        w.dir
            .join("userdata")
            .join("sheets-autosave")
            .read_dir()
            .is_err()
    );
    // an unsaved tab's window closing keeps its pending count out of the next tab
    let again = w.tab(1);
    assert_eq!(
        w.call(again, "sheets:has-queued-workbook", vec![]).unwrap(),
        IpcValue::Bool(false)
    );
}

#[test]
fn the_sidecar_dying_is_told_to_every_tab() {
    let w = world("sheets-crash", Value::Null);
    let (one, two) = (w.tab(1), w.tab(2));
    w.kit.take_events();
    w.engine().exit();
    assert_eq!(
        names(&w.kit.take_events()),
        [
            (Target::View(one.view), "workbook:sidecar-crashed"),
            (Target::View(two.view), "workbook:sidecar-crashed"),
        ]
    );
}

#[test]
fn flags_the_shell_and_the_renderer_share() {
    let w = world("sheets-flags", Value::Null);
    let one = w.tab(1);
    assert_eq!(
        w.call(one, "sheets:consume-new-blank", vec![]).unwrap(),
        IpcValue::Bool(false)
    );
    views().unwrap().set_new_blank();
    assert_eq!(
        w.call(one, "sheets:consume-new-blank", vec![]).unwrap(),
        IpcValue::Bool(true)
    );
    assert_eq!(
        w.call(one, "sheets:consume-new-blank", vec![]).unwrap(),
        IpcValue::Bool(false)
    );
    assert_eq!(
        w.call(one, "sheets:consume-headless-export", vec![])
            .unwrap(),
        IpcValue::Null
    );
    let settled = Rc::new(RefCell::new(None));
    let sink = settled.clone();
    views().unwrap().register_headless_export(
        one.view,
        "/out.pdf",
        Box::new(move |report| *sink.borrow_mut() = Some(report)),
    );
    assert_eq!(
        w.call(one, "sheets:consume-headless-export", vec![])
            .unwrap(),
        string("/out.pdf")
    );
    w.call(
        one,
        "sheets:headless-export-done",
        vec![obj([("ok", IpcValue::Bool(true))])],
    )
    .unwrap();
    assert_eq!(settled.borrow().as_ref().map(|r| r.ok), Some(true));
}

fn headless_target_unregistered(w: &World, sender: Sender) -> shell_contract::ipc::NativeInvocationTarget {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let created = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let issuer = NativeInvocationIssuer::new(shell_contract::runtime::CancelToken::default());
    let operation = issuer.issue(NativeInvocationSelection::HeadlessExport {
        surface: Surface::Sheets, input: "/input.xlsx".into(), output: "/output.pdf".into(),
        format: "pdf".into(), page: None, ops: None, batch: None,
    }, shell_contract::runtime::CancelToken::default());
    operation.reserve_target(sender.view, created).unwrap()
}

fn headless_target(w: &World, sender: Sender) -> shell_contract::ipc::NativeInvocationTarget {
    let target = headless_target_unregistered(w, sender);
    shell_contract::hosts::synthetic::register_native_invocation_fixture(&target, w.native_owner(sender)).unwrap();
    target
}

#[test]
fn headless_export_uses_created_renderer_and_cleans_the_original_tab() {
    let w = world("sheets-headless-created", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let created = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, created.clone());
    let settled = Rc::new(RefCell::new(None));
    let output = settled.clone();
    let sheets = views().unwrap();
    let registration = Rc::new(sheets.begin_headless_export(sender.view, headless_target(&w, sender), "/input.xlsx", "/output.pdf",
        Box::new(move |report| *output.borrow_mut() = Some(report)), &reservation, &owner).unwrap());
    assert_eq!(w.call(sender, "sheets:consume-headless-export", vec![]).unwrap(), string("/output.pdf"));
    w.call(sender, "sheets:headless-export-done", vec![obj([("ok", IpcValue::Bool(true))])]).unwrap();
    assert_eq!(settled.borrow().as_ref().map(|report| report.ok), Some(true));
    let (finished, then) = outcome();
    sheets.finish_headless_export(registration, issuer.closed(), then);
    assert!(matches!(&*finished.borrow(), Some(Ok(()))));
    assert!(!sheets.is_tab(sender.view));
}

#[test]
fn headless_close_waits_for_started_work_and_rejects_new_work() {
    let w = world("sheets-headless-drain", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation);
    let sheets = views().unwrap();
    let registration = Rc::new(sheets.begin_headless_export(sender.view, headless_target(&w, sender),
        "/input.xlsx", "/output.pdf", Box::new(|_| {}), &reservation, &owner).unwrap());
    w.kit.hold_workers();
    let pending = w.start(sender, "workbook:select", vec![]);
    assert!(pending.try_recv().is_err());
    let (finished, then) = outcome();
    sheets.finish_headless_export(registration, issuer.closed(), then);
    assert!(finished.borrow().is_none());
    assert!(w.start(sender, "workbook:select", vec![]).try_recv().unwrap().is_err());
    w.kit.release_workers();
    assert!(pending.try_recv().is_ok());
    assert!(matches!(&*finished.borrow(), Some(Ok(()))));
    assert!(!sheets.is_tab(sender.view));
}

#[test]
fn unsubmitted_headless_reservation_cleans_its_registered_seed() {
    let w = world("sheets-headless-not-created", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation);
    let sheets = views().unwrap();
    let registration = Rc::new(sheets.begin_headless_export(sender.view, headless_target_unregistered(&w, sender),
        "/input.xlsx", "/output.pdf", Box::new(|_| {}), &reservation, &owner).unwrap());
    let (finished, then) = outcome();
    sheets.finish_headless_export(registration, issuer.not_created(), then);
    assert!(matches!(&*finished.borrow(), Some(Ok(()))));
    assert!(!sheets.is_tab(sender.view));
    assert!(!sheets.0.state.borrow().queued_workbooks.contains_key(&sender.view));
}

#[test]
fn headless_export_rejects_successor_renderer_and_stale_cleanup() {
    let w = world("sheets-headless-replaced", Value::Null);
    let original = w.kit.view(1, Surface::Sheets);
    let created = w.kit.services.views.renderer_generation(original.view).unwrap();
    let (_issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(original.view, created.clone());
    let sheets = views().unwrap();
    let registration = Rc::new(sheets.begin_headless_export(original.view, headless_target(&w, original), "/input.xlsx", "/output.pdf", Box::new(|_| {}), &reservation, &owner).unwrap());
    let successor = w.kit.view(1, Surface::Sheets);
    assert!(w.call(successor, "sheets:consume-headless-export", vec![]).is_err());
    let (other, _) = shell_contract::ipc::NativeClosureIssuer::new(original.view, created);
    let (finished, then) = outcome();
    sheets.finish_headless_export(registration, other.closed(), then);
    assert!(matches!(&*finished.borrow(), Some(Err(_))));
    assert!(sheets.is_tab(original.view));
}

#[test]
fn native_snapshot_pins_input_and_rejects_another_invocation() {
    use super::engine::WorkbookEngine;
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    use shell_contract::runtime::CancelToken;
    let w = world("sheets-native-snapshot", Value::Null);
    let sender = w.kit.view(1, Surface::Sheets);
    let input = w.file("source.csv", b"first");
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let selection = || NativeInvocationSelection::HeadlessExport {
        surface: Surface::Sheets, input: input.clone().into(), output: "/output.pdf".into(),
        format: "pdf".into(), page: None, ops: None, batch: None,
    };
    let operation = issuer.issue(selection(), CancelToken::default());
    let target = operation.reserve_target(sender.view, generation.clone()).unwrap();
    let document = shell_contract::hosts::synthetic::register_native_invocation_fixture(&target, w.native_owner(sender)).unwrap();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let engine = w.engine();
    let snapshot = engine.snapshot_native_invocation(&document, &cancel, &pending).unwrap();
    fs::write(&input, b"second").unwrap();
    assert_eq!(fs::read(snapshot.path()).unwrap(), b"first");
    assert!(snapshot.path().ends_with(".csv"));
    let other = issuer.issue(selection(), CancelToken::default());
    let other_target = other.reserve_target(sender.view, generation).unwrap();
    let other_document = shell_contract::hosts::synthetic::register_native_invocation_fixture(&other_target, w.native_owner(sender)).unwrap();
    assert!(engine.open_native_invocation(&snapshot, "en", "M/d/yyyy", None, &other_document, &cancel, &pending).is_err());
    cancel.cancel();
    assert!(engine.open_native_invocation(&snapshot, "en", "M/d/yyyy", None, &document, &cancel, &pending).is_err());
}

#[test]
fn an_oversized_delimited_file_is_refused_before_it_is_read() {
    let w = world(
        "sheets-limit",
        serde_json::json!({ "sheets": { "importLimit": 1024.0 } }),
    );
    let one = w.tab(1);
    let big = w.file("big.csv", &vec![b'a'; 2048]);
    views().unwrap().queue_workbook(one.view, &big);
    let error = w.call(one, "workbook:select", vec![]).unwrap_err();
    assert_eq!(error.message, "exceeds the 0MB limit (0.0 MB)");
    // the open left nothing behind
    assert!(
        !w.dir
            .join("temp")
            .join("seuoffice-imports")
            .read_dir()
            .is_ok_and(|d| d.count() > 0)
    );
}

#[test]
fn a_view_the_shell_never_registered_is_an_untrusted_sender() {
    let w = world("sheets-untrusted", Value::Null);
    let stray = w.kit.view(7, Surface::Sheets);
    assert!(editor_for(&w.registry.provided, stray.view).is_none());
    for (channel, args) in [
        ("workbook:save", vec![save_request("x", "save")]),
        ("workbook:select", vec![]),
        ("workbook:csv-save-confirm", vec![]),
        ("sheets:files-add-pasted-image", vec![]),
    ] {
        let error = w.call(stray, channel, args).expect_err(channel);
        assert_eq!(message(error), "Untrusted IPC sender.", "{channel}");
    }
    // a rejected call registers nothing
    assert!(editor_for(&w.registry.provided, stray.view).is_none());
    views().unwrap().register_tab(stray.view);
    assert!(editor_for(&w.registry.provided, stray.view).is_some());
    let error = w
        .call(stray, "workbook:save", vec![save_request("x", "save")])
        .expect_err("no such session");
    assert_ne!(message(error), "Untrusted IPC sender.");
}

#[test]
fn every_channel_of_the_list_has_a_handler() {
    let w = world("sheets-channels", Value::Null);
    let mine: Vec<&str> = shell_contract::CHANNELS
        .iter()
        .filter(|spec| {
            spec.batch == Batch::D1
                && (spec.name.starts_with("workbook:") || spec.name.starts_with("sheets:"))
                && !spec
                    .kinds
                    .contains(&shell_contract::ipc::ChannelKind::Event)
        })
        .map(|spec| spec.name)
        .collect();
    let missing: Vec<&&str> = mine
        .iter()
        .filter(|name| !w.registry.handlers.contains_key(**name))
        .collect();
    assert!(missing.is_empty(), "no handler for {missing:?}");
    assert_eq!(w.registry.handlers.len(), mine.len());
    assert_eq!(mine.len(), 54);
}

fn owned_save_request(w: &World, sender: Sender, target: PathBuf) -> (
    String, shell_contract::runtime::CancelToken, Rc<RefCell<Option<Result<(), IpcError>>>>,
) {
    let origin = w.kit.view(44, Surface::Home);
    let origin = w.native_owner(origin);
    let cancel = shell_contract::runtime::CancelToken::default();
    let outcome = Rc::new(RefCell::new(None));
    let answer = outcome.clone();
    editor_for(&w.registry.provided, sender.view).unwrap().save_to_path_owned(
        sender.view, target, origin, cancel.clone(),
        Box::new(move |result| *answer.borrow_mut() = Some(result)),
    );
    let events = w.kit.take_events();
    let request = events.iter().find(|(_, channel, _)| *channel == "workbook:close-save-request")
        .expect("an owned save reaches the actual destination renderer");
    (text(&request.2[0], "requestId"), cancel, outcome)
}

#[test]
fn owned_save_origin_cancellation_prevents_the_worker_from_writing_or_repointing() {
    let w = world("sheets-owned-cancel", Value::Null);
    let sender = w.tab(1);
    let original = w.file("original.xlsx", b"original workbook");
    let opened = w.open(sender, &original);
    w.kit.take_events();
    let target = w.dir.join("cancelled.xlsx");
    let (token, cancel, outcome) = owned_save_request(&w, sender, target.clone());
    w.kit.hold_workers();
    let answer = w.start(sender, "workbook:save", vec![obj([
        ("sessionId", string(text(&opened, "sessionId"))),
        ("mode", string("save-as")),
        ("targetPath", string(target.to_string_lossy())),
        ("overwrite", IpcValue::Bool(true)),
        ("ownedSaveRequestId", string(token)),
    ])]);
    assert!(answer.try_recv().is_err());
    cancel.cancel();
    assert!(matches!(&*outcome.borrow(), Some(Err(_))));
    w.kit.release_workers();
    assert!(answer.try_recv().unwrap().is_err());
    assert!(!target.exists());
    assert_eq!(editor_for(&w.registry.provided, sender.view).unwrap().document_path(sender.view),
        Some(PathBuf::from(original)));
}

#[test]
fn owned_save_requires_matching_renderer_and_real_completed_write_before_success() {
    let w = world("sheets-owned-write", Value::Null);
    let sender = w.tab(1);
    let opened = w.open(sender, &w.file("original.xlsx", b"original workbook"));
    w.kit.take_events();
    let target = w.dir.join("saved.xlsx");
    let (token, _cancel, outcome) = owned_save_request(&w, sender, target.clone());
    let foreign = w.tab(2);
    assert!(w.call(foreign, "workbook:close-save-result",
        vec![IpcValue::Bool(true), string(token.clone())]).is_err());
    assert!(outcome.borrow().is_none());
    let result = w.call(sender, "workbook:save", vec![obj([
        ("sessionId", string(text(&opened, "sessionId"))),
        ("mode", string("save-as")),
        ("targetPath", string(target.to_string_lossy())),
        ("overwrite", IpcValue::Bool(true)),
        ("ownedSaveRequestId", string(token.clone())),
    ])]).unwrap();
    assert_eq!(field(&result, "canceled"), &IpcValue::Bool(false));
    assert!(target.is_file());
    assert!(outcome.borrow().is_none());
    w.call(sender, "workbook:close-save-result",
        vec![IpcValue::Bool(true), string(token.clone())]).unwrap();
    assert!(matches!(&*outcome.borrow(), Some(Ok(()))));
    assert_eq!(editor_for(&w.registry.provided, sender.view).unwrap().document_path(sender.view), Some(target));
    assert!(w.call(sender, "workbook:close-save-result",
        vec![IpcValue::Bool(true), string(token)]).is_err());
}

#[test]
fn owned_save_propagates_session_publication_failure() {
    let w = world("sheets-owned-publication-error", serde_json::json!({
        "sheets": { "savedPublicationError": "session publication failed" }
    }));
    let sender = w.tab(1);
    let opened = w.open(sender, &w.file("original.xlsx", b"original workbook"));
    w.kit.take_events();
    let target = w.dir.join("saved.xlsx");
    let (token, _cancel, _outcome) = owned_save_request(&w, sender, target.clone());
    let error = w.call(sender, "workbook:save", vec![obj([
        ("sessionId", string(text(&opened, "sessionId"))),
        ("mode", string("save-as")),
        ("targetPath", string(target.to_string_lossy())),
        ("overwrite", IpcValue::Bool(true)),
        ("ownedSaveRequestId", string(token)),
    ])]).unwrap_err();
    assert!(error.message.contains("session publication failed"));
}

#[test]
fn screen_capture_requires_request_native_authority() {
    let w = world("sheets-screen-native-required", Value::Null);
    let sender = w.tab(1);
    let result = start(&w.registry, sender, "sheets:capture-screen-source",
        vec![obj([("id", string("screen:0"))])]).try_recv().unwrap();
    assert!(result.unwrap_err().message.contains("native request authority"));
}

#[test]
fn screen_capture_does_not_rebind_a_queued_request_to_a_successor_renderer() {
    let w = world("sheets-screen-captured-owner", Value::Null);
    let sender = w.tab(1);
    let args = vec![obj([("id", string("screen:0"))])];
    assert!(matches!(w.call(sender, "sheets:capture-screen-source", args.clone()), Ok(IpcValue::Object(_))));
    w.kit.hold_workers();
    let pending = w.start(sender, "sheets:capture-screen-source", args);
    assert!(pending.try_recv().is_err());
    let successor = w.tab(1);
    assert_eq!(successor.view, sender.view);
    w.kit.release_workers();
    assert!(pending.try_recv().unwrap().is_err());
}

#[test]
fn owned_save_as_does_not_accept_renderer_success_without_a_file_write() {
    let w = world("sheets-owned-premature-ack", Value::Null);
    let sender = w.tab(1);
    w.open(sender, &w.file("original.xlsx", b"original workbook"));
    w.kit.take_events();
    let target = w.dir.join("not-written.xlsx");
    let (token, _cancel, outcome) = owned_save_request(&w, sender, target.clone());
    w.call(sender, "workbook:close-save-result", vec![IpcValue::Bool(true), string(token)]).unwrap();
    assert!(matches!(&*outcome.borrow(), Some(Err(_))));
    assert!(!target.exists());
}

#[test]
fn owned_close_preserves_cancel_and_waits_for_real_save() {
    let w = world("sheets-owned-close", Value::Null);
    let sender = w.tab(1);
    let opened = w.open(sender, &w.file("original.xlsx", b"original workbook"));
    w.kit.take_events();
    w.call(sender, "workbook:pending-edits", vec![num(1.0)]).unwrap();
    let origin = w.kit.view(44, Surface::Home);
    let owner = w.native_owner(origin);
    let editor = editor_for(&w.registry.provided, sender.view).unwrap();
    w.kit.dialogs.messages.lock().unwrap().push_back(2);
    let (cancelled, then) = outcome();
    editor.request_close_owned(sender.view, Some(WindowId(1)), owner.clone(),
        shell_contract::runtime::CancelToken::default(), then);
    assert_eq!(*cancelled.borrow(), Some(Ok(false)));

    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (saved, then) = outcome();
    editor.request_close_owned(sender.view, Some(WindowId(1)), owner,
        shell_contract::runtime::CancelToken::default(), then);
    assert!(saved.borrow().is_none());
    let events = w.kit.take_events();
    let request = events.iter().find(|(_, channel, _)| *channel == "workbook:close-save-request").unwrap();
    let token = text(&request.2[0], "requestId");
    let result = w.call(sender, "workbook:save", vec![obj([
        ("sessionId", string(text(&opened, "sessionId"))), ("mode", string("save")),
        ("ownedSaveRequestId", string(token.clone())),
    ])]).unwrap();
    assert_eq!(field(&result, "canceled"), &IpcValue::Bool(false));
    assert!(saved.borrow().is_none());
    w.call(sender, "workbook:close-save-result", vec![IpcValue::Bool(true), string(token)]).unwrap();
    assert_eq!(*saved.borrow(), Some(Ok(true)));
}

#[test]
fn owned_close_cancel_revokes_the_child_without_settling_legacy_close() {
    let w = world("sheets-owned-close-cancel", Value::Null);
    let sender = w.tab(1);
    w.open(sender, &w.file("original.xlsx", b"original workbook"));
    w.kit.take_events();
    w.call(sender, "workbook:pending-edits", vec![num(1.0)]).unwrap();
    let origin = w.kit.view(44, Surface::Home);
    w.kit.dialogs.messages.lock().unwrap().push_back(0);
    let cancel = shell_contract::runtime::CancelToken::default();
    let (result, then) = outcome();
    editor_for(&w.registry.provided, sender.view).unwrap().request_close_owned(
        sender.view, None, w.native_owner(origin), cancel.clone(), then,
    );
    let events = w.kit.take_events();
    let token = text(&events.iter().find(|(_, channel, _)| *channel == "workbook:close-save-request").unwrap().2[0], "requestId");
    cancel.cancel();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert!(w.call(sender, "workbook:close-save-result", vec![IpcValue::Bool(true), string(token)]).is_err());
}

#[test]
fn owned_rename_rejects_released_additional_participant_without_repointing() {
    let w = world("sheets-rename-many-release", Value::Null);
    let sender = w.tab(1);
    let original = w.file("original.xlsx", b"original workbook");
    w.open(sender, &original);
    let source = w.kit.view(44, Surface::Home);
    let extra = w.kit.view(45, Surface::Home);
    let origin = w.native_owner(source);
    let participant = w.native_owner(extra);
    let editor = editor_for(&w.registry.provided, sender.view).unwrap();
    let destination = editor.capture_document_owner(sender.view, w.native_owner(sender)).unwrap();
    participant.context.renderer().unwrap().revoke();
    let (result, then) = outcome();
    editor.file_renamed_many_owned_extra(
        sender.view, PathBuf::from(&original).as_path(), w.dir.join("renamed.xlsx").as_path(),
        &origin, &destination, &[participant], &shell_contract::runtime::CancelToken::default(),
        &shell_contract::runtime::CancelToken::default(), &[], then,
    );
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(editor.document_path(sender.view), Some(PathBuf::from(original)));
}

#[test]
fn recovery_rename_respects_cancellation_and_notifies_after_success() {
    use shell_contract::hosts::OwnedEffectsHost;
    use shell_contract::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use shell_contract::runtime::CancelToken;

    let w = world("sheets-recovery-rename", Value::Null);
    let sender = w.tab(1);
    let original = w.file("original.xlsx", b"original workbook");
    w.open(sender, &original);
    w.kit.take_events();
    let source = w.kit.view(44, Surface::Home);
    let source_owner = w.native_owner(source);
    let receiver = w.native_owner(sender);
    let generation = w.kit.services.views.renderer_generation(sender.view).unwrap();
    let (_issuer, reservation) = shell_contract::ipc::NativeClosureIssuer::new(sender.view, generation.clone());
    let seed_cancel = CancelToken::default();
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source: source_owner.clone(), participants: vec![source_owner.clone()],
        cancel: seed_cancel.clone(), pending: seed_cancel,
    }, sender.view, generation, reservation).unwrap();
    let effects = w.kit.services.hosts.get::<OwnedEffectsHost>().unwrap().clone();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    target.retain_native_registration(receiver.clone()).unwrap();
    let editor = editor_for(&w.registry.provided, sender.view).unwrap();
    let destination = editor.capture_document_owner(sender.view, receiver.clone()).unwrap();
    let renamed = w.dir.join("renamed.xlsx");
    let cancel = CancelToken::default();
    cancel.cancel();
    let pending = CancelToken::default();
    let (failed, then) = outcome();
    editor.file_renamed_recovery(
        sender.view, PathBuf::from(&original).as_path(), &renamed, &receipt, &receiver,
        &destination, &[source_owner.clone()], &cancel, &pending, then,
    );
    assert!(matches!(&*failed.borrow(), Some(Err(_))));
    assert_eq!(editor.document_path(sender.view), Some(PathBuf::from(&original)));
    assert!(w.kit.take_events().is_empty());

    let (completed, then) = outcome();
    editor.file_renamed_recovery(
        sender.view, PathBuf::from(&original).as_path(), &renamed, &receipt, &receiver,
        &destination, &[source_owner], &CancelToken::default(), &pending, then,
    );
    assert!(matches!(&*completed.borrow(), Some(Ok(()))));
    assert_eq!(editor.document_path(sender.view), Some(renamed));
    assert!(names(&w.kit.take_events()).contains(&(Target::View(sender.view), "workbook:renamed")));
}
