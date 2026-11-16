//! Retirement of a released view's document before its browser closes.
//!
//! The system webviews expose no process identity, so a released view is not waited on until
//! its renderer exits. Its document is revoked in Rust and replaced by `about:blank` (the
//! blank commit is observed), then the view closes; the browser's `Retired` receipt completes
//! the retirement.
use crate::native::{
    BrowserState, DocumentContext, NativeEvent, NativeEventKind, ObservedNavigation,
};
use shell_contract::ids::ViewId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tauri::Url;

type Close = Box<dyn FnOnce() + Send + 'static>;
type OwnedDone = Box<dyn FnOnce(Result<(), String>) + Send>;
struct OwnedWait {
    done: OwnedDone,
    identity: Arc<()>,
    navigation: Option<ObservedNavigation>,
}
impl OwnedWait {
    fn finish(self, result: Result<(), String>) {
        let Self {
            done, navigation, ..
        } = self;
        drop(navigation);
        done(result);
    }
}

#[derive(Clone)]
struct CapturedIdentity {
    view: ViewId,
    browser: BrowserState,
    identity: Arc<()>,
}
#[derive(Clone)]
pub(crate) struct CapturedRenderer {
    identity: CapturedIdentity,
    origin: DocumentContext,
    retirement: Arc<()>,
}

#[derive(Default)]
struct Entry {
    browser: Option<BrowserState>,
    /// The latest registered document; frozen once the view is retired.
    document: Option<DocumentContext>,
    switched: bool,
    retired: bool,
    /// The document is gone from the browser: blank committed, process failed or retired.
    unloaded: bool,
    close: Option<Close>,
    identity: Arc<()>,
    owned_retirement: Option<Arc<()>>,
    owned: Option<OwnedWait>,
}

enum Release {
    Nothing,
    Close(Close),
    Unload {
        browser: BrowserState,
        identity: Arc<()>,
    },
}

#[derive(Default)]
struct Registry {
    entries: HashMap<ViewId, Entry>,
}
impl Registry {
    fn observe(&mut self, view: ViewId, event: &NativeEvent) -> Option<Close> {
        let entry = self.entries.entry(view).or_default();
        if entry
            .browser
            .as_ref()
            .is_some_and(|known| !known.is_same_browser(&event.browser))
        {
            entry.switched = true;
            entry.document = None;
            return entry.fail_owned("The captured renderer browser was replaced");
        }
        if entry.browser.is_none() {
            entry.browser = Some(event.browser.clone());
        }
        match &event.kind {
            NativeEventKind::DocumentRegistered { context } if !entry.retired => {
                entry.document = Some(context.clone());
                None
            }
            NativeEventKind::WebviewProcessFailed { .. } if entry.retired => entry.settle(),
            NativeEventKind::Retired { .. } => {
                entry.close = None;
                entry.settle()
            }
            _ => None,
        }
    }

    fn release(&mut self, view: ViewId, close: Close) -> Release {
        let entry = self.entries.entry(view).or_default();
        if entry.retired {
            return Release::Nothing;
        }
        entry.retired = true;
        let live = entry
            .document
            .as_ref()
            .is_some_and(DocumentContext::is_registered);
        match entry.browser.clone() {
            Some(browser) if live => {
                entry.close = Some(close);
                Release::Unload {
                    browser,
                    identity: entry.identity.clone(),
                }
            }
            _ => {
                entry.unloaded = true;
                Release::Close(close)
            }
        }
    }

    fn finish_unload(&mut self, view: ViewId, identity: &Arc<()>) -> Option<Close> {
        self.entries
            .get_mut(&view)
            .filter(|entry| Arc::ptr_eq(&entry.identity, identity))
            .and_then(Entry::settle)
    }

    fn can_close(&self, view: ViewId) -> bool {
        self.entries
            .get(&view)
            .is_some_and(|entry| entry.retired && !entry.switched && entry.unloaded)
    }
}

impl Entry {
    fn fail_owned(&mut self, message: &str) -> Option<Close> {
        let waiter = self.owned.take()?;
        let message = message.to_owned();
        Some(Box::new(move || waiter.finish(Err(message))))
    }

    /// The document is gone: finish the owned waiter and run the queued close.
    fn settle(&mut self) -> Option<Close> {
        self.unloaded = true;
        let close = self.close.take();
        let waiter = self.owned.take();
        if close.is_none() && waiter.is_none() {
            return None;
        }
        Some(Box::new(move || {
            if let Some(waiter) = waiter {
                waiter.finish(Ok(()));
            }
            if let Some(close) = close {
                close();
            }
        }))
    }
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|error| error.into_inner())
}

fn blank() -> Url {
    Url::parse("about:blank").expect("about:blank is a valid URL")
}

/// Close the view once its document is unloaded. `close` runs at most once and never inline
/// with an event dispatch it did not ask for.
pub(crate) fn release(view: ViewId, close: impl FnOnce() + Send + 'static) {
    let plan = registry().release(view, Box::new(close));
    match plan {
        Release::Nothing => {}
        Release::Close(close) => close(),
        Release::Unload { browser, identity } => {
            let unloaded = identity.clone();
            let started = browser
                .navigator()
                .navigate_observed(blank(), move |result| {
                    if let Err(error) = result {
                        eprintln!("shell: unloading released view {}: {error}", view.0);
                    }
                    let action = registry().finish_unload(view, &unloaded);
                    if let Some(action) = action {
                        action();
                    }
                });
            if let Err(error) = started {
                eprintln!("shell: unloading released view {}: {error}", view.0);
                let action = registry().finish_unload(view, &identity);
                if let Some(action) = action {
                    action();
                }
            }
        }
    }
}

pub(crate) fn on_frame(view: ViewId, event: &NativeEvent) {
    let action = registry().observe(view, event);
    if let Some(action) = action {
        action();
    }
}
pub(crate) fn forget(view: ViewId) {
    let action = registry().entries.remove(&view).and_then(|mut entry| {
        entry.fail_owned("The captured renderer was removed before disposal completed")
    });
    if let Some(action) = action {
        action();
    }
}

pub(crate) fn can_close(view: ViewId) -> bool {
    registry().can_close(view)
}

pub(crate) fn capture(view: ViewId, origin: &DocumentContext) -> Result<CapturedRenderer, String> {
    if !origin.is_registered() {
        return Err("The original native document is unavailable".into());
    }
    let registry = registry();
    let entry = registry
        .entries
        .get(&view)
        .ok_or("The original renderer has not been observed")?;
    if entry.document.as_ref() != Some(origin)
        || entry.retired
        || entry.switched
        || !entry
            .browser
            .as_ref()
            .is_some_and(|browser| browser.is_same_browser(origin.browser()))
    {
        return Err("The original native renderer is not available for captured disposal".into());
    }
    Ok(CapturedRenderer {
        identity: CapturedIdentity {
            view,
            browser: origin.browser().clone(),
            identity: entry.identity.clone(),
        },
        origin: origin.clone(),
        retirement: Arc::new(()),
    })
}

fn matches(entry: &Entry, captured: &CapturedIdentity) -> bool {
    !entry.switched
        && Arc::ptr_eq(&entry.identity, &captured.identity)
        && entry
            .browser
            .as_ref()
            .is_some_and(|browser| browser.is_same_browser(&captured.browser))
}
fn matches_owned(entry: &Entry, captured: &CapturedRenderer) -> bool {
    matches(entry, &captured.identity) && entry.document.as_ref() == Some(&captured.origin)
}

fn reserve_owned(entry: &mut Entry, retirement: &Arc<()>) -> Result<(), String> {
    if entry.owned.is_some() {
        return Err("The captured renderer already has a disposal waiter".into());
    }
    if entry.retired {
        if !entry
            .owned_retirement
            .as_ref()
            .is_some_and(|original| Arc::ptr_eq(original, retirement))
        {
            return Err("The captured renderer was retired by another operation".into());
        }
    } else {
        entry.retired = true;
        entry.owned_retirement = Some(retirement.clone());
    }
    Ok(())
}

fn take_owned(
    captured: &CapturedRenderer,
    waiter_identity: &Arc<()>,
    unloaded: bool,
) -> Option<OwnedWait> {
    let mut registry = registry();
    registry
        .entries
        .get_mut(&captured.identity.view)
        .filter(|entry| matches_owned(entry, captured))
        .filter(|entry| {
            entry
                .owned
                .as_ref()
                .is_some_and(|waiter| Arc::ptr_eq(&waiter.identity, waiter_identity))
        })
        .and_then(|entry| {
            entry.unloaded |= unloaded;
            entry.owned.take()
        })
}

/// Revoke the captured document by navigating its browser to `about:blank`. `done` runs once:
/// `Ok` when the blank document committed or the document was already revoked in Rust (every
/// capability is Rust-side, so nothing of it can act any more), `Err` while the document
/// is still live.
pub(crate) fn release_owned(captured: &CapturedRenderer, done: OwnedDone) -> Result<(), String> {
    let waiter_identity = Arc::new(());
    {
        let mut registry = registry();
        let entry = registry
            .entries
            .get_mut(&captured.identity.view)
            .ok_or("The captured renderer no longer exists")?;
        if !matches_owned(entry, captured) {
            return Err("The captured renderer was replaced".into());
        }
        reserve_owned(entry, &captured.retirement)?;
        entry.owned = Some(OwnedWait {
            done,
            identity: waiter_identity.clone(),
            navigation: None,
        });
    }
    let original = captured.clone();
    let callback_identity = waiter_identity.clone();
    let started = captured
        .identity
        .browser
        .navigator()
        .navigate_observed(blank(), move |result| {
            let outcome = match result {
                Ok(_) => Ok(()),
                Err(_) if original.origin.is_revoked() => Ok(()),
                Err(error) => Err(error),
            };
            let waiter = take_owned(&original, &callback_identity, outcome.is_ok());
            if let Some(waiter) = waiter {
                waiter.finish(outcome);
            }
        });
    match started {
        Ok(navigation) => {
            let stale = {
                let mut registry = registry();
                let waiter = registry
                    .entries
                    .get_mut(&captured.identity.view)
                    .filter(|entry| matches_owned(entry, captured))
                    .and_then(|entry| entry.owned.as_mut())
                    .filter(|waiter| Arc::ptr_eq(&waiter.identity, &waiter_identity));
                match waiter {
                    Some(waiter) => {
                        waiter.navigation = Some(navigation);
                        None
                    }
                    None => Some(navigation),
                }
            };
            if let Some(navigation) = stale {
                navigation.cancel();
            }
            Ok(())
        }
        Err(error) => {
            let revoked = captured.origin.is_revoked();
            let waiter = take_owned(captured, &waiter_identity, revoked);
            match waiter {
                Some(waiter) if revoked => {
                    waiter.finish(Ok(()));
                    Ok(())
                }
                _ => Err(error),
            }
        }
    }
}

pub(crate) fn cancel_owned(captured: &CapturedRenderer) {
    let waiter = {
        let mut registry = registry();
        registry
            .entries
            .get_mut(&captured.identity.view)
            .filter(|entry| matches_owned(entry, captured))
            .and_then(|entry| entry.owned.take())
    };
    if let Some(navigation) = waiter.and_then(|waiter| waiter.navigation) {
        navigation.cancel();
    }
}

pub(crate) fn can_close_owned(captured: &CapturedRenderer) -> bool {
    registry()
        .entries
        .get(&captured.identity.view)
        .is_some_and(|entry| matches_owned(entry, captured) && entry.retired && entry.unloaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::browser::testing::registered_context;
    use crate::native::events::ProcessFailure;
    use crate::native::{BrowserCloseOutcome, ViewEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn event(browser: &BrowserState, kind: NativeEventKind) -> NativeEvent {
        NativeEvent {
            browser: browser.clone(),
            kind,
        }
    }
    fn registered(browser: &BrowserState, context: &DocumentContext) -> NativeEvent {
        event(
            browser,
            NativeEventKind::DocumentRegistered {
                context: context.clone(),
            },
        )
    }
    fn callback(count: Arc<AtomicUsize>) -> Close {
        Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_view_without_a_live_document_closes_at_once_and_only_once() {
        let (browser, _context) = registered_context("dispose-blank");
        let mut registry = Registry::default();
        let view = ViewId(1);
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(
            registry
                .observe(
                    view,
                    &event(&browser, NativeEventKind::View(ViewEvent::Focus))
                )
                .is_none()
        );
        match registry.release(view, callback(calls.clone())) {
            Release::Close(close) => close(),
            _ => panic!("nothing is left to unload"),
        }
        assert!(registry.can_close(view));
        assert!(matches!(
            registry.release(view, callback(calls.clone())),
            Release::Nothing
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_live_document_is_unloaded_before_the_close_runs() {
        let (browser, context) = registered_context("dispose-live");
        let mut registry = Registry::default();
        let view = ViewId(1);
        let calls = Arc::new(AtomicUsize::new(0));
        registry.observe(view, &registered(&browser, &context));
        let Release::Unload { identity, .. } = registry.release(view, callback(calls.clone()))
        else {
            panic!("a live document must be unloaded first");
        };
        assert!(!registry.can_close(view));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        registry
            .finish_unload(view, &identity)
            .expect("the queued close")();
        assert!(registry.can_close(view));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            registry.finish_unload(view, &identity).is_none(),
            "the close runs once"
        );
    }

    #[test]
    fn a_failed_process_unloads_a_retired_view() {
        let (browser, context) = registered_context("dispose-failed");
        let mut registry = Registry::default();
        let view = ViewId(1);
        let calls = Arc::new(AtomicUsize::new(0));
        registry.observe(view, &registered(&browser, &context));
        assert!(matches!(
            registry.release(view, callback(calls.clone())),
            Release::Unload { .. }
        ));
        let failed = event(
            &browser,
            NativeEventKind::WebviewProcessFailed {
                kind: ProcessFailure::Unknown,
            },
        );
        registry.observe(view, &failed).expect("the queued close")();
        assert!(registry.can_close(view));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_retired_browser_drops_the_queued_close() {
        let (browser, context) = registered_context("dispose-retired");
        let mut registry = Registry::default();
        let view = ViewId(1);
        let calls = Arc::new(AtomicUsize::new(0));
        registry.observe(view, &registered(&browser, &context));
        registry.release(view, callback(calls.clone()));
        let retired = event(
            &browser,
            NativeEventKind::Retired {
                outcome: BrowserCloseOutcome::Retired,
            },
        );
        assert!(registry.observe(view, &retired).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(registry.entries[&view].unloaded);
    }

    #[test]
    fn a_replaced_browser_never_authorizes_the_close() {
        let (first, context) = registered_context("dispose-first");
        let (second, _) = registered_context("dispose-second");
        let mut registry = Registry::default();
        let view = ViewId(1);
        registry.observe(view, &registered(&first, &context));
        registry.observe(
            view,
            &event(&second, NativeEventKind::View(ViewEvent::Blur)),
        );
        assert!(registry.entries[&view].switched);
        let calls = Arc::new(AtomicUsize::new(0));
        if let Release::Close(close) = registry.release(view, callback(calls.clone())) {
            close();
        }
        assert!(!registry.can_close(view));
    }

    #[test]
    fn a_retired_view_keeps_the_document_it_was_captured_with() {
        let (browser, context) = registered_context("dispose-frozen");
        let (_, later) = registered_context("dispose-later");
        let mut registry = Registry::default();
        let view = ViewId(1);
        registry.observe(view, &registered(&browser, &context));
        registry.release(view, callback(Arc::new(AtomicUsize::new(0))));
        registry.observe(view, &registered(&browser, &later));
        assert_eq!(registry.entries[&view].document.as_ref(), Some(&context));
    }

    #[test]
    fn captured_original_entry_cannot_authorize_a_reused_view_id() {
        let (browser, context) = registered_context("dispose-reused");
        let mut registry = Registry::default();
        let view = ViewId(5);
        registry.observe(view, &registered(&browser, &context));
        let captured = CapturedIdentity {
            view,
            browser: browser.clone(),
            identity: registry.entries[&view].identity.clone(),
        };
        assert!(matches(&registry.entries[&view], &captured));
        registry.entries.remove(&view);
        registry.observe(view, &registered(&browser, &context));
        assert!(!matches(&registry.entries[&view], &captured));
    }

    #[test]
    fn failed_wait_rearms_only_the_same_retirement_and_rejects_concurrent_waiters() {
        let mut entry = Entry::default();
        let retirement = Arc::new(());
        let other = Arc::new(());
        reserve_owned(&mut entry, &retirement).unwrap();
        let first = Arc::new(());
        entry.owned = Some(OwnedWait {
            done: Box::new(|_| {}),
            identity: first.clone(),
            navigation: None,
        });
        assert!(reserve_owned(&mut entry, &retirement).is_err());
        entry
            .owned
            .take()
            .unwrap()
            .finish(Err("wait failed".into()));
        assert!(entry.retired);
        assert!(reserve_owned(&mut entry, &other).is_err());
        reserve_owned(&mut entry, &retirement).unwrap();
        let second = Arc::new(());
        entry.owned = Some(OwnedWait {
            done: Box::new(|_| {}),
            identity: second.clone(),
            navigation: None,
        });
        assert!(!Arc::ptr_eq(
            &first,
            &entry.owned.as_ref().unwrap().identity
        ));
        assert!(Arc::ptr_eq(
            &second,
            &entry.owned.as_ref().unwrap().identity
        ));
    }

    #[test]
    fn an_owned_waiter_fails_when_its_browser_is_replaced() {
        let (first, context) = registered_context("dispose-owned-first");
        let (second, _) = registered_context("dispose-owned-second");
        let mut registry = Registry::default();
        let view = ViewId(1);
        let results = Arc::new(Mutex::new(Vec::new()));
        registry.observe(view, &registered(&first, &context));
        let sink = results.clone();
        let entry = registry.entries.get_mut(&view).unwrap();
        entry.retired = true;
        entry.owned = Some(OwnedWait {
            done: Box::new(move |result| sink.lock().unwrap().push(result)),
            identity: Arc::new(()),
            navigation: None,
        });
        registry
            .observe(
                view,
                &event(&second, NativeEventKind::View(ViewEvent::Focus)),
            )
            .expect("the failed waiter")();
        assert!(matches!(results.lock().unwrap().as_slice(), [Err(_)]));
    }
}
