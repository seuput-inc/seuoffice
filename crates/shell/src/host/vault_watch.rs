//! Watches the settings directory so external replacements revoke vault permits.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use super::document_assets::{invalidate_vault, set_vault_watch_available};

pub(crate) struct VaultWatch {
    active: Arc<Mutex<bool>>,
    stop: mpsc::Sender<()>,
}

fn fail(active: &Mutex<bool>) {
    match active.lock() {
        Ok(mut active) => *active = false,
        Err(poisoned) => *poisoned.into_inner() = false,
    }
    if let Err(error) = set_vault_watch_available(false) {
        eprintln!("shell: disabling Markdown vault: {error}");
    }
}

impl Drop for VaultWatch {
    fn drop(&mut self) {
        fail(&self.active);
        let _ = self.stop.send(());
    }
}

/// Starts without filesystem IO on the caller. Until the watch is installed,
/// the vault remains unavailable. Watch errors and shutdown revoke it again.
pub(crate) fn start(settings_path: PathBuf) -> Result<VaultWatch, String> {
    set_vault_watch_available(false)?;
    let directory = settings_path
        .parent()
        .ok_or("settings path has no parent")?
        .to_path_buf();
    let active = Arc::new(Mutex::new(true));
    let (stop, receiver) = mpsc::channel();
    let worker_active = active.clone();
    std::thread::Builder::new()
        .name("seuoffice-vault-watch".into())
        .spawn(move || {
            let callback_active = worker_active.clone();
            let watched_directory = directory.clone();
            let callback = move |event: notify::Result<notify::Event>| {
                let mut active = match callback_active.lock() {
                    Ok(active) => active,
                    Err(poisoned) => {
                        *poisoned.into_inner() = false;
                        let _ = set_vault_watch_available(false);
                        return;
                    }
                };
                if !*active {
                    return;
                }
                match event {
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                    Ok(event)
                        if event.paths.iter().any(|path| path == &watched_directory)
                            && matches!(
                                event.kind,
                                notify::EventKind::Remove(_)
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ) =>
                    {
                        *active = false;
                        let _ = set_vault_watch_available(false);
                    }
                    Ok(_) => {
                        if let Err(error) = invalidate_vault() {
                            *active = false;
                            let _ = set_vault_watch_available(false);
                            eprintln!("shell: invalidating Markdown vault: {error}");
                        }
                    }
                    Err(error) => {
                        *active = false;
                        let _ = set_vault_watch_available(false);
                        eprintln!("shell: settings watch failed: {error}");
                    }
                }
            };
            let result = (|| -> notify::Result<RecommendedWatcher> {
                let mut watcher = notify::recommended_watcher(callback)?;
                watcher.watch(&directory, RecursiveMode::NonRecursive)?;
                Ok(watcher)
            })();
            let _watcher = match result {
                Ok(watcher) => watcher,
                Err(error) => {
                    fail(&worker_active);
                    eprintln!("shell: cannot watch settings directory: {error}");
                    return;
                }
            };
            {
                let active = match worker_active.lock() {
                    Ok(active) => active,
                    Err(poisoned) => {
                        drop(poisoned.into_inner());
                        fail(&worker_active);
                        return;
                    }
                };
                if !*active {
                    return;
                }
                if let Err(error) = set_vault_watch_available(true) {
                    drop(active);
                    fail(&worker_active);
                    eprintln!("shell: enabling Markdown vault: {error}");
                    return;
                }
            }
            let _ = receiver.recv();
        })
        .map_err(|error| format!("cannot start settings watcher: {error}"))?;
    Ok(VaultWatch { active, stop })
}
