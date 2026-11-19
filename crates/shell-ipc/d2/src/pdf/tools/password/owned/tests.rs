use crate::kit::IntoIpc;
use crate::pdf::tools::test_support::*;
use serde_json::json;
use shell_contract::{
    IpcValue, Surface, ViewId,
    ipc::{NativeContext, NativeRequestOwner},
    runtime::CancelToken,
};
use std::{cell::RefCell, rc::Rc};

fn state() -> IpcValue {
    v(
        json!({"fileName":"fixture.pdf","retry":false,"busy":true,"lang":"ko","strings":{"title":"Password"}}),
    )
}
fn owner(t: &T, view: ViewId) -> NativeRequestOwner {
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    let renderer = t.rt.services.views.renderer_generation(view).unwrap();
    effects.capture_owner(view, &renderer).unwrap()
}
#[allow(clippy::type_complexity)]
fn start(
    t: &T,
    key: &str,
    source: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    answers: Rc<RefCell<Vec<Result<Option<String>, String>>>>,
) {
    t.tools().prompt_password_owned(
        None,
        state(),
        key.into(),
        source.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        Box::new(move |answer| answers.borrow_mut().push(answer)),
    );
}

#[test]
fn owned_prompt_requires_actual_owner_and_never_uses_legacy_open() {
    let t = tools(json!({}));
    let invalid = NativeRequestOwner {
        view: ViewId(1),
        context: NativeContext::new(()),
    };
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &invalid,
        &invalid,
        &CancelToken::default(),
        &CancelToken::default(),
        answers.clone(),
    );
    assert_eq!(answers.borrow().len(), 1);
    assert!(answers.borrow()[0].is_err());
    assert!(t.fake.called("open_password_window_owned").is_empty());
    assert!(t.fake.called("open_password_window").is_empty());
}

#[test]
fn actual_created_renderer_and_attempt_are_required_for_ack_and_retry_reuses_window() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &cancel,
        &pending,
        answers.clone(),
    );
    assert!(
        answers.borrow().is_empty(),
        "queued construction is not a created prompt"
    );
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let created = owner(&t, prompt.view);
    t.fake
        .resume_owned_password(Ok((prompt.view, created.clone())));
    t.rt.drain();
    let events = t.rt.events();
    assert_eq!(events.len(), 1);
    assert_eq!(j(&events[0].args[0])["generation"], "1");
    assert_eq!(j(&events[0].args[0])["busy"], false);
    let _ = t.rt.invoke_native(
        "pdf-password:submit",
        prompt,
        vec!["fixture".into_ipc(), "0".into_ipc()],
        created.context.clone(),
        CancelToken::default(),
    );
    let _ = t.rt.invoke_native(
        "pdf-password:submit",
        prompt,
        vec!["fixture".into_ipc(), "1".into_ipc()],
        source.context.clone(),
        CancelToken::default(),
    );
    t.call(
        "pdf-password:submit",
        prompt,
        vec!["fixture".into_ipc(), "1".into_ipc()],
    );
    assert!(
        answers.borrow().is_empty(),
        "foreign/native-less/stale ACKs are inert"
    );
    let _ = t.rt.invoke_native(
        "pdf-password:submit",
        prompt,
        vec!["fixture".into_ipc(), "1".into_ipc()],
        created.context.clone(),
        CancelToken::default(),
    );
    assert_eq!(&*answers.borrow(), &[Ok(Some("fixture".into()))]);
    assert!(!t.fake.owned_password_closed(0));
    assert_eq!(j(&t.rt.events()[0].args[0])["busy"], true);
    start(
        &t,
        "owned",
        &source,
        &destination,
        &cancel,
        &pending,
        answers.clone(),
    );
    t.rt.drain();
    assert_eq!(t.fake.called("open_password_window_owned").len(), 1);
    assert_eq!(j(&t.rt.events()[0].args[0])["generation"], "2");
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["1".into_ipc()],
        created.context.clone(),
        CancelToken::default(),
    );
    assert_eq!(answers.borrow().len(), 1);
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["2".into_ipc()],
        created.context,
        CancelToken::default(),
    );
    assert_eq!(&*answers.borrow(), &[Ok(Some("fixture".into())), Ok(None)]);
    assert!(t.fake.owned_password_closed(0));
    t.rt.destroy(prompt.view);
    assert_eq!(answers.borrow().len(), 2);
}

#[test]
fn every_original_owner_loss_closes_only_its_exact_prompt_once() {
    for loss in 0..4 {
        let mut t = tools(json!({}));
        t.pdf_view(1);
        t.pdf_view(2);
        let source = owner(&t, ViewId(1));
        let destination = owner(&t, ViewId(2));
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let answers = Rc::new(RefCell::new(Vec::new()));
        start(
            &t,
            "owned",
            &source,
            &destination,
            &cancel,
            &pending,
            answers.clone(),
        );
        let prompt = t.rt.view(901, Surface::ShellPdfPassword);
        let created = owner(&t, prompt.view);
        t.fake
            .resume_owned_password(Ok((prompt.view, created.clone())));
        t.rt.drain();
        t.rt.events();
        match loss {
            0 => t.rt.replace_renderer(source.view),
            1 => t.rt.replace_renderer(destination.view),
            2 => cancel.cancel(),
            _ => pending.cancel(),
        }
        t.rt.drain();
        assert!(t.fake.owned_password_closed(0));
        assert_eq!(answers.borrow().len(), 1);
        assert!(answers.borrow()[0].is_err());
        let _ = t.rt.invoke_native(
            "pdf-password:submit",
            prompt,
            vec!["fixture".into_ipc(), "1".into_ipc()],
            created.context,
            CancelToken::default(),
        );
        t.tools().close_password_dialog_owned("owned");
        assert_eq!(answers.borrow().len(), 1);
        assert!(t.rt.events().is_empty());
    }
}

#[test]
fn callback_captured_before_close_cannot_replace_or_close_new_prompt() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let old = Rc::new(RefCell::new(Vec::new()));
    let new = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "old",
        &source,
        &destination,
        &cancel,
        &pending,
        old.clone(),
    );
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let created = owner(&t, prompt.view);
    t.fake
        .resume_owned_password(Ok((prompt.view, created.clone())));
    t.tools().close_password_dialog_owned("old");
    assert_eq!(&*old.borrow(), &[Ok(None)]);
    assert!(t.fake.owned_password_closed(0));
    start(
        &t,
        "new",
        &source,
        &destination,
        &cancel,
        &pending,
        new.clone(),
    );
    t.rt.drain();
    assert!(t.rt.events().is_empty());
    assert!(!t.fake.owned_password_closed(1));
    t.fake
        .resume_owned_password(Ok((prompt.view, created.clone())));
    t.rt.drain();
    t.rt.events();
    t.tools().close_password_dialog_owned("old");
    assert!(!t.fake.owned_password_closed(1));
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["2".into_ipc()],
        created.context,
        CancelToken::default(),
    );
    assert_eq!(&*new.borrow(), &[Ok(None)]);
    assert!(t.fake.owned_password_closed(1));
    assert_eq!(old.borrow().len(), 1);
}

#[test]
fn same_prompt_key_cannot_upgrade_to_another_captured_request() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    t.pdf_view(3);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let foreign = owner(&t, ViewId(3));
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let first = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &cancel,
        &pending,
        first.clone(),
    );
    start(
        &t,
        "owned",
        &foreign,
        &destination,
        &cancel,
        &pending,
        rejected.clone(),
    );
    assert_eq!(rejected.borrow().len(), 1);
    assert!(rejected.borrow()[0].is_err());
    assert!(first.borrow().is_empty());
    assert!(!t.fake.owned_password_closed(0));
    assert_eq!(t.fake.called("open_password_window_owned").len(), 1);
    t.tools().close_password_dialog_owned("owned");
    t.rt.drain();
}

#[test]
fn native_constructor_failure_keeps_its_error_and_settles_once() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &CancelToken::default(),
        &CancelToken::default(),
        answers.clone(),
    );
    t.fake
        .resume_owned_password(Err("native constructor failed".into()));
    t.rt.drain();
    assert_eq!(
        &*answers.borrow(),
        &[Err("native constructor failed".into())]
    );
    assert!(t.fake.owned_password_closed(0));
    t.tools().close_password_dialog_owned("owned");
    assert_eq!(answers.borrow().len(), 1);
}

#[test]
fn close_before_native_completion_cancels_the_original_attempt_once() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &CancelToken::default(),
        &CancelToken::default(),
        answers.clone(),
    );
    t.tools().close_password_dialog_owned("owned");
    t.rt.drain();
    assert!(t.fake.owned_password_closed(0));
    assert_eq!(&*answers.borrow(), &[Ok(None)]);
    assert!(t.rt.events().is_empty());
    assert!(
        !t.fake
            .resume_owned_password(Err("late native callback".into()))
    );
    t.tools().close_password_dialog_owned("owned");
    t.rt.drain();
    assert_eq!(answers.borrow().len(), 1);
}

#[test]
fn document_revoked_after_native_capture_before_core_delivery_cannot_publish() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let mut destination = owner(&t, ViewId(2));
    let document = shell_contract::runtime::RendererGeneration::new();
    destination.context = destination.context.with_document_lease(document.clone());
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &CancelToken::default(),
        &CancelToken::default(),
        answers.clone(),
    );
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let created = owner(&t, prompt.view);
    t.fake.resume_owned_password(Ok((prompt.view, created)));
    document.revoke();
    t.rt.drain();
    assert_eq!(answers.borrow().len(), 1);
    assert!(answers.borrow()[0].is_err());
    assert!(t.fake.owned_password_closed(0));
    assert!(t.rt.events().is_empty());
}

#[test]
fn same_renderer_foreign_document_ack_cannot_settle_the_prompt() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let answers = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "owned",
        &source,
        &destination,
        &CancelToken::default(),
        &CancelToken::default(),
        answers.clone(),
    );
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let mut created = owner(&t, prompt.view);
    let foreign_context = created
        .context
        .clone()
        .with_document_lease(shell_contract::runtime::RendererGeneration::new());
    created.context = created
        .context
        .with_document_lease(shell_contract::runtime::RendererGeneration::new());
    t.fake
        .resume_owned_password(Ok((prompt.view, created.clone())));
    t.rt.drain();
    t.rt.events();
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["1".into_ipc()],
        foreign_context,
        CancelToken::default(),
    );
    assert!(answers.borrow().is_empty());
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["1".into_ipc()],
        created.context,
        CancelToken::default(),
    );
    assert_eq!(&*answers.borrow(), &[Ok(None)]);
    assert!(t.fake.owned_password_closed(0));
}

#[test]
fn replaced_prompt_renderer_cannot_ack_a_successor_with_the_same_view_id() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let destination = owner(&t, ViewId(2));
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let old = Rc::new(RefCell::new(Vec::new()));
    let new = Rc::new(RefCell::new(Vec::new()));
    start(
        &t,
        "old",
        &source,
        &destination,
        &cancel,
        &pending,
        old.clone(),
    );
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let previous = owner(&t, prompt.view);
    t.fake
        .resume_owned_password(Ok((prompt.view, previous.clone())));
    t.rt.drain();
    t.rt.events();
    t.rt.replace_renderer(prompt.view);
    t.rt.drain();
    assert_eq!(old.borrow().len(), 1);
    assert!(old.borrow()[0].is_err());
    assert!(t.fake.owned_password_closed(0));
    start(
        &t,
        "new",
        &source,
        &destination,
        &cancel,
        &pending,
        new.clone(),
    );
    let current = owner(&t, prompt.view);
    t.fake
        .resume_owned_password(Ok((prompt.view, current.clone())));
    t.rt.drain();
    t.rt.events();
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["2".into_ipc()],
        previous.context,
        CancelToken::default(),
    );
    assert!(new.borrow().is_empty());
    assert!(!t.fake.owned_password_closed(1));
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["2".into_ipc()],
        current.context,
        CancelToken::default(),
    );
    assert_eq!(&*new.borrow(), &[Ok(None)]);
    assert!(t.fake.owned_password_closed(1));
}

fn recovery_target(
    t: &T,
    source: &NativeRequestOwner,
    cancel: &CancelToken,
) -> shell_contract::ipc::RecoveryOpenTarget {
    let created = t.rt.services.views.renderer_generation(ViewId(2)).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(2), created.clone());
    shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: source.clone(),
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: CancelToken::default(),
        },
        ViewId(2),
        created,
        closure,
        browser_owner,
    )
    .unwrap()
}

#[test]
fn prepared_recovery_uses_the_real_password_window_and_retries_its_same_binding() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let target = recovery_target(&t, &source, &CancelToken::default());
    let snapshot = target.authority_snapshot().unwrap();
    let answers = Rc::new(RefCell::new(Vec::new()));
    let first = answers.clone();
    t.tools().prompt_password_prepared(
        None,
        state(),
        "recovery".into(),
        target.clone(),
        snapshot.clone(),
        Box::new(move |answer| first.borrow_mut().push(answer)),
    );
    assert_eq!(t.fake.called("open_password_window_prepared").len(), 1);
    assert!(t.fake.called("open_password_window").is_empty());
    let prompt = t.rt.view(901, Surface::ShellPdfPassword);
    let dialog = owner(&t, prompt.view);
    t.fake
        .resume_owned_password(Ok((prompt.view, dialog.clone())));
    t.rt.drain();
    t.rt.events();
    let _ = t.rt.invoke_native(
        "pdf-password:submit",
        prompt,
        vec!["wrong".into_ipc(), "1".into_ipc()],
        dialog.context.clone(),
        CancelToken::default(),
    );
    assert_eq!(&*answers.borrow(), &[Ok(Some("wrong".into()))]);
    let second = answers.clone();
    t.tools().prompt_password_prepared(
        None,
        state(),
        "recovery".into(),
        target,
        snapshot,
        Box::new(move |answer| second.borrow_mut().push(answer)),
    );
    assert_eq!(t.fake.called("open_password_window_prepared").len(), 1);
    let _ = t.rt.invoke_native(
        "pdf-password:cancel",
        prompt,
        vec!["2".into_ipc()],
        dialog.context,
        CancelToken::default(),
    );
    assert_eq!(&*answers.borrow(), &[Ok(Some("wrong".into())), Ok(None)]);
    assert!(t.fake.owned_password_closed(0));
}

#[test]
fn prepared_retry_rejects_another_binding_even_with_the_same_view_and_generation() {
    let mut t = tools(json!({}));
    t.pdf_view(1);
    t.pdf_view(2);
    let source = owner(&t, ViewId(1));
    let cancel = CancelToken::default();
    let original = recovery_target(&t, &source, &cancel);
    let foreign = recovery_target(&t, &source, &cancel);
    let original_snapshot = original.authority_snapshot().unwrap();
    let foreign_snapshot = foreign.authority_snapshot().unwrap();
    let first = Rc::new(RefCell::new(Vec::new()));
    let accepted = first.clone();
    t.tools().prompt_password_prepared(
        None,
        state(),
        "recovery".into(),
        original,
        original_snapshot,
        Box::new(move |answer| accepted.borrow_mut().push(answer)),
    );
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let result = rejected.clone();
    t.tools().prompt_password_prepared(
        None,
        state(),
        "recovery".into(),
        foreign,
        foreign_snapshot,
        Box::new(move |answer| result.borrow_mut().push(answer)),
    );
    assert!(rejected.borrow()[0].is_err());
    assert!(first.borrow().is_empty());
    assert!(!t.fake.owned_password_closed(0));
    cancel.cancel();
    t.rt.drain();
    assert_eq!(first.borrow().len(), 1);
    assert!(first.borrow()[0].is_err());
    assert!(t.fake.owned_password_closed(0));
}
