//! Saved-document recovery prompt bound to the original native window.
use std::sync::Arc;

use main_core::packages::electron_utils::recovery_labels::recovery_prompt_labels;
use main_core::packages::electron_utils::recovery_prompt::RecoveryPromptAnswer;
use main_core::packages::electron_utils::recovery_store::{RecoveryCopyInfo, RecoveryFileIdentity, RecoveryIdentity};
use shell_contract::{hosts::MessageBox, ipc::NativeSavedDocument, runtime::Thread};

use crate::views::NativeWindowLifecycle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateAction { OpenOriginal, Restore, Ask }

fn candidate_action(state: Option<main_core::packages::electron_utils::recovery_store::RecoveryCopyState>) -> CandidateAction {
    use main_core::packages::electron_utils::recovery_store::RecoveryCopyState;
    match state {
        None => CandidateAction::OpenOriginal,
        Some(RecoveryCopyState::RestoreChosen) => CandidateAction::Restore,
        Some(RecoveryCopyState::Undecided | RecoveryCopyState::Later) => CandidateAction::Ask,
    }
}

fn saved_admission(document: NativeSavedDocument) -> main_core::packages::electron_utils::atomic_write::owned::Admission {
    Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut value = None;
        let target = document.target();
        crate::ipc::native_owner::with_saved_document_input_effect(
            &document, &[], target.cancel(), target.pending(), &[], &mut |_| {
                let action = effect.take().ok_or("The saved recovery effect was already consumed")?;
                value = Some(action());
                Ok(())
            },
        ).map_err(|message| main_core::node_fs::NodeError { code: None, message })?;
        value.ok_or_else(|| main_core::node_fs::NodeError { code: None, message: "The saved recovery effect was not admitted".into() })
    })
}

pub(crate) fn capture_fifo_saved_candidate(
    document: NativeSavedDocument, user_data_dir: String,
    done: Box<dyn FnOnce(Result<Option<(main_core::packages::electron_utils::recovery_store::captured_pair::CapturedPair, RecoveryCopyInfo)>, String>) + Send>,
) {
    use main_core::packages::electron_utils::recovery_store::captured_pair::capture_saved_pair;
    let target = document.target();
    let kind = match target.surface() {
        shell_contract::Surface::Docs => "docs", shell_contract::Surface::Slides => "slides",
        shell_contract::Surface::Pdf => "pdf", shell_contract::Surface::Html => "html",
        shell_contract::Surface::Markdown => "markdown", shell_contract::Surface::Diagram => "diagram",
        shell_contract::Surface::Mindmap => "mindmap", shell_contract::Surface::Sheets => "sheets",
        shell_contract::Surface::Hwp => "hwp",
        _ => return done(Err("The saved document does not support recovery".into())),
    };
    let Some(path) = target.path().path().to_str() else { return done(Err("The saved document path is not Unicode".into())); };
    let identity = RecoveryIdentity::File(RecoveryFileIdentity { kind: kind.into(), file_path: path.to_owned() });
    let admit = saved_admission(document.clone());
    capture_saved_pair(user_data_dir, identity, admit, Box::new(move |result| {
        let result = result.and_then(|pair| {
            let Some(pair) = pair else { return Ok(None); };
            let original_mtime = document.target().input().source_commit().metadata().ok()
                .map(|metadata| main_core::shell::recent_files::StatInfo::of(&metadata).mtime_ms);
            let copy = pair.info(original_mtime).ok_or("The pinned recovery copy metadata is unavailable")?;
            Ok(Some((pair, copy)))
        });
        done(result);
    }));
}

struct SelectedRecovery {
    document: NativeSavedDocument,
    pair: main_core::packages::electron_utils::recovery_store::captured_pair::CapturedPair,
    copy: RecoveryCopyInfo,
}

enum RecoverySelection {
    Original,
    Restore(SelectedRecovery),
    Discard(SelectedRecovery),
}

fn select_saved_recovery(
    ui: Arc<dyn Thread>, parent: Arc<NativeWindowLifecycle>, document: NativeSavedDocument,
    user_data_dir: String, document_name: String, lang: String,
    done: Box<dyn FnOnce(Result<RecoverySelection, String>) + Send>,
) {
    let retained = document.clone();
    capture_fifo_saved_candidate(document, user_data_dir, Box::new(move |result| {
        let candidate = match result { Ok(candidate) => candidate, Err(error) => return done(Err(error)) };
        let action = candidate_action(candidate.as_ref().map(|(_, copy)| copy.state));
        let Some((pair, copy)) = candidate else {
            let result = main_core::packages::electron_utils::atomic_write::owned::admitted(&saved_admission(retained), || ())
                .map(|()| RecoverySelection::Original).map_err(|error| error.message);
            return done(result);
        };
        let selected = SelectedRecovery { document: retained.clone(), pair, copy };
        match action {
            CandidateAction::OpenOriginal => done(Err("The recovery candidate changed unexpectedly".into())),
            CandidateAction::Restore => {
                let result = main_core::packages::electron_utils::atomic_write::owned::admitted(&saved_admission(retained), || ())
                    .map(|()| RecoverySelection::Restore(selected)).map_err(|error| error.message);
                done(result);
            }
            CandidateAction::Ask => {
                let copy = selected.copy.clone();
                prompt_saved_recovery(ui, parent, retained, &copy, &document_name, &lang, Box::new(move |answer| {
                    done(answer.map(|answer| match answer {
                        RecoveryPromptAnswer::Restore => RecoverySelection::Restore(selected),
                        RecoveryPromptAnswer::Discard => RecoverySelection::Discard(selected),
                    }));
                }));
            }
        }
    }));
}

pub(crate) struct SavedRecoveryRestore {
    document: NativeSavedDocument,
    pair: main_core::packages::electron_utils::recovery_store::captured_pair::CapturedPair,
    copy: RecoveryCopyInfo,
}
impl SavedRecoveryRestore {
    pub(crate) fn bytes(&self) -> &[u8] { self.pair.bytes() }
    pub(crate) fn copy_info(&self) -> &RecoveryCopyInfo { &self.copy }
    pub(crate) fn document(&self) -> &NativeSavedDocument { &self.document }
}

pub(crate) enum SavedRecoveryResult {
    Original,
    Restore(SavedRecoveryRestore),
}

pub(crate) enum SavedRecoveryFailure {
    Error(String),
    Superseded,
    Discard(main_core::packages::electron_utils::recovery_store::pair_discard::PairDiscardFailure),
}

pub(crate) fn resolve_saved_recovery(
    ui: Arc<dyn Thread>, parent: Arc<NativeWindowLifecycle>, document: NativeSavedDocument,
    user_data_dir: String, document_name: String, lang: String,
    done: Box<dyn FnOnce(Result<SavedRecoveryResult, SavedRecoveryFailure>) + Send>,
) {
    select_saved_recovery(ui, parent, document, user_data_dir, document_name, lang, Box::new(move |result| {
        match result {
            Err(error) => done(Err(SavedRecoveryFailure::Error(error))),
            Ok(RecoverySelection::Original) => done(Ok(SavedRecoveryResult::Original)),
            Ok(RecoverySelection::Restore(selected)) => done(Ok(SavedRecoveryResult::Restore(SavedRecoveryRestore {
                document: selected.document, pair: selected.pair, copy: selected.copy,
            }))),
            Ok(RecoverySelection::Discard(selected)) => {
                let admission = saved_admission(selected.document);
                let final_admission = admission.clone();
                selected.pair.discard(admission, Box::new(move |result| {
                    use main_core::packages::electron_utils::recovery_store::conditional_pair::PairCleanupOutcome;
                    match result {
                        Ok(PairCleanupOutcome::Deleted) => {
                            let result = main_core::packages::electron_utils::atomic_write::owned::admitted(&final_admission, || ())
                                .map(|()| SavedRecoveryResult::Original)
                                .map_err(|error| SavedRecoveryFailure::Error(error.message));
                            done(result);
                        }
                        Ok(PairCleanupOutcome::Superseded) => done(Err(SavedRecoveryFailure::Superseded)),
                        Err(failure) => done(Err(SavedRecoveryFailure::Discard(failure))),
                    }
                }));
            }
        }
    }));
}


pub(crate) fn prompt_saved_recovery(
    ui: Arc<dyn Thread>, parent: Arc<NativeWindowLifecycle>, document: NativeSavedDocument,
    copy: &RecoveryCopyInfo, document_name: &str, lang: &str,
    done: Box<dyn FnOnce(Result<RecoveryPromptAnswer, String>) + Send>,
) {
    let labels = recovery_prompt_labels(lang, document_name);
    let detail = if copy.file_changed_after_copy {
        format!("{}\n\n{}", labels.detail, labels.file_changed)
    } else {
        labels.detail
    };
    let options = MessageBox {
        kind: Some("question".into()),
        title: None,
        message: labels.message,
        detail: Some(detail),
        buttons: vec![labels.restore, labels.discard],
        default_id: Some(0),
        cancel_id: Some(0),
        checkbox_label: None,
    };
    super::dialog::owned::message_saved(ui, parent, document, options, Box::new(move |answer| {
        done(answer.map(|answer| if answer.response == 1 {
            RecoveryPromptAnswer::Discard
        } else {
            RecoveryPromptAnswer::Restore
        }));
    }));
}

#[cfg(test)]
mod tests {
    use super::{candidate_action, CandidateAction};
    use main_core::packages::electron_utils::recovery_store::RecoveryCopyState;

    #[test]
    fn saved_recovery_preserves_existing_choice_policy() {
        assert_eq!(candidate_action(None), CandidateAction::OpenOriginal);
        assert_eq!(candidate_action(Some(RecoveryCopyState::RestoreChosen)), CandidateAction::Restore);
        assert_eq!(candidate_action(Some(RecoveryCopyState::Undecided)), CandidateAction::Ask);
        assert_eq!(candidate_action(Some(RecoveryCopyState::Later)), CandidateAction::Ask);
    }
}
