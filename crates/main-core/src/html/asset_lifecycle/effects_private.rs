//! A cleanup capability for one newly-created private payload, never for an
//! arbitrary directory, a renderer path, or a published output.
use super::{Arc, AssetAdmission, Effects, NodeError, R, Stage, create_private_stage, native, publish, refused};
use std::io::{Seek, SeekFrom};
use jscompat::node_path::host::{basename, dirname, join, resolve};

#[derive(Debug)]
struct OutputParent { directory: native::Directory, original_path: String }

/// Capture once at export approval. Numbered siblings retain this same parent.
#[derive(Debug, Clone)]
pub struct CapturedAssetOutput { parent: Arc<OutputParent>, name: String }

impl CapturedAssetOutput {
    pub fn capture(path: &str) -> R<Self> {
        let path = resolve(&[path]);
        let original_path = dirname(&path);
        let name = basename(&path, None);
        if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
            return Err(refused("invalid export output name"));
        }
        let directory = native::Directory::open(&original_path, true)?;
        Ok(Self { parent: Arc::new(OutputParent { directory, original_path }), name })
    }

    pub fn with_name(&self, name: &str) -> R<Self> {
        if name.is_empty() || name == "." || name == ".." || name.contains('\0') || basename(name, None) != name {
            return Err(refused("export sibling must be one file name"));
        }
        Ok(Self { parent: self.parent.clone(), name: name.into() })
    }

    fn check(&self) -> R<()> {
        if !self.parent.directory.matches_path(&self.parent.original_path, true)? {
            return Err(refused("export output parent changed"));
        }
        Ok(())
    }

    /// Staging beside the captured output keeps the original same-volume
    /// rename behavior, even for removable drives. Creation is an admitted
    /// namespace effect; later bulk writes affect only this private payload.
    pub fn create_payload(&self, admit: &mut AssetAdmission<'_>) -> R<PrivateAssetPayload> {
        let mut effects = Effects::new(admit);
        let mut diagnostics = Vec::new();
        let stage = create_private_stage(&self.parent.directory, &mut effects, &mut diagnostics, || self.check())?;
        Ok(PrivateAssetPayload { stage, diagnostics, cleaned: false })
    }
}

/// Native handles are Send. Drop this value outside authority/state gates:
/// cancellation cleanup never reacquires the request's authority or reopens a
/// parent path. It cannot remove a final output after successful publication.
#[must_use]
pub struct PrivateAssetPayload { stage: Stage, diagnostics: Vec<String>, cleaned: bool }

impl PrivateAssetPayload {
    pub fn write(&mut self, bytes: &[u8]) -> R<()> {
        if self.cleaned || self.stage.published { return Err(refused("private export payload is no longer writable")); }
        let display = join(&[&self.stage.directory.path, &self.stage.name]);
        self.stage.write(bytes, &display)
    }

    /// The caller supplies its real origin/destination admission for the exact
    /// publication. Cleanup authority alone never admits a destination write.
    pub fn publish(&mut self, target: &CapturedAssetOutput, replace: bool, admit: &mut AssetAdmission<'_>) -> R<()> {
        self.publish_inner(target, replace, false, admit)
    }

    fn publish_inner(&mut self, target: &CapturedAssetOutput, replace: bool, check_identity: bool, admit: &mut AssetAdmission<'_>) -> R<()> {
        if self.cleaned || self.stage.published { return Err(refused("private export payload was already consumed")); }
        let mut effects = Effects::new(admit);
        let result = effects.run(|| {
            target.check()?;
            publish(&target.parent.directory, &self.stage, &target.name, replace)?;
            self.stage.published = true;
            if check_identity {
                let file = self.stage.file.as_ref().ok_or_else(|| refused("asset stage has no file"))?;
                let expected = native::version(file, &self.stage.directory.path)?;
                if !target.parent.directory.matches(&target.name, &expected, false)? {
                    return Err(refused("published export output does not match the private payload"));
                }
            }
            Ok(())
        });
        result.map_err(|mut error| {
            if self.stage.published { error.message.push_str("; export output was already published"); }
            error
        })
    }

    /// Retain the staged object's readable handle after checking the destination.
    /// Windows renames the handle itself. Unix renames a private name, so the
    /// identity check is not atomic against another same-user namespace writer.
    pub fn publish_with_file(&mut self, target: &CapturedAssetOutput, replace: bool, admit: &mut AssetAdmission<'_>) -> R<std::fs::File> {
        if self.cleaned || self.stage.published { return Err(refused("private export payload was already consumed")); }
        let file = self.stage.file.as_mut().ok_or_else(|| refused("asset stage has no file"))?;
        file.seek(SeekFrom::Start(0)).map_err(|error| super::node_error(&error, "seek", &self.stage.directory.path))?;
        let retained = file.try_clone().map_err(|error| super::node_error(&error, "dup", &self.stage.directory.path))?;
        self.publish_inner(target, replace, true, admit)?;
        Ok(retained)
    }

    pub fn cleanup(&mut self) -> R<Vec<String>> {
        if self.cleaned { return Ok(Vec::new()); }
        if let Err(error) = self.stage.cleanup() {
            return Err(NodeError { code: error.code, message: format!("private export payload retained at {}: {}", join(&[&self.stage.directory.path, &self.stage.name]), error.message) });
        }
        #[cfg(windows)]
        if let Err(error) = native::remove_handle(&self.stage.directory.file, &self.stage.directory.path) {
            self.diagnostics.push(format!("private export staging retained at {}: {}", self.stage.directory.path, error.message));
        }
        // Unix deliberately leaves the empty outer entry: there is no
        // directory-unlink-if-inode primitive. Never unlink its mutable name.
        self.cleaned = true;
        Ok(std::mem::take(&mut self.diagnostics))
    }
}

impl Drop for PrivateAssetPayload {
    fn drop(&mut self) {
        match self.cleanup() {
            Ok(diagnostics) => for diagnostic in diagnostics { eprintln!("[export] {diagnostic}"); },
            Err(error) => eprintln!("[export] {}", error.message),
        }
    }
}
