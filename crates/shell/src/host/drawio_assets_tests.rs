use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let parent = PathBuf::from(
            std::env::var_os("SEUOFFICE_NATIVE_TEST_SCRATCH")
                .expect("hosted test must supply an isolated scratch root"),
        );
        let root = parent.join(format!(
            "drawio-assets-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn rejects_untrusted_paths_without_accessing_files() {
    let absolute = if cfg!(windows) {
        Path::new("C:\\root")
    } else {
        Path::new("/root")
    };
    assert!(read(Path::new("relative-root"), Path::new("a"), 16).is_err());
    for path in [
        "",
        "../outside",
        "a/../../outside",
        "/absolute",
        "C:\\outside",
        "a\\outside",
        "a:stream",
    ] {
        assert!(read(absolute, Path::new(path), 16).is_err(), "{path}");
    }
}

#[test]
fn reads_regular_file_and_enforces_length_and_type() {
    let scratch = Scratch::new();
    let root = scratch.0.join("root");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("nested/a"), b"asset").unwrap();
    assert_eq!(read(&root, Path::new("nested/a"), 5).unwrap(), b"asset");
    assert!(read(&root, Path::new("nested/a"), 4).is_err());
    assert!(read(&root, Path::new("nested"), 16).is_err());
    fs::remove_file(root.join("nested/a")).unwrap();
    assert!(read(&root, Path::new("nested/a"), 16).is_err());
}

#[test]
fn exact_length_rejects_truncation_and_growth() {
    assert_eq!(read_exact_length(&mut &b"ok"[..], 2).unwrap(), b"ok");
    assert_eq!(
        read_exact_length(&mut &b"x"[..], 2).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        read_exact_length(&mut &b"xyz"[..], 2).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[cfg(unix)]
#[test]
fn symlink_components_never_escape_root() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    let root = scratch.0.join("root");
    let outside = scratch.0.join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("safe"), b"inside").unwrap();
    fs::write(outside.join("secret"), b"outside").unwrap();
    symlink(&outside, root.join("link")).unwrap();
    symlink(outside.join("secret"), root.join("file")).unwrap();
    symlink(&root, scratch.0.join("root-link")).unwrap();
    assert!(read(&root, Path::new("link/secret"), 100).is_err());
    assert!(read(&root, Path::new("file"), 100).is_err());
    assert!(read(&scratch.0.join("root-link"), Path::new("safe"), 100).is_err());
}

#[cfg(unix)]
#[test]
fn opened_handle_remains_original_after_path_replacement() {
    let scratch = Scratch::new();
    let root = scratch.0.join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("asset"), b"original").unwrap();
    let mut file = native::open(&root, &[std::ffi::OsStr::new("asset")]).unwrap();
    fs::rename(root.join("asset"), root.join("old")).unwrap();
    fs::write(root.join("asset"), b"different").unwrap();
    assert_eq!(read_file(&mut file, 32).unwrap(), b"original");
}

#[cfg(windows)]
#[test]
fn reparse_components_are_refused() {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let scratch = Scratch::new();
    let root = scratch.0.join("root");
    let outside = scratch.0.join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("safe"), b"inside").unwrap();
    fs::write(outside.join("secret"), b"outside").unwrap();
    symlink_dir(&outside, root.join("link")).unwrap();
    symlink_file(outside.join("secret"), root.join("file")).unwrap();
    symlink_dir(&root, scratch.0.join("root-link")).unwrap();
    assert!(read(&root, Path::new("link/secret"), 100).is_err());
    assert!(read(&root, Path::new("file"), 100).is_err());
    assert!(read(&scratch.0.join("root-link"), Path::new("safe"), 100).is_err());
}

#[test]
fn confinement_model_never_follows_replaced_link() {
    // A captured directory identity is retained even when its parent entry
    // changes; a subsequent walk must not follow the replacement link.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Entry {
        Directory(u8),
        Link(u8),
        Missing,
    }
    fn open_directory(entry: Entry) -> Option<u8> {
        match entry {
            Entry::Directory(id) => Some(id),
            _ => None,
        }
    }
    let original = 1;
    let outside = 2;
    for replacement in [
        Entry::Directory(original),
        Entry::Link(outside),
        Entry::Missing,
    ] {
        let already_open = open_directory(Entry::Directory(original)).unwrap();
        assert_eq!(already_open, original);
        let subsequent_open = open_directory(replacement);
        assert_ne!(subsequent_open, Some(outside));
        if matches!(replacement, Entry::Link(_) | Entry::Missing) {
            assert!(subsequent_open.is_none());
        }
    }
}

#[cfg(windows)]
#[test]
fn reads_below_a_verbatim_root() {
    // `fs::canonicalize` answers the `\\?\` form, the form the app's resources folder comes in
    let scratch = Scratch::new();
    let root = scratch.0.join("root");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("nested/a"), b"asset").unwrap();
    let verbatim = fs::canonicalize(&root).unwrap();
    assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
    assert_eq!(read(&verbatim, Path::new("nested/a"), 5).unwrap(), b"asset");
    assert!(read(&verbatim, Path::new("nested/../a"), 16).is_err());
}
