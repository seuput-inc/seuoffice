//! Batch M's process state: what the Electron main process kept in module
//! variables for these channels. The shell makes one [`MState`] and puts it
//! in `Hosts` as [`MStateRef`] (settings paths come from the caller); the handlers take it from there, and the
//! shell's own M host files (the context menu, the menus that need the
//! current language) keep a clone.
//!
//! Locks: the handlers run on the core thread, so the locks never contend
//! there; they exist because the shell's menu code and the print workers
//! reach the same state from other threads.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use jscompat::JsonValue;
use main_core::packages::i18n::{self, Lang};
use main_core::shell::app_settings::read_app_settings;
use shell_contract::ViewId;

use crate::hosts::MHost;

/// `UiTheme` of the home API
pub const UI_THEMES: [&str; 3] = ["light", "dark", "system"];
/// `DocTheme` of the home API
pub const DOC_THEMES: [&str; 3] = ["light", "dark", "follow"];

/// The language as the process holds it: `uiLang` of the shell and the
/// `uiLang`/`requestedLocale` pair of `@seuoffice/i18n`
#[derive(Clone, Debug, PartialEq)]
pub struct UiLanguage {
    pub lang: Lang,
    /// what `documentLocale()` derives the document locale from
    pub requested: String,
}

/// `ClickClaims` of `apps/docs/src/shared/context-menu-claims.ts`: the
/// right-clicks a docs renderer claimed, oldest first, each live for 2 s
#[derive(Clone, Debug, Default)]
pub struct ClickClaims(Vec<(f64, f64)>);

pub const CLAIM_MAX_AGE_MS: f64 = 2000.0;

impl ClickClaims {
    fn prune(&mut self, now: f64) {
        self.0.retain(|&(_, at)| now - at <= CLAIM_MAX_AGE_MS);
    }

    pub fn claim(&mut self, seq: f64, now: f64) {
        self.prune(now);
        self.0.push((seq, now));
    }

    pub fn take(&mut self, now: f64) -> Option<f64> {
        self.prune(now);
        if self.0.is_empty() {
            None
        } else {
            Some(self.0.remove(0).0)
        }
    }
}

#[derive(Default)]
struct Inner {
    lang: Option<UiLanguage>,
    theme: Option<String>,
    doc_theme: Option<String>,
    /// views that armed the docs context menu, and their claims
    armed: HashSet<ViewId>,
    claims: HashMap<ViewId, ClickClaims>,
    /// `spellIgnored`: word -> the views that ignored it
    spell_ignored: Vec<(String, Vec<ViewId>)>,
    /// views with a print running (`printing` of hwp, `printingDocuments`
    /// of markdown)
    hwp_printing: HashSet<ViewId>,
    markdown_printing: HashSet<ViewId>,
    hwp_pending: Vec<(ViewId, crate::print::PendingHwpPublication)>,
}

#[derive(Default)]
pub struct MState {
    inner: Mutex<Inner>,
}

pub type MStateRef = Arc<MState>;

impl MState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call on the original core thread during process shutdown.
    pub fn cancel_prepared_hwp_publications(&self) {
        crate::print::cancel_prepared_hwp_publications();
    }

    pub(crate) fn retain_hwp_publication(
        &self,
        view: ViewId,
        publication: crate::print::PendingHwpPublication,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .hwp_pending
            .push((view, publication));
    }

    pub(crate) fn attach_hwp_receipt(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
        receipt: Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<(), String> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let (_, entry) = state
            .hwp_pending
            .iter_mut()
            .find(|(candidate, entry)| *candidate == view && Arc::ptr_eq(&entry.terminal, terminal))
            .ok_or("The HWP publication is no longer pending")?;
        if entry.receipt.is_some() {
            return Err("The HWP publication already has a pending receipt".into());
        }
        entry.receipt = Some(receipt);
        Ok(())
    }

    pub(crate) fn recover_hwp_delivery(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
    ) {
        let receipt = {
            let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            state
                .hwp_pending
                .iter()
                .find(|(candidate, entry)| {
                    *candidate == view
                        && Arc::ptr_eq(&entry.terminal, terminal)
                        && entry.payload.is_some()
                })
                .and_then(|(_, entry)| entry.receipt.clone())
        };
        let Some(receipt) = receipt else {
            return;
        };
        if terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
        {
            return;
        }
        match receipt.take_redelivery() {
            Ok(Some(delivery)) => {
                let mut delivery = Some(delivery);
                {
                    let mut slot = terminal.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.is_none() {
                        *slot = delivery.take().map(Ok);
                    }
                }
                drop(delivery);
            }
            Ok(None) => {}
            Err(error) => eprintln!("hwp: generated PDF redelivery failed: {error}"),
        }
    }

    pub(crate) fn take_ready_hwp_publication(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
    ) -> Option<(
        Option<crate::print::PendingHwpPayload>,
        Result<shell_contract::hosts::GeneratedTerminalDelivery, String>,
    )> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let (_, pending) = state.hwp_pending.iter_mut().find(|(candidate, pending)| {
            *candidate == view && Arc::ptr_eq(&pending.terminal, terminal)
        })?;
        if pending.settling {
            return None;
        }
        if let Some(delivery) = pending.delivery.take() {
            pending.settling = true;
            return Some((None, Ok(delivery)));
        }
        if pending.payload.is_none() {
            return None;
        }
        let delivery = {
            let mut slot = terminal.lock().unwrap_or_else(|error| error.into_inner());
            slot.take()?
        };
        let payload = pending.payload.take()?;
        pending.settling = true;
        Some((Some(payload), delivery))
    }

    pub(crate) fn finish_hwp_publication_ack(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
        retry: Option<shell_contract::hosts::GeneratedTerminalDelivery>,
    ) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = state.hwp_pending.iter().position(|(candidate, pending)| {
            *candidate == view && Arc::ptr_eq(&pending.terminal, terminal)
        }) {
            if let Some(delivery) = retry {
                state.hwp_pending[index].1.delivery = Some(delivery);
                state.hwp_pending[index].1.settling = false;
            } else {
                state.hwp_pending.remove(index);
            }
        }
    }

    /// A delivered cleanup that is still pending settles nothing: the payload
    /// goes back, the delivery waits for acknowledgement, and the pending
    /// operation becomes the receipt whose terminal result is awaited.
    pub(crate) fn retain_hwp_cleanup_pending(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
        payload: crate::print::PendingHwpPayload,
        delivery: shell_contract::hosts::GeneratedTerminalDelivery,
        pending: Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let displaced = match state.hwp_pending.iter_mut().find(|(candidate, entry)| {
            *candidate == view && Arc::ptr_eq(&entry.terminal, terminal)
        }) {
            Some((_, entry)) => {
                entry.payload = Some(payload);
                entry.cleanup.push(delivery);
                entry.settling = false;
                Ok((entry.receipt.replace(pending), entry.observer.take()))
            }
            None => Err((payload, delivery)),
        };
        drop(state);
        drop(displaced);
    }

    /// The newest delivery held for a pending cleanup, to acknowledge once the
    /// delivery that settled the original reply was acknowledged.
    pub(crate) fn next_hwp_cleanup_delivery(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
    ) -> Option<shell_contract::hosts::GeneratedTerminalDelivery> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let (_, entry) = state.hwp_pending.iter_mut().find(|(candidate, entry)| {
            *candidate == view && Arc::ptr_eq(&entry.terminal, terminal)
        })?;
        entry.cleanup.pop()
    }

    /// The pending cleanup whose observer registration failed and must be retried.
    pub(crate) fn unobserved_hwp_cleanup(
        &self,
        view: ViewId,
        terminal: &Arc<
            Mutex<Option<Result<shell_contract::hosts::GeneratedTerminalDelivery, String>>>,
        >,
    ) -> Option<Arc<dyn shell_contract::hosts::GeneratedPathPending>> {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let (_, entry) = state.hwp_pending.iter().find(|(candidate, entry)| {
            *candidate == view && Arc::ptr_eq(&entry.terminal, terminal)
        })?;
        if entry.cleanup.is_empty() || entry.observer.is_none() {
            return None;
        }
        entry.receipt.clone()
    }

    pub fn wake_hwp_publications(self: &Arc<Self>, core: Arc<dyn shell_contract::runtime::Thread>) {
        let entries = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .hwp_pending
            .iter()
            .map(|(view, pending)| (*view, pending.terminal.clone()))
            .collect::<Vec<_>>();
        for (view, terminal) in entries {
            crate::print::wake_hwp_publication(Arc::downgrade(self), view, terminal, core.clone());
        }
    }

    pub fn wake_hwp_publication(
        self: &Arc<Self>,
        view: ViewId,
        receipt: &Arc<dyn shell_contract::hosts::GeneratedPathPending>,
        core: Arc<dyn shell_contract::runtime::Thread>,
    ) {
        let terminal = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .hwp_pending
            .iter()
            .find(|(candidate, pending)| {
                *candidate == view
                    && pending
                        .receipt
                        .as_ref()
                        .is_some_and(|stored| Arc::ptr_eq(stored, receipt))
            })
            .map(|(_, pending)| pending.terminal.clone());
        if let Some(terminal) = terminal {
            crate::print::wake_hwp_publication(Arc::downgrade(self), view, terminal, core);
        }
    }

    pub(crate) fn retain_hwp_observer(
        &self,
        view: ViewId,
        receipt: &Arc<dyn shell_contract::hosts::GeneratedPathPending>,
        observer: (
            shell_contract::hosts::GeneratedPathObserver,
            shell_contract::hosts::GeneratedPathWake,
        ),
    ) -> Result<
        (),
        (
            shell_contract::hosts::GeneratedPathObserver,
            shell_contract::hosts::GeneratedPathWake,
        ),
    > {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((_, pending)) = state.hwp_pending.iter_mut().find(|(candidate, pending)| {
            *candidate == view
                && pending
                    .receipt
                    .as_ref()
                    .is_some_and(|stored| Arc::ptr_eq(stored, receipt))
        }) {
            pending.observer = Some(observer);
            Ok(())
        } else {
            Err(observer)
        }
    }

    /// Retry only an observer registration that explicitly returned its original
    /// callback. The saved-output continuation itself remains owned by W.
    pub fn retry_hwp_publication_observer(
        &self,
        view: ViewId,
        receipt: &Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<(), String> {
        let (observer, wake) = {
            let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let (_, pending) = state
                .hwp_pending
                .iter_mut()
                .find(|(candidate, pending)| {
                    *candidate == view
                        && pending
                            .receipt
                            .as_ref()
                            .is_some_and(|stored| Arc::ptr_eq(stored, receipt))
                })
                .ok_or("The HWP publication is no longer pending")?;
            pending
                .observer
                .take()
                .ok_or("The HWP observer is already installed or being registered")?
        };
        match receipt.observe(observer, wake) {
            Ok(()) => Ok(()),
            Err(failure) => {
                let error = failure.error;
                if self
                    .retain_hwp_observer(view, receipt, (failure.observer, failure.wake))
                    .is_err()
                {
                    return Err("The HWP publication ended during observer registration".into());
                }
                Err(error)
            }
        }
    }

    /// `currentLang()` with what it set in `@seuoffice/i18n`: settled once
    /// from `SEUOFFICE_LANG`, else the stored setting and `app.getLocale()`.
    /// The shell calls it before any tab loads, as index.ts did at start.
    pub fn language(&self, host: &dyn MHost, settings_path: &str) -> UiLanguage {
        if let Some(lang) = self.inner.lock().unwrap().lang.clone() {
            return lang;
        }
        let settled = match host.env_lang() {
            Some(env) => UiLanguage {
                lang: i18n::normalize_lang(Some(&env)),
                requested: env,
            },
            None => {
                let stored = read_app_settings(settings_path).get("language").cloned();
                let locale = host.app_locale();
                UiLanguage {
                    lang: i18n::install_lang(stored.as_ref(), Some(&locale)),
                    requested: i18n::requested_language(stored.as_ref(), Some(&locale)),
                }
            }
        };
        self.inner
            .lock()
            .unwrap()
            .lang
            .get_or_insert(settled)
            .clone()
    }

    /// `uiLang = lang; setUiLang(lang)` once the setting is on disk
    pub fn set_language(&self, lang: Lang) {
        self.inner.lock().unwrap().lang = Some(UiLanguage {
            lang,
            requested: lang.as_str().to_owned(),
        });
    }

    /// `currentTheme()`
    pub fn theme(&self, settings_path: &str) -> String {
        if let Some(theme) = self.inner.lock().unwrap().theme.clone() {
            return theme;
        }
        let theme = match read_app_settings(settings_path).get("theme").cloned() {
            Some(JsonValue::String(s)) if s == "light" || s == "dark" => s,
            _ => "system".to_owned(),
        };
        self.inner
            .lock()
            .unwrap()
            .theme
            .get_or_insert(theme)
            .clone()
    }

    pub fn set_theme(&self, theme: &str) {
        self.inner.lock().unwrap().theme = Some(theme.to_owned());
    }

    /// `currentDocTheme()`
    pub fn doc_theme(&self, settings_path: &str) -> String {
        if let Some(theme) = self.inner.lock().unwrap().doc_theme.clone() {
            return theme;
        }
        let theme = match read_app_settings(settings_path)
            .get("documentTheme")
            .cloned()
        {
            Some(JsonValue::String(s)) if s == "light" || s == "dark" => s,
            _ => "follow".to_owned(),
        };
        self.inner
            .lock()
            .unwrap()
            .doc_theme
            .get_or_insert(theme)
            .clone()
    }

    pub fn set_doc_theme(&self, theme: &str) {
        self.inner.lock().unwrap().doc_theme = Some(theme.to_owned());
    }

    /// `currentAutoSaveDefault()`. Read through the file each time: its
    /// setter (`home:set-auto-save-default`) is batch W's and writes the file
    /// before it broadcasts, so the file is the shared truth.
    pub fn auto_save_default(&self, settings_path: &str) -> (bool, f64) {
        let settings = read_app_settings(settings_path);
        let on = matches!(settings.get("autoSaveDefault"), Some(JsonValue::Bool(true)));
        let updated_at = match settings.get("autoSaveDefaultUpdatedAt") {
            Some(JsonValue::Number(n)) if *n > 0.0 => *n,
            _ => 0.0,
        };
        (on, updated_at)
    }

    pub fn arm_context_menu(&self, view: ViewId) {
        self.inner.lock().unwrap().armed.insert(view);
    }

    pub fn claim_context_menu(&self, view: ViewId, seq: f64, now: f64) {
        self.inner
            .lock()
            .unwrap()
            .claims
            .entry(view)
            .or_default()
            .claim(seq, now);
    }

    /// The interceptor's half: `Some(seq)` when the view armed the menu and
    /// holds a live claim (the renderer draws the menu), else `None` (the
    /// native menu shows)
    pub fn take_context_menu_claim(&self, view: ViewId, now: f64) -> Option<f64> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.armed.contains(&view) {
            return None;
        }
        inner.claims.get_mut(&view)?.take(now)
    }

    /// `ignore`: the word is held by `view`; true when it is new (the
    /// journal must be rewritten)
    pub fn spell_ignore(&self, word: &str, view: ViewId) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.spell_ignored.iter_mut().find(|(w, _)| w == word) {
            Some((_, holders)) => {
                if !holders.contains(&view) {
                    holders.push(view);
                }
                false
            }
            None => {
                inner.spell_ignored.push((word.to_owned(), vec![view]));
                true
            }
        }
    }

    /// `spellIgnored.delete(word)`
    pub fn spell_unignore(&self, word: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.spell_ignored.len();
        inner.spell_ignored.retain(|(w, _)| w != word);
        inner.spell_ignored.len() != before
    }

    pub fn spell_ignored_words(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .spell_ignored
            .iter()
            .map(|(w, _)| w.clone())
            .collect()
    }

    /// A view went away: forget its context-menu arming and claims, and
    /// release its ignored words (`releaseSpellIgnores`); returns the words
    /// no view holds any more, to take out of the dictionary
    pub fn view_destroyed(&self, view: ViewId) -> Vec<String> {
        let mut inner = self.inner.lock().unwrap();
        inner.armed.remove(&view);
        inner.claims.remove(&view);
        let mut released = Vec::new();
        inner.spell_ignored.retain_mut(|(word, holders)| {
            holders.retain(|v| *v != view);
            if holders.is_empty() {
                released.push(word.clone());
                false
            } else {
                true
            }
        });
        released
    }

    /// Marks `view` printing in the hwp or markdown set; false when it already was
    pub fn begin_print(&self, hwp: bool, view: ViewId) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let set = if hwp {
            &mut inner.hwp_printing
        } else {
            &mut inner.markdown_printing
        };
        set.insert(view)
    }

    pub fn end_print(&self, hwp: bool, view: ViewId) {
        let mut inner = self.inner.lock().unwrap();
        let set = if hwp {
            &mut inner.hwp_printing
        } else {
            &mut inner.markdown_printing
        };
        set.remove(&view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_taken_oldest_first_and_expire_after_two_seconds() {
        let mut claims = ClickClaims::default();
        claims.claim(1.0, 0.0);
        claims.claim(2.0, 1500.0);
        assert_eq!(claims.take(2000.0), Some(1.0));
        // 2.0 was claimed at 1500: alive at 3500, gone at 3501
        claims.claim(3.0, 3400.0);
        assert_eq!(claims.take(3501.0), Some(3.0));
        assert_eq!(claims.take(3501.0), None);
    }

    #[test]
    fn a_claim_without_arming_shows_the_native_menu() {
        let state = MState::new();
        state.claim_context_menu(ViewId(1), 7.0, 0.0);
        assert_eq!(state.take_context_menu_claim(ViewId(1), 10.0), None);
        state.arm_context_menu(ViewId(1));
        state.claim_context_menu(ViewId(1), 8.0, 20.0);
        assert_eq!(state.take_context_menu_claim(ViewId(1), 30.0), Some(7.0));
    }

    #[test]
    fn an_ignored_word_is_released_when_its_last_view_closes() {
        let state = MState::new();
        assert!(state.spell_ignore("foo", ViewId(1)));
        assert!(!state.spell_ignore("foo", ViewId(2)));
        assert!(state.view_destroyed(ViewId(1)).is_empty());
        assert_eq!(state.view_destroyed(ViewId(2)), vec!["foo".to_owned()]);
        assert!(state.spell_ignored_words().is_empty());
    }
}
