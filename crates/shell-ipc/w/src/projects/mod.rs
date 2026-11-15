//! `projectFolder:*` and `project:*`: the project folder bridge of the AI
//! panels and the home project screen (decision 0088).
//!
//! `projectFolder:*` is `registerProjectFolderChannels` of
//! `apps/shell/src/main/index.ts` over `main-core`'s
//! [`ProjectIpc`](main_core::shell::projects::project_ipc::ProjectIpc): every
//! request is judged by the window that sent it. [`Projects`] is the state of
//! this area on the core thread; the project guide, the chat store and the
//! tools it serves the requests with are made on the first request, as
//! `projectFolderServices()` made them (see [`services`]).
//!
//! Five requests reach the first-time trust prompt in the middle of the
//! guide's call. [`ProjectIpc`](main_core::shell::projects::project_ipc::ProjectIpc)
//! answers them as steps, and this file carries the steps out without ever
//! waiting for a renderer: the prompt is a message box over the asking window
//! (`DialogsHost`), and [view all]'s wait for the person to come back to the
//! page is a [`PageEvent`] the host reports later, answered while other
//! requests are served meanwhile.
//!
//! `project:*` is the per-document chat of the app-data project store, in
//! [`legacy`]. What the shell must provide is in [`host`].

mod chat_io;
pub mod host;
mod legacy;
mod mutations;
mod new_document_cleanup;
#[cfg(feature = "probe")]
mod probe;
mod services;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use jscompat::JsonValue;
use main_core::packages::electron_utils::default_save_dir::Thrown;
use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::packages::project_store::{
    PROJECT_FOLDER_CHANNELS, PROJECT_FOLDER_LISTEN_CHANNEL, ProjectId, ProjectTrustAnswer,
};
use main_core::shell::projects::chat_store::OutsideChatAssociation;
use main_core::shell::projects::new_documents::NewDocumentPlacer;
use main_core::shell::projects::project_guide::{
    AdoptPlan, AdoptPreparation, AdoptScan, AdoptStart, ContextPreparation, CreateNew,
    CreatePreparation, InstallIdPreparation, NewProjectRequest, ProjectGuide, ProjectSettingsWrite,
    TrustPreparation, TrustUsePreparation, TrustWrite,
};
use main_core::shell::projects::project_ipc::{
    ChatAppendRequest, IpcStep, PageReturnWatch, ProjectIpc, ProjectIpcDeps, SelectionRequest,
    TRUST_RETURN_ARM_MS,
};
use main_core::shell::projects::project_tools::{ProjectReadStep, finish_project_read};
use serde_json::Value;
use shell_contract::error::IpcError;
use shell_contract::ids::ViewId;
use shell_contract::ipc::{
    CallContext, ChannelKind, Handler, NativeRequestOwner, RegisterError, Registry,
    Responder as RawResponder,
};
use shell_contract::runtime::{Services as ShellServices, Thread};
use shell_contract::value::IpcValue;
use shell_contract::{Batch, DialogsHost, OsShellHost};

pub use host::{
    LegacyProjectStore, LegacyProjectStoreHost, PageEvent, PageEvents, ProjectFiles,
    ProjectFilesHost, ProjectTimer, ProjectsHost, ProjectsHostRef,
};
pub use new_document_cleanup::{ProjectCleanupShutdown, ProjectCleanupSink, ProjectOperation};
#[cfg(feature = "probe")]
pub use probe::{probe_hosts, probe_trigger};

use self::services::{GuidePending, Services, Shell};

enum AdoptWork {
    Plan(AdoptPlan),
    Answer(
        main_core::shell::projects::project_guide::PendingAdopt,
        ProjectTrustAnswer,
    ),
}

enum AdoptDestination {
    Plain,
    Create(NewProjectRequest, bool),
    Hint(
        main_core::shell::projects::project_ipc::ProjectWindow,
        String,
    ),
}

fn guarded_settings_commit(
    host: &ProjectsHostRef,
    owner: &Option<RequestOwner>,
    from: &str,
    to: &str,
) -> Result<(), main_core::node_fs::NodeError> {
    let denied = |message: String| main_core::node_fs::NodeError {
        code: None,
        message,
    };
    let owner = owner
        .as_ref()
        .ok_or_else(|| denied("authenticated project owner is unavailable".into()))?;
    let mut outcome = None;
    host.with_request_owner(&owner.native, &owner.cancel, &mut || {
        outcome = Some(
            std::fs::rename(from, to)
                .map_err(|error| main_core::node_fs::node_error2(&error, "rename", from, to)),
        );
        Ok(())
    })
    .map_err(|error| denied(error.message))?;
    outcome.unwrap_or_else(|| Err(denied("project owner guard did not run".into())))
}

fn guarded_project_publish(
    host: &ProjectsHostRef,
    owner: &Option<RequestOwner>,
    effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
) -> Result<(), main_core::node_fs::NodeError> {
    let denied = |message: String| main_core::node_fs::NodeError {
        code: None,
        message,
    };
    let owner = owner
        .as_ref()
        .ok_or_else(|| denied("authenticated project owner is unavailable".into()))?;
    let mut outcome = None;
    let guarded = host.with_request_owner(&owner.native, &owner.cancel, &mut || {
        let result = effect();
        let answer = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| error_of(Thrown(error.message.clone())));
        outcome = Some(result);
        answer
    });
    match outcome {
        Some(Err(error)) => Err(error),
        Some(Ok(())) => guarded.map_err(|error| denied(error.message)),
        None => Err(denied(
            guarded
                .err()
                .map(|error| error.message)
                .unwrap_or_else(|| "project owner guard did not run".into()),
        )),
    }
}

enum ChatContinuation {
    Initial {
        window: main_core::shell::projects::project_ipc::ProjectWindow,
        selection: main_core::packages::project_store::ProjectSelection,
    },
    List {
        window: main_core::shell::projects::project_ipc::ProjectWindow,
        project_id: ProjectId,
    },
    Open {
        sender: i64,
        chat_ref: main_core::packages::project_store::ProjectChatRef,
        args: JsonValue,
    },
    Rename {
        chat_ref: main_core::packages::project_store::ProjectChatRef,
        args: JsonValue,
    },
    Attach {
        chat_ref: main_core::packages::project_store::ProjectChatRef,
        doc_path: String,
    },
    OpenDocument {
        chat_ref: main_core::packages::project_store::ProjectChatRef,
    },
    New {
        sender: i64,
        project_id: ProjectId,
    },
    Delete {
        sender: i64,
        chat_ref: main_core::packages::project_store::ProjectChatRef,
    },
    Append(ChatAppendRequest),
}

#[derive(Clone)]
struct RequestOwner {
    native: NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
}

struct Responder {
    raw: RawResponder,
    owner: Option<RequestOwner>,
}

impl Responder {
    fn reply(self, result: shell_contract::ipc::Reply) {
        self.raw.reply(result);
    }
}

/// A JSON value as the structured clone would carry it
pub(crate) fn to_ipc(value: &Value) -> IpcValue {
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(b) => IpcValue::Bool(*b),
        Value::Number(n) => IpcValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => IpcValue::String(s.clone()),
        Value::Array(items) => IpcValue::Array(items.iter().map(to_ipc).collect()),
        Value::Object(map) => {
            IpcValue::Object(map.iter().map(|(k, v)| (k.clone(), to_ipc(v))).collect())
        }
    }
}

/// What an `unknown` the renderer sent is to a handler that reads it as JSON:
/// `None` is `undefined`; an `undefined` member is a member that is not there
pub(crate) fn json_of(value: &IpcValue) -> Option<JsonValue> {
    Some(match value {
        IpcValue::Undefined => return None,
        IpcValue::Null => JsonValue::Null,
        IpcValue::Bool(flag) => JsonValue::Bool(*flag),
        IpcValue::Number(number) => JsonValue::Number(*number),
        IpcValue::String(text) => JsonValue::String(text.clone()),
        IpcValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|item| json_of(item).unwrap_or(JsonValue::Null))
                .collect(),
        ),
        IpcValue::Object(entries) => {
            let mut object = jscompat::JsObject::new();
            for (key, item) in entries {
                if let Some(item) = json_of(item) {
                    object.set(key.clone(), item);
                }
            }
            JsonValue::Object(object)
        }
        // a byte buffer or a date has no member a handler reads
        IpcValue::Bytes(..) | IpcValue::Date(_) => JsonValue::Object(jscompat::JsObject::new()),
    })
}

fn error_of(thrown: Thrown) -> IpcError {
    IpcError {
        name: Some("Error".into()),
        message: thrown.0,
    }
}

/// A request that waits for the person to come back to the page [view all]
/// left: what it was answering, and its reply
struct Wait {
    id: u64,
    view: ViewId,
    /// whether the host reports the page's focus and blur for it
    watching: bool,
    watch: PageReturnWatch,
    pending: main_core::shell::projects::project_ipc::AwaitingReturn<GuidePending>,
    reply: Responder,
}

struct Inner {
    ipc: ProjectIpc,
    /// made on the first request, as `projectFolderServices()` was
    services: Option<Services>,
    waits: Vec<Wait>,
    next_wait: u64,
    /// whether the host holds the timer of the guide's use times
    touch_timer: bool,
}

/// The state of the project folder requests on the core thread
pub struct Projects {
    shell: Shell,
    core: Arc<dyn Thread>,
    inner: RefCell<Inner>,
    /// `projectTrackedContents`
    tracked: Rc<RefCell<HashSet<ViewId>>>,
    this: Weak<Projects>,
    cleanup: ProjectCleanupSink,
}

thread_local! {
    /// The state of this core thread: the host reports its page events to it
    static CURRENT: RefCell<Option<Rc<Projects>>> = const { RefCell::new(None) };
    static PENDING_SETTINGS: RefCell<HashMap<u64, (Weak<Projects>, Responder)>> = RefCell::new(HashMap::new());
    static PENDING_TOUCHES: RefCell<HashMap<u64, (Weak<Projects>, Option<Box<dyn FnOnce(Result<(), String>)>>)>> = RefCell::new(HashMap::new());
    static PENDING_OUTSIDE: RefCell<HashMap<u64, (Weak<Projects>, Responder, Result<IpcStep<GuidePending>, Thrown>, Vec<OutsideChatAssociation>)>> = RefCell::new(HashMap::new());
    static NEXT_SETTINGS: Cell<u64> = const { Cell::new(1) };
}

/// Where the host's page events go: onto the core thread, into
/// [`Projects::page_event`]
struct Reporter {
    core: Arc<dyn Thread>,
}

impl PageEvents for Reporter {
    fn send(&self, event: PageEvent) {
        self.core.post(Box::new(move || {
            if let Some(projects) = CURRENT.with(|current| current.borrow().clone()) {
                projects.page_event(event);
            }
        }));
    }
    fn send_if(&self, event: PageEvent, guard: Box<dyn FnOnce() -> bool + Send>) {
        self.core.post(Box::new(move || {
            if guard() {
                if let Some(projects) = CURRENT.with(|current| current.borrow().clone()) {
                    projects.page_event(event);
                }
            }
        }));
    }
}

impl Projects {
    fn new(
        shell: Shell,
        core: Arc<dyn Thread>,
        cleanup_shutdown: ProjectCleanupShutdown,
    ) -> Rc<Projects> {
        let cleanup = ProjectCleanupSink::new(cleanup_shutdown.0, core.clone());
        Rc::new_cyclic(|this| Projects {
            shell,
            core,
            inner: RefCell::new(Inner {
                ipc: ProjectIpc::new(None),
                services: None,
                waits: Vec::new(),
                next_wait: 1,
                touch_timer: false,
            }),
            tracked: Rc::default(),
            cleanup,
            this: this.clone(),
        })
    }

    fn guarded_core<T>(
        &self,
        reply: &Responder,
        work: impl FnOnce() -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        let owner = reply
            .owner
            .as_ref()
            .ok_or_else(|| Thrown("authenticated project owner is unavailable".into()))?;
        let mut work = Some(work);
        let mut result = None;
        self.shell
            .host
            .with_request_owner(&owner.native, &owner.cancel, &mut || {
                let work = work
                    .take()
                    .ok_or_else(|| error_of(Thrown("project continuation repeated".into())))?;
                result = Some(work());
                Ok(())
            })
            .map_err(|error| Thrown(error.message))?;
        result.unwrap_or_else(|| Err(Thrown("project continuation did not run".into())))
    }
    fn open_trust_files(
        &self,
        mut paths: std::vec::IntoIter<String>,
        chats: Option<String>,
        opened: bool,
        pending: main_core::shell::projects::project_ipc::AwaitingTrust<GuidePending>,
        reply: Responder,
    ) {
        if let Err(error) = self.guarded_core(&reply, || Ok(())) {
            return self.respond_guarded(reply, Err(error_of(error)));
        }
        if let Some(path) = paths.next() {
            let Some(owner) = reply.owner.clone() else {
                return self.respond_guarded(
                    reply,
                    Err(error_of(Thrown(
                        "authenticated project request owner is unavailable".into(),
                    ))),
                );
            };
            let Some(tabs) = self.shell.tabs() else {
                return self.respond_guarded(
                    reply,
                    Err(error_of(Thrown("tab services are unavailable".into()))),
                );
            };
            let weak = self.this.clone();
            tabs.route_document_path_owned(
                path,
                owner.native,
                owner.cancel,
                Box::new(move |result| {
                    let Some(projects) = weak.upgrade() else {
                        return reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    };
                    match result {
                        Err(error) => projects.respond_guarded(reply, Err(error_of(Thrown(error)))),
                        Ok(view) => {
                            if let Some(view) = view {
                                projects.track(view);
                            }
                            projects.open_trust_files(
                                paths,
                                chats,
                                opened || view.is_some(),
                                pending,
                                reply,
                            );
                        }
                    }
                }),
            );
            return;
        }
        if let Some(path) = chats {
            let mut accepted = None;
            let result = self.guarded_core(&reply, || {
                accepted = Some(path);
                Ok(())
            });
            if let Err(error) = result {
                return self.respond_guarded(reply, Err(error_of(error)));
            }
            if let Some(path) = accepted {
                let _ = self.shell.os.open_path(std::path::Path::new(&path));
            }
        }
        self.advance(Ok(pending.finish_view(opened)), reply);
    }
    fn open_chat_target(
        &self,
        path: Option<String>,
        chat_ref: main_core::packages::project_store::ProjectChatRef,
        reply: Responder,
    ) {
        if let Err(error) = self.guarded_core(&reply, || Ok(())) {
            return self.respond_guarded(reply, Err(error_of(error)));
        }
        let Some(owner) = reply.owner.clone() else {
            return self.respond_guarded(
                reply,
                Err(error_of(Thrown(
                    "authenticated project request owner is unavailable".into(),
                ))),
            );
        };
        let Some(tabs) = self.shell.tabs() else {
            return self.respond_guarded(
                reply,
                Err(error_of(Thrown("tab services are unavailable".into()))),
            );
        };
        let weak = self.this.clone();
        let done = crate::home_files::back_to_core(
            self.core.clone(),
            move |result: Result<Option<ViewId>, String>| {
                let Some(projects) = weak.upgrade() else {
                    return reply.reply(Err(error_of(Thrown(
                        "project services are unavailable".into(),
                    ))));
                };
                let step = projects.guarded_core(&reply, || {
                    let target = result.map_err(Thrown)?.map(|view| i64::from(view.0));
                    projects.serve(|ipc, deps| {
                        ipc.finish_open_document_chat_target(deps, &chat_ref, target)
                    })
                });
                projects.advance(step, reply);
            },
        );
        tabs.open_document_chat_owned(owner.native, owner.cancel, path, done);
    }

    fn finish_guarded_install(
        &self,
        reply: &Responder,
        result: Result<String, String>,
    ) -> Result<String, String> {
        let mut id = Some(result?);
        let mut finished = None;
        let owner = reply
            .owner
            .as_ref()
            .ok_or_else(|| "authenticated project owner is unavailable".to_owned())?;
        self.shell
            .host
            .with_request_owner(&owner.native, &owner.cancel, &mut || {
                if let Some(id) = id.take() {
                    finished = Some(self.guide().finish_install_id(id));
                }
                Ok(())
            })
            .map_err(|error| error.message)?;
        finished.ok_or_else(|| "project owner guard did not run".to_owned())
    }

    fn queue_outside_writes(
        &self,
        items: Vec<OutsideChatAssociation>,
        step: Result<IpcStep<GuidePending>, Thrown>,
        reply: Responder,
    ) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let guide = self.guide();
        let writes: Vec<_> = items
            .iter()
            .map(|item| {
                guide
                    .prepare_outside_chat(&item.doc_path, &item.project_id, &item.chat_id)
                    .map_err(|error| error.message())
            })
            .collect();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_OUTSIDE.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply, step, items));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            let results: Vec<_> = writes
                .into_iter()
                .map(|write| match write {
                    Ok(write) => write
                        .execute_guarded(&path, |from, to| {
                            guarded_settings_commit(&commit_host, &commit_owner, from, to)
                        })
                        .map_err(|error| error.message()),
                    Err(error) => Err(error),
                })
                .collect();
            core.post(Box::new(move || {
                let pending = PENDING_OUTSIDE.with(|pending| pending.borrow_mut().remove(&token));
                let Some((projects, reply, step, items)) = pending else {
                    drop(completion);
                    return;
                };
                let Some(projects) = projects.upgrade() else {
                    drop(completion);
                    return reply.reply(Err(error_of(Thrown(
                        "project services are unavailable".into(),
                    ))));
                };
                {
                    let mut inner = projects.inner.borrow_mut();
                    if let Some(services) = inner.services.as_mut() {
                        for (item, result) in items.into_iter().zip(results) {
                            services.chats.finish_outside_write(item, result);
                        }
                    }
                }
                drop(completion);
                projects.advance(step, reply);
                projects.sync_touch_timer();
            }));
        });
    }

    fn write_settings(&self, write: ProjectSettingsWrite, reply: Responder) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            let result = write.execute_guarded(&path, |from, to| guarded_settings_commit(&commit_host, &commit_owner, from, to)).map_err(|error| Thrown(error.message()));
            core.post(Box::new(move || {
                let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                let Some((projects, reply)) = pending else {
                    drop(completion);
                    return;
                };
                let Some(projects) = projects.upgrade() else {
                    drop(completion);
                    return reply.reply(Err(error_of(Thrown("project services are unavailable".into()))));
                };
                let step = projects.guarded_core(&reply, || {
                    if let (ProjectSettingsWrite::Remove { project_id }, Ok(main_core::shell::projects::project_guide::ProjectSettingsWriteResult::Removed)) = (&write, &result) {
                        projects.guide().finish_remove_from_list(project_id);
                    }
                    projects.serve(|ipc, deps| ipc.finish_settings_write(deps, write, result))
                });
                drop(completion);
                projects.advance(step, reply);
                projects.sync_touch_timer();
            }));
        });
    }

    fn write_selection(&self, request: SelectionRequest, reply: Responder) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            core.post(Box::new(move || {
                let owner = PENDING_SETTINGS
                    .with(|pending| pending.borrow().get(&token).map(|entry| entry.0.clone()));
                let Some(projects) = owner.and_then(|owner| owner.upgrade()) else {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    }
                    return;
                };
                let owner = projects.serve(|ipc, deps| ipc.selection_owner_current(deps, &request));
                if !matches!(owner, Ok(true)) {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    projects.guide().restore_touch_timer();
                    drop(completion);
                    projects.sync_touch_timer();
                    if let Some((_, reply)) = pending {
                        let error = match owner {
                            Err(error) => error,
                            _ => Thrown("selection owner changed".into()),
                        };
                        reply.reply(Err(error_of(error)));
                    }
                    return;
                }
                let write = match projects
                    .guide()
                    .prepare_selection(&request.target, request.project_id.as_deref())
                {
                    Ok(write) => write,
                    Err(error) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            reply.reply(Err(error_of(Thrown(error.message()))));
                        }
                        return;
                    }
                };
                let workers = projects.shell.workers.clone();
                let core = projects.core.clone();
                workers.spawn(Box::new(move || {
                    let result = write
                        .execute_guarded(&path, |from, to| {
                            guarded_settings_commit(&commit_host, &commit_owner, from, to)
                        })
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        let Some((projects, reply)) = pending else {
                            drop(completion);
                            return;
                        };
                        let Some(projects) = projects.upgrade() else {
                            drop(completion);
                            return reply.reply(Err(error_of(Thrown(
                                "project services are unavailable".into(),
                            ))));
                        };
                        let guide = projects.guide();
                        let step = projects.guarded_core(&reply, || {
                            if !projects
                                .serve(|ipc, deps| ipc.selection_owner_current(deps, &request))?
                            {
                                return Err(Thrown("selection owner changed".into()));
                            }
                            let result = result
                                .map(|ack| guide.finish_selection(ack))
                                .map_err(Thrown);
                            projects.serve(|ipc, deps| ipc.finish_selection(deps, request, result))
                        });
                        if step.is_err() {
                            guide.restore_touch_timer();
                        }
                        drop(completion);
                        projects.advance(step, reply);
                        projects.sync_touch_timer();
                    }));
                }));
            }));
        });
    }

    fn write_hint_selection(
        &self,
        owner: i64,
        window: main_core::shell::projects::project_ipc::ProjectWindow,
        doc_path: String,
        result: main_core::packages::project_store::AddFolderResult,
        reply: Responder,
    ) {
        let project_id = match &result {
            main_core::packages::project_store::AddFolderResult::Added { project, .. } => {
                project.id.clone()
            }
            _ => {
                let step = self.guarded_core(&reply, || {
                    self.serve(|ipc, deps| ipc.finish_hint_selection(deps, result))
                });
                return self.advance(step, reply);
            }
        };
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            core.post(Box::new(move || {
                let projects = PENDING_SETTINGS
                    .with(|pending| pending.borrow().get(&token).map(|entry| entry.0.clone()));
                let Some(projects) = projects.and_then(|projects| projects.upgrade()) else {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    }
                    return;
                };
                let native = commit_owner.as_ref().is_some_and(|owner| {
                    commit_host
                        .with_request_owner(&owner.native, &owner.cancel, &mut || Ok(()))
                        .is_ok()
                });
                let current =
                    projects.serve(|ipc, deps| ipc.hint_owner_current(deps, owner, &window));
                if !native || !current {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown("hint owner changed".into()))));
                    }
                    return;
                }
                let target = main_core::shell::projects::project_guide::SelectionTarget::Document {
                    path: doc_path,
                };
                let write = match projects
                    .guide()
                    .prepare_selection(&target, Some(&project_id))
                {
                    Ok(write) => write,
                    Err(error) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            reply.reply(Err(error_of(Thrown(error.message()))));
                        }
                        return;
                    }
                };
                let workers = projects.shell.workers.clone();
                let core = projects.core.clone();
                workers.spawn(Box::new(move || {
                    let outcome = write
                        .execute_guarded(&path, |from, to| {
                            guarded_settings_commit(&commit_host, &commit_owner, from, to)
                        })
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        let Some((projects, reply)) = pending else {
                            drop(completion);
                            return;
                        };
                        let Some(projects) = projects.upgrade() else {
                            drop(completion);
                            return reply.reply(Err(error_of(Thrown(
                                "project services are unavailable".into(),
                            ))));
                        };
                        let step = match outcome {
                            Ok(ack) => projects.guarded_core(&reply, || {
                                if !projects
                                    .serve(|ipc, deps| ipc.hint_owner_current(deps, owner, &window))
                                {
                                    return Err(Thrown("hint owner changed".into()));
                                }
                                projects.guide().finish_selection(ack);
                                projects.serve(|ipc, deps| ipc.finish_hint_selection(deps, result))
                            }),
                            Err(error) => Err(Thrown(error)),
                        };
                        if step.is_err() {
                            projects.guide().restore_touch_timer();
                        }
                        drop(completion);
                        projects.advance(step, reply);
                        projects.sync_touch_timer();
                    }));
                }));
            }));
        });
    }

    fn read_context(&self, project_id: String, reply: Responder) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let prepared = self.guide().prepare_context(&project_id);
        let write = match prepared {
            Ok(ContextPreparation::Done(result)) => {
                return self.advance(ProjectIpc::finish_context_read(Ok(result)), reply);
            }
            Ok(ContextPreparation::Write(write)) => write,
            Err(error) => return reply.reply(Err(error_of(Thrown(error.message())))),
        };
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            let result = write
                .execute_guarded(&path, |from, to| {
                    guarded_settings_commit(&commit_host, &commit_owner, from, to)
                })
                .map_err(|error| error.message());
            core.post(Box::new(move || {
                let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                let Some((projects, reply)) = pending else {
                    drop(completion);
                    return;
                };
                let Some(projects) = projects.upgrade() else {
                    drop(completion);
                    return reply.reply(Err(error_of(Thrown(
                        "project services are unavailable".into(),
                    ))));
                };
                let guide = projects.guide();
                let step = projects.guarded_core(&reply, || {
                    let result = result.map_err(Thrown).and_then(|ack| {
                        guide
                            .finish_context(ack)
                            .map_err(|error| Thrown(error.message()))
                    });
                    ProjectIpc::finish_context_read(result)
                });
                if step.is_err() {
                    guide.restore_touch_timer();
                }
                drop(completion);
                projects.advance(step, reply);
                projects.sync_touch_timer();
            }));
        });
    }

    fn trust_answer(
        &self,
        project_id: String,
        pending: GuidePending,
        answer: ProjectTrustAnswer,
        reply: Responder,
    ) {
        let GuidePending::DecideTrust(pending) = pending else {
            return reply.reply(Err(error_of(Thrown(
                "the trust answer belongs to another request".into(),
            ))));
        };
        let write = match self.guide().prepare_trust_answer(pending, answer) {
            TrustPreparation::Done(result) => {
                let step = self.guarded_core(&reply, || {
                    self.serve(|ipc, deps| ipc.finish_trust_answer(deps, &project_id, Ok(result)))
                });
                return self.advance(step, reply);
            }
            TrustPreparation::Write(write) => write,
        };
        self.queue_trust_write(project_id, write, reply);
    }

    fn trust_without(&self, project_id: String, reply: Responder) {
        match self.guide().prepare_trust_without(&project_id) {
            Ok(write) => self.queue_trust_write(project_id, write, reply),
            Err(error) => reply.reply(Err(error_of(Thrown(error.message())))),
        }
    }

    fn trust_use(
        &self,
        project_id: String,
        sender: i64,
        install_id: Option<String>,
        reply: Responder,
    ) {
        match self
            .guide()
            .prepare_decide_trust_use(&project_id, install_id.as_deref())
        {
            Err(error) => reply.reply(Err(error_of(Thrown(error.message())))),
            Ok(TrustUsePreparation::Done(result)) => {
                let step = self.guarded_core(&reply, || {
                    self.serve(|ipc, deps| ipc.finish_trust_answer(deps, &project_id, Ok(result)))
                });
                self.advance(step, reply);
            }
            Ok(TrustUsePreparation::Ask { prompt, pending }) => {
                let step = ProjectIpc::prompt_decide_trust(
                    sender,
                    project_id,
                    prompt,
                    GuidePending::DecideTrust(pending),
                );
                self.advance(Ok(step), reply);
            }
            Ok(TrustUsePreparation::Write(write)) => {
                self.queue_trust_write(project_id, write, reply)
            }
            Ok(TrustUsePreparation::NeedInstallId) => match self.guide().prepare_install_id() {
                InstallIdPreparation::Ready(id) => {
                    self.trust_use(project_id, sender, Some(id), reply)
                }
                InstallIdPreparation::Write(write) => {
                    let commit_owner = reply.owner.clone();
                    let commit_host = self.shell.host.clone();
                    let token = NEXT_SETTINGS.with(|next| {
                        let token = next.get();
                        next.set(
                            token
                                .checked_add(1)
                                .expect("project settings token exhausted"),
                        );
                        token
                    });
                    PENDING_SETTINGS.with(|pending| {
                        pending
                            .borrow_mut()
                            .insert(token, (self.this.clone(), reply));
                    });
                    let path = self.shell.settings_path();
                    let queue = main_core::shell::app_settings::queue::for_path(&path);
                    let core = self.core.clone();
                    queue.submit(move |completion| {
                        let result = write
                            .execute_guarded(&path, |from, to| {
                                guarded_settings_commit(&commit_host, &commit_owner, from, to)
                            })
                            .map_err(|error| error.message());
                        core.post(Box::new(move || {
                            let pending = PENDING_SETTINGS
                                .with(|pending| pending.borrow_mut().remove(&token));
                            let Some((projects, reply)) = pending else {
                                drop(completion);
                                return;
                            };
                            let Some(projects) = projects.upgrade() else {
                                drop(completion);
                                return reply.reply(Err(error_of(Thrown(
                                    "project services are unavailable".into(),
                                ))));
                            };
                            let result = projects.finish_guarded_install(&reply, result);
                            drop(completion);
                            match result {
                                Ok(id) => projects.trust_use(project_id, sender, Some(id), reply),
                                Err(error) => reply.reply(Err(error_of(Thrown(error)))),
                            }
                        }));
                    });
                }
            },
        }
    }

    fn queue_trust_write(&self, project_id: String, write: TrustWrite, reply: Responder) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            let result = write
                .execute_guarded(&path, |from, to| {
                    guarded_settings_commit(&commit_host, &commit_owner, from, to)
                })
                .map_err(|error| error.message());
            core.post(Box::new(move || {
                let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                let Some((projects, reply)) = pending else {
                    drop(completion);
                    return;
                };
                let Some(projects) = projects.upgrade() else {
                    drop(completion);
                    return reply.reply(Err(error_of(Thrown(
                        "project services are unavailable".into(),
                    ))));
                };
                let step = projects.guarded_core(&reply, || {
                    let result = result.map_err(Thrown).and_then(|ack| {
                        projects
                            .guide()
                            .finish_trust_answer(ack)
                            .map_err(|error| Thrown(error.message()))
                    });
                    projects.serve(|ipc, deps| ipc.finish_trust_answer(deps, &project_id, result))
                });
                drop(completion);
                projects.advance(step, reply);
                projects.sync_touch_timer();
            }));
        });
    }

    fn start_creation(
        &self,
        owner: i64,
        request: NewProjectRequest,
        second: bool,
        reply: Responder,
    ) {
        match self.guide().prepare_create(&request) {
            Err(error) => reply.reply(Err(error_of(Thrown(error.message())))),
            Ok(CreatePreparation::Done(result)) => {
                let step = self.guarded_core(&reply, || {
                    self.serve(|ipc, deps| {
                        ipc.finish_create_answer(deps, owner, request, second, Ok(result))
                    })
                });
                self.advance(step, reply);
            }
            Ok(CreatePreparation::Adopt(start)) => self.start_adoption(
                owner,
                AdoptDestination::Create(request, second),
                Ok(start),
                reply,
            ),
            Ok(CreatePreparation::New {
                parent,
                name,
                target,
            }) => self.queue_creation(owner, request, second, parent, name, target, reply),
        }
    }

    fn queue_creation(
        &self,
        owner: i64,
        request: NewProjectRequest,
        second: bool,
        parent: String,
        name: String,
        target: String,
        reply: Responder,
    ) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            core.post(Box::new(move || {
                let projects = PENDING_SETTINGS
                    .with(|pending| pending.borrow().get(&token).map(|entry| entry.0.clone()));
                let Some(projects) = projects.and_then(|projects| projects.upgrade()) else {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    }
                    return;
                };
                let write = match projects
                    .guide()
                    .prepare_new_project(&parent, &name, &target)
                {
                    Ok(CreateNew::Write(write)) => write,
                    Ok(CreateNew::Done(result)) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            let step = projects.guarded_core(&reply, || {
                                projects.serve(|ipc, deps| {
                                    ipc.finish_create_answer(
                                        deps,
                                        owner,
                                        request,
                                        second,
                                        Ok(result),
                                    )
                                })
                            });
                            projects.advance(step, reply);
                        }
                        return;
                    }
                    Err(error) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            reply.reply(Err(error_of(Thrown(error.message()))));
                        }
                        return;
                    }
                };
                let workers = projects.shell.workers.clone();
                let core = projects.core.clone();
                workers.spawn(Box::new(move || {
                    let result = write
                        .execute_authorized(
                            &path,
                            |effect| guarded_project_publish(&commit_host, &commit_owner, effect),
                            |from, to| {
                                guarded_settings_commit(&commit_host, &commit_owner, from, to)
                            },
                        )
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        let Some((projects, reply)) = pending else {
                            drop(completion);
                            return;
                        };
                        let Some(projects) = projects.upgrade() else {
                            drop(completion);
                            return reply.reply(Err(error_of(Thrown(
                                "project services are unavailable".into(),
                            ))));
                        };
                        let result = result
                            .map(|execution| projects.guide().finish_create(execution))
                            .map_err(Thrown);
                        drop(completion);
                        let step = projects.guarded_core(&reply, || {
                            projects.serve(|ipc, deps| {
                                ipc.finish_create_answer(deps, owner, request, second, result)
                            })
                        });
                        projects.advance(step, reply);
                    }));
                }));
            }));
        });
    }

    fn finish_adopt_step(
        &self,
        owner: i64,
        destination: AdoptDestination,
        result: Result<main_core::packages::project_store::AddFolderResult, Thrown>,
        reply: &Responder,
    ) -> Result<IpcStep<GuidePending>, Thrown> {
        self.guarded_core(reply, || {
            self.serve(|ipc, deps| match destination {
                AdoptDestination::Plain => ipc.finish_adopt_answer(deps, owner, None, result),
                AdoptDestination::Create(request, second) => {
                    ipc.finish_adopt_answer(deps, owner, Some((request, second)), result)
                }
                AdoptDestination::Hint(window, doc_path) => {
                    ipc.finish_hint_adoption(deps, owner, window, doc_path, result)
                }
            })
        })
    }

    fn start_adoption(
        &self,
        owner: i64,
        destination: AdoptDestination,
        start: Result<AdoptStart, main_core::shell::projects::project_guide::GuideError>,
        reply: Responder,
    ) {
        if !reply.owner.as_ref().is_some_and(|owner| {
            self.shell
                .host
                .with_request_owner(&owner.native, &owner.cancel, &mut || Ok(()))
                .is_ok()
        }) {
            return reply.reply(Err(error_of(Thrown(
                "adoption owner is no longer available".into(),
            ))));
        }
        if let AdoptDestination::Hint(window, _) = &destination
            && !self.serve(|ipc, deps| ipc.hint_owner_current(deps, owner, window))
        {
            return reply.reply(Err(error_of(Thrown("hint owner changed".into()))));
        }
        match start {
            Err(error) => reply.reply(Err(error_of(Thrown(error.message())))),
            Ok(AdoptStart::Done(result)) => {
                let step = self.finish_adopt_step(owner, destination, Ok(result), &reply);
                self.advance(step, reply);
            }
            Ok(AdoptStart::Ask { prompt, pending }) => {
                let pending = GuidePending::Adopt(pending);
                let step = match destination {
                    AdoptDestination::Create(request, second) => {
                        ProjectIpc::prompt_create(owner, request, second, prompt, pending)
                    }
                    AdoptDestination::Plain => ProjectIpc::prompt_adopt(owner, prompt, pending),
                    AdoptDestination::Hint(window, doc_path) => {
                        ProjectIpc::prompt_hint(owner, window, doc_path, prompt, pending)
                    }
                };
                self.advance(Ok(step), reply);
            }
            Ok(AdoptStart::Ready(plan)) => {
                self.queue_adoption(owner, AdoptWork::Plan(plan), destination, reply)
            }
            Ok(AdoptStart::NeedInstallId(scan)) => {
                self.adoption_install_id(owner, destination, scan, reply)
            }
        }
    }

    fn adoption_install_id(
        &self,
        owner: i64,
        destination: AdoptDestination,
        scan: AdoptScan,
        reply: Responder,
    ) {
        match self.guide().prepare_install_id() {
            InstallIdPreparation::Ready(id) => {
                let start = self.guide().scan_adoption(scan, &id);
                self.start_adoption(owner, destination, start, reply);
            }
            InstallIdPreparation::Write(write) => {
                let commit_owner = reply.owner.clone();
                let commit_host = self.shell.host.clone();
                let token = NEXT_SETTINGS.with(|next| {
                    let token = next.get();
                    next.set(
                        token
                            .checked_add(1)
                            .expect("project settings token exhausted"),
                    );
                    token
                });
                PENDING_SETTINGS.with(|pending| {
                    pending
                        .borrow_mut()
                        .insert(token, (self.this.clone(), reply));
                });
                let path = self.shell.settings_path();
                let queue = main_core::shell::app_settings::queue::for_path(&path);
                let core = self.core.clone();
                queue.submit(move |completion| {
                    let result = write
                        .execute_guarded(&path, |from, to| {
                            guarded_settings_commit(&commit_host, &commit_owner, from, to)
                        })
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        let Some((projects, reply)) = pending else {
                            drop(completion);
                            return;
                        };
                        let Some(projects) = projects.upgrade() else {
                            drop(completion);
                            return reply.reply(Err(error_of(Thrown(
                                "project services are unavailable".into(),
                            ))));
                        };
                        let result = projects.finish_guarded_install(&reply, result);
                        drop(completion);
                        match result {
                            Ok(id) => {
                                if !reply.owner.as_ref().is_some_and(|owner| {
                                    projects
                                        .shell
                                        .host
                                        .with_request_owner(
                                            &owner.native,
                                            &owner.cancel,
                                            &mut || Ok(()),
                                        )
                                        .is_ok()
                                }) {
                                    return reply.reply(Err(error_of(Thrown(
                                        "adoption owner is no longer available".into(),
                                    ))));
                                }
                                let start = projects.guide().scan_adoption(scan, &id);
                                projects.start_adoption(owner, destination, start, reply);
                            }
                            Err(error) => reply.reply(Err(error_of(Thrown(error)))),
                        }
                    }));
                });
            }
        }
    }

    fn adopt_answer(
        &self,
        owner: i64,
        pending: GuidePending,
        answer: ProjectTrustAnswer,
        destination: AdoptDestination,
        reply: Responder,
    ) {
        let GuidePending::Adopt(pending) = pending else {
            return reply.reply(Err(error_of(Thrown(
                "the trust answer belongs to another request".into(),
            ))));
        };
        if answer.is_none() {
            let result = self
                .guide()
                .prepare_adopt_answer(pending, answer)
                .map_err(|error| Thrown(error.message()));
            let result = match result {
                Ok(AdoptPreparation::Done(result)) => Ok(result),
                Ok(AdoptPreparation::Write(_)) => Err(Thrown(
                    "cancelled adoption unexpectedly requested a write".into(),
                )),
                Err(error) => Err(error),
            };
            let step = self.finish_adopt_step(owner, destination, result, &reply);
            return self.advance(step, reply);
        }
        self.queue_adoption(
            owner,
            AdoptWork::Answer(pending, answer),
            destination,
            reply,
        );
    }

    fn queue_adoption(
        &self,
        owner: i64,
        work: AdoptWork,
        destination: AdoptDestination,
        reply: Responder,
    ) {
        let commit_owner = reply.owner.clone();
        let commit_host = self.shell.host.clone();
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_SETTINGS.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), reply));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            core.post(Box::new(move || {
                let projects = PENDING_SETTINGS
                    .with(|pending| pending.borrow().get(&token).map(|entry| entry.0.clone()));
                let Some(projects) = projects.and_then(|projects| projects.upgrade()) else {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    }
                    return;
                };
                let live = commit_owner.as_ref().is_some_and(|owner| {
                    commit_host
                        .with_request_owner(&owner.native, &owner.cancel, &mut || Ok(()))
                        .is_ok()
                }) && match &destination {
                    AdoptDestination::Hint(window, _) => {
                        projects.serve(|ipc, deps| ipc.hint_owner_current(deps, owner, window))
                    }
                    _ => true,
                };
                if !live {
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        reply.reply(Err(error_of(Thrown(
                            "adoption owner is no longer available".into(),
                        ))));
                    }
                    return;
                }
                let preparation = match work {
                    AdoptWork::Plan(plan) => projects
                        .guide()
                        .prepare_adopt_write(plan)
                        .map(AdoptPreparation::Write),
                    AdoptWork::Answer(pending, answer) => {
                        projects.guide().prepare_adopt_answer(pending, answer)
                    }
                };
                let write = match preparation {
                    Ok(AdoptPreparation::Write(write)) => write,
                    Ok(AdoptPreparation::Done(result)) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            let step =
                                projects.finish_adopt_step(owner, destination, Ok(result), &reply);
                            projects.advance(step, reply);
                        }
                        return;
                    }
                    Err(error) => {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        if let Some((_, reply)) = pending {
                            reply.reply(Err(error_of(Thrown(error.message()))));
                        }
                        return;
                    }
                };
                let workers = projects.shell.workers.clone();
                let core = projects.core.clone();
                workers.spawn(Box::new(move || {
                    let result = write
                        .execute_authorized(
                            &path,
                            |effect| guarded_project_publish(&commit_host, &commit_owner, effect),
                            |from, to| {
                                guarded_settings_commit(&commit_host, &commit_owner, from, to)
                            },
                        )
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let pending =
                            PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        let Some((projects, reply)) = pending else {
                            drop(completion);
                            return;
                        };
                        let Some(projects) = projects.upgrade() else {
                            drop(completion);
                            return reply.reply(Err(error_of(Thrown(
                                "project services are unavailable".into(),
                            ))));
                        };
                        let mut result =
                            Some(result.map(|ack| projects.guide().prepare_adoption_ack(ack)));
                        let mut completed = None;
                        let guarded = match reply.owner.as_ref() {
                            Some(owner) => projects.shell.host.with_request_owner(
                                &owner.native,
                                &owner.cancel,
                                &mut || {
                                    let Some(result) = result.take() else {
                                        return Ok(());
                                    };
                                    completed = Some(match result {
                                        Ok(ack) => Ok(projects.guide().finish_adoption_ack(ack)),
                                        Err(error) => Err(Thrown(error)),
                                    });
                                    Ok(())
                                },
                            ),
                            None => Err(error_of(Thrown(
                                "authenticated project request owner is unavailable".into(),
                            ))),
                        };
                        let result = match guarded {
                            Ok(()) => completed.unwrap_or_else(|| {
                                Err(Thrown("adoption acknowledgement did not run".into()))
                            }),
                            Err(error) => Err(Thrown(error.message)),
                        };
                        let (result, warnings) = match result {
                            Ok((result, warnings)) => (Ok(result), warnings),
                            Err(error) => (Err(error), Vec::new()),
                        };
                        drop(completion);
                        projects.guide().report_write_warnings(&warnings);
                        let step = projects.finish_adopt_step(owner, destination, result, &reply);
                        projects.advance(step, reply);
                        projects.sync_touch_timer();
                    }));
                }));
            }));
        });
    }

    /// Calls `f` with the request layer and the services it is served with
    fn serve<R>(
        &self,
        f: impl FnOnce(&mut ProjectIpc, &mut ProjectIpcDeps<'_, GuidePending>) -> R,
    ) -> R {
        self.serve_with_open_files(None, f)
    }

    fn serve_with_open_files<R>(
        &self,
        open_files: Option<Result<Vec<OpenFileState>, OpenFilesSourceFailed>>,
        f: impl FnOnce(&mut ProjectIpc, &mut ProjectIpcDeps<'_, GuidePending>) -> R,
    ) -> R {
        let mut guard = self.inner.borrow_mut();
        let Inner { ipc, services, .. } = &mut *guard;
        let services = services.get_or_insert_with(|| {
            Services::new(&self.shell, self.tracked.clone(), self.this.clone())
        });
        let previous = open_files.map(|result| services.tools.replace_open_files(result));
        let mut deps = ProjectIpcDeps {
            guide: &mut services.guide,
            chats: &mut services.chats,
            tools: &mut services.tools,
            scopes: &mut services.scopes,
            dialogs: &mut services.dialogs,
            host: &mut services.host,
        };
        let result = f(ipc, &mut deps);
        if let Some(previous) = previous {
            services.tools.restore_open_files(previous);
        }
        result
    }

    /// `trackProjectContents(contents)`: follow the view's close and reloads
    fn track(&self, view: ViewId) {
        if self.tracked.borrow_mut().insert(view) {
            self.shell.host.track(view);
        }
    }

    /// One request of the bridge: `handlers[name](sender, raw)`
    fn request(
        &self,
        name: &'static str,
        sender: ViewId,
        raw: Option<JsonValue>,
        reply: Responder,
    ) {
        self.track(sender);
        if matches!(name, "search" | "read") {
            let projects = self.this.clone();
            self.shell
                .files
                .open_files_async(Box::new(move |open_files| {
                    if let Some(projects) = projects.upgrade() {
                        projects.request_ready(name, sender, raw, reply, Some(open_files));
                    } else {
                        reply.reply(Err(error_of(Thrown(
                            "project services are unavailable".into(),
                        ))));
                    }
                }));
        } else {
            self.request_ready(name, sender, raw, reply, None);
        }
    }

    fn request_ready(
        &self,
        name: &'static str,
        sender: ViewId,
        raw: Option<JsonValue>,
        reply: Responder,
        open_files: Option<Result<Vec<OpenFileState>, OpenFilesSourceFailed>>,
    ) {
        if name == "read" {
            let guarded = self.with_tools(|tools| {
                tools.prepare_read(
                    sender,
                    raw.as_ref().unwrap_or(&JsonValue::Null),
                    open_files.unwrap_or(Err(OpenFilesSourceFailed)),
                )
            });
            match guarded {
                Err(error) => reply.reply(Err(error_of(error))),
                Ok(ProjectReadStep::Done(result)) => reply.reply(Ok(to_ipc(
                    &serde_json::to_value(result).expect("project read serializes"),
                ))),
                Ok(ProjectReadStep::Read { path, offset }) => {
                    let files = self.shell.files.clone();
                    self.shell.workers.spawn(Box::new(move || {
                        let result = finish_project_read(files.read_page(&path, offset));
                        reply.reply(Ok(to_ipc(
                            &serde_json::to_value(result).expect("project read serializes"),
                        )));
                    }));
                }
            }
            return;
        }
        let step = self.serve_with_open_files(open_files, |ipc, deps| {
            ipc.handle(deps, name, i64::from(sender.0), raw.as_ref())
        });
        self.advance(step, reply);
        self.sync_touch_timer();
    }

    /// `ipcMain.on(PROJECT_FOLDER_LISTEN_CHANNEL)`: a bridge started or
    /// stopped listening to one event
    fn listen(&self, sender: ViewId, message: Option<JsonValue>) {
        self.track(sender);
        let (method, listening) = match &message {
            Some(JsonValue::Object(object)) => (object.get("method"), object.get("listening")),
            _ => (None, None),
        };
        self.serve(|ipc, deps| ipc.listen(&mut *deps.host, i64::from(sender.0), method, listening));
    }

    fn respond_guarded(&self, reply: Responder, value: shell_contract::ipc::Reply) {
        let Some(owner) = reply.owner.clone() else {
            return reply.reply(Err(error_of(Thrown(
                "authenticated project request owner is unavailable".into(),
            ))));
        };
        let mut reply = Some(reply);
        let mut value = Some(value);
        let result = self
            .shell
            .host
            .with_request_owner(&owner.native, &owner.cancel, &mut || {
                if let (Some(reply), Some(value)) = (reply.take(), value.take()) {
                    reply.reply(value);
                }
                Ok(())
            });
        if let Some(reply) = reply.take() {
            let error = result
                .err()
                .unwrap_or_else(|| error_of(Thrown("project owner guard did not run".into())));
            reply.reply(Err(error));
        }
    }

    fn advance(&self, mut step: Result<IpcStep<GuidePending>, Thrown>, reply: Responder) {
        loop {
            let outside = self
                .inner
                .borrow_mut()
                .services
                .as_mut()
                .map(|services| services.chats.take_outside_writes())
                .unwrap_or_default();
            if !outside.is_empty() {
                return self.queue_outside_writes(outside, step, reply);
            }
            match step {
                Err(thrown) => return self.respond_guarded(reply, Err(error_of(thrown))),
                Ok(IpcStep::Done(value)) => {
                    return self.respond_guarded(
                        reply,
                        Ok(value.as_ref().map_or(IpcValue::Undefined, to_ipc)),
                    );
                }
                Ok(IpcStep::SettingsWrite(write)) => return self.write_settings(write, reply),
                Ok(IpcStep::Mutation(request)) => return self.mutation(request, reply),
                Ok(IpcStep::ContextRead { project_id }) => {
                    return self.read_context(project_id, reply);
                }
                Ok(IpcStep::SelectionWrite(request)) => {
                    return self.write_selection(request, reply);
                }
                Ok(IpcStep::InitialChat { window, selection }) => {
                    return self
                        .queue_chat_io(ChatContinuation::Initial { window, selection }, reply);
                }
                Ok(IpcStep::ListChats { window, project_id }) => {
                    return self
                        .queue_chat_io(ChatContinuation::List { window, project_id }, reply);
                }
                Ok(IpcStep::OpenChat {
                    sender,
                    chat_ref,
                    args,
                }) => {
                    return self.queue_chat_io(
                        ChatContinuation::Open {
                            sender,
                            chat_ref,
                            args,
                        },
                        reply,
                    );
                }
                Ok(IpcStep::RenameChat { chat_ref, args }) => {
                    return self.queue_chat_io(ChatContinuation::Rename { chat_ref, args }, reply);
                }
                Ok(IpcStep::AttachChat { chat_ref, doc_path }) => {
                    return self
                        .queue_chat_io(ChatContinuation::Attach { chat_ref, doc_path }, reply);
                }
                Ok(IpcStep::OpenDocumentChat { chat_ref }) => {
                    return self.queue_chat_io(ChatContinuation::OpenDocument { chat_ref }, reply);
                }
                Ok(IpcStep::NewChat { sender, project_id }) => {
                    return self.queue_chat_io(ChatContinuation::New { sender, project_id }, reply);
                }
                Ok(IpcStep::DeleteChat { sender, chat_ref }) => {
                    return self
                        .queue_chat_io(ChatContinuation::Delete { sender, chat_ref }, reply);
                }
                Ok(IpcStep::ChatAppend(request)) => {
                    return self.queue_chat_io(ChatContinuation::Append(request), reply);
                }
                Ok(IpcStep::TrustAnswer {
                    project_id,
                    pending,
                    answer,
                }) => return self.trust_answer(project_id, pending, answer, reply),
                Ok(IpcStep::TrustWithout {
                    project_id,
                    sender: _,
                }) => return self.trust_without(project_id, reply),
                Ok(IpcStep::TrustUse { project_id, sender }) => {
                    return self.trust_use(project_id, sender, None, reply);
                }
                Ok(IpcStep::AdoptStart {
                    owner,
                    folder,
                    relocate_id,
                }) => {
                    let start = self.guide().prepare_adopt(&folder, relocate_id.as_deref());
                    return self.start_adoption(owner, AdoptDestination::Plain, start, reply);
                }
                Ok(IpcStep::CreateStart {
                    owner,
                    request,
                    second,
                }) => return self.start_creation(owner, request, second, reply),
                Ok(IpcStep::ConfirmExisting {
                    owner,
                    request,
                    folder_path,
                }) => {
                    if let Err(error) = self.guarded_core(&reply, || Ok(())) {
                        return self.respond_guarded(reply, Err(error_of(error)));
                    }
                    step = self.serve(|_, deps| {
                        ProjectIpc::continue_confirm_existing(deps, owner, request, &folder_path)
                    });
                }
                Ok(IpcStep::HintStart {
                    owner,
                    window,
                    doc_path,
                }) => {
                    let start = self.guide().prepare_adopt_hint(&doc_path);
                    return self.start_adoption(
                        owner,
                        AdoptDestination::Hint(window, doc_path),
                        start,
                        reply,
                    );
                }
                Ok(IpcStep::HintAnswer {
                    owner,
                    window,
                    doc_path,
                    pending,
                    answer,
                }) => {
                    return self.adopt_answer(
                        owner,
                        pending,
                        answer,
                        AdoptDestination::Hint(window, doc_path),
                        reply,
                    );
                }
                Ok(IpcStep::HintSelection {
                    owner,
                    window,
                    doc_path,
                    result,
                }) => return self.write_hint_selection(owner, window, doc_path, result, reply),
                Ok(IpcStep::AdoptAnswer {
                    owner,
                    pending,
                    answer,
                    create,
                }) => {
                    return self.adopt_answer(
                        owner,
                        pending,
                        answer,
                        match create {
                            Some((request, second)) => AdoptDestination::Create(request, second),
                            None => AdoptDestination::Plain,
                        },
                        reply,
                    );
                }
                Ok(IpcStep::AskTrust {
                    owner,
                    prompt,
                    pending,
                }) => {
                    // a modal message box over the asking window
                    let choice = self.shell.ask_trust(owner, &prompt);
                    step = self.serve(|ipc, deps| ipc.resume_trust(deps, pending, choice));
                }
                Ok(IpcStep::ViewTrustFiles {
                    paths,
                    chats,
                    pending,
                }) => {
                    return self.open_trust_files(paths.into_iter(), chats, false, pending, reply);
                }
                Ok(IpcStep::OpenDocumentChatTarget { path, chat_ref }) => {
                    return self.open_chat_target(path, chat_ref, reply);
                }
                Ok(IpcStep::WaitForReturn { owner, pending }) => {
                    let page = u32::try_from(owner)
                        .ok()
                        .map(ViewId)
                        .filter(|view| self.shell.views.sender_of(*view).is_some());
                    let Some(view) = page else {
                        // `webContents.fromId(owner)` is none: the page is gone
                        step = self.serve(|ipc, deps| ipc.resume_return(deps, pending, false));
                        continue;
                    };
                    // `waitForPageReturn(page)`: the timer is set, then the
                    // page's listeners added
                    let destroyed = self.shell.host.page_destroyed(view);
                    let id = {
                        let mut inner = self.inner.borrow_mut();
                        let id = inner.next_wait;
                        inner.next_wait += 1;
                        id
                    };
                    if !destroyed {
                        self.shell
                            .host
                            .set_timer(ProjectTimer::ReturnArm(id), TRUST_RETURN_ARM_MS);
                        self.shell.host.watch_return(view);
                    }
                    self.inner.borrow_mut().waits.push(Wait {
                        id,
                        view,
                        watching: !destroyed,
                        watch: PageReturnWatch::new(destroyed),
                        pending,
                        reply,
                    });
                    return self.settle();
                }
            }
        }
    }

    /// Answers the waits whose page came back or went away
    fn settle(&self) {
        loop {
            let done = {
                let mut inner = self.inner.borrow_mut();
                let at = inner
                    .waits
                    .iter()
                    .position(|wait| wait.watch.outcome().is_some());
                at.map(|at| inner.waits.remove(at))
            };
            let Some(wait) = done else {
                return;
            };
            let back = wait.watch.outcome().unwrap_or(false);
            if wait.watching {
                self.shell
                    .host
                    .clear_timer(ProjectTimer::ReturnArm(wait.id));
                self.shell.host.stop_return(wait.view);
            }
            let step = self.serve(|ipc, deps| ipc.resume_return(deps, wait.pending, back));
            self.advance(step, wait.reply);
        }
    }

    /// What the host reports of a page or a timer
    fn page_event(&self, event: PageEvent) {
        match event {
            PageEvent::Navigated(view) => {
                if self.tracked.borrow().contains(&view) {
                    self.inner.borrow_mut().ipc.reset(i64::from(view.0));
                }
            }
            PageEvent::Destroyed(view) => {
                if self.tracked.borrow_mut().remove(&view) {
                    self.inner.borrow_mut().ipc.forget(i64::from(view.0));
                }
                for wait in &mut self.inner.borrow_mut().waits {
                    if wait.view == view {
                        wait.watch.destroyed();
                    }
                }
            }
            PageEvent::Blur(view) => {
                for wait in &mut self.inner.borrow_mut().waits {
                    if wait.view == view {
                        wait.watch.blur();
                    }
                }
            }
            PageEvent::Focus(view) => {
                for wait in &mut self.inner.borrow_mut().waits {
                    if wait.view == view {
                        wait.watch.focus();
                    }
                }
            }
            PageEvent::Timer(ProjectTimer::ReturnArm(id)) => {
                for wait in &mut self.inner.borrow_mut().waits {
                    if wait.id == id {
                        // the timer fired: its clear is still asked at the end, as `finish` does
                        wait.watch.arm();
                    }
                }
            }
            PageEvent::Timer(ProjectTimer::Touches) => {
                self.inner.borrow_mut().touch_timer = false;
                self.queue_touch_flush(None);
            }
        }
        self.settle();
        self.sync_touch_timer();
    }

    /// The guide records when it would have armed `setTimeout` for a use
    /// time; the host runs that timer
    fn sync_touch_timer(&self) {
        let (armed, guide) = {
            let inner = self.inner.borrow();
            let guide = inner
                .services
                .as_ref()
                .map(|services| services.guide.0.clone());
            (inner.touch_timer, guide)
        };
        let Some(guide) = guide else {
            return;
        };
        match (armed, guide.touch_timer()) {
            (false, Some(timer)) => {
                self.shell
                    .host
                    .set_timer(ProjectTimer::Touches, timer.delay);
                self.inner.borrow_mut().touch_timer = true;
            }
            (true, None) => {
                self.shell.host.clear_timer(ProjectTimer::Touches);
                self.inner.borrow_mut().touch_timer = false;
            }
            _ => {}
        }
    }

    // ---- what the rest of the shell asks of this area ----
    /// Whether the project's current folder is available to its AI tools.
    pub fn project_available(&self, project_id: &str) -> Result<bool, Thrown> {
        self.with_tools(|tools| tools.project_available(project_id))
    }

    /// Checks the current project visibility rules for an absolute path.
    pub fn project_visible(&self, project_id: &str, path: &str) -> Result<bool, Thrown> {
        self.with_tools(|tools| tools.project_visible(project_id, path))
    }

    /// Reads through the same guards and download prompt as project tools.
    pub fn prepare_project_read_with_open_files(
        &self,
        owner: ViewId,
        project_id: &str,
        path: &str,
        offset: f64,
        open_files: Result<Vec<OpenFileState>, OpenFilesSourceFailed>,
    ) -> Result<ProjectReadStep, Thrown> {
        self.with_tools(|tools| {
            tools.prepare_project_read_with_open_files(owner, project_id, path, offset, open_files)
        })
    }

    fn with_tools<R>(&self, f: impl FnOnce(&mut services::ToolsOfShell) -> R) -> R {
        let mut inner = self.inner.borrow_mut();
        let services = inner.services.get_or_insert_with(|| {
            Services::new(&self.shell, self.tracked.clone(), self.this.clone())
        });
        f(&mut services.tools)
    }

    /// `projectFolderIpc.windowProject(id)`: the project this window's AI
    /// panel has chosen now (the panel's last answer, else the starting
    /// selection); `None` for "no project" and for a window that is no AI window
    pub fn window_project(&self, sender: i64) -> Option<ProjectId> {
        self.serve(|ipc, deps| ipc.window_project(deps, sender))
    }

    /// `projectFolderIpc.untitledProject(id)`: the project an untitled
    /// document's panel chose
    pub fn untitled_project(&self, sender: i64) -> Option<ProjectId> {
        self.inner.borrow().ipc.untitled_project(sender)
    }

    /// `projectFolderServices().guide`
    pub fn guide(&self) -> Rc<ProjectGuide> {
        self.serve(|_, deps| {
            let _ = deps;
        });
        self.inner
            .borrow()
            .services
            .as_ref()
            .map(|services| services.guide.0.clone())
            .expect("the services were made")
    }

    /// `newDocuments`: where a window's new documents go
    pub fn new_documents(&self) -> Rc<RefCell<NewDocumentPlacer<Rc<ProjectGuide>>>> {
        self.serve(|_, deps| {
            let _ = deps;
        });
        self.inner
            .borrow()
            .services
            .as_ref()
            .map(|services| services.placer.clone())
            .expect("the services were made")
    }

    pub fn capture_new_document(
        &self,
        view: ViewId,
    ) -> (
        main_core::shell::projects::new_documents::NewDocumentPlacementSeed,
        main_core::shell::projects::project_ipc::WindowProjectLookup,
        main_core::shell::projects::project_guide::NewDocumentIo,
    ) {
        let sender = i64::from(view.0);
        let placer = self.new_documents();
        let seed = placer.borrow_mut().capture_placement(sender);
        let preset = seed.preset_project();
        let lookup = self.serve(|ipc, deps| {
            let window = deps.host.window_of(sender);
            ipc.prepare_window_project(sender, window.as_ref(), preset)
        });
        (seed, lookup, self.guide().new_document_io())
    }

    pub fn new_document_cleanup(&self) -> ProjectCleanupSink {
        self.cleanup.clone()
    }

    /// Flush before shutdown; the callback runs on core after the write.
    pub fn flush_touches(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_TOUCHES.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), Some(done)));
        });
        let cleanup = self.cleanup.clone();
        let core = self.core.clone();
        self.shell.workers.spawn(Box::new(move || {
            let result = cleanup.quiesce_and_drain();
            core.post(Box::new(move || {
                let Some((projects, done)) =
                    PENDING_TOUCHES.with(|pending| pending.borrow_mut().remove(&token))
                else {
                    return;
                };
                let Some(done) = done else {
                    return;
                };
                match (projects.upgrade(), result) {
                    (Some(projects), Ok(())) => projects.queue_touch_flush(Some(done)),
                    (_, Err(error)) => done(Err(error)),
                    (None, Ok(())) => done(Err("project services are unavailable".into())),
                }
            }));
        }));
    }

    fn queue_touch_flush(&self, done: Option<Box<dyn FnOnce(Result<(), String>)>>) {
        let guide = self
            .inner
            .borrow()
            .services
            .as_ref()
            .map(|services| services.guide.0.clone());
        let Some(flush) = guide.and_then(|guide| guide.prepare_touch_flush()) else {
            if let Some(done) = done {
                done(Ok(()));
            }
            return;
        };
        let token = NEXT_SETTINGS.with(|next| {
            let token = next.get();
            next.set(
                token
                    .checked_add(1)
                    .expect("project settings token exhausted"),
            );
            token
        });
        PENDING_TOUCHES.with(|pending| {
            pending
                .borrow_mut()
                .insert(token, (self.this.clone(), done));
        });
        let path = self.shell.settings_path();
        let queue = main_core::shell::app_settings::queue::for_path(&path);
        let core = self.core.clone();
        queue.submit(move |completion| {
            let result = flush.execute(&path).map_err(|error| error.message());
            core.post(Box::new(move || {
                let pending = PENDING_TOUCHES.with(|pending| pending.borrow_mut().remove(&token));
                let Some((projects, done)) = pending else {
                    drop(completion);
                    return;
                };
                if let Some(projects) = projects.upgrade() {
                    if result.is_ok() {
                        projects.guide().finish_touch_flush(&flush);
                    } else {
                        projects.guide().restore_touch_timer();
                    }
                    drop(completion);
                    projects.sync_touch_timer();
                    if let Err(error) = result {
                        eprintln!("shell: flushing project touches: {error}");
                    }
                    if let Some(done) = done {
                        done(Ok(()));
                    }
                } else {
                    drop(completion);
                    if let Some(done) = done {
                        done(Err("project services are unavailable".into()));
                    }
                }
            }));
        });
    }
}

/// One `PROJECT_FOLDER_CHANNELS` request
struct Request {
    projects: Rc<Projects>,
    name: &'static str,
}

impl Handler for Request {
    fn call(&self, cx: &CallContext, args: Vec<IpcValue>, reply: RawResponder) {
        let raw = args.first().and_then(json_of);
        let Some(context) = cx.native.clone() else {
            return reply.reply(Err(error_of(Thrown(
                "authenticated project request owner is unavailable".into(),
            ))));
        };
        let owner = NativeRequestOwner {
            view: cx.sender.view,
            context,
        };
        if let Err(error) =
            self.projects
                .shell
                .host
                .with_request_owner(&owner, &cx.cancel, &mut || Ok(()))
        {
            return reply.reply(Err(error));
        }
        self.projects.request(
            self.name,
            cx.sender.view,
            raw,
            Responder {
                raw: reply,
                owner: Some(RequestOwner {
                    native: owner,
                    cancel: cx.cancel.clone(),
                }),
            },
        );
    }
}

/// `ipcMain.on(PROJECT_FOLDER_LISTEN_CHANNEL)`
struct Listen {
    projects: Rc<Projects>,
}

impl Handler for Listen {
    fn call(&self, cx: &CallContext, args: Vec<IpcValue>, reply: RawResponder) {
        self.projects
            .listen(cx.sender.view, args.first().and_then(json_of));
        reply.reply(Ok(IpcValue::Undefined));
    }
}

pub fn register(
    registry: &mut dyn Registry,
    services: &ShellServices,
) -> Result<(), RegisterError> {
    let hosts = &services.hosts;
    let shell = Shell {
        host: hosts.need::<ProjectsHostRef>(Batch::W)?,
        home: hosts.need(Batch::W)?,
        search: hosts.need(Batch::W)?,
        files: hosts.need::<ProjectFilesHost>(Batch::W)?,
        legacy: hosts.need::<LegacyProjectStoreHost>(Batch::W)?,
        dialogs: hosts.need::<DialogsHost>(Batch::W)?,
        os: hosts.need::<OsShellHost>(Batch::W)?,
        events: services.events.clone(),
        views: services.views.clone(),
        paths: services.paths.clone(),
        workers: services.workers.clone(),
        provided: registry.provided(),
        save_dir: hosts.need(Batch::W)?,
    };
    let cleanup_shutdown = hosts.need::<ProjectCleanupShutdown>(Batch::W)?;
    legacy::register(registry, &shell.legacy, &shell.home)?;
    let projects = Projects::new(shell, services.core.clone(), cleanup_shutdown);
    projects.shell.host.attach(Arc::new(Reporter {
        core: services.core.clone(),
    }));
    CURRENT.with(|current| *current.borrow_mut() = Some(projects.clone()));
    registry.provided().add::<Projects>(projects.clone());
    for (name, channel) in PROJECT_FOLDER_CHANNELS {
        registry.register(
            Batch::W,
            channel,
            ChannelKind::Invoke,
            Rc::new(Request {
                projects: projects.clone(),
                name,
            }),
        )?;
    }
    registry.register(
        Batch::W,
        PROJECT_FOLDER_LISTEN_CHANNEL,
        ChannelKind::Send,
        Rc::new(Listen { projects }),
    )
}
