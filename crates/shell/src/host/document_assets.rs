//! Document-scoped asset protocol for the HTML and Markdown editors.
//!
//! URL and MIME rules follow apps/html/src/main/html-main.ts and
//! apps/markdown/src/main/markdown-main.ts. Unlike their process-global maps,
//! each request here must name the live view whose document grants it.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use jscompat::node_path::host::{dirname, extname, relative, resolve};
use main_core::docs::lazy_media::MediaAnswer;
use main_core::html::asset_mime::{
    ASSET_SNIFF_BYTES, PREVIEW_ASSET_EXTS, extensionless_asset_mime,
};
use shell_contract::ViewId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AssetKind {
    Html,
    Markdown,
    Docs,
}

#[derive(Clone)]
struct Grant {
    paths: Vec<String>,
    vault: Option<String>,
    generation: u64,
}

#[derive(Default)]
struct Registry {
    grants: HashMap<(ViewId, AssetKind), Grant>,
    present: HashMap<ViewId, (ViewId, u64)>,
    next: u64,
}

struct RegistryLock {
    inner: Mutex<Registry>,
    epoch: AtomicU64,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

impl Default for RegistryLock {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Registry::default()),
            epoch: AtomicU64::new(0),
            wait_lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }
}

struct RegistryGuard<'a> {
    owner: &'a RegistryLock,
    guard: Option<MutexGuard<'a, Registry>>,
}
impl std::ops::Deref for RegistryGuard<'_> {
    type Target = Registry;
    fn deref(&self) -> &Registry {
        self.guard.as_deref().expect("registry guard")
    }
}
impl std::ops::DerefMut for RegistryGuard<'_> {
    fn deref_mut(&mut self) -> &mut Registry {
        self.guard.as_deref_mut().expect("registry guard")
    }
}
impl Drop for RegistryGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let _guard = self
            .owner
            .wait_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .owner
            .epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            });
        self.owner.wake.notify_all();
    }
}
impl RegistryLock {
    fn lock(&self) -> std::sync::LockResult<RegistryGuard<'_>> {
        self.inner
            .lock()
            .map(|guard| RegistryGuard {
                owner: self,
                guard: Some(guard),
            })
            .map_err(|error| {
                std::sync::PoisonError::new(RegistryGuard {
                    owner: self,
                    guard: Some(error.into_inner()),
                })
            })
    }
    fn try_lock(&self) -> std::sync::TryLockResult<RegistryGuard<'_>> {
        match self.inner.try_lock() {
            Ok(guard) => Ok(RegistryGuard {
                owner: self,
                guard: Some(guard),
            }),
            Err(std::sync::TryLockError::WouldBlock) => Err(std::sync::TryLockError::WouldBlock),
            Err(std::sync::TryLockError::Poisoned(error)) => Err(
                std::sync::TryLockError::Poisoned(std::sync::PoisonError::new(RegistryGuard {
                    owner: self,
                    guard: Some(error.into_inner()),
                })),
            ),
        }
    }
    fn wait_since(
        &'static self,
        epoch: u64,
        tokens: Vec<shell_contract::runtime::CancelToken>,
    ) -> Result<(), String> {
        std::thread::scope(|threads| {
            let (signal, received) = std::sync::mpsc::channel::<()>();
            let notifier = threads.spawn(move || {
                if received.recv().is_ok() {
                    let _guard = self
                        .wait_lock
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    self.wake.notify_all();
                }
            });
            let subscriptions: Vec<_> = tokens
                .iter()
                .map(|token| {
                    let signal = signal.clone();
                    token.on_cancel(move || {
                        let _ = signal.send(());
                    })
                })
                .collect();
            let result = (|| {
                let mut guard = self
                    .wait_lock
                    .lock()
                    .map_err(|_| "document asset wake lock is poisoned")?;
                loop {
                    if tokens.iter().any(|token| token.is_cancelled()) {
                        return Err("the recovery asset rename was cancelled".into());
                    }
                    let current = self.epoch.load(Ordering::SeqCst);
                    if current != epoch {
                        return Ok(());
                    }
                    if current == u64::MAX {
                        return Err("document asset wake generation exhausted".into());
                    }
                    guard = self
                        .wake
                        .wait(guard)
                        .map_err(|_| "document asset wake lock is poisoned")?;
                }
            })();
            drop(subscriptions);
            drop(signal);
            let _ = notifier.join();
            result
        })
    }
}

static REGISTRY: OnceLock<RegistryLock> = OnceLock::new();
type VaultResolver = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;
static VAULT_RESOLVER: OnceLock<VaultResolver> = OnceLock::new();
// bit 0 = watcher ready, bits 1..16 = in-flight settings writes, upper
// bits = revision. A single atomic snapshot prevents mixed-state permits.
const VAULT_CHANGE_UNIT: u64 = 2;
const VAULT_CHANGE_MASK: u64 = 0x1fffe;
const VAULT_REVISION_UNIT: u64 = 0x20000;
static VAULT_REVISION: AtomicU64 = AtomicU64::new(VAULT_REVISION_UNIT);
fn registry() -> &'static RegistryLock {
    REGISTRY.get_or_init(RegistryLock::default)
}

pub(crate) fn set_vault_resolver(resolver: VaultResolver) -> Result<(), String> {
    VAULT_RESOLVER
        .set(resolver)
        .map_err(|_| "Markdown asset vault resolver is already installed".to_owned())
}

fn current_vault() -> Result<Option<String>, String> {
    let resolver = VAULT_RESOLVER
        .get()
        .ok_or("Markdown asset vault resolver is unavailable")?;
    resolver().map(|path| path.map(|path| resolve(&[&path])))
}

const VAULT_POISON: u64 = u64::MAX;
static VAULT_ADMISSION: Mutex<()> = Mutex::new(());

fn vault_enabled(stamp: u64) -> bool {
    stamp != VAULT_POISON && stamp & 1 != 0 && stamp & VAULT_CHANGE_MASK == 0
}

fn update_vault(
    state: &AtomicU64,
    update: impl Fn(u64) -> Option<u64>,
    error: &str,
) -> Result<(), String> {
    let _admission = VAULT_ADMISSION
        .lock()
        .map_err(|_| "Markdown vault admission is poisoned")?;
    let previous = state
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            Some(if value == VAULT_POISON {
                VAULT_POISON
            } else {
                update(value).unwrap_or(VAULT_POISON)
            })
        })
        .expect("vault transition always supplies a value");
    if previous == VAULT_POISON || update(previous).is_none_or(|value| value == VAULT_POISON) {
        Err(error.to_owned())
    } else {
        Ok(())
    }
}

/// Invalidate previously captured permits when a watched settings file changes.
pub(crate) fn invalidate_vault() -> Result<(), String> {
    invalidate_vault_state(&VAULT_REVISION)
}

fn invalidate_vault_state(state: &AtomicU64) -> Result<(), String> {
    update_vault(
        state,
        |n| n.checked_add(VAULT_REVISION_UNIT),
        "Markdown vault generation exhausted",
    )
}

/// A failed or stopped watcher immediately removes cross-folder vault access.
pub(crate) fn set_vault_watch_available(available: bool) -> Result<(), String> {
    set_vault_watch_state(&VAULT_REVISION, available)
}

fn set_vault_watch_state(state: &AtomicU64, available: bool) -> Result<(), String> {
    update_vault(
        state,
        |n| {
            n.checked_add(VAULT_REVISION_UNIT)
                .map(|next| if available { next | 1 } else { next & !1 })
        },
        "Markdown vault generation exhausted",
    )
}

/// Bracket local settings writes; concurrent writes keep the vault disabled
/// until the last one ends. A failed watcher is never silently re-enabled.
pub(crate) fn begin_vault_settings_change() -> Result<(), String> {
    begin_vault_change_state(&VAULT_REVISION)
}

fn begin_vault_change_state(state: &AtomicU64) -> Result<(), String> {
    update_vault(
        state,
        |n| {
            if n & VAULT_CHANGE_MASK == VAULT_CHANGE_MASK {
                return None;
            }
            n.checked_add(VAULT_REVISION_UNIT)?
                .checked_add(VAULT_CHANGE_UNIT)
        },
        "Markdown vault change counter exhausted",
    )
}

pub(crate) fn end_vault_settings_change() -> Result<(), String> {
    end_vault_change_state(&VAULT_REVISION)
}

fn end_vault_change_state(state: &AtomicU64) -> Result<(), String> {
    update_vault(
        state,
        |n| {
            if n & VAULT_CHANGE_MASK == 0 {
                return None;
            }
            n.checked_add(VAULT_REVISION_UNIT)
                .map(|next| next - VAULT_CHANGE_UNIT)
        },
        "No matching Markdown vault settings change",
    )
}

/// Replace the complete grant for one live view after open/save/path changes.
pub(crate) fn set_paths(
    view: ViewId,
    kind: AssetKind,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<(), String> {
    if kind == AssetKind::Docs {
        return Err("Docs assets require an exact session".into());
    }
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    state.next = state
        .next
        .checked_add(1)
        .ok_or("document asset generation exhausted")?;
    let generation = state.next;
    let mut paths = Vec::new();
    for path in [open, save]
        .into_iter()
        .flatten()
        .filter(|path| !path.is_empty())
    {
        let path = resolve(&[path]);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    state.grants.insert(
        (view, kind),
        Grant {
            paths,
            vault: None,
            generation,
        },
    );
    Ok(())
}

/// Prepare a recovery grant before admission. The returned one-shot must be
/// invoked only inside `with_recovery_seed_effect` for this exact target,
/// alongside the corresponding TextDocs state transition.
pub(crate) fn prepare_recovery_paths(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    kind: AssetKind,
    tracked: bool,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<Box<dyn FnMut() -> Result<(), String> + Send>, String> {
    let surface = match kind {
        AssetKind::Html => shell_contract::Surface::Html,
        AssetKind::Markdown => shell_contract::Surface::Markdown,
        AssetKind::Docs => return Err("Docs assets require an exact session".into()),
    };
    if let shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation) = target.context()
        && operation.selection().surface() != surface
    {
        return Err("The recovery target has a different asset surface".into());
    }
    let mut paths = Vec::new();
    for path in [open, save]
        .into_iter()
        .flatten()
        .filter(|path| !path.is_empty())
    {
        let path = resolve(&[path]);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let target = target.clone();
    let mut staged = Some(paths);
    Ok(Box::new(move || {
        let paths = staged
            .take()
            .ok_or("recovery asset paths were already published")?;
        let mut state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        if tracked {
            state.next = state
                .next
                .checked_add(1)
                .ok_or("document asset generation exhausted")?;
            let generation = state.next;
            state.grants.insert(
                (target.view(), kind),
                Grant {
                    paths,
                    vault: None,
                    generation,
                },
            );
        } else {
            state.grants.remove(&(target.view(), kind));
        }
        Ok(())
    }))
}

/// Prepare a post-seed asset replacement. The returned operation is committed
/// in the same recovery admission as the matching TextDocs state change.
pub(crate) fn prepare_rename_recovery_paths(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    kind: AssetKind,
    tracked: bool,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<shell_ipc_d2::html::RecoveryAssetStage, String> {
    if !receipt.matches(receipt.target()) || receiver.view != receipt.target().view() {
        return Err("the recovery asset receiver does not match its receipt".into());
    }
    prepare_existing_asset_paths(destination, kind, tracked, open, save, None)
}

pub(crate) fn prepare_native_save_paths(
    destination: &shell_contract::ipc::NativeRequestOwner,
    kind: AssetKind,
    tracked: bool,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<shell_ipc_d2::html::RecoveryAssetStage, String> {
    prepare_existing_asset_paths(destination, kind, tracked, open, save, None)
}

pub(crate) fn prepare_saved_document_paths(
    destination: &shell_contract::ipc::NativeRequestOwner,
    kind: AssetKind,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<shell_ipc_d2::html::SavedAssetStage, String> {
    let generation = Arc::new(AtomicU64::new(0));
    let stage = prepare_existing_asset_paths(
        destination,
        kind,
        true,
        open,
        save,
        Some(generation.clone()),
    )?;
    let view = destination.view;
    let revoke = Box::new(move || {
        let generation = generation.load(Ordering::SeqCst);
        if generation == 0 {
            return Err("the saved document asset grant was not published".into());
        }
        revoke_session(AssetSession {
            view,
            kind,
            generation,
        })
    });
    Ok(shell_ipc_d2::html::SavedAssetStage { stage, revoke })
}

fn prepare_existing_asset_paths(
    destination: &shell_contract::ipc::NativeRequestOwner,
    kind: AssetKind,
    tracked: bool,
    open: Option<&str>,
    save: Option<&str>,
    published: Option<Arc<AtomicU64>>,
) -> Result<shell_ipc_d2::html::RecoveryAssetStage, String> {
    let surface = match kind {
        AssetKind::Html => shell_contract::Surface::Html,
        AssetKind::Markdown => shell_contract::Surface::Markdown,
        AssetKind::Docs => return Err("Docs assets require an exact session".into()),
    };
    destination
        .context
        .downcast_ref::<crate::views::NativeAssetAuthorization>()
        .filter(|asset| asset.view == destination.view && asset.surface == surface)
        .ok_or("the recovery asset destination is not authenticated")?;
    let mut paths = Vec::new();
    for path in [open, save]
        .into_iter()
        .flatten()
        .filter(|path| !path.is_empty())
    {
        let path = resolve(&[path]);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let view = destination.view;
    let expected = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?
        .grants
        .get(&(view, kind))
        .map(|grant| grant.generation);
    let mut staged = Some(paths);
    let commit = Box::new(move || {
        let mut state = match registry().try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("document asset registry is poisoned".into());
            }
        };
        if state
            .grants
            .get(&(view, kind))
            .map(|grant| grant.generation)
            != expected
        {
            return Err("the document asset grant changed during recovery rename".into());
        }
        let paths = staged
            .take()
            .ok_or("recovery rename asset paths were already published")?;
        if tracked {
            state.next = state
                .next
                .checked_add(1)
                .ok_or("document asset generation exhausted")?;
            let generation = state.next;
            state.grants.insert(
                (view, kind),
                Grant {
                    paths,
                    vault: None,
                    generation,
                },
            );
            if let Some(published) = &published {
                published.store(generation, Ordering::SeqCst);
            }
        } else {
            state.grants.remove(&(view, kind));
        }
        Ok(true)
    });
    let subscribe = Box::new(|| {
        let epoch = registry().epoch.load(Ordering::SeqCst);
        Box::new(move |tokens| registry().wait_since(epoch, tokens))
            as shell_ipc_d2::html::RecoveryAssetWaiter
    });
    Ok(shell_ipc_d2::html::RecoveryAssetStage { commit, subscribe })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn set_paths_many_owned(
    origin: &shell_contract::ipc::NativeRequestOwner,
    captured: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    additional: &[shell_contract::runtime::CancelToken],
    effects: &shell_contract::OwnedEffectsHost,
    kind: AssetKind,
    tracked: bool,
    open: Option<&str>,
    save: Option<&str>,
) -> Result<(), String> {
    let destination = captured
        .first()
        .ok_or("the asset destination was not captured")?;
    let surface = match kind {
        AssetKind::Html => shell_contract::Surface::Html,
        AssetKind::Markdown => shell_contract::Surface::Markdown,
        AssetKind::Docs => return Err("Docs assets require an exact session".into()),
    };
    let asset = destination
        .context
        .downcast_ref::<crate::views::NativeAssetAuthorization>()
        .filter(|asset| asset.view == destination.view && asset.surface == surface)
        .ok_or("the asset destination is not authenticated")?;
    let _ = asset;
    let mut paths = Vec::new();
    for path in [open, save]
        .into_iter()
        .flatten()
        .filter(|path| !path.is_empty())
    {
        let path = resolve(&[path]);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let mut staged = Some(paths);
    let extra = additional.iter().collect::<Vec<_>>();
    effects.with_owned_effect_many_extra(origin, captured, cancel, pending, &extra, &mut || {
        let paths = staged.take().ok_or("asset paths were already published")?;
        let mut state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        if tracked {
            state.next = state
                .next
                .checked_add(1)
                .ok_or("document asset generation exhausted")?;
            let generation = state.next;
            state.grants.insert(
                (destination.view, kind),
                Grant {
                    paths,
                    vault: None,
                    generation,
                },
            );
        } else {
            state.grants.remove(&(destination.view, kind));
        }
        Ok(())
    })
}

/// Exact lifetime token for a pathless native document view.
pub(crate) struct AssetSession {
    view: ViewId,
    kind: AssetKind,
    generation: u64,
}

impl AssetSession {
    pub(crate) fn view(&self) -> ViewId {
        self.view
    }
    pub(crate) fn kind(&self) -> AssetKind {
        self.kind
    }
}

pub(crate) fn register_session(view: ViewId, kind: AssetKind) -> Result<AssetSession, String> {
    if kind != AssetKind::Docs {
        return Err("pathless sessions are reserved for Docs".into());
    }
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    state.next = state
        .next
        .checked_add(1)
        .ok_or("document asset generation exhausted")?;
    let generation = state.next;
    state.grants.insert(
        (view, kind),
        Grant {
            paths: Vec::new(),
            vault: None,
            generation,
        },
    );
    Ok(AssetSession {
        view,
        kind,
        generation,
    })
}

pub(crate) fn revoke_session(session: AssetSession) -> Result<(), String> {
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    if state
        .grants
        .get(&(session.view, session.kind))
        .is_some_and(|grant| grant.generation == session.generation)
    {
        state.grants.remove(&(session.view, session.kind));
    }
    Ok(())
}

pub(crate) fn revoke_kind(view: ViewId, kind: AssetKind) -> Result<(), String> {
    if kind == AssetKind::Docs {
        return Err("Docs assets require exact-session revocation".into());
    }
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    state.grants.remove(&(view, kind));
    Ok(())
}

/// Delegate an HTML presentation view to its owning editor, or revoke it.
pub(crate) fn set_present_owner(view: ViewId, owner: Option<ViewId>) -> Result<(), String> {
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    match owner {
        Some(owner) if owner != view => {
            state.next = state
                .next
                .checked_add(1)
                .ok_or("document asset generation exhausted")?;
            let generation = state.next;
            state.present.insert(view, (owner, generation));
        }
        Some(_) => return Err("a present view cannot own itself".into()),
        None => {
            state.present.remove(&view);
        }
    }
    Ok(())
}

/// Destroying a view revokes its HTML and Markdown grants and every presentation alias
/// to or from it, including outstanding reads. Docs sessions are revoked by exact session.
pub(crate) fn revoke(view: ViewId) -> Result<(), String> {
    let mut state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    state
        .grants
        .retain(|(owner, kind), _| *owner != view || *kind == AssetKind::Docs);
    state
        .present
        .retain(|present, (owner, _)| *present != view && *owner != view);
    Ok(())
}

fn status(code: u16) -> MediaAnswer {
    MediaAnswer {
        status: code,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn image_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".svg" => "image/svg+xml",
        ".bmp" => "image/bmp",
        ".avif" => "image/avif",
        _ => return None,
    })
}

fn known_mime(ext: &str) -> &'static str {
    image_mime(ext).unwrap_or(match ext {
        ".ico" => "image/x-icon",
        ".css" => "text/css",
        ".js" | ".mjs" => "text/javascript",
        ".woff" => "font/woff",
        ".woff2" => "font/woff2",
        ".ttf" => "font/ttf",
        ".otf" => "font/otf",
        ".mp4" => "video/mp4",
        ".webm" => "video/webm",
        ".mp3" => "audio/mpeg",
        ".ogg" => "audio/ogg",
        ".wav" => "audio/wav",
        _ => "application/octet-stream",
    })
}

fn asset_scheme(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Html | AssetKind::Docs => "html-asset",
        AssetKind::Markdown => "md-asset",
    }
}

fn target_asset_url(kind: AssetKind, url: &tauri::Url, token: &str) -> bool {
    if url.scheme() == "html-asset" && url.host_str() == Some(token) {
        return true;
    }
    let wire = match kind {
        AssetKind::Html | AssetKind::Docs => "html-asset.localhost",
        AssetKind::Markdown => "md-asset.localhost",
    };
    url.scheme() == asset_scheme(kind) || (url.scheme() == "http" && url.host_str() == Some(wire))
}

fn original_asset_path(kind: AssetKind, input: &str) -> Result<String, String> {
    let url = tauri::Url::parse(input).map_err(|error| error.to_string())?;
    let scheme = asset_scheme(kind);
    let host = match kind {
        AssetKind::Html | AssetKind::Docs => "html-asset.localhost",
        AssetKind::Markdown => "md-asset.localhost",
    };
    let allowed =
        url.scheme() == scheme || (url.scheme() == "http" && url.host_str() == Some(host));
    if !allowed {
        return Err("invalid document asset URL".into());
    }
    let mut path = jscompat::decode_uri_component(url.path()).map_err(|error| error.to_string())?;
    if path.as_bytes().contains(&0) {
        return Err("invalid document asset path".into());
    }
    if path.len() >= 4
        && path.as_bytes()[0] == b'/'
        && path.as_bytes()[1].is_ascii_alphabetic()
        && path.as_bytes()[2] == b':'
        && path.as_bytes()[3] == b'/'
    {
        path.remove(0);
    }
    Ok(resolve(&[&path]))
}

fn range(header: &str, len: u64) -> Option<(u64, u64)> {
    let (a, b) = header.strip_prefix("bytes=")?.split_once('-')?;
    if len == 0 {
        return None;
    }
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        return Some((len.saturating_sub(n), len - 1));
    }
    let start: u64 = a.parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    (start <= end).then_some((start, end))
}

/// Immutable grant captured after native request provenance is checked on core.
#[derive(Clone)]
pub(crate) struct AssetPermit {
    view: ViewId,
    kind: AssetKind,
    owner: ViewId,
    alias: Option<(ViewId, u64)>,
    grant: Grant,
    vault_revision: u64,
}

impl AssetPermit {
    pub(crate) fn with_current_delivery(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let _vault = VAULT_ADMISSION
            .lock()
            .map_err(|_| "Markdown vault admission is poisoned")?;
        let state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        if !self.current_in(&state) {
            return Err("document asset authority is revoked".into());
        }
        effect()
    }

    fn current_in(&self, state: &Registry) -> bool {
        if self.kind == AssetKind::Markdown
            && (self.vault_revision == VAULT_POISON
                || VAULT_REVISION.load(Ordering::SeqCst) != self.vault_revision)
        {
            return false;
        }
        let alias = if self.kind == AssetKind::Html {
            state.present.get(&self.view).copied()
        } else {
            None
        };
        alias == self.alias
            && state
                .grants
                .get(&(self.owner, self.kind))
                .is_some_and(|grant| grant.generation == self.grant.generation)
    }

    /// Revalidate the exact view generation immediately before response delivery.
    pub(crate) fn is_current(&self) -> Result<bool, String> {
        let state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        Ok(self.current_in(&state))
    }
}

pub(crate) fn authorize(view: ViewId, kind: AssetKind) -> Result<Option<AssetPermit>, String> {
    let vault_revision = VAULT_REVISION.load(Ordering::SeqCst);
    let state = registry()
        .lock()
        .map_err(|_| "document asset registry is poisoned")?;
    let alias = if kind == AssetKind::Html {
        state.present.get(&view).copied()
    } else {
        None
    };
    let owner = alias.map_or(view, |(owner, _)| owner);
    Ok(state
        .grants
        .get(&(owner, kind))
        .cloned()
        .map(|grant| AssetPermit {
            view,
            kind,
            owner,
            alias,
            grant,
            vault_revision,
        }))
}

/// Worker handler for a permit captured on core. Rechecks generation after IO.
pub(crate) fn handle_permit(
    permit: AssetPermit,
    uri: &str,
    accept: Option<&str>,
    range_header: Option<&str>,
) -> Result<MediaAnswer, String> {
    handle_permit_inner(permit, uri, accept, range_header, None)
}

fn handle_permit_inner(
    permit: AssetPermit,
    uri: &str,
    accept: Option<&str>,
    range_header: Option<&str>,
    cancel: Option<&shell_contract::runtime::CancelToken>,
) -> Result<MediaAnswer, String> {
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Err("asset request cancelled".into());
    }
    let AssetPermit {
        view,
        kind,
        owner,
        alias,
        mut grant,
        vault_revision,
    } = permit;
    if kind == AssetKind::Markdown {
        if vault_revision == VAULT_POISON || VAULT_REVISION.load(Ordering::SeqCst) != vault_revision
        {
            return Ok(status(403));
        }
        if vault_enabled(vault_revision) {
            grant.vault = current_vault()?;
        }
    }
    let target = match original_asset_path(kind, uri) {
        Ok(path) => path,
        Err(_) => return Ok(status(400)),
    };
    let ext = extname(&target).to_lowercase();
    let inside = |root: &str| {
        main_core::html::asset_lifecycle::is_in_doc_dir(
            &target,
            root,
            std::path::MAIN_SEPARATOR_STR,
        )
    };
    if !grant.paths.iter().any(|doc| inside(&dirname(doc)))
        && !grant.vault.as_deref().is_some_and(inside)
    {
        return Ok(status(403));
    }
    if (kind == AssetKind::Markdown && image_mime(&ext).is_none())
        || !std::path::Path::new(&target).exists()
    {
        return Ok(status(404));
    }
    let mut safe = None;
    for doc in &grant.paths {
        let dir = dirname(doc);
        let rel = relative(&dir, &target);
        let resolved = match kind {
            AssetKind::Html => {
                main_core::html::asset_lifecycle::resolve_safe_relative_image_path(doc, &rel)
            }
            AssetKind::Markdown => {
                main_core::markdown::asset_lifecycle::resolve_safe_relative_image_path(doc, &rel)
            }
            AssetKind::Docs => None,
        };
        if resolved.is_some() {
            safe = resolved;
            break;
        }
    }
    if safe.is_none() && kind == AssetKind::Markdown {
        safe = grant.vault.as_deref().and_then(|root| {
            main_core::markdown::asset_lifecycle::resolve_safe_vault_image_path(root, &target)
        });
    }
    let Some(path) = safe else {
        return Ok(status(403));
    };
    let mut file =
        std::fs::File::open(main_core::node_fs::namespaced(&path)).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let mime = if kind == AssetKind::Html && !PREVIEW_ASSET_EXTS.contains(&ext.as_str()) {
        let mut head = [0; ASSET_SNIFF_BYTES];
        let n = file.read(&mut head).map_err(|e| e.to_string())?;
        match extensionless_asset_mime(&target, &head[..n], accept) {
            Some(mime) => mime,
            None => return Ok(status(404)),
        }
    } else {
        known_mime(&ext)
    };
    let selected = match range_header {
        Some(header) => match range(header, len) {
            Some((start, end)) => Some((start, end)),
            None => {
                return Ok(MediaAnswer {
                    status: 416,
                    headers: vec![("Content-Range".into(), format!("bytes */{len}"))],
                    body: Vec::new(),
                });
            }
        },
        None => None,
    };
    let (start, length) = selected.map_or((0, len), |(start, end)| (start, end - start + 1));
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    let mut reader = file.take(length);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            return Err("asset request cancelled".into());
        }
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Err("asset request cancelled".into());
    }
    let live = {
        let state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        let current_alias = if kind == AssetKind::Html {
            state.present.get(&view).copied()
        } else {
            None
        };
        current_alias == alias
            && state
                .grants
                .get(&(owner, kind))
                .is_some_and(|current| current.generation == grant.generation)
    };
    if !live
        || (kind == AssetKind::Markdown
            && (vault_revision == VAULT_POISON
                || VAULT_REVISION.load(Ordering::SeqCst) != vault_revision
                || (vault_enabled(vault_revision) && current_vault()? != grant.vault)))
    {
        return Ok(status(403));
    }
    let mut headers = vec![
        ("Content-Type".into(), mime.into()),
        ("Content-Length".into(), body.len().to_string()),
        ("Accept-Ranges".into(), "bytes".into()),
    ];
    if let Some((start, end)) = selected {
        headers.push(("Content-Range".into(), format!("bytes {start}-{end}/{len}")));
    }
    Ok(MediaAnswer {
        status: if selected.is_some() { 206 } else { 200 },
        headers,
        body,
    })
}

pub(crate) fn handle_cancellable(
    view: ViewId,
    kind: AssetKind,
    uri: &str,
    accept: Option<&str>,
    range_header: Option<&str>,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<MediaAnswer, String> {
    if cancel.is_cancelled() {
        return Err("asset request cancelled".into());
    }
    match authorize(view, kind)? {
        Some(permit) => handle_permit_inner(permit, uri, accept, range_header, Some(cancel)),
        None => Ok(status(403)),
    }
}
/// A generated page is supplied by native code; an existing page must be the
/// originating editor's own current open/save path.
#[derive(Clone)]
pub(crate) enum TargetDocument {
    Generated(Vec<u8>),
    Existing(std::path::PathBuf),
}

#[derive(Clone)]
enum TargetAuthority {
    Interactive {
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    },
    NativeInvocation {
        target: shell_contract::ipc::NativeInvocationTarget,
        owner: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        actor: shell_contract::runtime::CancelToken,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    },
    ExistingDocument {
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        stop: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
    },
    OwnedMany {
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        stop: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        invocation: Option<shell_contract::ipc::NativeInvocationTarget>,
    },
}

struct Target {
    label: String,
    browser: crate::native::BrowserState,
    permit: AssetPermit,
    document: TargetDocument,
    root_path: String,
    generated: bool,
    root_navigation: Option<crate::native::NavigationIdentity>,
    claimed: bool,
    root_served: bool,
    ready: Option<crate::native::DocumentIdentity>,
    cancel: Option<shell_contract::runtime::CancelToken>,
    delivery: shell_contract::runtime::RendererGeneration,
    authority: Option<TargetAuthority>,
}

static TARGETS: OnceLock<Mutex<HashMap<String, Target>>> = OnceLock::new();
fn targets() -> &'static Mutex<HashMap<String, Target>> {
    TARGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Owns an asset-only browser capability. Dropping it revokes all its requests.
pub(crate) struct TargetGuard {
    token: String,
    root_path: String,
}

impl TargetGuard {
    /// The logical prefix of this target's URLs: the grant, tokens and the scheme handler
    /// speak it. The webview is given [`Self::document_url`], the platform shape of it.
    pub(crate) fn url_prefix(&self) -> String {
        format!("html-asset://{}", self.token)
    }
    /// The URL to navigate the hidden browser to, in the shape the platform serves and its
    /// navigation hooks and events report (`crate::native::url`).
    pub(crate) fn document_url(&self) -> String {
        crate::native::url::platform_url_text(&format!("{}{}", self.url_prefix(), self.root_path))
    }

    /// Whether an authenticated root response was prepared.
    pub(crate) fn root_served(&self) -> Result<bool, String> {
        let targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        Ok(targets
            .get(&self.token)
            .ok_or("document target is revoked")?
            .root_served)
    }

    /// Replace a native-generated page before the first root request.
    pub(crate) fn set_root_document(&self, bytes: Vec<u8>) -> Result<String, String> {
        let mut targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        let target = targets
            .get_mut(&self.token)
            .ok_or("document target is revoked")?;
        if target.claimed || !target.generated {
            return Err("document target is already in use".into());
        }
        target.document = TargetDocument::Generated(bytes);
        Ok(self.document_url())
    }

    /// Bind cancellation before navigation so requests fail before window cleanup.
    pub(crate) fn set_cancel_token(
        &self,
        cancel: shell_contract::runtime::CancelToken,
    ) -> Result<(), String> {
        let mut targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        let target = targets
            .get_mut(&self.token)
            .ok_or("document target is revoked")?;
        if target.claimed {
            return Err("document target is already in use".into());
        }
        target.cancel = Some(cancel);
        Ok(())
    }

    pub(crate) fn set_owned_authority(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<(), String> {
        crate::ipc::native_owner::with_paired_effect(origin, destination, cancel, pending, || {
            let mut targets = targets()
                .lock()
                .map_err(|_| "document target registry is poisoned")?;
            let target = targets
                .get_mut(&self.token)
                .ok_or("document target is revoked")?;
            if target.claimed
                || target.authority.is_some()
                || target.permit.view != destination.view
            {
                return Err("document target authority does not match its origin".into());
            }
            target.authority = Some(TargetAuthority::Interactive {
                origin: origin.clone(),
                destination: destination.clone(),
                cancel: cancel.clone(),
                pending: pending.clone(),
            });
            Ok(())
        })
        .ok_or_else(|| "document target authority is revoked".to_owned())?
    }

    pub(crate) fn set_native_invocation_authority_many(
        &self,
        invocation: &shell_contract::ipc::NativeInvocationTarget,
        owner: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        actor: &shell_contract::runtime::CancelToken,
    ) -> Result<(), String> {
        crate::ipc::native_owner::with_native_invocation_print_many_effect(
            invocation,
            owner,
            participants,
            cancel,
            pending,
            actor,
            actor,
            invocation.created(),
            || {
                let mut targets = targets()
                    .lock()
                    .map_err(|_| "document target registry is poisoned")?;
                let target = targets
                    .get_mut(&self.token)
                    .ok_or("document target is revoked")?;
                if target.claimed || target.authority.is_some() || target.permit.view != owner.view
                {
                    return Err(
                        "document target authority does not match its native invocation".into(),
                    );
                }
                target.authority = Some(TargetAuthority::NativeInvocation {
                    target: invocation.clone(),
                    owner: owner.clone(),
                    participants: participants.to_vec(),
                    actor: actor.clone(),
                    cancel: cancel.clone(),
                    pending: pending.clone(),
                });
                Ok(())
            },
        )
        .ok_or_else(|| "document native invocation is revoked".to_owned())?
    }

    pub(crate) fn set_existing_document_authority(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        stop: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<(), String> {
        crate::ipc::native_owner::with_existing_document_print_effect(
            binding,
            participants,
            cancel,
            pending,
            stop,
            binding.operation().authority_generation(),
            &additional.iter().collect::<Vec<_>>(),
            || {
                let mut targets = targets()
                    .lock()
                    .map_err(|_| "document target registry is poisoned")?;
                let target = targets
                    .get_mut(&self.token)
                    .ok_or("document target is revoked")?;
                if target.claimed
                    || target.authority.is_some()
                    || target.permit.view != binding.owner().view
                {
                    return Err(
                        "document target authority does not match its existing document".into(),
                    );
                }
                target.authority = Some(TargetAuthority::ExistingDocument {
                    binding: binding.clone(),
                    participants: participants.to_vec(),
                    cancel: cancel.clone(),
                    pending: pending.clone(),
                    stop: stop.clone(),
                    additional: additional.to_vec(),
                });
                Ok(())
            },
        )
        .ok_or_else(|| "document save authority is revoked".to_owned())?
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_many_extra_authority(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        stop: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        invocation: Option<&shell_contract::ipc::NativeInvocationTarget>,
    ) -> Result<(), String> {
        let generation = origin
            .context
            .renderer()
            .ok_or("document origin renderer unavailable")?;
        let bind = || {
            let mut targets = targets()
                .lock()
                .map_err(|_| "document target registry is poisoned")?;
            let target = targets
                .get_mut(&self.token)
                .ok_or("document target is revoked")?;
            if target.claimed
                || target.authority.is_some()
                || !std::iter::once(origin)
                    .chain(participants.iter())
                    .any(|owner| owner.view == target.permit.view)
            {
                return Err("document target authority does not match its origin".into());
            }
            target.authority = Some(TargetAuthority::OwnedMany {
                origin: origin.clone(),
                participants: participants.to_vec(),
                cancel: cancel.clone(),
                pending: pending.clone(),
                stop: stop.clone(),
                additional: additional.to_vec(),
                invocation: invocation.cloned(),
            });
            Ok(())
        };
        let controls = additional.iter().collect::<Vec<_>>();
        let admitted = match invocation {
            Some(invocation) => {
                crate::ipc::native_owner::with_native_invocation_print_many_extra_effect(
                    invocation,
                    origin,
                    participants,
                    cancel,
                    pending,
                    stop,
                    generation,
                    &controls,
                    bind,
                )
            }
            None => crate::ipc::native_owner::with_owned_print_many_extra_effect(
                origin,
                participants,
                cancel,
                pending,
                stop,
                generation,
                &controls,
                bind,
            ),
        };
        admitted.ok_or_else(|| "document print authority is revoked".to_owned())?
    }
}

impl Drop for TargetGuard {
    fn drop(&mut self) {
        let target = targets()
            .lock()
            .ok()
            .and_then(|mut targets| targets.remove(&self.token));
        if let Some(target) = target {
            target.delivery.revoke();
        }
    }
}

/// The caller must own a fresh blank browser, allow exactly one controlled
/// navigation, and reject every later navigation and popup before loading it.
/// This browser must not be registered for the application's IPC bridge.
pub(crate) fn grant_target(
    label: &str,
    origin: ViewId,
    kind: AssetKind,
    native: crate::native::BrowserState,
    document: TargetDocument,
) -> Result<TargetGuard, String> {
    if label.is_empty() {
        return Err("empty document target label".into());
    }
    let permit = authorize(origin, kind)?.ok_or("origin document is not live")?;
    let source_path = match &document {
        TargetDocument::Existing(path) => Some(resolve(&[&path.to_string_lossy()])),
        TargetDocument::Generated(_) => permit.grant.paths.last().cloned(),
    };
    let root_dir = match source_path.as_deref() {
        Some(path) => {
            let dir = dirname(path);
            tauri::Url::from_directory_path(&dir)
                .map_err(|_| "invalid document directory")?
                .path()
                .to_owned()
        }
        None => "/".to_owned(),
    };
    let generated = matches!(&document, TargetDocument::Generated(_));
    let document = match document {
        TargetDocument::Existing(path) => {
            let normalized = resolve(&[&path.to_string_lossy()]);
            if !permit.grant.paths.contains(&normalized) {
                return Err("conversion page is not the origin document".into());
            }
            TargetDocument::Generated(std::fs::read(path).map_err(|error| error.to_string())?)
        }
        generated => generated,
    };
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let root_path = format!("{root_dir}__seuoffice-{token}.html");
    let mut targets = targets()
        .lock()
        .map_err(|_| "document target registry is poisoned")?;
    if targets
        .values()
        .any(|target| target.label == label || target.browser.is_same_browser(&native))
        || targets.contains_key(&token)
    {
        return Err("document target already exists".into());
    }
    targets.insert(
        token.clone(),
        Target {
            label: label.to_owned(),
            browser: native,
            permit,
            document,
            root_path: root_path.clone(),
            generated,
            root_navigation: None,
            claimed: false,
            root_served: false,
            ready: None,
            cancel: None,
            delivery: shell_contract::runtime::RendererGeneration::new(),
            authority: None,
        },
    );
    Ok(TargetGuard { token, root_path })
}

/// A request authenticated against the native browser lifetime and navigation.
#[derive(Clone)]
pub(crate) struct TargetPermit {
    token: String,
    navigation: crate::native::NavigationIdentity,
    document: Option<crate::native::DocumentIdentity>,
    permit: AssetPermit,
    cancel: Option<shell_contract::runtime::CancelToken>,
    uri: String,
    root: bool,
    delivery: shell_contract::runtime::RendererGeneration,
    authority: Option<TargetAuthority>,
}

impl TargetPermit {
    /// Revalidate target, origin and native navigation before final delivery.
    pub(crate) fn with_current_delivery(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deliver = || {
            let _vault = VAULT_ADMISSION
                .lock()
                .map_err(|_| "Markdown vault admission is poisoned")?;
            let state = registry()
                .lock()
                .map_err(|_| "document asset registry is poisoned")?;
            let targets = targets()
                .lock()
                .map_err(|_| "document target registry is poisoned")?;
            if !self.current_in(&state, &targets) {
                return Err("document target is revoked".into());
            }
            effect()
        };
        match &self.authority {
            Some(TargetAuthority::Interactive {
                origin,
                destination,
                cancel,
                pending,
            }) => crate::ipc::native_owner::with_print_pair_effect(
                origin,
                destination,
                &self.delivery,
                self.cancel.as_ref().unwrap_or(cancel),
                cancel,
                pending,
                deliver,
            )
            .ok_or_else(|| "document target authority is revoked".to_owned())?,
            Some(TargetAuthority::NativeInvocation {
                target,
                owner,
                participants,
                cancel,
                pending,
                actor,
            }) => crate::ipc::native_owner::with_native_invocation_print_many_effect(
                target,
                owner,
                participants,
                cancel,
                pending,
                actor,
                self.cancel.as_ref().unwrap_or(actor),
                &self.delivery,
                deliver,
            )
            .ok_or_else(|| "document native invocation is revoked".to_owned())?,
            Some(TargetAuthority::ExistingDocument {
                binding,
                participants,
                cancel,
                pending,
                stop,
                additional,
            }) => crate::ipc::native_owner::with_existing_document_print_effect(
                binding,
                participants,
                cancel,
                pending,
                stop,
                &self.delivery,
                &additional.iter().collect::<Vec<_>>(),
                deliver,
            )
            .ok_or_else(|| "document save authority is revoked".to_owned())?,
            Some(TargetAuthority::OwnedMany {
                origin,
                participants,
                cancel,
                pending,
                stop,
                additional,
                invocation,
            }) => {
                let controls = additional.iter().collect::<Vec<_>>();
                let result = match invocation {
                    Some(invocation) => {
                        crate::ipc::native_owner::with_native_invocation_print_many_extra_effect(
                            invocation,
                            origin,
                            participants,
                            cancel,
                            pending,
                            stop,
                            &self.delivery,
                            &controls,
                            deliver,
                        )
                    }
                    None => crate::ipc::native_owner::with_owned_print_many_extra_effect(
                        origin,
                        participants,
                        cancel,
                        pending,
                        stop,
                        &self.delivery,
                        &controls,
                        deliver,
                    ),
                };
                result.ok_or_else(|| "document print authority is revoked".to_owned())?
            }
            None => self
                .delivery
                .with_active(|| {
                    shell_contract::runtime::CancelToken::with_active_all(
                        [self.cancel.as_ref()],
                        deliver,
                    )
                })
                .flatten()
                .ok_or_else(|| "document target is revoked".to_owned())?,
        }
    }

    fn current_in(&self, state: &Registry, targets: &HashMap<String, Target>) -> bool {
        self.permit.current_in(state)
            && targets.get(&self.token).is_some_and(|target| {
                target.browser.navigation().as_ref() == Some(&self.navigation)
                    && target.root_navigation.as_ref() == Some(&self.navigation)
                    && self
                        .document
                        .as_ref()
                        .is_none_or(|document| target.browser.document().as_ref() == Some(document))
            })
    }

    pub(crate) fn is_current(&self) -> Result<bool, String> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
        {
            return Ok(false);
        }
        let state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        let targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        Ok(self.current_in(&state, &targets))
    }
}

/// Classify by the native browser lifetime, never by label or request headers.
/// A request from no registered browser is an error so browserless requests fail closed.
pub(crate) fn is_target_browser(
    context: &crate::native::SchemeRequestContext,
) -> Result<bool, String> {
    let browser = context
        .browser_state()
        .ok_or("document request has no native browser lifetime")?;
    let targets = targets()
        .lock()
        .map_err(|_| "document target registry is poisoned")?;
    Ok(targets
        .values()
        .any(|target| target.browser.is_same_browser(browser)))
}

/// Authenticate hidden requests by the exact native browser lifetime and
/// controlled root navigation. Only the initial root uses the secret URL. Such a browser
/// admits one controlled navigation, so its navigation at intake is the root binding.
pub(crate) fn authorize_target(
    uri: &str,
    context: &crate::native::SchemeRequestContext,
) -> Result<Option<TargetPermit>, String> {
    let url = match tauri::Url::parse(uri) {
        Ok(url) => crate::native::url::into_logical(url),
        Err(_) => return Ok(None),
    };
    let Some(authority) = url.host_str() else {
        return Ok(None);
    };
    let (Some(browser), Some(navigation)) = (context.browser_state(), context.navigation()) else {
        return Ok(None);
    };
    let mut targets = targets()
        .lock()
        .map_err(|_| "document target registry is poisoned")?;
    let mut matches = targets
        .iter()
        .filter(|(_, target)| target.browser.is_same_browser(browser))
        .map(|(token, _)| token.clone());
    let Some(token) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Ok(None);
    }
    let Some(target) = targets.get_mut(&token) else {
        return Ok(None);
    };
    let root = url.scheme() == "html-asset" && authority == token && url.path() == target.root_path;
    if !root && !target_asset_url(target.permit.kind, &url, &token) {
        return Ok(None);
    }
    if root {
        if target
            .root_navigation
            .as_ref()
            .is_some_and(|bound| bound != navigation)
        {
            return Ok(None);
        }
        if target.browser.navigation().as_ref() != Some(navigation) {
            return Ok(None);
        }
        target.root_navigation = Some(navigation.clone());
    } else if target.root_navigation.as_ref() != Some(navigation) {
        return Ok(None);
    }
    if target
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.is_cancelled())
        || !target.browser.is_same_browser(browser)
        || target.browser.navigation().as_ref() != Some(navigation)
    {
        return Ok(None);
    }
    let document = context.document().cloned();
    if let Some(document) = &document {
        if target.browser.document().as_ref() != Some(document) {
            return Ok(None);
        }
        if target.ready.as_ref().is_some_and(|ready| ready != document) {
            return Ok(None);
        }
        target.ready = Some(document.clone());
    } else if target.ready.is_some() {
        return Ok(None);
    }
    Ok(Some(TargetPermit {
        token,
        navigation: navigation.clone(),
        document,
        permit: target.permit.clone(),
        cancel: target.cancel.clone(),
        uri: uri.to_owned(),
        root,
        delivery: target.delivery.clone(),
        authority: target.authority.clone(),
    }))
}

/// Serve only this request's root or the originating view's scoped assets.
pub(crate) fn handle_target(
    request: TargetPermit,
    uri: &str,
    accept: Option<&str>,
    range_header: Option<&str>,
) -> Result<MediaAnswer, String> {
    let url = match tauri::Url::parse(uri) {
        Ok(url) => crate::native::url::into_logical(url),
        Err(_) => return Ok(status(400)),
    };
    if uri != request.uri {
        return Ok(status(403));
    }
    let root = request.root;
    let document = {
        let mut targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        let Some(target) = targets.get_mut(&request.token) else {
            return Ok(status(403));
        };
        if target
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
            || target.browser.navigation().as_ref() != Some(&request.navigation)
            || request
                .document
                .as_ref()
                .is_some_and(|document| target.browser.document().as_ref() != Some(document))
        {
            return Ok(status(403));
        }
        if root {
            target.claimed = true;
        }
        if root {
            Some((target.document.clone(), target.generated))
        } else {
            None
        }
    };
    let answer = if let Some(document) = document {
        let (document, generated) = document;
        let body = match document {
            TargetDocument::Generated(bytes) => bytes,
            TargetDocument::Existing(path) => {
                std::fs::read(path).map_err(|error| error.to_string())?
            }
        };
        let mime = if generated {
            "text/html; charset=utf-8"
        } else {
            "text/html"
        };
        MediaAnswer {
            status: 200,
            headers: vec![
                ("Content-Type".into(), mime.into()),
                ("Content-Length".into(), body.len().to_string()),
            ],
            body,
        }
    } else {
        let original = if url.host_str() == Some(request.token.as_str()) {
            format!(
                "{}://local{}",
                asset_scheme(request.permit.kind),
                url.path()
            )
        } else {
            uri.to_owned()
        };
        handle_permit_inner(
            request.permit.clone(),
            &original,
            accept,
            range_header,
            request.cancel.as_ref(),
        )?
    };
    {
        // Synchronize origin revocation and target revocation at the final check.
        let state = registry()
            .lock()
            .map_err(|_| "document asset registry is poisoned")?;
        let mut targets = targets()
            .lock()
            .map_err(|_| "document target registry is poisoned")?;
        let permit = &request.permit;
        let alias = if permit.kind == AssetKind::Html {
            state.present.get(&permit.view).copied()
        } else {
            None
        };
        let origin_live = alias == permit.alias
            && state
                .grants
                .get(&(permit.owner, permit.kind))
                .is_some_and(|grant| grant.generation == permit.grant.generation);
        let Some(target) = targets.get_mut(&request.token) else {
            return Ok(status(403));
        };
        if !origin_live
            || target
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
            || target.browser.navigation().as_ref() != Some(&request.navigation)
            || request
                .document
                .as_ref()
                .is_some_and(|document| target.browser.document().as_ref() != Some(document))
        {
            return Ok(status(403));
        }
        if root {
            target.root_served = true;
        }
    }
    Ok(answer)
}

/// Convert the Windows `http://<scheme>.localhost` media URL back to the original authority-based DOCX URL.
pub(crate) fn docx_original_uri(uri: &str) -> Option<String> {
    let url = tauri::Url::parse(uri).ok()?;
    if url.scheme() == "seuoffice-docx-media" {
        return Some(uri.to_owned());
    }
    if url.scheme() != "http" || url.host_str()? != "seuoffice-docx-media.localhost" {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let (authority, rest) = path.split_once('/')?;
    if authority.is_empty() || authority.contains('%') {
        return None;
    }
    let mut original = format!("seuoffice-docx-media://{authority}/{rest}");
    if let Some(query) = url.query() {
        original.push('?');
        original.push_str(query);
    }
    Some(original)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(
        view: ViewId,
        kind: AssetKind,
        uri: &str,
        accept: Option<&str>,
        range_header: Option<&str>,
    ) -> Result<MediaAnswer, String> {
        match authorize(view, kind)? {
            Some(permit) => handle_permit(permit, uri, accept, range_header),
            None => Ok(status(403)),
        }
    }

    #[test]
    fn registry_wait_ticket_observes_release_before_wait_and_cancellation() {
        static TEST_REGISTRY: OnceLock<RegistryLock> = OnceLock::new();
        let registry = TEST_REGISTRY.get_or_init(RegistryLock::default);
        let guard = registry
            .lock()
            .unwrap_or_else(|_| panic!("fixture registry is poisoned"));
        let ticket = registry.epoch.load(Ordering::SeqCst);
        assert!(matches!(
            registry.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        drop(guard);
        assert!(registry.wait_since(ticket, Vec::new()).is_ok());
        let ticket = registry.epoch.load(Ordering::SeqCst);
        let cancel = shell_contract::runtime::CancelToken::default();
        cancel.cancel();
        assert!(registry.wait_since(ticket, vec![cancel]).is_err());
        let ticket = registry.epoch.load(Ordering::SeqCst);
        let cancel = shell_contract::runtime::CancelToken::default();
        let waiting_cancel = cancel.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let worker = std::thread::spawn(move || {
            ready.wait();
            registry.wait_since(ticket, vec![waiting_cancel])
        });
        barrier.wait();
        cancel.cancel();
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn authored_target_urls_keep_their_document_kind_and_private_relative_namespace() {
        let allowed =
            |kind, uri| target_asset_url(kind, &tauri::Url::parse(uri).unwrap(), "private-target");
        assert!(allowed(
            AssetKind::Markdown,
            "md-asset://local/doc/image.png"
        ));
        assert!(allowed(
            AssetKind::Markdown,
            "http://md-asset.localhost/doc/image.png"
        ));
        assert!(allowed(
            AssetKind::Markdown,
            "html-asset://private-target/doc/image.png"
        ));
        assert!(!allowed(
            AssetKind::Markdown,
            "html-asset://another-target/doc/image.png"
        ));
        assert!(!allowed(
            AssetKind::Markdown,
            "html-asset://local/doc/image.png"
        ));
        assert!(!allowed(AssetKind::Markdown, "file:///doc/image.png"));
        assert!(!allowed(
            AssetKind::Markdown,
            "http://other.localhost/doc/image.png"
        ));
        assert!(!allowed(AssetKind::Html, "md-asset://local/doc/image.png"));
        assert!(allowed(AssetKind::Html, "html-asset://local/doc/image.png"));
    }

    #[test]
    fn vault_writes_invalidate_old_permits_and_preserve_watcher_failure() {
        let state = AtomicU64::new(VAULT_REVISION_UNIT);
        set_vault_watch_state(&state, true).unwrap();
        let old_permit = state.load(Ordering::SeqCst);
        assert!(vault_enabled(old_permit));
        begin_vault_change_state(&state).unwrap();
        let during_write = state.load(Ordering::SeqCst);
        assert_ne!(old_permit, during_write);
        assert!(!vault_enabled(during_write));
        begin_vault_change_state(&state).unwrap();
        set_vault_watch_state(&state, false).unwrap();
        end_vault_change_state(&state).unwrap();
        assert!(!vault_enabled(state.load(Ordering::SeqCst)));
        end_vault_change_state(&state).unwrap();
        let after_write = state.load(Ordering::SeqCst);
        assert!(!vault_enabled(after_write));
        assert_ne!(old_permit, after_write);
    }

    #[test]
    fn vault_generation_overflow_is_irreversibly_closed() {
        let state = AtomicU64::new(u64::MAX - VAULT_REVISION_UNIT + 1);
        assert!(invalidate_vault_state(&state).is_err());
        assert_eq!(state.load(Ordering::SeqCst), VAULT_POISON);
        assert!(!vault_enabled(state.load(Ordering::SeqCst)));
        assert!(set_vault_watch_state(&state, true).is_err());
        assert_eq!(state.load(Ordering::SeqCst), VAULT_POISON);
    }

    #[test]
    fn assets_follow_live_document_and_present_grants() {
        let root =
            std::env::temp_dir().join(format!("document-assets-{}", jscompat::random_uuid()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let document = a.join("page.html");
        let image = a.join("image space.png");
        let secret = b.join("image.png");
        std::fs::write(&document, "page").unwrap();
        std::fs::write(&image, b"abcdef").unwrap();
        std::fs::write(&secret, b"private").unwrap();
        let forbidden = a.join("secret.txt");
        std::fs::write(&forbidden, b"private").unwrap();
        #[cfg(unix)]
        let link = {
            let link = a.join("escape.png");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            link
        };
        let url = |path: &std::path::Path| {
            format!(
                "http://html-asset.localhost{}",
                tauri::Url::from_file_path(path).unwrap().path()
            )
        };
        let owner = ViewId(987_000_001);
        let present = ViewId(987_000_002);
        let stranger = ViewId(987_000_003);
        set_paths(owner, AssetKind::Html, document.to_str(), document.to_str()).unwrap();
        set_present_owner(present, Some(owner)).unwrap();
        assert_eq!(
            handle(stranger, AssetKind::Html, &url(&image), None, None)
                .unwrap()
                .status,
            403
        );
        assert_eq!(
            handle(owner, AssetKind::Html, &url(&secret), None, None)
                .unwrap()
                .status,
            403
        );
        assert_eq!(
            handle(
                owner,
                AssetKind::Html,
                &url(&forbidden),
                Some("text/css"),
                None
            )
            .unwrap()
            .status,
            404
        );
        #[cfg(unix)]
        assert_eq!(
            handle(owner, AssetKind::Html, &url(&link), None, None)
                .unwrap()
                .status,
            403
        );
        let result = handle(
            present,
            AssetKind::Html,
            &url(&image),
            None,
            Some("bytes=1-3"),
        )
        .unwrap();
        assert_eq!(result.status, 206);
        assert_eq!(result.body, b"bcd");
        set_present_owner(present, None).unwrap();
        assert_eq!(
            handle(present, AssetKind::Html, &url(&image), None, None)
                .unwrap()
                .status,
            403
        );
        revoke(owner).unwrap();
        assert_eq!(
            handle(owner, AssetKind::Html, &url(&image), None, None)
                .unwrap()
                .status,
            403
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_session_cannot_revoke_reused_view() {
        let view = ViewId(987_000_004);
        let old = register_session(view, AssetKind::Docs).unwrap();
        let new = register_session(view, AssetKind::Docs).unwrap();
        revoke_session(old).unwrap();
        assert!(authorize(view, AssetKind::Docs).unwrap().is_some());
        revoke_session(new).unwrap();
        assert!(authorize(view, AssetKind::Docs).unwrap().is_none());
    }

    #[test]
    fn saved_html_asset_receipt_cannot_revoke_a_newer_grant() {
        let view = ViewId(987_000_024);
        set_paths(view, AssetKind::Html, Some("/first/page.html"), None).unwrap();
        let generation = registry()
            .lock()
            .unwrap()
            .grants
            .get(&(view, AssetKind::Html))
            .unwrap()
            .generation;
        let old = AssetSession {
            view,
            kind: AssetKind::Html,
            generation,
        };
        set_paths(view, AssetKind::Html, Some("/second/page.html"), None).unwrap();
        revoke_session(old).unwrap();
        let grant = authorize(view, AssetKind::Html).unwrap().unwrap();
        assert_eq!(grant.grant.paths, vec![resolve(&["/second/page.html"])]);
        revoke_kind(view, AssetKind::Html).unwrap();
    }

    #[test]
    fn stale_asset_permit_cannot_accept_a_response_after_replacement() {
        let view = ViewId(987_000_090);
        let old = register_session(view, AssetKind::Docs).unwrap();
        let permit = authorize(view, AssetKind::Docs).unwrap().unwrap();
        let mut accepted = false;
        permit
            .with_current_delivery(&mut || {
                accepted = true;
                Ok(())
            })
            .unwrap();
        assert!(accepted);
        let new = register_session(view, AssetKind::Docs).unwrap();
        accepted = false;
        assert!(
            permit
                .with_current_delivery(&mut || {
                    accepted = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!accepted);
        revoke_session(old).unwrap();
        revoke_session(new).unwrap();
    }

    #[test]
    fn recovery_seed_grant_is_bound_one_shot_and_denied_after_cancellation() {
        use shell_contract::ipc::{
            NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
            RecoveryOpenContext, RecoveryOpenTarget,
        };
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let cancel = CancelToken::default();
        let operation = issuer.issue(
            NativeInvocationSelection::RecoveryFile {
                surface: shell_contract::Surface::Html,
                path: "recovery-example.html".into(),
                decision: NativeRecoveryDecision::Restore,
            },
            cancel.clone(),
        );
        let view = ViewId(987_000_091);
        let other = ViewId(987_000_092);
        let created = RendererGeneration::new();
        let (_closure_issuer, closure, browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::NativeStartup(operation.clone()),
            view,
            created,
            closure,
            browser,
        )
        .unwrap();
        let mut publish = prepare_recovery_paths(
            &target,
            AssetKind::Html,
            true,
            Some("recovery-example.html"),
            None,
        )
        .unwrap();
        assert!(authorize(view, AssetKind::Html).unwrap().is_none());
        assert!(
            crate::ipc::native_owner::with_recovery_seed_effect(&target, &mut publish)
                .unwrap()
                .is_ok()
        );
        assert!(authorize(view, AssetKind::Html).unwrap().is_some());
        assert!(authorize(other, AssetKind::Html).unwrap().is_none());
        assert!(
            crate::ipc::native_owner::with_recovery_seed_effect(&target, &mut publish)
                .unwrap()
                .is_err()
        );
        assert!(
            prepare_recovery_paths(&target, AssetKind::Markdown, true, Some("later.md"), None)
                .is_err()
        );
        let mut later =
            prepare_recovery_paths(&target, AssetKind::Html, false, None, None).unwrap();
        cancel.cancel();
        assert!(crate::ipc::native_owner::with_recovery_seed_effect(&target, &mut later).is_none());
        assert!(authorize(view, AssetKind::Html).unwrap().is_some());
        revoke(view).unwrap();
    }

    #[test]
    fn docx_wire_url_restores_the_original_authority() {
        assert_eq!(
            docx_original_uri("http://seuoffice-docx-media.localhost/handoff/abc"),
            Some("seuoffice-docx-media://handoff/abc".into())
        );
        assert_eq!(
            docx_original_uri("http://seuoffice-docx-media.localhost/abc/word/media/a%20b.png"),
            Some("seuoffice-docx-media://abc/word/media/a%20b.png".into())
        );
        assert_eq!(
            docx_original_uri("http://other.localhost/handoff/abc"),
            None
        );
    }
}
