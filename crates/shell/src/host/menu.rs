//! Batch M's hosts (the stage 3 shell contract, the batch table: host/menu.rs
//! and host/print.rs). This file inserts them all:
//!
//! - `MHostRef`: the app's locale and user, the native theme, the menus that
//!   follow the language, the spellchecker of the default session, and the
//!   printing of [`super::print`];
//! - `MStateRef`: the batch's process state (language and theme caches,
//!   context-menu claims, ignored words, print guards), one per process;
//! - `FontQueriesRef`: the installed-font queries of main-core's
//!   packages/font-metrics and the PDF editor's edit fonts ([`fonts`]).
//!
//! The application menu ([`apply_menu_for`]) and the context menus live in
//! the submodules.

mod app;
mod context;
pub(crate) mod doc_links;
mod dock;
mod fonts;

pub(crate) use app::{
    app_menu_activate, app_menu_popup, e2e_export_pdf, e2e_save_as_owned, prepare_activation_menu,
};
#[cfg(windows)]
pub(crate) use app::{run_close_accelerator, run_file_accelerator};
// what batch A and the editors' hosts call for the document links
#[allow(unused_imports)]
pub(crate) use doc_links::{
    OpenMakeWindow, attach_make_window, document_link_file_moved, document_link_paths,
    document_view_destroyed, make_from_document, note_document_saved,
};
// what D2's docs host (`DocsShell`) drives: the Docs View and Insert state
#[allow(unused_imports)]
pub(crate) use app::{
    DocsViewState, ExistingDocumentSavedMenu, RecoverySavedMenu, SavedDocumentMenu, SavedMenu,
    prepare_docs_menu_refresh_existing_document, prepare_docs_menu_refresh_many_owned,
    prepare_docs_menu_refresh_many_owned_extra, prepare_docs_menu_refresh_native,
    prepare_docs_menu_refresh_owned, prepare_docs_menu_refresh_recovery,
    prepare_docs_menu_refresh_saved_document, rebuild_docs_menu, set_docs_view_state_source,
    set_menu_item_checked, set_menu_item_enabled,
};

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use main_core::packages::electron_utils::default_save_dir::{
    PathProvider, Thrown, configured_default_save_dir,
};
use main_core::packages::electron_utils::printer_options::CommandOutcome;
use main_core::packages::electron_utils::template_fonts_dir::{
    TemplateFontsHost, template_fonts_dir,
};
use main_core::shell::tab_manager::TabKind;
use shell_contract::Hosts;
use shell_contract::ids::ViewId;
use shell_contract::runtime::{AppPaths, PathName, Thread};
use shell_ipc_m::hosts::{
    FontQueriesRef, KeyInput, MHost, MHostRef, PrintSurface, PrintWindowSpec, Printer,
};
use shell_ipc_m::state::{MState, MStateRef};
use shell_ipc_m::{MenuDict, menu_label};
use shell_ipc_w::SharedSaveDirState;
use shell_ipc_w::quick_look::QuickLookHost;
use tauri::{AppHandle, Theme};

use super::shell_tabs::shell_tabs;
use super::w::{on_core, post_core};
use super::window::Windows;
use super::{HostContext, on_ui, print};
use crate::native::{self, KeyChord, Modifiers, WebviewExt};
use crate::paths::ShellPaths;
use crate::views::ViewTable;

struct ShellM {
    app: AppHandle,
    ui: Arc<dyn Thread>,
    windows: Arc<Windows>,
    views: Arc<ViewTable>,
    cleanup_shutdown: shell_contract::runtime::CancelToken,
    headless_cleanup: Arc<super::headless_cleanup::Supervisor>,
    paths: ShellPaths,
    tray: Option<QuickLookHost>,
    /// the process's one default-save-folder state (`host::install_process_state`)
    save_dir: SharedSaveDirState,
}

/// The app the menus are set on, for callers that have no handle (the tab
/// hooks on the core thread)
static APP: OnceLock<AppHandle> = OnceLock::new();

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    let _ = APP.set(cx.app.clone());
    context::install(&cx.app, cx.views.clone(), cx.windows.clone());
    app::install(cx);
    doc_links::install(cx.windows.clone());
    let shell = Arc::new(ShellM {
        app: cx.app.clone(),
        ui: cx.ui.clone(),
        windows: cx.windows.clone(),
        views: cx.views.clone(),
        cleanup_shutdown: cx.cleanup_shutdown.clone(),
        headless_cleanup: cx.headless_cleanup.clone(),
        paths: ShellPaths::new(),
        tray: hosts.get::<QuickLookHost>().cloned(),
        save_dir: hosts
            .get::<SharedSaveDirState>()
            .cloned()
            .expect("install_process_state runs before the batches' hosts"),
    });
    let fonts = fonts::Fonts::new(
        shell
            .paths
            .path(PathName::Home)
            .to_string_lossy()
            .into_owned(),
        template_fonts_dir(&*shell),
        &shell
            .paths
            .path(PathName::UserData)
            .join("font-metrics")
            .to_string_lossy(),
    );
    hosts.insert::<MHostRef>(shell);
    hosts.insert::<MStateRef>(Arc::new(MState::new()));
    hosts.insert::<FontQueriesRef>(Arc::new(fonts));
}

/// [`apply_menu_for`] on the shell's app; nothing before the hosts are installed
pub(crate) fn apply_menu(kind: TabKind) {
    if let Some(app) = APP.get() {
        apply_menu_for(app, kind);
    }
}

/// `homeKinds.set`: the home menu and the Dock list the new-document kinds
pub(crate) fn home_kinds_changed() {
    if let Some(app) = APP.get() {
        apply_menu_for(app, TabKind::Home);
        install_dock_menu(app);
    }
}

/// `applyMenuFor(kind)`: the application menu of the active editor. Callable
/// from any thread; the menu is set on the UI thread.
pub(crate) fn apply_menu_for(app: &AppHandle, kind: TabKind) {
    app::apply_menu_for(app, kind);
    // the Dock needs the tabs, which exist once a menu is first applied
    static DOCK: std::sync::Once = std::sync::Once::new();
    DOCK.call_once(|| install_dock_menu(app));
}

/// The kinds the Dock may offer (`installDockMenu`'s `only`); they appear in home's order
const DOCK_KINDS: [(&str, &str); 5] = [
    ("docx", "menuNewDoc"),
    ("xlsx", "menuNewSheet"),
    ("pptx", "menuNewSlide"),
    ("md", "menuNewMarkdown"),
    ("pdf", "menuNewPdf"),
];

/// `installDockMenu()`: Home, then the new-document entries the person did not
/// hide, in home's order and the app's language. At start, after a language
/// change and after the home kinds change.
pub(crate) fn install_dock_menu(app: &AppHandle) {
    let app = app.clone();
    post_core(move |provided, services| {
        let Ok(lang) = shell_ipc_m::current_lang(services) else {
            return;
        };
        let Some(tabs) = shell_tabs(provided) else {
            return;
        };
        let visible = tabs.new_document_kinds();
        let mut entries = vec![dock::DockEntry {
            label: menu_label(MenuDict::Shell, lang, "menuHome"),
            action: dock::DockAction::Home,
        }];
        entries.extend(visible.iter().filter_map(|kind| {
            let (kind, key) = DOCK_KINDS.iter().find(|(k, _)| k == kind)?;
            Some(dock::DockEntry {
                label: menu_label(MenuDict::Shell, lang, key),
                action: dock::DockAction::NewDocument(kind),
            })
        }));
        dock::set_dock_menu(
            &app,
            entries,
            Arc::new(|action| {
                post_core(move |provided, _| {
                    let Some(tabs) = shell_tabs(provided) else {
                        return;
                    };
                    match action {
                        dock::DockAction::Home => {
                            tabs.reveal_shell_window();
                            tabs.open_home_tab();
                        }
                        dock::DockAction::NewDocument(kind) => {
                            tabs.reveal_shell_window();
                            tabs.new_document(kind);
                        }
                    }
                });
            }),
        );
    });
}

/// The first of the operating system's preferred languages: the locale the app runs in,
/// which is what Electron's `app.getLocale()` reported
pub(super) fn first_accept_language(_ui: &Arc<dyn Thread>) -> Option<String> {
    native::locale::first_language()
}

impl PathProvider for ShellPaths {
    fn get_path(&self, name: &str) -> Result<String, Thrown> {
        let name = match name {
            "home" => PathName::Home,
            "appData" => PathName::AppData,
            "userData" => PathName::UserData,
            "temp" => PathName::Temp,
            "exe" => PathName::Exe,
            "desktop" => PathName::Desktop,
            "documents" => PathName::Documents,
            "downloads" => PathName::Downloads,
            "logs" => PathName::Logs,
            other => return Err(Thrown(format!("Failed to get '{other}' path"))),
        };
        Ok(self.path(name).to_string_lossy().into_owned())
    }
}

impl MHost for ShellM {
    fn now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }

    fn app_locale(&self) -> String {
        first_accept_language(&self.ui).unwrap_or_else(|| "en-US".to_owned())
    }

    fn system_locale(&self) -> String {
        self.app_locale()
    }

    fn env_lang(&self) -> Option<String> {
        // the old name is still read (old-names, decision 0024)
        ["SEUOFFICE_LANG", "GENOFFICE_LANG"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    fn username(&self) -> Option<String> {
        ["USER", "USERNAME", "LOGNAME"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    fn set_native_theme(&self, theme: &str) {
        let theme = match theme {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            _ => None,
        };
        let app = self.app.clone();
        on_ui(&self.ui, move || app.set_theme(theme));
    }

    /// The window frames follow `set_native_theme` themselves; Electron only
    /// recoloured its custom title-bar overlay, which the Tauri windows do not use
    fn refresh_title_bar_overlay(&self) {}

    fn rebuild_menus_for_language(&self) {
        // the language switcher lives on the home page, so the home menu is the active one;
        // the editors' Back to Home items are built with each editor menu, in its language
        apply_menu_for(&self.app, TabKind::Home);
        install_dock_menu(&self.app);
    }

    fn sync_tray_menu(&self) {
        if let Some(tray) = &self.tray {
            tray.refresh_tray();
        }
    }

    fn reveal_home(&self) -> bool {
        on_core(|provided| match shell_tabs(provided) {
            Some(tabs) => {
                tabs.reveal_shell_window();
                tabs.open_home_tab();
                true
            }
            None => false,
        })
        .unwrap_or(false)
    }

    fn is_packaged(&self) -> bool {
        !cfg!(debug_assertions)
    }

    fn app_path(&self) -> String {
        // `app.getAppPath()` of an unpackaged run: the shell's package folder,
        // from which `templateFontsDir` climbs to packages/ui
        std::env::var_os("SEUOFFICE_APPS_DIR")
            .map_or_else(
                || Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps")).to_path_buf(),
                std::path::PathBuf::from,
            )
            .join("shell")
            .to_string_lossy()
            .into_owned()
    }

    /// `process.resourcesPath`: on Windows one level below the exe folder
    fn resources_path(&self) -> String {
        crate::paths::resources_path(&self.app)
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn headless(&self) -> bool {
        crate::headless::is_headless()
    }

    fn open_generated_path(&self, path: &str) -> bool {
        crate::host::d2::docs_tabs::open_generated(path).unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_generated_output(
        &self,
        output: shell_contract::ipc::SavedPublication,
        input: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        let mut result = None;
        let on_core = crate::host::w::on_core_local(|provided| {
            result = Some(
                match provided
                    .all::<shell_ipc_w::tabs::TabsState>()
                    .into_iter()
                    .next()
                {
                    Some(tabs) => tabs
                        .prepare_generated_output(
                            output,
                            input,
                            "pdf",
                            participants,
                            cancel,
                            pending,
                            additional,
                            effects,
                            accepted,
                            wake,
                        )
                        .map(|prepared| {
                            Box::new(prepared)
                                as Box<dyn shell_contract::hosts::PreparedGeneratedPath>
                        }),
                    None => Err("The tab manager is unavailable".into()),
                },
            );
        });
        if !on_core {
            return Err("The exported PDF must be prepared on the core thread".into());
        }
        result.ok_or_else(|| "The tab manager did not prepare the exported PDF".to_owned())?
    }

    fn default_save_dir(&self) -> String {
        let mut state = self
            .save_dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        configured_default_save_dir(&self.paths, &mut state, self.now_ms()).unwrap_or_else(|_| {
            self.paths
                .path(PathName::Documents)
                .to_string_lossy()
                .into_owned()
        })
    }

    fn spell_add_word(&self, view: ViewId, word: &str) -> bool {
        if self.windows.webview(view).is_none() {
            return false;
        }
        let word = word.to_owned();
        match on_ui(&self.ui, move || native::menu::learn_word(&word)) {
            Ok(added) => added,
            Err(error) => {
                eprintln!("shell: the spellchecker did not learn the word: {error}");
                false
            }
        }
    }

    /// The system's user dictionary, shared by every app (`NSSpellChecker`, Windows
    /// `ISpellChecker2`)
    fn spell_remove_word(&self, word: &str) {
        let word = word.to_owned();
        match on_ui(&self.ui, move || native::menu::unlearn_word(&word)) {
            Ok(true) => {}
            Ok(false) => eprintln!("shell: the spellchecker keeps the word: no dictionary to edit"),
            Err(error) => eprintln!("shell: the spellchecker did not drop the word: {error}"),
        }
    }

    /// The system checks spelling in the languages it picks itself, on both systems:
    /// Electron listed none on macOS, and WebView2 has no call to list or choose them (its
    /// language is the environment's, fixed when wry creates it). No languages listed hides
    /// the UI's language submenu, so `spell_set_languages` is never reached from it.
    fn spell_languages(&self, _view: ViewId) -> Vec<String> {
        Vec::new()
    }

    fn spell_available_languages(&self, _view: ViewId) -> Vec<String> {
        Vec::new()
    }

    fn spell_set_languages(&self, _view: ViewId, languages: &[String]) {
        eprintln!("shell: the spellchecker languages {languages:?} are chosen by the system");
    }

    fn replace_misspelling(&self, view: ViewId, word: &str) {
        let Some(webview) = self.windows.webview(view) else {
            return;
        };
        let Some((misspelled, x, y)) = context::last_misspelling(view) else {
            return eprintln!("shell: there is no misspelled word to replace");
        };
        native::menu::replace_misspelling(&webview, &misspelled, word, x, y);
    }

    /// A key press of the page command is whole (down, character, up), so the first stage
    /// of the old three-event sequence sends it and the others have nothing left to send
    fn send_space(&self, view: ViewId, input: KeyInput) {
        if !matches!(input, KeyInput::KeyDown) {
            return;
        }
        let Some(webview) = self.windows.webview(view) else {
            return;
        };
        let chord = KeyChord {
            key: " ".to_owned(),
            code: "Space".to_owned(),
            modifiers: Modifiers::default(),
        };
        match webview.page() {
            Ok(page) => page.key(
                chord,
                Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("shell: the Space key was not delivered: {error}");
                    }
                }),
            ),
            Err(error) => eprintln!("shell: the Space key was not delivered: {error}"),
        }
    }

    fn printers(&self, _view: ViewId) -> Vec<Printer> {
        print::printers()
    }

    fn print_window_native_invocation(
        &self,
        spec: &PrintWindowSpec,
        document: &shell_contract::ipc::NativeInvocationDocument,
        sender: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn PrintSurface>, String> {
        let cx = HostContext {
            app: self.app.clone(),
            ui: self.ui.clone(),
            views: self.views.clone(),
            windows: self.windows.clone(),
            cleanup_shutdown: self.cleanup_shutdown.clone(),
            headless_cleanup: self.headless_cleanup.clone(),
        };
        print::window_native_invocation(
            &cx,
            spec.clone(),
            document.target(),
            sender,
            cancel,
            pending,
        )
    }

    fn print_window_existing_document(
        &self,
        spec: &PrintWindowSpec,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Box<dyn PrintSurface>, String> {
        let cx = HostContext {
            app: self.app.clone(),
            ui: self.ui.clone(),
            views: self.views.clone(),
            windows: self.windows.clone(),
            cleanup_shutdown: self.cleanup_shutdown.clone(),
            headless_cleanup: self.headless_cleanup.clone(),
        };
        print::window_existing_document(
            &cx,
            spec.clone(),
            binding,
            participants,
            cancel,
            pending,
            additional,
        )
    }

    fn print_window(&self, spec: &PrintWindowSpec) -> Box<dyn PrintSurface> {
        print::window(
            self.app.clone(),
            self.ui.clone(),
            self.views.clone(),
            spec.clone(),
        )
    }

    fn print_window_owned(
        &self,
        spec: &PrintWindowSpec,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn PrintSurface>, String> {
        let cx = HostContext {
            app: self.app.clone(),
            ui: self.ui.clone(),
            views: self.views.clone(),
            windows: self.windows.clone(),
            cleanup_shutdown: self.cleanup_shutdown.clone(),
            headless_cleanup: self.headless_cleanup.clone(),
        };
        print::window_owned(&cx, spec.clone(), origin, destination, cancel, pending)
    }

    fn run_command(&self, file: &str, args: &[String], stdin: Option<&[u8]>) -> CommandOutcome {
        print::run_command(file, args, stdin)
    }
}

/// `templateFontsDir({ isPackaged, getAppPath, resourcesPath })` of the docs module
impl TemplateFontsHost for ShellM {
    fn is_packaged(&self) -> bool {
        MHost::is_packaged(self)
    }

    fn get_app_path(&self) -> String {
        MHost::app_path(self)
    }

    fn resources_path(&self) -> String {
        MHost::resources_path(self)
    }
}
