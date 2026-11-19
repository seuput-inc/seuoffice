//! One FIFO owns every recent-list read/modify/write. A moved path is read raw
//! because the old file no longer exists when the shell publishes its new name.
use super::state::App;
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::{self, Admission, EffectValue};
use main_core::shell::app_settings::queue;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;
use shell_contract::slides::OwnedRecentMove;
use std::sync::{Arc, mpsc};

enum Mutation {
    Push(String),
    Move(String, String),
}

fn apply(path: &str, mutation: Mutation, admission: Option<&Admission>) -> Result<(), String> {
    let read = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let value = match mutation {
        Mutation::Push(path) => {
            let previous = read
                .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
                .unwrap_or_default();
            let mut next = vec![path.clone()];
            next.extend(previous.into_iter().filter(|candidate| {
                candidate != &path && std::path::Path::new(candidate).exists()
            }));
            next.truncate(10);
            Some(serde_json::to_vec(&next).map_err(|error| error.to_string())?)
        }
        Mutation::Move(previous, next) => match read {
            Some(serde_json::Value::Array(mut values)) => {
                for value in &mut values {
                    if value.as_str() == Some(&previous) {
                        *value = serde_json::Value::String(next.clone());
                    }
                }
                Some(serde_json::to_vec(&values).map_err(|error| error.to_string())?)
            }
            _ => None,
        },
    };
    if let Some(bytes) = value {
        if let Some(admission) = admission {
            let _ = owned::write_file(path, &bytes, admission.clone());
        } else {
            let _ = std::fs::write(path, bytes);
        }
    }
    if let Some(admission) = admission {
        owned::admitted(admission, || ()).map_err(|error| error.message)?;
    }
    Ok(())
}

pub(super) fn push_unowned(path: String, document: String) {
    queue::for_path(&path).submit(move |completion| {
        let _ = apply(&path, Mutation::Push(document), None);
        drop(completion);
    });
}

pub(super) fn push_owned(
    path: String,
    document: String,
    admission: Admission,
) -> Result<(), String> {
    let (send, receive) = mpsc::sync_channel(1);
    queue::for_path(&path).submit(move |completion| {
        let result = apply(&path, Mutation::Push(document), Some(&admission));
        drop(completion);
        let _ = send.send(result);
    });
    receive
        .recv()
        .map_err(|_| "The recent-list worker was released".to_owned())?
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_recent_move(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        effects: &OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String> {
        let mut targets = participants.to_vec();
        targets.push(destination.clone());
        effects.with_owned_effect_many_extra(
            origin,
            &targets,
            cancel,
            pending,
            &additional.iter().collect::<Vec<_>>(),
            &mut || Ok(()),
        )?;
        let path = super::super::host::path_string(
            self.services
                .paths
                .path(shell_contract::runtime::PathName::UserData)
                .join("slides-recent.json"),
        );
        let core = self.services.core.clone();
        let origin = origin.clone();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let additional = additional.to_vec();
        let effects = effects.clone();
        let admission: Admission = Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let origin = origin.clone();
            let targets = targets.clone();
            let cancel = cancel.clone();
            let pending = pending.clone();
            let additional = additional.clone();
            let effects = effects.clone();
            core.post(Box::new(move || {
                let mut effect = Some(effect);
                let mut value: Option<EffectValue> = None;
                let result = effects
                    .with_owned_effect_many_extra(
                        &origin,
                        &targets,
                        &cancel,
                        &pending,
                        &additional.iter().collect::<Vec<_>>(),
                        &mut || {
                            let effect = effect.take().ok_or_else(|| {
                                "The recent-list effect was already consumed".to_owned()
                            })?;
                            value = Some(effect());
                            Ok(())
                        },
                    )
                    .and_then(|()| {
                        value.ok_or_else(|| "The recent-list effect was not admitted".to_owned())
                    })
                    .map_err(|message| NodeError {
                        code: None,
                        message,
                    });
                let _ = send.send(result);
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The recent-list core dispatcher was released".into(),
            })?
        });
        Ok(Box::new(move |old, new, done| {
            queue::for_path(&path).submit(move |completion| {
                let result = apply(&path, Mutation::Move(old, new), Some(&admission));
                drop(completion);
                done(result);
            })
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_recent_move_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String> {
        let mut targets = participants.to_vec();
        targets.push(destination.clone());
        let snapshot = snapshot.clone();
        effects.with_recovery_open_snapshot(
            receipt,
            &snapshot,
            receiver,
            &targets,
            cancel,
            pending,
            &mut || Ok(()),
        )?;
        let path = super::super::host::path_string(
            self.services
                .paths
                .path(shell_contract::runtime::PathName::UserData)
                .join("slides-recent.json"),
        );
        let core = self.services.core.clone();
        let (receipt, receiver, cancel, pending, effects) = (
            receipt.clone(),
            receiver.clone(),
            cancel.clone(),
            pending.clone(),
            effects.clone(),
        );
        let admission: Admission = Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let (receipt, receiver, targets, cancel, pending, effects, snapshot) = (
                receipt.clone(),
                receiver.clone(),
                targets.clone(),
                cancel.clone(),
                pending.clone(),
                effects.clone(),
                snapshot.clone(),
            );
            core.post(Box::new(move || {
                let mut effect = Some(effect);
                let mut value: Option<EffectValue> = None;
                let result = effects
                    .with_recovery_open_snapshot(
                        &receipt,
                        &snapshot,
                        &receiver,
                        &targets,
                        &cancel,
                        &pending,
                        &mut || {
                            let effect = effect.take().ok_or_else(|| {
                                "The recent-list effect was already consumed".to_owned()
                            })?;
                            value = Some(effect());
                            Ok(())
                        },
                    )
                    .and_then(|()| {
                        value.ok_or_else(|| "The recent-list effect was not admitted".to_owned())
                    })
                    .map_err(|message| NodeError {
                        code: None,
                        message,
                    });
                let _ = send.send(result);
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The recent-list core dispatcher was released".into(),
            })?
        });
        Ok(Box::new(move |old, new, done| {
            queue::for_path(&path).submit(move |completion| {
                let result = apply(&path, Mutation::Move(old, new), Some(&admission));
                drop(completion);
                done(result);
            })
        }))
    }
    pub(super) fn prepare_recent_move_existing(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        effects: &OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String> {
        effects.with_existing_document_save_effect_extra(
            binding,
            participants,
            cancel,
            pending,
            &additional.iter().collect::<Vec<_>>(),
            &mut || Ok(()),
        )?;
        let path = super::super::host::path_string(
            self.services
                .paths
                .path(shell_contract::runtime::PathName::UserData)
                .join("slides-recent.json"),
        );
        let core = self.services.core.clone();
        let (binding, participants, cancel, pending, additional, effects) = (
            binding.clone(),
            participants.to_vec(),
            cancel.clone(),
            pending.clone(),
            additional.to_vec(),
            effects.clone(),
        );
        let admission: Admission = Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let (binding, participants, cancel, pending, additional, effects) = (
                binding.clone(),
                participants.clone(),
                cancel.clone(),
                pending.clone(),
                additional.clone(),
                effects.clone(),
            );
            core.post(Box::new(move || {
                let mut effect = Some(effect);
                let mut value: Option<EffectValue> = None;
                let result = effects
                    .with_existing_document_save_effect_extra(
                        &binding,
                        &participants,
                        &cancel,
                        &pending,
                        &additional.iter().collect::<Vec<_>>(),
                        &mut || {
                            let effect = effect.take().ok_or_else(|| {
                                "The recent-list effect was already consumed".to_owned()
                            })?;
                            value = Some(effect());
                            Ok(())
                        },
                    )
                    .and_then(|()| {
                        value.ok_or_else(|| "The recent-list effect was not admitted".to_owned())
                    })
                    .map_err(|message| NodeError {
                        code: None,
                        message,
                    });
                let _ = send.send(result);
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The recent-list core dispatcher was released".into(),
            })?
        });
        Ok(Box::new(move |old, new, done| {
            queue::for_path(&path).submit(move |completion| {
                let result = apply(&path, Mutation::Move(old, new), Some(&admission));
                drop(completion);
                done(result);
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_a_missing_old_path_keeps_its_position_and_raw_entries() {
        let dir =
            std::env::temp_dir().join(format!("slides-recent-move-{}", jscompat::random_uuid()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("slides-recent.json");
        std::fs::write(&path, br#"["before","old","after",null]"#).unwrap();
        apply(
            path.to_str().unwrap(),
            Mutation::Move("old".into(), "new".into()),
            None,
        )
        .unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written, serde_json::json!(["before", "new", "after", null]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refused_owned_recent_write_keeps_the_previous_list_and_reports_refusal() {
        let dir =
            std::env::temp_dir().join(format!("slides-recent-refused-{}", jscompat::random_uuid()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("slides-recent.json");
        std::fs::write(&path, br#"["old"]"#).unwrap();
        let admission: Admission = Arc::new(|_| {
            Err(NodeError {
                code: None,
                message: "released owner".into(),
            })
        });
        assert_eq!(
            apply(
                path.to_str().unwrap(),
                Mutation::Move("old".into(), "new".into()),
                Some(&admission)
            ),
            Err("released owner".into())
        );
        assert_eq!(std::fs::read(&path).unwrap(), br#"["old"]"#);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
