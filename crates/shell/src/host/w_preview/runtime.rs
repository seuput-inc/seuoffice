#[cfg(windows)]
use std::sync::Arc;
use std::sync::Mutex;

#[cfg(windows)]
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::quick_look::{QuickLookControl, QuickLookRuntime, QuickLookSettings};

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum HelperEvent<'a> {
    Space(&'a str),
    Navigate(&'a str),
    Escape,
}

#[cfg(any(windows, test))]
fn helper_event(message: &serde_json::Value) -> Option<HelperEvent<'_>> {
    let path = || {
        message
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
    };
    match message.get("type").and_then(serde_json::Value::as_str) {
        Some("space") => path().map(HelperEvent::Space),
        Some("nav") => path().map(HelperEvent::Navigate),
        Some("escape") => Some(HelperEvent::Escape),
        _ => None,
    }
}

use super::HostContext;

pub(super) struct Runtime {
    #[cfg(windows)]
    cx: HostContext,
    #[cfg(windows)]
    state: Arc<Mutex<State>>,
    #[cfg(not(windows))]
    control: Mutex<Option<QuickLookControl>>,
}

#[cfg(windows)]
#[derive(Default)]
struct State {
    control: Option<QuickLookControl>,
    enabled: bool,
    keep_in_tray: bool,
    generation: u64,
    child: Option<std::process::Child>,
}

impl Runtime {
    pub(super) fn new(cx: &HostContext) -> Self {
        #[cfg(windows)]
        {
            Self {
                cx: cx.clone(),
                state: Arc::new(Mutex::new(State::default())),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = cx;
            Self {
                control: Mutex::new(None),
            }
        }
    }
}

impl QuickLookRuntime for Runtime {
    fn attach(&self, control: QuickLookControl) {
        #[cfg(windows)]
        {
            self.state.lock().expect("quick look runtime").control = Some(control);
        }
        #[cfg(not(windows))]
        {
            *self.control.lock().expect("quick look runtime") = Some(control);
        }
    }

    fn windows_explorer(&self) -> bool {
        cfg!(windows)
    }

    fn sync(&self, settings: &QuickLookSettings) {
        #[cfg(windows)]
        {
            self.set_enabled(settings.explorer_space_preview);
            self.set_tray(settings.explorer_space_preview && settings.keep_in_tray);
        }
        #[cfg(not(windows))]
        let _ = settings;
    }

    fn refresh_tray(&self) {
        #[cfg(windows)]
        self.refresh_tray_menu();
    }

    fn keeps_in_tray(&self) -> bool {
        #[cfg(windows)]
        {
            self.state.lock().expect("quick look runtime").keep_in_tray
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn stop(&self) {
        #[cfg(windows)]
        {
            self.set_enabled(false);
            self.remove_tray();
            {
                let mut state = self.state.lock().expect("quick look runtime");
                state.control = None;
                state.keep_in_tray = false;
            }
        }
        #[cfg(not(windows))]
        {
            *self.control.lock().expect("quick look runtime") = None;
        }
    }
}

#[cfg(windows)]
impl Runtime {
    fn set_enabled(&self, enabled: bool) {
        use std::io::{BufRead, BufReader};
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut state = self.state.lock().expect("quick look runtime");
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        state.generation = state.generation.wrapping_add(1);
        if !enabled {
            let child = state.child.take();
            drop(state);
            if let Some(mut child) = child {
                drop(child.stdin.take());
                if let Err(error) = child.kill() {
                    eprintln!("stopping quick look helper: {error}");
                }
                let _ = child.wait();
            }
            return;
        }
        let path = crate::paths::resources_path(&self.cx.app)
            .map(|root| root.join("quicklook").join("win-quicklook.exe"));
        let Some(path) = path.filter(|path| path.is_file()) else {
            eprintln!("quick look helper is not installed");
            return;
        };
        let mut child = match Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("starting quick look helper: {error}");
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            eprintln!("quick look helper has no stdout");
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let generation = state.generation;
        state.child = Some(child);
        let weak = Arc::downgrade(&self.state);
        drop(state);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let control = {
                    let state = state.lock().expect("quick look runtime");
                    if !state.enabled || state.generation != generation {
                        break;
                    }
                    state.control.clone()
                };
                let Some(control) = control else {
                    continue;
                };
                match helper_event(&message) {
                    Some(HelperEvent::Space(path)) => control.explorer_space(path),
                    Some(HelperEvent::Navigate(path)) => control.explorer_navigate(path),
                    Some(HelperEvent::Escape) => control.explorer_escape(),
                    None => {}
                }
            }
            if let Some(state) = weak.upgrade() {
                let child = {
                    let mut state = state.lock().expect("quick look runtime");
                    if state.generation == generation {
                        state.child.take()
                    } else {
                        None
                    }
                };
                if let Some(mut child) = child {
                    let _ = child.wait();
                }
            }
        });
    }

    fn set_tray(&self, visible: bool) {
        let app = self.cx.app.clone();
        let lang = super::ShellLanguage(self.cx.ui.clone()).lang();
        let control = {
            let mut state = self.state.lock().expect("quick look runtime");
            state.keep_in_tray = visible;
            state.control.clone()
        };
        super::on_ui(&self.cx.ui, move || {
            if visible {
                if app.tray_by_id("quick-look").is_none() {
                    if let Err(error) = create_tray(&app, lang, control) {
                        eprintln!("creating quick look tray: {error}");
                    }
                } else if let Some(tray) = app.tray_by_id("quick-look") {
                    if let Err(error) = tray.set_visible(true) {
                        eprintln!("showing quick look tray: {error}");
                    }
                    if let Err(error) = update_menu(&app, &tray, lang) {
                        eprintln!("updating quick look tray: {error}");
                    }
                }
            } else if let Some(tray) = app.tray_by_id("quick-look") {
                if let Err(error) = tray.set_visible(false) {
                    eprintln!("hiding quick look tray: {error}");
                }
            }
        });
    }

    fn refresh_tray_menu(&self) {
        let app = self.cx.app.clone();
        let lang = super::ShellLanguage(self.cx.ui.clone()).lang();
        super::on_ui(&self.cx.ui, move || {
            if let Some(tray) = app.tray_by_id("quick-look") {
                if let Err(error) = update_menu(&app, &tray, lang) {
                    eprintln!("updating quick look tray: {error}");
                }
            }
        });
    }

    fn remove_tray(&self) {
        let app = self.cx.app.clone();
        super::on_ui(&self.cx.ui, move || {
            drop(app.remove_tray_by_id("quick-look"));
        });
    }
}

#[cfg(windows)]
fn menu(
    app: &tauri::AppHandle,
    lang: main_core::packages::i18n::Lang,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use main_core::packages::i18n::Lang;
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let (open, quit) = match lang {
        Lang::En => ("Open SeuOffice", "Quit SeuOffice"),
        Lang::Ko => ("SeuOffice 열기", "SeuOffice 종료"),
    };
    let open = MenuItem::with_id(app, "quick-look-open", open, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quick-look-quit", quit, true, None::<&str>)?;
    Menu::with_items(app, &[&open, &separator, &quit])
}

#[cfg(windows)]
fn tooltip(lang: main_core::packages::i18n::Lang) -> String {
    use main_core::packages::i18n::{Lang, product_name};
    let title = match lang {
        Lang::En => "Quick Look",
        Lang::Ko => "빠른 미리보기",
    };
    format!("{} — {title}", product_name(Some(lang.as_str())))
}

#[cfg(windows)]
fn update_menu(
    app: &tauri::AppHandle,
    tray: &tauri::tray::TrayIcon,
    lang: main_core::packages::i18n::Lang,
) -> tauri::Result<()> {
    tray.set_menu(Some(menu(app, lang)?))?;
    tray.set_tooltip(Some(tooltip(lang)))
}

#[cfg(windows)]
fn create_tray(
    app: &tauri::AppHandle,
    lang: main_core::packages::i18n::Lang,
    control: Option<shell_ipc_w::quick_look::QuickLookControl>,
) -> Result<(), String> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../apps/shell/src/main/assets/menu-home.png"
    ));
    let icon = image::load_from_memory(bytes)
        .map_err(|error| error.to_string())?
        .to_rgba8();
    let (width, height) = icon.dimensions();
    let icon = tauri::image::Image::new_owned(icon.into_raw(), width, height);
    let menu = menu(app, lang).map_err(|error| error.to_string())?;
    TrayIconBuilder::with_id("quick-look")
        .icon(icon)
        .tooltip(tooltip(lang))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "quick-look-open" => super::post_core(|provided, _| {
                if let Some(tabs) = provided
                    .all::<shell_ipc_w::tabs::TabsState>()
                    .into_iter()
                    .next()
                {
                    tabs.reveal_shell_window();
                }
            }),
            // `stopQuickLookRuntime(); app.quit()`: the quit goes through every editor's
            // unsaved-changes guard like any other quit
            "quick-look-quit" => {
                if let Some(control) = control.clone() {
                    super::post_core(move |_, _| control.stop());
                }
                crate::host::quit::request_quit(app);
            }
            _ => {}
        })
        .on_tray_icon_event(|_, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                super::post_core(|provided, _| {
                    if let Some(tabs) = provided
                        .all::<shell_ipc_w::tabs::TabsState>()
                        .into_iter()
                        .next()
                    {
                        tabs.reveal_shell_window();
                    }
                });
            }
        })
        .build(app)
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn helper_events_require_real_nonempty_paths() {
        assert_eq!(
            helper_event(&json!({"type":"space","path":"C:\\\\a.docx"})),
            Some(HelperEvent::Space("C:\\\\a.docx"))
        );
        assert_eq!(
            helper_event(&json!({"type":"nav","path":"C:\\\\b.xlsx"})),
            Some(HelperEvent::Navigate("C:\\\\b.xlsx"))
        );
        assert_eq!(
            helper_event(&json!({"type":"escape"})),
            Some(HelperEvent::Escape)
        );
        assert_eq!(helper_event(&json!({"type":"space","path":""})), None);
        assert_eq!(helper_event(&json!({"type":"ready"})), None);
    }
}
