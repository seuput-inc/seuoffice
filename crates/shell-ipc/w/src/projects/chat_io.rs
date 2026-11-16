use super::*;
use main_core::packages::project_store::{LegacyDocumentChat, ProjectChatRef, ProjectSelection};
use main_core::shell::app_settings::queue::Completion;
use main_core::shell::projects::chat_store::{
    ChatIo, ChatIoAcknowledgement, ChatIoProgress, ChatIoResult, ChatOperation, ChatStoreError,
    ChatStoreErrorKind, ChatStoreHost, default_new_id,
};
use main_core::shell::projects::project_guide::{ChatGuideAuthority, ChatGuideSeed, GuideError};
use main_core::shell::projects::project_ipc::{ChatAppendOwner, ChatDocuments, ProjectWindow};

pub(super) enum ChatFinish {
    Initial(ProjectSelection),
    List {
        legacy_path: Option<String>,
    },
    Open {
        sender: i64,
        chat_ref: ProjectChatRef,
    },
    Rename(ProjectChatRef),
    Attach,
    OpenDocument(ProjectChatRef),
    New {
        sender: i64,
        project_id: ProjectId,
    },
    Delete {
        sender: i64,
        chat_ref: ProjectChatRef,
    },
    Append(ChatAppendOwner),
}

struct WorkerChatHost {
    home: crate::home_files::HomeFilesHost,
    host: ProjectsHostRef,
    os: OsShellHost,
    owner: Option<RequestOwner>,
    authority: ChatGuideAuthority,
    settings_path: String,
    trash_chat: Option<String>,
}

impl ChatStoreHost for WorkerChatHost {
    fn trash(&mut self, path: &str) -> Result<(), String> {
        let chat_id = self
            .trash_chat
            .as_deref()
            .ok_or_else(|| "this chat operation cannot move files to trash".to_owned())?;
        self.authority
            .authorize_chat_trash(&self.settings_path, path, chat_id)
            .map_err(|error| error.message())?;
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| "authenticated project owner is unavailable".to_owned())?;
        self.os
            .trash_item_authorized(std::path::Path::new(path), &mut |effect| {
                let mut result = None;
                let guarded =
                    self.host
                        .with_request_owner(&owner.native, &owner.cancel, &mut || {
                            let actual = effect();
                            let status = actual
                                .as_ref()
                                .map(|_| ())
                                .map_err(|error| error_of(Thrown(error.clone())));
                            result = Some(actual);
                            status
                        });
                match result {
                    Some(Err(error)) => Err(error),
                    Some(Ok(())) => guarded.map_err(|error| error.message),
                    None => Err(guarded
                        .err()
                        .map(|error| error.message)
                        .unwrap_or_else(|| "trash admission did not run".into())),
                }
            })
    }

    fn now_iso(&mut self) -> Result<String, ChatStoreError> {
        jscompat::to_iso_string(self.home.now_ms()).ok_or_else(|| ChatStoreError {
            message: "Invalid time value".to_owned(),
            code: None,
            kind: ChatStoreErrorKind::Failure,
        })
    }

    fn new_id(&mut self) -> String {
        let mut bytes = [0u8; 12];
        bytes.copy_from_slice(&self.host.random_bytes(12));
        default_new_id(&bytes)
    }

    fn warn(&mut self, label: &str, detail: Option<&str>) {
        self.host.warn(label, detail);
    }
}

impl Projects {
    fn with_chat_store<R>(&self, work: impl FnOnce(&mut services::ChatsOfShell) -> R) -> R {
        let mut inner = self.inner.borrow_mut();
        work(
            &mut inner
                .services
                .as_mut()
                .expect("project services were initialized before dispatch")
                .chats,
        )
    }

    fn chat_owner_core<T>(
        &self,
        token: u64,
        work: impl FnOnce() -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        PENDING_SETTINGS.with(|pending| {
            let pending = pending.borrow();
            let (_, reply) = pending
                .get(&token)
                .ok_or_else(|| Thrown("chat request is no longer active".into()))?;
            self.guarded_core(reply, work)
        })
    }

    pub(super) fn queue_chat_io(&self, continuation: ChatContinuation, reply: Responder) {
        let (operation, finish) = match continuation {
            ChatContinuation::Initial { window, selection } => {
                let doc_path = match window {
                    ProjectWindow::Editor { doc_path, .. } => doc_path,
                    ProjectWindow::Home => None,
                };
                match (
                    selection.project_id.as_ref().filter(|id| !id.is_empty()),
                    doc_path.filter(|path| !path.is_empty()),
                ) {
                    (Some(project_id), Some(doc_path)) => (
                        ChatOperation::ForDocument {
                            project_id: project_id.clone(),
                            doc_path,
                        },
                        ChatFinish::Initial(selection),
                    ),
                    _ => {
                        let step = self.guarded_core(&reply, || {
                            ProjectIpc::finish_initial_chat_result(selection, None)
                        });
                        return self.advance(step, reply);
                    }
                }
            }
            ChatContinuation::List { window, project_id } => {
                let legacy_path = match window {
                    ProjectWindow::Editor { doc_path, .. } => {
                        doc_path.filter(|path| !path.is_empty())
                    }
                    ProjectWindow::Home => None,
                };
                (
                    ChatOperation::List { project_id },
                    ChatFinish::List { legacy_path },
                )
            }
            ChatContinuation::Open {
                sender,
                chat_ref,
                args,
            } => (
                ChatOperation::Open { args },
                ChatFinish::Open { sender, chat_ref },
            ),
            ChatContinuation::Rename { chat_ref, args } => {
                (ChatOperation::Rename { args }, ChatFinish::Rename(chat_ref))
            }
            ChatContinuation::Attach { chat_ref, doc_path } => (
                ChatOperation::Attach { chat_ref, doc_path },
                ChatFinish::Attach,
            ),
            ChatContinuation::OpenDocument { chat_ref } => (
                ChatOperation::Info {
                    chat_ref: chat_ref.clone(),
                },
                ChatFinish::OpenDocument(chat_ref),
            ),
            ChatContinuation::New { sender, project_id } => (
                ChatOperation::New {
                    project_id: project_id.clone(),
                },
                ChatFinish::New { sender, project_id },
            ),
            ChatContinuation::Delete { sender, chat_ref } => (
                ChatOperation::Delete {
                    chat_ref: chat_ref.clone(),
                },
                ChatFinish::Delete { sender, chat_ref },
            ),
            ChatContinuation::Append(request) => {
                let (operation, owner) = request.into_io_parts();
                (operation, ChatFinish::Append(owner))
            }
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
        let core = self.core.clone();
        main_core::shell::app_settings::queue::for_path(&path).submit(move |completion| {
            core.post(Box::new(move || {
                let projects = PENDING_SETTINGS.with(|pending| {
                    pending
                        .borrow()
                        .get(&token)
                        .and_then(|(projects, _)| projects.upgrade())
                });
                let Some(projects) = projects else {
                    PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    return;
                };
                let project_id = match operation.project_id() {
                    Some(id) => id.to_owned(),
                    None => {
                        return projects.fail_chat_io(
                            token,
                            completion,
                            "chat operation has no valid project identity".into(),
                        );
                    }
                };
                let mut seed = match projects.guide().prepare_chat_io(&project_id) {
                    Ok(seed) => seed,
                    Err(error) => return projects.fail_chat_io(token, completion, error.message()),
                };
                if let ChatOperation::Delete { chat_ref } = &operation {
                    if let Err(error) = seed.capture_chat_trash(&chat_ref.chat_id) {
                        return projects.fail_chat_io(token, completion, error.message());
                    }
                }
                let authority = seed.authority();
                let io = projects.chat_owner_core(token, || {
                    if let ChatFinish::Append(owner) = &finish {
                        if !projects
                            .serve(|ipc, deps| ipc.chat_append_io_owner_current(deps, owner))?
                        {
                            return Err(Thrown("append owner changed".into()));
                        }
                    }
                    projects
                        .with_chat_store(|chats| chats.take_io(operation))
                        .map_err(|error| Thrown(error.message))
                });
                match io {
                    Ok(io) => projects.run_chat_io(token, finish, io, seed, authority, completion),
                    Err(error) => projects.fail_chat_io(token, completion, error.0),
                }
            }));
        });
    }

    fn run_chat_io(
        &self,
        token: u64,
        finish: ChatFinish,
        io: ChatIo,
        seed: ChatGuideSeed,
        authority: ChatGuideAuthority,
        completion: Completion,
    ) {
        let request = PENDING_SETTINGS.with(|pending| {
            pending
                .borrow()
                .get(&token)
                .and_then(|(_, reply)| reply.owner.clone())
        });
        let Some(request_owner) = request else {
            let error = self.cancel_chat_snapshot(io, "chat request is no longer active".into());
            self.shell
                .host
                .warn("[projects] cancelled chat worker:", Some(&error));
            drop(completion);
            return;
        };
        let owner = Some(request_owner);
        let host = self.shell.host.clone();
        let home = self.shell.home.clone();
        let os = self.shell.os.clone();
        let legacy = self.shell.legacy.clone();
        let path = self.shell.settings_path();
        let core = self.core.clone();
        let trash_chat = match io.operation() {
            ChatOperation::Delete { chat_ref } => Some(chat_ref.chat_id.clone()),
            _ => None,
        };
        self.shell.workers.spawn(Box::new(move || {
            let write_host = host.clone();
            let write_owner = owner.clone();
            let commit_host = host.clone();
            let commit_owner = owner.clone();
            let clock = home.clone();
            let guide = seed.into_worker(
                path.clone(),
                move |effect| guarded_project_publish(&write_host, &write_owner, effect),
                move |from, to| guarded_settings_commit(&commit_host, &commit_owner, from, to),
                move || {
                    jscompat::to_iso_string(clock.now_ms())
                        .ok_or_else(|| GuideError::Plain("Invalid time value".to_owned()))
                },
            );
            let worker_host = WorkerChatHost {
                home,
                host,
                os,
                owner,
                authority: authority.clone(),
                settings_path: path,
                trash_chat,
            };
            let ack = io.execute(guide, worker_host);
            let legacy_chat = if ack.is_success() {
                match &finish {
                    ChatFinish::List {
                        legacy_path: Some(path),
                    } => legacy.legacy_chat_for(path).ok().flatten(),
                    _ => None,
                }
            } else {
                None
            };
            core.post(Box::new(move || {
                let projects = PENDING_SETTINGS.with(|pending| {
                    pending
                        .borrow()
                        .get(&token)
                        .and_then(|(projects, _)| projects.upgrade())
                });
                let Some(projects) = projects else {
                    PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    return;
                };
                projects.ack_chat_io(token, finish, authority, ack, legacy_chat, completion);
            }));
        }));
    }

    fn cancel_chat_snapshot(&self, io: ChatIo, error: String) -> String {
        match self.with_chat_store(|chats| chats.cancel_io(io)) {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; chat lease recovery failed: {}", cleanup.message),
        }
    }

    fn abandon_chat_snapshot(&self, ack: ChatIoAcknowledgement, error: String) -> String {
        match self.with_chat_store(|chats| chats.abandon_io(ack)) {
            Ok(abandoned) => {
                self.report_chat_warnings(&abandoned.warnings);
                error
            }
            Err(cleanup) => format!(
                "{error}; chat acknowledgement recovery failed: {}",
                cleanup.message
            ),
        }
    }

    fn ack_chat_io(
        &self,
        token: u64,
        finish: ChatFinish,
        authority: ChatGuideAuthority,
        ack: ChatIoAcknowledgement,
        legacy: Option<LegacyDocumentChat>,
        completion: Completion,
    ) {
        if let Err(error) = self.guide().validate_chat_io_ack(&authority, &ack.changes) {
            let error = self.abandon_chat_snapshot(ack, error.message());
            return self.fail_chat_io(token, completion, error);
        }
        let mut ack = Some(ack);
        let mut accepted = None;
        let guarded = self.chat_owner_core(token, || {
            let ack = ack
                .take()
                .ok_or_else(|| Thrown("chat acknowledgement repeated".into()))?;
            let mut result = self
                .with_chat_store(|chats| chats.finish_io(ack))
                .map_err(|error| Thrown(error.message))?;
            self.guide()
                .finish_chat_io(std::mem::take(&mut result.changes));
            accepted = Some(result);
            Ok(())
        });
        if let Err(error) = guarded {
            let mut error = error.0;
            if let Some(ack) = ack {
                error = self.abandon_chat_snapshot(ack, error);
            } else if let Some(result) = accepted {
                if let ChatIoProgress::NeedInstallId(io) = result.progress {
                    error = self.cancel_chat_snapshot(*io, error);
                }
                self.report_chat_warnings(&result.warnings);
            }
            return self.fail_chat_io(token, completion, error);
        }
        let Some(result) = accepted else {
            return self.fail_chat_io(token, completion, "chat acknowledgement did not run".into());
        };
        self.report_chat_warnings(&result.warnings);
        self.with_chat_store(|chats| chats.publish_io_events(&result.appended));
        if let Err(error) = self.chat_owner_core(token, || {
            self.serve(|ipc, deps| ipc.forward_chat_io_events(deps));
            Ok(())
        }) {
            self.serve(|_, deps| {
                deps.chats.take_appended();
            });
            let error = match result.progress {
                ChatIoProgress::NeedInstallId(io) => self.cancel_chat_snapshot(*io, error.0),
                ChatIoProgress::Done(_) => error.0,
            };
            return self.fail_chat_io(token, completion, error);
        }
        match result.progress {
            ChatIoProgress::NeedInstallId(io) => {
                self.chat_install_id(token, finish, *io, authority, completion)
            }
            ChatIoProgress::Done(result) => {
                self.complete_chat_io(token, finish, result, legacy, completion)
            }
        }
    }

    fn chat_install_id(
        &self,
        token: u64,
        finish: ChatFinish,
        io: ChatIo,
        authority: ChatGuideAuthority,
        completion: Completion,
    ) {
        match self.guide().prepare_install_id() {
            InstallIdPreparation::Ready(_) => {
                self.resume_chat_io(token, finish, io, authority, completion)
            }
            InstallIdPreparation::Write(write) => {
                let owner = PENDING_SETTINGS.with(|pending| {
                    pending
                        .borrow()
                        .get(&token)
                        .and_then(|(_, reply)| reply.owner.clone())
                });
                let host = self.shell.host.clone();
                let path = self.shell.settings_path();
                let core = self.core.clone();
                self.shell.workers.spawn(Box::new(move || {
                    let result = write
                        .execute_guarded(&path, |from, to| {
                            guarded_settings_commit(&host, &owner, from, to)
                        })
                        .map_err(|error| error.message());
                    core.post(Box::new(move || {
                        let projects = PENDING_SETTINGS.with(|pending| {
                            pending
                                .borrow()
                                .get(&token)
                                .and_then(|(projects, _)| projects.upgrade())
                        });
                        let Some(projects) = projects else {
                            drop(completion);
                            return;
                        };
                        let result = PENDING_SETTINGS.with(|pending| {
                            let pending = pending.borrow();
                            let (_, reply) = pending
                                .get(&token)
                                .ok_or_else(|| "chat request is no longer active".to_owned())?;
                            projects.finish_guarded_install(reply, result)
                        });
                        match result {
                            Ok(_) => {
                                projects.resume_chat_io(token, finish, io, authority, completion)
                            }
                            Err(error) => {
                                let error = projects.cancel_chat_snapshot(io, error);
                                projects.fail_chat_io(token, completion, error);
                            }
                        }
                    }));
                }));
            }
        }
    }

    fn resume_chat_io(
        &self,
        token: u64,
        finish: ChatFinish,
        io: ChatIo,
        authority: ChatGuideAuthority,
        completion: Completion,
    ) {
        if let Err(error) = self
            .guide()
            .validate_chat_io_ack(&authority, &Default::default())
        {
            let error = self.cancel_chat_snapshot(io, error.message());
            return self.fail_chat_io(token, completion, error);
        }
        let project_id = match io.operation().project_id() {
            Some(id) => id.to_owned(),
            None => {
                let error = self
                    .cancel_chat_snapshot(io, "chat continuation has no project identity".into());
                return self.fail_chat_io(token, completion, error);
            }
        };
        match self.guide().prepare_chat_io(&project_id) {
            Ok(seed) => self.run_chat_io(token, finish, io, seed, authority, completion),
            Err(error) => {
                let error = self.cancel_chat_snapshot(io, error.message());
                self.fail_chat_io(token, completion, error);
            }
        }
    }

    fn complete_chat_io(
        &self,
        token: u64,
        finish: ChatFinish,
        result: Result<ChatIoResult, ChatStoreError>,
        legacy: Option<LegacyDocumentChat>,
        completion: Completion,
    ) {
        if let (ChatFinish::OpenDocument(chat_ref), Ok(ChatIoResult::Info(info))) =
            (&finish, &result)
        {
            let info = info.as_ref().map(|info| ChatDocuments {
                documents: info.documents(),
                last_editor: info.last_editor(),
            });
            let target =
                self.serve(|ipc, deps| ipc.prepare_open_document_chat_result(deps, chat_ref, info));
            if let Some(target) = target {
                return self.open_chat_document(token, chat_ref.clone(), target.path, completion);
            }
        }
        let step = self.chat_owner_core(token, || {
            let result = result.map_err(|error| Thrown(error.message))?;
            self.serve(|ipc, deps| match (finish, result) {
                (ChatFinish::Initial(selection), ChatIoResult::ForDocument(chat)) => {
                    ProjectIpc::finish_initial_chat_result(selection, chat)
                }
                (ChatFinish::List { .. }, ChatIoResult::List(chats)) => {
                    ProjectIpc::finish_list_chats_result(chats, legacy)
                }
                (ChatFinish::Open { sender, chat_ref }, ChatIoResult::Open(opened)) => ipc
                    .finish_open_chat_result(
                        sender,
                        &chat_ref,
                        serde_json::to_value(opened).map_err(|error| Thrown(error.to_string()))?,
                    ),
                (ChatFinish::Rename(chat_ref), ChatIoResult::Write(result)) => {
                    ipc.finish_rename_chat_result(deps, &chat_ref, result)
                }
                (ChatFinish::Attach, ChatIoResult::Write(result)) => {
                    ProjectIpc::finish_chat_write_result(result)
                }
                (ChatFinish::New { sender, project_id }, ChatIoResult::New(chat_id)) => {
                    ipc.finish_new_chat_result(sender, project_id, chat_id)
                }
                (ChatFinish::Delete { sender, chat_ref }, ChatIoResult::Write(result)) => {
                    ipc.finish_delete_chat_result(deps, sender, &chat_ref, result)
                }
                (ChatFinish::Append(owner), ChatIoResult::Append(result)) => {
                    if !ipc.chat_append_io_owner_current(deps, &owner)? {
                        return Err(Thrown("append owner changed".into()));
                    }
                    ipc.finish_append_chat_result(
                        deps,
                        owner,
                        serde_json::to_value(result).map_err(|error| Thrown(error.to_string()))?,
                    )
                }
                (ChatFinish::OpenDocument(_), ChatIoResult::Info(None)) => Ok(IpcStep::Done(None)),
                _ => Err(Thrown(
                    "chat worker returned the wrong operation result".into(),
                )),
            })
        });
        let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
        drop(completion);
        if let Some((_, reply)) = pending {
            self.advance(step, reply);
        }
    }

    fn open_chat_document(
        &self,
        token: u64,
        chat_ref: ProjectChatRef,
        path: Option<String>,
        completion: Completion,
    ) {
        let request = PENDING_SETTINGS.with(|pending| {
            pending
                .borrow()
                .get(&token)
                .and_then(|(_, reply)| reply.owner.clone())
        });
        let Some(owner) = request else {
            return self.fail_chat_io(
                token,
                completion,
                "authenticated project owner is unavailable".into(),
            );
        };
        let Some(tabs) = self.shell.tabs() else {
            return self.fail_chat_io(token, completion, "tab services are unavailable".into());
        };
        let core = self.core.clone();
        tabs.open_document_chat_owned(
            owner.native,
            owner.cancel,
            path,
            Box::new(move |result| {
                core.post(Box::new(move || {
                    let projects = PENDING_SETTINGS.with(|pending| {
                        pending
                            .borrow()
                            .get(&token)
                            .and_then(|(projects, _)| projects.upgrade())
                    });
                    let Some(projects) = projects else {
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                        drop(completion);
                        return;
                    };
                    let step = projects.chat_owner_core(token, || {
                        let view = result.map_err(Thrown)?.map(|view| i64::from(view.0));
                        projects.serve(|ipc, deps| {
                            ipc.finish_open_document_chat_target(deps, &chat_ref, view)
                        })
                    });
                    let pending =
                        PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
                    drop(completion);
                    if let Some((_, reply)) = pending {
                        projects.advance(step, reply);
                    }
                }));
            }),
        );
    }

    fn fail_chat_io(&self, token: u64, completion: Completion, error: String) {
        let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
        drop(completion);
        if let Some((_, reply)) = pending {
            self.advance(Err(Thrown(error)), reply);
        }
    }

    fn report_chat_warnings(&self, warnings: &[(String, Option<String>)]) {
        for (label, detail) in warnings {
            self.shell.host.warn(label, detail.as_deref());
        }
    }
}
