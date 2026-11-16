//! Contract-state regressions; these do not authenticate a CEF renderer.
use super::*;
use crate::ipc::NativeContext;

fn operation(cancel: CancelToken, shutdown: CancelToken) -> NativeInvocationOperation {
    NativeInvocationIssuer::new(shutdown).issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Docs,
            input: PathBuf::from("input.docx"),
            output: PathBuf::from("output.pdf"),
            format: "pdf".into(),
            page: None,
            ops: None,
            batch: None,
        },
        cancel,
    )
}

fn synthetic_owner(view: ViewId, created: &RendererGeneration) -> NativeRequestOwner {
    NativeRequestOwner {
        view,
        context: NativeContext::with_owner_loss((), created.clone(), CancelToken::default()),
    }
}

fn register_synthetic_state(
    target: &NativeInvocationTarget,
    owner: NativeRequestOwner,
) -> Result<(), String> {
    target
        .with_active(|| target.register_from_native_admitted(owner))
        .ok_or_else(|| "The synthetic invocation was revoked.".to_owned())?
}
#[test]
fn headless_cleanup_retains_only_the_original_reservation_after_cancellation() {
    let cancel = CancelToken::default();
    let cleanup_shutdown = CancelToken::default();
    let operation = operation(cancel.clone(), CancelToken::default());
    let created = RendererGeneration::new();
    let target = operation
        .reserve_target(ViewId(99), created.clone())
        .unwrap();
    let (_issuer, reservation, owner) =
        crate::ipc::NativeClosureIssuer::new_with_owner(target.view(), created);
    let cleanup =
        NativeHeadlessCleanup::capture(&target, &reservation, &owner, cleanup_shutdown.clone())
            .unwrap();
    let (_other_issuer, other, other_owner) =
        crate::ipc::NativeClosureIssuer::new_with_owner(ViewId(100), RendererGeneration::new());
    assert!(
        NativeHeadlessCleanup::capture(&target, &other, &other_owner, cleanup_shutdown).is_err()
    );
    cancel.cancel();
    operation.finish();
    assert!(
        NativeHeadlessCleanup::capture(&target, &reservation, &owner, CancelToken::default())
            .is_err()
    );
    let claim = cleanup
        .claim_close(|transition| {
            assert!(transition.matches_owner(&owner));
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        claim,
        crate::ipc::NativeBrowserCloseClaim::Won { .. }
    ));
    assert!(cleanup.claim_close(|_| Ok(())).is_err());
}
#[test]
fn headless_cleanup_stops_at_process_cleanup_shutdown() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let target = operation
        .reserve_target(ViewId(101), RendererGeneration::new())
        .unwrap();
    let (_issuer, reservation, owner) =
        crate::ipc::NativeClosureIssuer::new_with_owner(target.view(), target.created().clone());
    let shutdown = CancelToken::default();
    let cleanup =
        NativeHeadlessCleanup::capture(&target, &reservation, &owner, shutdown.clone()).unwrap();
    shutdown.cancel();
    let mut called = false;
    assert!(
        cleanup
            .claim_close(|_| {
                called = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!called);
}
#[test]
fn headless_cleanup_reports_only_the_actual_successor() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let target = operation
        .reserve_target(ViewId(102), RendererGeneration::new())
        .unwrap();
    let (_issuer, reservation, owner) =
        crate::ipc::NativeClosureIssuer::new_with_owner(target.view(), target.created().clone());
    let cleanup =
        NativeHeadlessCleanup::capture(&target, &reservation, &owner, CancelToken::default())
            .unwrap();
    reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
    let replacement = reservation.prepare_replacement(&owner).unwrap();
    let (successor, _, ()) = replacement.commit(|_, _| Ok(())).unwrap();
    let mut retired = false;
    let result = cleanup
        .claim_close(|_| {
            retired = true;
            Ok(())
        })
        .unwrap();
    match result {
        crate::ipc::NativeBrowserCloseClaim::Superseded(proof) => {
            assert!(proof.matches(&reservation, &owner));
            assert!(proof.successor_matches(&successor));
        }
        crate::ipc::NativeBrowserCloseClaim::Won { .. } => {
            panic!("replacement browser was claimed")
        }
    }
    assert!(!retired);
}

#[test]
fn existing_save_binds_exact_document_without_a_constructor() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let renderer = RendererGeneration::new();
    let document = RendererGeneration::new();
    let owner = NativeRequestOwner {
        view: ViewId(31),
        context: NativeContext::with_owner_loss((), renderer.clone(), CancelToken::default())
            .with_document_lease(document.clone()),
    };
    assert!(
        issuer
            .issue_existing_document_save(
                Surface::Docs,
                ViewId(32),
                NativeSaveSelection::ExactPath("a.docx".into()),
                owner.clone(),
                CancelToken::default()
            )
            .is_err()
    );
    assert!(
        issuer
            .issue_existing_document_save(
                Surface::Docs,
                ViewId(31),
                NativeSaveSelection::ExactPath("a.docx".into()),
                synthetic_owner(ViewId(31), &renderer),
                CancelToken::default()
            )
            .is_err()
    );
    let binding = issuer
        .issue_existing_document_save(
            Surface::Docs,
            ViewId(31),
            NativeSaveSelection::ExactPath("a.docx".into()),
            owner,
            CancelToken::default(),
        )
        .unwrap();
    assert_eq!(
        binding.operation().selection(),
        &NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
            surface: Surface::Docs,
            view: ViewId(31),
            selection: NativeSaveSelection::ExactPath("a.docx".into())
        })
    );
    assert!(
        binding
            .owner()
            .context
            .renderer()
            .unwrap()
            .same_generation(&renderer)
    );
    assert!(
        binding
            .owner()
            .context
            .document_lease()
            .unwrap()
            .same_generation(&document)
    );
    let destination = NativeSaveDestination::from_exact_selection(binding.clone()).unwrap();
    assert!(destination.matches_binding(&binding));
    assert_eq!(destination.path(), std::path::Path::new("a.docx"));
    assert!(
        NativeSaveDestination::from_native_save_dialog(binding.clone(), "other.docx".into())
            .is_err()
    );
    assert!(
        binding
            .operation()
            .reserve_target(ViewId(31), renderer)
            .is_err()
    );
}

#[test]
fn native_save_revision_successor_retires_every_view_and_preserves_selection() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let old = PdfDocumentLease::new();
    let next = PdfDocumentLease::new();
    let other_old = PdfDocumentLease::new();
    let other_next = PdfDocumentLease::new();
    let renderer = RendererGeneration::new();
    let mut owner = synthetic_owner(ViewId(71), &renderer);
    owner.context = owner
        .context
        .with_document_lease(RendererGeneration::new())
        .with_pdf_document(PdfDocumentLease::new())
        .with_pdf_revision(old.clone());
    let binding = issuer
        .issue_existing_document_save(
            Surface::Pdf,
            owner.view,
            NativeSaveSelection::ExactPath("saved.pdf".into()),
            owner.clone(),
            CancelToken::default(),
        )
        .unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let callback = owner.clone();
    let authority = NativeSaveRevisionAuthority::new(selection, vec![]);
    let old_snapshot = authority.snapshot().unwrap();
    let mut prepared = old_snapshot
        .prepare_with_request_aliases(
            vec![
                (old.clone(), next.clone()),
                (other_old.clone(), other_next.clone()),
            ],
            std::slice::from_ref(&callback),
        )
        .unwrap();
    assert!(prepared.renewed_request_owner(&callback).is_none());
    assert!(prepared.receipt().is_none());
    let mut gate = crate::ipc::PdfDocumentGatePlan::new(&[&old, &next, &other_old, &other_next]);
    let mut retirement = gate
        .prepare_revision_set(&[(&old, &next), (&other_old, &other_next)])
        .unwrap();
    let result = gate
        .with_live_revision_set(&mut retirement, |permit| prepared.commit(permit))
        .unwrap()
        .unwrap();
    assert!(!old.is_live() && !other_old.is_live());
    assert!(next.is_live() && other_next.is_live());
    assert!(!old.loss_token().is_cancelled() && !old_snapshot.loss_token().is_cancelled());
    assert_eq!(result.selection().path(), std::path::Path::new("saved.pdf"));
    assert!(
        result
            .selection()
            .binding()
            .owner()
            .pdf_revision()
            .unwrap()
            .same_document(&next)
    );
    assert!(result.aliases().is_empty());
    assert!(
        prepared
            .renewed_request_owner(&callback)
            .unwrap()
            .pdf_revision()
            .unwrap()
            .same_document(&next)
    );
    let receipt = prepared.receipt().unwrap();
    assert!(receipt.succeeds(&old_snapshot));
    assert!(!receipt.succeeds(&result));
    assert!(receipt.snapshot().same_snapshot(&result));
    assert!(receipt.snapshot().with_current(|| ()).is_some());
    assert!(
        receipt
            .renewed_request_owner(&callback)
            .unwrap()
            .pdf_revision()
            .unwrap()
            .same_document(&next)
    );
    assert!(
        receipt
            .renewed_owner(&owner)
            .unwrap()
            .pdf_revision()
            .unwrap()
            .same_document(&next)
    );
    let foreign = synthetic_owner(ViewId(71), &renderer);
    assert!(receipt.renewed_request_owner(&foreign).is_none());
    prepared.notify_replaced();
    drop(retirement);
    let mut selection_change = receipt
        .snapshot()
        .prepare_selection(receipt.snapshot().selection().clone())
        .unwrap();
    let permit = selection_change.admit();
    selection_change.commit(&permit).unwrap();
    assert!(receipt.snapshot().with_current(|| ()).is_none());
    assert!(
        receipt
            .renewed_request_owner(&callback)
            .unwrap()
            .pdf_revision()
            .unwrap()
            .same_document(&next)
    );
    selection_change.notify_replaced();
    assert!(old.loss_token().is_cancelled() && old_snapshot.loss_token().is_cancelled());
}

#[test]
fn native_reload_receipt_requires_the_exact_accepted_lineage() {
    use crate::hosts::{AcceptedPdfReload, CommittedPdfReload, NativeSaveReloadHandoff};
    use crate::ipc::{
        NativeCommittedOutput, NativeSavedInput, NativeSavedPath, SavedPublication,
        native_saved_target::NativeOutputScope,
    };
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    let dir = std::env::temp_dir().join(format!(
        "native-reload-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    {
        let path = dir.join("out.pdf");
        let view = ViewId(42);
        let predecessor_generation = RendererGeneration::new();
        let mut predecessor = synthetic_owner(view, &predecessor_generation);
        predecessor.context = predecessor
            .context
            .with_document_lease(RendererGeneration::new())
            .with_pdf_document(PdfDocumentLease::new())
            .with_pdf_revision(PdfDocumentLease::new());
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let binding = issuer
            .issue_existing_document_save(
                Surface::Pdf,
                view,
                NativeSaveSelection::ExactPath(path.clone()),
                predecessor.clone(),
                CancelToken::default(),
            )
            .unwrap();
        let selected = NativeSaveDestination::from_exact_selection(binding).unwrap();
        let parent = file_commit::open_output_parent(&dir).unwrap();
        let write = file_commit::prepare_direct(&parent, &path)
            .unwrap()
            .open()
            .unwrap();
        let snapshot = match write.write_owned(b"synthetic pdf".to_vec()) {
            file_commit::DirectOwnedWriteAttempt::Completed(
                file_commit::DirectOwnedWriteOutcome::Committed(snapshot),
            ) => snapshot,
            _ => panic!("synthetic direct write lacked a receipt"),
        };
        let scope = Arc::new(NativeOutputScope {
            participants: vec![],
            cancel: CancelToken::default(),
            pending: CancelToken::default(),
            additional: vec![],
        });
        let publication = SavedPublication::Native(
            NativeCommittedOutput::from_publication_admitted(
                selected,
                snapshot.receipt().clone(),
                scope,
            )
            .unwrap(),
        );
        let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot).unwrap();
        let saved_path = NativeSavedPath::Selected(publication);
        let reserved = RendererGeneration::new();
        let marker = AcceptedPdfReload::from_native_admission(
            predecessor.clone(),
            reserved.clone(),
            CancelToken::default(),
            CancelToken::default(),
            Arc::new(|_| true),
        );
        let foreign = AcceptedPdfReload::from_native_admission(
            predecessor.clone(),
            reserved.clone(),
            CancelToken::default(),
            CancelToken::default(),
            Arc::new(|_| true),
        );
        let handoff = || {
            NativeSaveReloadHandoff::from_native_admission(
                input.clone(),
                saved_path.clone(),
                marker.clone(),
                vec![predecessor.clone()],
                CancelToken::default(),
                CancelToken::default(),
                vec![],
            )
            .unwrap()
        };
        let mut successor = synthetic_owner(view, &reserved);
        successor.context = successor
            .context
            .with_pdf_document(PdfDocumentLease::new())
            .with_pdf_revision(PdfDocumentLease::new());
        let wrong =
            CommittedPdfReload::from_completed_native_load(successor.clone(), foreign).unwrap();
        assert!(handoff().complete(wrong).is_err());
        assert!(
            handoff()
                .completion_participants(&synthetic_owner(view, &RendererGeneration::new()))
                .is_err()
        );
        let without_gate =
            CommittedPdfReload::from_completed_native_load(successor.clone(), marker.clone())
                .unwrap();
        assert!(handoff().complete(without_gate).is_err());
        let accepted = handoff();
        let old_slot = predecessor.pdf_document().unwrap();
        let old_revision = predecessor.pdf_revision().unwrap();
        let new_slot = successor.pdf_document().unwrap();
        let new_revision = successor.pdf_revision().unwrap();
        let mut gate =
            crate::ipc::PdfDocumentGatePlan::new(&[old_slot, old_revision, new_slot, new_revision]);
        let mut retirement = gate
            .prepare_reload_retirement(&predecessor, &successor)
            .unwrap();
        gate.with_live_reloading(&mut retirement, |permit| {
            permit.retire().unwrap();
            accepted
                .record_completion_admitted(&successor, permit)
                .unwrap();
        })
        .unwrap();
        drop(retirement);
        let completed =
            CommittedPdfReload::from_completed_native_load(successor.clone(), marker.clone())
                .unwrap();
        let receipt = accepted.complete(completed).unwrap();
        assert!(
            receipt
                .successor()
                .context
                .renderer()
                .unwrap()
                .same_generation(&reserved)
        );
        assert!(
            receipt.participants().iter().all(|owner| owner
                .context
                .renderer()
                .unwrap()
                .same_generation(&reserved))
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn selected_save_normalization_is_one_hop_and_surface_bound() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let make = |surface, name: &str| {
        let generation = RendererGeneration::new();
        let mut owner = synthetic_owner(ViewId(31), &generation);
        owner.context = owner.context.with_document_lease(RendererGeneration::new());
        let binding = issuer
            .issue_existing_document_save(
                surface,
                ViewId(31),
                NativeSaveSelection::Mode(crate::editors::SaveMode::SaveAs),
                owner,
                CancelToken::default(),
            )
            .unwrap();
        NativeSaveDestination::from_native_save_dialog(binding, name.into()).unwrap()
    };
    let html = make(Surface::Html, "folder/page")
        .normalize_dialog_format(NativeSaveFormat::Html)
        .unwrap();
    assert_eq!(html.raw_path(), std::path::Path::new("folder/page"));
    assert_eq!(html.path(), std::path::Path::new("folder/page.html"));
    assert!(
        html.clone()
            .normalize_dialog_format(NativeSaveFormat::Html)
            .is_err()
    );
    assert!(
        make(Surface::Html, "page")
            .normalize_dialog_format(NativeSaveFormat::Markdown)
            .is_err()
    );
    assert_eq!(
        make(Surface::Html, "page.")
            .normalize_dialog_format(NativeSaveFormat::Html)
            .unwrap()
            .path(),
        std::path::Path::new("page.")
    );
    assert_eq!(
        make(Surface::Diagram, "diagram.SVG")
            .normalize_dialog_format(NativeSaveFormat::DiagramSvg)
            .unwrap()
            .path(),
        std::path::Path::new("diagram.drawio.svg")
    );
    assert_eq!(
        make(Surface::Diagram, "diagram.DIO")
            .normalize_dialog_format(NativeSaveFormat::DiagramSvg)
            .unwrap()
            .path(),
        std::path::Path::new("diagram.DIO")
    );
    assert_eq!(
        make(Surface::Mindmap, "map.PDF")
            .normalize_dialog_format(NativeSaveFormat::Mindmap)
            .unwrap()
            .path(),
        std::path::Path::new("map.PDF")
    );
    assert_eq!(
        make(Surface::Epub, "book.txt")
            .normalize_dialog_format(NativeSaveFormat::Epub)
            .unwrap()
            .path(),
        std::path::Path::new("book.txt.epub")
    );
    assert_eq!(
        make(Surface::Epub, ".epub")
            .normalize_dialog_format(NativeSaveFormat::Epub)
            .unwrap()
            .path(),
        std::path::Path::new(".epub.epub")
    );
    let slides = make(Surface::Slides, "slides.PNG")
        .normalize_dialog_format(NativeSaveFormat::Slides)
        .unwrap();
    assert_eq!(
        slides.numbered_png_page(1, 0).unwrap().path(),
        std::path::Path::new("slides.PNG")
    );
    assert_eq!(
        slides.numbered_png_page(12, 0).unwrap().path(),
        std::path::Path::new("slides-01.PNG")
    );
    assert_eq!(
        slides.numbered_png_page(100, 99).unwrap().path(),
        std::path::Path::new("slides-100.PNG")
    );
    assert!(slides.numbered_png_page(2, 2).is_err());
    let sheets = |name: &str, macro_enabled, offer_copies| {
        make(Surface::Sheets, name)
            .normalize_dialog_format(NativeSaveFormat::Sheets {
                macro_enabled,
                offer_copies,
            })
            .unwrap()
            .path()
            .to_owned()
    };
    let path = std::path::PathBuf::from;
    assert_eq!(sheets("dir/report.v2", false, true), path("dir/report.v2.xlsx"));
    assert_eq!(sheets("dir/book", false, false), path("dir/book.xlsx"));
    assert_eq!(sheets("dir/book", false, true), path("dir/book"));
    assert_eq!(sheets("book.XLSX", false, true), path("book.XLSX"));
    assert_eq!(sheets("book.xlsx", true, true), path("book.xlsx.xlsm"));
    assert_eq!(sheets("book.csv", false, false), path("book.csv"));
    assert_eq!(sheets("book.csv", true, false), path("book.csv.xlsm"));
    assert_eq!(sheets("book.PDF", false, true), path("book.PDF"));
    assert_eq!(sheets("book.pdf", false, false), path("book.pdf.xlsx"));
    assert!(
        make(Surface::Docs, "book")
            .normalize_dialog_format(NativeSaveFormat::Sheets {
                macro_enabled: false,
                offer_copies: false,
            })
            .is_err()
    );
    let hwp = |name: &str, fallback_hwpx, offer_pdf| {
        make(Surface::Hwp, name)
            .normalize_dialog_format(NativeSaveFormat::Hwp {
                fallback_hwpx,
                offer_pdf,
            })
            .unwrap()
            .path()
            .to_owned()
    };
    assert_eq!(hwp("dir/letter", false, true), path("dir/letter.hwp"));
    assert_eq!(hwp("dir/letter", true, false), path("dir/letter.hwpx"));
    assert_eq!(hwp("letter.HWPX", false, true), path("letter.HWPX"));
    assert_eq!(hwp("letter.pdf", false, true), path("letter.pdf"));
    assert_eq!(hwp("letter.pdf", false, false), path("letter.pdf.hwp"));
    assert!(
        make(Surface::Docs, "letter")
            .normalize_dialog_format(NativeSaveFormat::Hwp {
                fallback_hwpx: false,
                offer_pdf: false,
            })
            .is_err()
    );
}

#[test]
fn project_document_destination_names_only_a_plain_save() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = |mode| {
        let generation = RendererGeneration::new();
        let mut owner = synthetic_owner(ViewId(32), &generation);
        owner.context = owner.context.with_document_lease(RendererGeneration::new());
        issuer
            .issue_existing_document_save(
                Surface::Docs,
                ViewId(32),
                NativeSaveSelection::Mode(mode),
                owner,
                CancelToken::default(),
            )
            .unwrap()
    };
    let saved = NativeSaveDestination::from_project_document(
        binding(crate::editors::SaveMode::Save),
        "project/new.docx".into(),
    )
    .unwrap();
    assert_eq!(saved.path(), std::path::Path::new("project/new.docx"));
    assert!(
        NativeSaveDestination::from_project_document(
            binding(crate::editors::SaveMode::SaveAs),
            "project/new.docx".into(),
        )
        .is_err()
    );
    assert!(
        NativeSaveDestination::from_project_document(
            binding(crate::editors::SaveMode::Save),
            "".into(),
        )
        .is_err()
    );
}

#[test]
fn startup_command_recovery_issuer_seals_all_three_selections() {
    let shutdown = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown);
    let owner = synthetic_owner(ViewId(1), &RendererGeneration::new());
    let startup = issuer.issue_startup(vec![], false, owner.clone(), CancelToken::default());
    let other = issuer.issue_startup(vec![], false, owner, CancelToken::default());
    let command = issuer
        .issue_command_child(
            &startup,
            NativeEditorCommand::OpenFile {
                surface: Surface::Docs,
                path: "selected.docx".into(),
            },
            CancelToken::default(),
        )
        .unwrap();
    let selection = NativeInvocationSelection::RecoveryBlank {
        surface: Surface::Docs,
        title: "Recovery".into(),
    };
    let child = issuer
        .issue_startup_command_recovery_child(
            &startup,
            &command,
            selection.clone(),
            CancelToken::default(),
        )
        .unwrap();
    assert!(command.is_child_of(&startup));
    assert!(child.is_child_of(&command));
    assert_eq!(child.selection(), &selection);
    assert!(
        issuer
            .issue_command_recovery_child(&command, selection.clone(), CancelToken::default())
            .is_err()
    );
    assert!(
        issuer
            .issue_startup_command_recovery_child(
                &other,
                &command,
                selection,
                CancelToken::default()
            )
            .is_err()
    );
    let wrong_surface = NativeInvocationSelection::RecoveryBlank {
        surface: Surface::Sheets,
        title: "Recovery".into(),
    };
    assert!(
        issuer
            .issue_startup_command_recovery_child(
                &startup,
                &command,
                wrong_surface,
                CancelToken::default()
            )
            .is_err()
    );
}

#[test]
fn one_operation_cannot_reserve_a_second_constructor() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let first = RendererGeneration::new();
    operation.reserve_target(ViewId(1), first.clone()).unwrap();
    assert!(
        operation
            .reserve_target(ViewId(2), RendererGeneration::new())
            .is_err()
    );
    assert!(operation.reserved_target(ViewId(2)).is_err());
    assert!(
        operation
            .reserved_target(ViewId(1))
            .unwrap()
            .created()
            .same_generation(&first)
    );
}

#[test]
fn recycled_view_id_cannot_replace_the_reserved_generation() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let first = RendererGeneration::new();
    let target = operation.reserve_target(ViewId(7), first.clone()).unwrap();
    assert!(
        register_synthetic_state(
            &target,
            synthetic_owner(ViewId(7), &RendererGeneration::new())
        )
        .is_err()
    );
    assert!(target.registered_document().is_err());
    register_synthetic_state(&target, synthetic_owner(ViewId(7), &first)).unwrap();
    assert!(register_synthetic_state(&target, synthetic_owner(ViewId(7), &first)).is_err());
}

#[test]
fn reading_the_registered_document_does_not_consume_its_lifetime() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let created = RendererGeneration::new();
    let target = operation
        .reserve_target(ViewId(8), created.clone())
        .unwrap();
    let owner = synthetic_owner(ViewId(8), &created);
    register_synthetic_state(&target, owner.clone()).unwrap();
    let first = target.registered_document().unwrap();
    let second = target.registered_document().unwrap();
    assert!(Arc::ptr_eq(
        &first.owner().context.as_arc(),
        &owner.context.as_arc()
    ));
    assert!(Arc::ptr_eq(
        &second.owner().context.as_arc(),
        &owner.context.as_arc()
    ));
    assert_eq!(
        first.target().with_active(|| first.owner().view),
        Some(ViewId(8))
    );
    operation.finish();
    assert!(first.operation().loss_token().is_cancelled());
    assert!(first.target().with_active(|| ()).is_none());
    assert!(second.target().registered_document().is_err());
}

#[test]
fn cancelling_either_native_input_or_shutdown_refuses_new_work() {
    for cancel_input in [true, false] {
        let cancel = CancelToken::default();
        let shutdown = CancelToken::default();
        let operation = operation(cancel.clone(), shutdown.clone());
        let target = operation
            .reserve_target(ViewId(9), RendererGeneration::new())
            .unwrap();
        if cancel_input {
            cancel.cancel();
        } else {
            shutdown.cancel();
        }
        assert!(operation.with_active(|| ()).is_none());
        assert!(target.with_active(|| ()).is_none());
        assert!(operation.reserved_target(ViewId(9)).is_err());
    }
}

#[test]
fn finish_revokes_only_its_invocation_not_shared_input_tokens() {
    let cancel = CancelToken::default();
    let shutdown = CancelToken::default();
    let first = operation(cancel.clone(), shutdown.clone());
    let second = operation(cancel.clone(), shutdown.clone());
    first.finish();
    assert!(!cancel.is_cancelled());
    assert!(!shutdown.is_cancelled());
    assert!(first.with_active(|| ()).is_none());
    assert_eq!(second.with_active(|| 1), Some(1));
}

#[test]
fn accepted_constructor_work_runs_after_its_admission_has_ended() {
    let operation = operation(CancelToken::default(), CancelToken::default());
    let selected = operation.with_active(|| || 42).unwrap();
    operation.finish();
    assert_eq!(selected(), 42);
    assert!(operation.with_active(|| || 43).is_none());
}

fn recovery_target(view: u32) -> RecoveryOpenTarget {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let operation = issuer.issue(
        NativeInvocationSelection::RecoveryBlank {
            surface: Surface::Docs,
            title: "Recovered".into(),
        },
        CancelToken::default(),
    );
    let created = RendererGeneration::new();
    let (_, closure, owner) =
        super::super::NativeClosureIssuer::new_with_owner(ViewId(view), created.clone());
    RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        ViewId(view),
        created,
        closure,
        owner,
    )
    .unwrap()
}

#[test]
fn recovery_seed_receipt_requires_successful_enqueue_and_exact_binding() {
    let target = recovery_target(10);
    assert!(
        target
            .commit_seed_from_admitted(|| Err("queue rejected".into()))
            .is_err()
    );
    assert!(target.committed_seed().is_none());
    let receipt = target.commit_seed_from_admitted(|| Ok(())).unwrap();
    assert!(receipt.matches(&target));
    assert!(!receipt.matches(&recovery_target(10)));
    let mut enqueued_again = false;
    assert!(
        target
            .commit_seed_from_admitted(|| {
                enqueued_again = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!enqueued_again);
}

#[test]
fn poisoned_recovery_state_does_not_grant_unregistered_or_committed_status() {
    let target = recovery_target(11);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _registered = target.0.registered.lock().unwrap();
        let _seed = target.0.seed_committed.lock().unwrap();
        panic!("state mutation failed");
    }));
    assert!(!target.is_unregistered());
    assert!(!target.is_seed_committed());
    assert!(target.commit_seed_from_admitted(|| Ok(())).is_err());
}

#[test]
fn recovery_settlement_notifies_once_only_after_explicit_gate_exit() {
    let target = recovery_target(12);
    let receipt = target.commit_seed_from_admitted(|| Ok(())).unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let bound = target.clone();
    let _subscription = receipt.on_settled(move |result| {
        assert!(bound.committed_seed().is_some());
        sink.lock().unwrap().push(result);
    });
    let deferred = receipt.settle_from_admitted(Ok(())).unwrap();
    assert!(receipt.settlement().is_none());
    assert!(received.lock().unwrap().is_empty());
    deferred.notify();
    assert_eq!(*received.lock().unwrap(), vec![Ok(())]);
    assert!(receipt.fail_after_loss("late loss".into()).is_err());
    let late = Arc::new(Mutex::new(None));
    let sink = late.clone();
    let _late_subscription = receipt.on_settled(move |result| *sink.lock().unwrap() = Some(result));
    assert_eq!(*late.lock().unwrap(), Some(Ok(())));
}

#[test]
fn recovery_loss_settlement_cannot_be_replaced_by_late_success() {
    let receipt = recovery_target(13)
        .commit_seed_from_admitted(|| Ok(()))
        .unwrap();
    let deferred = receipt.fail_after_loss("target closed".into()).unwrap();
    assert!(receipt.settle_from_admitted(Ok(())).is_err());
    deferred.notify();
    assert_eq!(receipt.settlement(), Some(Err("target closed".into())));
}

#[test]
fn recovery_drain_waits_for_worker_and_core_continuation_without_inline_callbacks() {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Core(Mutex<VecDeque<Box<dyn FnOnce() + Send>>>);
    impl crate::runtime::Thread for Core {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            self.0.lock().unwrap().push_back(job);
        }
        fn is_current(&self) -> bool {
            true
        }
    }
    let target = recovery_target(18);
    let preparation = target.begin_preparation_work().unwrap();
    let receipt = target.commit_seed_from_admitted(|| Ok(())).unwrap();
    let worker = receipt.begin_work().unwrap();
    let continuation = worker.clone();
    let second_worker = receipt.begin_work().unwrap();
    let core = Arc::new(Core::default());
    let fired = Arc::new(AtomicUsize::new(0));
    let observed = fired.clone();
    let reentrant = receipt.clone();
    target
        .drain_work(
            core.clone(),
            Box::new(move || {
                assert!(reentrant.begin_work().is_err());
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
    assert!(receipt.begin_work().is_err());
    assert!(target.begin_preparation_work().is_err());
    drop(worker);
    drop(second_worker);
    drop(preparation);
    assert!(core.0.lock().unwrap().is_empty());
    drop(continuation);
    assert_eq!(fired.load(Ordering::SeqCst), 0);
    let done = core.0.lock().unwrap().pop_front().unwrap();
    done();
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert!(core.0.lock().unwrap().is_empty());
}

#[test]
fn recovery_child_lineage_cannot_be_substituted_by_an_identically_selected_startup() {
    let shutdown = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown.clone());
    let owner = synthetic_owner(ViewId(19), &RendererGeneration::new());
    let root = issuer.issue_startup(Vec::new(), false, owner.clone(), CancelToken::default());
    let other = issuer.issue_startup(Vec::new(), false, owner, CancelToken::default());
    let selection = NativeInvocationSelection::RecoveryBlank {
        surface: Surface::Docs,
        title: "Draft".into(),
    };
    let child = issuer
        .issue_recovery_child(&root, selection.clone(), CancelToken::default())
        .unwrap();
    assert!(child.is_child_of(&root.clone()));
    assert!(!child.is_child_of(&other));
    assert!(
        NativeInvocationIssuer::new(CancelToken::default())
            .issue_recovery_child(&root, selection, CancelToken::default())
            .is_err()
    );
    root.finish();
    assert!(child.with_active(|| ()).is_some());
    shutdown.cancel();
    assert!(child.with_active(|| ()).is_none());
}

#[test]
fn remembered_command_requires_the_exact_startup_and_issuer() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let owner = synthetic_owner(ViewId(20), &RendererGeneration::new());
    let root = issuer.issue_startup(Vec::new(), false, owner.clone(), CancelToken::default());
    let other = issuer.issue_startup(Vec::new(), false, owner, CancelToken::default());
    let command = NativeEditorCommand::OpenFile {
        surface: Surface::Docs,
        path: "remembered.docx".into(),
    };
    let child = issuer
        .issue_command_child(&root, command.clone(), CancelToken::default())
        .unwrap();
    assert!(child.is_native_command());
    assert!(!child.is_root_native_command());
    assert!(
        issuer
            .issue_command(NativeEditorCommand::OpenHome, CancelToken::default())
            .is_root_native_command()
    );
    assert_eq!(
        child.selection(),
        &NativeInvocationSelection::NativeCommand(command.clone())
    );
    assert!(child.is_child_of(&root));
    assert!(!child.is_child_of(&other));
    assert!(
        NativeInvocationIssuer::new(CancelToken::default())
            .issue_command_child(&root, command.clone(), CancelToken::default())
            .is_err()
    );
    let unowned = issuer.issue(
        NativeInvocationSelection::StartupLaunch {
            paths: Vec::new(),
            reopened_by_installer: false,
        },
        CancelToken::default(),
    );
    assert!(
        issuer
            .issue_command_child(&unowned, command.clone(), CancelToken::default())
            .is_err()
    );
    assert!(
        issuer
            .issue_command_child(&child, command, CancelToken::default())
            .is_err()
    );
}

#[test]
fn native_recovery_activation_retains_its_exact_command_root() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let root = issuer.issue_command(
        NativeEditorCommand::OpenFile {
            surface: Surface::Docs,
            path: "cached.docx".into(),
        },
        CancelToken::default(),
    );
    let selection = NativeInvocationSelection::RecoveryBlank {
        surface: Surface::Docs,
        title: "Cached".into(),
    };
    let child = issuer
        .issue_command_recovery_child(&root, selection.clone(), CancelToken::default())
        .unwrap();
    let context = RecoveryOpenContext::NativeStartup(child.clone());
    let created = RendererGeneration::new();
    let (_, closure, owner) =
        super::super::NativeClosureIssuer::new_with_owner(ViewId(21), created.clone());
    assert!(
        RecoveryOpenTarget::bind(
            context.clone(),
            ViewId(21),
            created.clone(),
            closure.clone(),
            owner.clone()
        )
        .is_err()
    );
    let other = issuer.issue_command(NativeEditorCommand::OpenHome, CancelToken::default());
    let constraint = |command| CommandRecoveryConstraint {
        command,
        participants: Vec::new(),
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
    };
    assert!(
        RecoveryOpenTarget::bind_command(
            context.clone(),
            ViewId(21),
            created.clone(),
            closure.clone(),
            owner.clone(),
            constraint(other)
        )
        .is_err()
    );
    let target = RecoveryOpenTarget::bind_command(
        context,
        ViewId(21),
        created,
        closure,
        owner,
        constraint(root.clone()),
    )
    .unwrap();
    assert!(child.is_child_of(&target.command_constraint().unwrap().command));
    assert!(
        NativeInvocationIssuer::new(CancelToken::default())
            .issue_command_recovery_child(&root, selection, CancelToken::default())
            .is_err()
    );
}

#[test]
fn triple_recovery_binding_keeps_exact_lineage_and_browser_owner() {
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let shell = synthetic_owner(ViewId(80), &RendererGeneration::new());
    let root = issuer.issue_startup(vec![], false, shell.clone(), CancelToken::default());
    let other_root = issuer.issue_startup(vec![], false, shell.clone(), CancelToken::default());
    let command = issuer
        .issue_command_child(
            &root,
            NativeEditorCommand::OpenFile {
                surface: Surface::Docs,
                path: "cached.docx".into(),
            },
            CancelToken::default(),
        )
        .unwrap();
    let child = issuer
        .issue_startup_command_recovery_child(
            &root,
            &command,
            NativeInvocationSelection::RecoveryBlank {
                surface: Surface::Docs,
                title: "Cached".into(),
            },
            CancelToken::default(),
        )
        .unwrap();
    let startup = |operation| StartupRecoveryConstraint {
        startup: operation,
        shell: shell.clone(),
        participants: vec![],
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
    };
    let command_constraint = CommandRecoveryConstraint {
        command: command.clone(),
        participants: vec![],
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
    };
    let created = RendererGeneration::new();
    let (_, closure, browser_owner) =
        super::super::NativeClosureIssuer::new_with_owner(ViewId(81), created.clone());
    let (_, _, foreign) =
        super::super::NativeClosureIssuer::new_with_owner(ViewId(81), created.clone());
    let context = RecoveryOpenContext::NativeStartup(child.clone());
    assert!(
        RecoveryOpenTarget::bind_startup_command(
            context.clone(),
            ViewId(81),
            created.clone(),
            closure.clone(),
            foreign,
            startup(root.clone()),
            command_constraint.clone(),
            None
        )
        .is_err()
    );
    assert!(
        RecoveryOpenTarget::bind_startup_command(
            context.clone(),
            ViewId(81),
            created.clone(),
            closure.clone(),
            browser_owner.clone(),
            startup(other_root),
            command_constraint.clone(),
            None
        )
        .is_err()
    );
    assert!(
        RecoveryOpenTarget::bind_command(
            context.clone(),
            ViewId(81),
            created.clone(),
            closure.clone(),
            browser_owner.clone(),
            command_constraint.clone()
        )
        .is_err()
    );
    let target = RecoveryOpenTarget::bind_startup_command(
        context,
        ViewId(81),
        created,
        closure,
        browser_owner.clone(),
        startup(root.clone()),
        command_constraint,
        None,
    )
    .unwrap();
    let snapshot = target.authority_snapshot().unwrap();
    assert!(target.browser_owner().same_owner(&browser_owner));
    assert!(
        snapshot
            .command
            .as_ref()
            .unwrap()
            .command
            .is_child_of(&root)
    );
    assert!(child.is_child_of(&snapshot.command.as_ref().unwrap().command));
}
