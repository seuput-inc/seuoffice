//! `apps/shell/src/main/projects/new-documents.ts`: new documents the AI makes
//! while a project is chosen (spec 6, invariant 3).
//!
//! The folder rule: the folder of the window's current document when that
//! document is inside the project folder (not under `.seuoffice/`), else the
//! project folder itself. The home ask has no document, so it is the top. A
//! name already taken gets " (2)", " (3)"... and is never written over. The
//! bytes go through the project guide's write guard only (`writeNewDocument`),
//! after the document was made: a failed make writes nothing.
//!
//! A tab a create flow opened (the create window, a template copy, the AI's new
//! Word document) carries the placement of the window that opened it, so the
//! new tab's AI panel starts in that project and its first save lands in the
//! same folder. Without a project nothing here applies: callers keep their
//! behavior of today.
//!
//! No host object here: windows are named by webContents id and the shell
//! passes how to read a window's project and document. What the TypeScript
//! awaits is a plain synchronous call: a promise that rejects is an `Err` of
//! [`NodeError`], and the placement a create flow hands to `preset` is the
//! result of its own call, settled already.

use std::collections::HashMap;

use jscompat::node_path::host::join;
use jscompat::{js_regex, number_to_string, truthy};
use serde::{Deserialize, Serialize};

use super::folder_files::is_data_dir_name;
use super::paths::relative_inside;
use super::write_guard::{ProjectWriteRefusal, ProjectWriteRefused, WriteGuardError};
use crate::node_fs::{NodeError, namespaced};
use crate::packages::project_store::{ProjectId, ProjectWriteResult};

/// `NewDocumentPlacement`: where one window's new documents go
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDocumentPlacement {
    pub project_id: ProjectId,
    /// the project folder (canonical, as listed)
    pub root: String,
    /// the folder new documents are written into: `root` or a folder below it
    pub dir: String,
}

/// `NewDocumentOpener`: who opened a create flow: the home ask, or an editor
/// window
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NewDocumentOpener {
    Home,
    #[serde(rename_all = "camelCase")]
    Window {
        web_contents_id: i64,
    },
}

/// `NEW_DOCUMENT_NAME_ATTEMPTS`: names tried for one new document before
/// giving up (a folder full of copies)
pub const NEW_DOCUMENT_NAME_ATTEMPTS: u32 = 1000;

/// `newDocumentFolder(root, docPath, platform)`: the folder rule (spec 6):
/// `docPath`'s folder when it is inside `root` and not under the data folder
/// (by any name it has had), else `root`. The result keeps the root's
/// spelling.
pub fn new_document_folder(root: &str, doc_path: Option<&str>, platform: &str) -> String {
    let Some(doc_path) = truthy(doc_path) else {
        return root.to_owned();
    };
    let Some(rel) = relative_inside(root, doc_path, platform) else {
        return root.to_owned();
    };
    let mut segments: Vec<&str> = rel.split('/').collect();
    segments.pop();
    let Some(first) = segments.first() else {
        return root.to_owned();
    };
    if is_data_dir_name(first) {
        return root.to_owned();
    }
    let mut parts: Vec<&str> = vec![root];
    parts.extend(segments);
    join(&parts)
}

/// `isBareFileName(fileName)`: whether `fileName` names a file only: no
/// folder part (either slash), no drive, not `.` or `..`. The folder always
/// comes from the placement, never from whoever named the document (an
/// editor passes a renderer's name on).
pub fn is_bare_file_name(file_name: &str) -> bool {
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return false;
    }
    !js_regex!(r"[\\/:]").test(file_name)
}

/// `numberedFileName(fileName, n)`: `name.ext` for 1, `name (2).ext` for 2,
/// and so on (the extension stays last)
pub fn numbered_file_name(file_name: &str, n: f64) -> String {
    if n <= 1.0 {
        return file_name.to_owned();
    }
    // `lastIndexOf('.')` is 0 only for a name that starts with the dot, and
    // `dot > 0` is the same test on the byte index
    let (base, ext) = match file_name.rfind('.') {
        Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
        _ => (file_name, ""),
    };
    format!("{base} ({}){ext}", number_to_string(n))
}

/// `entryExists(path)`: `lstat` answers
fn entry_exists(path: &str) -> bool {
    std::fs::symlink_metadata(namespaced(path)).is_ok()
}

/// `NewDocumentPlacerDeps.guide`: `Pick<ProjectGuide, 'projectRoot' | 'writeNewDocument'>`
pub trait NewDocumentsGuide {
    /// `projectRoot(id)`: the canonical folder of a listed project, or `None` (`null`)
    fn project_root(&mut self, id: &str) -> Option<String>;

    /// `writeNewDocument(id, target, data)`: throws `ProjectWriteRefused` (or
    /// the file system error); callers pick a free name
    fn write_new_document(
        &mut self,
        id: &str,
        target: &str,
        data: &[u8],
    ) -> Result<ProjectWriteResult, WriteGuardError>;
}

/// `NewDocumentPlacerDeps`
pub struct NewDocumentPlacerDeps<G: NewDocumentsGuide> {
    pub guide: G,
    /// the project the window's AI panel has chosen now; `None` for none or
    /// no AI window
    pub window_project: Box<dyn FnMut(i64) -> Result<Option<ProjectId>, NodeError>>,
    /// the window's saved document, as the shell knows it; `None` when
    /// untitled or unknown
    pub document_of: Box<dyn FnMut(i64) -> Option<String>>,
    /// `NodeJS.Platform` (`process.platform` where the TypeScript leaves it out)
    pub platform: String,
}

/// `NewDocumentPlacer`
pub struct NewDocumentPlacer<G: NewDocumentsGuide> {
    deps: NewDocumentPlacerDeps<G>,
    /// tabs opened by a create flow → the placement they were opened with
    presets: HashMap<i64, PresetEntry>,
    prepared_slots: std::rc::Rc<std::cell::Cell<usize>>,
}

struct PresetEntry {
    identity: std::rc::Rc<std::cell::Cell<bool>>,
    placement: Option<NewDocumentPlacement>,
}

pub struct PreparedNewDocumentPreset {
    id: i64,
    entry: Option<PresetEntry>,
    expected: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    slots: std::rc::Rc<std::cell::Cell<usize>>,
}

impl Drop for PreparedNewDocumentPreset {
    fn drop(&mut self) { self.slots.set(self.slots.get() - 1); }
}

pub struct ValidatedNewDocumentPreset<'a, G: NewDocumentsGuide> {
    placer: &'a mut NewDocumentPlacer<G>,
    prepared: PreparedNewDocumentPreset,
}

impl<G: NewDocumentsGuide> ValidatedNewDocumentPreset<'_, G> {
    pub fn commit(mut self) -> NewDocumentPresetReceipt {
        let entry = self.prepared.entry.take().expect("validated preset entry");
        let identity = entry.identity.clone();
        let previous = self.placer.presets.insert(self.prepared.id, entry);
        NewDocumentPresetReceipt { id: self.prepared.id, identity, previous, restored: false }
    }
}

pub struct NewDocumentPresetReceipt {
    id: i64,
    identity: std::rc::Rc<std::cell::Cell<bool>>,
    previous: Option<PresetEntry>,
    restored: bool,
}

pub struct NewDocumentPlacementSeed {
    preset: Option<NewDocumentPlacement>,
    document: Option<String>,
    platform: String,
}

impl NewDocumentPlacementSeed {
    pub fn preset_project(&self) -> Option<ProjectId> {
        self.preset.as_ref().map(|placement| placement.project_id.clone())
    }

    pub fn resolve(self, project_id: String, root: String, file_name: &str) -> Result<NewDocumentWrite, WriteGuardError> {
        NewDocumentWrite::new(self.resolve_placement(project_id, root), file_name)
    }

    pub fn resolve_placement(self, project_id: String, root: String) -> NewDocumentPlacement {
        match self.preset {
            Some(preset) if preset.project_id == project_id && preset.root == root => preset,
            _ => NewDocumentPlacement {
                project_id, dir: new_document_folder(&root, self.document.as_deref(), &self.platform), root,
            },
        }
    }
}

/// Placement captured on core; filename selection and filesystem writes run on a worker.
pub struct NewDocumentWrite {
    placement: NewDocumentPlacement,
    file_name: String,
}

impl NewDocumentWrite {
    pub fn placement(&self) -> &NewDocumentPlacement { &self.placement }

    pub fn execute(
        &self,
        bytes: &[u8],
        mut write: impl FnMut(&str, &str, &[u8]) -> Result<ProjectWriteResult, WriteGuardError>,
    ) -> Result<String, WriteGuardError> {
        for n in 1..=NEW_DOCUMENT_NAME_ATTEMPTS {
            let target = join(&[&self.placement.dir, &numbered_file_name(&self.file_name, f64::from(n))]);
            if entry_exists(&target) { continue; }
            match write(&self.placement.project_id, &target, bytes) {
                Ok(_) => return Ok(target),
                Err(WriteGuardError::Refused(refused)) if refused.reason == ProjectWriteRefusal::Exists => {}
                Err(error) => return Err(error),
            }
        }
        Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, join(&[&self.placement.dir, &self.file_name])).into())
    }
    pub fn execute_with_receipt(
        &self,bytes:&[u8],
        mut write:impl FnMut(&str,&str,&[u8])->Result<file_commit::FileCommitReceipt,WriteGuardError>,
    )->Result<file_commit::FileCommitReceipt,WriteGuardError> {
        for n in 1..=NEW_DOCUMENT_NAME_ATTEMPTS {
            let target=join(&[&self.placement.dir,&numbered_file_name(&self.file_name,f64::from(n))]);
            if entry_exists(&target) { continue; }
            match write(&self.placement.project_id,&target,bytes) {
                Ok(commit) if commit.path()==std::path::Path::new(&target)=>return Ok(commit),
                Ok(_) => return Err(NodeError { code:None,message:"project commit path changed".into() }.into()),
                Err(WriteGuardError::Refused(refused)) if refused.reason==ProjectWriteRefusal::Exists=>{},
                Err(error)=>return Err(error),
            }
        }
        Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists,join(&[&self.placement.dir,&self.file_name])).into())
    }


    fn new(placement: NewDocumentPlacement, file_name: &str) -> Result<Self, WriteGuardError> {
        if !is_bare_file_name(file_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, file_name).into());
        }
        Ok(Self { placement, file_name: file_name.to_owned() })
    }
}

impl<G: NewDocumentsGuide> NewDocumentPlacer<G> {
    pub fn new(deps: NewDocumentPlacerDeps<G>) -> Self {
        Self {
            deps,
            presets: HashMap::new(),
            prepared_slots: std::rc::Rc::new(std::cell::Cell::new(0)),
        }
    }

    /// `placementFor(webContentsId)`: where `webContentsId`'s new documents
    /// go now, or `None` when its panel has no project (the caller then does
    /// what it did before projects existed).
    pub fn placement_for(
        &mut self,
        web_contents_id: i64,
    ) -> Result<Option<NewDocumentPlacement>, NodeError> {
        let project_id = (self.deps.window_project)(web_contents_id)?;
        let Some(project_id) = truthy(project_id.as_deref()) else {
            return Ok(None);
        };
        let root = self.deps.guide.project_root(project_id);
        let Some(root) = truthy(root.as_deref()) else {
            return Ok(None);
        };
        let preset = self.preset_of(web_contents_id);
        if let Some(preset) = preset
            && preset.project_id == project_id
            && preset.root == root
        {
            return Ok(Some(preset));
        }
        let document = (self.deps.document_of)(web_contents_id);
        Ok(Some(NewDocumentPlacement {
            project_id: project_id.to_owned(),
            root: root.to_owned(),
            dir: new_document_folder(root, document.as_deref(), &self.deps.platform),
        }))
    }

    /// `placementOfOpener(opener, homeId)`: the placement of the window or
    /// home ask that opened a create flow
    pub fn placement_of_opener(
        &mut self,
        opener: Option<&NewDocumentOpener>,
        home_id: Option<i64>,
    ) -> Result<Option<NewDocumentPlacement>, NodeError> {
        match opener {
            None => Ok(None),
            Some(NewDocumentOpener::Window { web_contents_id }) => {
                self.placement_for(*web_contents_id)
            }
            Some(NewDocumentOpener::Home) => match home_id {
                None => Ok(None),
                Some(home_id) => self.placement_for(home_id),
            },
        }
    }

    /// `preset(webContentsId, placement)`: a create flow opened
    /// `webContentsId` for its opener: the tab starts in the opener's project
    /// and saves into the opener's folder (spec 5.2, 6). A placement that
    /// resolves to `None` leaves the tab as it is today; one that failed is
    /// `None` too (`placement.catch(() => null)`).
    pub fn preset(
        &mut self,
        web_contents_id: i64,
        placement: Result<Option<NewDocumentPlacement>, NodeError>,
    ) {
        self.preset_owned(web_contents_id, placement);
    }

    pub fn prepare_preset(
        &mut self, web_contents_id: i64, placement: Result<Option<NewDocumentPlacement>, NodeError>,
    ) -> Result<PreparedNewDocumentPreset, String> {
        let slots = self.prepared_slots.get().checked_add(1).ok_or("Too many prepared project presets")?;
        self.presets.try_reserve(slots).map_err(|error| error.to_string())?;
        let expected = self.presets.get(&web_contents_id).map(|entry| entry.identity.clone());
        self.prepared_slots.set(slots);
        Ok(PreparedNewDocumentPreset {
            id: web_contents_id,
            entry: Some(PresetEntry {
                identity: std::rc::Rc::new(std::cell::Cell::new(false)),
                placement: placement.ok().flatten(),
            }),
            expected, slots: self.prepared_slots.clone(),
        })
    }

    pub fn validate_preset(&mut self, prepared: PreparedNewDocumentPreset) -> Result<ValidatedNewDocumentPreset<'_, G>, String> {
        if !std::rc::Rc::ptr_eq(&self.prepared_slots, &prepared.slots) {
            return Err("The project preset belongs to another placer".into());
        }
        let current = self.presets.get(&prepared.id).map(|entry| &entry.identity);
        let same = match (current, &prepared.expected) {
            (None, None) => true,
            (Some(current), Some(expected)) => std::rc::Rc::ptr_eq(current, expected),
            _ => false,
        };
        if !same { return Err("The project preset changed before publication".into()); }
        Ok(ValidatedNewDocumentPreset { placer: self, prepared })
    }

    pub fn preset_owned(
        &mut self,
        web_contents_id: i64,
        placement: Result<Option<NewDocumentPlacement>, NodeError>,
    ) -> NewDocumentPresetReceipt {
        self.presets.reserve(self.prepared_slots.get() + 1);
        let entry = PresetEntry { identity: std::rc::Rc::new(std::cell::Cell::new(false)), placement: placement.ok().flatten() };
        let identity = entry.identity.clone();
        let previous = self.presets.insert(web_contents_id, entry);
        NewDocumentPresetReceipt { id: web_contents_id, identity, previous, restored: false }
    }

    pub fn rollback_preset(&mut self, receipt: &mut NewDocumentPresetReceipt) -> Result<(), String> {
        if receipt.restored { return Ok(()); }
        if receipt.identity.get() {
            receipt.previous = None;
            receipt.restored = true;
            return Ok(());
        }
        if self.presets.get(&receipt.id).is_none_or(|entry| !std::rc::Rc::ptr_eq(&entry.identity, &receipt.identity)) {
            return Err("The new-document preset was replaced".into());
        }
        self.presets.remove(&receipt.id);
        if let Some(previous) = receipt.previous.take() { self.presets.insert(receipt.id, previous); }
        receipt.restored = true;
        Ok(())
    }

    pub fn retire_preset(&mut self, receipt: &mut NewDocumentPresetReceipt) -> Result<(), String> {
        if receipt.restored { return Ok(()); }
        if self.presets.get(&receipt.id).is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity,&receipt.identity)) {
            self.presets.remove(&receipt.id);
        }
        receipt.identity.set(true);
        receipt.previous=None;
        receipt.restored=true;
        Ok(())
    }

    /// `presetProjectOf(webContentsId)`: the project a create flow opened
    /// this tab in, or `None` (for the project request layer)
    pub fn preset_project_of(&mut self, web_contents_id: i64) -> Option<ProjectId> {
        let preset = self.preset_of(web_contents_id)?;
        let root = self.deps.guide.project_root(&preset.project_id);
        truthy(root.as_deref()).map(|_| preset.project_id)
    }

    /// `forget(webContentsId)`: the tab closed
    pub fn forget(&mut self, web_contents_id: i64) {
        if let Some(entry) = self.presets.remove(&web_contents_id) { entry.identity.set(true); }
    }

    /// `saveDialogFolder(webContentsId)`: the folder an untitled document's
    /// save dialog opens in (spec 6): the placement's folder, or `None` for
    /// the editor's default save folder.
    pub fn save_dialog_folder(
        &mut self,
        web_contents_id: i64,
    ) -> Result<Option<String>, NodeError> {
        Ok(self
            .placement_for(web_contents_id)?
            .map(|placement| placement.dir))
    }

    pub fn prepare_write_for(
        &mut self,
        web_contents_id: i64,
        file_name: &str,
    ) -> Result<Option<NewDocumentWrite>, WriteGuardError> {
        self.placement_for(web_contents_id)?
            .map(|placement| NewDocumentWrite::new(placement, file_name)).transpose()
    }

    pub fn capture_placement(&mut self, web_contents_id: i64) -> NewDocumentPlacementSeed {
        NewDocumentPlacementSeed {
            preset: self.preset_of(web_contents_id),
            document: (self.deps.document_of)(web_contents_id),
            platform: self.deps.platform.clone(),
        }
    }

    /// `writeFor(webContentsId, fileName, bytes)`: write a new document of
    /// `webContentsId`'s run into its project folder; `None` (nothing
    /// written) when its panel has no project. Fails when the project folder
    /// refuses the write: nothing goes elsewhere instead.
    pub fn write_for(
        &mut self,
        web_contents_id: i64,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, WriteGuardError> {
        let Some(write) = self.prepare_write_for(web_contents_id, file_name)? else { return Ok(None); };
        write.execute(bytes, |id, target, bytes| self.deps.guide.write_new_document(id, target, bytes)).map(Some)
    }

    /// `write(placement, fileName, bytes)`: write `bytes` as `fileName` (or
    /// the first free numbered name) in the placement's folder through the
    /// write guard; the path written.
    pub fn write(
        &mut self,
        placement: &NewDocumentPlacement,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, WriteGuardError> {
        let write = NewDocumentWrite::new(placement.clone(), file_name)?;
        write.execute(bytes, |id, target, bytes| self.deps.guide.write_new_document(id, target, bytes))
    }

    /// `presetOf(webContentsId)`
    fn preset_of(&self, web_contents_id: i64) -> Option<NewDocumentPlacement> {
        self.presets.get(&web_contents_id).and_then(|entry| entry.placement.clone())
    }
}

#[cfg(test)]
mod owned_write_tests {
    use super::*;
    use crate::shell::projects::write_guard::ProjectWriteGuard;

    struct Guide;
    impl NewDocumentsGuide for Guide {
        fn project_root(&mut self, _: &str) -> Option<String> { None }
        fn write_new_document(&mut self, _: &str, _: &str, _: &[u8]) -> Result<ProjectWriteResult, WriteGuardError> {
            Ok(ProjectWriteResult::Ok)
        }
    }

    #[test]
    fn preset_rollback_restores_displaced_entry_without_touching_replacements() {
        let mut placer = NewDocumentPlacer::new(NewDocumentPlacerDeps {
            guide: Guide, window_project: Box::new(|_| Ok(None)),
            document_of: Box::new(|_| None), platform: "linux".into(),
        });
        let placement = |dir: &str| Some(NewDocumentPlacement {
            project_id: "project".into(), root: "/project".into(), dir: dir.into(),
        });
        placer.preset(1, Ok(placement("/project/first")));
        let mut receipt = placer.preset_owned(1, Ok(placement("/project/second")));
        placer.rollback_preset(&mut receipt).unwrap();
        assert_eq!(placer.preset_of(1), placement("/project/first"));
        let mut replaced = placer.preset_owned(1, Ok(placement("/project/second")));
        placer.preset(1, Ok(placement("/project/third")));
        assert!(placer.rollback_preset(&mut replaced).is_err());
        assert_eq!(placer.preset_of(1), placement("/project/third"));
        let mut retired = placer.preset_owned(1, Ok(placement("/project/fourth")));
        placer.forget(1);
        placer.preset(1, Ok(placement("/project/fifth")));
        placer.rollback_preset(&mut retired).unwrap();
        assert_eq!(placer.preset_of(1), placement("/project/fifth"));
        let mut adopted=placer.preset_owned(1,Ok(placement("/project/sixth")));
        placer.preset(1,Ok(placement("/project/successor")));
        placer.retire_preset(&mut adopted).unwrap();
        assert_eq!(placer.preset_of(1),placement("/project/successor"));
    }

    #[test]
    fn prepared_preset_rejects_replacement_and_preserves_other_pending_commits() {
        let mut placer = NewDocumentPlacer::new(NewDocumentPlacerDeps {
            guide: Guide, window_project: Box::new(|_| Ok(None)),
            document_of: Box::new(|_| None), platform: "linux".into(),
        });
        let placement = |dir: &str| Some(NewDocumentPlacement {
            project_id: "project".into(), root: "/project".into(), dir: dir.into(),
        });
        let stale = placer.prepare_preset(1, Ok(placement("/project/stale"))).unwrap();
        let first = placer.prepare_preset(2, Ok(placement("/project/first"))).unwrap();
        let second = placer.prepare_preset(3, Ok(placement("/project/second"))).unwrap();
        placer.preset(1, Ok(placement("/project/current")));
        assert!(placer.validate_preset(stale).is_err());
        placer.validate_preset(second).unwrap().commit();
        placer.validate_preset(first).unwrap().commit();
        assert_eq!(placer.preset_of(1), placement("/project/current"));
        assert_eq!(placer.preset_of(2), placement("/project/first"));
        assert_eq!(placer.preset_of(3), placement("/project/second"));
    }

    #[test]
    fn concurrent_creation_uses_next_number_without_overwriting() {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let folder = std::env::temp_dir().join(format!("seuoffice-placement-{suffix}"));
        std::fs::create_dir(&folder).unwrap();
        let root = std::fs::canonicalize(&folder).unwrap().to_string_lossy().into_owned();
        let write = NewDocumentWrite::new(NewDocumentPlacement {
            project_id: "project".into(), root: root.clone(), dir: root.clone(),
        }, "draft.md").unwrap();
        let guard = ProjectWriteGuard::new(crate::packages::ai_provider::agent_session_policy::process_platform());
        let raced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let path = write.execute(b"new document", |_, target, bytes| {
            let collision = target.to_owned();
            let raced = raced.clone();
            guard.write_new_document_authorized(&root, target, bytes, std::sync::Arc::new(move |effect| {
                if !raced.swap(true,std::sync::atomic::Ordering::AcqRel) {
                    std::fs::write(&collision, b"other document").unwrap();
                }
                Ok(effect())
            }))?;
            Ok(ProjectWriteResult::Ok)
        }).unwrap();
        assert_eq!(std::path::Path::new(&path).file_name().unwrap(), "draft (2).md");
        assert_eq!(std::fs::read(&path).unwrap(), b"new document");
        assert_eq!(std::fs::read(std::path::Path::new(&root).join("draft.md")).unwrap(), b"other document");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
