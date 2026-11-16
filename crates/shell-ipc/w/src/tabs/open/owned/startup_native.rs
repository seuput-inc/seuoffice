//! Startup-launch tab effects that run under the captured launch operation: closing a
//! recovered tab, activating one, the recovered split, and the remembered split or Home.
use super::{TabsState, document_kind, editor_request_for_path};
use crate::tabs::activation::{TabActivationAuthority, TabUiDone};
use main_core::shell::{
    split_layout::{Pane, SplitOrientation},
    split_memory::{
        RememberedSplitPlan, SettingsPath, SplitMemoryOptions, plan_remembered_split,
        read_split_memory,
    },
};
use shell_contract::{
    ViewId,
    hosts::OwnedEffectsHost,
    ipc::{NativeInvocationOperation, NativeRequestOwner},
    runtime::CancelToken,
};

type Found = Box<dyn FnOnce(Result<Option<String>, String>)>;

impl TabsState {
    fn startup_effects(&self) -> Result<OwnedEffectsHost, String> {
        self.services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| "Owned startup is unavailable".into())
    }

    fn capture_startup_tab_owner(&self, id: &str) -> Result<NativeRequestOwner, String> {
        let view = self
            .tab_contents(id)
            .ok_or("The startup tab has no loaded view")?;
        let generation = self
            .services
            .views
            .renderer_generation(view)
            .ok_or("The startup tab renderer is unavailable")?;
        self.startup_effects()?.capture_owner(view, &generation)
    }

    /// Closes a recovered tab once the launch operation, its owner and both tokens still admit it.
    #[allow(clippy::too_many_arguments)]
    pub fn close_tab_startup(
        &self,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        id: String,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let effects = match self.startup_effects() {
            Ok(effects) => effects,
            Err(error) => return done(Err(error)),
        };
        if let Err(error) = effects.with_native_operation_effect(
            &startup,
            &owner,
            &[],
            &cancel,
            &pending,
            &mut || Ok(()),
        ) {
            return done(Err(error));
        }
        self.close_tab(&id, done);
    }

    /// Shows a loaded recovered tab under the launch operation.
    pub fn activate_startup_owned(
        &self,
        id: String,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let target = match self.capture_startup_tab_owner(&id) {
            Ok(target) => target,
            Err(error) => return done(Err(error)),
        };
        self.activate_loaded_native_operation_many(
            &id,
            startup,
            owner,
            target,
            Vec::new(),
            cancel,
            pending,
            done,
        );
    }

    /// Restores a recovered split; `orientation` is `side` or `stack`, `focus` is `a` or `b`.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_split_startup(
        &self,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        a: String,
        b: String,
        orientation: String,
        ratio: f64,
        focus: String,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let orientation = match orientation.as_str() {
            "side" => SplitOrientation::Side,
            "stack" => SplitOrientation::Stack,
            other => return done(Err(format!("Unknown split orientation: {other}"))),
        };
        let focus = match focus.as_str() {
            "a" => Pane::A,
            "b" => Pane::B,
            other => return done(Err(format!("Unknown split focus: {other}"))),
        };
        self.begin_split_loaded(
            startup,
            owner,
            (a, b),
            orientation,
            ratio,
            focus,
            cancel,
            pending,
            done,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_split_loaded(
        &self,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        (a, b): (String, String),
        orientation: SplitOrientation,
        ratio: f64,
        focus: Pane,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let a_owner = match self.capture_startup_tab_owner(&a) {
            Ok(owner) => owner,
            Err(error) => return done(Err(error)),
        };
        let b_owner = match self.capture_startup_tab_owner(&b) {
            Ok(owner) => owner,
            Err(error) => return done(Err(error)),
        };
        self.begin_loaded_split_startup(
            startup,
            owner,
            (a, a_owner),
            (b, b_owner),
            orientation,
            ratio,
            focus,
            cancel,
            pending,
            done,
        );
    }

    /// `restoreSplitMemoryOrHome`: reopens the remembered one or two files and splits them, or
    /// shows Home when none of them is on disk or none could be opened. The memory stays as
    /// it is; the next split change rewrites it.
    pub fn restore_split_or_home_startup(
        &self,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let options = SplitMemoryOptions {
            settings_path: SettingsPath::Fixed(self.inner.borrow().bridge.settings_path.clone()),
            read_settings: None,
            write_setting: None,
        };
        let plan = plan_remembered_split(
            read_split_memory(&options),
            |path| std::path::Path::new(path).exists(),
            |path| {
                std::fs::canonicalize(path)
                    .map(|resolved| resolved.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| path.to_owned())
            },
        );
        let home = {
            let (startup, owner) = (startup.clone(), owner.clone());
            let (cancel, pending) = (cancel.clone(), pending.clone());
            move |state: &std::rc::Rc<TabsState>, done: TabUiDone| {
                state.startup_open_home(startup, owner, cancel, pending, done)
            }
        };
        let state = self.rc();
        match plan {
            RememberedSplitPlan::Home => home(&state, done),
            RememberedSplitPlan::Single(path) => {
                let fallback = state.clone();
                self.open_remembered(
                    path,
                    startup,
                    owner,
                    cancel,
                    pending,
                    Box::new(move |result| match result {
                        Ok(Some(_)) => done(Ok(())),
                        Ok(None) => home(&fallback, done),
                        Err(error) => done(Err(error)),
                    }),
                );
            }
            RememberedSplitPlan::Pair {
                a,
                b,
                duplicate,
                orientation,
                ratio,
                focus,
            } => {
                let split_startup = startup.clone();
                let split_owner = owner.clone();
                let split_cancel = cancel.clone();
                let split_pending = pending.clone();
                let second = state.clone();
                let split = state.clone();
                let fallback = state.clone();
                let (b_startup, b_owner, b_cancel, b_pending) = (
                    startup.clone(),
                    owner.clone(),
                    cancel.clone(),
                    pending.clone(),
                );
                self.open_remembered(
                    a.clone(),
                    startup,
                    owner,
                    cancel,
                    pending,
                    Box::new(move |first| {
                        let a_id = match first {
                            Ok(Some(id)) => id,
                            Ok(None) if duplicate => return home(&fallback, done),
                            Ok(None) => {
                                // Only the second pane can still come back.
                                return second.open_remembered(
                                    b,
                                    b_startup,
                                    b_owner,
                                    b_cancel,
                                    b_pending,
                                    Box::new(move |result| match result {
                                        Ok(Some(_)) => done(Ok(())),
                                        Ok(None) => home(&fallback, done),
                                        Err(error) => done(Err(error)),
                                    }),
                                );
                            }
                            Err(error) => return done(Err(error)),
                        };
                        let finish = move |b_id: String, done: TabUiDone| {
                            split.begin_split_loaded(
                                split_startup,
                                split_owner,
                                (a_id, b_id),
                                orientation,
                                ratio,
                                focus,
                                split_cancel,
                                split_pending,
                                done,
                            )
                        };
                        if duplicate {
                            second.open_duplicate_pane_startup(
                                &a,
                                b_startup,
                                b_owner,
                                b_cancel,
                                b_pending,
                                Box::new(move |result| match result {
                                    Ok(id) => finish(id, done),
                                    Err(error) => done(Err(error)),
                                }),
                            );
                        } else {
                            second.open_remembered(
                                b,
                                b_startup,
                                b_owner,
                                b_cancel,
                                b_pending,
                                Box::new(move |result| match result {
                                    Ok(Some(b_id)) => finish(b_id, done),
                                    // The first pane is already open; one pane is a valid outcome.
                                    Ok(None) => done(Ok(())),
                                    Err(error) => done(Err(error)),
                                }),
                            );
                        }
                    }),
                );
            }
        }
    }

    /// Routes one remembered path and reports the id of the tab that shows it.
    fn open_remembered(
        &self,
        path: String,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Found,
    ) {
        let state = self.rc();
        let wanted = path.clone();
        self.route_document_path_authorized(
            path,
            TabActivationAuthority::NativeOperation {
                operation: startup,
                owner,
            },
            cancel,
            pending,
            Box::new(move |result| match result {
                Ok(_) => {
                    let id = state
                        .with_manager(|manager, host| {
                            manager
                                .find_tab_by_path(host, Some(&wanted))
                                .map(|tab| tab.id)
                        })
                        .flatten();
                    done(Ok(id));
                }
                Err(error) => done(Err(error)),
            }),
        );
    }

    /// A second tab on the same file for the same-document split.
    fn open_duplicate_pane_startup(
        &self,
        path: &str,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<String, String>)>,
    ) {
        let Some(request) =
            document_kind(path).and_then(|kind| editor_request_for_path(kind, path))
        else {
            return done(Err("The remembered document cannot be duplicated".into()));
        };
        let effects = match self.startup_effects() {
            Ok(effects) => effects,
            Err(error) => return done(Err(error)),
        };
        if let Err(error) = effects.with_native_operation_effect(
            &startup,
            &owner,
            &[],
            &cancel,
            &pending,
            &mut || Ok(()),
        ) {
            return done(Err(error));
        }
        self.open_editor_owned(
            request,
            owner,
            cancel,
            Box::new(move |result| done(result.map(|(id, _, _): (String, ViewId, _)| id))),
        );
    }
}
