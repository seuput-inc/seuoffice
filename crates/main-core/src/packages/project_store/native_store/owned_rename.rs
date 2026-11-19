use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub type Admission<'a> = dyn FnMut(&mut dyn FnMut()) -> Result<(), String> + 'a;

fn admitted<T>(admit: &mut Admission<'_>, operation: impl FnOnce() -> T) -> Result<T, String> {
    let mut operation = Some(operation);
    let mut value = None;
    admit(&mut || {
        if let Some(operation) = operation.take() {
            value = Some(operation());
        }
    })?;
    value.ok_or_else(|| "project rename effect was not admitted".into())
}

pub(super) struct StagedJson {
    target: PathBuf,
    temporary: PathBuf,
    published: bool,
}
impl StagedJson {
    pub(super) fn prepare(
        path: &Path,
        value: &Value,
        admit: &mut Admission<'_>,
    ) -> Result<Self, String> {
        let parent = path.parent().ok_or("missing parent")?;
        admitted(admit, || fs::create_dir_all(parent))?.map_err(|error| error.to_string())?;
        let mut random = [0; 6];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{suffix}.tmp"));
        let temporary = PathBuf::from(name);
        let mut file = admitted(admit, || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
        })?
        .map_err(|error| error.to_string())?;
        let staged = Self {
            target: path.to_owned(),
            temporary,
            published: false,
        };
        let result = serde_json::to_writer_pretty(&mut file, value)
            .map_err(io::Error::from)
            .and_then(|()| file.flush());
        drop(file);
        result.map_err(|error| error.to_string())?;
        Ok(staged)
    }
    pub(super) fn publish(&mut self, admit: &mut Admission<'_>) -> Result<(), String> {
        admitted(admit, || fs::rename(&self.temporary, &self.target))?
            .map_err(|error| error.to_string())?;
        self.published = true;
        Ok(())
    }
}
impl Drop for StagedJson {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

pub(super) trait RenameIo {
    fn json(&mut self, path: &Path, value: &Value) -> Result<(), String>;
    fn trash(&mut self, source: &Path, directory: &Path, target: &Path) -> Result<(), String>;
    fn forget(
        &mut self,
        sequences: &mut HashMap<(String, String), f64>,
        key: (String, String),
    ) -> Result<(), String>;
}

pub(super) struct NativeIo<'a, 'b> {
    pub(super) admit: &'a mut Admission<'b>,
}
impl RenameIo for NativeIo<'_, '_> {
    fn json(&mut self, path: &Path, value: &Value) -> Result<(), String> {
        StagedJson::prepare(path, value, self.admit)?.publish(self.admit)
    }
    fn trash(&mut self, source: &Path, directory: &Path, target: &Path) -> Result<(), String> {
        let made = admitted(self.admit, || fs::create_dir_all(directory))?;
        let result = match made {
            Ok(()) => admitted(self.admit, || fs::rename(source, target))?,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            eprintln!(
                "[project-store] fileRenamed move of displaced chat to trash failed: {error}"
            );
        }
        Ok(())
    }
    fn forget(
        &mut self,
        sequences: &mut HashMap<(String, String), f64>,
        key: (String, String),
    ) -> Result<(), String> {
        admitted(self.admit, || {
            sequences.remove(&key);
        })
    }
}

enum Operation {
    Json(StagedJson),
    Trash {
        source: PathBuf,
        directory: PathBuf,
        target: PathBuf,
    },
    Forget((String, String)),
}
struct PrepareIo<'a, 'b> {
    operations: Vec<Operation>,
    admit: &'a mut Admission<'b>,
}
impl RenameIo for PrepareIo<'_, '_> {
    fn json(&mut self, path: &Path, value: &Value) -> Result<(), String> {
        self.operations.push(Operation::Json(StagedJson::prepare(
            path, value, self.admit,
        )?));
        Ok(())
    }
    fn trash(&mut self, source: &Path, directory: &Path, target: &Path) -> Result<(), String> {
        self.operations.push(Operation::Trash {
            source: source.to_owned(),
            directory: directory.to_owned(),
            target: target.to_owned(),
        });
        Ok(())
    }
    fn forget(
        &mut self,
        _: &mut HashMap<(String, String), f64>,
        key: (String, String),
    ) -> Result<(), String> {
        self.operations.push(Operation::Forget(key));
        Ok(())
    }
}

/// Capture only while the shared store is briefly borrowed. Preparing the
/// original algorithm performs all reads, JSON encoding and temp IO off-lock.
pub struct RenameContext {
    base: PathBuf,
    revision: Arc<AtomicU64>,
    expected: u64,
}
impl RenameContext {
    pub fn prepare(self, old: &str, new: &str, admit: &mut Admission<'_>) -> PreparedRename {
        let mut reader = NativeProjectStore {
            base: self.base.clone(),
            sequences: HashMap::new(),
            pending: HashMap::new(),
            revision: self.revision.clone(),
        };
        let mut io = PrepareIo {
            operations: Vec::new(),
            admit,
        };
        let terminal = reader.file_renamed_with_io(old, new, &mut io).err();
        PreparedRename {
            base: self.base,
            expected: self.expected,
            operations: io.operations,
            terminal,
            started: false,
        }
    }
}

pub struct PreparedRename {
    base: PathBuf,
    expected: u64,
    operations: Vec<Operation>,
    terminal: Option<String>,
    started: bool,
}
pub enum RenameCommit {
    Retry,
    Applied(Result<(), String>),
}
impl PreparedRename {
    /// Enter the request's native admission first, then try_lock the shared
    /// store. Neither gate may wait for the other. On contention, retry only
    /// after both dropped. Temp cleanup is deferred to the caller's plan drop.
    pub fn commit(
        &mut self,
        store: &mut NativeProjectStore,
        admit: &mut Admission<'_>,
    ) -> RenameCommit {
        if self.started {
            return RenameCommit::Applied(Err("project rename plan already started".into()));
        }
        if store.base != self.base || store.revision.load(Ordering::Acquire) != self.expected {
            return RenameCommit::Retry;
        }
        self.started = true;
        store.revision.fetch_add(1, Ordering::AcqRel);
        let mut native = NativeIo { admit };
        for operation in &mut self.operations {
            let result = match operation {
                Operation::Json(staged) => staged.publish(native.admit),
                Operation::Trash {
                    source,
                    directory,
                    target,
                } => native.trash(source, directory, target),
                Operation::Forget(key) => admitted(native.admit, || {
                    store.sequences.remove(&*key);
                }),
            };
            if let Err(error) = result {
                return RenameCommit::Applied(Err(error));
            }
        }
        RenameCommit::Applied(match self.terminal.take() {
            Some(error) => Err(error),
            None => Ok(()),
        })
    }
}

impl NativeProjectStore {
    pub fn prepare_rename_context(&self) -> RenameContext {
        RenameContext {
            base: self.base.clone(),
            revision: self.revision.clone(),
            expected: self.revision.load(Ordering::Acquire),
        }
    }
}

#[cfg(test)]
mod tests;
