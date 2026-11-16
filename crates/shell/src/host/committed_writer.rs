//! Preparation of real atomic-write stages. Publication remains an admitted effect.

#[cfg(unix)]
pub(crate) use unix::{finish_native, open_native_rescue, prepare, prepare_fallback, prepare_rescue, publish_native, FallbackStage, NativeFinishError, NativeRenameStep, NativeRescueError, NativeRescuePlan, PreparedStage, RenameRetries, RetainedStage, SaveOperation, SaveSupervisor, StageError};

#[cfg(unix)]
mod unix {
    use std::{ffi::CString, io, path::{Path, PathBuf}, sync::{Arc, Condvar, Mutex}};
    use file_commit::{OwnedStage, OwnedWrittenCommit, PendingStage, PrivateDirectory, PublicationMode};

    pub(crate) enum RetainedStage {
        Directory(PrivateDirectory, Vec<u8>),
        Pending(PendingStage, Vec<u8>),
        Quarantined(OwnedStage, Vec<u8>),
    }

    impl RetainedStage {
        /// Remove only the held private stage, retaining an unsuccessful
        /// inverse or directory removal for retry.
        pub(crate) fn cleanup(self) -> Result<Vec<u8>, (io::Error, Self)> {
            let mut current = self;
            loop {
                current = match current {
                    Self::Directory(directory, bytes) => match directory.remove() {
                        Ok(()) => return Ok(bytes),
                        Err(error) => return Err((error, Self::Directory(directory, bytes))),
                    },
                    Self::Quarantined(stage, bytes) => {
                        let directory = stage.directory();
                        match stage.remove() {
                            Ok(()) => Self::Directory(directory, bytes),
                            Err((error, stage)) => return Err((error, Self::Quarantined(stage, bytes))),
                        }
                    }
                    Self::Pending(pending, bytes) => {
                        let directory = pending.directory();
                        match pending.quarantine() {
                            Ok(stage) => Self::Quarantined(stage, bytes),
                            Err((error, pending)) if pending.is_quarantined() && error.kind() == io::ErrorKind::InvalidData => {
                                match pending.restore() {
                                    Ok(()) => Self::Directory(directory, bytes),
                                    Err((error, pending)) => return Err((error, Self::Pending(pending, bytes))),
                                }
                            }
                            Err((error, pending)) => return Err((error, Self::Pending(pending, bytes))),
                        }
                    }
                };
            }
        }
    }

    #[derive(Default)]
    struct SupervisorState {
        closing: bool,
        draining: bool,
        active: usize,
        retained: Vec<RetainedStage>,
        indeterminate: bool,
    }
    #[derive(Default)]
    struct SupervisorShared { state: Mutex<SupervisorState>, changed: Condvar }

    /// One application-owned cleanup ledger. A worker retains its operation
    /// ticket through every core callback and records failed cleanup before
    /// dropping the ticket.
    #[derive(Clone, Default)]
    pub(crate) struct SaveSupervisor(Arc<SupervisorShared>);
    pub(crate) struct SaveOperation { supervisor: SaveSupervisor, awaiting: bool }

    impl SaveSupervisor {
        pub(crate) fn begin(&self) -> Result<SaveOperation, String> {
            let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closing { return Err("The save service is shutting down.".into()); }
            state.active = state.active.checked_add(1).ok_or("Too many active saves.")?;
            Ok(SaveOperation { supervisor: self.clone(), awaiting: false })
        }


        /// Run only on a worker before the core and native workers stop.
        /// Failed cleanup remains owned for the next shutdown attempt.
        pub(crate) fn close_and_drain(&self) -> io::Result<()> {
            let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closing = true;
            while state.draining || state.active != 0 {
                state = self.0.changed.wait(state).unwrap_or_else(|error| error.into_inner());
            }
            state.draining = true;
            loop {
                let Some(stage) = state.retained.pop() else {
                    state.draining = false;
                    self.0.changed.notify_all();
                    return if state.indeterminate {
                        Err(io::Error::other("A save core action ended without an acknowledged result."))
                    } else { Ok(()) };
                };
                drop(state);
                let result = stage.cleanup();
                state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
                if let Err((error, stage)) = result {
                    state.retained.push(stage);
                    state.draining = false;
                    self.0.changed.notify_all();
                    return Err(error);
                }
            }
        }
    }

    impl SaveOperation {
        pub(crate) fn retain(&self, stage: RetainedStage) {
            let mut state = self.supervisor.0.state.lock().unwrap_or_else(|error| error.into_inner());
            state.retained.push(stage);
            self.supervisor.0.changed.notify_all();
        }

        fn mark_indeterminate(&self) {
            let mut state = self.supervisor.0.state.lock().unwrap_or_else(|error| error.into_inner());
            state.indeterminate = true;
        }
    }

    enum ActorDispatchError<T> { NotRun(T), Indeterminate }

    impl SaveOperation {
        fn on_actor<T: Send + 'static, R: Send + 'static>(
            &mut self, dispatch: &shell_contract::hosts::SaveCoreDispatch, input: T,
            action: impl FnOnce(T, &mut dyn FnMut() -> Result<(), String>) -> R + Send + 'static,
        ) -> Result<R, ActorDispatchError<T>> {
            let input = Arc::new(Mutex::new(Some(input)));
            let queued = input.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            self.awaiting = true;
            dispatch(Box::new(move |validate| {
                let input = queued.lock().unwrap_or_else(|error| error.into_inner()).take();
                if let Some(input) = input { let _ = tx.send(action(input, validate)); }
            }));
            match rx.recv() {
                Ok(result) => { self.awaiting = false; Ok(result) }
                Err(_) => {
                    let remaining = input.lock().unwrap_or_else(|error| error.into_inner()).take();
                    match remaining {
                        Some(input) => { self.awaiting = false; Err(ActorDispatchError::NotRun(input)) }
                        None => { self.mark_indeterminate(); Err(ActorDispatchError::Indeterminate) }
                    }
                }
            }
        }
    }

    impl Drop for SaveOperation {
        fn drop(&mut self) {
            let mut state = self.supervisor.0.state.lock().unwrap_or_else(|error| error.into_inner());
            if self.awaiting { state.indeterminate = true; }
            state.active -= 1;
            self.supervisor.0.changed.notify_all();
        }
    }

    pub(crate) struct StageError {
        pub error: io::Error,
        pub retained: Option<RetainedStage>,
    }

    pub(crate) struct PreparedStage {
        directory: PrivateDirectory,
        commit: OwnedWrittenCommit,
    }

    /// Runs entirely on a worker. Every failed stage retains the actual object
    /// and its bytes for the caller's cleanup or rescue, never a recreated path.
    pub(crate) fn prepare(path: &Path, bytes: Vec<u8>) -> Result<PreparedStage, StageError> {
        let fail = |error| StageError { error, retained: None };
        if !path.is_absolute() {
            return Err(fail(io::Error::new(io::ErrorKind::InvalidInput, "output path is not absolute")));
        }
        let parent_path = path.parent().ok_or_else(|| fail(io::Error::new(io::ErrorKind::InvalidInput, "output has no parent")))?;
        let name = path.file_name().and_then(|value| value.to_str())
            .ok_or_else(|| fail(io::Error::new(io::ErrorKind::InvalidInput, "output has no UTF-8 name")))?;
        let destination = CString::new(name).map_err(|error| fail(io::Error::new(io::ErrorKind::InvalidInput, error)))?;
        let parent = file_commit::open_output_parent(parent_path).map_err(&fail)?;
        let directory = PrivateDirectory::create(&parent).map_err(&fail)?;
        let pending = match directory.create_stage_for(name, 0o666) {
            Ok(pending) => pending,
            Err(error) => return Err(StageError { error, retained: Some(RetainedStage::Directory(directory, bytes)) }),
        };
        let pending = match pending.write_owned(bytes) {
            Ok(pending) => pending,
            Err((error, stage, bytes)) => return Err(StageError { error, retained: Some(RetainedStage::Pending(stage, bytes)) }),
        };
        let written = match pending.quarantine() {
            Ok(stage) => stage,
            Err((error, pending)) => {
                let (stage, bytes) = pending.into_parts();
                return Err(StageError { error, retained: Some(RetainedStage::Pending(stage, bytes)) });
            }
        };
        if let Err(error) = written.sync_all() {
            let tolerated = matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EBUSY | libc::EINVAL | libc::ENOSYS | libc::ENOTSUP));
            if !tolerated {
                let (stage, bytes) = written.into_parts();
                return Err(StageError { error, retained: Some(RetainedStage::Quarantined(stage, bytes)) });
            }
        }
        let commit = match written.prepare(&parent, &destination, path, PublicationMode::Replace) {
            Ok(commit) => commit,
            Err((error, written)) => {
                let (stage, bytes) = written.into_parts();
                return Err(StageError { error, retained: Some(RetainedStage::Quarantined(stage, bytes)) });
            }
        };
        Ok(PreparedStage { directory, commit })
    }

    /// Run on the actor's core turn after worker-side stage preparation.
    pub(crate) fn publish_native(
        prepared: PreparedStage, effects: &dyn shell_contract::hosts::OwnedEffects,
        selection: &shell_contract::ipc::NativeSaveDestination,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> (Result<(shell_contract::ipc::SavedPublication, file_commit::CommittedWriteSnapshot), shell_contract::ipc::StagedWriteError>, PrivateDirectory) {
        let PreparedStage { directory, commit } = prepared;
        let result = effects.publish_native_staged_write_validated(
            selection, participants, cancel, pending, additional, commit, validate,
        );
        (result, directory)
    }

    /// A retry returns the same stage, never a new path or a new authority.
    pub(crate) enum NativeRenameStep {
        Published { publication: shell_contract::ipc::SavedPublication, snapshot: file_commit::CommittedWriteSnapshot, directory: PrivateDirectory },
        Retry { prepared: PreparedStage, delay: std::time::Duration },
        Fallback { prepared: PreparedStage, error: io::Error },
        Denied { error: shell_contract::ipc::StagedWriteError, directory: PrivateDirectory },
    }

    #[derive(Default)]
    pub(crate) struct RenameRetries { next: usize }

    impl RenameRetries {
        /// Call on a worker after each core publication attempt. Sleep only on
        /// the worker before dispatching a retry back to the original actor.
        pub(crate) fn classify(
            &mut self,
            result: Result<(shell_contract::ipc::SavedPublication, file_commit::CommittedWriteSnapshot), shell_contract::ipc::StagedWriteError>,
            directory: PrivateDirectory,
        ) -> NativeRenameStep {
            match result {
                Ok((publication, snapshot)) => NativeRenameStep::Published { publication, snapshot, directory },
                Err(shell_contract::ipc::StagedWriteError::Io { error, stage }) => {
                    let prepared = PreparedStage { directory, commit: *stage };
                    const DELAYS: [u64; 4] = [50, 100, 200, 400];
                    let retryable = matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EBUSY));
                    if retryable && self.next < DELAYS.len() {
                        let delay = std::time::Duration::from_millis(DELAYS[self.next]);
                        self.next += 1;
                        NativeRenameStep::Retry { prepared, delay }
                    } else {
                        NativeRenameStep::Fallback { prepared, error }
                    }
                }
                Err(error) => NativeRenameStep::Denied { error, directory },
            }
        }
    }

    /// The private source remains available through direct-write failure and
    /// rescue. Its bytes travel separately with the actual direct descriptor.
    pub(crate) struct FallbackStage {
        directory: PrivateDirectory,
        stage: OwnedStage,
        path: PathBuf,
    }

    impl FallbackStage {
        pub(crate) fn into_retained(self, bytes: Vec<u8>) -> RetainedStage {
            RetainedStage::Quarantined(self.stage, bytes)
        }
    }

    pub(crate) fn prepare_fallback(prepared: PreparedStage)
        -> Result<(FallbackStage, Vec<u8>, file_commit::PreparedDirectWrite), (io::Error, FallbackStage, Vec<u8>)>
    {
        let PreparedStage { directory, commit } = prepared;
        let path = commit.path().to_owned();
        let (stage, bytes) = commit.into_parts();
        let fallback = FallbackStage { directory, stage, path };
        let plan = fallback.stage.original_parent()
            .and_then(|parent| file_commit::prepare_direct(&parent, &fallback.path));
        match plan {
            Ok(plan) => Ok((fallback, bytes, plan)),
            Err(error) => Err((error, fallback, bytes)),
        }
    }

    /// The rescue file name is derived only from the selected output name.
    /// The parent is the original held directory, never a pathname reopen.
    pub(crate) fn prepare_rescue(fallback: &mut FallbackStage) -> io::Result<NativeRescuePlan> {
        let name = fallback.path.file_name().and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no UTF-8 name"))?;
        let mut end = name.len().min(200);
        while !name.is_char_boundary(end) { end -= 1; }
        let rescue = CString::new(format!("{}.unsaved.tmp", &name[..end]))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let parent = fallback.stage.original_parent()?;
        let prepared = file_commit::PreparedRescueCopy::new(&parent, &rescue, &mut fallback.stage)?;
        Ok(NativeRescuePlan { path: fallback.path.clone(), prepared })
    }

    pub(crate) struct NativeRescuePlan {
        path: PathBuf,
        prepared: file_commit::PreparedRescueCopy,
    }

    pub(crate) enum NativeRescueError {
        NotAdmitted { error: String, prepared: NativeRescuePlan },
        Open(file_commit::RescueCopyError),
        OpenedButNotAdmitted { error: String, opened: UnadmittedRescue },
    }

    /// Holds the actual open outcome after authority loss without granting
    /// access to continue the rescue copy.
    pub(crate) struct UnadmittedRescue {
        _opened: Result<file_commit::RescueOpen, file_commit::RescueCopyError>,
    }

    /// Only the descriptor-relative open/truncation runs inside admission.
    /// Copying the stage bytes is performed later on a worker.
    pub(crate) fn open_native_rescue(
        prepared: NativeRescuePlan, effects: &dyn shell_contract::hosts::OwnedEffects,
        selection: &shell_contract::ipc::NativeSaveDestination,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<file_commit::RescueOpen, NativeRescueError> {
        if prepared.path != selection.path() {
            return Err(NativeRescueError::NotAdmitted {
                error: "The rescue does not belong to the selected output.".into(), prepared,
            });
        }
        let controls: Vec<_> = additional.iter().collect();
        let mut prepared = Some(prepared);
        let mut opened = None;
        let admission = effects.with_existing_document_destination_effect(
            selection, selection.path(), participants, cancel, pending, &controls, &mut || {
                validate()?;
                let plan = prepared.take().ok_or("The rescue open was already admitted.")?;
                opened = Some(plan.prepared.open());
                Ok(())
            });
        match (opened, admission) {
            (Some(Ok(opened)), Ok(())) => Ok(opened),
            (Some(Err(error)), Ok(())) => Err(NativeRescueError::Open(error)),
            (Some(opened), Err(error)) => Err(NativeRescueError::OpenedButNotAdmitted { error, opened: UnadmittedRescue { _opened: opened } }),
            (None, result) => Err(NativeRescueError::NotAdmitted {
                error: result.err().unwrap_or_else(|| "The rescue open was not admitted.".into()),
                prepared: prepared.expect("unconsumed rescue plan"),
            }),
        }
    }

    pub(crate) enum NativeFinishError {
        Publication { error: shell_contract::ipc::StagedWriteError, directory: PrivateDirectory },
        Input { error: String, publication: shell_contract::ipc::SavedPublication, snapshot: file_commit::CommittedWriteSnapshot, directory: PrivateDirectory },
        Cleanup { error: io::Error, publication: shell_contract::ipc::SavedPublication, input: shell_contract::ipc::NativeSavedInput, directory: PrivateDirectory },
    }

    /// Finish on a worker. Even a cleanup failure retains the actual committed
    /// result and the directory that still needs removal.
    pub(crate) fn finish_native(
        result: Result<(shell_contract::ipc::SavedPublication, file_commit::CommittedWriteSnapshot), shell_contract::ipc::StagedWriteError>,
        directory: PrivateDirectory,
    ) -> Result<(shell_contract::ipc::SavedPublication, shell_contract::ipc::NativeSavedInput), NativeFinishError> {
        let (publication, snapshot) = match result {
            Ok(value) => value,
            Err(error) => return Err(NativeFinishError::Publication { error, directory }),
        };
        let input = match shell_contract::ipc::NativeSavedInput::try_from_written_snapshot(publication.clone(), snapshot) {
            Ok(input) => input,
            Err((error, snapshot)) => return Err(NativeFinishError::Input { error, publication, snapshot, directory }),
        };
        if let Err(error) = directory.remove() {
            return Err(NativeFinishError::Cleanup { error, publication, input, directory });
        }
        Ok((publication, input))
    }
}
