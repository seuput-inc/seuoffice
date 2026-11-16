//! Native shell services for the PDF editor.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use main_core::pdf::font_locate::Platform;
use main_core::pdf::ocr::OcrEnv;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use shell_contract::{IpcValue, ViewId};
use shell_ipc_d2::pdf::editor::hosts::{PdfShell, PdfShellHost};
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::tabs::TabsState;

use super::tabs;
use crate::host::{HostContext, w::on_core, w_files::ShellLanguage};

struct Watch {
    active: Arc<AtomicBool>,
    _watcher: RecommendedWatcher,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct Shell {
    cx: HostContext,
    next_watch: AtomicU64,
    watches: Mutex<HashMap<u64, Watch>>,
}

pub(super) fn host(cx: &HostContext) -> PdfShellHost {
    Arc::new(Shell {
        cx: cx.clone(),
        next_watch: AtomicU64::new(1),
        watches: Mutex::new(HashMap::new()),
    })
}

impl PdfShell for Shell {
    fn ui_lang(&self) -> String {
        ShellLanguage(self.cx.ui.clone()).lang().as_str().to_owned()
    }
    fn username(&self) -> Option<String> {
        ["USER", "USERNAME", "LOGNAME"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }
    fn platform(&self) -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Other
        }
    }
    fn is_path_open_elsewhere(&self, path: &str) -> bool {
        let path = path.to_owned();
        on_core(move |provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .is_some_and(|tabs| tabs.is_path_open_elsewhere(&path))
        })
        .unwrap_or(false)
    }
    fn open_generated_path(&self, path: &str) -> Result<bool, String> {
        Ok(tabs::open_result(path, false))
    }
    fn create_document_synthetic(
        &self,
        _: &IpcValue,
        _: ViewId,
    ) -> Option<Result<IpcValue, String>> {
        unreachable!("native PDF hosts do not install the synthetic creator capability")
    }
    fn create_document_owned(
        &self,
        request: IpcValue,
        owner: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        done: Box<dyn FnOnce(Result<IpcValue, String>) + Send>,
    ) {
        tabs::create_document_owned(
            request,
            owner,
            origin,
            destination,
            cancel,
            pending,
            admission,
            done,
        )
    }
    fn open_pdf_tools(&self, path: &str, tool: &str) -> Option<bool> {
        tabs::open_tools(path, tool)
    }
    fn has_note_hooks(&self) -> bool {
        true
    }
    fn open_note(&self, path: &str, done: Box<dyn FnOnce(Result<bool, String>)>) {
        tabs::note(path, None, done);
    }
    fn insert_into_note(
        &self,
        path: &str,
        text: &str,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        tabs::note(path, Some(text), done);
    }
    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        crate::host::d1::shells::new_document_dir(view)
    }
    fn redaction_saved_owned(
        &self,
        _view: ViewId,
        path: &str,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_many_owned_extra(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            path.into(),
            false,
            Box::new(move |result| {
                done(result.map(|saved| saved.map_path(|path| path.to_string_lossy().into_owned())))
            }),
        );
    }
    fn redaction_saved_existing_document(
        &self,
        _view: ViewId,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        path: &str,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_existing_document(
            revision,
            path.into(),
            participants,
            cancel,
            pending,
            additional,
            effects,
            false,
            on_revision,
            Box::new(move |result| {
                done(result.map(|saved| saved.map_path(|path| path.to_string_lossy().into_owned())))
            }),
        );
    }
    fn redaction_saved(&self, view: ViewId, path: &str) -> Result<(), String> {
        let path = path.to_owned();
        on_core(move |provided| {
            let tabs = provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .ok_or("The tab manager is not available")?;
            tabs.docs_file_saved(view, &path);
            Ok(())
        })
        .ok_or("Core services are not available")?
    }
    fn renamed(&self, view: ViewId, old_path: &str, new_path: &str) -> Result<(), String> {
        let old = old_path.to_owned();
        let new = new_path.to_owned();
        on_core(move |provided| {
            let tabs = provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .ok_or("The tab manager is not available")?;
            tabs.set_document_file(view, &new);
            let home = provided
                .all::<shell_ipc_w::home_files::HomeFiles>()
                .into_iter()
                .next()
                .ok_or("Home services are not available")?;
            home.after_file_moved(&old, &new)
                .map_err(|error| error.to_string())
        })
        .ok_or("Core services are not available")?
    }
    fn reload(&self, view: ViewId) {
        let windows = self.cx.windows.clone();
        crate::host::on_ui(&self.cx.ui, move || {
            windows.reload(view);
        });
    }
    fn watch_dir(
        &self,
        dir: &str,
        on_event: Box<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Option<u64> {
        let active = Arc::new(AtomicBool::new(true));
        let enabled = active.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if !enabled.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(event) = event {
                    if event.paths.is_empty() {
                        on_event(None);
                    } else {
                        for path in event.paths {
                            if !enabled.load(Ordering::Acquire) {
                                break;
                            }
                            on_event(
                                path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned()),
                            );
                        }
                    }
                }
            })
            .ok()?;
        watcher
            .watch(Path::new(dir), RecursiveMode::NonRecursive)
            .ok()?;
        let id = self
            .next_watch
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        self.watches.lock().ok()?.insert(
            id,
            Watch {
                active,
                _watcher: watcher,
            },
        );
        Some(id)
    }
    fn close_watch(&self, handle: u64) {
        if let Ok(mut watches) = self.watches.lock() {
            watches.remove(&handle);
        }
    }
    fn random_uuid(&self) -> String {
        jscompat::random_uuid()
    }
    fn ocr_env(&self) -> OcrEnv {
        OcrEnv {
            platform: self.platform(),
            here: concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps/pdf/src/main").to_owned(),
            resources_path: crate::paths::resources_path(&self.cx.app)
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }
}
