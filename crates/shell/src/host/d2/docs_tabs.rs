//! Word editor hooks into the shell's live tabs.

use crate::host::{HostContext, w::on_core};
use shell_contract::ViewId;
use shell_contract::runtime::Views;
use shell_ipc_w::tabs::TabsState;

pub(super) fn active(cx: &HostContext) -> Option<ViewId> {
    super::ACTIVE_DOCS
        .with(|active| active.get())
        .and_then(|(view, null_if_destroyed)| {
            (!null_if_destroyed || cx.views.sender_of(view).is_some()).then_some(view)
        })
}

pub(super) fn file_saved(view: ViewId, path: &str) -> Option<String> {
    let path = path.to_owned();
    on_core(move |provided| {
        provided
            .all::<TabsState>()
            .into_iter()
            .next()
            .and_then(|tabs| tabs.docs_file_saved(view, &path))
    })
    .flatten()
}

pub(super) fn file_opened(view: ViewId, path: &str) {
    let path = path.to_owned();
    on_core(move |provided| {
        if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
            tabs.docs_file_opened(view, &path);
        }
    });
}

type OpenDone = Box<dyn FnOnce(Result<bool, String>) + Send>;

/// `openDocumentPath` (Drive copies reopen through Drive, else the native `OpenFile` route);
/// `then` runs on the core thread once the open settles: `Ok(true)` opened or focused,
/// `Ok(false)` nothing opened, `Err` refused or failed.
pub(crate) fn open_path_native(path: String, then: OpenDone) {
    open_with(path, true, then);
}

/// `routeDocumentPath`: the native `OpenFile` route without the Drive check, for Drive's own
/// reopen of its local copy.
pub(crate) fn route_path_native(path: String, then: OpenDone) {
    open_with(path, false, then);
}

fn open_with(path: String, drive: bool, then: OpenDone) {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(then)));
    let unavailable = slot.clone();
    let take = |slot: &std::sync::Mutex<Option<OpenDone>>| {
        slot.lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    };
    crate::host::w::post_core_checked(
        move |provided, _| {
            let Some(then) = take(&slot) else { return };
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                return then(Err("The tab manager is not available".into()));
            };
            if drive {
                tabs.open_document_path(&path, then);
            } else {
                tabs.route_document_path(&path, then);
            }
        },
        move || {
            if let Some(then) = take(&unavailable) {
                then(Err("Core services are not available".into()));
            }
        },
    );
}

/// `openGeneratedFile`: opened in a tab, else (or on failure) revealed in its folder. The
/// caller's fallback is already handled here, so `Ok(true)` means the host took over.
pub(crate) fn open_generated(path: &str) -> Result<bool, String> {
    let reveal = std::path::PathBuf::from(path);
    open_path_native(
        path.to_owned(),
        Box::new(move |result| {
            match result {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => eprintln!(
                    "shell: opening generated file {}: {error}",
                    reveal.display()
                ),
            }
            if let Err(error) = crate::host::os_shell::show_item_in_folder(&reveal) {
                eprintln!(
                    "shell: revealing generated file {}: {error}",
                    reveal.display()
                );
            }
        }),
    );
    Ok(true)
}

#[derive(Clone)]
pub(crate) struct DocsAiSeedRollback(std::rc::Rc<std::cell::RefCell<DocsAiSeedState>>);

enum DocsAiSeedState {
    Prepared,
    Aborted,
    Committed {
        ai: Option<shell_ipc_a::ai_core::AiSeedRollback>,
        blank: Option<shell_ipc_d2::docs::editors::DocsBlankReceipt>,
    },
}

pub(crate) struct DocsAiSeedFailure {
    pub error: String,
    pub rollback: Option<DocsAiSeedRollback>,
}

impl DocsAiSeedRollback {
    fn with_committed(
        &self,
        action: impl FnOnce(
            Option<&shell_ipc_a::ai_core::AiSeedRollback>,
            Option<&shell_ipc_d2::docs::editors::DocsBlankReceipt>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let state = self
            .0
            .try_borrow()
            .map_err(|_| "The Docs seed receipt is busy".to_owned())?;
        match &*state {
            DocsAiSeedState::Committed { ai, blank } => action(ai.as_ref(), blank.as_ref()),
            _ => Err("The Docs seed was not committed".into()),
        }
    }
    pub(crate) fn retire_closed(
        &self,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        self.with_committed(|ai, blank| {
            let ai = ai.map_or(Ok(()), |seed| seed.retire_closed(receipt));
            let blank = blank.map_or(Ok(()), |seed| seed.retire_closed(receipt));
            ai.and(blank)
        })
    }
    pub(crate) fn retire_adopted(
        &self,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        self.with_committed(|ai, blank| {
            let ai = ai.map_or(Ok(()), |seed| seed.retire_adopted(receipt));
            let blank = blank.map_or(Ok(()), |seed| seed.retire_adopted(receipt));
            ai.and(blank)
        })
    }
    pub(crate) fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        self.with_committed(|ai, blank| {
            let ai = ai.map_or(Ok(()), |seed| seed.retire_superseded(proof));
            let blank = blank.map_or(Ok(()), |seed| seed.retire_superseded(proof));
            ai.and(blank)
        })
    }
    pub(crate) fn rollback(&self) -> Result<(), String> {
        let mut state = self
            .0
            .try_borrow_mut()
            .map_err(|_| "The Docs seed receipt is busy".to_owned())?;
        match &*state {
            DocsAiSeedState::Prepared => {
                *state = DocsAiSeedState::Aborted;
                Ok(())
            }
            DocsAiSeedState::Aborted => Ok(()),
            DocsAiSeedState::Committed { ai, blank } => {
                let ai = ai.as_ref().map_or(Ok(()), |seed| seed.rollback());
                let blank = blank.as_ref().map_or(Ok(()), |seed| seed.rollback());
                ai.and(blank)
            }
        }
    }
}

pub(crate) struct PreparedDocsAiSeed {
    blank: Option<shell_ipc_d2::docs::editors::DocsPreparedBlank>,
    ai: Option<shell_ipc_a::ai_core::PreparedAiSeed>,
    owner: shell_contract::ipc::NativeBrowserOwner,
    receipt: DocsAiSeedRollback,
}

pub(crate) fn prepare_docs_ai_seed_owned(
    provided: &shell_contract::Provided,
    view: ViewId,
    new_blank: bool,
    content: Option<serde_json::Value>,
    source: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    created: &shell_contract::runtime::RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    owner: &shell_contract::ipc::NativeBrowserOwner,
) -> Result<PreparedDocsAiSeed, DocsAiSeedFailure> {
    let docs = provided
        .all::<shell_ipc_d2::docs::editors::DocsEditors>()
        .into_iter()
        .next()
        .ok_or_else(|| DocsAiSeedFailure {
            error: "The document editor is unavailable".into(),
            rollback: None,
        })?;
    let blank = if new_blank {
        Some(
            docs.prepare_blank_seed(view, created.clone(), reservation)
                .map_err(|error| DocsAiSeedFailure {
                    error,
                    rollback: None,
                })?,
        )
    } else {
        None
    };
    let ai = content
        .map(|content| {
            let seed = shell_ipc_a::ai_core::AcceptedAiDoc::new(
                created.clone(),
                content,
                source.clone(),
                participants.to_vec(),
                cancel.clone(),
                pending.clone(),
                reservation.clone(),
                owner.clone(),
            );
            crate::host::net::ai::prepare_ai_docs_seed(view, seed)
        })
        .transpose()
        .map_err(|error| DocsAiSeedFailure {
            error,
            rollback: None,
        })?;
    Ok(PreparedDocsAiSeed {
        blank,
        ai,
        owner: owner.clone(),
        receipt: DocsAiSeedRollback(std::rc::Rc::new(std::cell::RefCell::new(
            DocsAiSeedState::Prepared,
        ))),
    })
}

impl PreparedDocsAiSeed {
    pub(crate) fn receipt(&self) -> DocsAiSeedRollback {
        self.receipt.clone()
    }

    pub(crate) fn commit(
        mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<DocsAiSeedRollback, DocsAiSeedFailure> {
        let mut state = self
            .receipt
            .0
            .try_borrow_mut()
            .map_err(|_| DocsAiSeedFailure {
                error: "The Docs seed receipt is busy".into(),
                rollback: None,
            })?;
        if !matches!(*state, DocsAiSeedState::Prepared) {
            return Err(DocsAiSeedFailure {
                error: "The Docs seed preparation was aborted".into(),
                rollback: None,
            });
        }
        let mut blank = self
            .blank
            .as_mut()
            .map(|blank| blank.validate_in_transition(transition, &self.owner))
            .transpose()
            .map_err(|error| DocsAiSeedFailure {
                error,
                rollback: None,
            })?;
        let mut blank_receipt = None;
        let mut initialize_and_commit = || {
            initialize()?;
            if let Some(blank) = blank.take() {
                blank_receipt = Some(blank.commit_with_receipt());
            }
            Ok(())
        };
        let ai = match self.ai.take() {
            Some(ai) => Some(ai.commit(transition, &mut initialize_and_commit).map_err(
                |error| DocsAiSeedFailure {
                    error,
                    rollback: None,
                },
            )?),
            None => {
                initialize_and_commit().map_err(|error| DocsAiSeedFailure {
                    error,
                    rollback: None,
                })?;
                None
            }
        };
        drop(initialize_and_commit);
        drop(blank);
        *state = DocsAiSeedState::Committed {
            ai,
            blank: blank_receipt,
        };
        drop(state);
        Ok(self.receipt.clone())
    }
}

pub(crate) fn open_ai_owned(
    content: serde_json::Value,
    origin: shell_contract::ipc::NativeRequestOwner,
    destination: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    then: Box<dyn FnOnce(Result<(), String>) + Send>,
) {
    post_owned(then, move |provided, then| {
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return then(Err("The tab manager is unavailable".into()));
        };
        let request = main_core::shell::tab_manager::EditorOpen::Docs {
            path: None,
            options: Some(main_core::shell::tab_manager::OpenDocsOptions {
                new_blank: true,
                ai_content: Some(content),
            }),
        };
        let opener = main_core::shell::projects::new_documents::NewDocumentOpener::Window {
            web_contents_id: i64::from(destination.view.0),
        };
        tabs.open_initialized_editor_owned(
            request,
            origin,
            vec![destination],
            cancel,
            pending,
            Some(opener),
            Box::new(move |result| {
                then(result.and_then(|receipt| receipt.with_terminal_commit(&mut || Ok(()))));
            }),
        );
    });
}

pub(crate) fn prepare_generated_output(
    output: shell_contract::ipc::SavedPublication,
    input: Option<shell_contract::ipc::NativeSavedInput>,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    accepted: shell_contract::hosts::PdfReloadAccepted,
    wake: shell_contract::hosts::GeneratedPathWake,
) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
    let mut result = None;
    let available = crate::host::w::on_core_services_local(|provided, _| {
        result = Some(match provided.all::<TabsState>().into_iter().next() {
            Some(tabs) => tabs
                .prepare_generated_output(
                    output,
                    input,
                    "generated",
                    participants,
                    cancel,
                    pending,
                    additional,
                    effects,
                    accepted,
                    wake,
                )
                .map(|prepared| {
                    Box::new(prepared) as Box<dyn shell_contract::hosts::PreparedGeneratedPath>
                }),
            None => Err("The tab manager is unavailable".into()),
        });
    });
    if !available {
        return Err("The Docs core services are unavailable".into());
    }
    result.ok_or_else(|| "The Docs generated-output preparation was not run".to_owned())?
}

fn post_owned<T: Send + 'static>(
    then: Box<dyn FnOnce(Result<T, String>) + Send>,
    job: impl FnOnce(&shell_contract::Provided, Box<dyn FnOnce(Result<T, String>) + Send>)
    + Send
    + 'static,
) {
    let callback = std::sync::Arc::new(std::sync::Mutex::new(Some(then)));
    let unavailable = callback.clone();
    crate::host::w::post_core_checked(
        move |provided, _| {
            let then = callback
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(then) = then {
                job(provided, then);
            }
        },
        move || {
            let then = unavailable
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(then) = then {
                then(Err("The Docs core services are unavailable".into()));
            }
        },
    );
}
