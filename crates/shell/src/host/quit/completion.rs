pub(super) struct Completion<F: FnOnce(Result<(), String>)> {
    done: Option<F>,
}

impl<F: FnOnce(Result<(), String>)> Completion<F> {
    pub(super) fn new(done: F) -> Self {
        Self { done: Some(done) }
    }

    pub(super) fn finish(mut self, result: Result<(), String>) {
        let done = self.done.take().unwrap();
        done(result);
    }
}

impl<F: FnOnce(Result<(), String>)> Drop for Completion<F> {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done(Err("session flush completion was discarded".into()));
        }
    }
}

pub(super) fn post_with_completion<F>(
    post: impl FnOnce(Box<dyn FnOnce() + Send>),
    start: impl FnOnce(Completion<F>) + Send + 'static,
    done: F,
) where
    F: FnOnce(Result<(), String>) + Send + 'static,
{
    let completion = Completion::new(done);
    post(Box::new(move || start(completion)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

    #[test]
    fn rejected_queue_reports_failure_instead_of_abandoning_waiter() {
        let (send, receive) = mpsc::channel();
        post_with_completion(
            drop,
            |done| done.finish(Ok(())),
            move |result| send.send(result).unwrap(),
        );
        assert!(matches!(receive.try_recv(), Ok(Err(_))));
    }

    #[test]
    fn accepted_job_with_discarded_completion_reports_failure() {
        let (send, receive) = mpsc::channel();
        post_with_completion(
            |job| job(),
            drop,
            move |result| send.send(result).unwrap(),
        );
        assert!(matches!(receive.try_recv(), Ok(Err(_))));
    }

    #[test]
    fn retained_completion_keeps_waiter_pending_until_the_original_result() {
        let (send, receive) = mpsc::channel();
        let pending = Arc::new(Mutex::new(None::<Done>));
        let captured = pending.clone();
        post_with_completion(
            |job| job(),
            move |done| {
                *captured.lock().unwrap() = Some(Box::new(move |result| done.finish(result)));
            },
            move |result| send.send(result).unwrap(),
        );
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let done = pending.lock().unwrap().take().unwrap();
        done(Ok(()));
        assert_eq!(receive.try_recv(), Ok(Ok(())));
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    }

    #[test]
    fn original_failure_reaches_the_waiter_without_becoming_success() {
        let (send, receive) = mpsc::channel();
        post_with_completion(
            |job| job(),
            |done| done.finish(Err("session persistence failed".into())),
            move |result| send.send(result).unwrap(),
        );
        assert_eq!(receive.try_recv(), Ok(Err("session persistence failed".into())));
    }
}
