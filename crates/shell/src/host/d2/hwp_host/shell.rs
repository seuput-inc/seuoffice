use std::collections::HashMap;
use std::sync::Mutex;

use main_core::packages::pdf_structure::files::system_now_ms;
use main_core::shell::tab_manager::TabKind;
use shell_contract::runtime::Views;
use shell_contract::{Surface, ViewId, WindowId};
use shell_ipc_d2::hwp::{HwpEditors, HwpShell};
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::projects::Projects;
use shell_ipc_w::tabs::TabsState;
use tauri::Manager;

use crate::host::w::{on_core, post_core};
use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::host::{HostContext, on_ui};
use crate::native::{EditCommand, KeyChord, Modifiers, NativeEventKind, WebviewExt};

pub(super) struct Shell {
    cx: HostContext,
    headless: Mutex<HashMap<ViewId, WindowId>>,
}

impl Shell {
    pub(super) fn new(cx: HostContext) -> Self {
        Self {
            cx,
            headless: Mutex::new(HashMap::new()),
        }
    }

    fn edit(&self, view: ViewId, cut: bool) {
        let Some(webview) = self.cx.windows.webview(view) else {
            return;
        };
        let command = if cut {
            EditCommand::Cut
        } else {
            EditCommand::Copy
        };
        crate::native::menu::edit(&webview, command);
    }
}

impl HwpShell for Shell {
    fn ui_lang(&self) -> String {
        crate::host::w_files::ShellLanguage(self.cx.ui.clone())
            .lang()
            .as_str()
            .to_owned()
    }
    fn is_headless_mode(&self) -> bool {
        crate::headless::is_headless()
    }
    fn focused_window(&self) -> Option<WindowId> {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            cx.app
                .windows()
                .into_values()
                .find(|window| window.is_focused().unwrap_or(false))
                .and_then(|window| cx.views.window_of_window_label(window.label()))
        })
    }
    fn random_uuid(&self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("operating system random source failed");
        bytes[6] = (bytes[6] & 15) | 0x40;
        bytes[8] = (bytes[8] & 63) | 0x80;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15]
        )
    }
    fn now_ms(&self) -> f64 {
        system_now_ms()
    }
    fn is_path_open_elsewhere(&self, path: &str) -> bool {
        let path = path.to_owned();
        on_core(move |provided| {
            provided
                .all::<TabsState>()
                .first()
                .is_some_and(|tabs| tabs.is_path_open_elsewhere(&path))
        })
        .unwrap_or(false)
    }
    fn import_pdf(&self) {
        on_core(|provided| {
            if let Some(view) = provided
                .all::<TabsState>()
                .first()
                .and_then(|tabs| tabs.active_editor(TabKind::Hwp))
                && let Some(conversions) =
                    shell_contract::app_actions::document_conversions(provided)
            {
                conversions.import_pdf_into_hwp(view);
            }
        });
    }
    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        on_core(move |provided| {
            let projects = provided.all::<Projects>().into_iter().next()?;
            let placer = projects.new_documents();
            let result = placer.borrow_mut().placement_for(i64::from(view.0));
            match result {
                Ok(placement) => placement.map(|placement| placement.dir),
                Err(error) => {
                    eprintln!("HWP project placement: {}", error.message);
                    None
                }
            }
        })
        .flatten()
    }
    fn file_saved(&self, view: ViewId, path: &str) {
        let path = path.to_owned();
        on_core(move |provided| {
            if let Some(tabs) = provided.all::<TabsState>().first() {
                tabs.file_saved(view, &path);
            }
        });
    }
    fn file_saved_owned(
        &self,
        view: ViewId,
        path: String,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        if destination.view != view {
            return done(Err("HWP save destination changed".into()));
        }
        let mut callback = Some(done);
        let entered = crate::host::w::on_core_local(|_| {
            if let Some(done) = callback.take() {
                crate::host::d1::shells::file_saved_owned(
                    origin,
                    destination,
                    cancel,
                    pending,
                    effects,
                    path.into(),
                    false,
                    Box::new(move |result| {
                        done(result.and_then(|saved| {
                            let shell_contract::hosts::SavedFile { path, session } = saved;
                            path.into_os_string()
                                .into_string()
                                .map(|path| shell_contract::hosts::SavedFile { path, session })
                                .map_err(|_| "HWP saved path is not UTF-8".into())
                        }))
                    }),
                );
            }
        });
        if !entered && let Some(done) = callback {
            done(Err(
                "HWP saved acknowledgement requires the core thread".into()
            ));
        }
    }
    fn file_saved_existing_document(
        &self,
        view: ViewId,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        if selection.binding().owner().view != view {
            return done(Err("HWP save destination changed".into()));
        }
        let mut callback = Some(done);
        let entered = crate::host::w::on_core_local(|_| {
            if let Some(done) = callback.take() {
                crate::host::d2::native_save::file_saved_existing_document(
                    selection,
                    participants,
                    cancel,
                    pending,
                    additional,
                    effects,
                    false,
                    Box::new(move |result| {
                        done(result.and_then(|saved| {
                            let shell_contract::hosts::SavedFile { path, session } = saved;
                            path.into_os_string()
                                .into_string()
                                .map(|path| shell_contract::hosts::SavedFile { path, session })
                                .map_err(|_| "HWP saved path is not UTF-8".into())
                        }))
                    }),
                );
            }
        });
        if !entered && let Some(done) = callback {
            done(Err(
                "HWP saved acknowledgement requires the core thread".into()
            ));
        }
    }
    fn open_file_dialog(&self) -> bool {
        on_core(|provided| {
            if let Some(tabs) = provided.all::<TabsState>().first() {
                tabs.open_file_via_dialog();
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
    }
    fn go_home(&self) -> bool {
        on_core(|provided| {
            if let Some(tabs) = provided.all::<TabsState>().first() {
                tabs.open_home_tab();
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
    }
    fn new_document(&self) -> bool {
        on_core(|provided| {
            provided
                .all::<TabsState>()
                .first()
                .is_some_and(|tabs| tabs.new_document("hwp"))
        })
        .unwrap_or(false)
    }
    fn reload(&self, view: ViewId) {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            cx.windows.reload(view);
        });
    }
    fn editor_takes(&self, view: ViewId, key: &str) -> bool {
        self.cx
            .windows
            .webview(view)
            .is_some_and(|webview| super::keyboard::editor_takes(&webview, key))
    }
    fn copy(&self, view: ViewId) {
        self.edit(view, false);
    }
    fn cut(&self, view: ViewId) {
        self.edit(view, true);
    }
    fn send_key(&self, view: ViewId, key_code: &str, modifiers: &[&str]) {
        let Some(webview) = self.cx.windows.webview(view) else {
            return;
        };
        let Some(chord) = resend_chord(key_code, modifiers) else {
            return;
        };
        match webview.page() {
            Ok(page) => page.key(
                chord,
                Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("shell: the HWP key was not resent: {error}");
                    }
                }),
            ),
            Err(error) => eprintln!("shell: the HWP key was not resent: {error}"),
        }
    }
    fn create_headless_view(&self) -> Result<ViewId, String> {
        let cx = self.cx.clone();
        let (window, view) = on_ui(&self.cx.ui, move || {
            cx.windows.open_deferred_window(
                WindowSpec {
                    title: String::new(),
                    size: (1280.0, 900.0),
                    min_size: None,
                    visible: false,
                    decorations: true,
                    transparent: false,
                    resizable: true,
                    parent: None,
                    focus: false,
                    main: false,
                    view: ViewSpec {
                        url: "genoffice-app://hwp/index.html".into(),
                        bridge: Bridge::Surface(Surface::Hwp),
                    },
                },
                WindowOptions {
                    on_frame: Some(std::sync::Arc::new(|view, event| {
                        let kind = match &event.kind {
                            NativeEventKind::WebviewProcessFailed { .. } => 1,
                            NativeEventKind::NavigationStarted { url, .. }
                                if url.as_str() != "about:blank" =>
                            {
                                2
                            }
                            _ => return,
                        };
                        post_core(move |provided, _| {
                            if let Some(editors) = provided.all::<HwpEditors>().first() {
                                if kind == 1 {
                                    editors.render_process_gone(view, "renderer terminated");
                                } else {
                                    editors.did_start_navigation(view, true, false);
                                }
                            }
                        });
                    })),
                    ..Default::default()
                },
            )
        })?;
        self.headless
            .lock()
            .map_err(|_| "HWP headless state unavailable")?
            .insert(view, window);
        Ok(view)
    }
    fn create_native_headless_view(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
    ) -> Result<shell_contract::ipc::NativeInvocationTarget, String> {
        let cx = self.cx.clone();
        let operation = operation.clone();
        let (window, target) = on_ui(&self.cx.ui, move || {
            let identity = std::sync::Arc::new(std::sync::OnceLock::<
                shell_contract::ipc::NativeInvocationTarget,
            >::new());
            let frames = identity.clone();
            let first_origin = std::sync::Arc::new(std::sync::OnceLock::new());
            let frame_origin = first_origin.clone();
            let views = cx.views.clone();
            let options = WindowOptions {
                background_throttling: Some(
                    tauri::utils::config::BackgroundThrottlingPolicy::Disabled,
                ),
                on_frame: Some(std::sync::Arc::new(move |view, event| {
                    let Some(target) = frames.get().filter(|target| target.view() == view) else {
                        return;
                    };
                    match &event.kind {
                        NativeEventKind::DocumentRegistered { context } => {
                            let Some(captured) = views
                                .capture_native_owner(view, target.created())
                                .filter(|captured| captured.origin == *context)
                            else {
                                return;
                            };
                            let Some(owner) = crate::ipc::native_owner::owner_with_browser(
                                view,
                                captured.origin,
                                captured.renderer,
                                (),
                                captured.browser_owner,
                            ) else {
                                return;
                            };
                            if let Err(error) =
                                crate::ipc::native_owner::register_native_invocation_target(
                                    target, owner,
                                )
                            {
                                post_core(move |provided, _| {
                                    if let Some(editors) = provided.all::<HwpEditors>().first() {
                                        editors.render_process_gone(view, &error);
                                    }
                                });
                            } else {
                                let _ = frame_origin.set(context.identity().clone());
                            }
                        }
                        NativeEventKind::DocumentRevoked { document, .. }
                            if frame_origin.get() == Some(document) =>
                        {
                            post_core(move |provided, _| {
                                if let Some(editors) = provided.all::<HwpEditors>().first() {
                                    editors.render_process_gone(
                                        view,
                                        "hidden renderer context was revoked",
                                    );
                                }
                            });
                        }
                        NativeEventKind::WebviewProcessFailed { .. }
                        | NativeEventKind::NavigationFailed { .. } => {
                            post_core(move |provided, _| {
                                if let Some(editors) = provided.all::<HwpEditors>().first() {
                                    editors.render_process_gone(
                                        view,
                                        "hidden renderer stopped or failed to load",
                                    );
                                }
                            });
                        }
                        _ => {}
                    }
                })),
                ..Default::default()
            };
            let windows = cx.windows.clone();
            let spec = WindowSpec {
                title: String::new(),
                size: (1280.0, 900.0),
                min_size: None,
                visible: false,
                decorations: true,
                transparent: false,
                resizable: true,
                parent: None,
                focus: false,
                main: false,
                view: ViewSpec {
                    url: "genoffice-app://hwp/index.html".into(),
                    bridge: Bridge::Surface(Surface::Hwp),
                },
            };
            let (window, view) = crate::ipc::native_owner::run_native_invocation_ui(
                cx.ui.as_ref(),
                &operation,
                move || windows.open_deferred_window(spec, options),
            )??;
            let target = cx
                .views
                .renderer_generation(view)
                .ok_or_else(|| "HWP hidden view generation is unavailable".to_owned())
                .and_then(|generation| operation.reserve_target(view, generation));
            match target {
                Ok(target) => {
                    let _ = identity.set(target.clone());
                    Ok((window, target))
                }
                Err(error) => {
                    let _ = cx.windows.close_window(window);
                    Err(error)
                }
            }
        })?;
        self.headless
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(target.view(), window);
        Ok(target)
    }
    fn load_native_headless_view(
        &self,
        target: &shell_contract::ipc::NativeInvocationTarget,
    ) -> Result<(), String> {
        let cx = self.cx.clone();
        let target = target.clone();
        on_ui(&self.cx.ui, move || {
            let view = target.view();
            let windows = cx.windows.clone();
            crate::ipc::native_owner::run_native_invocation_created_ui(
                cx.ui.as_ref(),
                &target,
                move || windows.load_view(view, "genoffice-app://hwp/index.html"),
            )?
        })
    }
    fn load_headless_view(&self, view: ViewId) -> Result<(), String> {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            cx.windows.load_view(view, "genoffice-app://hwp/index.html")
        })
    }
    fn destroy_headless_view(&self, view: ViewId) {
        let Some(window) = self
            .headless
            .lock()
            .ok()
            .and_then(|mut windows| windows.remove(&view))
        else {
            return;
        };
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let _ = cx.windows.close_window(window);
        });
    }
}

/// The key the studio gets again when it did not take a shortcut itself: the clipboard keys
/// and Ctrl+E, with the modifiers the shortcut had (`control`, `shift`, `alt`, `meta`)
fn resend_chord(key_code: &str, modifiers: &[&str]) -> Option<KeyChord> {
    let letter = key_code
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)?
        .to_ascii_uppercase();
    if !matches!(letter, 'C' | 'X' | 'E') {
        return None;
    }
    let mut held = Modifiers::default();
    for modifier in modifiers {
        match *modifier {
            "control" => held.ctrl = true,
            "shift" => held.shift = true,
            "alt" => held.alt = true,
            "meta" => held.meta = true,
            _ => {}
        }
    }
    let key = if held.shift {
        letter
    } else {
        letter.to_ascii_lowercase()
    };
    Some(KeyChord {
        key: key.to_string(),
        code: format!("Key{letter}"),
        modifiers: held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_keys_and_ctrl_e_are_resent_with_their_modifiers() {
        let chord = resend_chord("c", &["control"]).unwrap();
        assert_eq!((chord.key.as_str(), chord.code.as_str()), ("c", "KeyC"));
        assert_eq!(
            chord.modifiers,
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        );
        let chord = resend_chord("E", &["meta", "shift"]).unwrap();
        assert_eq!((chord.key.as_str(), chord.code.as_str()), ("E", "KeyE"));
        assert_eq!(
            chord.modifiers,
            Modifiers {
                meta: true,
                shift: true,
                ..Modifiers::default()
            }
        );
    }

    #[test]
    fn other_keys_are_not_resent() {
        assert_eq!(resend_chord("v", &["control"]), None);
        assert_eq!(resend_chord("1", &["control"]), None);
        assert_eq!(resend_chord("", &[]), None);
    }
}
