use super::*;
use main_core::shell::app_settings::queue::Completion;
use main_core::shell::projects::project_guide::{
    MutationAcknowledgement, MutationPreparation, MutationRequest, MutationWrite,
};

impl Projects {
    pub(super) fn mutation(&self, request: MutationRequest, reply: Responder) {
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
                let live = PENDING_SETTINGS.with(|pending| {
                    pending.borrow().get(&token).is_some_and(|(_, reply)| {
                        reply.owner.as_ref().is_some_and(|owner| {
                            projects
                                .shell
                                .host
                                .with_request_owner(&owner.native, &owner.cancel, &mut || {
                                    projects.guide().begin_retry_mutation(&request);
                                    Ok(())
                                })
                                .is_ok()
                        })
                    })
                });
                if !live {
                    return projects.finish_mutation_reply(token, completion, |_, _| {
                        Err(Thrown("mutation owner is no longer available".into()))
                    });
                }
                let project_id = request.project_id().to_owned();
                match projects.guide().prepare_mutation(request) {
                    Err(error) => projects.finish_mutation_reply(token, completion, |_, _| {
                        Err(Thrown(error.message()))
                    }),
                    Ok(MutationPreparation::Done(mut result)) => {
                        projects.finish_mutation_reply(token, completion, |projects, warnings| {
                            warnings.append(&mut result.warnings);
                            projects
                                .serve(|ipc, deps| ipc.finish_mutation(deps, &project_id, result))
                        })
                    }
                    Ok(MutationPreparation::Write(write)) => {
                        projects.mutation_worker(token, project_id, write, completion)
                    }
                }
            }));
        });
    }

    fn mutation_worker(
        &self,
        token: u64,
        project_id: String,
        write: MutationWrite,
        completion: Completion,
    ) {
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
                .execute_authorized(
                    &path,
                    |effect| guarded_project_publish(&host, &owner, effect),
                    |from, to| guarded_settings_commit(&host, &owner, from, to),
                )
                .map_err(|error| error.message());
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
                match result {
                    Ok(MutationAcknowledgement::NeedBackupName(write)) => {
                        match projects.guide().resume_mutation_backup(write) {
                            Ok(write) => {
                                projects.mutation_worker(token, project_id, write, completion)
                            }
                            Err(error) => {
                                projects.finish_mutation_reply(token, completion, |_, _| {
                                    Err(Thrown(error.message()))
                                })
                            }
                        }
                    }
                    result => {
                        let ack = result.and_then(|ack| {
                            projects
                                .guide()
                                .validate_mutation_ack(ack)
                                .map_err(|error| error.message())
                        });
                        projects.finish_mutation_reply(token, completion, |projects, warnings| {
                            let ack = ack.map_err(Thrown)?;
                            let mut result = projects
                                .guide()
                                .finish_mutation(ack)
                                .map_err(|error| Thrown(error.message()))?;
                            warnings.append(&mut result.warnings);
                            projects
                                .serve(|ipc, deps| ipc.finish_mutation(deps, &project_id, result))
                        });
                    }
                }
            }));
        }));
    }

    fn finish_mutation_reply(
        &self,
        token: u64,
        completion: Completion,
        work: impl FnOnce(
            &Projects,
            &mut Vec<(String, String)>,
        ) -> Result<IpcStep<GuidePending>, Thrown>,
    ) {
        let pending = PENDING_SETTINGS.with(|pending| pending.borrow_mut().remove(&token));
        let Some((_, reply)) = pending else {
            drop(completion);
            return;
        };
        let mut work = Some(work);
        let mut answer = None;
        let mut warnings = Vec::new();
        let guarded = match reply.owner.as_ref() {
            Some(owner) => {
                self.shell
                    .host
                    .with_request_owner(&owner.native, &owner.cancel, &mut || {
                        let work = work.take().ok_or_else(|| {
                            error_of(Thrown("mutation acknowledgement repeated".into()))
                        })?;
                        answer = Some(work(self, &mut warnings));
                        Ok(())
                    })
            }
            None => Err(error_of(Thrown(
                "authenticated project owner is unavailable".into(),
            ))),
        };
        let answer = match guarded {
            Ok(()) => {
                answer.unwrap_or_else(|| Err(Thrown("mutation acknowledgement did not run".into())))
            }
            Err(error) => Err(Thrown(error.message)),
        };
        drop(completion);
        self.guide().report_write_warnings(&warnings);
        self.advance(answer, reply);
    }
}
