//! Legacy PID observations never stand in for an owned renderer's native exit receipt.
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, Ordering}};
use shell_contract::ids::ViewId;
use tauri_runtime_cef::{FrameEvent, FrameEventKind, NativeContextToken, NativeRendererProcess, NativeRendererProcessSubscription};

type Close = Box<dyn FnOnce() + Send + 'static>;
type OwnedDone = Box<dyn FnOnce(Result<(), String>) + Send>;
type Progress = Arc<dyn Fn() + Send + Sync>;
struct OwnedWait {
    done: OwnedDone,
    progress: Progress,
    identity: Arc<()>,
    subscription: Option<NativeRendererProcessSubscription>,
}
impl OwnedWait {
    fn finish(self, result: Result<(), String>) {
        let Self { done, subscription, .. } = self;
        drop(subscription);
        done(result);
    }
}

#[derive(Clone)]
struct CapturedIdentity {
    view: ViewId,
    browser: i32,
    identity: Arc<()>,
}
#[derive(Clone)]
pub(crate) struct CapturedRenderer {
    identity: CapturedIdentity,
    origin: NativeContextToken,
    process: NativeRendererProcess,
    exited: Arc<AtomicBool>,
    retirement: Arc<()>,
}

#[derive(Default)]
struct Entry {
    browser: Option<i32>,
    process: Process,
    switched: bool,
    retired: bool,
    close: Option<Close>,
    identity: Arc<()>,
    native_origin: Option<NativeContextToken>,
    native_process: Option<NativeRendererProcess>,
    owned_retirement: Option<Arc<()>>,
    owned: Option<OwnedWait>,
}
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Process {
    #[default]
    Unknown,
    Running(u32),
    Gone,
}
#[derive(Default)]
struct Registry { entries: HashMap<ViewId, Entry> }
impl Registry {
    fn can_end_owned(&self, captured: &CapturedRenderer) -> bool {
        let Some(entry) = self.entries.get(&captured.identity.view) else { return false; };
        if !matches_owned(entry, captured) || !entry.retired { return false; }
        let process = captured.process.identity();
        no_live_peer_matches(process, self.entries.iter()
            .filter(|(view, peer)| **view != captured.identity.view && !peer.retired)
            .map(|(_, peer)| peer.native_process.as_ref().map(|peer| peer.identity())))
    }

    fn observe(&mut self, view: ViewId, browser_id: i32, is_main: bool, kind: &FrameEventKind) -> Option<Close> {
        let entry = self.entries.entry(view).or_default();
        if entry.browser.is_some_and(|browser| browser != browser_id) {
            entry.switched = true;
            entry.process = Process::Unknown;
            entry.native_origin = None;
            entry.native_process = None;
            return entry.fail_owned("The captured renderer browser was replaced");
        }
        entry.browser = Some(browser_id);
        if is_main && matches!(kind, FrameEventKind::NavigationStarted { .. }) {
            if entry.owned.is_some() { return entry.fail_owned("The captured renderer navigated before disposal"); }
            entry.native_origin = None;
            entry.native_process = None;
        }
        match kind {
            FrameEventKind::RendererProcessIdentified { pid } if is_main && *pid != 0 => {
                if matches!(entry.process, Process::Running(previous) if previous != *pid) || entry.process == Process::Gone {
                    entry.switched = true;
                }
                entry.process = Process::Running(*pid);
            }
            FrameEventKind::NavigationStarted { .. } if is_main && entry.process != Process::Unknown => {
                entry.switched = true;
                entry.process = Process::Unknown;
            }
            FrameEventKind::NativeContextRevoked { origin } if entry.native_origin.as_ref() == Some(origin) && !entry.retired => {
                entry.native_origin = None;
                entry.native_process = None;
            }
            FrameEventKind::Attached | FrameEventKind::Created if is_main && entry.process == Process::Gone => {
                entry.switched = true;
                entry.process = Process::Unknown;
            }
            FrameEventKind::RendererTerminated => {
                if !entry.switched && matches!(entry.process, Process::Running(_)) {
                    entry.process = Process::Gone;
                    if entry.retired { return entry.close.take(); }
                } else if entry.process != Process::Gone {
                    entry.process = Process::Unknown;
                }
            }
            _ => {}
        }
        None
    }
    fn release(&mut self, view: ViewId, close: Close) -> Option<Close> {
        let entry = self.entries.entry(view).or_default();
        if entry.retired { return None; }
        entry.retired = true;
        if entry.process == Process::Gone && !entry.switched { Some(close) }
        else { entry.close = Some(close); None }
    }
}

fn no_live_peer_matches<I: PartialEq>(process: I, mut peers: impl Iterator<Item = Option<I>>) -> bool {
    !peers.any(|peer| peer.is_none_or(|peer| peer == process))
}

impl Entry {
    fn fail_owned(&mut self, message: &str) -> Option<Close> {
        let waiter = self.owned.take()?;
        let message = message.to_owned();
        Some(Box::new(move || waiter.finish(Err(message))))
    }
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

pub(crate) fn release(view: ViewId, close: impl FnOnce() + Send + 'static) {
    let action = REGISTRY.lock().unwrap().release(view, Box::new(close));
    if let Some(action) = action { action(); }
}
pub(crate) fn on_frame(view: ViewId, event: &FrameEvent) {
    let registered = match &event.kind {
        FrameEventKind::NativeContextRegistered { origin } if event.is_main =>
            Some((origin.clone(), origin.renderer_process())),
        _ => None,
    };
    let (action, progress) = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
        let mut action = registry.observe(view, event.browser_id, event.is_main, &event.kind);
        if let Some((origin, process)) = registered {
            if let Some(entry) = registry.entries.get_mut(&view).filter(|entry| entry.browser == Some(event.browser_id)) {
                if entry.native_origin.as_ref().is_some_and(|previous| previous != &origin) {
                    action = entry.fail_owned("The captured native document was replaced").or(action);
                }
                entry.native_origin = Some(origin);
                entry.native_process = process;
            }
        }
        let progress = registry.entries.values().filter_map(|entry| entry.owned.as_ref().map(|waiter| waiter.progress.clone())).collect::<Vec<_>>();
        (action, progress)
    };
    if let Some(action) = action { action(); }
    for progress in progress { progress(); }
}
pub(crate) fn forget(view: ViewId) {
    let action = REGISTRY.lock().unwrap_or_else(|error| error.into_inner()).entries.remove(&view)
        .and_then(|mut entry| entry.fail_owned("The captured renderer was removed before disposal completed"));
    if let Some(action) = action { action(); }
}

pub(crate) fn can_close(view: ViewId) -> bool {
    REGISTRY.lock().unwrap().entries.get(&view)
        .is_some_and(|entry| entry.retired && !entry.switched && entry.process == Process::Gone)
}

pub(crate) fn capture(view: ViewId, origin: &NativeContextToken) -> Result<CapturedRenderer, String> {
    if !origin.is_verified() { return Err("The original native document is unavailable".into()); }
    let process = origin.renderer_process().ok_or("The original renderer has no native process receipt")?;
    let registry = REGISTRY.lock().map_err(|_| "The renderer disposal state is poisoned")?;
    let entry = registry.entries.get(&view).ok_or("The original renderer has not been observed")?;
    if entry.native_origin.as_ref() != Some(origin) || entry.retired
        || entry.native_process.as_ref().is_none_or(|current| current.identity() != process.identity()) {
        return Err("The original native renderer is not available for captured disposal".into());
    }
    Ok(CapturedRenderer {
        identity: CapturedIdentity { view, browser: origin.browser_id(), identity: entry.identity.clone() },
        origin: origin.clone(), process, exited: Arc::new(AtomicBool::new(false)), retirement: Arc::new(()),
    })
}

fn matches(entry: &Entry, captured: &CapturedIdentity) -> bool {
    entry.browser == Some(captured.browser) && Arc::ptr_eq(&entry.identity, &captured.identity)
}
fn matches_owned(entry: &Entry, captured: &CapturedRenderer) -> bool {
    matches(entry, &captured.identity) && entry.native_origin.as_ref() == Some(&captured.origin)
        && entry.native_process.as_ref().is_some_and(|current| current.identity() == captured.process.identity())
}

fn reserve_owned(entry: &mut Entry, retirement: &Arc<()>) -> Result<(), String> {
    if entry.owned.is_some() {
        return Err("The captured renderer already has a disposal waiter".into());
    }
    if entry.retired {
        if !entry.owned_retirement.as_ref().is_some_and(|original| Arc::ptr_eq(original, retirement)) {
            return Err("The captured renderer was retired by another operation".into());
        }
    } else {
        entry.retired = true;
        entry.owned_retirement = Some(retirement.clone());
    }
    Ok(())
}

pub(crate) fn release_owned(captured: &CapturedRenderer, done: OwnedDone, progress: Progress) -> Result<(), String> {
    let waiter_identity = Arc::new(());
    {
        let mut registry = REGISTRY.lock().map_err(|_| "The renderer disposal state is poisoned")?;
        let entry = registry.entries.get_mut(&captured.identity.view).ok_or("The captured renderer no longer exists")?;
        if !matches_owned(entry, captured) {
            return Err("The captured renderer was replaced".into());
        }
        reserve_owned(entry, &captured.retirement)?;
        entry.owned = Some(OwnedWait { done, progress, identity: waiter_identity.clone(), subscription: None });
    }
    let original = captured.clone();
    let callback_identity = waiter_identity.clone();
    let subscription = captured.process.when_exited(move |result| {
        let waiter = {
            let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
            registry.entries.get_mut(&original.identity.view)
                .filter(|entry| matches_owned(entry, &original))
                .filter(|entry| entry.owned.as_ref().is_some_and(|waiter| Arc::ptr_eq(&waiter.identity, &callback_identity)))
                .and_then(|entry| entry.owned.take())
        };
        if let Some(waiter) = waiter {
            if result.is_ok() { original.exited.store(true, Ordering::Release); }
            waiter.finish(result);
        }
    });
    let mut subscription = match subscription {
        Ok(subscription) => Some(subscription),
        Err(error) => {
            let waiter = REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner()).entries.get_mut(&captured.identity.view)
                .filter(|entry| matches_owned(entry, captured))
                .filter(|entry| entry.owned.as_ref().is_some_and(|waiter| Arc::ptr_eq(&waiter.identity, &waiter_identity)))
                .and_then(|entry| entry.owned.take());
            drop(waiter);
            return Err(error);
        }
    };
    {
        let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(waiter) = registry.entries.get_mut(&captured.identity.view)
            .filter(|entry| matches_owned(entry, captured)).and_then(|entry| entry.owned.as_mut())
            .filter(|waiter| Arc::ptr_eq(&waiter.identity, &waiter_identity)) {
            waiter.subscription = subscription.take();
        }
    }
    drop(subscription);
    Ok(())
}

pub(crate) fn cancel_owned(captured: &CapturedRenderer) {
    let waiter = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
        registry.entries.get_mut(&captured.identity.view).filter(|entry| matches_owned(entry, captured)).and_then(|entry| entry.owned.take())
    };
    drop(waiter);
}

pub(crate) fn can_close_owned(captured: &CapturedRenderer) -> bool {
    captured.exited.load(Ordering::Acquire) && REGISTRY.lock().is_ok_and(|registry| registry.entries.get(&captured.identity.view)
        .is_some_and(|entry| matches_owned(entry, captured) && entry.retired))
}

pub(crate) fn can_end_owned(captured: &CapturedRenderer) -> bool {
    REGISTRY.lock().is_ok_and(|registry| registry.can_end_owned(captured))
}

pub(crate) fn retired_native_documents(captured: &CapturedRenderer) -> Result<Vec<NativeContextToken>, String> {
    let registry = REGISTRY.lock().map_err(|_| "The renderer disposal state is poisoned")?;
    let entry = registry.entries.get(&captured.identity.view).ok_or("The captured renderer no longer exists")?;
    if !matches_owned(entry, captured) || !entry.retired { return Err("The captured renderer was replaced".into()); }
    Ok(registry.entries.values().filter(|entry| entry.retired
        && entry.native_process.as_ref().is_some_and(|process| process.identity() == captured.process.identity()))
        .filter_map(|entry| entry.native_origin.clone()).collect())
}

pub(crate) fn permits_native_termination(captured: &CapturedRenderer, retired: &[NativeContextToken]) -> Result<bool, String> {
    captured.process.permits_termination_for(retired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    fn event(registry: &mut Registry, view: ViewId, kind: FrameEventKind) -> Option<Close> {
        registry.observe(view, 1, true, &kind)
    }
    fn callback(count: Arc<AtomicUsize>) -> Close {
        Box::new(move || { count.fetch_add(1, Ordering::SeqCst); })
    }
    #[test]
    fn failed_wait_rearms_only_the_same_retirement_and_rejects_concurrent_waiters() {
        let mut entry = Entry::default();
        let retirement = Arc::new(());
        let other = Arc::new(());
        reserve_owned(&mut entry, &retirement).unwrap();
        let first = Arc::new(());
        entry.owned = Some(OwnedWait { done: Box::new(|_| {}), progress: Arc::new(|| {}), identity: first.clone(), subscription: None });
        assert!(reserve_owned(&mut entry, &retirement).is_err());
        entry.owned.take().unwrap().finish(Err("wait failed".into()));
        assert!(entry.retired);
        assert!(reserve_owned(&mut entry, &other).is_err());
        reserve_owned(&mut entry, &retirement).unwrap();
        let second = Arc::new(());
        entry.owned = Some(OwnedWait { done: Box::new(|_| {}), progress: Arc::new(|| {}), identity: second.clone(), subscription: None });
        assert!(!Arc::ptr_eq(&first, &entry.owned.as_ref().unwrap().identity));
        assert!(Arc::ptr_eq(&second, &entry.owned.as_ref().unwrap().identity));
    }
    #[test]
    fn close_after_real_exit_in_either_order_only_once() {
        for exit_first in [false, true] {
            let mut registry = Registry::default();
            let view = ViewId(1);
            let calls = Arc::new(AtomicUsize::new(0));
            event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 100 });
            if exit_first { event(&mut registry, view, FrameEventKind::RendererTerminated); }
            if let Some(close) = registry.release(view, callback(calls.clone())) { close(); }
            if !exit_first {
                if let Some(close) = event(&mut registry, view, FrameEventKind::RendererTerminated) { close(); }
            }
            assert!(event(&mut registry, view, FrameEventKind::RendererTerminated).is_none());
            assert!(registry.release(view, callback(calls.clone())).is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
    #[test]
    fn late_renderer_after_exit_revokes_queued_close() {
        let mut registry = Registry::default();
        let view = ViewId(1);
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 1 });
        registry.release(view, callback(Arc::new(AtomicUsize::new(0))));
        assert!(event(&mut registry, view, FrameEventKind::RendererTerminated).is_some());
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 2 });
        let entry = &registry.entries[&view];
        assert!(entry.switched);
        assert!(entry.process != Process::Gone);
    }
    #[test]
    fn unknown_and_switched_processes_are_never_assumed_gone() {
        let mut registry = Registry::default();
        let view = ViewId(1);
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(registry.release(view, callback(calls.clone())).is_none());
        assert!(event(&mut registry, view, FrameEventKind::RendererTerminated).is_none());
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 1 });
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 2 });
        assert!(event(&mut registry, view, FrameEventKind::RendererTerminated).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn captured_original_entry_cannot_authorize_a_reused_view_id() {
        let mut registry = Registry::default();
        let view = ViewId(5);
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 77 });
        let captured = CapturedIdentity { view, browser: 1, identity: registry.entries[&view].identity.clone() };
        assert!(matches(&registry.entries[&view], &captured));
        registry.entries.remove(&view);
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 77 });
        assert!(!matches(&registry.entries[&view], &captured));
    }

    #[test]
    fn a_live_or_unknown_peer_prevents_crashing_the_shared_renderer() {
        assert!(!no_live_peer_matches(77, [None].into_iter()));
        assert!(!no_live_peer_matches(77, [Some(77)].into_iter()));
        assert!(!no_live_peer_matches(77, [Some(88), None].into_iter()));
        assert!(no_live_peer_matches(77, [Some(88)].into_iter()));
        assert!(no_live_peer_matches(77, std::iter::empty()));
    }

    #[test]
    fn uncorrelated_legacy_exit_never_acknowledges_an_owned_renderer() {
        let mut registry = Registry::default();
        let view = ViewId(5);
        let results = Arc::new(Mutex::new(Vec::new()));
        event(&mut registry, view, FrameEventKind::RendererProcessIdentified { pid: 77 });
        let sink = results.clone();
        let entry = registry.entries.get_mut(&view).unwrap();
        entry.retired = true;
        entry.owned = Some(OwnedWait { done: Box::new(move |result| sink.lock().unwrap().push(result)), progress: Arc::new(|| {}), identity: Arc::new(()), subscription: None });
        assert!(results.lock().unwrap().is_empty());
        assert!(event(&mut registry, view, FrameEventKind::RendererTerminated).is_none());
        assert!(results.lock().unwrap().is_empty());
        assert!(registry.entries[&view].owned.is_some());
    }
}
