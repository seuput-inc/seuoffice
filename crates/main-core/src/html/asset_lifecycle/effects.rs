use crate::node_fs::{NodeError, namespaced, node_error, node_error_named};
use crate::packages::electron_utils::atomic_write::{
    AtomicByteWriteBackend, atomic_write_file_with_backend, rescue_path_for, temp_path_beside,
};
use jscompat::node_path::host::{basename, dirname, join, resolve};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, LazyLock, Mutex, Weak};

#[cfg(unix)]
#[path = "effects_unix.rs"]
mod native;
#[cfg(windows)]
#[path = "effects_windows.rs"]
mod native;
#[path = "effects_private.rs"]
mod private;
pub use private::{CapturedAssetOutput, PrivateAssetPayload};

pub type AssetAdmission<'a> =
    dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError> + 'a;
type R<T> = Result<T, NodeError>;
fn refused(message: &str) -> NodeError {
    NodeError {
        code: None,
        message: message.into(),
    }
}
/// `copyFile(from, to)` refused for the system error `code`: Node names both paths.
fn copyfile_error(code: &'static str, from: &str, to: &str) -> NodeError {
    let named = node_error_named(code, "copyfile", from);
    NodeError {
        code: named.code,
        message: format!("{} -> '{to}'", named.message),
    }
}

pub(crate) struct Effects<'a, 'f> {
    admit: &'a mut AssetAdmission<'f>,
    denied: Option<NodeError>,
}
impl<'a, 'f> Effects<'a, 'f> {
    pub(crate) fn new(admit: &'a mut AssetAdmission<'f>) -> Self {
        Self {
            admit,
            denied: None,
        }
    }
    fn admitted<T>(&mut self, effect: impl FnOnce() -> R<T>) -> R<R<T>> {
        self.admit_effect(effect, true)
    }
    fn cleanup_only<T>(&mut self, effect: impl FnOnce() -> R<T>) -> R<R<T>> {
        self.admit_effect(effect, false)
    }
    fn admit_effect<T>(&mut self, effect: impl FnOnce() -> R<T>, required: bool) -> R<R<T>> {
        if let Some(error) = &self.denied {
            return Err(error.clone());
        }
        let mut effect = Some(effect);
        let mut result = None;
        let mut repeated = false;
        let admission = (self.admit)(&mut || {
            let Some(effect) = effect.take() else {
                repeated = true;
                return Err(refused("asset effect repeated"));
            };
            let actual = effect();
            let status = actual.as_ref().map(|_| ()).map_err(Clone::clone);
            result = Some(actual);
            status
        });
        let outcome = if repeated {
            Err(refused("asset effect repeated"))
        } else {
            match (admission, result) {
                (Ok(()), Some(actual)) => Ok(actual),
                (Err(_), Some(Err(actual))) => Ok(Err(actual)),
                (Err(error), _) => Err(error),
                (Ok(()), None) => Err(refused("asset admission did not execute its effect")),
            }
        };
        if required && let Err(error) = &outcome {
            self.denied = Some(error.clone());
        }
        outcome
    }
    pub(crate) fn run<T>(&mut self, effect: impl FnOnce() -> R<T>) -> R<T> {
        self.admitted(effect)?
    }
    pub(crate) fn finish<T>(&self, result: R<T>) -> R<T> {
        match (&self.denied, result) {
            (None, result) => result,
            (Some(denied), Err(error)) if error.message.starts_with(&denied.message) => Err(error),
            (Some(denied), Err(error)) => {
                let mut denied = denied.clone();
                denied
                    .message
                    .push_str(&format!("; asset operation failed: {}", error.message));
                Err(denied)
            }
            (Some(denied), Ok(_)) => Err(denied.clone()),
        }
    }
}

static MANIFEST_LOCKS: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(crate) fn manifest_lock(document: &str) -> Arc<Mutex<()>> {
    let parent = dirname(&resolve(&[document]));
    let key = std::fs::canonicalize(namespaced(&parent))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(parent);
    let mut locks = MANIFEST_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, value| value.strong_count() != 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

#[derive(Debug)]
struct AssetDirectory {
    parent: native::Directory,
    parent_path: String,
    directory: native::Directory,
}
impl AssetDirectory {
    fn check(&self) -> R<()> {
        if !self.parent.matches_path(&self.parent_path, true)? {
            return Err(refused("asset document parent changed"));
        }
        let current = self
            .parent
            .child_directory("assets", false, false)?
            .ok_or_else(|| refused("asset directory disappeared"))?;
        if !native::version(&self.directory.file, &self.directory.path)?
            .same_object(&native::version(&current.file, &current.path)?)
        {
            return Err(refused("asset directory changed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PreparedAssetOwnership(Arc<AssetDirectory>);
impl PartialEq for PreparedAssetOwnership {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone)]
pub(crate) struct PinnedSource {
    inner: Arc<native::PinnedSource>,
    path: String,
}
impl PinnedSource {
    pub(crate) fn capture(root: &str, path: &str) -> R<Self> {
        Ok(Self {
            inner: Arc::new(native::pin_source(root, path)?),
            path: path.to_owned(),
        })
    }
}

pub(crate) fn capture_sources(
    source_document: &str,
    sources: &[String],
    resolve_source: impl Fn(&str) -> Option<String>,
) -> R<HashMap<String, (String, PinnedSource)>> {
    let mut result = HashMap::new();
    let mut captured = HashMap::<String, PinnedSource>::new();
    let mut root = None;
    for source in sources {
        if result.contains_key(source) {
            continue;
        }
        let Some(path) = resolve_source(source) else {
            continue;
        };
        let pinned = if let Some(existing) = captured.get(&path) {
            existing.clone()
        } else {
            if root.is_none() {
                let directory = dirname(&resolve(&[source_document]));
                let canonical = std::fs::canonicalize(namespaced(&directory))
                    .map_err(|error| node_error(&error, "realpath", &directory))?;
                root = Some(
                    canonical
                        .to_str()
                        .ok_or_else(|| refused("asset source root is not Unicode"))?
                        .to_owned(),
                );
            }
            let pinned = PinnedSource::capture(
                root.as_deref()
                    .ok_or_else(|| refused("asset source root is unavailable"))?,
                &path,
            )?;
            captured.insert(path.clone(), pinned.clone());
            pinned
        };
        result.insert(source.clone(), (path, pinned));
    }
    Ok(result)
}

struct Stage {
    directory: native::Directory,
    name: String,
    file: Option<File>,
    content_version: Option<native::Version>,
    published: bool,
    /// The pinned parent of the private directory and the entry name it has
    /// there: enough to remove exactly that directory again.
    #[cfg(unix)]
    container: native::Directory,
    #[cfg(unix)]
    entry: String,
}
impl Stage {
    fn cleanup(&mut self) -> R<()> {
        if self.published {
            return Ok(());
        }
        #[cfg(windows)]
        if self.file.is_none() {
            if let Some(expected) = &self.content_version {
                let file = self.directory.open_copy(&self.name)?;
                if !expected.same_object(&native::version(&file, &self.directory.path)?) {
                    return Err(refused("private payload identity changed before cleanup"));
                }
                self.file = Some(file);
            }
        }
        if self.file.is_none() {
            return Ok(());
        }
        #[cfg(unix)]
        let expected = native::version(
            self.file.as_ref().expect("stage file is present"),
            &self.directory.path,
        )?;
        #[cfg(windows)]
        {
            native::remove_handle(
                self.file.as_ref().expect("stage file is present"),
                &self.directory.path,
            )?;
        }
        #[cfg(unix)]
        self.directory.remove(&self.name, Some(&expected))?;
        self.file = None;
        self.content_version = None;
        Ok(())
    }
    fn write(&mut self, data: &[u8], display: &str) -> R<()> {
        self.file = Some(self.directory.create(&self.name)?);
        let file = self
            .file
            .as_mut()
            .expect("created private payload has a file");
        file.write_all(data)
            .map_err(|error| node_error(&error, "write", display))?;
        sync(file, display)?;
        #[cfg(windows)]
        {
            // Settle LastWriteTime before retaining the read/delete handle.
            self.content_version = Some(native::version(file, display)?);
            self.file = None;
            let reopened = self.directory.open_copy(&self.name)?;
            if !self
                .content_version
                .as_ref()
                .expect("private payload witness exists")
                .same_object(&native::version(&reopened, display)?)
            {
                return Err(refused("private asset payload changed before reopening"));
            }
            self.file = Some(reopened);
        }
        self.content_version = Some(native::version(
            self.file.as_ref().expect("private payload has a file"),
            display,
        )?);
        Ok(())
    }
    /// Removes the private directory this stage lives in, once its payload is
    /// published or gone. Only the directory this operation created, found
    /// again through its handle or its pinned parent, is ever removed.
    fn remove_directory(&self) -> R<()> {
        #[cfg(windows)]
        {
            native::remove_handle(&self.directory.file, &self.directory.path)
        }
        #[cfg(unix)]
        {
            self.container
                .remove_private_child(&self.entry, &self.directory)
        }
    }
}

fn create_private_stage(
    parent: &native::Directory,
    effects: &mut Effects<'_, '_>,
    validate: impl FnOnce() -> R<()>,
) -> R<Stage> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| refused(&error.to_string()))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!(".seuoffice-assets-stage-{suffix}");
    // Held before the directory exists: a failure here leaves nothing behind.
    #[cfg(unix)]
    let container = native::duplicate(parent)?;
    let directory = effects.run(|| {
        validate()?;
        parent
            .child_directory(&name, true, true)?
            .ok_or_else(|| refused("private asset staging was not created"))
    })?;
    Ok(Stage {
        directory,
        name: "payload".into(),
        file: None,
        content_version: None,
        published: false,
        #[cfg(unix)]
        container,
        #[cfg(unix)]
        entry: name,
    })
}

pub(crate) struct Session {
    document: String,
    directory: Option<Arc<AssetDirectory>>,
    manifest: Option<(String, Option<native::Version>)>,
    diagnostics: Vec<String>,
}
impl Drop for Session {
    fn drop(&mut self) {
        // Session is owned by the worker operation, never by an admission
        // closure. Diagnostics must not run while a native effect gate is held.
        for diagnostic in &self.diagnostics {
            eprintln!("[assets] {diagnostic}");
        }
    }
}
/// The TypeScript meets each recorded name with `lstat` first, so a name the
/// system refuses as too long is worded as that call, not as our `open`.
fn first_lstat(error: NodeError, directory: &str, name: &str) -> NodeError {
    if error.code == Some("ENAMETOOLONG") {
        node_error_named("ENAMETOOLONG", "lstat", &join(&[directory, name]))
    } else {
        error
    }
}

impl Session {
    /// `app` names the editor in the error Node's `existingAssetsDir` gives an
    /// assets path that is no real directory.
    pub(crate) fn open(
        document: &str,
        ownership: Option<&PreparedAssetOwnership>,
        app: &str,
    ) -> R<Self> {
        let parent_path = dirname(&resolve(&[document]));
        let directory = if let Some(ownership) = ownership {
            if ownership.0.parent_path != parent_path {
                return Err(refused("asset plan document parent changed"));
            }
            ownership.0.check()?;
            Some(ownership.0.clone())
        } else {
            match native::Directory::open(&parent_path, true) {
                Ok(parent) => match parent.child_directory("assets", false, false) {
                    Ok(found) => found.map(|directory| {
                        Arc::new(AssetDirectory {
                            parent,
                            parent_path,
                            directory,
                        })
                    }),
                    // a file or a link where the folder belongs
                    Err(error) if matches!(error.code, Some("ENOTDIR" | "ELOOP")) => {
                        let message = format!("{app} assets path must be a real directory");
                        return Err(refused(&message));
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if error.code == Some("ENOENT") => None,
                Err(error) => return Err(error),
            }
        };
        Ok(Self {
            document: document.into(),
            directory,
            manifest: None,
            diagnostics: Vec::new(),
        })
    }
    pub(crate) fn ownership(&self) -> Option<PreparedAssetOwnership> {
        self.directory
            .as_ref()
            .map(|d| PreparedAssetOwnership(d.clone()))
    }
    pub(crate) fn diagnostics(&self) -> Vec<String> {
        self.diagnostics.clone()
    }
    pub(crate) fn retained_asset(&mut self, name: &str, detail: &str) {
        let directory = self
            .directory
            .as_ref()
            .map(|directory| directory.directory.path.as_str());
        let path = match directory {
            Some(directory) => join(&[directory, name]),
            None => join(&[&dirname(&self.document), "assets", name]),
        };
        self.diagnostics
            .push(format!("prepared asset retained at {path}: {detail}"));
    }
    pub(crate) fn failure(&self, mut error: NodeError) -> NodeError {
        for diagnostic in &self.diagnostics {
            if !error.message.contains(diagnostic) {
                error.message.push_str("; ");
                error.message.push_str(diagnostic);
            }
        }
        error
    }
    pub(crate) fn exists(&self) -> bool {
        self.directory.is_some()
    }
    pub(crate) fn ensure(&mut self, effects: &mut Effects<'_, '_>) -> R<()> {
        if let Some(directory) = &self.directory {
            return directory.check();
        }
        let parent_path = dirname(&resolve(&[&self.document]));
        let parent = ensure_parent(&parent_path, effects)?;
        let directory = effects.run(|| {
            if !parent.matches_path(&parent_path, true)? {
                return Err(refused("asset document parent changed"));
            }
            parent
                .child_directory("assets", true, false)?
                .ok_or_else(|| refused("asset directory was not created"))
        })?;
        self.directory = Some(Arc::new(AssetDirectory {
            parent,
            parent_path,
            directory,
        }));
        Ok(())
    }
    fn dir(&self) -> R<&Arc<AssetDirectory>> {
        self.directory.as_ref().ok_or_else(|| {
            node_error_named(
                "ENOENT",
                "open",
                &join(&[&dirname(&self.document), "assets"]),
            )
        })
    }
    pub(crate) fn read_manifest(&mut self, path: &str) -> R<Option<Vec<u8>>> {
        let name = basename(path, None);
        let Some(directory) = &self.directory else {
            self.manifest = Some((name, None));
            return Ok(None);
        };
        directory.check()?;
        let found = match directory.directory.read(&name) {
            // a folder, a link or a special file where the list belongs
            Err(error) if matches!(error.code, Some("ELOOP" | "EISDIR" | "EINVAL")) => {
                return Err(refused("asset manifest path must be a real file"));
            }
            other => other?,
        };
        let Some(mut file) = found else {
            self.manifest = Some((name, None));
            return Ok(None);
        };
        let version = native::version(&file, path)?;
        let bytes = read_bytes(&mut file, path)?;
        if version != native::version(&file, path)? {
            return Err(refused("asset manifest changed during read"));
        }
        self.manifest = Some((name, Some(version)));
        Ok(Some(bytes))
    }
    pub(crate) fn owned(&self, name: &str, hash: &str) -> R<bool> {
        let directory = self.dir()?;
        directory.check()?;
        let Some(mut file) = (match directory.directory.read(name) {
            Err(error) if matches!(error.code, Some("ELOOP" | "EISDIR" | "EINVAL")) => {
                return Ok(false);
            }
            other => other.map_err(|error| first_lstat(error, &directory.directory.path, name)),
        })?
        else {
            return Err(node_error_named(
                "ENOENT",
                "open",
                &join(&[&directory.directory.path, name]),
            ));
        };
        let version = native::version(&file, name)?;
        let digest = hash_file(&mut file, name)?;
        Ok(digest == hash && version == native::version(&file, name)?)
    }
    pub(crate) fn remove_owned(
        &self,
        name: &str,
        hash: &str,
        effects: &mut Effects<'_, '_>,
    ) -> R<bool> {
        let directory = self.dir()?;
        directory.check()?;
        let Some(mut file) = (match directory.directory.read(name) {
            Err(error) if matches!(error.code, Some("ELOOP" | "EISDIR" | "EINVAL")) => {
                return Ok(false);
            }
            other => other.map_err(|error| first_lstat(error, &directory.directory.path, name)),
        })?
        else {
            return Err(node_error_named(
                "ENOENT",
                "unlink",
                &join(&[&directory.directory.path, name]),
            ));
        };
        let version = native::version(&file, name)?;
        if hash_file(&mut file, name)? != hash || version != native::version(&file, name)? {
            return Ok(false);
        }
        effects.run(|| {
            directory.check()?;
            directory.directory.remove(name, Some(&version))
        })?;
        Ok(true)
    }
    /// `unavailable` words a failure to make the private directory as the
    /// error Node gives the operation it stands in for: the TypeScript has no
    /// such directory and meets an unwritable folder at its own first write.
    /// It gets the system code and the path of the assets folder.
    fn stage(
        &mut self,
        effects: &mut Effects<'_, '_>,
        unavailable: impl FnOnce(&'static str, &str) -> NodeError,
    ) -> R<Stage> {
        self.ensure(effects)?;
        let directory = self.dir()?.clone();
        let made = create_private_stage(&directory.directory, effects, || directory.check());
        made.map_err(|error| match error.code {
            Some(code) => unavailable(code, &directory.directory.path),
            None => error,
        })
    }
    pub(crate) fn copy_pinned(
        &mut self,
        source: &PinnedSource,
        name: &str,
        effects: &mut Effects<'_, '_>,
    ) -> R<String> {
        let source_path = source.path.as_str();
        let mut stage = self.stage(effects, |code, assets| {
            copyfile_error(code, source_path, &join(&[assets, name]))
        })?;
        let result = (|| {
            #[cfg(windows)]
            {
                let path = join(&[&stage.directory.path, &stage.name]);
                let copied = source.inner.copy_to(&stage.directory, &stage.name);
                match stage.directory.open_copy(&stage.name) {
                    Ok(file) => stage.file = Some(file),
                    Err(error) if error.code == Some("ENOENT") && copied.is_err() => {}
                    Err(error) => {
                        self.diagnostics.push(format!(
                            "private asset payload may remain at {path}: {}",
                            error.message
                        ));
                        return Err(copied.err().unwrap_or(error));
                    }
                }
                copied?;
            }
            #[cfg(unix)]
            {
                source.inner.validate()?;
                let permissions = source.inner.permissions()?;
                stage.file = Some(stage.directory.create(&stage.name)?);
                let file = stage.file.as_mut().expect("created stage has a file");
                file.set_permissions(permissions)
                    .map_err(|e| node_error(&e, "fchmod", source_path))?;
                source.inner.copy_to(file)?;
            }
            let file = stage.file.as_mut().expect("copied stage has a file");
            sync(file, source_path)?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| node_error(&e, "seek", source_path))?;
            let version = native::version(file, source_path)?;
            let hash = hash_file(file, source_path)?;
            if native::version(file, source_path)? != version {
                return Err(refused("private asset payload changed during hashing"));
            }
            stage.content_version = Some(version);
            let directory = self.dir()?.clone();
            effects.run(|| {
                directory.check()?;
                publish(&directory.directory, &stage, name, false)
            })?;
            stage.published = true;
            Ok(hash)
        })();
        let result = cleanup_stage(&mut stage, result);
        self.finish_stage(&stage);
        result.map_err(|error| self.failure(error))
    }
    fn finish_stage(&mut self, stage: &Stage) {
        if let Err(error) = stage.remove_directory() {
            self.diagnostics.push(format!(
                "private asset staging retained at {}: {}",
                stage.directory.path, error.message
            ));
        }
    }
    pub(crate) fn write_manifest(
        &mut self,
        path: &str,
        bytes: &[u8],
        effects: &mut Effects<'_, '_>,
    ) -> R<()> {
        self.ensure(effects)?;
        let name = basename(path, None);
        let expected = match &self.manifest {
            Some((captured_name, expected)) if captured_name == &name => expected.clone(),
            _ => return Err(refused("manifest publication has no captured read")),
        };
        let directory = self.dir()?.clone();
        let rescue = basename(&rescue_path_for(path), None);
        let rescue_expected = match directory.directory.read(&rescue) {
            Ok(Some(file)) => native::version(&file, path).map(Some),
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        };
        let stage = self.stage(effects, |code, _| {
            node_error_named(code, "open", &temp_path_beside(path))
        })?;
        let mut backend = ManifestWriter {
            directory,
            name,
            path: path.into(),
            expected,
            rescue,
            rescue_expected,
            stage,
            effects,
            diagnostics: Vec::new(),
        };
        let result = atomic_write_file_with_backend(bytes, &mut backend);
        if result.is_err() && backend.stage.file.is_some() && !backend.stage.published {
            self.diagnostics.push(format!(
                "unpublished asset payload retained at {}",
                join(&[&backend.stage.directory.path, &backend.stage.name])
            ));
        }
        self.diagnostics.append(&mut backend.diagnostics);
        self.finish_stage(&backend.stage);
        result.map_err(|error| self.failure(error))
    }
}

fn ensure_parent(path: &str, effects: &mut Effects<'_, '_>) -> R<native::Directory> {
    match native::Directory::open(path, true) {
        Ok(parent) => Ok(parent),
        Err(error) if error.code == Some("ENOENT") => {
            let ancestor = dirname(path);
            if ancestor == path {
                return Err(error);
            }
            let parent = ensure_parent(&ancestor, effects)?;
            effects.run(|| {
                if !parent.matches_path(&ancestor, true)? {
                    return Err(refused("asset ancestor changed"));
                }
                parent
                    .child_directory(&basename(path, None), true, false)?
                    .ok_or_else(|| refused("asset parent was not created"))
            })
        }
        Err(error) => Err(error),
    }
}
fn read_bytes(file: &mut File, path: &str) -> R<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| node_error(&e, "read", path))?;
    Ok(bytes)
}
fn hash_file(file: &mut File, path: &str) -> R<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| node_error(&e, "read", path))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn sync(file: &File, path: &str) -> R<()> {
    if let Err(error) = file.sync_all() {
        #[cfg(unix)]
        let ignored = matches!(
            error.raw_os_error(),
            Some(
                libc::EPERM
                    | libc::EACCES
                    | libc::EBUSY
                    | libc::EINVAL
                    | libc::ENOSYS
                    | libc::ENOTSUP
            )
        );
        #[cfg(windows)]
        let ignored = matches!(error.raw_os_error(), Some(1 | 5 | 6 | 50 | 87));
        if !ignored {
            return Err(node_error(&error, "fsync", path));
        }
    }
    Ok(())
}
fn publish(directory: &native::Directory, stage: &Stage, name: &str, replace: bool) -> R<()> {
    let file = stage
        .file
        .as_ref()
        .ok_or_else(|| refused("asset stage has no file"))?;
    let expected = stage
        .content_version
        .as_ref()
        .ok_or_else(|| refused("asset stage has no content witness"))?;
    if &native::version(file, &stage.directory.path)? != expected {
        return Err(refused("private asset payload changed before publication"));
    }
    #[cfg(windows)]
    {
        directory.publish(file, name, replace)
    }
    #[cfg(unix)]
    {
        directory.publish(&stage.directory, &stage.name, file, name, replace)
    }
}
fn cleanup_stage<T>(stage: &mut Stage, result: R<T>) -> R<T> {
    match (result, stage.cleanup()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(NodeError {
            code: error.code,
            message: format!(
                "unpublished asset payload retained at {}: {}",
                join(&[&stage.directory.path, &stage.name]),
                error.message
            ),
        }),
        (Err(mut error), Err(cleanup)) => {
            error.message.push_str(&format!(
                "; unpublished asset payload retained at {}: {}",
                join(&[&stage.directory.path, &stage.name]),
                cleanup.message
            ));
            Err(error)
        }
    }
}

struct ManifestWriter<'a, 'b, 'f> {
    directory: Arc<AssetDirectory>,
    name: String,
    path: String,
    expected: Option<native::Version>,
    rescue: String,
    rescue_expected: R<Option<native::Version>>,
    stage: Stage,
    effects: &'a mut Effects<'b, 'f>,
    diagnostics: Vec<String>,
}
impl ManifestWriter<'_, '_, '_> {
    fn validate(&self, name: &str, expected: &Option<native::Version>) -> R<()> {
        self.directory.check()?;
        match expected {
            Some(expected) if self.directory.directory.matches(name, expected, true)? => Ok(()),
            None if self.directory.directory.read(name)?.is_none() => Ok(()),
            _ => Err(refused("asset manifest object changed before publication")),
        }
    }
}
impl AtomicByteWriteBackend for ManifestWriter<'_, '_, '_> {
    fn stage(&mut self, data: &[u8]) -> R<()> {
        self.stage.write(data, &self.path)
    }
    fn rename(&mut self) -> R<R<()>> {
        // Validation is not a retryable native rename error.
        self.validate(&self.name, &self.expected)?;
        let directory = &self.directory;
        let name = &self.name;
        let expected = &self.expected;
        let stage = &self.stage;
        let result = self.effects.admitted(|| {
            directory.check()?;
            match expected {
                Some(version) if directory.directory.matches(name, version, true)? => {}
                None if directory.directory.read(name)?.is_none() => {}
                _ => return Err(refused("asset manifest object changed before publication")),
            }
            publish(&directory.directory, stage, name, true)
        })?;
        if result.is_ok() {
            self.stage.published = true;
            self.stage.file = None;
        }
        Ok(result)
    }
    fn write_fallback(&mut self, data: &[u8]) -> R<R<()>> {
        self.validate(&self.name, &self.expected)?;
        let directory = &self.directory;
        let name = &self.name;
        let expected = &self.expected;
        let result = self.effects.admitted(|| {
            directory.check()?;
            match expected {
                Some(version) if directory.directory.matches(name, version, true)? => {}
                None if directory.directory.read(name)?.is_none() => {}
                _ => return Err(refused("asset manifest object changed before fallback")),
            }
            directory.directory.fallback(name)
        })?;
        Ok(match result {
            Ok(mut file) => file
                .write_all(data)
                .map_err(|error| node_error(&error, "write", &self.path)),
            Err(error) => Err(error),
        })
    }
    fn copy_stage_to_rescue(&mut self) -> R<R<()>> {
        let rescue_expected = match &self.rescue_expected {
            Ok(expected) => expected,
            Err(error) => return Ok(Err(error.clone())),
        };
        self.validate(&self.rescue, rescue_expected)?;
        #[cfg(windows)]
        {
            let source_path = join(&[&self.stage.directory.path, &self.stage.name]);
            let private_name = "rescue";
            let private_path = join(&[&self.stage.directory.path, private_name]);
            let expected_source = native::version(
                self.stage
                    .file
                    .as_ref()
                    .ok_or_else(|| refused("asset rescue stage missing"))?,
                &source_path,
            )?;
            let source = self
                .stage
                .file
                .take()
                .expect("validated rescue stage is present");
            drop(source);
            // CopyFileW, not a byte loop: retain the original attributes/EFS
            // semantics in the pinned private container before publication.
            let copied = crate::node_fs::copy_file(&source_path, &private_path, true);
            let reopened = self.stage.directory.open_copy(&self.stage.name);
            let private = self.stage.directory.open_copy(private_name);
            let mut published = false;
            let result = (|| {
                let source = reopened?;
                if native::version(&source, &source_path)? != expected_source {
                    return Err(refused("private asset source changed during rescue copy"));
                }
                self.stage.file = Some(source);
                if let Err(error) = copied {
                    return Ok(Err(error));
                }
                let file = private.as_ref().map_err(Clone::clone)?;
                let directory = &self.directory;
                let rescue = &self.rescue;
                let expected = rescue_expected;
                let result = self.effects.admitted(|| {
                    directory.check()?;
                    match expected {
                        Some(version) if directory.directory.matches(rescue, version, true)? => {}
                        None if directory.directory.read(rescue)?.is_none() => {}
                        _ => return Err(refused("asset rescue object changed before publication")),
                    }
                    directory.directory.publish(file, rescue, true)
                })?;
                published = result.is_ok();
                Ok(result)
            })();
            if !published {
                match &private {
                    Ok(file) => {
                        if let Err(error) = native::remove_handle(file, &private_path) {
                            self.diagnostics.push(format!(
                                "private rescue payload retained at {private_path}: {}",
                                error.message
                            ));
                        }
                    }
                    Err(error) if error.code != Some("ENOENT") => self.diagnostics.push(format!(
                        "private rescue payload may remain at {private_path}: {}",
                        error.message
                    )),
                    Err(_) => {}
                }
            }
            result
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let directory = &self.directory;
            let rescue = &self.rescue;
            let expected = rescue_expected;
            let opened = self.effects.admitted(|| {
                directory.check()?;
                match expected {
                    Some(version) if directory.directory.matches(rescue, version, true)? => {}
                    None if directory.directory.read(rescue)?.is_none() => {}
                    _ => return Err(refused("asset rescue object changed before fallback")),
                }
                directory.directory.open_rescue(rescue)
            })?;
            let mut destination = match opened {
                Ok(file) => file,
                Err(error) => return Ok(Err(error)),
            };
            let source = self
                .stage
                .file
                .as_mut()
                .ok_or_else(|| refused("asset rescue stage missing"))?;
            let metadata = match source.metadata() {
                Ok(metadata) => metadata,
                Err(error) => return Ok(Err(node_error(&error, "fstat", &self.path))),
            };
            let target = match destination.metadata() {
                Ok(metadata) => metadata,
                Err(error) => return Ok(Err(node_error(&error, "fstat", &self.path))),
            };
            if metadata.dev() == target.dev() && metadata.ino() == target.ino() {
                return Ok(Ok(()));
            }
            if let Err(error) = destination.set_len(0) {
                return Ok(Err(node_error(&error, "open", &self.path)));
            }
            source
                .seek(SeekFrom::Start(0))
                .map_err(|error| node_error(&error, "seek", &self.path))?;
            let copied = destination
                .set_permissions(std::fs::Permissions::from_mode(metadata.mode() & 0o7777))
                .and_then(|()| std::io::copy(source, &mut destination).map(|_| ()));
            if let Err(error) = copied {
                let copy_error = node_error(&error, "copyfile", &self.path);
                match native::version(&destination, &self.path) {
                    Ok(version) => {
                        let removed = self.effects.admitted(|| {
                            directory.check()?;
                            directory.directory.remove(rescue, Some(&version))
                        })?;
                        if let Err(error) = removed {
                            self.diagnostics.push(format!(
                                "partial asset rescue retained at {}: {}",
                                join(&[&directory.directory.path, rescue]),
                                error.message
                            ));
                        }
                    }
                    Err(error) => self.diagnostics.push(format!(
                        "partial asset rescue retained at {}: {}",
                        join(&[&directory.directory.path, rescue]),
                        error.message
                    )),
                }
                return Ok(Err(copy_error));
            }
            Ok(Ok(()))
        }
    }
    fn clear_rescue(&mut self) -> R<()> {
        let expected = match &self.rescue_expected {
            Ok(Some(expected)) => expected,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.diagnostics.push(format!(
                    "obsolete asset rescue retained at {}: {}",
                    join(&[&self.directory.directory.path, &self.rescue]),
                    error.message
                ));
                return Ok(());
            }
        };
        let directory = &self.directory;
        let rescue = &self.rescue;
        let result = self.effects.cleanup_only(|| {
            directory.check()?;
            directory.directory.remove(rescue, Some(expected))
        });
        if let Err(error) = result.and_then(|result| result) {
            self.diagnostics.push(format!(
                "obsolete asset rescue retained at {}: {}",
                join(&[&directory.directory.path, rescue]),
                error.message
            ));
        }
        Ok(())
    }
    fn remove_stage(&mut self) -> R<()> {
        self.stage.cleanup().map_err(|error| NodeError {
            code: error.code,
            message: format!(
                "unpublished asset payload retained at {}: {}",
                join(&[&self.stage.directory.path, &self.stage.name]),
                error.message
            ),
        })
    }
}

#[cfg(test)]
#[path = "effects_tests.rs"]
mod tests;
