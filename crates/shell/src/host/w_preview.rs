//! Quick Look and document picker windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::native::{NativeEventKind, WindowExt};
use main_core::packages::i18n::Lang;
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::doc_picker::asker::DocAsker;
use main_core::shell::doc_picker::doc_list::{
    DocListDeps, OpenDocumentEntry, ProjectFile, RecentDocumentEntry,
};
use main_core::shell::projects::project_files::{ListVisibleFiles, project_mention_files};
use main_core::shell::quick_look::{ParsedFile as PreviewParsedFile, QuickLookEngines, ZipArchive};
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::{Hosts, Surface};
use shell_ipc_w::doc_picker::{
    DocPickerControl, DocPickerSources, DocPickerSourcesHost, DocPickerWindow, DocPickerWindowHost,
    DocPickerWindowOptions,
};
use shell_ipc_w::editor_files::{FileParseHost, UiLanguage};
use shell_ipc_w::quick_look::{
    QuickLookControl, QuickLookEnginesHost, QuickLookHost, QuickLookWindow, QuickLookWindowHost,
    QuickLookWindowOptions,
};
use shell_ipc_w::tabs::TabsState;
use tauri::Manager;

use super::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use super::{HostContext, on_ui, w::post_core, w_files::ShellLanguage};
use crate::paths::ShellPaths;

mod runtime;

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let quick: QuickLookWindowHost = Arc::new(QuickWindow::new(cx));
    let runtime: QuickLookHost = Arc::new(runtime::Runtime::new(cx));
    hosts.insert(runtime);
    hosts.insert(quick);
    let picker: DocPickerWindowHost = Arc::new(PickerWindow::new(cx));
    hosts.insert(picker);
    let sources: DocPickerSourcesHost = Arc::new(PickerSources);
    hosts.insert(sources);
    let parser = hosts
        .get::<FileParseHost>()
        .expect("file parser host installed before preview")
        .clone();
    let engines: QuickLookEnginesHost = Arc::new(PreviewEngines(parser));
    hosts.insert(engines);
}

struct PreviewEngines(FileParseHost);
struct PreviewArchive(ooxml_pkg::jszip::Archive);

impl QuickLookEngines for PreviewEngines {
    fn parse_file_to_text(&self, path: &str) -> Result<PreviewParsedFile, String> {
        let parsed = self.0.parse_file_to_text(std::path::Path::new(path))?;
        Ok(PreviewParsedFile {
            ok: parsed.ok,
            text: parsed.text,
            error: parsed.error,
        })
    }

    fn open_zip(&self, bytes: Vec<u8>) -> Option<Box<dyn ZipArchive>> {
        ooxml_pkg::assert_zip_inflates_within_limits(&bytes, &ooxml_pkg::DEFAULT_ZIP_LIMITS)
            .ok()?;
        Some(Box::new(PreviewArchive(
            ooxml_pkg::jszip::open_owned(bytes).ok()?,
        )))
    }
}

impl ZipArchive for PreviewArchive {
    fn entry(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.entry(name)
    }
}
struct PickerSources;

impl DocListDeps for PickerSources {
    fn open_documents(&self) -> Result<Vec<OpenDocumentEntry>, String> {
        super::w_files::open_documents().map(|documents| {
            documents
                .into_iter()
                .map(|document| OpenDocumentEntry {
                    id: document.id,
                    path: document.file_path,
                    title: document.title,
                    dirty: document.dirty,
                })
                .collect()
        })
    }

    fn recent(&self) -> Vec<RecentDocumentEntry> {
        use shell_contract::runtime::{AppPaths, PathName};
        let path = ShellPaths::new()
            .path(PathName::UserData)
            .join("recent.json");
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        let Ok(jscompat::JsonValue::Array(items)) =
            jscompat::json_parse(&String::from_utf8_lossy(&bytes))
        else {
            return Vec::new();
        };
        items
            .into_iter()
            .filter_map(|item| match item {
                jscompat::JsonValue::String(path) => Some(RecentDocumentEntry {
                    title: jscompat::node_path::host::basename(&path, None),
                    path,
                    missing: false,
                }),
                _ => None,
            })
            .collect()
    }

    fn project_files(&self, asker: &DocAsker) -> Vec<ProjectFile> {
        let Some(view) = asker.web_contents_id else {
            return Vec::new();
        };
        let visibility = super::w::on_core(move |provided| {
            let projects = provided
                .all::<shell_ipc_w::projects::Projects>()
                .into_iter()
                .next()?;
            let project = projects.window_project(i64::from(view))?;
            projects.guide().ai_visibility(&project).ok().flatten()
        })
        .flatten();
        let Some(visibility) = visibility else {
            return Vec::new();
        };
        match project_mention_files(&visibility, None, &mut ListVisibleFiles) {
            Ok(files) => files
                .into_iter()
                .map(|file| ProjectFile {
                    path: file.path,
                    title: file.title,
                })
                .collect(),
            Err(error) => {
                eprintln!("listing picker project files: {}", error.0);
                Vec::new()
            }
        }
    }

    fn stat(&self, path: &str) -> Option<f64> {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.len() as f64)
    }
}

impl DocPickerSources for PickerSources {
    fn asker_for(&self, view: ViewId) -> DocAsker {
        super::w::on_core(move |provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .map(|tabs| tabs.doc_asker(view))
        })
        .flatten()
        .unwrap_or_else(|| DocAsker {
            web_contents_id: Some(view.0),
            ..Default::default()
        })
    }
}

fn theme() -> String {
    let settings = read_app_settings(&ShellPaths::new().settings_file().to_string_lossy());
    match settings.get("theme") {
        Some(jscompat::JsonValue::String(value)) if value == "light" || value == "dark" => {
            value.clone()
        }
        _ => "system".to_owned(),
    }
}

struct WindowState {
    cx: HostContext,
    window: Arc<Mutex<Option<WindowId>>>,
}

impl WindowState {
    fn new(cx: &HostContext) -> Self {
        Self {
            cx: cx.clone(),
            window: Arc::new(Mutex::new(None)),
        }
    }

    fn lang(&self) -> Lang {
        ShellLanguage(self.cx.ui.clone()).lang()
    }

    fn window(&self) -> Option<WindowId> {
        *self.window.lock().expect("preview window state")
    }

    fn with_window<R: Send + 'static>(
        &self,
        f: impl FnOnce(tauri::WebviewWindow) -> R + Send + 'static,
    ) -> Option<R> {
        let window = self.window()?;
        let app = self.cx.app.clone();
        let views = self.cx.views.clone();
        on_ui(&self.cx.ui, move || {
            let label = views.label_of_window(window)?;
            app.get_webview_window(&label).map(f)
        })
    }

    fn close(&self) {
        if let Some(window) = self.window() {
            let windows = self.cx.windows.clone();
            on_ui(&self.cx.ui, move || windows.close_window(window))
                .unwrap_or_else(|error| eprintln!("closing preview window: {error}"));
        }
    }
}

struct QuickWindow {
    state: WindowState,
    control: Mutex<Option<QuickLookControl>>,
    title_lifecycle: Mutex<Option<(WindowId, Arc<crate::views::NativeWindowLifecycle>)>>,
}

impl QuickWindow {
    fn new(cx: &HostContext) -> Self {
        Self {
            state: WindowState::new(cx),
            control: Mutex::new(None),
            title_lifecycle: Mutex::new(None),
        }
    }
}

impl QuickLookWindow for QuickWindow {
    fn attach(&self, control: QuickLookControl) {
        *self.control.lock().expect("quick look control") = Some(control);
    }
    fn lang(&self) -> Lang {
        self.state.lang()
    }
    fn theme(&self) -> String {
        theme()
    }
    fn create(&self, options: &QuickLookWindowOptions) -> ViewId {
        let control = self
            .control
            .lock()
            .expect("quick look control")
            .as_ref()
            .expect("quick look control attached")
            .clone();
        let ready = control.clone();
        let views = self.state.cx.views.clone();
        let slot = self.state.window.clone();
        let app = self.state.cx.app.clone();
        let spec = WindowSpec {
            title: options.title.clone(),
            size: (options.width as f64, options.height as f64),
            min_size: Some((options.min_width as f64, options.min_height as f64)),
            visible: options.show,
            decorations: options.frame,
            transparent: options.transparent,
            resizable: true,
            parent: None,
            focus: false,
            main: false,
            view: ViewSpec {
                url: "genoffice-app://shell/quick-look.html".into(),
                bridge: Bridge::Surface(Surface::ShellQuickLook),
            },
        };
        let extras = WindowOptions {
            center: options.center,
            always_on_top: options.always_on_top,
            minimizable: options.minimizable,
            maximizable: options.maximizable,
            fullscreenable: options.fullscreenable,
            shadow: options.has_shadow,
            background_throttling: None,
            request_policy: None,
            on_open: None,
            on_navigation: None,
            on_frame: Some(Arc::new(move |view, event| match &event.kind {
                NativeEventKind::LoadFinished { ok: true, .. } => ready.window_ready(view),
                NativeEventKind::LoadFinished { ok: false, .. } => {
                    eprintln!("quick look page failed to load; closing the preview window");
                    if let Some(window) = app.get_window(&format!("view-{}", view.0)) {
                        if let Err(error) = window.close() {
                            eprintln!("closing the failed preview window: {error}");
                        }
                    }
                }
                _ => {}
            })),
            on_closed: Some(Box::new(move |window| {
                let view = views.views_of_window(window).into_iter().next();
                let mut current = slot.lock().expect("preview window state");
                if *current == Some(window) {
                    *current = None;
                }
                drop(current);
                if let Some(view) = view {
                    std::thread::spawn(move || control.window_closed(view));
                }
            })),
        };
        let windows = self.state.cx.windows.clone();
        let views = self.state.cx.views.clone();
        let (window, view, lifecycle) = on_ui(&self.state.cx.ui, move || {
            let (window, view) = windows.open_window_with_options(spec, extras)?;
            let lifecycle = views
                .window_lifecycle(window)
                .ok_or("The preview window lifecycle is unavailable")?;
            Ok::<_, String>((window, view, lifecycle))
        })
        .unwrap_or_else(|error| panic!("opening quick look: {error}"));
        *self
            .title_lifecycle
            .lock()
            .expect("preview title lifecycle") = Some((window, lifecycle));
        *self.state.window.lock().expect("preview window state") = Some(window);
        view
    }
    fn is_visible(&self) -> bool {
        self.state
            .with_window(|window| {
                window.is_visible().unwrap_or_else(|error| {
                    eprintln!("reading preview visibility: {error}");
                    false
                })
            })
            .unwrap_or(false)
    }
    fn set_title(&self, title: &str) {
        let lifecycle = {
            let id = self.state.window();
            self.title_lifecycle
                .lock()
                .expect("preview title lifecycle")
                .as_ref()
                .filter(|(window, _)| Some(*window) == id)
                .map(|(_, lifecycle)| lifecycle.clone())
        };
        let Some(lifecycle) = lifecycle else {
            return;
        };
        let title = title.to_owned();
        on_ui(&self.state.cx.ui, move || {
            if let Err(error) = lifecycle.set_title(&title) {
                eprintln!("setting preview title: {error}");
            }
        });
    }
    fn show_inactive(&self) {
        let Some(id) = self.state.window() else {
            return;
        };
        let Some(label) = self.state.cx.views.label_of_window(id) else {
            return;
        };
        let Some(window) = self.state.cx.app.get_window(&label) else {
            return;
        };
        if let Err(error) = window.show_inactive() {
            eprintln!("showing quick look without activation: {error}");
        }
    }
    fn hide(&self) {
        self.state.with_window(|window| {
            if let Err(error) = window.hide() {
                eprintln!("hiding preview window: {error}");
            }
        });
    }
    fn destroy(&self) {
        self.state.with_window(|window| {
            if let Err(error) = window.destroy() {
                eprintln!("destroying preview window: {error}");
            }
        });
    }
    fn open_in_editor(&self, path: &str) {
        let path = path.to_owned();
        post_core(move |provided, _| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.reveal_shell_window();
                tabs.open_document_path(&path, tabs.report_open(&path));
            }
        });
    }
}

#[derive(Default)]
struct PickerLifecycle {
    created: Mutex<Option<(WindowId, ViewId)>>,
    created_changed: Condvar,
    parent: Option<WindowId>,
    scheduled: AtomicBool,
    ready: AtomicBool,
    closed: AtomicBool,
    owner_dead: AtomicBool,
    close_requested: AtomicBool,
}

fn close_picker_for_owner(cx: &HostContext, life: &Arc<PickerLifecycle>) {
    let created = life.created.lock().expect("picker creation");
    life.owner_dead.store(true, Ordering::SeqCst);
    life.created_changed.notify_all();
    let window = *created;
    drop(created);
    if let Some((window, _)) = window {
        if !life.closed.load(Ordering::SeqCst) && !life.close_requested.swap(true, Ordering::SeqCst)
        {
            let cx = cx.clone();
            std::thread::spawn(move || {
                let windows = cx.windows.clone();
                if let Err(error) = on_ui(&cx.ui, move || windows.close_window(window)) {
                    eprintln!("closing picker after its owner ended: {error}");
                }
            });
        }
    }
}

fn picker_ready(
    cx: HostContext,
    life: Arc<PickerLifecycle>,
    parent: Option<WindowId>,
    view: ViewId,
    control: DocPickerControl,
) {
    let created = {
        let mut created = life.created.lock().expect("picker creation");
        while created.is_none()
            && !life.closed.load(Ordering::SeqCst)
            && !life.owner_dead.load(Ordering::SeqCst)
        {
            created = life.created_changed.wait(created).expect("picker creation");
        }
        *created
    };
    let Some((window, created_view)) = created else {
        return;
    };
    if created_view != view
        || life.closed.load(Ordering::SeqCst)
        || life.owner_dead.load(Ordering::SeqCst)
    {
        return;
    }
    if let Some(parent) = parent {
        let owner = cx
            .views
            .label_of_window(parent)
            .and_then(|label| cx.app.get_window(&label));
        let child = cx
            .views
            .label_of_window(window)
            .and_then(|label| cx.app.get_window(&label));
        let (Some(child), Some(owner)) = (child, owner) else {
            close_picker_for_owner(&cx, &life);
            return;
        };
        if let Err(error) = child.begin_modal(&owner) {
            eprintln!("making document picker modal: {error}");
            close_picker_for_owner(&cx, &life);
            return;
        }
        if life.closed.load(Ordering::SeqCst)
            || life.owner_dead.load(Ordering::SeqCst)
            || cx.views.label_of_window(parent).is_none()
            || cx.views.label_of_window(window).is_none()
        {
            if let Err(error) = child.end_modal() {
                eprintln!("ending stale picker modality: {error}");
            }
            close_picker_for_owner(&cx, &life);
            return;
        }
    }
    life.ready.store(true, Ordering::SeqCst);
    control.window_ready(view);
}

struct PickerWindow {
    state: WindowState,
    control: Mutex<Option<DocPickerControl>>,
    active: Mutex<Option<Arc<PickerLifecycle>>>,
}

impl PickerWindow {
    fn new(cx: &HostContext) -> Self {
        Self {
            state: WindowState::new(cx),
            control: Mutex::new(None),
            active: Mutex::new(None),
        }
    }
}

impl DocPickerWindow for PickerWindow {
    fn attach(&self, control: DocPickerControl) {
        *self.control.lock().expect("picker control") = Some(control);
    }
    fn lang(&self) -> Lang {
        self.state.lang()
    }
    fn theme(&self) -> String {
        theme()
    }
    fn create(&self, options: &DocPickerWindowOptions) -> ViewId {
        let control = self
            .control
            .lock()
            .expect("picker control")
            .as_ref()
            .expect("picker control attached")
            .clone();
        let life = Arc::new(PickerLifecycle {
            parent: options.parent,
            ..Default::default()
        });
        *self.active.lock().expect("picker lifecycle") = Some(life.clone());
        let parent = options.parent;
        if let Some(parent) = parent {
            if let Some(owner) = self
                .state
                .cx
                .views
                .label_of_window(parent)
                .and_then(|label| self.state.cx.app.get_window(&label))
            {
                let weak = Arc::downgrade(&life);
                let cx = self.state.cx.clone();
                owner.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Destroyed) {
                        if let Some(life) = weak.upgrade() {
                            close_picker_for_owner(&cx, &life);
                        }
                    }
                });
                if self
                    .state
                    .cx
                    .views
                    .label_of_window(parent)
                    .and_then(|label| self.state.cx.app.get_window(&label))
                    .is_none()
                {
                    close_picker_for_owner(&self.state.cx, &life);
                }
            } else {
                close_picker_for_owner(&self.state.cx, &life);
            }
        }
        let closed_life = life.clone();
        let views = self.state.cx.views.clone();
        let slot = self.state.window.clone();
        let spec = WindowSpec {
            title: options.title.clone(),
            size: (options.width as f64, options.height as f64),
            min_size: Some((options.min_width as f64, options.min_height as f64)),
            visible: options.show,
            decorations: true,
            transparent: false,
            resizable: true,
            parent: None,
            focus: false,
            main: false,
            view: ViewSpec {
                url: "genoffice-app://shell/doc-picker.html".into(),
                bridge: Bridge::Surface(Surface::ShellDocPicker),
            },
        };
        let extras = WindowOptions {
            center: options.center,
            minimizable: options.minimizable,
            maximizable: options.maximizable,
            fullscreenable: options.fullscreenable,
            on_closed: Some(Box::new(move |window| {
                let created = closed_life.created.lock().expect("picker creation");
                closed_life.closed.store(true, Ordering::SeqCst);
                closed_life.created_changed.notify_all();
                drop(created);
                let view = views.views_of_window(window).into_iter().next();
                let mut current = slot.lock().expect("picker window state");
                if *current == Some(window) {
                    *current = None;
                }
                drop(current);
                if let Some(view) = view {
                    control.window_closed(view);
                }
            })),
            ..Default::default()
        };
        let windows = self.state.cx.windows.clone();
        let result = on_ui(&self.state.cx.ui, move || {
            windows.open_window_with_options(spec, extras)
        });
        let (window, view) = match result {
            Ok(created) => created,
            Err(error) => {
                let created = life.created.lock().expect("picker creation");
                life.closed.store(true, Ordering::SeqCst);
                life.created_changed.notify_all();
                drop(created);
                panic!("opening document picker: {error}");
            }
        };
        *self.state.window.lock().expect("picker window state") = Some(window);
        {
            let mut created = life.created.lock().expect("picker creation");
            *created = Some((window, view));
            life.created_changed.notify_all();
        }
        if life.owner_dead.load(Ordering::SeqCst) {
            close_picker_for_owner(&self.state.cx, &life);
        }
        view
    }
    fn renderer_ready(&self, view: ViewId) {
        let Some(life) = self.active.lock().expect("picker lifecycle").clone() else {
            return;
        };
        if life.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let cx = self.state.cx.clone();
        let control = self
            .control
            .lock()
            .expect("picker control")
            .as_ref()
            .expect("picker control attached")
            .clone();
        std::thread::spawn(move || {
            let parent = life.parent;
            picker_ready(cx, life, parent, view, control);
        });
    }
    fn show(&self) {
        let life = self.active.lock().expect("picker lifecycle").clone();
        let Some(life) = life else {
            return;
        };
        if !life.ready.load(Ordering::SeqCst)
            || life.closed.load(Ordering::SeqCst)
            || life.owner_dead.load(Ordering::SeqCst)
        {
            return;
        }
        let Some((window, _)) = *life.created.lock().expect("picker creation") else {
            return;
        };
        let windows = self.state.cx.windows.clone();
        on_ui(&self.state.cx.ui, move || {
            if life.ready.load(Ordering::SeqCst)
                && !life.closed.load(Ordering::SeqCst)
                && !life.owner_dead.load(Ordering::SeqCst)
            {
                if let Err(error) = windows.show(window, true) {
                    eprintln!("showing document picker: {error}");
                }
            }
        });
    }
    fn focus(&self) {
        let life = self.active.lock().expect("picker lifecycle").clone();
        let Some(life) = life else {
            return;
        };
        if !life.ready.load(Ordering::SeqCst)
            || life.closed.load(Ordering::SeqCst)
            || life.owner_dead.load(Ordering::SeqCst)
        {
            return;
        }
        self.state.with_window(move |window| {
            if life.ready.load(Ordering::SeqCst)
                && !life.closed.load(Ordering::SeqCst)
                && !life.owner_dead.load(Ordering::SeqCst)
            {
                if let Err(error) = window.set_focus() {
                    eprintln!("focusing picker window: {error}");
                }
            }
        });
    }
    fn close(&self) {
        self.state.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_thumbnail_lookup_prefers_exact_then_case_insensitive() {
        let mut package = ooxml_pkg::Package::default();
        package.set_file("Preview/PrvImage.PNG", b"first".to_vec());
        package.set_file("Preview/PrvImage.png", b"exact".to_vec());
        let bytes = ooxml_pkg::write::generate(&package, 0.0).unwrap();
        let parser: FileParseHost = Arc::new(super::super::w_files::ShellFileParser);
        let archive = PreviewEngines(parser).open_zip(bytes).unwrap();
        assert_eq!(
            archive.entry("Preview/PrvImage.png").unwrap(),
            Some(b"exact".to_vec())
        );
        assert_eq!(
            archive.entry("preview/prvimage.png").unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(archive.entry("missing").unwrap(), None);
        assert!(
            PreviewEngines(Arc::new(super::super::w_files::ShellFileParser))
                .open_zip(b"not a zip".to_vec())
                .is_none()
        );
    }
}
