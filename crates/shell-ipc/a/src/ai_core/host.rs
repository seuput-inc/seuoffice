//! What the shared AI channels reach that is neither `main-core` nor a shared
//! host: the libraries `main-core` does not hold (the search backends, the
//! page reader, the remote image download), the AI routing state of the
//! subscription service, the entry's own values, and the queue of AI content
//! the tabs hand to a new docs tab.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use jscompat::JsonValue;
use main_core::packages::ai_search::search_tools::{SearchAnswer, SearchOptions};
use main_core::packages::i18n::Lang;
use shell_contract::runtime::CancelToken;

/// The answer of `fetchRemoteImage(url)` for a response that is ok: its body
/// as `readBodyCapped(resp, MAX_REMOTE_IMAGE_BYTES)` read it, and its
/// `content-type` header (`""` when it has none)
pub struct RemoteImage {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// Batch A host of the shared AI channels. The calls that wait on the network
/// (`web_search`, `image_search`, `read_url`, `fetch_remote_image`) are
/// blocking: the port runs each on a worker and posts the answer back to the
/// core thread; `signal` is the call's `AbortController.signal`.
pub trait AiCoreHost: Send + Sync {
    /// `getUiLang()`
    fn ui_lang(&self) -> Lang;
    /// `sharedSubscriptionStatuses()`
    fn subscription_statuses(&self) -> JsonValue;
    /// `isHeadlessMode()`: a headless export changes no setting
    fn is_headless_mode(&self) -> bool;
    /// `aiToolCapabilities(settings, context)` of `@seuoffice/ai-search`: the
    /// routing state it asks (the Codex image backend) is the subscription
    /// service's. `context` is `{ path: 'screen', statuses }`.
    fn ai_tool_capabilities(&self, settings: &JsonValue, context: &JsonValue) -> JsonValue;
    /// `webSearch(query, maxResults, options)` of `@seuoffice/ai-search`
    /// (Serper, Serply, SearchApi, Tavily, Parallel, Exa, Firecrawl, Keenable,
    /// DuckDuckGo); `Err` is what it threw
    fn web_search(&self, query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer;
    /// `imageSearch(query, maxResults, options)`; `Err` is what it threw
    fn image_search(&self, query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer;
    /// `readUrlTool(settingsPath, url, offset, { signal })` of
    /// `@seuoffice/ai-search`; `Err` is what it threw
    fn read_url(
        &self,
        settings_path: &str,
        url: &str,
        offset: f64,
        signal: &CancelToken,
    ) -> Result<JsonValue, JsonValue>;
    /// `fetchRemoteImage(url)` and `readBodyCapped(resp, MAX_REMOTE_IMAGE_BYTES)`:
    /// `None` for a refused address, a response that is not ok, a body over
    /// the cap or a failure of any kind (the handler's `catch` makes them all
    /// `null`)
    fn fetch_remote_image(&self, url: &str) -> Option<RemoteImage>;
}

pub type AiCoreHostHandle = Arc<dyn AiCoreHost>;

/// `pendingAiDocContents` of docs-main: the AI-authored content a fresh blank
/// docs tab waits for, by the tab's view. The shell inserts one, and hands the
/// same to what carries out `queueDocsAiContent` for the tabs.
#[derive(Default)]
pub struct PendingAiDocs {
    contents: Arc<Mutex<HashMap<i64, PendingAiDoc>>>,
    #[cfg(any(test, feature = "probe"))]
    synthetic: bool,
}

enum PendingAiDoc {
    #[cfg(any(test, feature = "probe"))]
    Synthetic(serde_json::Value),
    Accepted(Box<AcceptedAiDoc>),
}

pub struct AcceptedAiDoc {
    pub created: shell_contract::runtime::RendererGeneration,
    pub content: serde_json::Value,
    pub origin: shell_contract::ipc::NativeRequestOwner,
    pub participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    identity: Arc<AtomicU8>,
}

impl AcceptedAiDoc {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        created: shell_contract::runtime::RendererGeneration,
        content: serde_json::Value,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        reservation: shell_contract::ipc::NativeClosureReservation,
        browser: shell_contract::ipc::NativeBrowserOwner,
    ) -> Self {
        Self {
            created,
            content,
            origin,
            participants,
            cancel,
            pending,
            reservation,
            browser,
            identity: Arc::new(AtomicU8::new(0)),
        }
    }
}

pub type PendingAiDocsHost = Arc<PendingAiDocs>;

/// Exact pending-content insertion, retained until the tab is adopted or rolled back.
pub struct AiSeedRollback {
    contents: Arc<Mutex<HashMap<i64, PendingAiDoc>>>,
    key: i64,
    view: shell_contract::ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    closed: AtomicBool,
    identity: Arc<AtomicU8>,
    displaced: Mutex<Option<PendingAiDoc>>,
}
impl AiSeedRollback {
    pub fn was_consumed(&self) -> bool {
        self.identity.load(Ordering::Acquire) == 1
    }
    pub fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err("The AI document close proof does not match".into());
        }
        if self.identity.load(Ordering::Acquire) == 0 {
            let contents = self
                .contents
                .lock()
                .map_err(|_| "The pending document state is poisoned")?;
            if self.identity.load(Ordering::Acquire)==0 && !contents.get(&self.key).is_some_and(|entry|matches!(entry,PendingAiDoc::Accepted(seed) if Arc::ptr_eq(&seed.identity,&self.identity))) {
                return Err("The AI document seed was replaced before close".into());
            }
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
    pub fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created)
            || proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
        {
            return Err("The adopted AI document close proof does not match".into());
        }
        let mut contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        match self.identity.load(Ordering::Acquire) {
            1 | 3 => return Ok(()),
            2 => return Err("The AI document seed was already rolled back".into()),
            _ => {}
        }
        if !contents.get(&self.key).is_some_and(|entry|matches!(entry,PendingAiDoc::Accepted(seed) if Arc::ptr_eq(&seed.identity,&self.identity))) { return Err("The adopted AI document seed was replaced".into()); }
        let removed = contents.remove(&self.key);
        self.identity.store(3, Ordering::Release);
        drop(contents);
        drop(removed);
        Ok(())
    }
    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.browser) {
            return Err("The AI document supersession proof does not match".into());
        }
        let contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        if self.identity.load(Ordering::Acquire)==0
            && contents.get(&self.key).is_some_and(|entry|matches!(entry,PendingAiDoc::Accepted(seed) if Arc::ptr_eq(&seed.identity,&self.identity))) {
            return Err("The superseded AI document seed is still pending".into());
        }
        let mut displaced = self
            .displaced
            .lock()
            .map_err(|_| "The AI seed receipt is poisoned")?;
        if self.identity.load(Ordering::Acquire) == 0 {
            self.identity.store(3, Ordering::Release);
        }
        displaced.take();
        Ok(())
    }
    pub fn rollback(&self) -> Result<(), String> {
        if matches!(self.identity.load(Ordering::Acquire), 1..=3) {
            return Ok(());
        }
        if !self.created.is_active() && !self.closed.load(Ordering::Acquire) {
            return Err("The AI document seed lacks an exact close proof".into());
        }
        let mut contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        if matches!(self.identity.load(Ordering::Acquire), 1..=3) {
            return Ok(());
        }
        if !contents.get(&self.key).is_some_and(|entry| matches!(entry, PendingAiDoc::Accepted(seed) if Arc::ptr_eq(&seed.identity, &self.identity))) {
            return Err("The AI document seed was replaced before rollback".into());
        }
        let mut displaced = self
            .displaced
            .lock()
            .map_err(|_| "The AI seed receipt is poisoned")?;
        let removed = contents.remove(&self.key);
        if let Some(previous) = displaced.take()
            && !matches!(&previous, PendingAiDoc::Accepted(seed) if seed.identity.load(Ordering::Acquire)!=0)
        {
            contents.insert(self.key, previous);
        }
        self.identity.store(2, Ordering::Release);
        drop(contents);
        drop(removed);
        Ok(())
    }
}

pub struct PreparedAiSeed {
    contents: Arc<Mutex<HashMap<i64, PendingAiDoc>>>,
    seed: AcceptedAiDoc,
    receipt: AiSeedRollback,
}

impl PreparedAiSeed {
    pub fn commit(
        self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<AiSeedRollback, String> {
        if !transition.matches(&self.seed.reservation)
            || !(transition.matches_owner(&self.seed.browser)
                || transition.proposes_owner(&self.seed.browser))
        {
            return Err(
                "The AI document seed does not match the admitted browser transition".into(),
            );
        }
        if !self.seed.created.is_active() {
            return Err("The initialized document was replaced".into());
        }
        let mut contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        if contents.get(&self.receipt.key).is_some_and(
            |entry| matches!(entry,PendingAiDoc::Accepted(entry) if entry.created.is_active()),
        ) {
            return Err("The initialized document already has pending AI content".into());
        }
        if contents.capacity() == contents.len() && !contents.contains_key(&self.receipt.key) {
            return Err("The pending AI document capacity was consumed".into());
        }
        let mut displaced = self
            .receipt
            .displaced
            .lock()
            .map_err(|_| "The AI seed receipt is poisoned")?;
        initialize()?;
        *displaced = contents.insert(
            self.receipt.key,
            PendingAiDoc::Accepted(Box::new(self.seed)),
        );
        drop(displaced);
        drop(contents);
        Ok(self.receipt)
    }
}

impl PendingAiDocs {
    pub fn prepare_accepted(
        &self,
        view: shell_contract::ViewId,
        seed: AcceptedAiDoc,
    ) -> Result<PreparedAiSeed, String> {
        if !seed.created.is_active() {
            return Err("The initialized document was replaced".into());
        }
        if !seed.reservation.matches(view, &seed.created)
            || !seed
                .browser
                .reservation()
                .same_reservation(&seed.reservation)
        {
            return Err("The AI document reservation or browser does not match".into());
        }
        self.contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        let receipt = AiSeedRollback {
            contents: self.contents.clone(),
            key: i64::from(view.0),
            view,
            created: seed.created.clone(),
            reservation: seed.reservation.clone(),
            browser: seed.browser.clone(),
            closed: AtomicBool::new(false),
            identity: seed.identity.clone(),
            displaced: Mutex::new(None),
        };
        Ok(PreparedAiSeed {
            contents: self.contents.clone(),
            seed,
            receipt,
        })
    }

    #[cfg(any(test, feature = "probe"))]
    pub fn synthetic() -> Self {
        Self {
            contents: Arc::default(),
            synthetic: true,
        }
    }

    #[cfg(any(test, feature = "probe"))]
    pub fn queue(&self, wc_id: i64, content: &serde_json::Value) {
        assert!(
            self.synthetic,
            "Numeric AI content queues require an explicit synthetic host"
        );
        self.contents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(wc_id, PendingAiDoc::Synthetic(content.clone()));
    }

    #[cfg(test)]
    /// Called only inside the native source/participants/created admission.
    pub fn queue_accepted_in_transition(
        &self,
        view: shell_contract::ViewId,
        seed: AcceptedAiDoc,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<AiSeedRollback, String> {
        if !transition.matches(&seed.reservation)
            || !(transition.matches_owner(&seed.browser)
                || transition.proposes_owner(&seed.browser))
        {
            return Err(
                "The AI document seed does not match the admitted browser transition".into(),
            );
        }
        self.queue_accepted_fenced(view, seed, initialize)
    }

    #[cfg(test)]
    fn queue_accepted_fenced(
        &self,
        view: shell_contract::ViewId,
        seed: AcceptedAiDoc,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<AiSeedRollback, String> {
        if !seed.created.is_active() {
            return Err("The initialized document was replaced".into());
        }
        if !seed.reservation.matches(view, &seed.created) {
            return Err("The AI document reservation does not match".into());
        }
        if !seed
            .browser
            .reservation()
            .same_reservation(&seed.reservation)
        {
            return Err("The AI document browser owner does not match".into());
        }
        let created = seed.created.clone();
        let reservation = seed.reservation.clone();
        let browser = seed.browser.clone();
        let identity = seed.identity.clone();
        let mut contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        if contents.get(&i64::from(view.0)).is_some_and(
            |entry| matches!(entry, PendingAiDoc::Accepted(entry) if entry.created.is_active()),
        ) {
            return Err("The initialized document already has pending AI content".into());
        }
        initialize()?;
        let displaced = contents.insert(i64::from(view.0), PendingAiDoc::Accepted(Box::new(seed)));
        Ok(AiSeedRollback {
            contents: self.contents.clone(),
            key: i64::from(view.0),
            view,
            created,
            reservation,
            browser,
            closed: AtomicBool::new(false),
            identity,
            displaced: Mutex::new(displaced),
        })
    }

    pub fn consume_owned(
        &self,
        view: shell_contract::ViewId,
        native: Option<&shell_contract::ipc::NativeContext>,
        cancel: &CancelToken,
        effects: Option<&shell_contract::hosts::OwnedEffectsHost>,
    ) -> Result<Option<serde_json::Value>, String> {
        #[cfg(any(test, feature = "probe"))]
        if self.synthetic {
            return Ok(self
                .contents
                .lock()
                .map_err(|_| "The pending document state is poisoned")?
                .remove(&i64::from(view.0))
                .map(|entry| match entry {
                    PendingAiDoc::Synthetic(content) => content,
                    PendingAiDoc::Accepted(entry) => entry.content,
                }));
        }
        let context = native
            .cloned()
            .ok_or("The initial document request has no native owner")?;
        let consumer = shell_contract::ipc::NativeRequestOwner { view, context };
        let generation = consumer
            .context
            .renderer()
            .ok_or("The initial document renderer is unavailable")?;
        let effects = effects.ok_or("Native initial document effects are unavailable")?;
        let mut result = None;
        effects.with_owned_effect(&consumer, &consumer, cancel, cancel, &mut || {
            result = self.take_accepted_fenced(view, generation)?;
            Ok(())
        })?;
        Ok(result)
    }

    fn take_accepted_fenced(
        &self,
        view: shell_contract::ViewId,
        generation: &shell_contract::runtime::RendererGeneration,
    ) -> Result<Option<serde_json::Value>, String> {
        let mut contents = self
            .contents
            .lock()
            .map_err(|_| "The pending document state is poisoned")?;
        let key = i64::from(view.0);
        match contents.get(&key) {
            Some(PendingAiDoc::Accepted(seed))
                if seed.created.is_active() && seed.created.same_generation(generation) => {}
            Some(_) => return Err("The initial AI content belongs to another document".into()),
            None => return Ok(None),
        }
        Ok(match contents.remove(&key) {
            Some(PendingAiDoc::Accepted(seed)) => {
                seed.identity.store(1, Ordering::Release);
                Some(seed.content)
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod accepted_tests {
    use super::*;
    use serde_json::json;
    use shell_contract::{
        ViewId,
        ipc::{NativeContext, NativeRequestOwner},
        runtime::RendererGeneration,
    };

    fn seed(
        created: &RendererGeneration,
        source: &RendererGeneration,
        cancel: &CancelToken,
    ) -> AcceptedAiDoc {
        let (_, reservation, browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), created.clone());
        AcceptedAiDoc::new(
            created.clone(),
            json!({ "title": "Real HTML", "html": "<h1>제목</h1><p>hello</p>" }),
            NativeRequestOwner {
                view: ViewId(1),
                context: NativeContext::with_renderer((), source.clone()),
            },
            Vec::new(),
            cancel.clone(),
            CancelToken::default(),
            reservation,
            browser,
        )
    }

    #[test]
    fn prepared_ai_seed_rejects_foreign_browser_and_failed_initializer() {
        let pending = PendingAiDocs::default();
        let created = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        let (_, reservation, browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), created.clone());
        let (_, foreign, foreign_browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), created.clone());
        let make = || {
            let mut value = seed(&created, &source, &cancel);
            value.reservation = reservation.clone();
            value.browser = browser.clone();
            pending.prepare_accepted(ViewId(7), value).unwrap()
        };
        let mut calls = 0;
        assert!(
            foreign
                .with_initial_owner(&foreign_browser, |transition| make().commit(
                    transition,
                    &mut || {
                        calls += 1;
                        Ok(())
                    }
                ))
                .is_err()
        );
        assert_eq!(calls, 0);
        assert!(
            reservation
                .with_initial_owner(&browser, |transition| make()
                    .commit(transition, &mut || Err("rejected".into())))
                .is_err()
        );
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &created)
                .unwrap()
                .is_none()
        );
        let receipt = reservation
            .with_initial_owner(&browser, |transition| {
                make().commit(transition, &mut || Ok(()))
            })
            .unwrap();
        receipt.rollback().unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &created)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn accepted_initialization_survives_normal_source_close_and_is_taken_once() {
        let pending = PendingAiDocs::default();
        let source = RendererGeneration::new();
        let created = RendererGeneration::new();
        let cancel = CancelToken::default();
        pending
            .queue_accepted_fenced(ViewId(7), seed(&created, &source, &cancel), &mut || Ok(()))
            .unwrap();
        source.revoke();
        cancel.cancel();
        let value = created
            .with_active(|| pending.take_accepted_fenced(ViewId(7), &created))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(value["html"], "<h1>제목</h1><p>hello</p>");
        assert_eq!(
            pending.take_accepted_fenced(ViewId(7), &created).unwrap(),
            None
        );
    }

    #[test]
    fn replacement_cannot_steal_payload_from_same_numeric_view() {
        let pending = PendingAiDocs::default();
        let created = RendererGeneration::new();
        pending
            .queue_accepted_fenced(
                ViewId(7),
                seed(
                    &created,
                    &RendererGeneration::new(),
                    &CancelToken::default(),
                ),
                &mut || Ok(()),
            )
            .unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &RendererGeneration::new())
                .is_err()
        );
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &created)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn target_loss_requires_exact_close_before_unconsumed_rollback() {
        let pending = PendingAiDocs::default();
        let created = RendererGeneration::new();
        let (issuer, reservation, browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), created.clone());
        let mut content = seed(
            &created,
            &RendererGeneration::new(),
            &CancelToken::default(),
        );
        content.reservation = reservation;
        content.browser = browser;
        let receipt = pending
            .queue_accepted_fenced(ViewId(7), content, &mut || Ok(()))
            .unwrap();
        created.revoke();
        assert!(receipt.rollback().is_err());
        receipt.retire_closed(&issuer.closed()).unwrap();
        receipt.rollback().unwrap();
        assert_eq!(
            pending
                .take_accepted_fenced(ViewId(7), &RendererGeneration::new())
                .unwrap(),
            None
        );
    }

    #[test]
    fn rejected_initialization_publishes_no_payload_and_does_not_consume_the_target() {
        let pending = PendingAiDocs::default();
        let created = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        assert!(
            pending
                .queue_accepted_fenced(ViewId(7), seed(&created, &source, &cancel), &mut || Err(
                    "The captured placement transaction was revoked".into()
                ))
                .is_err()
        );
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &created)
                .unwrap()
                .is_none()
        );
        pending
            .queue_accepted_fenced(ViewId(7), seed(&created, &source, &cancel), &mut || Ok(()))
            .unwrap();
        assert_eq!(
            pending
                .take_accepted_fenced(ViewId(7), &created)
                .unwrap()
                .unwrap()["title"],
            "Real HTML"
        );
    }

    #[test]
    fn production_consumer_never_falls_back_to_a_numeric_queue() {
        assert!(
            PendingAiDocs::default()
                .consume_owned(ViewId(7), None, &CancelToken::default(), None)
                .is_err()
        );
    }
    #[test]
    fn receipt_restores_exact_displaced_seed_and_never_erases_replacement() {
        let pending = PendingAiDocs::default();
        let old_generation = RendererGeneration::new();
        let next_generation = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        let (old_issuer, old_reservation, old_browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(
                ViewId(7),
                old_generation.clone(),
            );
        let mut old_seed = seed(&old_generation, &source, &cancel);
        old_seed.reservation = old_reservation;
        old_seed.browser = old_browser;
        let old = pending
            .queue_accepted_fenced(ViewId(7), old_seed, &mut || Ok(()))
            .unwrap();
        old_generation.revoke();
        let (next_issuer, next_reservation, next_browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(
                ViewId(7),
                next_generation.clone(),
            );
        let mut next_seed = seed(&next_generation, &source, &cancel);
        next_seed.reservation = next_reservation;
        next_seed.browser = next_browser;
        let next = pending
            .queue_accepted_fenced(ViewId(7), next_seed, &mut || Ok(()))
            .unwrap();
        assert!(old.rollback().is_err());
        next_generation.revoke();
        let (forged, _) =
            shell_contract::ipc::NativeClosureIssuer::new(ViewId(7), next_generation.clone());
        assert!(next.retire_closed(&forged.closed()).is_err());
        next.retire_closed(&next_issuer.closed()).unwrap();
        next.rollback().unwrap();
        next.rollback().unwrap();
        old.retire_closed(&old_issuer.closed()).unwrap();
        old.rollback().unwrap();
    }
    #[test]
    fn consumed_receipt_is_idempotent_and_preserves_later_seed() {
        let pending = PendingAiDocs::default();
        let generation = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        let receipt = pending
            .queue_accepted_fenced(ViewId(7), seed(&generation, &source, &cancel), &mut || {
                Ok(())
            })
            .unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &generation)
                .unwrap()
                .is_some()
        );
        assert!(receipt.was_consumed());
        let later = pending
            .queue_accepted_fenced(ViewId(7), seed(&generation, &source, &cancel), &mut || {
                Ok(())
            })
            .unwrap();
        receipt.rollback().unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &generation)
                .unwrap()
                .is_some()
        );
        later.rollback().unwrap();
    }
    #[test]
    fn supersession_retires_only_the_exact_consumed_seed() {
        let pending = PendingAiDocs::default();
        let generation = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        let (_, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), generation.clone());
        let mut original = seed(&generation, &source, &cancel);
        original.reservation = reservation.clone();
        original.browser = owner.clone();
        let receipt = reservation
            .with_initial_owner(&owner, |transition| {
                pending.queue_accepted_in_transition(
                    ViewId(7),
                    original,
                    transition,
                    &mut || Ok(()),
                )
            })
            .unwrap();
        let (_, proof, ()) = reservation
            .prepare_replacement(&owner)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        let (_, foreign_reservation, foreign_owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(7), generation.clone());
        foreign_reservation
            .with_initial_owner(&foreign_owner, |_| Ok(()))
            .unwrap();
        let (_, foreign_proof, ()) = foreign_reservation
            .prepare_replacement(&foreign_owner)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        assert!(receipt.retire_superseded(&foreign_proof).is_err());
        assert!(receipt.retire_superseded(&proof).is_err());
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &generation)
                .unwrap()
                .is_some()
        );
        receipt.retire_superseded(&proof).unwrap();
        let later = pending
            .queue_accepted_fenced(ViewId(7), seed(&generation, &source, &cancel), &mut || {
                Ok(())
            })
            .unwrap();
        receipt.retire_superseded(&proof).unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &generation)
                .unwrap()
                .is_some()
        );
        later.rollback().unwrap();
    }
    #[test]
    fn retired_displaced_ai_seed_is_not_restored_by_later_rollback() {
        let pending = PendingAiDocs::default();
        let old_generation = RendererGeneration::new();
        let next_generation = RendererGeneration::new();
        let source = RendererGeneration::new();
        let cancel = CancelToken::default();
        let (_, reservation, browser) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(
            ViewId(7),
            old_generation.clone(),
        );
        let mut old_seed = seed(&old_generation, &source, &cancel);
        old_seed.reservation = reservation.clone();
        old_seed.browser = browser.clone();
        let old = reservation
            .with_initial_owner(&browser, |transition| {
                pending
                    .prepare_accepted(ViewId(7), old_seed)?
                    .commit(transition, &mut || Ok(()))
            })
            .unwrap();
        let (_, proof, ()) = reservation
            .prepare_replacement(&browser)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        old_generation.revoke();
        let next = pending
            .queue_accepted_fenced(
                ViewId(7),
                seed(&next_generation, &source, &cancel),
                &mut || Ok(()),
            )
            .unwrap();
        old.retire_superseded(&proof).unwrap();
        next.rollback().unwrap();
        assert!(
            pending
                .take_accepted_fenced(ViewId(7), &next_generation)
                .unwrap()
                .is_none()
        );
    }
}
