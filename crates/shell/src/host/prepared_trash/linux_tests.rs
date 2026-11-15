use super::*;
use super::super::fixtures::Sandbox;
use std::os::unix::fs::symlink;

fn input(scratch: &Sandbox) -> PathBuf {
    let path = scratch.path().join("project/.seuoffice/chats/chat %한글.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(&path, b"synthetic chat").unwrap(); path
}
fn prepared(path: &Path, scratch: &Sandbox) -> PreparedTrash {
    PreparedTrash::prepare_with(path, Some(&scratch.path().join("isolated-trash"))).unwrap()
}
fn empty(path: &Path) -> bool { fs::read_dir(path).unwrap().next().is_none() }

#[test]
fn split_commit_retains_original_name_location_and_inode_for_restore() {
    let scratch = Sandbox::new(); let path = input(&scratch);
    let original = fs::metadata(&path).unwrap(); let work = prepared(&path, &scratch);
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    let metadata = work.destination.info.path.join(OsStr::from_bytes(work.destination.info_name.as_bytes()));
    assert_eq!(fs::read(&path).unwrap(), b"synthetic chat");
    assert!(!payload.exists());
    assert!(fs::read_to_string(&metadata).unwrap().contains(&format!("Path={}\n", encoded_path(&path))));
    work.execute_authorized(&mut |effect| { assert!(path.exists()); effect() }).unwrap();
    assert!(!path.exists()); assert!(path.parent().unwrap().is_dir());
    assert_eq!(fs::metadata(&payload).unwrap().ino(), original.ino());
    assert_eq!(fs::read(&payload).unwrap(), b"synthetic chat");
    fs::rename(&payload, &path).unwrap();
    assert_eq!(path.file_name().unwrap(), OsStr::new("chat %한글.json"));
    assert_eq!(fs::read(path).unwrap(), b"synthetic chat");
}

#[test]
fn revoke_before_commit_cleans_only_preparation_and_keeps_the_source() {
    let scratch = Sandbox::new(); let path = input(&scratch); let work = prepared(&path, &scratch);
    let files = work.destination.files.path.clone(); let info = work.destination.info.path.clone();
    let result = work.execute_authorized(&mut |_| Err(NodeError { code: None, message: "owner revoked".into() }));
    assert!(result.unwrap_err().contains("owner revoked"));
    assert_eq!(fs::read(path).unwrap(), b"synthetic chat"); assert!(empty(&files)); assert!(empty(&info));
}

#[test]
fn collision_never_overwrites_an_existing_recycled_file() {
    let scratch = Sandbox::new(); let path = input(&scratch); let work = prepared(&path, &scratch);
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    fs::write(&payload, b"other recycled file").unwrap();
    assert!(work.execute_authorized(&mut |effect| effect()).is_err());
    assert_eq!(fs::read(payload).unwrap(), b"other recycled file");
    assert_eq!(fs::read(path).unwrap(), b"synthetic chat");
}

#[test]
fn changed_target_and_replaced_ancestor_are_not_removed() {
    let scratch = Sandbox::new(); let path = input(&scratch); let work = prepared(&path, &scratch);
    fs::rename(&path, path.with_extension("saved")).unwrap(); fs::write(&path, b"replacement").unwrap();
    assert!(work.execute_authorized(&mut |effect| effect()).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    let work = prepared(&path, &scratch); let parent = path.parent().unwrap();
    fs::rename(parent, parent.with_extension("saved")).unwrap(); fs::create_dir(parent).unwrap();
    fs::write(&path, b"new ancestor").unwrap();
    assert!(work.execute_authorized(&mut |effect| effect()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"new ancestor");
}

#[test]
fn folders_links_and_source_modification_are_refused() {
    let scratch = Sandbox::new(); let path = input(&scratch);
    assert!(PreparedTrash::prepare_with(path.parent().unwrap(), Some(&scratch.path().join("isolated-trash"))).is_err());
    let link = path.with_extension("link"); symlink(&path, &link).unwrap();
    assert!(PreparedTrash::prepare_with(&link, Some(&scratch.path().join("isolated-trash"))).is_err());
    let work = prepared(&path, &scratch); fs::write(&path, b"changed after staging").unwrap();
    assert!(work.execute_authorized(&mut |effect| effect()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"changed after staging");
}

#[test]
fn one_file_success_then_failure_keeps_the_first_in_trash_and_second_in_place() {
    let scratch = Sandbox::new(); let first = input(&scratch); let second = first.with_extension("jsonl");
    fs::write(&second, b"second file").unwrap();
    let work = prepared(&first, &scratch);
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    work.execute_authorized(&mut |effect| effect()).unwrap();
    let work = prepared(&second, &scratch);
    assert!(work.execute_authorized(&mut |_| Err(NodeError { code: None, message: "revoked between files".into() })).is_err());
    assert!(!first.exists()); assert_eq!(fs::read(payload).unwrap(), b"synthetic chat");
    assert_eq!(fs::read(second).unwrap(), b"second file");
}

#[test]
fn cross_device_copy_is_prepared_outside_gate_and_original_restore_path_is_retained() {
    let scratch = Sandbox::new(); let other = Sandbox::in_env("SEUOFFICE_HOSTED_TRASH_CROSS_VOLUME_ROOT");
    let path = input(&scratch);
    assert_ne!(fs::metadata(&path).unwrap().dev(), fs::metadata(other.path()).unwrap().dev(), "hosted fixture must supply different filesystems");
    let work = PreparedTrash::prepare_with(&path, Some(&other.path().join("isolated-trash"))).unwrap();
    assert!(work.destination.copy.is_some());
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    let metadata = work.destination.info.path.join(OsStr::from_bytes(work.destination.info_name.as_bytes()));
    work.execute_authorized(&mut |effect| { assert_eq!(fs::read(&path).unwrap(), b"synthetic chat"); effect() }).unwrap();
    assert!(!path.exists()); assert_eq!(fs::read(payload).unwrap(), b"synthetic chat");
    assert!(fs::read_to_string(metadata).unwrap().contains(&format!("Path={}\n", encoded_path(&path))));
    assert!(empty(path.parent().unwrap()));
}

#[test]
fn encoded_restore_paths_preserve_non_utf8_bytes() {
    assert_eq!(encoded_path(Path::new(OsStr::from_bytes(b"/project/a\xff% b.json"))), "/project/a%FF%25%20b.json");
    assert_eq!(mount_path(b"/mnt/a\\040b").unwrap(), PathBuf::from("/mnt/a b"));
    assert!(mount_path(b"/mnt/a\\999").is_err());
}

#[test]
fn cross_device_publish_failure_conditionally_restores_the_original() {
    let scratch = Sandbox::new(); let other = Sandbox::in_env("SEUOFFICE_HOSTED_TRASH_CROSS_VOLUME_ROOT");
    let path = input(&scratch);
    assert_ne!(fs::metadata(&path).unwrap().dev(), fs::metadata(other.path()).unwrap().dev());
    let work = PreparedTrash::prepare_with(&path, Some(&other.path().join("isolated-trash"))).unwrap();
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    fs::write(&payload, b"unrelated recycle collision").unwrap();
    assert!(work.execute_authorized(&mut |effect| effect()).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"synthetic chat");
    assert_eq!(fs::read(payload).unwrap(), b"unrelated recycle collision");
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn repeated_callback_never_repeats_the_move_or_cleans_committed_metadata() {
    let scratch = Sandbox::new(); let path = input(&scratch); let work = prepared(&path, &scratch);
    let payload = work.destination.files.path.join(OsStr::from_bytes(work.destination.name.as_bytes()));
    let metadata = work.destination.info.path.join(OsStr::from_bytes(work.destination.info_name.as_bytes()));
    let error = work.execute_authorized(&mut |effect| { effect()?; effect() }).unwrap_err();
    assert!(error.contains("already moved to trash"));
    assert!(!path.exists()); assert_eq!(fs::read(payload).unwrap(), b"synthetic chat");
    assert!(fs::read_to_string(metadata).unwrap().contains(&format!("Path={}\n", encoded_path(&path))));
}
