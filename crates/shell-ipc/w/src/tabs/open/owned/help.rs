//! `openHelpTab(topic)`: the manual's tab, created through the host's asynchronous creation hook
//! and shown under the caller's authority. `done` runs once, after the tab is shown or after
//! the failure and any creation cleanup settled.
use super::TabsState;
use crate::tabs::{
    activation::{TabActivationAuthority, TabUiDone},
    host::{
        NativeBrowserBinding, RetainedSpareBinding, ViewCreationCleanup, ViewCreationCleanupState,
        ViewCreationFailure,
    },
};
use main_core::shell::tab_manager::{HelpOpenCommit, PreparedHelpOpen, TabError, TabKind};
use shell_contract::{
    ViewId,
    ipc::{
        NativeEditorCommand, NativeInvocationDocument, NativeInvocationOperation,
        NativeInvocationSelection, NativeRequestOwner,
    },
    runtime::CancelToken,
};
use std::rc::Rc;

type HelpDone = Box<dyn FnOnce(Result<ViewId, TabError>)>;
type HelpCreated = Result<HelpView, ViewCreationFailure>;
type HelpCreate = Box<dyn FnOnce(Box<dyn FnOnce(HelpCreated)>)>;

struct HelpView {
    view: ViewId,
    cleanup: ViewCreationCleanup,
    binding: NativeBrowserBinding,
    document: Option<NativeInvocationDocument>,
}

enum Committed {
    Created(String),
    Existing {
        id: String,
        view: Option<ViewId>,
        navigate: Option<String>,
    },
}

/// What admits the open and shows the tab.
#[derive(Clone)]
enum HelpAuthority {
    Owned {
        origin: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
    },
    Native {
        operation: NativeInvocationOperation,
        expected: NativeEditorCommand,
    },
}

impl HelpAuthority {
    fn cancel(&self) -> CancelToken {
        match self {
            Self::Owned { cancel, .. } => cancel.clone(),
            Self::Native { operation, .. } => operation.cancel().clone(),
        }
    }

    fn pending(&self) -> CancelToken {
        match self {
            Self::Owned { pending, .. } => pending.clone(),
            Self::Native { operation, .. } => operation.cancel().clone(),
        }
    }
}

impl TabsState {
    /// `openHelpTab(topic)` for a renderer request owned by `origin`.
    pub fn open_help_owned(
        &self,
        topic: Option<String>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: HelpDone,
    ) {
        self.open_help(
            topic,
            HelpAuthority::Owned {
                origin,
                cancel,
                pending,
            },
            done,
        );
    }

    /// `openHelpTab(topic)` for the selected native `OpenHelp` command.
    pub fn open_help_native(
        &self,
        topic: Option<String>,
        operation: NativeInvocationOperation,
        done: HelpDone,
    ) {
        let expected = NativeEditorCommand::OpenHelp {
            topic: topic.clone(),
        };
        if !operation.is_root_native_command()
            || operation.selection() != &NativeInvocationSelection::NativeCommand(expected.clone())
        {
            return done(Err(TabError(
                "The native help command does not match its selection".into(),
            )));
        }
        self.open_help(topic, HelpAuthority::Native { operation, expected }, done);
    }

    fn admit_help(&self, authority: &HelpAuthority) -> Result<(), String> {
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Help effects are unavailable")?;
        match authority {
            HelpAuthority::Owned {
                origin,
                cancel,
                pending,
            } => effects.with_owned_effect_many(origin, &[], cancel, pending, &mut || Ok(())),
            HelpAuthority::Native {
                operation,
                expected,
            } => {
                let cancel = operation.cancel().clone();
                effects.with_native_command_preparation_effect(
                    operation,
                    expected,
                    &[],
                    &cancel,
                    &cancel,
                    &mut || Ok(()),
                )
            }
        }
    }

    fn open_help(&self, topic: Option<String>, authority: HelpAuthority, done: HelpDone) {
        if let Err(error) = self.admit_help(&authority) {
            return done(Err(TabError(error)));
        }
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => return done(Err(TabError(error))),
        };
        if let Err(error) = self.begin_creation(id) {
            return done(Err(TabError(error)));
        }
        let finished = self.rc();
        let done: HelpDone = Box::new(move |result| {
            finished.finish_creation(id);
            done(result);
        });
        let epoch = self.manager_epoch.get();
        let plan = self.with_manager(|manager, host| {
            let dev_url = host.renderer_dev_url();
            manager.prepare_help_open_pure(dev_url.as_deref(), topic.as_deref())
        });
        let plan = match plan {
            Some(Ok(plan)) => plan,
            Some(Err(error)) => return done(Err(error)),
            None => {
                return done(Err(TabError("the shell window is not available".into())));
            }
        };
        match plan {
            Err((tab, view, navigate)) => match view {
                Some(view) => self.show_help(tab, view, navigate, None, authority, done),
                None => done(Err(TabError("The help tab has no loaded view".into()))),
            },
            Ok(plan) => {
                let state = self.rc();
                let create = self.help_creator(&authority);
                create(Box::new(move |created| {
                    state.help_created(plan, authority, epoch, done, created);
                }));
            }
        }
    }

    fn help_creator(&self, authority: &HelpAuthority) -> HelpCreate {
        let hooks = self.hooks.clone();
        match authority {
            HelpAuthority::Owned { .. } => Box::new(move |then| {
                hooks.create_help_view(Box::new(move |result| {
                    then(result.map(|ready| HelpView {
                        view: ready.view,
                        cleanup: ready.cleanup,
                        binding: ready.reservation,
                        document: None,
                    }));
                }));
            }),
            HelpAuthority::Native { operation, .. } => {
                let operation = operation.clone();
                Box::new(move |then| {
                    let cancel = operation.cancel().clone();
                    hooks.create_help_native_command(
                        operation,
                        Vec::new(),
                        cancel.clone(),
                        cancel,
                        Box::new(move |result| {
                            then(result.map(|ready| HelpView {
                                view: ready.view.owner().view,
                                cleanup: ready.cleanup,
                                binding: ready.reservation,
                                document: Some(ready.view),
                            }));
                        }),
                    );
                })
            }
        }
    }

    fn help_created(
        self: Rc<Self>,
        plan: PreparedHelpOpen,
        authority: HelpAuthority,
        epoch: u64,
        done: HelpDone,
        created: HelpCreated,
    ) {
        let HelpView {
            view,
            cleanup,
            binding,
            document,
        } = match created {
            Ok(created) => created,
            Err(ViewCreationFailure { error, cleanup }) => {
                let (cleanup, proof) = match cleanup {
                    ViewCreationCleanupState::Complete => (None, None),
                    ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof)),
                    ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup), None),
                };
                let original = error.0.clone();
                let failed = self.clone();
                self.retain_editor_cleanup(
                    cleanup,
                    proof,
                    Rc::default(),
                    true,
                    Box::new(move |failure| {
                        failed.hooks.warn(
                            "[shell] help view cleanup failed:",
                            &format!("{original}; {failure}"),
                        );
                    }),
                    Box::new(move || done(Err(error))),
                );
                return;
            }
        };
        if epoch != self.manager_epoch.get() || authority.cancel().is_cancelled() {
            return self.discard_help_view(
                cleanup,
                Box::new(move || {
                    done(Err(TabError(
                        "The help request is no longer active".into(),
                    )));
                }),
            );
        }
        let binding = RetainedSpareBinding::new(binding, Rc::default(), None);
        let loaded_by_host = document.is_some();
        let committed = self.with_manager(|manager, host| {
            let title = host
                .untitled_title_for(TabKind::Help)
                .unwrap_or_else(|| format!("{} Help", main_core::packages::i18n::PRODUCT_NAME));
            match manager.commit_help_open_bound(plan, view, title, binding) {
                HelpOpenCommit::Created { id, view, url } => {
                    if !loaded_by_host {
                        host.load_url(&view, &url);
                    }
                    host.add_child_view(&view);
                    host.set_visible(&view, false);
                    Committed::Created(id)
                }
                HelpOpenCommit::Existing {
                    id,
                    view,
                    navigate,
                    ..
                } => Committed::Existing { id, view, navigate },
            }
        });
        match committed {
            Some(Committed::Created(tab)) => {
                self.show_help(tab, view, None, document, authority, done)
            }
            Some(Committed::Existing { id, view: existing, navigate }) => {
                // Another help tab appeared while this view was being made.
                let state = self.clone();
                self.discard_help_view(
                    cleanup,
                    Box::new(move || match existing {
                        Some(existing) => {
                            state.show_help(id, existing, navigate, None, authority, done)
                        }
                        None => done(Err(TabError("The help tab has no loaded view".into()))),
                    }),
                );
            }
            None => self.discard_help_view(
                cleanup,
                Box::new(move || {
                    done(Err(TabError(
                        "The shell window is no longer available".into(),
                    )));
                }),
            ),
        }
    }

    /// Closes a help view that did not become a tab; `then` runs once its cleanup settled.
    fn discard_help_view(&self, cleanup: ViewCreationCleanup, then: Box<dyn FnOnce()>) {
        let failed = self.rc();
        self.retain_editor_cleanup(
            Some(cleanup),
            None,
            Rc::default(),
            true,
            Box::new(move |failure| {
                failed
                    .hooks
                    .warn("[shell] unused help view cleanup failed:", failure);
            }),
            then,
        );
    }

    fn capture_help_owner(&self, view: ViewId) -> Result<NativeRequestOwner, String> {
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Help effects are unavailable")?;
        let generation = self
            .services
            .views
            .renderer_generation(view)
            .ok_or("The help renderer is unavailable")?;
        effects.capture_owner(view, &generation)
    }

    /// Activates the help tab under the request's authority; an existing tab is first sent to
    /// the requested topic's page.
    fn show_help(
        &self,
        tab: String,
        view: ViewId,
        navigate: Option<String>,
        document: Option<NativeInvocationDocument>,
        authority: HelpAuthority,
        done: HelpDone,
    ) {
        if let Some(url) = navigate {
            self.with_manager(|_, host| host.load_url(&view, &url));
        }
        let target = match &document {
            Some(document) => document.owner().clone(),
            None => match self.capture_help_owner(view) {
                Ok(owner) => owner,
                Err(error) => return done(Err(TabError(error))),
            },
        };
        let cancel = authority.cancel();
        let pending = authority.pending();
        let shown: TabUiDone = Box::new(move |result| {
            done(result.map(|()| view).map_err(TabError));
        });
        match authority {
            HelpAuthority::Owned { origin, .. } => self.activate_loaded_owned_many(
                &tab,
                origin,
                target,
                Vec::new(),
                cancel,
                pending,
                shown,
            ),
            HelpAuthority::Native {
                operation,
                expected,
            } => {
                let authority = match document {
                    Some(document) => TabActivationAuthority::NativeCommandTarget {
                        document,
                        expected,
                        owner: target.clone(),
                    },
                    None => TabActivationAuthority::NativeCommand {
                        operation,
                        expected,
                        owner: target.clone(),
                    },
                };
                self.activate_loaded_authorized(
                    &tab,
                    authority,
                    target,
                    Vec::new(),
                    cancel,
                    pending,
                    shown,
                    None,
                    None,
                );
            }
        }
    }
}
