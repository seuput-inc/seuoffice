//! Recovery's core-thread access to the installed document editors.

use std::rc::Rc;
use std::sync::Arc;

use main_core::packages::electron_utils::recovery_contract::{
    RecoveryEditorContract, RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use main_core::shell::recovery_shell::RecoveryEditors;
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::tab_manager::TabKind;
use shell_contract::runtime::Services;
use shell_contract::{Provided, ViewId};

use super::recovery_manager::{Contents, SharedStore};
use crate::views::ViewTable;

pub(crate) struct Editors {
    pub services: Services,
    pub provided: Rc<Provided>,
    pub views: Arc<ViewTable>,
    pub store: Rc<SharedStore>,
    pub issuer: super::recovery_launch::RecoveryIssuer,
    pub owner: shell_contract::WindowId,
    pub startup: Option<shell_contract::ipc::StartupRecoveryConstraint>,
}

fn view(id: i64) -> Result<ViewId, Failure> {
    u32::try_from(id)
        .map(ViewId)
        .map_err(|_| Failure("invalid recovery view id".into()))
}

struct DirectRecovery {
    id: u64,
    retained: bool,
}
impl DirectRecovery {
    fn retain_pending(&mut self) -> u64 {
        self.retained = true;
        self.id
    }
}
impl Drop for DirectRecovery {
    fn drop(&mut self) {
        if !self.retained {
            super::recovery_openers::complete_direct(self.id);
        }
    }
}

impl RecoveryEditors for Editors {
    type Contents = Contents;
    type View = ViewId;

    fn docs_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        let editor = self
            .provided
            .all::<shell_ipc_d2::docs::editors::DocsEditors>()
            .into_iter()
            .next()
            .ok_or_else(|| Failure("docs editor is unavailable".into()))?;
        Ok(editor
            .recovery_identity(view(id)?)
            .map(RecoveryIdentity::Untitled))
    }
    fn sheets_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        Ok(shell_ipc_d1::sheets::views()
            .ok_or_else(|| Failure("sheets editor is unavailable".into()))?
            .recovery_identity(view(id)?))
    }
    fn pdf_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        let editor = self
            .provided
            .all::<shell_ipc_d2::pdf::editor::editors::PdfEditors>()
            .into_iter()
            .next()
            .ok_or_else(|| Failure("pdf editor is unavailable".into()))?;
        Ok(editor.recovery_identity(view(id)?))
    }
    fn markdown_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        let editor = self
            .provided
            .all::<shell_ipc_d2::markdown::MarkdownEditors>()
            .into_iter()
            .next()
            .ok_or_else(|| Failure("markdown editor is unavailable".into()))?;
        Ok(editor.recovery_identity(view(id)?))
    }
    fn html_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        let editor = self
            .provided
            .all::<shell_ipc_d2::html::HtmlEditors>()
            .into_iter()
            .next()
            .ok_or_else(|| Failure("html editor is unavailable".into()))?;
        Ok(editor.recovery_identity(view(id)?))
    }
    fn mindmap_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        Ok(shell_ipc_d1::mindmap::handle()
            .ok_or_else(|| Failure("mindmap editor is unavailable".into()))?
            .recovery_identity(view(id)?))
    }
    fn hwp_recovery_identity(
        &self,
        contents: &Contents,
    ) -> Result<Option<RecoveryIdentity>, Failure> {
        let editor = self
            .provided
            .all::<shell_ipc_d2::hwp::HwpEditors>()
            .into_iter()
            .next()
            .ok_or_else(|| Failure("hwp editor is unavailable".into()))?;
        Ok(editor.recovery_identity(contents.view))
    }
    fn slides_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        Ok(shell_ipc_d1::slides::views()
            .ok_or_else(|| Failure("slides editor is unavailable".into()))?
            .recovery_identity(view(id)?))
    }
    fn diagram_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure> {
        Ok(shell_ipc_d1::diagram::views()
            .ok_or_else(|| Failure("diagram editor is unavailable".into()))?
            .recovery_identity(view(id)?))
    }

    fn discard_docs_recovery(&self, contents: &Contents) {
        if let Some(editor) = self
            .provided
            .all::<shell_ipc_d2::docs::editors::DocsEditors>()
            .into_iter()
            .next()
        {
            editor.discard_recovery(contents.view);
        } else {
            eprintln!("[recovery] docs editor is unavailable");
        }
    }
    fn discard_sheets_recovery(&self, contents: &Contents) {
        if let Some(editor) = shell_ipc_d1::sheets::views() {
            editor.discard_recovery(contents.view);
        } else {
            eprintln!("[recovery] sheets editor is unavailable");
        }
    }
    fn discard_pdf_recovery(&self, contents: &Contents) {
        if let Some(editor) = self
            .provided
            .all::<shell_ipc_d2::pdf::editor::editors::PdfEditors>()
            .into_iter()
            .next()
        {
            editor.discard_recovery(contents.view);
        } else {
            eprintln!("[recovery] pdf editor is unavailable");
        }
    }
    fn discard_mindmap_recovery(&self, contents: &Contents) {
        if let Some(editor) = shell_ipc_d1::mindmap::handle() {
            editor.discard_recovery(contents.view);
        } else {
            eprintln!("[recovery] mindmap editor is unavailable");
        }
    }
    fn hwp_discard_recovery_copy(&self, contents: &Contents) {
        if let Some(editor) = self
            .provided
            .all::<shell_ipc_d2::hwp::HwpEditors>()
            .into_iter()
            .next()
        {
            editor.discard_recovery_copy(contents.view);
        } else {
            eprintln!("[recovery] hwp editor is unavailable");
        }
    }
    fn discard_diagram_recovery(&self, contents: &Contents) {
        if let Some(editor) = shell_ipc_d1::diagram::views() {
            editor.discard_recovery(contents.view);
        } else {
            eprintln!("[recovery] diagram editor is unavailable");
        }
    }
    fn open_recovered_view(
        &self,
        kind: &str,
        title: &str,
        file_path: Option<&str>,
        copy: Option<&RecoveryOpenUntitledRequest>,
        done: Box<dyn FnOnce(Result<ViewId, Failure>)>,
    ) {
        let Some(tab_kind) = TabKind::parse(kind) else {
            return done(Err(Failure(format!("unknown recovery editor kind {kind}"))));
        };
        let spec = main_core::shell::recovery_shell::UnloadedTabSpec {
            kind: kind.to_owned(),
            title: title.to_owned(),
            file_path: file_path.map(str::to_owned),
            opener: match copy {
                Some(copy) => main_core::shell::recovery_shell::RestoredTabOpener::UntitledCopy {
                    copy: copy.copy.clone(),
                    save_as_default_path: copy.save_as_default_path.clone(),
                },
                None => main_core::shell::recovery_shell::RestoredTabOpener::File,
            },
        };
        let Some(startup) = self.startup.clone() else {
            return done(Err(Failure(
                "the captured startup recovery authority is unavailable".into(),
            )));
        };
        let operation = match super::recovery_launch::issuer::capture(&self.issuer, &spec) {
            Ok(operation) => operation,
            Err(error) => return done(Err(Failure(error))),
        };
        let editor = self
            .provided
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == kind);
        if copy.is_some() && editor.is_none() {
            operation.finish();
            return done(Err(Failure(format!(
                "the {kind} editor cannot open a recovery copy"
            ))));
        }
        if copy.is_none()
            && file_path.is_some_and(|path| !path.is_empty())
            && editor.is_none()
            && main_core::packages::electron_utils::recovery_store::is_recovery_editor_kind(kind)
        {
            operation.finish();
            return done(Err(Failure(format!(
                "the {kind} recovery editor is unavailable"
            ))));
        }
        let rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore = Rc::default();
        let id = match super::recovery_openers::register_direct(
            self.owner,
            operation.clone(),
            rollback.clone(),
        ) {
            Ok(id) => id,
            Err(error) => {
                operation.finish();
                return done(Err(Failure(error)));
            }
        };
        let untitled = copy.is_some();
        let copy = copy.cloned();
        let request=file_path.filter(|path|!path.is_empty()).map(|path| main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest {
            file_path:path.to_owned(),decision:main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Original,
        });
        let blank_docs = if tab_kind == TabKind::Docs && copy.is_none() && request.is_none() {
            self.provided
                .all::<shell_ipc_d2::docs::editors::DocsEditors>()
                .into_iter()
                .next()
        } else {
            None
        };
        let epub = if tab_kind == TabKind::Epub && copy.is_none() && request.is_some() {
            self.provided
                .all::<shell_ipc_d2::epub::EpubEditors>()
                .into_iter()
                .next()
        } else {
            None
        };
        let title = title.to_owned();
        let before=Box::new(move |view:ViewId,target:&shell_contract::ipc::RecoveryOpenTarget,finish:main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone| {
            match (editor,copy,request,blank_docs,epub) {
                (Some(editor),Some(copy),_,_,_)=>editor.open_untitled_copy(&view,&copy,target,finish),
                (Some(editor),None,Some(request),_,_)=>editor.open_file_with_decision(&view,&request,target,finish),
                (_,None,Some(request),_,Some(editor))=>editor.queue_recovery_direct(view,target,request.file_path,finish),
                (_,None,None,Some(editor),_)=>editor.queue_blank_recovery(view,target,title,finish),
                (_,None,_,_,_)=>finish(Err(Failure(format!("the {} recovery initialization is unavailable",tab_kind.as_str())))),
                (None,Some(_),_,_,_)=>finish(Err(Failure("The recovery editor is unavailable".into()))),
            }
        });
        let completion = Rc::new(std::cell::RefCell::new(Some((
            done,
            DirectRecovery {
                id,
                retained: false,
            },
        ))));
        let completed = completion.clone();
        let core = self.services.core.clone();
        let immediate_core = self.services.core.clone();
        let adoption_tabs = self
            .provided
            .all::<shell_ipc_w::tabs::TabsState>()
            .into_iter()
            .next();
        let adopted_rollback = rollback.clone();
        let result = super::w_tabs::create_recovery_view_owned(
            &self.services,
            self.provided.clone(),
            tab_kind,
            file_path,
            untitled,
            shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation),
            None,
            Some(startup),
            None,
            rollback,
            before,
            Box::new(move |result| {
                let entry = completed.borrow_mut().take();
                let Some((done, mut guard)) = entry else {
                    return;
                };
                let id = guard.retain_pending();
                match result {
                    Ok(ready) => {
                        let (owner, cleanup, binding) = ready.into_parts();
                        let adoption =
                            match (owner.context.renderer().cloned(), adoption_tabs.as_ref()) {
                                (Some(created), Some(tabs)) => tabs
                                    .retain_adopted_editor(
                                        owner.view,
                                        created,
                                        binding.reservation,
                                        binding.owner,
                                        binding.closure,
                                        adopted_rollback,
                                    )
                                    .map_err(|failure| {
                                        let error = failure.message().to_owned();
                                        let lifetime = match failure {
                                        shell_ipc_w::tabs::AdoptedLifetimeFailure::CallerOwned(
                                            input,
                                        ) => super::recovery_openers::DirectLifetime::CallerOwned(
                                            input,
                                        ),
                                        shell_ipc_w::tabs::AdoptedLifetimeFailure::Retained(
                                            retained,
                                        ) => super::recovery_openers::DirectLifetime::Retained(
                                            retained.into_handle(),
                                        ),
                                    };
                                        (error, Some(lifetime))
                                    }),
                                (None, _) => {
                                    Err(("The recovery renderer is unavailable".into(), None))
                                }
                                (_, None) => Err(("The tab manager is unavailable".into(), None)),
                            };
                        match adoption {
                            Ok(handle) => {
                                let active = handle.is_unclosed().and_then(|unclosed| {
                                    if unclosed
                                        && owner
                                            .loss_token()
                                            .is_some_and(|loss| !loss.is_cancelled())
                                    {
                                        Ok(())
                                    } else {
                                        Err("The recovery editor closed before adoption completed"
                                            .to_owned())
                                    }
                                });
                                match active {
                                    Ok(()) => {
                                        done(Ok(owner.view));
                                        super::recovery_openers::complete_direct(id);
                                    }
                                    Err(error) => super::recovery_openers::fail_direct(
                                        id,
                                        Failure(error),
                                        shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(
                                            cleanup,
                                        ),
                                        Some(super::recovery_openers::DirectLifetime::Retained(
                                            handle,
                                        )),
                                        core,
                                        done,
                                    ),
                                }
                            }
                            Err((error, lifetime)) => super::recovery_openers::fail_direct(
                                id,
                                Failure(error),
                                shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup),
                                lifetime,
                                core,
                                done,
                            ),
                        }
                    }
                    Err(error) => {
                        super::recovery_openers::fail_direct(
                            id,
                            Failure(error.message),
                            error.cleanup,
                            None,
                            core,
                            done,
                        );
                    }
                }
            }),
        );
        if let Err(error) = result {
            let entry = completion.borrow_mut().take();
            if let Some((done, mut guard)) = entry {
                let id = guard.retain_pending();
                super::recovery_openers::fail_direct(
                    id,
                    Failure(error),
                    shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete,
                    None,
                    immediate_core,
                    done,
                );
            }
        }
    }
    fn has_recovery_editor(&self, kind: &str) -> bool {
        self.provided
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .any(|editor| editor.kind() == kind)
    }
    fn warn(&self, prefix: &str, detail: &str) {
        eprintln!("{prefix} {detail}");
    }
    fn after_sheets_open_soon(&self) {
        self.services.core.post(Box::new(|| {
            if let Err(error) = super::w_tabs::nudge_recovery_sheets() {
                eprintln!("[recovery] {error}");
            }
        }));
    }
    fn set_recovery_title_source(&self, editor: &str) {
        let Some(tabs) = self
            .provided
            .all::<shell_ipc_w::tabs::TabsState>()
            .into_iter()
            .next()
        else {
            eprintln!("[recovery] tab manager is unavailable");
            return;
        };
        match editor {
            "markdown" => {
                if let Some(editor) = self
                    .provided
                    .all::<shell_ipc_d2::markdown::MarkdownEditors>()
                    .into_iter()
                    .next()
                {
                    editor.set_recovery_title_source(Some(Box::new(move |view| {
                        tabs.details_of(view).map(|details| details.title)
                    })));
                } else {
                    eprintln!("[recovery] markdown editor is unavailable");
                }
            }
            "html" => {
                if let Some(editor) = self
                    .provided
                    .all::<shell_ipc_d2::html::HtmlEditors>()
                    .into_iter()
                    .next()
                {
                    editor.set_recovery_title_source(Some(Box::new(move |view| {
                        tabs.details_of(view).map(|details| details.title)
                    })));
                } else {
                    eprintln!("[recovery] html editor is unavailable");
                }
            }
            "mindmap" if shell_ipc_d1::mindmap::handle().is_some() => {}
            "diagram" if shell_ipc_d1::diagram::views().is_some() => {}
            _ => eprintln!("[recovery] unknown title source {editor}"),
        }
    }
    fn set_hwp_recovery_title(&self, contents: &Contents, title: &str) {
        if let Some(editor) = self
            .provided
            .all::<shell_ipc_d2::hwp::HwpEditors>()
            .into_iter()
            .next()
        {
            editor.set_recovery_title(contents.view, title);
        } else {
            eprintln!("[recovery] hwp editor is unavailable");
        }
    }
}
