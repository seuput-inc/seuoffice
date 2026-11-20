use std::any::Any;
use std::fs::{self, File};
use std::io::{self, Write};
use std::sync::Arc;

use super::{
    AtomicByteWriteBackend, atomic_write_file_with_backend, sync_file_best_effort, temp_path_beside,
};
use crate::node_fs::{NodeError, node_error, node_error2};

pub type EffectValue = Box<dyn Any + Send>;
pub type ShortEffect = Box<dyn FnOnce() -> EffectValue + Send>;
pub type Admission = Arc<dyn Fn(ShortEffect) -> Result<EffectValue, NodeError> + Send + Sync>;

pub fn admitted<T: Send + 'static>(
    admit: &Admission,
    effect: impl FnOnce() -> T + Send + 'static,
) -> Result<T, NodeError> {
    let value = admit(Box::new(move || Box::new(effect())))?;
    value
        .downcast::<T>()
        .map(|value| *value)
        .map_err(|_| NodeError {
            code: None,
            message: "The save admission returned an incompatible operation result".into(),
        })
}

pub fn write_file(path: &str, bytes: &[u8], admit: Admission) -> Result<(), NodeError> {
    let mut backend = Writer {
        path: path.to_owned(),
        tmp: temp_path_beside(path),
        admit,
    };
    atomic_write_file_with_backend(bytes, &mut backend)
}

pub fn write_json(
    path: &str,
    value: &jscompat::JsonValue,
    admit: Admission,
) -> Result<(), NodeError> {
    let mut backend = JsonWriter {
        path: path.to_owned(),
        tmp: temp_path_beside(path),
        admit,
    };
    super::write_json_atomic_with_backend(value, &mut backend)
}

struct JsonWriter {
    path: String,
    tmp: String,
    admit: Admission,
}

impl super::AtomicJsonWriteBackend for JsonWriter {
    fn ensure_parent(&mut self) -> Result<(), NodeError> {
        create_parent(&self.path, &self.admit)
    }
    fn stage(&mut self, json: &str) -> Result<Result<(), NodeError>, NodeError> {
        Ok((|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.tmp)
                .map_err(|error| node_error(&error, "open", &self.tmp))?;
            file.write_all(json.as_bytes())
                .map_err(|error| node_error(&error, "write", &self.tmp))?;
            drop(file);
            sync_file_best_effort(&self.tmp)
        })())
    }
    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError> {
        let from = self.tmp.clone();
        let to = self.path.clone();
        admitted(&self.admit, move || {
            fs::rename(&from, &to).map_err(|error| node_error2(&error, "rename", &from, &to))
        })
    }
    fn open_fallback(&mut self) -> Result<Result<File, NodeError>, NodeError> {
        let path = self.path.clone();
        admitted(&self.admit, move || {
            File::create(&path).map_err(|error| node_error(&error, "open", &path))
        })
    }
    fn remove_stage(&mut self) -> Result<(), NodeError> {
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }
    fn write_error(&self, error: &io::Error) -> NodeError {
        node_error(error, "write", &self.path)
    }
}

pub fn create_parent(path: &str, admit: &Admission) -> Result<(), NodeError> {
    let Some(parent) = std::path::Path::new(path).parent() else {
        return Ok(());
    };
    let mut missing = Vec::new();
    for directory in parent.ancestors() {
        if directory.as_os_str().is_empty() || directory.exists() {
            break;
        }
        missing.push(directory.to_path_buf());
    }
    for directory in missing.into_iter().rev() {
        admitted(admit, move || match fs::create_dir(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {
                Ok(())
            }
            Err(error) => Err(node_error(&error, "mkdir", &directory.to_string_lossy())),
        })??;
    }
    Ok(())
}

struct Writer {
    path: String,
    tmp: String,
    admit: Admission,
}

impl AtomicByteWriteBackend for Writer {
    fn stage(&mut self, data: &[u8]) -> Result<(), NodeError> {
        let mut file =
            File::create(&self.tmp).map_err(|error| node_error(&error, "open", &self.tmp))?;
        file.write_all(data)
            .map_err(|error| node_error(&error, "write", &self.tmp))?;
        drop(file);
        sync_file_best_effort(&self.tmp)
    }

    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError> {
        let from = self.tmp.clone();
        let to = self.path.clone();
        admitted(&self.admit, move || {
            fs::rename(&from, &to).map_err(|error| node_error2(&error, "rename", &from, &to))
        })
    }

    fn write_fallback(&mut self, data: &[u8]) -> Result<Result<(), NodeError>, NodeError> {
        let path = self.path.clone();
        let opened = admitted(&self.admit, move || {
            File::create(&path).map_err(|error| node_error(&error, "open", &path))
        })?;
        Ok(opened.and_then(|mut file| {
            file.write_all(data)
                .map_err(|error| node_error(&error, "write", &self.path))
        }))
    }

    fn copy_stage_to_rescue(&mut self) -> Result<Result<(), NodeError>, NodeError> {
        let from = self.tmp.clone();
        let to = rescue_path(&self.path);
        copy_rescue(&self.admit, &from, &to)
    }

    fn clear_rescue(&mut self) -> Result<(), NodeError> {
        let rescue = rescue_path(&self.path);
        admitted(&self.admit, move || {
            let _ = fs::remove_file(rescue);
        })
    }

    fn remove_stage(&mut self) -> Result<(), NodeError> {
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }
}

fn rescue_path(path: &str) -> String {
    let path = std::path::Path::new(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut end = name.len().min(200);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    path.with_file_name(format!("{}.unsaved.tmp", &name[..end]))
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn copy_rescue(
    admit: &Admission,
    from: &str,
    to: &str,
) -> Result<Result<(), NodeError>, NodeError> {
    crate::node_fs::copy_file_authorized(from, to, false, &mut |effect| {
        admitted(admit, || ())?;
        effect();
        Ok(())
    })
}

#[cfg(not(windows))]
fn copy_rescue(
    admit: &Admission,
    from: &str,
    to: &str,
) -> Result<Result<(), NodeError>, NodeError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let fail = |error: &io::Error| node_error2(error, "copyfile", from, to);
    let mut source = match File::open(crate::node_fs::namespaced(from)) {
        Ok(file) => file,
        Err(error) => return Ok(Err(fail(&error))),
    };
    let info = match source.metadata() {
        Ok(info) => info,
        Err(error) => return Ok(Err(fail(&error))),
    };
    let (dev, ino, mode) = (info.dev(), info.ino(), info.permissions().mode() & 0o7777);
    let target = to.to_owned();
    let original = from.to_owned();
    let destination = admitted(admit, move || {
        let mut options = fs::OpenOptions::new();
        let file = options
            .write(true)
            .create(true)
            .mode(mode)
            .open(crate::node_fs::namespaced(&target))
            .map_err(|error| node_error2(&error, "copyfile", &original, &target))?;
        let info = file
            .metadata()
            .map_err(|error| node_error2(&error, "copyfile", &original, &target))?;
        if info.dev() == dev && info.ino() == ino {
            return Ok(None);
        }
        file.set_len(0)
            .map_err(|error| node_error2(&error, "copyfile", &original, &target))?;
        Ok(Some(file))
    })?;
    let mut destination = match destination {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(Ok(())),
        Err(error) => return Ok(Err(error)),
    };
    let result = destination
        .set_permissions(fs::Permissions::from_mode(mode))
        .and_then(|()| io::copy(&mut source, &mut destination).map(|_| ()));
    if let Err(error) = result {
        drop(destination);
        let target = to.to_owned();
        admitted(admit, move || {
            let _ = fs::remove_file(crate::node_fs::namespaced(&target));
        })?;
        return Ok(Err(fail(&error)));
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refused_owned_commit_preserves_existing_file_and_removes_stage() {
        let root = std::env::temp_dir().join(format!("owned-write-{}", jscompat::random_uuid()));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("document.bin");
        fs::write(&target, b"original").unwrap();
        let admission: Admission = Arc::new(|_| {
            Err(NodeError {
                code: None,
                message: "owner released".into(),
            })
        });
        let error = write_file(&target.to_string_lossy(), b"replacement", admission).unwrap_err();
        assert_eq!(error.message, "owner released");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn owned_byte_and_json_writers_publish_real_bytes() {
        let root = std::env::temp_dir().join(format!("owned-write-{}", jscompat::random_uuid()));
        fs::create_dir_all(&root).unwrap();
        let admission: Admission = Arc::new(|effect| Ok(effect()));
        let target = root.join("document.bin");
        write_file(&target.to_string_lossy(), b"first", admission.clone()).unwrap();
        write_file(&target.to_string_lossy(), b"second", admission.clone()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
        let metadata = root.join("meta.json");
        let value = jscompat::JsonValue::Array(vec![jscompat::JsonValue::String("actual".into())]);
        write_json(&metadata.to_string_lossy(), &value, admission).unwrap();
        assert_eq!(fs::read_to_string(&metadata).unwrap(), "[\n  \"actual\"\n]");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }
}
