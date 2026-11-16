//! Rust-only startup/CLI authority. These types are never IPC payloads.
use std::{path::PathBuf, sync::{Arc, Mutex}};
use crate::{Surface, ViewId, runtime::{CancelSubscription, CancelToken, RendererGeneration}};
use super::{AdmittedPdfRevisionRetirement, NativeBrowserOwner, NativeClosureReservation, NativeRequestOwner, PdfDocumentLease, PreparedPdfRevisionOwner};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRecoveryDecision { Restore, Original }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeRecoveryCopy {
    File { path: PathBuf },
    Untitled { id: String, ext: String },
}

/// An ordinary save may require a native dialog; explicit path saves retain the
/// exact trusted path instead of inventing a destination before selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeSaveSelection {
    Mode(crate::editors::SaveMode),
    ExactPath(PathBuf),
}

/// The editor's actual format selection, restricted to its own surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSaveFormat {
    Html, Markdown, DiagramDrawio, DiagramDio, DiagramSvg, DiagramPng, Mindmap, Epub, Slides, PdfUntitled,
    DocsDocx { offer_exports: bool }, DocsOdt { offer_exports: bool },
}

/// Inputs selected by a native menu, OS or authenticated application service.
/// Never deserialize this authority from a renderer invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeEditorCommand {
    OpenFile { surface: Surface, path: PathBuf },
    SaveDocument { surface: Surface, view: ViewId, selection: NativeSaveSelection },
    NewDocument { surface: Surface, directory: Option<PathBuf> },
    HwpCopy { path: PathBuf, title: String },
    OpenHome,
    OpenHelp { topic: Option<String> },
}

/// The actual already-selected native input. The issuer, not a renderer, supplies
/// these values from recovery selection or the parsed/validated CLI request.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeInvocationSelection {
    StartupLaunch { paths: Vec<PathBuf>, reopened_by_installer: bool },
    NativeCommand(NativeEditorCommand),
    RecoveryFile { surface: Surface, path: PathBuf, decision: NativeRecoveryDecision },
    RecoveryBlank { surface: Surface, title: String },
    RecoveryCopy { surface: Surface, copy: NativeRecoveryCopy, title: String, save_as_default: Option<PathBuf> },
    HeadlessExport { surface: Surface, input: PathBuf, output: PathBuf, format: String,
        page: Option<f64>, ops: Option<String>, batch: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeInvocationPurpose { StartupLaunch, NativeCommand, Recovery, HeadlessExport }

impl NativeInvocationSelection {
    pub fn purpose(&self) -> NativeInvocationPurpose {
        match self {
            Self::StartupLaunch { .. } => NativeInvocationPurpose::StartupLaunch,
            Self::NativeCommand(_) => NativeInvocationPurpose::NativeCommand,
            Self::RecoveryFile { .. } | Self::RecoveryCopy { .. } | Self::RecoveryBlank { .. } => NativeInvocationPurpose::Recovery,
            Self::HeadlessExport { .. } => NativeInvocationPurpose::HeadlessExport,
        }
    }
    pub fn surface(&self) -> Surface {
        match self {
            Self::StartupLaunch { .. } => Surface::Shell,
            Self::NativeCommand(command) => match command {
                NativeEditorCommand::OpenFile { surface, .. } | NativeEditorCommand::NewDocument { surface, .. }
                | NativeEditorCommand::SaveDocument { surface, .. } => *surface,
                NativeEditorCommand::OpenHome | NativeEditorCommand::OpenHelp { .. } => Surface::Shell,
                NativeEditorCommand::HwpCopy { .. } => Surface::Hwp,
            },
            Self::RecoveryFile { surface, .. } | Self::RecoveryCopy { surface, .. }
            | Self::RecoveryBlank { surface, .. } | Self::HeadlessExport { surface, .. } => *surface,
        }
    }
}

/// Kept by trusted main/startup assembly only; never install it as a renderer
/// service or construct it from an IPC payload. There is no parallel registry.
pub struct NativeInvocationIssuer { shutdown: CancelToken }

impl NativeInvocationIssuer {
    pub fn new(shutdown: CancelToken) -> Self { Self { shutdown } }
    pub fn issue(&self, selection: NativeInvocationSelection, cancel: CancelToken) -> NativeInvocationOperation {
        NativeInvocationOperation(Arc::new(Invocation {
            selection, cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: false, parent: None,
        }))
    }
    /// Main calls this only from the first native Registered Shell callback.
    /// The retained owner is immutable; a later view cannot supply a replacement.
    pub fn issue_startup(&self, paths: Vec<PathBuf>, reopened_by_installer: bool,
        owner: NativeRequestOwner, cancel: CancelToken) -> NativeInvocationOperation {
        NativeInvocationOperation(Arc::new(Invocation {
            selection: NativeInvocationSelection::StartupLaunch { paths, reopened_by_installer },
            cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: Some(owner), native_command: false, parent: None,
        }))
    }
    pub fn issue_command(&self, command: NativeEditorCommand, cancel: CancelToken) -> NativeInvocationOperation {
        NativeInvocationOperation(Arc::new(Invocation {
            selection: NativeInvocationSelection::NativeCommand(command),
            cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: true, parent: None,
        }))
    }

    /// Bind a native save selection to the actual existing editor registration.
    /// The trusted editor capture must already have attached its document lease.
    pub fn issue_existing_document_save(&self, surface: Surface, view: ViewId, selection: NativeSaveSelection,
        owner: NativeRequestOwner, cancel: CancelToken) -> Result<NativeExistingDocumentBinding, String> {
        if owner.view != view || owner.context.renderer().is_none() || owner.context.document_lease().is_none()
            || owner.loss_token().is_none() {
            return Err("The captured existing document is unavailable.".into());
        }
        let operation = self.issue_command(NativeEditorCommand::SaveDocument { surface, view, selection }, cancel);
        Ok(NativeExistingDocumentBinding { operation, owner })
    }
    /// Trusted startup assembly issues remembered-path commands only after its
    /// worker has selected the concrete command. Consumers admit root and child
    /// together; the child alone does not inherit the root's live authority.
    pub fn issue_command_child(&self, startup: &NativeInvocationOperation,
        command: NativeEditorCommand, cancel: CancelToken) -> Result<NativeInvocationOperation, String> {
        if startup.purpose() != NativeInvocationPurpose::StartupLaunch || startup.startup_owner().is_none()
            || !startup.shutdown().same_token(&self.shutdown) {
            return Err("The native command does not belong to this selected startup.".into());
        }
        Ok(NativeInvocationOperation(Arc::new(Invocation {
            selection: NativeInvocationSelection::NativeCommand(command),
            cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: true, parent: Some(startup.clone()),
        })))
    }
    /// A trusted native open may activate an unloaded recovery selection. The
    /// entry supplies its original immutable selection; no renderer is invented.
    pub fn issue_command_recovery_child(&self, command: &NativeInvocationOperation,
        selection: NativeInvocationSelection, cancel: CancelToken) -> Result<NativeInvocationOperation, String> {
        let matches_surface = matches!(command.selection(),
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::OpenFile { surface, .. })
                if *surface == selection.surface());
        if !command.is_root_native_command() || !matches_surface || !command.shutdown().same_token(&self.shutdown)
            || selection.purpose() != NativeInvocationPurpose::Recovery {
            return Err("The recovery child does not belong to this native command.".into());
        }
        Ok(NativeInvocationOperation(Arc::new(Invocation {
            selection, cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: false, parent: Some(command.clone()),
        })))
    }
    /// A remembered startup command may hydrate its exact cached recovery entry.
    /// Consumers must admit all three retained operations together.
    pub fn issue_startup_command_recovery_child(&self, startup: &NativeInvocationOperation,
        command: &NativeInvocationOperation, selection: NativeInvocationSelection,
        cancel: CancelToken) -> Result<NativeInvocationOperation, String> {
        let matches_surface = matches!(command.selection(),
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::OpenFile { surface, .. })
                if *surface == selection.surface());
        if startup.purpose() != NativeInvocationPurpose::StartupLaunch || startup.startup_owner().is_none()
            || !startup.shutdown().same_token(&self.shutdown) || !command.shutdown().same_token(&self.shutdown)
            || !command.is_native_command() || !command.is_child_of(startup) || !matches_surface
            || selection.purpose() != NativeInvocationPurpose::Recovery {
            return Err("The recovery child does not belong to this startup command.".into());
        }
        Ok(NativeInvocationOperation(Arc::new(Invocation {
            selection, cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: false, parent: Some(command.clone()),
        })))
    }

    /// Seal the selection's startup lineage. A later renderer activation of an
    /// unloaded plan has its own child lifetime; only startup hydration admits
    /// both live operations together.
    pub fn issue_recovery_child(&self, startup: &NativeInvocationOperation,
        selection: NativeInvocationSelection, cancel: CancelToken) -> Result<NativeInvocationOperation, String> {
        if startup.purpose() != NativeInvocationPurpose::StartupLaunch || startup.startup_owner().is_none()
            || !startup.shutdown().same_token(&self.shutdown) || selection.purpose() != NativeInvocationPurpose::Recovery {
            return Err("The recovery child does not belong to this selected startup.".into());
        }
        Ok(NativeInvocationOperation(Arc::new(Invocation {
            selection, cancel, shutdown: self.shutdown.clone(), lifetime: RendererGeneration::new(),
            target: Mutex::new(None), startup_owner: None, native_command: false, parent: Some(startup.clone()),
        })))
    }
}

struct Invocation {
    selection: NativeInvocationSelection,
    cancel: CancelToken,
    shutdown: CancelToken,
    lifetime: RendererGeneration,
    target: Mutex<Option<TargetState>>,
    startup_owner: Option<NativeRequestOwner>,
    native_command: bool,
    parent: Option<NativeInvocationOperation>,
}

struct TargetState {
    view: ViewId,
    created: RendererGeneration,
    registered: Option<NativeRequestOwner>,
}

#[derive(Clone)]
pub struct NativeInvocationOperation(Arc<Invocation>);

/// Immutable authority for a native command on one already-existing document.
/// This never reserves or replaces a native browser target.
#[derive(Clone)]
pub struct NativeExistingDocumentBinding {
    operation: NativeInvocationOperation,
    owner: NativeRequestOwner,
}

impl NativeExistingDocumentBinding {
    pub fn operation(&self) -> &NativeInvocationOperation { &self.operation }
    pub fn owner(&self) -> &NativeRequestOwner { &self.owner }
}

/// A path selected for one specific existing-document save, not a generic path grant.
#[derive(Clone)]
pub struct NativeSaveDestination {
    binding: NativeExistingDocumentBinding,
    path: PathBuf,
    raw_path: Option<PathBuf>,
    normalized: bool,
    dialog_selected: bool,
}

impl NativeSaveDestination {
    /// Called only by the trusted native dialog callback inside its binding admission.
    pub fn from_native_save_dialog(binding: NativeExistingDocumentBinding, path: PathBuf) -> Result<Self, String> {
        if !matches!(binding.operation.selection(), NativeInvocationSelection::NativeCommand(
            NativeEditorCommand::SaveDocument { selection: NativeSaveSelection::Mode(_), .. })) || path.as_os_str().is_empty() {
            return Err("The native save destination does not match its selection.".into());
        }
        Ok(Self { binding, path, raw_path: None, normalized: false, dialog_selected: true })
    }
    /// Construct only from the exact path already sealed in the native selection.
    pub fn from_exact_selection(binding: NativeExistingDocumentBinding) -> Result<Self, String> {
        let path = match binding.operation.selection() {
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                selection: NativeSaveSelection::ExactPath(path), ..
            }) if !path.as_os_str().is_empty() => path.clone(),
            _ => return Err("The native save did not select an exact path.".into()),
        };
        Ok(Self { binding, path, raw_path: None, normalized: false, dialog_selected: false })
    }
    /// Called only by the owning editor while its exact captured document and
    /// current path are jointly admitted; never pass a renderer-provided path.
    pub fn from_current_document(binding: NativeExistingDocumentBinding, path: PathBuf) -> Result<Self, String> {
        if !matches!(binding.operation.selection(), NativeInvocationSelection::NativeCommand(
            NativeEditorCommand::SaveDocument { selection: NativeSaveSelection::Mode(crate::editors::SaveMode::Save), .. }))
            || path.as_os_str().is_empty() {
            return Err("The native save does not select the current document.".into());
        }
        Ok(Self { binding, path, raw_path: None, normalized: false, dialog_selected: false })
    }
    pub fn binding(&self) -> &NativeExistingDocumentBinding { &self.binding }
    pub fn path(&self) -> &std::path::Path { &self.path }
    pub fn raw_path(&self) -> &std::path::Path { self.raw_path.as_deref().unwrap_or(&self.path) }

    /// One deterministic, same-directory normalization of a native dialog
    /// result. Exact-path selections cannot be widened or rewritten.
    pub fn normalize_dialog_format(mut self, format: NativeSaveFormat) -> Result<Self, String> {
        if !self.dialog_selected || self.normalized { return Err("The save destination cannot be normalized again.".into()); }
        let (surface, mode) = match self.binding.operation.selection() {
            NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                surface, selection: NativeSaveSelection::Mode(mode), ..
            }) => (*surface, *mode),
            _ => return Err("The save format does not match its native selection.".into()),
        };
        let compatible = match format {
            NativeSaveFormat::Html => surface == Surface::Html && mode == crate::editors::SaveMode::SaveAs,
            NativeSaveFormat::Markdown => surface == Surface::Markdown,
            NativeSaveFormat::DiagramDrawio | NativeSaveFormat::DiagramDio | NativeSaveFormat::DiagramSvg
                | NativeSaveFormat::DiagramPng => surface == Surface::Diagram,
            NativeSaveFormat::Mindmap => surface == Surface::Mindmap,
            NativeSaveFormat::Epub => surface == Surface::Epub,
            NativeSaveFormat::Slides => surface == Surface::Slides,
            NativeSaveFormat::PdfUntitled => surface == Surface::Pdf && mode == crate::editors::SaveMode::Save,
            NativeSaveFormat::DocsDocx { .. } | NativeSaveFormat::DocsOdt { .. } => surface == Surface::Docs,
        };
        if !compatible { return Err("The save format belongs to a different editor.".into()); }
        let name = self.path.file_name().ok_or("The selected save name is unavailable.")?.as_encoded_bytes();
        let ends = |suffix: &[u8]| name.len() >= suffix.len()
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix);
        let mut ending = match format {
            NativeSaveFormat::Html if self.path.extension().is_none() => Some(".html"),
            NativeSaveFormat::Markdown if self.path.extension().is_none() => Some(".md"),
            NativeSaveFormat::Html | NativeSaveFormat::Markdown => None,
            NativeSaveFormat::Epub if !self.path.extension().is_some_and(|extension|
                extension.as_encoded_bytes().eq_ignore_ascii_case(b"epub")) => Some(".epub"),
            NativeSaveFormat::Epub => None,
            NativeSaveFormat::Mindmap if ends(b".xmind") || (mode == crate::editors::SaveMode::SaveAs
                && [b".png".as_slice(), b".svg", b".pdf", b".md"].iter().any(|suffix| ends(suffix))) => None,
            NativeSaveFormat::Mindmap => Some(".xmind"),
            NativeSaveFormat::Slides if [b".pptx".as_slice(), b".png", b".pdf"].iter().any(|suffix| ends(suffix)) => None,
            NativeSaveFormat::Slides => Some(".pptx"),
            NativeSaveFormat::PdfUntitled if ends(b".pdf") => None,
            NativeSaveFormat::PdfUntitled => Some(".pdf"),
            NativeSaveFormat::DocsDocx { offer_exports } | NativeSaveFormat::DocsOdt { offer_exports } => {
                let native = ends(b".docx") || ends(b".odt");
                let export = offer_exports && [b".pdf".as_slice(), b".hwp", b".hwpx", b".html",
                    b".htm", b".md", b".markdown", b".png"].iter().any(|suffix| ends(suffix));
                if native || export { None } else {
                    Some(if matches!(format, NativeSaveFormat::DocsDocx { .. }) { ".docx" } else { ".odt" })
                }
            }
            NativeSaveFormat::DiagramDrawio | NativeSaveFormat::DiagramDio | NativeSaveFormat::DiagramSvg | NativeSaveFormat::DiagramPng => {
                if [b".drawio.svg".as_slice(), b".drawio.png", b".drawio", b".dio"].iter().any(|suffix| ends(suffix)) {
                    None
                } else {
                    Some(match format {
                        NativeSaveFormat::DiagramDrawio => ".drawio", NativeSaveFormat::DiagramDio => ".dio",
                        NativeSaveFormat::DiagramSvg => ".drawio.svg", NativeSaveFormat::DiagramPng => ".drawio.png",
                        _ => unreachable!(),
                    })
                }
            }
        };
        let swap = match (format, ending) {
            (NativeSaveFormat::DiagramSvg, Some(_)) if ends(b".svg") => Some("drawio.svg"),
            (NativeSaveFormat::DiagramPng, Some(_)) if ends(b".png") => Some("drawio.png"),
            _ => None,
        };
        self.raw_path = Some(self.path.clone());
        self.normalized = true;
        if let Some(extension) = swap {
            if name.eq_ignore_ascii_case(if extension.ends_with("svg") { b".svg" } else { b".png" }) {
                self.path.set_file_name(format!(".{extension}"));
            } else {
                self.path.set_extension(extension);
            }
            ending = None;
        }
        if let Some(ending) = ending { self.path.as_mut_os_string().push(ending); }
        Ok(self)
    }
    pub fn matches_binding(&self, binding: &NativeExistingDocumentBinding) -> bool {
        Arc::ptr_eq(&self.binding.operation.0, &binding.operation.0)
            && same_captured_owner(&self.binding.owner, &binding.owner)
    }
    /// Derive only the indexed page name for a selected Slides or Docs PNG.
    /// `count` is the number of images actually produced; `index` is zero-based.
    pub fn numbered_png_page(&self, count: usize, index: usize) -> Result<NativeSaveNumberedDestination, String> {
        if !matches!(self.binding.operation.selection().surface(), Surface::Slides | Surface::Docs) || count == 0 || index >= count
            || count > u32::MAX as usize {
            return Err("The numbered PNG output is unavailable.".into());
        }
        let name = self.path.file_name().ok_or("The selected save name is unavailable.")?;
        let bytes = name.as_encoded_bytes();
        if bytes.len() < 4 || !bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".png") {
            return Err("The selected output is not PNG.".into());
        }
        let mut path = self.path.clone();
        if count > 1 {
            let width = count.to_string().len().max(2);
            let mut numbered = path.file_stem().ok_or("The selected save name is unavailable.")?.to_os_string();
            numbered.push(format!("-{:0width$}", index + 1));
            if let Some(extension) = path.extension() {
                numbered.push(".");
                numbered.push(extension);
            }
            path.set_file_name(numbered);
        }
        Ok(NativeSaveNumberedDestination { selection: self.clone(), path, count, index })
    }
}

#[derive(Clone)]
pub struct NativeSaveNumberedDestination {
    selection: NativeSaveDestination,
    path: PathBuf,
    count: usize,
    index: usize,
}
impl NativeSaveNumberedDestination {
    pub fn selection(&self) -> &NativeSaveDestination { &self.selection }
    pub fn path(&self) -> &std::path::Path { &self.path }
    pub fn count(&self) -> usize { self.count }
    pub fn index(&self) -> usize { self.index }
}

/// One exact constructor reservation. It is not a NativeRequestOwner and cannot
/// impersonate an authenticated renderer before first native registration.
#[derive(Clone)]
pub struct NativeInvocationTarget {
    operation: NativeInvocationOperation,
    view: ViewId,
    created: RendererGeneration,
}
/// Cleanup authority captured for the original headless browser reservation.
/// It cannot admit ordinary work after the invocation has ended.
#[derive(Clone)]
pub struct NativeHeadlessCleanup {
    target: NativeInvocationTarget,
    reservation: NativeClosureReservation,
    owner: NativeBrowserOwner,
    cleanup_shutdown: CancelToken,
}

impl NativeHeadlessCleanup {
    /// Capture before dispatching the constructor, while its invocation is live.
    pub fn capture(
        target: &NativeInvocationTarget, reservation: &NativeClosureReservation,
        owner: &NativeBrowserOwner, cleanup_shutdown: CancelToken,
    ) -> Result<Self, String> {
        if target.operation().purpose() != NativeInvocationPurpose::HeadlessExport
            || !reservation.matches(target.view(), target.created())
            || !owner.reservation().same_reservation(reservation) {
            return Err("The headless cleanup does not match its original reservation.".into());
        }
        target.with_active(|| Self {
            target: target.clone(), reservation: reservation.clone(), owner: owner.clone(), cleanup_shutdown,
        }).ok_or_else(|| "The headless invocation ended before cleanup was captured.".into())
    }

    /// Admit only the original browser close and its synchronous actor retirement.
    pub fn claim_close<T>(&self, retire: impl FnOnce(&super::NativeBrowserTransition<'_>) -> Result<T, String>)
        -> Result<super::NativeBrowserCloseClaim<T>, String> {
        CancelToken::with_active_all([Some(&self.cleanup_shutdown)], || self.reservation.claim_close(&self.owner, retire))
            .ok_or_else(|| "The process cleanup lifetime ended.".to_owned())?
    }
    pub fn target(&self) -> &NativeInvocationTarget { &self.target }
}


#[derive(Clone)]
pub enum RecoveryOpenContext {
    Renderer { source: NativeRequestOwner, participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken },
    NativeStartup(NativeInvocationOperation),
}

impl NativeInvocationOperation {
    pub fn selection(&self) -> &NativeInvocationSelection { &self.0.selection }
    pub fn purpose(&self) -> NativeInvocationPurpose { self.selection().purpose() }
    pub fn cancel(&self) -> &CancelToken { &self.0.cancel }
    pub fn shutdown(&self) -> &CancelToken { &self.0.shutdown }
    pub fn loss_token(&self) -> CancelToken { self.0.lifetime.loss_token() }
    pub fn authority_generation(&self) -> &RendererGeneration { &self.0.lifetime }
    /// Native adapters additionally compare their private attested origin with
    /// this exact retained owner. A generic issued selection has no root owner.
    pub fn startup_owner(&self) -> Option<&NativeRequestOwner> { self.0.startup_owner.as_ref() }
    pub fn is_native_command(&self) -> bool { self.0.native_command }
    pub fn is_root_native_command(&self) -> bool { self.0.native_command && self.0.parent.is_none() }
    pub fn is_child_of(&self, parent: &NativeInvocationOperation) -> bool {
        self.0.parent.as_ref().is_some_and(|original| Arc::ptr_eq(&original.0, &parent.0))
    }

    /// Pre-construction admission. Consume an immutable constructor action here;
    /// execute it only after this method returns and every gate has dropped.
    pub fn with_active<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        self.0.lifetime.with_active(|| CancelToken::with_active_all([
            Some(&self.0.cancel), Some(&self.0.shutdown),
        ], effect)).flatten()
    }

    /// Return only the constructor reservation already retained by this exact
    /// operation. This never creates authority from a view/current-document lookup.
    pub fn reserved_target(&self, view: ViewId) -> Result<NativeInvocationTarget, String> {
        self.with_active(|| {
            let state = self.0.target.lock().map_err(|_| "The native invocation state is unavailable.")?;
            let target = state.as_ref().filter(|target| target.view == view)
                .ok_or("This native invocation did not reserve the requested target.")?;
            Ok(NativeInvocationTarget { operation: self.clone(), view, created: target.created.clone() })
        }).ok_or_else(|| "The native invocation is no longer active.".to_owned())?
    }

    /// Called after the real creator reserves view+generation, before loading.
    pub fn reserve_target(&self, view: ViewId, created: RendererGeneration) -> Result<NativeInvocationTarget, String> {
        if matches!(self.selection(), NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument { .. })) {
            return Err("An existing document save cannot reserve a browser target.".into());
        }
        let reserved = self.0.lifetime.with_active(|| CancelToken::with_active_all([
            Some(&self.0.cancel), Some(&self.0.shutdown),
        ], || {
            if !created.is_active() { return Err("The native target reservation was replaced.".into()); }
            let mut state = self.0.target.lock().map_err(|_| "The native invocation state is unavailable.")?;
            if state.is_some() { return Err("The native invocation already reserved its target.".into()); }
            *state = Some(TargetState { view, created: created.clone(), registered: None });
            Ok(NativeInvocationTarget { operation: self.clone(), view, created })
        })).flatten();
        reserved.ok_or_else(|| "The native invocation is no longer active.".to_owned())?
    }

    /// End only this invocation's authority, not a caller-shared cancel token.
    /// Invoke outside admission: already admitted short work may finish.
    pub fn finish(&self) {
        self.0.lifetime.revoke();
        if let Ok(mut target) = self.0.target.lock() { *target = None; }
    }
}

impl NativeInvocationTarget {
    pub fn view(&self) -> ViewId { self.view }
    pub fn created(&self) -> &RendererGeneration { &self.created }
    pub fn operation(&self) -> &NativeInvocationOperation { &self.operation }

    /// State check only; the native adapter already holds the canonical
    /// operation, constructor and cancellation gates. Never reacquire them here.
    pub fn with_reserved_state<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        let matches = self.operation.0.target.lock().ok().is_some_and(|state| state.as_ref()
            .is_some_and(|target| target.view == self.view && target.created.same_generation(&self.created)));
        if matches { Some(effect()) } else { None }
    }

    /// Called only within the native adapter's canonical combined admission,
    /// with the retained first-Registered snapshot. No generation gate is
    /// reacquired, and the state mutex is released before the short effect.
    pub fn with_registered_state<T>(&self, registered: &NativeRequestOwner, effect: impl FnOnce() -> T) -> Option<T> {
        if registered.view != self.view
            || !registered.context.renderer().is_some_and(|created| created.same_generation(&self.created)) {
            return None;
        }
        let matches = self.operation.0.target.lock().ok().is_some_and(|state| state.as_ref()
            .filter(|target| target.view == self.view && target.created.same_generation(&self.created))
            .and_then(|target| target.registered.as_ref())
            .is_some_and(|owner| owner.view == registered.view
                && Arc::ptr_eq(&owner.context.as_arc(), &registered.context.as_arc())
                && owner.loss_token().zip(registered.loss_token())
                    .is_some_and(|(expected, actual)| expected.same_token(&actual))));
        if matches { Some(effect()) } else { None }
    }

    /// A short prepared seed/state admission, not a UI constructor callback.
    /// To construct on UI, consume the immutable action here and execute it only
    /// after this method returns and all gates have been dropped.
    pub fn with_active<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        self.with_active_loss(None, effect)
    }

    fn with_active_loss<T>(&self, owner_loss: Option<&CancelToken>, effect: impl FnOnce() -> T) -> Option<T> {
        RendererGeneration::with_active_all([
            Some(&self.operation.0.lifetime), Some(&self.created),
        ], || CancelToken::with_active_all([
            Some(&self.operation.0.cancel), Some(&self.operation.0.shutdown), owner_loss,
        ], || {
            let valid = self.operation.0.target.lock().ok().is_some_and(|state| state.as_ref()
                .is_some_and(|target| target.view == self.view && target.created.same_generation(&self.created)));
            if valid { Some(effect()) } else { None }
        }).flatten()).flatten()
    }

    /// State mutation only. The shell's attested adapter must already hold the
    /// canonical operation, created, owner-loss, native and document gates.
    pub fn register_from_native_admitted(&self, owner: NativeRequestOwner) -> Result<(), String> {
        if owner.view != self.view || !owner.context.renderer().is_some_and(|renderer| renderer.same_generation(&self.created)) {
            return Err("The registered native target does not match its reservation.".into());
        }
        if owner.loss_token().is_none() {
            return Err("The registered target has no native loss signal.".into());
        }
        let mut state = self.operation.0.target.lock().map_err(|_| "The native invocation state is unavailable.")?;
        let target = state.as_mut().filter(|target| target.view == self.view
            && target.created.same_generation(&self.created))
            .ok_or("The native target reservation was released.")?;
        if target.registered.is_some() { return Err("The native target was already registered.".into()); }
        target.registered = Some(owner);
        Ok(())
    }

    /// A non-consuming, immutable first-Registered lineage. It is not a loaded
    /// receipt or native permission by itself; all final effects use the shared
    /// native+invocation canonical admission and retain this operation to finish.
    pub fn registered_document(&self) -> Result<NativeInvocationDocument, String> {
        self.with_active(|| {
            let state = self.operation.0.target.lock().map_err(|_| "The native invocation state is unavailable.")?;
            let target = state.as_ref().ok_or("The native target reservation was released.")?;
            let owner = target.registered.as_ref().ok_or("The native target has not registered.")?.clone();
            Ok(NativeInvocationDocument { target: self.clone(), owner })
        }).ok_or_else(|| "The native invocation target is no longer active.".to_owned())?
    }
}

/// Retains the actual reservation, its first registered owner and native
/// invocation lifetime through final worker publication. Never serialized.
#[derive(Clone)]
pub struct NativeInvocationDocument {
    target: NativeInvocationTarget,
    owner: NativeRequestOwner,
}

impl NativeInvocationDocument {
    pub fn owner(&self) -> &NativeRequestOwner { &self.owner }
    pub fn target(&self) -> &NativeInvocationTarget { &self.target }
    pub fn operation(&self) -> &NativeInvocationOperation { self.target.operation() }
    pub fn created(&self) -> &RendererGeneration { self.target.created() }
}

/// A real renderer request that activates a previously selected startup entry.
/// It constrains preparation until the immutable recovery seed is committed.
#[derive(Clone)]
pub struct RecoveryRequestConstraint {
    pub source: NativeRequestOwner,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
}

/// The actual startup action that selected this recovery child. Retained by
/// the target so asynchronous seed commit cannot lose the root's admission.
#[derive(Clone)]
pub struct StartupRecoveryConstraint {
    pub startup: NativeInvocationOperation,
    pub shell: NativeRequestOwner,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
}

/// An authenticated native service command activating an immutable recovery
/// entry. Retained through asynchronous preparation and seed application.
#[derive(Clone)]
pub struct CommandRecoveryConstraint {
    pub command: NativeInvocationOperation,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
}

#[derive(Clone)]
pub struct RecoveryAuthority {
    pub context: RecoveryOpenContext,
    pub request: Option<RecoveryRequestConstraint>,
    pub startup: Option<StartupRecoveryConstraint>,
    pub command: Option<CommandRecoveryConstraint>,
    revision_loss: CancelToken,
}

#[derive(Clone)]
pub struct RecoveryAuthoritySnapshot {
    target: RecoveryOpenTarget,
    authority: Arc<RecoveryAuthority>,
    pub registered: Option<NativeRequestOwner>,
}

impl std::ops::Deref for RecoveryAuthoritySnapshot {
    type Target = RecoveryAuthority;
    fn deref(&self) -> &Self::Target { &self.authority }
}

impl RecoveryAuthoritySnapshot {
    pub fn revision_loss_token(&self) -> CancelToken { self.authority.revision_loss.clone() }
    /// Pre-seed dialog cancellation fences. Registration is not a new authority
    /// epoch; callers still use this exact snapshot in the admission gate.
    pub fn preparation_loss_tokens(&self) -> Result<Vec<CancelToken>, String> {
        fn owner(tokens: &mut Vec<CancelToken>, owner: &NativeRequestOwner) -> Result<(), String> {
            tokens.push(owner.loss_token().ok_or("The captured recovery owner has no loss token.")?);
            Ok(())
        }
        fn operation(tokens: &mut Vec<CancelToken>, operation: &NativeInvocationOperation) {
            tokens.extend([operation.loss_token(), operation.cancel().clone(), operation.shutdown().clone()]);
        }
        let mut tokens = vec![self.revision_loss_token(), self.target.created().loss_token(), self.target.preparation_loss_token()];
        match &self.context {
            RecoveryOpenContext::Renderer { source, participants, cancel, pending } => {
                owner(&mut tokens, source)?;
                for participant in participants { owner(&mut tokens, participant)?; }
                tokens.extend([cancel.clone(), pending.clone()]);
            }
            RecoveryOpenContext::NativeStartup(child) => operation(&mut tokens, child),
        }
        if let Some(request) = &self.request {
            owner(&mut tokens, &request.source)?;
            for participant in &request.participants { owner(&mut tokens, participant)?; }
            tokens.extend([request.cancel.clone(), request.pending.clone()]);
        }
        if let Some(startup) = &self.startup {
            operation(&mut tokens, &startup.startup);
            owner(&mut tokens, &startup.shell)?;
            for participant in &startup.participants { owner(&mut tokens, participant)?; }
            tokens.extend([startup.cancel.clone(), startup.pending.clone()]);
        }
        if let Some(command) = &self.command {
            operation(&mut tokens, &command.command);
            for participant in &command.participants { owner(&mut tokens, participant)?; }
            tokens.extend([command.cancel.clone(), command.pending.clone()]);
        }
        if let Some(registered) = &self.registered { owner(&mut tokens, registered)?; }
        Ok(tokens)
    }
    pub fn belongs_to(&self, target: &RecoveryOpenTarget) -> bool { self.target.same_binding(target) }
    /// Compare the retained authority epoch and first-registration identity.
    pub fn same_snapshot(&self, other: &Self) -> bool {
        self.target.same_binding(&other.target) && Arc::ptr_eq(&self.authority, &other.authority)
            && match (&self.registered, &other.registered) {
                (None, None) => true,
                (Some(a), Some(b)) => same_captured_owner(a, b),
                _ => false,
            }
    }
    /// Identity-only observation for lifetime subscriptions; first registration
    /// changes registered state but does not replace the authority epoch.
    pub fn authority_is_current(&self) -> bool {
        self.target.0.authority.lock().is_ok_and(|authority| Arc::ptr_eq(&authority, &self.authority))
    }
    /// Claim only short in-memory lifecycle state; run cleanup after this returns.
    pub fn with_authority_current<R>(&self, effect: impl FnOnce() -> R) -> Option<R> {
        let authority = self.target.0.authority.lock().ok()?;
        if Arc::ptr_eq(&authority, &self.authority) { Some(effect()) } else { None }
    }

    /// The caller holds the canonical generation and cancellation gates.
    /// This state lock covers only the short prepared mutation, never UI or I/O.
    pub fn with_current<R>(&self, effect: impl FnOnce() -> R) -> Option<R> {
        let authority = self.target.0.authority.lock().ok()?;
        if !Arc::ptr_eq(&authority, &self.authority) { return None; }
        let registered = self.target.0.registered.lock().ok()?;
        let same = match (&*registered, &self.registered) {
            (None, None) => true,
            (Some(a), Some(b)) => a.view == b.view
                && Arc::ptr_eq(&a.context.as_arc(), &b.context.as_arc())
                && a.loss_token().zip(b.loss_token()).is_some_and(|(a, b)| a.same_token(&b)),
            _ => false,
        };
        if same { Some(effect()) } else { None }
    }

    /// First registration only. The caller holds the full native/creation plan.
    pub fn register_native_from_admitted(&self, owner: NativeRequestOwner) -> Result<(), String> {
        if self.registered.is_some() || owner.view != self.target.view()
            || !owner.context.renderer().is_some_and(|renderer| renderer.same_generation(self.target.created())) {
            return Err("The recovery registration does not match its reservation.".into());
        }
        let authority = self.target.0.authority.lock().map_err(|_| "The recovery authority is unavailable.")?;
        if !Arc::ptr_eq(&authority, &self.authority) { return Err("The recovery authority was replaced.".into()); }
        let mut registered = self.target.0.registered.lock().map_err(|_| "The recovery binding is unavailable.")?;
        if registered.is_some() { return Err("The recovery target was already registered.".into()); }
        if let Some(invocation) = self.target.invocation_target() {
            invocation.register_from_native_admitted(owner.clone())?;
        }
        *registered = Some(owner);
        Ok(())
    }
}

fn same_captured_owner(a: &NativeRequestOwner, b: &NativeRequestOwner) -> bool {
    a.view == b.view && Arc::ptr_eq(&a.context.as_arc(), &b.context.as_arc())
        && a.loss_token().zip(b.loss_token()).is_some_and(|(a, b)| a.same_token(&b))
}

struct NativeSaveRevisionState {
    selection: NativeSaveDestination,
    aliases: Vec<NativeRequestOwner>,
    loss: CancelToken,
}

/// One pending save's replaceable, generation-bound authority. The actor
/// retains this cell across worker and callback continuations.
#[derive(Clone)]
pub struct NativeSaveRevisionAuthority(Arc<Mutex<Arc<NativeSaveRevisionState>>>);

#[derive(Clone)]
pub struct NativeSaveRevisionSnapshot {
    authority: NativeSaveRevisionAuthority,
    state: Arc<NativeSaveRevisionState>,
}
/// Exact committed save epoch and only its request-local owner substitutions.
#[derive(Clone)]
pub struct NativeSaveRevisionReceipt {
    snapshot: NativeSaveRevisionSnapshot,
    predecessor: NativeSaveRevisionSnapshot,
    request_aliases: Arc<[(NativeRequestOwner, NativeRequestOwner)]>,
}

impl NativeSaveRevisionReceipt {
    pub fn snapshot(&self) -> &NativeSaveRevisionSnapshot { &self.snapshot }
    pub fn succeeds(&self, previous: &NativeSaveRevisionSnapshot) -> bool {
        self.predecessor.same_snapshot(previous)
    }
    pub fn renewed_request_owner(&self, owner: &NativeRequestOwner) -> Option<&NativeRequestOwner> {
        self.request_aliases.iter().find(|(old, _)| same_captured_owner(old, owner)).map(|(_, new)| new)
    }
    /// Return only a changed owner captured by this exact transition.
    pub fn renewed_owner(&self, owner: &NativeRequestOwner) -> Option<&NativeRequestOwner> {
        let binding = (self.predecessor.selection().binding().owner(), self.snapshot.selection().binding().owner());
        self.request_aliases.iter().map(|(old, new)| (old, new))
            .chain(std::iter::once(binding))
            .chain(self.predecessor.aliases().iter().zip(self.snapshot.aliases().iter()))
            .find(|(old, new)| same_captured_owner(old, owner) && !same_captured_owner(old, new))
            .map(|(_, new)| new)
    }
}


pub struct PreparedNativeSaveRevision {
    snapshot: NativeSaveRevisionSnapshot,
    replacement: Option<Arc<NativeSaveRevisionState>>,
    pairs: Vec<(PdfDocumentLease, PdfDocumentLease)>,
    request_aliases: Arc<[(NativeRequestOwner, NativeRequestOwner)]>,
    committed: bool,
    committed_state: Option<Arc<NativeSaveRevisionState>>,
    notified: bool,
}

/// A preallocated selection replacement for one exact pending save epoch.
pub struct PreparedNativeSaveSelection {
    snapshot: NativeSaveRevisionSnapshot,
    replacement: Option<Arc<NativeSaveRevisionState>>,
    key: Arc<()>,
    committed: bool,
    notified: bool,
}
pub struct AdmittedNativeSaveSelection { key: Arc<()> }

impl NativeSaveRevisionAuthority {
    pub fn new(selection: NativeSaveDestination, aliases: Vec<NativeRequestOwner>) -> Self {
        Self(Arc::new(Mutex::new(Arc::new(NativeSaveRevisionState {
            selection, aliases, loss: CancelToken::default(),
        }))))
    }
    pub fn snapshot(&self) -> Result<NativeSaveRevisionSnapshot, String> {
        let state = self.0.lock().map_err(|_| "The native save revision authority is unavailable.")?.clone();
        Ok(NativeSaveRevisionSnapshot { authority: self.clone(), state })
    }
}

impl NativeSaveRevisionSnapshot {
    pub fn selection(&self) -> &NativeSaveDestination { &self.state.selection }
    pub fn aliases(&self) -> &[NativeRequestOwner] { &self.state.aliases }
    pub fn loss_token(&self) -> CancelToken { self.state.loss.clone() }
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.authority.0, &other.authority.0) && Arc::ptr_eq(&self.state, &other.state)
    }
    /// Hold only across a short in-memory effect; never invoke UI, I/O or user callbacks.
    pub fn with_current<R>(&self, effect: impl FnOnce() -> R) -> Option<R> {
        let current = self.authority.0.lock().ok()?;
        if Arc::ptr_eq(&current, &self.state) { Some(effect()) } else { None }
    }
    /// Stage a destination selected by the same exact native save operation.
    pub fn prepare_selection(&self, selection: NativeSaveDestination) -> Result<PreparedNativeSaveSelection, String> {
        if !selection.matches_binding(self.state.selection.binding()) {
            return Err("The selected destination belongs to a different native save.".into());
        }
        Ok(PreparedNativeSaveSelection {
            snapshot: self.clone(),
            replacement: Some(Arc::new(NativeSaveRevisionState {
                selection, aliases: self.state.aliases.clone(), loss: CancelToken::default(),
            })),
            key: Arc::new(()), committed: false, notified: false,
        })
    }
    /// Allocate all successor owner subscriptions before entering any gate.
    /// Raw pairs include PDF views that have no renderer owner.
    pub fn prepare(&self, pairs: Vec<(PdfDocumentLease, PdfDocumentLease)>) -> Result<PreparedNativeSaveRevision, String> {
        self.prepare_with_request_aliases(pairs, &[])
    }
    /// Request-local callback owners are renewed for this transition only;
    /// they are never retained in the persistent authority cell.
    pub fn prepare_with_request_aliases(&self, pairs: Vec<(PdfDocumentLease, PdfDocumentLease)>,
        request_aliases: &[NativeRequestOwner]) -> Result<PreparedNativeSaveRevision, String> {
        if pairs.is_empty() || pairs.iter().any(|(old, new)| old.same_document(new))
            || pairs.iter().any(|(old, new)| pairs.iter().any(|(other_old, other_new)|
                (old.same_document(other_old) && !new.same_document(other_new))
                    || (!old.same_document(other_old) && new.same_document(other_new))
                    || new.same_document(other_old))) {
            return Err("The PDF revision successor set is inconsistent.".into());
        }
        let binding_owner = self.state.selection.binding().owner();
        let renew = |owner: &NativeRequestOwner| -> Result<NativeRequestOwner, String> {
            match owner.pdf_revision().and_then(|revision| pairs.iter().find(|(old, _)| old.same_document(revision))) {
                Some((_, successor)) => Ok(owner.prepare_pdf_revision_successor(successor.clone())?.owner),
                None => Ok(owner.clone()),
            }
        };
        let mut selection = self.state.selection.clone();
        selection.binding.owner = renew(binding_owner)?;
        let aliases = self.state.aliases.iter().map(renew).collect::<Result<Vec<_>, _>>()?;
        let request_aliases = request_aliases.iter()
            .map(|owner| Ok((owner.clone(), renew(owner)?))).collect::<Result<Vec<_>, String>>()?.into();
        Ok(PreparedNativeSaveRevision {
            snapshot: self.clone(),
            replacement: Some(Arc::new(NativeSaveRevisionState { selection, aliases, loss: CancelToken::default() })),
            pairs, request_aliases, committed: false, committed_state: None, notified: false,
        })
    }
}

impl PreparedNativeSaveSelection {
    pub fn snapshot(&self) -> &NativeSaveRevisionSnapshot { &self.snapshot }
    pub fn is_committed(&self) -> bool { self.committed }
    pub(crate) fn admit(&self) -> AdmittedNativeSaveSelection { AdmittedNativeSaveSelection { key: self.key.clone() } }
    pub fn commit(&mut self, permit: &AdmittedNativeSaveSelection) -> Result<NativeSaveRevisionSnapshot, String> {
        if self.committed || self.replacement.is_none() || !Arc::ptr_eq(&self.key, &permit.key) {
            return Err("The native save selection transition is unavailable.".into());
        }
        let authority = self.snapshot.authority.clone();
        let mut current = authority.0.lock().map_err(|_| "The native save revision authority is unavailable.")?;
        if !Arc::ptr_eq(&current, &self.snapshot.state) {
            return Err("The native save revision authority was replaced.".into());
        }
        let replacement = self.replacement.take().expect("validated selection replacement");
        *current = replacement.clone();
        self.committed = true;
        drop(current);
        Ok(NativeSaveRevisionSnapshot { authority, state: replacement })
    }
    pub fn notify_replaced(&mut self) {
        if self.committed && !self.notified {
            self.notified = true;
            self.snapshot.state.loss.cancel();
        }
    }
}

impl PreparedNativeSaveRevision {
    pub fn snapshot(&self) -> &NativeSaveRevisionSnapshot { &self.snapshot }
    pub fn pairs(&self) -> &[(PdfDocumentLease, PdfDocumentLease)] { &self.pairs }
    pub fn is_committed(&self) -> bool { self.committed }
    pub fn request_aliases(&self) -> impl Iterator<Item = &NativeRequestOwner> {
        self.request_aliases.iter().map(|(old, _)| old)
    }
    /// This is available only after the exact transition committed.
    pub fn renewed_request_owner(&self, owner: &NativeRequestOwner) -> Option<&NativeRequestOwner> {
        if !self.committed { return None; }
        self.request_aliases.iter().find(|(old, _)| same_captured_owner(old, owner)).map(|(_, new)| new)
    }
    /// Retain this committed epoch even if a later transition replaces it.
    pub fn receipt(&self) -> Option<NativeSaveRevisionReceipt> {
        Some(NativeSaveRevisionReceipt {
            snapshot: NativeSaveRevisionSnapshot {
                authority: self.snapshot.authority.clone(),
                state: self.committed_state.as_ref()?.clone(),
            },
            predecessor: self.snapshot.clone(),
            request_aliases: self.request_aliases.clone(),
        })
    }
    /// The caller holds the exact native/owner/PDF/cancellation admission.
    /// No application callbacks or I/O may run while the authority cell is locked.
    pub fn commit(&mut self, admitted: &mut super::AdmittedPdfRevisionSet<'_, '_>) -> Result<NativeSaveRevisionSnapshot, String> {
        if self.committed || self.replacement.is_none()
            || !admitted.matches_exact(&self.pairs) {
            return Err("The prepared native save revision does not match its admission.".into());
        }
        let authority = self.snapshot.authority.clone();
        let mut current = authority.0.lock().map_err(|_| "The native save revision authority is unavailable.")?;
        if !Arc::ptr_eq(&current, &self.snapshot.state) {
            return Err("The native save revision authority was replaced.".into());
        }
        admitted.retire_all()?;
        let replacement = self.replacement.take().expect("validated revision replacement");
        *current = replacement.clone();
        self.committed_state = Some(replacement.clone());
        self.committed = true;
        drop(current);
        Ok(NativeSaveRevisionSnapshot { authority, state: replacement })
    }
    /// Signal old snapshot observers only after admission and subscription cleanup.
    pub fn notify_replaced(&mut self) {
        if self.committed && !self.notified {
            self.notified = true;
            self.snapshot.state.loss.cancel();
        }
    }
}

/// A fully allocated successor authority. It grants no effects until committed
/// together with the exact captured PDF revision retirement.
pub struct PreparedRecoveryAuthorityRenewal {
    snapshot: RecoveryAuthoritySnapshot,
    replacement: Option<Arc<RecoveryAuthority>>,
    registered: Option<NativeRequestOwner>,
    sources: Vec<NativeRequestOwner>,
    staged: Vec<Option<PreparedPdfRevisionOwner>>,
    outputs: Vec<Option<NativeRequestOwner>>,
    retired_authority: Option<Arc<RecoveryAuthority>>,
    retired_registered: Option<NativeRequestOwner>,
    committed: bool,
    notified: bool,
}

impl RecoveryAuthoritySnapshot {
    pub fn prepare_pdf_revision_renewal(&self, explicit: &[NativeRequestOwner],
        old: &PdfDocumentLease, successor: &PdfDocumentLease) -> Result<PreparedRecoveryAuthorityRenewal, String> {
        let mut sources: Vec<NativeRequestOwner> = Vec::new();
        let mut staged: Vec<Option<PreparedPdfRevisionOwner>> = Vec::new();
        let mut successors: Vec<NativeRequestOwner> = Vec::new();
        let mut replace = |owner: &mut NativeRequestOwner| -> Result<(), String> {
            if !owner.pdf_revision().is_some_and(|revision| revision.same_document(old)) { return Ok(()); }
            let index = match sources.iter().position(|source| same_captured_owner(source, owner)) {
                Some(index) => index,
                None => {
                    let prepared = owner.prepare_pdf_revision_successor(successor.clone())?;
                    sources.push(owner.clone());
                    successors.push(prepared.owner.clone());
                    staged.push(Some(prepared));
                    sources.len() - 1
                }
            };
            *owner = successors[index].clone();
            Ok(())
        };
        for owner in explicit {
            let mut candidate = owner.clone();
            replace(&mut candidate)?;
        }
        let mut authority = (*self.authority).clone();
        authority.revision_loss = CancelToken::default();
        if let Some(startup) = &mut authority.startup {
            replace(&mut startup.shell)?;
            for owner in &mut startup.participants { replace(owner)?; }
        }
        if let Some(command) = &mut authority.command {
            for owner in &mut command.participants { replace(owner)?; }
        }
        let mut registered = self.registered.clone();
        if let Some(owner) = &mut registered { replace(owner)?; }
        if staged.is_empty() { return Err("No captured recovery owner has the PDF revision.".into()); }
        let outputs = vec![None; staged.len()];
        Ok(PreparedRecoveryAuthorityRenewal {
            snapshot: self.clone(), replacement: Some(Arc::new(authority)), registered,
            sources, staged, outputs, retired_authority: None, retired_registered: None, committed: false, notified: false,
        })
    }
}

impl PreparedRecoveryAuthorityRenewal {
    pub fn sources(&self) -> &[NativeRequestOwner] { &self.sources }
    pub fn matches(&self, receipt: &RecoverySeedReceipt) -> bool {
        self.snapshot.target.same_binding(receipt.target()) && receipt.matches(receipt.target())
    }
    pub fn successor(&self, index: usize) -> Option<&NativeRequestOwner> {
        if self.committed { self.outputs.get(index)?.as_ref() } else { None }
    }
    pub fn successor_for(&self, source: &NativeRequestOwner) -> Option<&NativeRequestOwner> {
        let index = self.sources.iter().position(|candidate| same_captured_owner(candidate, source))?;
        self.successor(index)
    }
    /// Notify retained observers only after every admission gate is released.
    pub fn notify_replaced(&mut self) {
        if self.committed && !self.notified {
            self.notified = true;
            if let Some(retired) = &self.retired_authority { retired.revision_loss.cancel(); }
        }
    }

    /// Call under the canonical captured-owner and PDF retirement gates.
    pub fn commit(&mut self, admitted: &mut AdmittedPdfRevisionRetirement<'_, '_>) -> Result<(), String> {
        let mut authority = self.snapshot.target.0.authority.lock().map_err(|_| "The recovery authority is unavailable.")?;
        if !Arc::ptr_eq(&authority, &self.snapshot.authority) { return Err("The recovery authority was replaced.".into()); }
        let mut registered = self.snapshot.target.0.registered.lock().map_err(|_| "The recovery binding is unavailable.")?;
        let same = match (&*registered, &self.snapshot.registered) {
            (None, None) => true,
            (Some(a), Some(b)) => same_captured_owner(a, b),
            _ => false,
        };
        if !same || self.committed || self.replacement.is_none() {
            return Err("The recovery authority was replaced.".into());
        }
        admitted.commit_many(&mut self.staged, &mut self.outputs)?;
        self.retired_authority = Some(std::mem::replace(&mut *authority, self.replacement.take().expect("validated recovery authority")));
        self.retired_registered = std::mem::replace(&mut *registered, self.registered.take());
        self.committed = true;
        Ok(())
    }
}

struct RecoveryBinding {
    context: RecoveryOpenContext,
    view: ViewId,
    created: RendererGeneration,
    closure: NativeClosureReservation,
    browser_owner: NativeBrowserOwner,
    invocation: Option<NativeInvocationTarget>,
    authority: Mutex<Arc<RecoveryAuthority>>,
    registered: Mutex<Option<NativeRequestOwner>>,
    request: Option<RecoveryRequestConstraint>,
    startup: Option<StartupRecoveryConstraint>,
    command: Option<CommandRecoveryConstraint>,
    seed_committed: Mutex<bool>,
    preparation_finished: CancelToken,
    settlement: Mutex<Option<Result<(), String>>>,
    settlement_ready: CancelToken,
    work: Arc<Mutex<RecoveryWorkState>>,
}

/// The native creator binds this before queueing the original recovery input
/// and before load. A registry consumer receives this bound context, not a bare
/// ViewId or a newly manufactured startup authority.
#[derive(Clone)]
pub struct RecoveryOpenTarget(Arc<RecoveryBinding>);

/// Receipt of one successful immutable seed enqueue, not document readiness.
/// Its queue identity is the exact one-shot bound target allocation.
#[derive(Clone)]
pub struct RecoverySeedReceipt { target: RecoveryOpenTarget }

type CoreWaiter = (Arc<dyn crate::runtime::Thread>, Box<dyn FnOnce() + Send>);

#[derive(Default)]
struct RecoveryWorkState {
    closing: bool,
    active: usize,
    waiters: Vec<CoreWaiter>,
}

/// Liveness only, not permission to perform an effect. Retain a clone through
/// both the worker and its final core continuation.
#[derive(Clone)]
pub struct RecoveryWorkTicket { _lease: Arc<RecoveryWorkLease> }

struct RecoveryWorkLease { state: Arc<Mutex<RecoveryWorkState>> }

impl Drop for RecoveryWorkLease {
    fn drop(&mut self) {
        let ready = {
            let Ok(mut state) = self.state.lock() else { return; };
            state.active -= 1;
            if state.closing && state.active == 0 { std::mem::take(&mut state.waiters) } else { Vec::new() }
        };
        // Thread is the actual core queue. Never invoke a drain continuation
        // synchronously from a worker destructor or inside admission.
        for (core, done) in ready { core.post(done); }
    }
}

impl RecoverySeedReceipt {
    pub fn target(&self) -> &RecoveryOpenTarget { &self.target }
    pub fn matches(&self, target: &RecoveryOpenTarget) -> bool {
        self.target.same_binding(target) && target.is_seed_committed()
    }
    pub fn preparation_loss_token(&self) -> CancelToken { self.target.preparation_loss_token() }
    pub fn begin_work(&self) -> Result<RecoveryWorkTicket, String> {
        if !self.target.is_seed_committed() { return Err("The recovery seed has not committed.".into()); }
        self.target.retain_work()
    }

    /// The native adapter must already admit the exact registered consumer.
    /// Notification is returned separately so callbacks never run under gates.
    pub fn settle_from_admitted(&self, result: Result<(), String>) -> Result<DeferredRecoverySettlement, String> {
        if !self.target.is_seed_committed() { return Err("The recovery seed has not committed.".into()); }
        let mut settlement = self.target.0.settlement.lock().map_err(|_| "The recovery settlement is unavailable.")?;
        if settlement.is_some() { return Err("The recovery seed was already settled.".into()); }
        *settlement = Some(result);
        Ok(DeferredRecoverySettlement { ready: self.target.0.settlement_ready.clone() })
    }

    /// Loss cleanup may only fail this exact seed, never acknowledge success.
    pub fn fail_after_loss(&self, error: String) -> Result<DeferredRecoverySettlement, String> {
        self.settle_from_admitted(Err(error))
    }

    pub fn settlement(&self) -> Option<Result<(), String>> {
        if !self.target.0.settlement_ready.is_cancelled() { return None; }
        self.target.0.settlement.lock().ok()?.clone()
    }

    pub fn on_settled(&self, callback: impl FnOnce(Result<(), String>) + Send + 'static) -> CancelSubscription {
        let receipt = self.clone();
        self.target.0.settlement_ready.on_cancel(move || {
            if let Some(result) = receipt.settlement() { callback(result); }
        })
    }
}

pub struct DeferredRecoverySettlement { ready: CancelToken }

impl DeferredRecoverySettlement {
    /// Call only after every native, document and seed-state gate has dropped.
    pub fn notify(self) { self.ready.cancel(); }
}

impl RecoveryOpenTarget {
    pub fn bind(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner) -> Result<Self, String> {
        Self::bind_with_constraint(context, view, created, closure, owner, None, None, None)
    }

    pub fn bind_request(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner, request: RecoveryRequestConstraint) -> Result<Self, String> {
        Self::bind_with_constraint(context, view, created, closure, owner, Some(request), None, None)
    }

    pub fn bind_startup(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner, startup: StartupRecoveryConstraint, request: Option<RecoveryRequestConstraint>) -> Result<Self, String> {
        Self::bind_with_constraint(context, view, created, closure, owner, request, Some(startup), None)
    }

    pub fn bind_command(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner, command: CommandRecoveryConstraint) -> Result<Self, String> {
        Self::bind_with_constraint(context, view, created, closure, owner, None, None, Some(command))
    }

    /// A remembered startup command retains all three immutable operations.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_startup_command(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner,
        startup: StartupRecoveryConstraint, command: CommandRecoveryConstraint,
        request: Option<RecoveryRequestConstraint>) -> Result<Self, String> {
        Self::bind_with_constraint(context, view, created, closure, owner, request, Some(startup), Some(command))
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_with_constraint(context: RecoveryOpenContext, view: ViewId, created: RendererGeneration,
        closure: NativeClosureReservation, owner: NativeBrowserOwner, request: Option<RecoveryRequestConstraint>, startup: Option<StartupRecoveryConstraint>,
        command: Option<CommandRecoveryConstraint>) -> Result<Self, String> {
        if !closure.matches(view, &created) || !owner.reservation().same_reservation(&closure) {
            return Err("The recovery target does not match its native creation reservation.".into());
        }
        match (&context, &startup, &command) {
            (RecoveryOpenContext::NativeStartup(child), Some(root), Some(command))
                if root.startup.purpose() == NativeInvocationPurpose::StartupLaunch
                    && root.startup.startup_owner().is_some_and(|original| same_captured_owner(original, &root.shell))
                    && command.command.is_native_command() && command.command.is_child_of(&root.startup)
                    && child.is_child_of(&command.command)
                    && root.startup.shutdown().same_token(command.command.shutdown())
                    && command.command.shutdown().same_token(child.shutdown())
                    && matches!(command.command.selection(),
                        NativeInvocationSelection::NativeCommand(NativeEditorCommand::OpenFile { surface, .. })
                            if *surface == child.selection().surface()) => {}
            (RecoveryOpenContext::NativeStartup(child), Some(root), None)
                if child.is_child_of(&root.startup) && root.startup.purpose() == NativeInvocationPurpose::StartupLaunch => {}
            (RecoveryOpenContext::NativeStartup(child), None, Some(root))
                if child.is_child_of(&root.command) && root.command.is_root_native_command() => {}
            (_, Some(_), _) | (_, _, Some(_)) =>
                return Err("The captured authority does not own this recovery child.".into()),
            (RecoveryOpenContext::NativeStartup(child), None, None) if child.0.parent.is_some() && request.is_none() =>
                return Err("The recovery constructor requires its captured root.".into()),
            _ => {}
        }
        let invocation = match &context {
            RecoveryOpenContext::Renderer { .. } => None,
            RecoveryOpenContext::NativeStartup(operation) => {
                if operation.purpose() != NativeInvocationPurpose::Recovery {
                    return Err("The native invocation is not a recovery operation.".into());
                }
                Some(operation.reserve_target(view, created.clone())?)
            }
        };
        let authority = Arc::new(RecoveryAuthority { context: context.clone(), request: request.clone(), startup: startup.clone(), command: command.clone(), revision_loss: CancelToken::default() });
        Ok(Self(Arc::new(RecoveryBinding { context, view, created, closure, browser_owner: owner, invocation, authority: Mutex::new(authority),
            registered: Mutex::new(None), request, startup, command, seed_committed: Mutex::new(false),
            preparation_finished: CancelToken::default(), settlement: Mutex::new(None),
            settlement_ready: CancelToken::default(), work: Arc::new(Mutex::new(RecoveryWorkState::default())) })))
    }
    pub fn context(&self) -> &RecoveryOpenContext { &self.0.context }
    pub fn same_binding(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
    pub fn browser_owner(&self) -> &NativeBrowserOwner { &self.0.browser_owner }
    pub fn closure_reservation(&self) -> &NativeClosureReservation { &self.0.closure }
    pub fn request_constraint(&self) -> Option<&RecoveryRequestConstraint> { self.0.request.as_ref() }
    pub fn startup_constraint(&self) -> Option<&StartupRecoveryConstraint> { self.0.startup.as_ref() }
    pub fn command_constraint(&self) -> Option<&CommandRecoveryConstraint> { self.0.command.as_ref() }
    pub fn authority_snapshot(&self) -> Result<RecoveryAuthoritySnapshot, String> {
        let authority = self.0.authority.lock().map_err(|_| "The recovery authority is unavailable.")?;
        let registered = self.0.registered.lock().map_err(|_| "The recovery binding is unavailable.")?;
        Ok(RecoveryAuthoritySnapshot { target: self.clone(), authority: authority.clone(), registered: registered.clone() })
    }
    pub fn preparation_loss_token(&self) -> CancelToken { self.0.preparation_finished.clone() }
    /// Liveness for a preparation worker and its final core continuation,
    /// including workers that start before immutable seed commit. This is not
    /// admission to read or publish; the caller retains its preparation gates.
    pub fn begin_preparation_work(&self) -> Result<RecoveryWorkTicket, String> { self.retain_work() }

    fn retain_work(&self) -> Result<RecoveryWorkTicket, String> {
        let mut state = self.0.work.lock().map_err(|_| "The recovery work state is unavailable.")?;
        if state.closing { return Err("The recovery target is draining.".into()); }
        state.active = state.active.checked_add(1).ok_or("The recovery work count is exhausted.")?;
        Ok(RecoveryWorkTicket { _lease: Arc::new(RecoveryWorkLease { state: self.0.work.clone() }) })
    }

    /// Close worker admission and wait for every retained ticket, including
    /// core continuations. Call from the real target cleanup before acknowledging
    /// Closed/NotCreated to the opener/runtime drain.
    pub fn drain_work(&self, core: Arc<dyn crate::runtime::Thread>, done: Box<dyn FnOnce() + Send>) -> Result<(), String> {
        let mut state = self.0.work.lock().map_err(|_| "The recovery work state is unavailable.")?;
        state.closing = true;
        if state.active == 0 {
            drop(state);
            core.post(done);
        } else {
            state.waiters.push((core, done));
        }
        Ok(())
    }
    pub fn is_seed_committed(&self) -> bool {
        self.0.seed_committed.lock().is_ok_and(|committed| *committed)
    }
    pub fn committed_seed(&self) -> Option<RecoverySeedReceipt> {
        self.is_seed_committed().then(|| RecoverySeedReceipt { target: self.clone() })
    }
    /// State transition only, inside canonical preparation admission. The
    /// callback must enqueue the validated immutable seed without reentering
    /// this binding or invoking callbacks. An enqueue failure remains preparing.
    pub fn commit_seed_from_admitted(&self, enqueue: impl FnOnce() -> Result<(), String>) -> Result<RecoverySeedReceipt, String> {
        let mut committed = self.0.seed_committed.lock().map_err(|_| "The recovery seed state is unavailable.")?;
        if *committed { return Err("The recovery seed was already committed.".into()); }
        enqueue()?;
        *committed = true;
        Ok(RecoverySeedReceipt { target: self.clone() })
    }
    pub fn view(&self) -> ViewId { self.0.view }
    pub fn created(&self) -> &RendererGeneration { &self.0.created }
    pub fn invocation_target(&self) -> Option<&NativeInvocationTarget> { self.0.invocation.as_ref() }
    pub fn registered_owner(&self) -> Option<NativeRequestOwner> {
        self.0.registered.lock().ok()?.clone()
    }
    pub fn is_unregistered(&self) -> bool {
        self.0.registered.lock().is_ok_and(|registered| registered.is_none())
    }
    /// Called only by the shell's attested first-Registered producer helper,
    /// inside its actual native/creation admission. Never call from IPC data.
    pub fn retain_native_registration(&self, owner: NativeRequestOwner) -> Result<(), String> {
        if owner.view != self.view() || !owner.context.renderer().is_some_and(|renderer| renderer.same_generation(self.created())) {
            return Err("The recovery target does not match its constructor reservation.".into());
        }
        let mut registered = self.0.registered.lock().map_err(|_| "The recovery binding is unavailable.")?;
        if registered.is_some() { return Err("The recovery target was already registered.".into()); }
        *registered = Some(owner);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
