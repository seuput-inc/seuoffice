use main_core::packages::electron_utils::recovery_store::{RecoveryIdentity, RecoveryLaunchDecision};
use main_core::shell::launch_recovery::LaunchPlan;
use main_core::shell::recovery_shell::{LaunchRecoveryStore, prepare_launch_staged};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::session_record::{SessionRecordProcess, SessionRecorderEnv};
use shell_contract::runtime::Services;

use super::{SessionEnv, SharedStore};

type Prepared = (SessionRecordProcess, Result<LaunchPlan, Failure>);

struct PrepareJob {
    session: Option<SessionRecordProcess>,
    done: Option<Box<dyn FnOnce(Prepared) + Send>>,
}

impl Drop for PrepareJob {
    fn drop(&mut self) {
        if let (Some(session), Some(done)) = (self.session.take(), self.done.take()) {
            done((session, Err(Failure("the recovery preparation worker was dropped".into()))));
        }
    }
}

pub(super) fn prepare(
    services: &Services, store: SharedStore, session: SessionRecordProcess,
    user_data: String, paths: Vec<String>, reopened: bool, shutdown:shell_contract::runtime::CancelToken,
    admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
    writer:super::session_writer::SessionWriter,
    done: impl FnOnce(Prepared) + 'static,
) {
    let done = shell_ipc_w::home_files::back_to_core(services.core.clone(), done);
    let mut job = PrepareJob { session: Some(session), done: Some(done) };
    services.workers.spawn(Box::new(move || {
        if shutdown.is_cancelled() {
            let session=job.session.take().expect("recovery session belongs to worker");
            if let Some(done)=job.done.take() { done((session,Err(Failure("recovery launch was cancelled".into())))); }
            return;
        }
        let session = job.session.as_mut().expect("recovery session belongs to worker");
        let result = prepare_launch_staged(&store, session, || SessionEnv.now(), user_data, &paths, reopened, || {
            let dir = std::path::Path::new(&store.user_data).join("sheets-autosave");
            let result = store.with_store(|recovery| main_core::sheets::recovery_policy::migrate_legacy_unsaved_new(
                recovery, &dir.to_string_lossy(), &|label, detail| eprintln!("{label} {detail}"),
            ));
            if let Err(error) = result { eprintln!("[sheets] legacy recovery migration failed: {}", error.message); }
        }, || if shutdown.is_cancelled() { Err(Failure("recovery launch was cancelled".into())) } else { Ok(()) });
        let result = result.and_then(|(plan, write)| {
            if let Some(write) = write {
                writer.bind_recorder(&write).map_err(Failure)?;
                let result = write.write_owned(admission).map_err(|error| error.message);
                if let Some(recorder) = session.session_recorder() {
                    recorder.complete_write(&write, result.clone(), &mut SessionEnv);
                }
                result.map_err(Failure)?;
            }
            Ok(plan)
        });
        let result=if shutdown.is_cancelled() { Err(Failure("recovery launch was cancelled".into())) } else { result };
        let session = job.session.take().expect("recovery session belongs to worker");
        if let Some(done) = job.done.take() { done((session, result)); }
    }));
}

struct DecisionsJob(Option<Box<dyn FnOnce(Vec<Failure>) + Send>>);
impl Drop for DecisionsJob {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() { done(vec![Failure("the recovery decision worker was dropped".into())]); }
    }
}

pub(super) fn decisions(
    services: &Services, store: SharedStore, decisions: Vec<(RecoveryIdentity, RecoveryLaunchDecision)>,
    shutdown:shell_contract::runtime::CancelToken, done: impl FnOnce(Vec<Failure>) + 'static,
) {
    let done = shell_ipc_w::home_files::back_to_core(services.core.clone(), done);
    let mut job = DecisionsJob(Some(done));
    services.workers.spawn(Box::new(move || {
        let mut errors=Vec::new();
        for (identity,decision) in decisions {
            if shutdown.is_cancelled() { errors.push(Failure("recovery launch was cancelled".into())); break; }
            if let Err(error)=store.apply_decision(&identity,decision) { errors.push(error); }
        }
        if let Some(done) = job.0.take() { done(errors); }
    }));
}
