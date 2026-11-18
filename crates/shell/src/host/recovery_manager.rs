//! Recovery's core-thread adapter over the shell tab manager.

use std::rc::Rc;
use std::sync::Arc;

use main_core::shell::recovery_shell::{LaunchManager, ListedTab, RecoveryContents, UnloadedTabSpec};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::tab_manager::{TabKind,UnloadedTabSpec as ManagerSpec};
use shell_contract::{ViewId, WindowId};
use shell_ipc_w::tabs::TabsState;

use super::recovery_openers::{self, NeedsUser, Open};
use crate::views::ViewTable;

#[derive(Clone)]
pub(crate) struct SharedStore {
    pub shared: Arc<std::sync::Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>>,
    pub user_data: String,
}

impl SharedStore {
    pub(crate) fn with_store<R>(&self, f: impl FnOnce(&main_core::packages::electron_utils::recovery_store::RecoveryStore<'_>) -> R) -> R {
        use main_core::packages::electron_utils::recovery_store::{create_recovery_store, RecoveryStoreOptions};
        let shared = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = || self.user_data.clone();
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_millis() as f64);
        let uuid = || jscompat::random_uuid();
        let store = create_recovery_store(RecoveryStoreOptions {
            user_data_dir: &dir, now_ms: &now, random_uuid: &uuid,
        }, &shared);
        f(&store)
    }
}

impl main_core::shell::recovery_shell::LaunchRecoveryStore for SharedStore {
    fn prune_stale_copies(&self) {
        self.with_store(|store| { store.prune_stale_copies(None); });
    }

    fn has_unresolved_copies(&self) -> Result<bool, Failure> {
        self.with_store(|store| store.has_unresolved_copies().map_err(|error| Failure(error.message)))
    }

    fn plan(&self, judgment: &main_core::shell::launch_recovery::LaunchJudgment,
        previous: &main_core::shell::session_record::SessionRecordReadResult,
        paths: &[String]) -> Result<main_core::shell::launch_recovery::LaunchPlan, Failure> {
        self.with_store(|store| main_core::shell::launch_recovery::plan_launch(judgment, previous, store, paths))
    }

    fn apply_decision(&self, identity: &main_core::packages::electron_utils::recovery_store::RecoveryIdentity,
        decision: main_core::packages::electron_utils::recovery_store::RecoveryLaunchDecision) -> Result<(), Failure> {
        self.with_store(|store| store.apply_launch_decision(identity, decision)
            .map(|_| ()).map_err(|error| Failure(error.message)))
    }
}

pub(crate) struct SessionEnv;

impl main_core::shell::session_record::SessionRecorderEnv for SessionEnv {
    fn now(&mut self) -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
    }

    fn write_json(&mut self, path: &str, record: &main_core::shell::session_record::SessionRecord) -> Result<(), String> {
        main_core::shell::session_record::write_record_atomic(path, record)
            .map_err(|error| error.message)
    }

    fn warn(&mut self, label: &str, error: &str) {
        eprintln!("{label} {error}");
    }
}

#[derive(Clone)]
pub(crate) struct Contents {
    pub view: ViewId,
    pub views: Arc<ViewTable>,
}

impl RecoveryContents for Contents {
    fn id(&self) -> i64 { i64::from(self.view.0) }
    fn is_destroyed(&self) -> bool { self.views.label_of_view(self.view).is_none() }
}

#[derive(Clone)]
pub(crate) struct Manager {
    pub tabs: Rc<TabsState>,
    pub views: Arc<ViewTable>,
    pub owner: WindowId,
    pub open: Rc<Open>,
    pub needs_user: Rc<NeedsUser>,
    pub issuer: super::recovery_launch::RecoveryIssuer,
}

impl Manager {
    pub(crate) fn spec(&self, spec: UnloadedTabSpec) -> Result<ManagerSpec, Failure> {
        let kind = TabKind::parse(&spec.kind).filter(|kind| !matches!(kind, TabKind::Home | TabKind::Help))
            .ok_or_else(|| Failure(format!("unknown recovery editor kind {}", spec.kind)))?;
        let plan = super::recovery_launch::issuer::RecoveryPlan::capture(&self.issuer,&spec).map_err(Failure)?;
        let opener = recovery_openers::register(self.owner, kind, spec.clone(), plan, self.open.clone(), self.needs_user.clone())
            .map_err(Failure)?;
        Ok(ManagerSpec { kind, title: Some(spec.title), file_path: spec.file_path, opener })
    }
}

impl LaunchManager for Manager {
    type Contents = Contents;


    fn list(&self) -> Result<Vec<ListedTab>, Failure> {
        self.tabs.with_manager(|manager, _| manager.list().into_iter().map(|tab| ListedTab {
            id: tab.id, kind: tab.kind.as_str().to_owned(), title: tab.title, file_path: tab.file_path,
        }).collect()).ok_or_else(|| Failure("shell window is unavailable".into()))
    }


    fn split_state(&self) -> Result<bool, Failure> {
        self.tabs.with_manager(|manager, _| manager.split_state().is_some())
            .ok_or_else(|| Failure("shell window is unavailable".into()))
    }


    fn web_contents_for_tab(&self, id: &str) -> Option<Contents> {
        let view = self.tabs.with_manager(|manager, _| manager.web_contents_for_tab(id)).flatten()?;
        Some(Contents { view, views: self.views.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
    use main_core::shell::recovery_shell::LaunchRecoveryStore;

    #[test]
    fn store_query_releases_lock_before_a_reentrant_editor_callback() {
        let shared = Arc::new(std::sync::Mutex::new(SharedRecoveryState::new("test")));
        let store = SharedStore {
            shared: shared.clone(),
            user_data: "/nonexistent-seuoffice-recovery-test".to_owned(),
        };
        let _ = store.has_unresolved_copies();
        let editor_callback = || {
            assert!(shared.try_lock().is_ok());
            let _ = store.has_unresolved_copies();
        };
        editor_callback();
    }
}
