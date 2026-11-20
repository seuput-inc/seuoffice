//! Process-owned retry for one Docs generated-output actor completion.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::host::headless_cleanup::{CleanupTask, Done, Registration, Supervisor};
use shell_ipc_d2::docs::hosts::GeneratedActorWake;

struct State {
    finished: bool,
    next: u64,
    waiters: HashMap<u64, Done>,
}
struct Task {
    wake: GeneratedActorWake,
    state: Mutex<State>,
}
struct RetryTask(Arc<Task>);

impl CleanupTask for RetryTask {
    fn retry(&self, done: Done) {
        let id = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.finished {
                drop(state);
                return done(Ok(()));
            }
            let id = state.next;
            let Some(next) = id.checked_add(1) else {
                drop(state);
                return done(Err("Docs cleanup retry identities are exhausted".into()));
            };
            state.next = next;
            state.waiters.insert(id, done);
            id
        };
        let task = self.0.clone();
        (self.0.wake)(Box::new(move |posted| {
            if let Err(error) = posted {
                let done = task
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .waiters
                    .remove(&id);
                if let Some(done) = done {
                    done(Err(error));
                }
            }
        }));
    }
}

struct ActorRegistration {
    task: Arc<Task>,
    registration: Registration,
}
impl shell_contract::hosts::GeneratedCleanupRegistration for ActorRegistration {
    fn complete(self: Box<Self>) {
        let Self { task, registration } = *self;
        let waiters = {
            let mut state = task.state.lock().unwrap_or_else(|error| error.into_inner());
            state.finished = true;
            std::mem::take(&mut state.waiters)
        };
        registration.complete();
        for (_, done) in waiters {
            done(Ok(()));
        }
    }
}

pub(in crate::host::d2) fn register(
    supervisor: &Arc<Supervisor>,
    wake: GeneratedActorWake,
) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
    let task = Arc::new(Task {
        wake,
        state: Mutex::new(State {
            finished: false,
            next: 0,
            waiters: HashMap::new(),
        }),
    });
    let registration = supervisor.register(Arc::new(RetryTask(task.clone())))?;
    Ok(Box::new(ActorRegistration { task, registration }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_waits_for_actor_ack_and_retries_discarded_post() {
        let supervisor = Supervisor::new();
        let posted: Arc<Mutex<Vec<Done>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = posted.clone();
        let registration = register(
            &supervisor,
            Arc::new(move |done| captured.lock().unwrap().push(done)),
        )
        .unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        let first = results.clone();
        supervisor.quiesce_and_retry(Box::new(move |result| first.lock().unwrap().push(result)));
        posted.lock().unwrap().pop().unwrap()(Err("core post discarded".into()));
        assert!(results.lock().unwrap()[0].is_err());
        let second = results.clone();
        supervisor.quiesce_and_retry(Box::new(move |result| second.lock().unwrap().push(result)));
        posted.lock().unwrap().pop().unwrap()(Ok(()));
        assert_eq!(results.lock().unwrap().len(), 1);
        registration.complete();
        assert_eq!(results.lock().unwrap().len(), 2);
        assert!(results.lock().unwrap()[1].is_ok());
    }
}
