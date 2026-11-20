//! `packages/electron-utils/src/unsaved-files.ts`: "is this file open
//! somewhere with unsaved changes?", asked before a file is read as another
//! document's material.
//!
//! The TypeScript keeps the host's list source behind a `Symbol.for` key on
//! `globalThis`; here the holder is an explicit value the caller owns and
//! passes to every question. The source's promise is a plain call: a source
//! that throws or rejects is an `Err`.

use std::collections::HashSet;
use std::fs;

use jscompat::node_path::host::resolve;

/// what the host knows about one open document
#[derive(Debug, Clone, PartialEq)]
pub struct OpenFileState {
    /// absent for a document that was never saved
    pub file_path: Option<String>,
    pub dirty: bool,
}

/// What a failing source (or a list the loop cannot read) comes to: the
/// TypeScript's `catch` answers 'unknown' for every one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFilesSourceFailed;

/// `OpenFilesSource`. The error of a source call, and of reading an entry of
/// its list (a `null` entry, a path `resolve` refuses), is [`OpenFilesSourceFailed`].
pub type OpenFilesSource = Box<dyn Fn() -> Result<Vec<OpenFileState>, OpenFilesSourceFailed>>;

/// what a refused read answers with; the caller's own error path carries it
pub const UNSAVED_CHANGES_MESSAGE: &str =
    "This file is open with unsaved changes. Save it, then try again.";

/// The `Symbol.for('seuoffice.open-files-source')` slot.
#[derive(Default)]
pub struct OpenFilesHolder {
    source: Option<OpenFilesSource>,
}

/// A data-only source answer. Path canonicalization is deferred until the
/// worker reaches the original unsaved-check boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenFilesSnapshot {
    NoSource,
    Unknown,
    Documents(Vec<OpenFileState>),
}

impl OpenFilesSnapshot {
    pub fn file_has_unsaved_changes(&self, file_path: &str) -> bool {
        file_has_unsaved_keys(snapshot_unsaved_keys(self), file_path)
    }
}

impl OpenFilesHolder {
    pub fn new() -> Self {
        Self::default()
    }

    /// the host's list of open documents; `None` removes it
    pub fn set_open_files_source(&mut self, source: Option<OpenFilesSource>) {
        self.source = source;
    }

    pub fn capture_snapshot(&self) -> OpenFilesSnapshot {
        match &self.source {
            None => OpenFilesSnapshot::NoSource,
            Some(source) => match source() {
                Ok(documents) => OpenFilesSnapshot::Documents(documents),
                Err(_) => OpenFilesSnapshot::Unknown,
            },
        }
    }
}

/// one spelling per file: resolved, through links, case-folded where the disk is
fn path_key(value: &str) -> String {
    let mut resolved = resolve(&[value]);
    // realpathSync.native: a moved or deleted file compares as resolved
    if let Ok(real) = fs::canonicalize(&resolved) {
        let real = real.to_string_lossy().into_owned();
        // std adds the verbatim prefix on Windows; realpath does not
        resolved = match real.strip_prefix(r"\\?\") {
            Some(rest) if !rest.starts_with(r"UNC\") => rest.to_owned(),
            _ => real,
        };
    }
    if cfg!(windows) {
        resolved.to_lowercase()
    } else {
        resolved
    }
}

enum Unsaved {
    /// no source: nothing is blocked
    NoSource,
    /// the host could not say
    Unknown,
    Keys(HashSet<String>),
}

fn unsaved_keys(holder: &OpenFilesHolder) -> Unsaved {
    snapshot_unsaved_keys(&holder.capture_snapshot())
}

fn snapshot_unsaved_keys(snapshot: &OpenFilesSnapshot) -> Unsaved {
    let docs = match snapshot {
        OpenFilesSnapshot::NoSource => return Unsaved::NoSource,
        OpenFilesSnapshot::Unknown => return Unsaved::Unknown,
        OpenFilesSnapshot::Documents(documents) => documents,
    };
    let mut keys = HashSet::new();
    for doc in docs {
        if doc.dirty
            && let Some(path) = doc.file_path.as_deref().filter(|path| !path.is_empty())
        {
            keys.insert(path_key(path));
        }
    }
    Unsaved::Keys(keys)
}

pub fn file_has_unsaved_changes(holder: &OpenFilesHolder, file_path: &str) -> bool {
    file_has_unsaved_keys(unsaved_keys(holder), file_path)
}

fn file_has_unsaved_keys(keys: Unsaved, file_path: &str) -> bool {
    match keys {
        Unsaved::NoSource => false,
        Unsaved::Unknown => true,
        Unsaved::Keys(keys) => keys.contains(&path_key(file_path)),
    }
}

/// `Err(UNSAVED_CHANGES_MESSAGE)` for a file that must be saved before it is read
pub fn assert_file_saved(holder: &OpenFilesHolder, file_path: &str) -> Result<(), &'static str> {
    if file_has_unsaved_changes(holder, file_path) {
        Err(UNSAVED_CHANGES_MESSAGE)
    } else {
        Ok(())
    }
}

/// the items whose file is not open with unsaved changes, in their order
pub fn without_unsaved_files<T: Clone>(
    holder: &OpenFilesHolder,
    items: &[T],
    path_of: impl Fn(&T) -> String,
) -> Vec<T> {
    match unsaved_keys(holder) {
        Unsaved::NoSource => items.to_vec(),
        Unsaved::Unknown => Vec::new(),
        Unsaved::Keys(keys) => items
            .iter()
            .filter(|item| !keys.contains(&path_key(&path_of(item))))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod owned_snapshot_tests {
    use super::*;

    #[test]
    fn data_only_snapshot_is_send_and_distinguishes_unknown_from_no_source() {
        fn send<T: Send>() {}
        send::<OpenFilesSnapshot>();
        let mut holder = OpenFilesHolder::new();
        assert_eq!(holder.capture_snapshot(), OpenFilesSnapshot::NoSource);
        assert!(
            !holder
                .capture_snapshot()
                .file_has_unsaved_changes("/fixture.md")
        );
        holder.set_open_files_source(Some(Box::new(|| Err(OpenFilesSourceFailed))));
        assert_eq!(holder.capture_snapshot(), OpenFilesSnapshot::Unknown);
        assert!(
            holder
                .capture_snapshot()
                .file_has_unsaved_changes("/fixture.md")
        );
        holder.set_open_files_source(Some(Box::new(|| Ok(Vec::new()))));
        assert_eq!(
            holder.capture_snapshot(),
            OpenFilesSnapshot::Documents(Vec::new())
        );
        assert!(
            !holder
                .capture_snapshot()
                .file_has_unsaved_changes("/fixture.md")
        );
    }
}
