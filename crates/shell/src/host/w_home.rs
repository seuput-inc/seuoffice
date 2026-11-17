mod app;
pub(crate) use app::open_help_from_menu;
mod decision;
mod index;
mod index_scheduler;
mod index_sources;
pub(crate) mod index_worker;
mod recovery_saved;
mod saved;
pub(crate) use recovery_saved::docs_file_opened_recovery;
pub(crate) use saved::{
    project_file_renamed_existing_document, project_file_renamed_owned_extra,
    project_file_renamed_recovery,
};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::w::on_core;
use crate::paths::ShellPaths;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use shell_contract::Hosts;
use shell_contract::IpcValue;
use shell_contract::runtime::{AppPaths, PathName};
use shell_ipc_w::home_files::{FolderWatch, FolderWatchHost, HomeFiles, HomeFilesHost};
use shell_ipc_w::tabs::TabsState;

use super::HostContext;

struct RootWatch {
    watcher: Option<RecommendedWatcher>,
    active: Arc<AtomicBool>,
}

impl Drop for RootWatch {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct FolderWatchers {
    roots: Mutex<HashMap<String, RootWatch>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stopping: AtomicBool,
}

impl FolderWatch for FolderWatchers {
    fn watch(&self, roots: &[String], changed: Arc<dyn Fn(Vec<String>) + Send + Sync>) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut watchers = self.roots.lock().expect("folder watcher lock poisoned");
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        watchers.retain(|root, watch| {
            watch.active.load(Ordering::Acquire) && roots.contains(root) && Path::new(root).exists()
        });
        for root in roots {
            if watchers.contains_key(root) || !Path::new(root).exists() {
                continue;
            }
            let (sender, receiver) = mpsc::channel::<notify::Result<notify::Event>>();
            let mut watcher = match notify::recommended_watcher(move |event| {
                let _ = sender.send(event);
            }) {
                Ok(watcher) => watcher,
                Err(error) => {
                    eprintln!("Cannot watch folder {root}: {error}");
                    continue;
                }
            };
            if let Err(error) = watcher.watch(Path::new(root), RecursiveMode::Recursive) {
                eprintln!("Cannot watch folder {root}: {error}");
                continue;
            }
            let root_path = PathBuf::from(root);
            let active = Arc::new(AtomicBool::new(true));
            let running = active.clone();
            let changed = changed.clone();
            let thread = std::thread::spawn(move || {
                let mut pending = Vec::new();
                let mut seen = HashSet::new();
                loop {
                    if !running.load(Ordering::Acquire) {
                        break;
                    }
                    let event = if pending.is_empty() {
                        receiver
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        receiver.recv_timeout(Duration::from_millis(250))
                    };
                    if !running.load(Ordering::Acquire) {
                        break;
                    }
                    match event {
                        Ok(Ok(event)) => {
                            let paths = if event.paths.is_empty() {
                                vec![root_path.clone()]
                            } else {
                                event.paths
                            };
                            for path in paths {
                                if let Some(parent) = path.parent() {
                                    let parent = parent.to_string_lossy().into_owned();
                                    if seen.insert(parent.clone()) {
                                        pending.push(parent);
                                    }
                                }
                                if path.is_dir() {
                                    let dir = path.to_string_lossy().into_owned();
                                    if seen.insert(dir.clone()) {
                                        pending.push(dir);
                                    }
                                }
                            }
                        }
                        Ok(Err(error)) => {
                            eprintln!("Folder watch failed for {}: {error}", root_path.display());
                            running.store(false, Ordering::Release);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            changed(std::mem::take(&mut pending));
                            seen.clear();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            let mut threads = self
                .threads
                .lock()
                .expect("folder watcher thread lock poisoned");
            threads.retain(|thread| !thread.is_finished());
            threads.push(thread);
            watchers.insert(
                root.clone(),
                RootWatch {
                    watcher: Some(watcher),
                    active,
                },
            );
        }
    }
}

/// Home's file sources (folder watchers, home files, the search index); net's
/// knowledge-base and drive tools read them at install, so they go first
pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let watcher = Arc::new(FolderWatchers::default());
    hosts.insert(watcher.clone());
    let watcher: FolderWatchHost = watcher;
    hosts.insert(watcher);
    let home: HomeFilesHost = Arc::new(ShellHome { ui: cx.ui.clone() });
    hosts.insert(home);
    index::install(hosts);
}

/// Home's application services, which share the MCP runtime installed by batch A
pub(super) fn install_app(hosts: &mut Hosts, cx: &HostContext) {
    app::install(hosts, cx);
}

pub(crate) fn shutdown(hosts: &Hosts) -> Result<(), String> {
    let watchers = hosts.get::<Arc<FolderWatchers>>();
    if let Some(watchers) = watchers {
        watchers.stopping.store(true, Ordering::Release);
    }
    index::shutdown(hosts)?;
    if let Some(watchers) = watchers {
        let roots = std::mem::take(&mut *watchers.roots.lock().map_err(|error| error.to_string())?);
        for (_, mut watch) in roots {
            watch.active.store(false, Ordering::Release);
            drop(watch.watcher.take());
        }
        let threads =
            std::mem::take(&mut *watchers.threads.lock().map_err(|error| error.to_string())?);
        let mut panicked = false;
        for thread in threads {
            panicked |= thread.join().is_err();
        }
        if panicked {
            return Err("folder watcher thread panicked".into());
        }
    }
    Ok(())
}

struct ShellHome {
    ui: Arc<dyn shell_contract::runtime::Thread>,
}

impl HomeFiles for ShellHome {
    fn now_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_millis() as f64
    }

    fn system_locale(&self) -> String {
        super::menu::first_accept_language(&self.ui).unwrap_or_else(|| "en-US".to_owned())
    }

    fn open_document_path_owned(
        &self,
        path: String,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<shell_contract::ViewId>, String>) + Send>,
    ) {
        let done = Arc::new(Mutex::new(Some(done)));
        let failure = done.clone();
        super::w::post_core_checked(
            move |provided, _| {
                let Some(done) = done.lock().ok().and_then(|mut done| done.take()) else {
                    return;
                };
                let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                    return done(Err("tab services unavailable".into()));
                };
                tabs.route_document_path_owned(path, origin, cancel, done);
            },
            move || {
                if let Some(done) = failure.lock().ok().and_then(|mut done| done.take()) {
                    done(Err("core services unavailable".into()));
                }
            },
        );
    }

    fn open_dropped_files(&self, paths: Vec<String>) {
        on_core(move |provided| {
            let tabs = provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .expect("tab manager is unavailable");
            let value = IpcValue::Array(paths.into_iter().map(IpcValue::String).collect());
            tabs.handle_dropped_files(Some(&value));
        })
        .expect("core services are unavailable");
    }

    fn open_file_paths(&self) -> Vec<String> {
        on_core(|provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .expect("tab manager is unavailable")
                .all_file_paths()
        })
        .expect("core services are unavailable")
    }

    fn project_file_paths(&self) -> Vec<String> {
        super::w_projects::store()
            .lock()
            .expect("project store lock poisoned")
            .known_file_paths()
    }

    fn slides_recent_files(&self) -> Vec<String> {
        let path = ShellPaths::new()
            .path(PathName::UserData)
            .join("slides-recent.json");
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    }

    fn document_link_paths(&self) -> Vec<String> {
        on_core(|_| super::menu::document_link_paths()).expect("core services are unavailable")
    }

    fn project_file_renamed(&self, old_path: &str, new_path: &str) {
        if let Err(error) = super::w_projects::store()
            .lock()
            .expect("project store lock poisoned")
            .file_renamed(old_path, new_path)
        {
            eprintln!("[project-store] fileRenamed failed: {error}");
        }
    }

    fn file_moved_before_template(&self, old_path: &str, new_path: &str) {
        let old = old_path.to_owned();
        let new = new_path.to_owned();
        on_core(move |_| super::menu::document_link_file_moved(&old, &new))
            .expect("core services are unavailable");
        if new_path.to_ascii_lowercase().ends_with(".pptx") {
            let path = ShellPaths::new()
                .path(PathName::UserData)
                .join("slides-recent.json");
            let old = old_path.to_owned();
            let new = new_path.to_owned();
            std::thread::spawn(move || {
                let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                    let mut recent: Vec<serde_json::Value> =
                        serde_json::from_slice(&std::fs::read(&path)?)?;
                    for value in &mut recent {
                        if value.as_str() == Some(old.as_str()) {
                            *value = serde_json::Value::String(new.clone());
                        }
                    }
                    std::fs::write(path, serde_json::to_vec(&recent)?)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("[slides] replace recent failed: {error}");
                }
            });
        }
    }

    fn file_moved(&self, old_path: &str, new_path: &str) {
        let old = old_path.to_owned();
        let new = new_path.to_owned();
        on_core(move |provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .expect("tab manager is unavailable")
                .file_moved(&old, &new)
        })
        .expect("core services are unavailable");
    }

    fn recents_changed(&self) {
        super::menu::rebuild_docs_menu();
    }
    fn prepare_recents_changed_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_owned(origin, destination, cancel, pending, effects)
    }
    fn prepare_recents_changed_many_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_many_owned(
            origin,
            destination,
            participants,
            cancel,
            pending,
            effects,
        )
    }
    fn prepare_recents_changed_many_owned_extra(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_many_owned_extra(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
        )
    }
    fn prepare_recents_changed_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_recovery(
            receipt.clone(),
            receiver.clone(),
            cancel.clone(),
            pending.clone(),
            effects.clone(),
        )
    }
    fn prepare_recents_changed_native(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_native(operation, expected, effects)
    }
    fn prepare_recents_changed_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_existing_document(
            binding,
            participants,
            cancel,
            pending,
            additional,
            effects,
        )
    }
    fn prepare_recents_changed_saved_document(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        super::menu::prepare_docs_menu_refresh_saved_document(
            document,
            participants,
            cancel,
            pending,
            additional,
            effects,
        )
    }

    fn open_recycle_bin(&self) {
        #[cfg(windows)]
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg("shell:RecycleBin")
            .spawn()
        {
            eprintln!("Could not open Recycle Bin: {error}");
        }
    }
}
