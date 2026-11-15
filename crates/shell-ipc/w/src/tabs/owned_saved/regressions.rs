use super::*;
use shell_contract::hosts::{SavedFile, SavedSessionReceipt};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, UNIX_EPOCH},
};

// index.ts1607–1633 uses lexical resolve and JavaScript signed stat truthiness.
#[test]
fn root_comparison_is_lexical_without_canonicalizing_a_mount_or_symlink() {
    let root = jscompat::node_path::host::join(&["synthetic-save-root", "folder"]);
    let path = jscompat::node_path::host::join(&[&root, "..", "folder", "book.docx"]);
    assert!(pending_root_matches(&path, &root));
    let other = jscompat::node_path::host::join(&[&root, "nested", "book.docx"]);
    assert!(!pending_root_matches(&other, &root));
}

#[test]
fn negative_birthtime_is_truthy_and_does_not_fall_back_to_recent_mtime() {
    assert_eq!(
        stat_millis(Ok(UNIX_EPOCH - Duration::from_millis(5000))),
        -5000.0
    );
    assert!(!pending_birth_is_fresh(-5000.0, || 10000.0, 0.0));
    assert!(pending_birth_is_fresh(0.0, || 10000.0, 0.0));
    assert!(pending_birth_is_fresh(-2000.0, || 0.0, 0.0));
    assert!(!pending_birth_is_fresh(-2000.001, || 0.0, 0.0));
    let read = Cell::new(false);
    assert!(pending_birth_is_fresh(
        100.0,
        || {
            read.set(true);
            0.0
        },
        0.0
    ));
    assert!(!read.get());
}

#[test]
fn mapping_actual_path_retains_core_receipt_and_only_editor_ack_finalizes_it() {
    let committed = Rc::new(Cell::new(false));
    let publications = Rc::new(Cell::new(0));
    let actual_commit = committed.clone();
    let published = publications.clone();
    let receipt = SavedSessionReceipt::Pending(Box::new(move |done| {
        assert!(actual_commit.get());
        published.set(published.get() + 1);
        done(Ok(()));
    }));
    let saved = SavedFile {
        path: "root/file.docx",
        session: receipt,
    }
    .map_path(str::to_owned);
    assert_eq!(saved.path, "root/file.docx");
    assert_eq!(publications.get(), 0);
    committed.set(true);
    saved.session.finalize(Box::new(|result| result.unwrap()));
    assert_eq!(publications.get(), 1);
}

#[test]
fn dropping_a_failed_editor_ack_does_not_publish_a_session() {
    let published = Rc::new(Cell::new(false));
    let target = published.clone();
    let receipt = SavedSessionReceipt::Pending(Box::new(move |done| {
        target.set(true);
        done(Ok(()));
    }));
    drop(receipt);
    assert!(!published.get());
}

#[test]
fn exact_pending_value_replacement_is_not_consumed_by_an_older_continuation() {
    // Model of clear_if_current's atomic original-pair + core map acknowledgement.
    let captured = Some(("folder-a".to_owned(), 10.0));
    let mut actual = Some(("folder-b".to_owned(), 20.0));
    let clear = |current: &mut Option<(String, f64)>| {
        if *current != captured {
            return Err("replaced");
        }
        *current = None;
        Ok(())
    };
    assert!(clear(&mut actual).is_err());
    assert_eq!(actual, Some(("folder-b".into(), 20.0)));
}
