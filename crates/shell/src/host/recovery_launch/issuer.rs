use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use main_core::shell::recovery_shell::{RestoredTabOpener, UnloadedTabSpec};
use shell_contract::Surface;
use shell_contract::ipc::{
    NativeInvocationOperation, NativeInvocationPurpose, NativeInvocationSelection,
    NativeRecoveryCopy, NativeRecoveryDecision,
};
use std::rc::Rc;

/// The factory is retained only by the native startup recovery runtime.
pub(crate) type RecoveryIssuer = Rc<
    dyn Fn(
        NativeInvocationSelection,
        shell_contract::runtime::CancelToken,
    ) -> Result<NativeInvocationOperation, String>,
>;

pub(crate) type StartupIssuer = Rc<
    dyn Fn(
        NativeInvocationSelection,
        shell_contract::runtime::CancelToken,
    ) -> Result<NativeInvocationOperation, String>,
>;

pub(crate) fn issue_startup(
    issuer: &StartupIssuer,
    paths: &[String],
    reopened_by_installer: bool,
    shutdown: &shell_contract::runtime::CancelToken,
) -> Result<NativeInvocationOperation, String> {
    let selection = NativeInvocationSelection::StartupLaunch {
        paths: paths.iter().map(std::path::PathBuf::from).collect(),
        reopened_by_installer,
    };
    let cancel = shell_contract::runtime::CancelToken::default();
    let operation = issuer(selection.clone(), cancel.clone())?;
    if operation.selection() != &selection
        || operation.purpose() != NativeInvocationPurpose::StartupLaunch
        || !operation.cancel().same_token(&cancel)
        || !operation.shutdown().same_token(shutdown)
    {
        operation.finish();
        return Err(
            "The startup issuer returned a different selected operation or lifetime".into(),
        );
    }
    if operation.with_active(|| ()).is_none() {
        operation.finish();
        return Err("The startup operation is no longer active".into());
    }
    Ok(operation)
}

fn selection(spec: &UnloadedTabSpec) -> Result<NativeInvocationSelection, String> {
    let surface = match spec.kind.as_str() {
        "docs" => Surface::Docs,
        "sheets" => Surface::Sheets,
        "slides" => Surface::Slides,
        "pdf" => Surface::Pdf,
        "markdown" => Surface::Markdown,
        "html" => Surface::Html,
        "mindmap" => Surface::Mindmap,
        "hwp" => Surface::Hwp,
        "diagram" => Surface::Diagram,
        "epub" => Surface::Epub,
        _ => return Err(format!("Unknown recovery editor kind {}", spec.kind)),
    };
    let selection = match &spec.opener {
        RestoredTabOpener::File => {
            match spec.file_path.as_deref().filter(|path| !path.is_empty()) {
                Some(path) => NativeInvocationSelection::RecoveryFile {
                    surface,
                    path: path.into(),
                    decision: NativeRecoveryDecision::Original,
                },
                None => NativeInvocationSelection::RecoveryBlank {
                    surface,
                    title: spec.title.clone(),
                },
            }
        }
        RestoredTabOpener::UntitledCopy {
            copy,
            save_as_default_path,
        } => {
            if copy.kind() != spec.kind {
                return Err("The recovery copy belongs to another editor kind".into());
            }
            let copy = match copy {
                RecoveryIdentity::File(file) => NativeRecoveryCopy::File {
                    path: file.file_path.clone().into(),
                },
                RecoveryIdentity::Untitled(copy) => NativeRecoveryCopy::Untitled {
                    id: copy.untitled_id.clone(),
                    ext: copy.ext.clone(),
                },
            };
            NativeInvocationSelection::RecoveryCopy {
                surface,
                copy,
                title: spec.title.clone(),
                save_as_default: save_as_default_path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .map(Into::into),
            }
        }
    };
    Ok(selection)
}

fn issue(
    issuer: &RecoveryIssuer,
    selection: &NativeInvocationSelection,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<NativeInvocationOperation, String> {
    let operation = issuer(selection.clone(), cancel.clone())?;
    if operation.purpose() != NativeInvocationPurpose::Recovery
        || operation.selection() != selection
        || !operation.cancel().same_token(&cancel)
    {
        operation.finish();
        return Err(
            "The recovery issuer returned a different selected operation or lifetime".into(),
        );
    }
    if operation.with_active(|| ()).is_none() {
        operation.finish();
        return Err("The selected recovery plan is no longer active".into());
    }
    Ok(operation)
}

pub(crate) fn capture(
    issuer: &RecoveryIssuer,
    spec: &UnloadedTabSpec,
) -> Result<NativeInvocationOperation, String> {
    issue(
        issuer,
        &selection(spec)?,
        shell_contract::runtime::CancelToken::default(),
    )
}

pub(crate) struct RecoveryPlan {
    issuer: RecoveryIssuer,
    selection: NativeInvocationSelection,
    cancel: shell_contract::runtime::CancelToken,
    guard: NativeInvocationOperation,
}

impl RecoveryPlan {
    pub(crate) fn capture(issuer: &RecoveryIssuer, spec: &UnloadedTabSpec) -> Result<Self, String> {
        let selection = selection(spec)?;
        let cancel = shell_contract::runtime::CancelToken::default();
        let guard = issue(issuer, &selection, cancel.clone())?;
        Ok(Self {
            issuer: issuer.clone(),
            selection,
            cancel,
            guard,
        })
    }

    pub(crate) fn guard(&self) -> NativeInvocationOperation {
        self.guard.clone()
    }
    /// The selection an open of this plan is issued for. A plan is captured before the launch
    /// list is answered, while its file is still named as saved; `decision` is what the answer
    /// this launch accepted made of that file. Only the decision of a file selection follows
    /// it: the surface and the path stay as captured, and so does every other kind of
    /// selection.
    pub(crate) fn selection_for(
        &self,
        decision: NativeRecoveryDecision,
    ) -> NativeInvocationSelection {
        match &self.selection {
            NativeInvocationSelection::RecoveryFile { surface, path, .. } => {
                NativeInvocationSelection::RecoveryFile {
                    surface: *surface,
                    path: path.clone(),
                    decision,
                }
            }
            captured => captured.clone(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
        self.guard.finish();
    }

    pub(crate) fn attempt(
        &self,
        decision: NativeRecoveryDecision,
    ) -> Result<NativeInvocationOperation, String> {
        if self.guard.with_active(|| ()).is_none() {
            return Err("The selected recovery plan is no longer active".into());
        }
        let selection = self.selection_for(decision);
        let child = issue(&self.issuer, &selection, self.cancel.clone())?;
        if self.guard.with_active(|| ()).is_none() {
            child.finish();
            return Err("The selected recovery plan is no longer active".into());
        }
        Ok(child)
    }
}

impl Drop for RecoveryPlan {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
pub(crate) fn test_startup(
    issuer: &shell_contract::ipc::NativeInvocationIssuer,
) -> NativeInvocationOperation {
    let generation = shell_contract::runtime::RendererGeneration::new();
    let owner = shell_contract::ipc::NativeRequestOwner {
        view: shell_contract::ViewId(1),
        context: shell_contract::ipc::NativeContext::with_owner_loss(
            (),
            generation,
            shell_contract::runtime::CancelToken::default(),
        ),
    };
    issuer.issue_startup(
        Vec::new(),
        false,
        owner,
        shell_contract::runtime::CancelToken::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryIdentity, RecoveryUntitledIdentity,
    };
    use shell_contract::ipc::{NativeInvocationIssuer, NativeRecoveryCopy};
    use shell_contract::runtime::CancelToken;

    #[test]
    fn selection_captures_copy_title_and_save_suggestion() {
        let issuer = Rc::new(NativeInvocationIssuer::new(CancelToken::default()));
        let startup = test_startup(&issuer);
        let root = startup.clone();
        let factory: RecoveryIssuer =
            Rc::new(move |selection, cancel| issuer.issue_recovery_child(&root, selection, cancel));
        let spec = UnloadedTabSpec {
            kind: "docs".into(),
            title: "Selected".into(),
            file_path: None,
            opener: RestoredTabOpener::UntitledCopy {
                copy: RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
                    kind: "docs".into(),
                    untitled_id: "original-id".into(),
                    ext: ".docx".into(),
                }),
                save_as_default_path: Some("/original/suggestion.docx".into()),
            },
        };
        let operation = capture(&factory, &spec).unwrap();
        assert!(operation.is_child_of(&startup));
        assert!(
            matches!(operation.selection(),NativeInvocationSelection::RecoveryCopy {
            surface:Surface::Docs,copy:NativeRecoveryCopy::Untitled { id,ext },title,save_as_default:Some(path)
        } if id=="original-id" && ext==".docx" && title=="Selected" && path==std::path::Path::new("/original/suggestion.docx"))
        );
        operation.finish();
    }

    #[test]
    fn rejects_issuer_that_relabels_selected_operation() {
        let issuer = Rc::new(NativeInvocationIssuer::new(CancelToken::default()));
        let startup = test_startup(&issuer);
        let factory: RecoveryIssuer = Rc::new(move |_, cancel| {
            issuer.issue_recovery_child(
                &startup,
                NativeInvocationSelection::RecoveryBlank {
                    surface: Surface::Docs,
                    title: "different".into(),
                },
                cancel,
            )
        });
        let spec = UnloadedTabSpec {
            kind: "docs".into(),
            title: "Selected".into(),
            file_path: None,
            opener: RestoredTabOpener::File,
        };
        assert!(capture(&factory, &spec).is_err());
    }

    /// A file's plan is captured before the launch list is answered. The open is issued for
    /// what the answer made of that file: its recovery copy, or the file as saved. The answer
    /// changes nothing else, and nothing of a plan that names no file.
    #[test]
    fn an_open_follows_the_launch_decision_of_its_file_and_nothing_else() {
        let issuer = Rc::new(NativeInvocationIssuer::new(CancelToken::default()));
        let startup = test_startup(&issuer);
        let factory: RecoveryIssuer = Rc::new(move |selection, cancel| {
            issuer.issue_recovery_child(&startup, selection, cancel)
        });
        let spec = |file_path: Option<&str>, opener| UnloadedTabSpec {
            kind: "markdown".into(),
            title: "notes.md".into(),
            file_path: file_path.map(str::to_owned),
            opener,
        };
        let decided = |decision| NativeInvocationSelection::RecoveryFile {
            surface: Surface::Markdown,
            path: "/work/notes.md".into(),
            decision,
        };

        let file = spec(Some("/work/notes.md"), RestoredTabOpener::File);
        let plan = RecoveryPlan::capture(&factory, &file).unwrap();
        assert_eq!(
            plan.guard().selection(),
            &decided(NativeRecoveryDecision::Original)
        );
        for decision in [
            NativeRecoveryDecision::Restore,
            NativeRecoveryDecision::Original,
        ] {
            let operation = plan.attempt(decision).unwrap();
            assert_eq!(operation.selection(), &decided(decision));
            operation.finish();
        }

        let blank = spec(None, RestoredTabOpener::File);
        let copy = spec(
            None,
            RestoredTabOpener::UntitledCopy {
                copy: RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
                    kind: "markdown".into(),
                    untitled_id: "copy-id".into(),
                    ext: ".md".into(),
                }),
                save_as_default_path: None,
            },
        );
        for spec in [blank, copy] {
            let plan = RecoveryPlan::capture(&factory, &spec).unwrap();
            let captured = plan.guard().selection().clone();
            let operation = plan.attempt(NativeRecoveryDecision::Restore).unwrap();
            assert_eq!(operation.selection(), &captured);
            operation.finish();
        }
    }

    #[test]
    fn os_launch_issues_exact_paths_and_obeys_shutdown() {
        let shutdown = CancelToken::default();
        let issuer = Rc::new(NativeInvocationIssuer::new(shutdown.clone()));
        let shell = test_startup(&issuer).startup_owner().unwrap().clone();
        let factory: StartupIssuer = Rc::new(move |selection, cancel| match selection {
            NativeInvocationSelection::StartupLaunch {
                paths,
                reopened_by_installer,
            } => Ok(issuer.issue_startup(paths, reopened_by_installer, shell.clone(), cancel)),
            _ => Err("not startup".into()),
        });
        let first = issue_startup(&factory, &["/one.docx".into()], false, &shutdown).unwrap();
        let second = issue_startup(&factory, &["/two.pdf".into()], false, &shutdown).unwrap();
        assert_ne!(first.selection(), second.selection());
        assert!(
            matches!(first.selection(),NativeInvocationSelection::StartupLaunch { paths,reopened_by_installer:false }
            if paths==&[std::path::PathBuf::from("/one.docx")])
        );
        first.finish();
        assert!(second.with_active(|| ()).is_some());
        shutdown.cancel();
        assert!(second.with_active(|| ()).is_none());
        assert!(issue_startup(&factory, &["/late.docx".into()], false, &shutdown).is_err());
    }
}
