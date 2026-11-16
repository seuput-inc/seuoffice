//! `ai-settings.json` as docs-main reads and writes it for the free
//! connections (`storedAiSettings`, `writeAiSettingsFile`,
//! `writeRendererAiSettings`, `broadcastAiSettingsChanged`): keys and tokens go
//! to disk sealed with the shell's codec, and fields a newer build wrote stay
//! as they are. The shared AI port keeps the same logic private to its crate.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use jscompat::{JsObject, JsonValue, json_parse};
use main_core::packages::ai_provider::ai_settings_guard::{GuardHost, sanitize_ai_settings};
use main_core::packages::ai_provider::key_model_refresh::{
    KeyModelRefresher, KeyModelRefresherDeps, SettingsUpdate, create_key_model_refresher,
};
use main_core::packages::ai_provider::legacy_settings::normalize_ai_settings;
use main_core::packages::ai_provider::providers::with_migrated_subscription_notice_version;
use main_core::packages::ai_provider::settings_secrets::seal_ai_secrets;
use main_core::packages::ai_provider::stored_settings::{
    with_stored_main_fields, with_stored_unknown_fields,
};
use main_core::packages::ai_provider::task::JsError;
use main_core::packages::electron_utils::atomic_write::write_json_atomic;
use shell_contract::ipc::Target;
use shell_ipc_a::runtime::Driver;

use super::codec;

/// What `sanitizeAiSettings` checks a path against: the home folder and the files on disk
struct FsGuard(String);

impl GuardHost for FsGuard {
    fn home_dir(&self) -> String {
        self.0.clone()
    }

    fn is_file(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }
}

#[derive(Clone)]
pub struct SettingsFile {
    path: PathBuf,
    home: String,
}

impl SettingsFile {
    pub fn new(user_data: PathBuf, home: PathBuf) -> SettingsFile {
        SettingsFile {
            path: user_data.join("ai-settings.json"),
            home: home.to_string_lossy().into_owned(),
        }
    }

    /// `readJson(SETTINGS_PATH(), {})`: a missing or damaged file is `{}`
    fn read_json(&self) -> JsonValue {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| json_parse(&String::from_utf8_lossy(&bytes)).ok())
            .unwrap_or_else(|| JsonValue::Object(JsObject::new()))
    }

    /// `writeAiSettingsFile(settings)`; the shell has no headless export
    fn write(&self, settings: &JsonValue) -> Result<(), String> {
        let merged = with_stored_unknown_fields(settings, &self.read_json());
        write_json_atomic(
            &self.path.to_string_lossy(),
            &seal_ai_secrets(codec(), &merged),
        )
        .map_err(|e| e.message)
    }

    /// `storedAiSettings()`: a file written before the notice carried a
    /// version is migrated on disk; removed and unknown selections read as
    /// their defaults
    pub fn stored(&self) -> Result<JsonValue, String> {
        let raw = self.read_json();
        let raw = match with_migrated_subscription_notice_version(&raw) {
            Some(next) => {
                self.write(&next)?;
                next
            }
            None => raw,
        };
        Ok(normalize_ai_settings(codec(), Some(&raw), None))
    }

    /// `writeRendererAiSettings(settings)`: persist what a renderer sent,
    /// schema-checked, with the live model lists kept from the file, then every
    /// renderer re-reads it and a key that changed is listed again. False when
    /// the payload is not settings at all.
    pub fn write_renderer(&self, settings: &JsonValue) -> Result<bool, String> {
        let sanitized =
            sanitize_ai_settings(&FsGuard(self.home.clone()), settings).map_err(|error| error.0)?;
        let Some(sanitized) = sanitized else {
            return Ok(false);
        };
        let stored = self.stored()?;
        self.write(&with_stored_main_fields(&sanitized, &stored))?;
        broadcast_changed();
        refresh_key_models(self.clone());
        Ok(true)
    }

    /// The `write` of docs-main's key model refresher: `update` is applied to
    /// the file as it is now, and nothing is written when it answers `None`
    fn write_listed(&self, update: SettingsUpdate<'_>) -> Result<(), JsError> {
        let latest = self.stored().map_err(JsError::error)?;
        let Some(next) = update(&latest)? else {
            return Ok(());
        };
        self.write(&next).map_err(JsError::error)?;
        broadcast_changed();
        Ok(())
    }
}

/// `broadcastAiSettingsChanged()`: on the core thread, after the call that wrote
fn broadcast_changed() {
    crate::host::w::post_core(|_, services| {
        services
            .events
            .emit(Target::All, "ai:settings-changed", Vec::new());
    });
}

thread_local! {
    /// `keyModelRefresher` of the free connections' writes, on the core thread.
    /// It keeps its own record of what each key was listed with, so the first
    /// write after the shared AI port's startup listing asks every key once more.
    static REFRESHER: RefCell<Option<(Driver, Rc<KeyModelRefresher>)>> = const { RefCell::new(None) };
}

/// `keyModelRefresher.refreshChanged()`: on the core thread, after the write
fn refresh_key_models(file: SettingsFile) {
    crate::host::w::post_core(move |provided, _| {
        let Some((driver, refresher)) = REFRESHER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                let driver = provided
                    .all::<Driver>()
                    .first()
                    .map(|driver| driver.as_ref().clone())?;
                let read = file.clone();
                let refresher = create_key_model_refresher(
                    &driver.rt(),
                    &driver.providers().fx,
                    KeyModelRefresherDeps {
                        read: Rc::new(move || read.stored().map_err(JsError::error)),
                        write: Rc::new(move |update| file.write_listed(update)),
                        list: None,
                    },
                );
                *slot = Some((driver, Rc::new(refresher)));
            }
            slot.clone()
        }) else {
            return;
        };
        let _ = refresher.refresh_changed();
        driver.run(async {}, |()| {});
    });
}
