#![cfg(unix)]
use std::{ffi::CString,fs::{self,File,OpenOptions},io::Write,os::unix::fs::PermissionsExt,path::PathBuf};
use file_commit::{prepare_commit,prepare_direct,DirectWriteOutcome,PublicationMode,ReadDisposition};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new()->Self {
        let nonce=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!("file-commit-{}-{nonce}",std::process::id()));
        fs::create_dir(&path).unwrap(); Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Sandbox { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }

#[test]
fn rescue_copy_keeps_the_captured_parent_and_stage_bytes() {
    let root = Sandbox::new();
    let selected = root.0.join("selected");
    fs::create_dir(&selected).unwrap();
    let parent = File::open(&selected).unwrap();
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = private.create_stage(0o640).unwrap();
    let mut stage = match pending.quarantine() { Ok(stage) => stage, Err((error, _)) => panic!("{error}") };
    stage.file_mut().write_all(b"retained bytes").unwrap();
    fs::write(selected.join("report.unsaved.tmp"), b"old rescue").unwrap();
    let prepared = file_commit::PreparedRescueCopy::new(&parent,
        &CString::new("report.unsaved.tmp").unwrap(), &mut stage).unwrap();
    let moved = root.0.join("moved");
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("report.unsaved.tmp"), b"other folder").unwrap();
    let copy = match prepared.open() {
        Ok(file_commit::RescueOpen::Ready(copy)) => copy,
        _ => panic!("rescue destination was not opened"),
    };
    let destination = match copy.copy() { Ok(destination) => destination, Err(_) => panic!("rescue copy failed") };
    drop(destination);
    assert_eq!(fs::read(moved.join("report.unsaved.tmp")).unwrap(), b"retained bytes");
    assert_eq!(fs::read(selected.join("report.unsaved.tmp")).unwrap(), b"other folder");
    assert!(stage.remove().is_ok());
    private.remove().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn stale_rescue_symlink_is_captured_and_removed_without_touching_its_target() {
    use std::os::unix::fs::symlink;
    let root = Sandbox::new();
    let parent = File::open(&root.0).unwrap();
    let target = root.0.join("target");
    fs::write(&target, b"keep").unwrap();
    symlink("target", root.0.join("old.unsaved.tmp")).unwrap();
    let name = CString::new("old.unsaved.tmp").unwrap();
    let captured = file_commit::capture_rescue_entry(&parent, &name).unwrap().unwrap();
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = private.quarantine_public(&parent, &name, captured).unwrap();
    let stage = match pending.quarantine() { Ok(stage) => stage, Err((error, _)) => panic!("{error}") };
    assert!(stage.remove().is_ok());
    private.remove().unwrap();
    assert!(fs::symlink_metadata(root.0.join("old.unsaved.tmp")).is_err());
    assert_eq!(fs::read(target).unwrap(), b"keep");
}

#[test]
fn committed_read_is_bound_to_the_published_object_not_replacement_path() {
    let root=Sandbox::new(); let parent=File::open(&root.0).unwrap();
    let source=root.0.join("stage"); let target=root.0.join("out");
    let mut file=OpenOptions::new().read(true).write(true).create_new(true).open(&source).unwrap();
    file.write_all(b"original").unwrap();
    let plan=prepare_commit(&parent,&file,&CString::new("stage").unwrap(),&CString::new("out").unwrap(),&target,PublicationMode::Exclusive).unwrap();
    let receipt=plan.publish().unwrap(); drop(file);
    assert_eq!(receipt.read_disposition(),ReadDisposition::Readable);
    assert_eq!(receipt.read_bounded(8,||false).unwrap().bytes(),b"original");
    assert!(receipt.read_bounded(7,||false).is_err());
    assert_eq!(receipt.read_bounded(8,||true).err().unwrap().kind(),std::io::ErrorKind::Interrupted);
    fs::remove_file(&target).unwrap(); fs::write(&target,b"replacement").unwrap();
    assert!(!receipt.matches_path(&target).unwrap());
    let read=receipt.read_bounded(8,||false).unwrap();
    assert!(read.receipt().same_commit(&receipt)); assert_eq!(read.bytes(),b"original");
}

#[test]
fn direct_fallback_never_opens_a_replaced_parent() {
    let root=Sandbox::new(); let dir=root.0.join("selected"); fs::create_dir(&dir).unwrap();
    let parent=File::open(&dir).unwrap(); let target=dir.join("out");
    let prepared=prepare_direct(&parent,&target).unwrap();
    let moved=root.0.join("moved"); fs::rename(&dir,&moved).unwrap(); fs::create_dir(&dir).unwrap();
    fs::write(&target,b"other").unwrap();
    let result=prepared.open().unwrap().write_all(b"actual").unwrap();
    assert!(matches!(result,DirectWriteOutcome::WrittenWithoutReceipt(_)));
    assert_eq!(fs::read(&target).unwrap(),b"other");
    assert_eq!(fs::read(moved.join("out")).unwrap(),b"actual");
}

#[test]
fn write_only_existing_file_still_saves_and_preserves_read_permission_error() {
    if unsafe { libc::geteuid() }==0 { return; }
    let root=Sandbox::new(); let parent=File::open(&root.0).unwrap(); let target=root.0.join("write-only");
    fs::write(&target,b"old").unwrap(); fs::set_permissions(&target,fs::Permissions::from_mode(0o200)).unwrap();
    let written=prepare_direct(&parent,&target).unwrap().open().unwrap().write_all(b"new").unwrap();
    let receipt=match written { DirectWriteOutcome::Committed(receipt)=>receipt,DirectWriteOutcome::WrittenWithoutReceipt(error)=>panic!("write completed without proof: {error}") };
    assert_eq!(receipt.read_disposition(),ReadDisposition::WriteOnly);
    assert_eq!(receipt.read_bounded(16,||false).err().unwrap().kind(),std::io::ErrorKind::PermissionDenied);
    fs::set_permissions(&target,fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&target).unwrap(),b"new");
}

#[test]
fn quarantine_mismatch_inverse_preserves_both_files_until_retry() {
    use std::io::Read;
    let root=Sandbox::new();
    let parent=File::open(&root.0).unwrap();
    let private=file_commit::PrivateDirectory::create(&parent).unwrap();
    let path=root.0.join("selected");
    fs::write(&path,b"selected").unwrap();
    let original=File::open(&path).unwrap();
    let pending=private.quarantine_public(&parent,&CString::new("selected").unwrap(),original).unwrap();
    assert_eq!(pending.location(),file_commit::PendingStageLocation::Public);
    fs::rename(&path,root.0.join("original")).unwrap();
    fs::write(&path,b"replacement").unwrap();
    let pending=match pending.quarantine() {
        Ok(_) => panic!("a substituted object was accepted"),
        Err((error,pending)) => { assert_eq!(error.kind(),std::io::ErrorKind::InvalidData); pending }
    };
    assert_eq!(pending.location(),file_commit::PendingStageLocation::Quarantine);
    fs::write(&path,b"occupant").unwrap();
    let pending=match pending.restore() {
        Ok(()) => panic!("inverse replaced a new occupant"),
        Err((error,pending)) => { assert_eq!(error.kind(),std::io::ErrorKind::AlreadyExists); pending }
    };
    assert_eq!(pending.location(),file_commit::PendingStageLocation::Quarantine);
    assert_eq!(fs::read(&path).unwrap(),b"occupant");
    fs::remove_file(&path).unwrap();
    assert!(pending.restore().is_ok());
    let mut restored=String::new();
    File::open(&path).unwrap().read_to_string(&mut restored).unwrap();
    assert_eq!(restored,"replacement");
    assert_eq!(fs::read(root.0.join("original")).unwrap(),b"selected");
    private.remove().unwrap();
}

#[test]
fn creator_stages_original_parent_file_and_retries_exclusive_conflict() {
    let root=Sandbox::new();
    use std::os::unix::fs::OpenOptionsExt;
    let control=root.0.join("control");
    OpenOptions::new().write(true).create_new(true).mode(0o640).open(&control).unwrap();
    let expected_mode=fs::metadata(&control).unwrap().permissions().mode() & 0o777;
    let parent=File::open(&root.0).unwrap();
    let private=file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending=private.create_stage(0o640).unwrap();
    let mut stage=match pending.quarantine() { Ok(stage)=>stage,Err((error,_))=>panic!("{error}") };
    stage.file_mut().write_all(b"created").unwrap();
    let target=root.0.join("out");
    fs::write(&target,b"existing").unwrap();
    let name=CString::new("out").unwrap();
    let plan=match stage.prepare(&parent,&name,&target,PublicationMode::Exclusive) { Ok(plan)=>plan,Err((error,_))=>panic!("{error}") };
    let plan=match plan.publish() { Ok(_)=>panic!("exclusive publication replaced a file"),Err((error,plan))=>{ assert_eq!(error.kind(),std::io::ErrorKind::AlreadyExists); plan } };
    assert_eq!(fs::read(&target).unwrap(),b"existing");
    fs::remove_file(&target).unwrap();
    let receipt=match plan.publish() { Ok(receipt)=>receipt,Err((error,_))=>panic!("{error}") };
    assert_eq!(receipt.read_bounded(32,||false).unwrap().bytes(),b"created");
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777,expected_mode);
    private.remove().unwrap();
}

#[test]
fn successful_owned_descriptor_write_returns_exact_snapshot() {
    let root=Sandbox::new();
    let parent=File::open(&root.0).unwrap();
    let target=root.0.join("out");
    let bytes=b"written snapshot".to_vec();
    let original_pointer=bytes.as_ptr();
    let snapshot=match prepare_direct(&parent,&target).unwrap().open().unwrap().write_owned(bytes).unwrap() {
        file_commit::DirectOwnedWriteOutcome::Committed(snapshot)=>snapshot,
        file_commit::DirectOwnedWriteOutcome::WrittenWithoutReceipt { error,.. }=>panic!("{error}"),
    };
    assert_eq!(snapshot.bytes().as_ptr(),original_pointer);
    assert_eq!(snapshot.bytes(),b"written snapshot");
    fs::write(&target,b"later contents").unwrap();
    assert_eq!(snapshot.bytes(),b"written snapshot");
    assert_eq!(snapshot.receipt().read_bounded(32,||false).unwrap().bytes(),b"later contents");
}

#[test]
fn renamed_written_snapshot_is_bound_to_the_real_commit() {
    let root=Sandbox::new();
    let parent=File::open(&root.0).unwrap();
    let private=file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending=private.create_stage(0o600).unwrap();
    let payload=b"owned stage bytes".to_vec();
    let pointer=payload.as_ptr();
    let written=match pending.write_owned(payload) { Ok(written)=>written,Err((error,_,_))=>panic!("{error}") };
    let written=match written.quarantine() { Ok(stage)=>stage,Err((error,_))=>panic!("{error}") };
    written.sync_all().unwrap();
    let target=root.0.join("out");
    let plan=match written.prepare(&parent,&CString::new("out").unwrap(),&target,PublicationMode::Replace) {
        Ok(plan)=>plan,Err((error,_))=>panic!("{error}"),
    };
    let snapshot=match plan.publish() { Ok(snapshot)=>snapshot,Err((error,_))=>panic!("{error}") };
    assert_eq!(snapshot.bytes().as_ptr(),pointer);
    assert_eq!(snapshot.bytes(),b"owned stage bytes");
    assert_eq!(snapshot.receipt().read_bounded(32,||false).unwrap().bytes(),snapshot.bytes());
    assert_eq!(fs::read(&target).unwrap(),b"owned stage bytes");
    private.remove().unwrap();
}

#[test]
fn pinned_read_uses_captured_parent_and_refuses_symlinks() {
    use std::{io::Read, os::unix::fs::symlink};
    let root=Sandbox::new();
    let selected=root.0.join("selected");
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("copy"),b"original").unwrap();
    symlink("copy",selected.join("link")).unwrap();
    let parent=File::open(&selected).unwrap();
    fs::rename(&selected,root.0.join("moved")).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("copy"),b"replacement").unwrap();
    let mut file=file_commit::open_pinned_file(&parent,"copy").unwrap();
    let mut bytes=Vec::new(); file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes,b"original");
    assert!(file_commit::open_pinned_file(&parent,"link").is_err());
    assert!(file_commit::open_pinned_file(&parent,"../copy").is_err());
}

#[test]
fn output_parent_does_not_require_directory_list_permission() {
    if unsafe { libc::geteuid() } == 0 { return; }
    let root=Sandbox::new();
    let selected=root.0.join("write-search");
    fs::create_dir(&selected).unwrap();
    fs::set_permissions(&selected,fs::Permissions::from_mode(0o300)).unwrap();
    let parent=file_commit::open_output_parent(&selected).unwrap();
    assert!(fs::read_dir(&selected).is_err());
    let target=selected.join("out");
    let result=file_commit::prepare_direct(&parent,&target).unwrap().open().unwrap().write_all(b"written").unwrap();
    assert!(matches!(result,DirectWriteOutcome::Committed(_)));
    fs::set_permissions(&selected,fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read(target).unwrap(),b"written");
}

#[test]
fn atomic_temporary_name_keeps_the_original_recognizable_format() {
    let root=Sandbox::new();
    let parent=file_commit::open_output_parent(&root.0).unwrap();
    let private=file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending=private.create_stage_for("report.md",0o666).unwrap();
    let name=fs::read_dir(&root.0).unwrap().map(|entry|entry.unwrap().file_name().into_string().unwrap())
        .find(|name|name.starts_with("report.md.")).unwrap();
    let suffix=name.strip_prefix("report.md.").unwrap().strip_suffix(".tmp").unwrap();
    assert_eq!(suffix.len(),12);
    assert!(suffix.bytes().all(|byte|byte.is_ascii_hexdigit()));
    let stage=match pending.quarantine() { Ok(stage)=>stage,Err((error,_))=>panic!("{error}") };
    assert!(stage.remove().is_ok());
    private.remove().unwrap();
}

#[test]
fn receipt_requarantine_never_deletes_a_replacement() {
    let root=Sandbox::new();
    let parent=file_commit::open_output_parent(&root.0).unwrap();
    let private=file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending=private.create_stage(0o600).unwrap();
    let stage=match pending.quarantine() { Ok(stage)=>stage,Err((error,_))=>panic!("{error}") };
    let written=match stage.write_owned(b"committed".to_vec()) { Ok(stage)=>stage,Err((error,_,_))=>panic!("{error}") };
    let target=root.0.join("out"); let name=CString::new("out").unwrap();
    let plan=match written.prepare(&parent,&name,&target,PublicationMode::Exclusive) { Ok(plan)=>plan,Err((error,_))=>panic!("{error}") };
    let snapshot=match plan.publish() { Ok(snapshot)=>snapshot,Err((error,_))=>panic!("{error}") };
    fs::remove_file(&target).unwrap(); fs::write(&target,b"replacement").unwrap();
    let pending=private.quarantine_receipt(snapshot.receipt()).unwrap();
    let pending=match pending.quarantine() { Ok(_)=>panic!("replacement accepted"),Err((_,pending))=>pending };
    assert!(pending.restore().is_ok());
    assert_eq!(fs::read(&target).unwrap(),b"replacement");
    assert_eq!(snapshot.receipt().read_bounded(32,||false).unwrap().bytes(),b"committed");
    private.remove().unwrap();
}
