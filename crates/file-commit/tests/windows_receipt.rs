#![cfg(windows)]
use file_commit::{DirectWriteOutcome, PublicationMode, prepare_commit, prepare_direct};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING,
        SYNCHRONIZE,
    },
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("file-commit-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open(path: &Path, access: u32, share: u32, disposition: u32, flags: u32) -> File {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            share,
            ptr::null(),
            disposition,
            flags,
            ptr::null_mut(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{:?}",
        std::io::Error::last_os_error()
    );
    unsafe { File::from_raw_handle(handle.cast()) }
}
fn parent(path: &Path) -> File {
    open(
        path,
        FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

#[test]
fn reopened_identity_keeps_share_zero_stage_exact_without_locking_published_file() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let stage_path = sandbox.0.join("stage.tmp");
    let target = sandbox.0.join("document.txt");
    // Match the project's NtCreateFile rights and share=0; CreateFileW is synchronous.
    let mut stage = open(
        &stage_path,
        FILE_READ_ATTRIBUTES | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | DELETE | SYNCHRONIZE,
        0,
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    );
    stage.write_all(b"initial").unwrap();
    let plan = prepare_commit(
        &stage,
        &parent,
        "document.txt",
        &target,
        PublicationMode::Exclusive,
    )
    .unwrap();
    let receipt = plan.publish().unwrap();
    drop(stage);
    assert!(receipt.matches_path(&target).unwrap());
    let mut ordinary = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .unwrap();
    ordinary.write_all(b"updated").unwrap();
    drop(ordinary);
    let replacement = sandbox.0.join("replacement.tmp");
    fs::write(&replacement, b"replacement").unwrap();
    fs::rename(&replacement, &target).unwrap();
    assert!(!receipt.matches_path(&target).unwrap());
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
}

#[test]
fn direct_fallback_receipt_follows_the_actual_created_descriptor() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let target = sandbox.0.join("fallback.txt");
    fs::write(&target, b"old").unwrap();
    let accepted = prepare_direct(&parent, &target).unwrap().open().unwrap();
    let receipt = match accepted.write_all(b"new").unwrap() {
        DirectWriteOutcome::Committed(receipt) => receipt,
        DirectWriteOutcome::WrittenWithoutReceipt(error) => {
            panic!("completed write without receipt: {error}")
        }
    };
    assert_eq!(receipt.mode(), PublicationMode::Direct);
    assert!(receipt.matches_path(&target).unwrap());
    assert_eq!(fs::read(&target).unwrap(), b"new");
    let replacement = sandbox.0.join("replacement.tmp");
    fs::write(&replacement, b"other").unwrap();
    fs::rename(&replacement, &target).unwrap();
    assert!(!receipt.matches_path(&target).unwrap());
}

#[test]
fn direct_open_stays_in_the_captured_directory_when_path_is_replaced() {
    let sandbox = Sandbox::new();
    let selected = sandbox.0.join("selected");
    fs::create_dir(&selected).unwrap();
    let parent = parent(&selected);
    let target = selected.join("out.txt");
    let prepared = prepare_direct(&parent, &target).unwrap();
    let moved = sandbox.0.join("moved");
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(&target, b"unrelated").unwrap();
    let result = prepared.open().unwrap().write_all(b"captured").unwrap();
    assert!(matches!(
        result,
        DirectWriteOutcome::WrittenWithoutReceipt(_)
    ));
    assert_eq!(fs::read(&target).unwrap(), b"unrelated");
    assert_eq!(fs::read(moved.join("out.txt")).unwrap(), b"captured");
}

#[test]
fn private_stage_publication_returns_actual_written_snapshot() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = match private.create_stage(0o666).unwrap() {
        file_commit::CreatedStage::Movable(pending) => pending,
        file_commit::CreatedStage::RenameUnavailable(stage) => {
            panic!("DELETE unavailable: {}", stage.error())
        }
    };
    let bytes = b"actual payload".to_vec();
    let pointer = bytes.as_ptr();
    let pending = match pending.write_owned(bytes) {
        Ok(stage) => stage,
        Err((error, _, _)) => panic!("{error}"),
    };
    let written = match pending.quarantine() {
        Ok(stage) => stage,
        Err((error, _)) => panic!("{error}"),
    };
    let target = sandbox.0.join("output.txt");
    fs::write(&target, b"older").unwrap();
    let plan = match written.prepare(&parent, "output.txt", &target, PublicationMode::Replace) {
        Ok(plan) => plan,
        Err((error, _)) => panic!("{error}"),
    };
    let snapshot = match plan.publish() {
        Ok(snapshot) => snapshot,
        Err(failure) => panic!("{}", failure.0),
    };
    assert_eq!(snapshot.bytes().as_ptr(), pointer);
    assert_eq!(snapshot.bytes(), b"actual payload");
    assert_eq!(fs::read(&target).unwrap(), b"actual payload");
    assert!(snapshot.receipt().matches_path(&target).unwrap());
    private.remove().unwrap();
}

#[test]
fn pinned_read_uses_the_original_directory_handle() {
    use std::io::Read;
    let sandbox = Sandbox::new();
    let selected = sandbox.0.join("selected");
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("copy"), b"original").unwrap();
    let parent = parent(&selected);
    fs::rename(&selected, sandbox.0.join("moved")).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("copy"), b"replacement").unwrap();
    let mut file = file_commit::open_pinned_file(&parent, "copy").unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"original");
    assert!(file_commit::open_pinned_file(&parent, "../copy").is_err());
}

#[test]
fn public_source_lease_conflict_retains_file_and_retries_exact_move() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let source = sandbox.0.join("source.txt");
    fs::write(&source, b"original").unwrap();
    let captured = File::open(&source).unwrap();
    let blocker = open(
        &source,
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = match private.quarantine_public(&parent, "source.txt", captured) {
        Ok(value) => value,
        Err((error, _)) => panic!("{error}"),
    };
    let pending = match pending.quarantine() {
        Ok(_) => panic!("lease ignored an existing DELETE open"),
        Err((_, pending)) => pending,
    };
    assert_eq!(fs::read(&source).unwrap(), b"original");
    drop(blocker);
    let stage = match pending.quarantine() {
        Ok(stage) => stage,
        Err((error, _)) => panic!("{error}"),
    };
    fs::write(&source, b"occupant").unwrap();
    let stage = match stage.restore() {
        Ok(()) => panic!("inverse replaced occupant"),
        Err((_, stage)) => stage,
    };
    assert_eq!(fs::read(&source).unwrap(), b"occupant");
    fs::remove_file(&source).unwrap();
    assert!(stage.restore().is_ok());
    assert_eq!(fs::read(&source).unwrap(), b"original");
    private.remove().unwrap();
}

#[test]
fn quarantine_renames_the_leased_source_link_not_another_hardlink() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let source = sandbox.0.join("source.txt");
    let alias = sandbox.0.join("alias.txt");
    fs::write(&source, b"original").unwrap();
    fs::hard_link(&source, &alias).unwrap();
    let captured = File::open(&alias).unwrap();
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = match private.quarantine_public(&parent, "source.txt", captured) {
        Ok(value) => value,
        Err((error, _)) => panic!("{error}"),
    };
    let stage = match pending.quarantine() {
        Ok(stage) => stage,
        Err((error, _)) => panic!("{error}"),
    };
    assert!(!source.exists());
    assert_eq!(fs::read(&alias).unwrap(), b"original");
    assert!(stage.restore().is_ok());
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert_eq!(fs::read(&alias).unwrap(), b"original");
    private.remove().unwrap();
}

#[test]
fn public_source_replacement_is_rejected_before_any_move() {
    let sandbox = Sandbox::new();
    let parent = parent(&sandbox.0);
    let source = sandbox.0.join("source.txt");
    let moved = sandbox.0.join("moved.txt");
    fs::write(&source, b"captured").unwrap();
    let captured = File::open(&source).unwrap();
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = match private.quarantine_public(&parent, "source.txt", captured) {
        Ok(value) => value,
        Err((error, _)) => panic!("{error}"),
    };
    fs::rename(&source, &moved).unwrap();
    fs::write(&source, b"replacement").unwrap();
    let pending = match pending.quarantine() {
        Ok(_) => panic!("replaced source was moved"),
        Err((error, pending)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            pending
        }
    };
    assert_eq!(
        pending.location(),
        file_commit::PendingStageLocation::Public
    );
    assert_eq!(fs::read(&source).unwrap(), b"replacement");
    assert_eq!(fs::read(&moved).unwrap(), b"captured");
    drop(pending);
    private.remove().unwrap();
}

#[test]
fn copyfileex_overwrite_with_zero_access_source_destination_and_ancestor_leases() {
    use windows_sys::Win32::Storage::FileSystem::{
        CopyFileExW, FILE_ATTRIBUTE_HIDDEN, GetFileAttributesW, SetFileAttributesW,
    };
    let sandbox = Sandbox::new();
    let dir = sandbox.0.join("leased");
    fs::create_dir(&dir).unwrap();
    let source = dir.join("source.txt");
    let target = dir.join("rescue.txt");
    fs::write(&source, b"staged bytes").unwrap();
    fs::write(&target, b"older rescue").unwrap();
    let source_stream = PathBuf::from(format!("{}:extra", source.display()));
    let target_stream = PathBuf::from(format!("{}:extra", target.display()));
    fs::write(&source_stream, b"alternate stream").unwrap();
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    assert_ne!(
        unsafe { SetFileAttributesW(source_wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
        0
    );
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    let _dir_lease = open(
        &dir,
        FILE_TRAVERSE,
        share,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
    );
    let _source_lease = open(
        &source,
        0,
        share,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let _target_lease = open(
        &target,
        0,
        share,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT,
    );
    assert!(fs::rename(&dir, sandbox.0.join("moved-ancestor")).is_err());
    assert!(fs::rename(&source, dir.join("moved-source")).is_err());
    assert!(fs::rename(&target, dir.join("moved-rescue")).is_err());
    let copied = unsafe {
        CopyFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            None,
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    assert_ne!(
        copied,
        0,
        "CopyFileExW with retained leases: {:?}",
        std::io::Error::last_os_error()
    );
    assert_eq!(fs::read(&target).unwrap(), b"staged bytes");
    assert_eq!(fs::read(&target_stream).unwrap(), b"alternate stream");
    assert_ne!(
        unsafe { GetFileAttributesW(target_wide.as_ptr()) } & FILE_ATTRIBUTE_HIDDEN,
        0
    );
}

#[test]
fn receipt_requarantine_rejects_replaced_public_name() {
    let root = Sandbox::new();
    let parent = parent(&root.0);
    let private = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = match private.create_stage(0o600).unwrap() {
        file_commit::CreatedStage::Movable(stage) => stage,
        file_commit::CreatedStage::RenameUnavailable(stage) => panic!("{}", stage.error()),
    };
    let pending = match pending.write_owned(b"committed".to_vec()) {
        Ok(stage) => stage,
        Err((error, _, _)) => panic!("{error}"),
    };
    let stage = match pending.quarantine() {
        Ok(stage) => stage,
        Err((error, _)) => panic!("{error}"),
    };
    let target = root.0.join("out");
    let plan = match stage.prepare(&parent, "out", &target, PublicationMode::Exclusive) {
        Ok(plan) => plan,
        Err((error, _)) => panic!("{error}"),
    };
    let snapshot = match plan.publish() {
        Ok(snapshot) => snapshot,
        Err(failure) => panic!("{}", failure.0),
    };
    fs::remove_file(&target).unwrap();
    fs::write(&target, b"replacement").unwrap();
    let pending = private.quarantine_receipt(snapshot.receipt()).unwrap();
    let pending = match pending.quarantine() {
        Ok(_) => panic!("replacement accepted"),
        Err((_, pending)) => pending,
    };
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    drop(pending);
    private.remove().unwrap();
}

#[test]
fn output_parent_handles_a_long_non_verbatim_path() {
    let root = Sandbox::new();
    let root_text = root.0.to_str().unwrap();
    let plain = if let Some(unc) = root_text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        root_text
            .strip_prefix(r"\\?\")
            .unwrap_or(root_text)
            .to_owned()
    };
    let mut directory = PathBuf::from(plain);
    for index in 0..8 {
        directory.push(format!("{index}-{}", "segment".repeat(6)));
    }
    fs::create_dir_all(&directory).unwrap();
    let parent = file_commit::open_output_parent(&directory).unwrap();
    let target = directory.join("out.txt");
    let result = prepare_direct(&parent, &target)
        .unwrap()
        .open()
        .unwrap()
        .write_all(b"long path")
        .unwrap();
    assert!(matches!(result, DirectWriteOutcome::Committed(_)));
    assert_eq!(fs::read(&target).unwrap(), b"long path");
}
