mod export;
mod native_export;
use main_core::shell::detached_windows::DetachedOpenDocumentsRun;
use main_core::shell::tab_manager::{OpenDocumentTab, OpenDocumentsRun, OpenDocumentsStep};
use shell_contract::ids::ViewId;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;

use super::state::{Inner, TabsState};

type Done = Box<dyn FnOnce(Result<Vec<OpenDocumentTab>, String>)>;

#[derive(Clone)]
struct DocumentRequest {
    owner: NativeRequestOwner,
    cancel: CancelToken,
}

impl TabsState {
    /// Keep the authenticated origin distinct from every document being listed.
    pub fn open_documents_owned(&self, owner: NativeRequestOwner, cancel: CancelToken, done: Done) {
        self.start_open_documents(Some(DocumentRequest { owner, cancel }), done);
    }

    fn documents_guard<R>(
        &self,
        request: &Option<DocumentRequest>,
        work: impl FnOnce() -> R,
    ) -> Result<R, String> {
        let Some(request) = request else {
            return Ok(work());
        };
        let mut work = Some(work);
        let mut result = None;
        self.hooks
            .with_request_owner(&request.owner, &request.cancel, &mut || {
                let work = work.take().ok_or_else(|| shell_contract::IpcError {
                    name: Some("Error".into()),
                    message: "document admission was repeated".into(),
                })?;
                result = Some(work());
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        result.ok_or_else(|| "document request was not admitted".into())
    }

    fn documents_done(
        &self,
        request: &Option<DocumentRequest>,
        result: Result<Vec<OpenDocumentTab>, String>,
        done: Done,
    ) {
        done(
            self.documents_guard(request, || result)
                .and_then(|result| result),
        );
    }
    pub fn prepare_html_docx_export(
        &self,
        path: &str,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        use main_core::shell::tab_manager::TabKind;
        let target = self
            .with_manager(|manager, _| {
                let stale = manager.find_tab_of_kind_by_path(TabKind::Docs, Some(path))?;
                let active = manager
                    .list()
                    .into_iter()
                    .find(|tab| tab.active)
                    .map(|tab| tab.id);
                Some((stale, active))
            })
            .flatten();
        let Some((stale, active)) = target else {
            return done(Ok(true));
        };
        let state = self.rc();
        let path = path.to_owned();
        let closed = stale.clone();
        self.close_tab(
            &stale,
            Box::new(move |result| {
                if let Err(error) = result {
                    return done(Err(error));
                }
                let ready = state
                    .with_manager(|manager, host| {
                        if let Some(active) = active.as_deref().filter(|active| *active != closed) {
                            manager.activate_tab(host, active);
                        }
                        manager
                            .find_tab_of_kind_by_path(TabKind::Docs, Some(&path))
                            .is_none()
                    })
                    .unwrap_or(true);
                done(Ok(ready));
            }),
        );
    }

    pub fn is_detached_tab(&self, id: &str) -> bool {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            detached, bridge, ..
        } = &mut *inner;
        detached.is_detached_tab_id(bridge, id)
    }

    pub fn session_snapshot(&self) -> Result<(serde_json::Value, Vec<serde_json::Value>), String> {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            manager,
            detached,
            bridge,
            ..
        } = &mut *inner;
        let strip = manager
            .as_ref()
            .map(|manager| manager.session_tabs().to_value())
            .unwrap_or(serde_json::Value::Null);
        let detached = detached
            .detached_session_tabs(bridge)
            .iter()
            .map(|tab| tab.to_value())
            .collect();
        Ok((strip, detached))
    }

    pub fn file_moved(&self, old_path: &str, new_path: &str) {
        let mut affected = self
            .with_manager(|manager, host| manager.rename_tab_file(host, old_path, new_path))
            .unwrap_or_default();
        {
            let mut inner = self.inner.borrow_mut();
            let Inner {
                detached, bridge, ..
            } = &mut *inner;
            if let Some(tab) = detached.detached_rename_file(bridge, old_path, new_path) {
                affected.push(tab);
            }
            inner.drain();
        }
        for (_, view) in affected {
            if let Some(editor) = shell_contract::editors::editor_for(&self.provided, view) {
                editor.file_renamed(
                    view,
                    std::path::Path::new(old_path),
                    std::path::Path::new(new_path),
                );
            }
        }
    }

    /// The request origin is not the destination editor named by `need`.
    pub fn save_material_owned(
        &self,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        need: main_core::shell::generate::open_materials::SaveNeed,
        done: Box<dyn FnOnce(main_core::shell::generate::open_materials::SaveAnswer)>,
    ) {
        self.start_save_material(DocumentRequest { owner, cancel }, need, done);
    }

    fn start_save_material(
        &self,
        request: DocumentRequest,
        need: main_core::shell::generate::open_materials::SaveNeed,
        done: Box<dyn FnOnce(main_core::shell::generate::open_materials::SaveAnswer)>,
    ) {
        use main_core::shell::generate::open_materials::{SaveAnswer, SaveNeed};
        use main_core::shell::renderer_disposal::Failure;
        use shell_contract::editors::{SaveMode, editor_for};

        let (id, to_path) = match &need {
            SaveNeed::ToPath {
                web_contents_id, ..
            } => (*web_contents_id, true),
            SaveNeed::Editor {
                web_contents_id, ..
            } => (*web_contents_id, false),
        };
        if !id.is_finite() || id.fract() != 0.0 || id < 0.0 || id > u32::MAX as f64 {
            let error = Failure("invalid document view id".to_owned());
            done(if to_path {
                SaveAnswer::ToPath(Err(error))
            } else {
                SaveAnswer::Editor(Err(error))
            });
            return;
        }
        let view = ViewId(id as u32);
        let editor =
            self.documents_guard(&Some(request.clone()), || editor_for(&self.provided, view));
        let editor = match editor.and_then(|editor| {
            editor.ok_or_else(|| "document editor is no longer available".to_owned())
        }) {
            Ok(editor) => editor,
            Err(error) => {
                let error = Failure(error);
                done(if to_path {
                    SaveAnswer::ToPath(Err(error))
                } else {
                    SaveAnswer::Editor(Err(error))
                });
                return;
            }
        };
        let state = self.rc();
        match need {
            SaveNeed::ToPath { path, .. } => {
                let continuation = Some(request.clone());
                let then = Box::new(move |result: Result<(), shell_contract::IpcError>| {
                    let result = state
                        .documents_guard(&continuation, || result)
                        .map_err(Failure)
                        .and_then(|result| result.map_err(|error| Failure(error.to_string())));
                    done(SaveAnswer::ToPath(result));
                });
                editor.save_to_path_owned(view, path.into(), request.owner, request.cancel, then);
            }
            SaveNeed::Editor { .. } => {
                let continuation = Some(request.clone());
                let then = Box::new(move |result: Result<bool, shell_contract::IpcError>| {
                    let result = state
                        .documents_guard(&continuation, || result)
                        .map_err(Failure)
                        .and_then(|result| result.map_err(|error| Failure(error.to_string())));
                    done(SaveAnswer::Editor(result));
                });
                editor.request_save_owned(
                    view,
                    SaveMode::Save,
                    request.owner,
                    request.cancel,
                    then,
                );
            }
        }
    }

    pub fn tab_contents(&self, id: &str) -> Option<ViewId> {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            manager,
            detached,
            bridge,
            ..
        } = &mut *inner;
        manager
            .as_ref()
            .and_then(|manager| manager.web_contents_for_tab(id))
            .or_else(|| detached.detached_web_contents_for(bridge, id))
    }

    pub fn reveal_tab(&self, id: &str) -> bool {
        {
            let mut inner = self.inner.borrow_mut();
            let Inner {
                detached, bridge, ..
            } = &mut *inner;
            if detached.activate_detached(bridge, id) {
                return true;
            }
        }
        let found = self
            .with_manager(|manager, host| {
                if !manager.list().iter().any(|tab| tab.id == id) {
                    return false;
                }
                manager.activate_tab(host, id);
                true
            })
            .unwrap_or(false);
        if found {
            self.reveal_shell_window();
        }
        found
    }

    pub fn open_documents(&self, done: Done) {
        self.start_open_documents(None, done);
    }

    fn start_open_documents(&self, request: Option<DocumentRequest>, done: Done) {
        let step = self.documents_guard(&request, || {
            self.with_manager(|manager, host| manager.open_documents(host))
        });
        match step {
            Ok(Some(step)) => self.drive_open_documents(step, request, done),
            Ok(None) => self.start_detached_documents(Vec::new(), request, done),
            Err(error) => done(Err(error)),
        }
    }

    fn query_documents_dirty(
        &self,
        view: ViewId,
        request: &Option<DocumentRequest>,
        done: shell_contract::editors::Then<bool>,
    ) {
        match request {
            Some(request) => self.hooks.docs_query_dirty_owned(
                view,
                request.owner.clone(),
                request.cancel.clone(),
                done,
            ),
            None => self.hooks.docs_query_dirty(view, done),
        }
    }

    fn drive_open_documents(
        &self,
        step: OpenDocumentsStep<ViewId, OpenDocumentsRun<ViewId>>,
        request: Option<DocumentRequest>,
        done: Done,
    ) {
        match step {
            OpenDocumentsStep::Done(tabs) => self.start_detached_documents(tabs, request, done),
            OpenDocumentsStep::Rejected(error) => self.documents_done(&request, Err(error), done),
            OpenDocumentsStep::QueryDocsDirty { view, next } => {
                let state = self.rc();
                let continuation = request.clone();
                self.query_documents_dirty(
                    view,
                    &request,
                    Box::new(move |answer| {
                        let step = state.documents_guard(&continuation, || {
                            state.with_manager(|manager, host| {
                                manager.resume_open_documents(
                                    host,
                                    next,
                                    answer.map_err(|error| error.to_string()),
                                )
                            })
                        });
                        match step {
                            Ok(Some(step)) => state.drive_open_documents(step, continuation, done),
                            Ok(None) => {
                                done(Err("shell window closed while listing documents".into()))
                            }
                            Err(error) => done(Err(error)),
                        }
                    }),
                );
            }
        }
    }

    fn start_detached_documents(
        &self,
        tabs: Vec<OpenDocumentTab>,
        request: Option<DocumentRequest>,
        done: Done,
    ) {
        let step = self.documents_guard(&request, || {
            let mut inner = self.inner.borrow_mut();
            let Inner {
                detached, bridge, ..
            } = &mut *inner;
            detached.detached_open_documents(bridge)
        });
        match step {
            Ok(step) => self.drive_detached_documents(step, tabs, request, done),
            Err(error) => done(Err(error)),
        }
    }

    fn drive_detached_documents(
        &self,
        step: OpenDocumentsStep<ViewId, DetachedOpenDocumentsRun>,
        mut tabs: Vec<OpenDocumentTab>,
        request: Option<DocumentRequest>,
        done: Done,
    ) {
        match step {
            OpenDocumentsStep::Done(detached) => {
                tabs.extend(detached);
                self.documents_done(&request, Ok(tabs), done);
            }
            OpenDocumentsStep::Rejected(error) => self.documents_done(&request, Err(error), done),
            OpenDocumentsStep::QueryDocsDirty { view, next } => {
                let state = self.rc();
                let continuation = request.clone();
                self.query_documents_dirty(
                    view,
                    &request,
                    Box::new(move |answer| {
                        let step = state.documents_guard(&continuation, || {
                            let mut inner = state.inner.borrow_mut();
                            let Inner {
                                detached, bridge, ..
                            } = &mut *inner;
                            detached.resume_detached_open_documents(
                                bridge,
                                next,
                                answer.map_err(|error| error.to_string()),
                            )
                        });
                        match step {
                            Ok(step) => {
                                state.drive_detached_documents(step, tabs, continuation, done)
                            }
                            Err(error) => done(Err(error)),
                        }
                    }),
                );
            }
        }
    }
}
