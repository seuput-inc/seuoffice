use super::*;
use crate::tabs::owned_saved::SavedAuthority;
use shell_contract::hosts::OwnedEffectsHost;

#[derive(Clone)]
struct CapturedDocsClose {
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    target: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
}

impl TabsState {
    pub fn prepare_html_docx_export_owned(
        &self,
        path: &std::path::Path,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        use main_core::shell::tab_manager::TabKind;
        let authority =
            SavedAuthority::new(origin, destination, Vec::new(), cancel, pending, effects);
        if let Err(error) = authority.check() {
            return done(Err(error));
        }
        let path = path.to_string_lossy().into_owned();
        let selection = self
            .with_manager(|manager, _| {
                let stale = manager.find_tab_of_kind_by_path(TabKind::Docs, Some(&path))?;
                let view = manager.web_contents_for_tab(&stale)?;
                let active = manager
                    .list()
                    .into_iter()
                    .find(|tab| tab.active && tab.id != stale)
                    .and_then(|tab| {
                        manager
                            .web_contents_for_tab(&tab.id)
                            .map(|view| (tab.id, view))
                    });
                Some((stale, view, active))
            })
            .flatten();
        let Some((stale, view, active)) = selection else {
            return done(Ok(true));
        };
        let capture = |view| {
            let epoch = self
                .services
                .views
                .renderer_generation(view)
                .ok_or("The captured export tab is unavailable")?;
            authority.effects.capture_owner(view, &epoch)
        };
        let native = match capture(view) {
            Ok(native) => native,
            Err(error) => return done(Err(error)),
        };
        let target = match shell_contract::editors::editor_for(&self.provided, view)
            .ok_or_else(|| "The stale Docs editor is unavailable".to_owned())
            .and_then(|editor| {
                editor
                    .capture_document_owner(view, native)
                    .map_err(|error| error.to_string())
            }) {
            Ok(target) => target,
            Err(error) => return done(Err(error)),
        };
        let active = match active
            .map(|(id, view)| capture(view).map(|owner| (id, owner)))
            .transpose()
        {
            Ok(active) => active,
            Err(error) => return done(Err(error)),
        };
        let state = self.this.clone();
        let retained = authority.clone();
        self.close_docs_owned_many(
            &stale,
            authority.origin,
            authority.destination,
            target,
            authority.cancel,
            authority.pending,
            authority.effects,
            Box::new(move |result| {
                if let Err(error) = result {
                    return done(Err(error));
                }
                let Some(state) = state.upgrade() else {
                    return done(Ok(true));
                };
                let finish_state = state.this.clone();
                let finish = Box::new(move |result: Result<(), String>| {
                    if let Err(error) = result {
                        return done(Err(error));
                    }
                    let ready = finish_state
                        .upgrade()
                        .and_then(|state| {
                            state.with_manager(|manager, _| {
                                manager
                                    .find_tab_of_kind_by_path(TabKind::Docs, Some(&path))
                                    .is_none()
                            })
                        })
                        .unwrap_or(true);
                    done(Ok(ready));
                });
                match active {
                    Some((id, active)) if state.shell_window().is_some() => state
                        .activate_loaded_owned_many(
                            &id,
                            retained.origin,
                            active,
                            vec![retained.destination],
                            retained.cancel,
                            retained.pending,
                            finish,
                        ),
                    _ => finish(Ok(())),
                }
            }),
        );
    }

    /// Close one captured Docs tab: the dirty query and the close request run
    /// under the captured origin, destination and document owner, and the
    /// manager removes the tab only on the editor's confirmation.
    pub(crate) fn close_docs_owned_many(
        &self,
        id: &str,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        target: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let authority = SavedAuthority::new(
            origin.clone(),
            destination.clone(),
            vec![target.clone()],
            cancel.clone(),
            pending.clone(),
            effects,
        );
        if let Err(error) = authority.check() {
            return done(Err(error));
        }
        if self.tab_contents(id) != Some(target.view) {
            return done(Err(
                "The captured Docs tab no longer matches its document".into()
            ));
        }
        let captured = CapturedDocsClose {
            origin,
            destination,
            target,
            cancel,
            pending,
        };
        let step = self
            .inner
            .borrow_mut()
            .manager_call(|manager, host| manager.close_tab(host, id));
        match step {
            None => done(Ok(())),
            Some(step) => self.drive_docs_close_captured(step, captured, done),
        }
    }

    fn drive_docs_close_captured(
        &self,
        step: main_core::shell::tab_manager::TabCloseStep<ViewId>,
        captured: CapturedDocsClose,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        use main_core::shell::tab_manager::TabCloseStep;
        let CapturedDocsClose {
            origin,
            destination,
            target,
            cancel,
            pending,
        } = captured.clone();
        match step {
            TabCloseStep::Done => done(Ok(())),
            TabCloseStep::Rejected(message) => done(Err(message)),
            TabCloseStep::QueryDocsDirty { id, view } => {
                if view != target.view {
                    return self.resume_docs_close_captured(
                        id,
                        Err("The captured Docs tab changed during close".into()),
                        captured,
                        done,
                    );
                }
                let state = self.rc();
                self.hooks.docs_query_dirty_captured_many(
                    view,
                    origin,
                    vec![destination],
                    target,
                    cancel,
                    pending,
                    Box::new(move |answer| {
                        let answer = answer.map_err(|error| error.message);
                        state.resume_docs_close_captured(id, answer, captured, done)
                    }),
                );
            }
            TabCloseStep::RequestClose { id, view, .. } => {
                if view != target.view {
                    return self.resume_docs_close_captured(
                        id,
                        Err("The captured Docs tab changed during close".into()),
                        captured,
                        done,
                    );
                }
                let state = self.rc();
                let parent = self.inner.borrow().bridge.shell.map(|shell| shell.window);
                self.hooks.docs_request_close_captured_many(
                    view,
                    parent,
                    origin,
                    vec![destination],
                    target,
                    cancel,
                    pending,
                    Box::new(move |answer| {
                        let answer = answer.map_err(|error| error.message);
                        state.resume_docs_close_captured(id, answer, captured, done)
                    }),
                );
            }
        }
    }

    fn resume_docs_close_captured(
        &self,
        id: String,
        answer: Result<bool, String>,
        captured: CapturedDocsClose,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let step = self
            .inner
            .borrow_mut()
            .manager_call(|manager, host| manager.resume_close(host, &id, answer))
            .unwrap_or(main_core::shell::tab_manager::TabCloseStep::Done);
        self.drive_docs_close_captured(step, captured, done);
    }
}
