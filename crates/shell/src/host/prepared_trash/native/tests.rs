//! These exercise real filesystem witnesses and the admission protocol. The
//! injected transport observes dispatch only, not Finder/Recycle Bin behavior.
use super::super::fixtures::Sandbox;
use super::*;
use std::cell::Cell;
use std::fs;

#[test]
fn revocation_before_admission_preserves_the_original() {
    let scratch = Sandbox::new();
    let path = scratch.path().join("chat.json");
    fs::write(&path, b"synthetic chat").unwrap();
    let prepared = PreparedTrash::prepare(&path).unwrap();
    let error = prepared
        .admit(&mut |_| {
            Err(NodeError {
                code: None,
                message: "owner revoked".into(),
            })
        })
        .err()
        .unwrap();
    assert_eq!(error, "owner revoked");
    assert_eq!(fs::read(path).unwrap(), b"synthetic chat");
}

#[test]
fn accepted_operation_outlives_owner_without_holding_the_gate() {
    let scratch = Sandbox::new();
    let path = scratch.path().join("chat.json");
    fs::write(&path, b"synthetic chat").unwrap();
    let gate = Cell::new(false);
    let revoked = Cell::new(false);
    let prepared = PreparedTrash::prepare(&path).unwrap();
    let admitted = prepared
        .admit(&mut |effect| {
            assert!(!revoked.get());
            gate.set(true);
            let result = effect();
            gate.set(false);
            result
        })
        .unwrap();
    revoked.set(true);
    let result = admitted.start(|actual| {
        assert!(revoked.get());
        assert!(!gate.get());
        assert_eq!(actual, path);
        Err("synthetic native transport refusal".into())
    });
    assert_eq!(result.unwrap_err(), "synthetic native transport refusal");
    assert_eq!(fs::read(path).unwrap(), b"synthetic chat");
}

#[test]
fn accepted_witness_is_rechecked_before_dispatch() {
    let scratch = Sandbox::new();
    let path = scratch.path().join("chat.json");
    fs::write(&path, b"original").unwrap();
    let admitted = PreparedTrash::prepare(&path)
        .unwrap()
        .admit(&mut |effect| effect())
        .unwrap();
    fs::rename(&path, scratch.path().join("saved-original")).unwrap();
    fs::write(&path, b"replacement").unwrap();
    assert!(
        admitted
            .start(|_| panic!("changed target must not reach transport"))
            .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), b"replacement");
}

#[test]
fn repeated_and_missing_owner_callbacks_do_not_dispatch() {
    let scratch = Sandbox::new();
    let path = scratch.path().join("chat.json");
    fs::write(&path, b"original").unwrap();
    assert!(
        PreparedTrash::prepare(&path)
            .unwrap()
            .admit(&mut |effect| {
                effect()?;
                effect()
            })
            .is_err()
    );
    assert!(
        PreparedTrash::prepare(&path)
            .unwrap()
            .admit(&mut |_| Ok(()))
            .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), b"original");
}

#[test]
fn folders_are_never_admitted_as_chat_files() {
    let scratch = Sandbox::new();
    let path = scratch.path().join("chat.json");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), b"preserved").unwrap();
    assert!(PreparedTrash::prepare(&path).is_err());
    assert_eq!(fs::read(path.join("keep")).unwrap(), b"preserved");
}

#[cfg(target_os = "macos")]
#[test]
fn linked_target_and_linked_ancestor_are_refused() {
    use std::os::unix::fs::symlink;
    let scratch = Sandbox::new();
    let actual = scratch.path().join("actual");
    fs::create_dir(&actual).unwrap();
    fs::write(actual.join("chat.json"), b"keep").unwrap();
    symlink(&actual, scratch.path().join("linked")).unwrap();
    symlink(actual.join("chat.json"), scratch.path().join("chat.json")).unwrap();
    assert!(PreparedTrash::prepare(&scratch.path().join("linked/chat.json")).is_err());
    assert!(PreparedTrash::prepare(&scratch.path().join("chat.json")).is_err());
    assert_eq!(fs::read(actual.join("chat.json")).unwrap(), b"keep");
}
