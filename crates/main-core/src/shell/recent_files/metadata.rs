//! One existing per-path FIFO for complete metadata read-modify-write IO.
//! Completion releases the queue before invoking any callback or core continuation.
use std::sync::mpsc;
use crate::node_fs::{NodeError, node_error};

pub fn submit<T: Send + 'static>(
    path: &str,
    work: impl FnOnce() -> Result<T,NodeError> + Send + 'static,
    done: impl FnOnce(Result<T,NodeError>) + Send + 'static,
) {
    let error_path = path.to_owned();
    super::super::app_settings::queue::for_path(path).submit(move |completion| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err(node_error(&std::io::Error::other("metadata worker failed"),"write",&error_path)));
        drop(completion);
        done(result);
    });
}

/// Compatibility for existing synchronous metadata APIs. A queued IO job must
/// never require a core acknowledgement, UI action or callback before returning.
/// New request-owned callers use `submit` and complete asynchronously instead.
pub fn run_sync<T: Send + 'static>(
    path: &str,
    work: impl FnOnce() -> Result<T,NodeError> + Send + 'static,
) -> Result<T,NodeError> {
    let (send,receive) = mpsc::sync_channel(1);
    submit(path,work,move |result| { let _ = send.send(result); });
    receive.recv().map_err(|_| node_error(&std::io::Error::other("metadata IO completion was lost"),"write",path))?
}

/// Only for legacy caller-thread IO whose same-path jobs never wait on core/UI.
/// The dispatch job transfers its permit and returns, without a core callback.
pub fn run_borrowed<T>(path:&str,work:impl FnOnce()->Result<T,NodeError>)->Result<T,NodeError> {
    let (send,receive) = mpsc::sync_channel(1);
    super::super::app_settings::queue::for_path(path).submit(move |completion| {
        let _ = send.send(completion);
    });
    let completion = receive.recv().map_err(|_| node_error(&std::io::Error::other("metadata IO permit was lost"),"write",path))?;
    let result = work();
    drop(completion);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc,Mutex};
    #[test]
    fn synchronous_and_async_writers_share_one_actual_path_fifo() {
        let path = format!("metadata-fixture-{}",jscompat::random_uuid());
        let values = Arc::new(Mutex::new(Vec::new()));
        let (started,await_started) = mpsc::sync_channel(1);
        let (release,await_release) = mpsc::sync_channel(1);
        let first = values.clone();
        submit(&path,move || {
            started.send(()).unwrap();
            await_release.recv().unwrap();
            first.lock().unwrap().push(1);
            Ok(())
        },|result| result.unwrap());
        await_started.recv().unwrap();
        let second = values.clone();
        let second_path = path.clone();
        let writer = std::thread::spawn(move || run_sync(&second_path,move || {
            second.lock().unwrap().push(2);
            Ok(())
        }).unwrap());
        release.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(*values.lock().unwrap(),vec![1,2]);
    }
    #[test]
    fn callback_can_queue_same_path_without_holding_the_previous_completion() {
        let path = format!("metadata-fixture-{}",jscompat::random_uuid());
        let next_path = path.clone();
        let (done,receive) = mpsc::sync_channel(1);
        submit(&path,|| Ok(1),move |first| {
            let second = run_sync(&next_path,|| Ok(2)).unwrap();
            done.send((first.unwrap(),second)).unwrap();
        });
        assert_eq!(receive.recv().unwrap(),(1,2));
    }

    #[test]
    fn borrowed_clock_and_error_keep_the_caller_thread_and_release_the_fifo() {
        let path = format!("borrowed-metadata-fixture-{}",jscompat::random_uuid());
        let caller = std::thread::current().id();
        let borrowed = std::rc::Rc::new(std::cell::Cell::new(0));
        let result:Result<(),NodeError> = run_borrowed(&path,|| {
            assert_eq!(std::thread::current().id(),caller);
            borrowed.set(borrowed.get()+1);
            Err(node_error(&std::io::Error::other("synthetic IO failure"),"write",&path))
        });
        assert!(result.is_err());
        assert_eq!(borrowed.get(),1);
        assert_eq!(run_sync(&path,|| Ok(7)).unwrap(),7);
    }
}
