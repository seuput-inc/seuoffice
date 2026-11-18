//! Per-settings-file FIFO for asynchronous read-modify-write operations.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, Weak};

type Job = Box<dyn FnOnce(Completion) + Send + 'static>;

#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    running: bool,
}

pub struct SettingsQueue {
    state: Mutex<State>,
}

/// Move this into the final core continuation. Dropping it admits the next job.
pub struct Completion(Arc<SettingsQueue>);

impl Drop for Completion {
    fn drop(&mut self) {
        self.0.dispatch();
    }
}

static QUEUES: LazyLock<Mutex<HashMap<String, Weak<SettingsQueue>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// All in-process users of the same settings path share this queue.
pub fn for_path(path: &str) -> Arc<SettingsQueue> {
    let mut queues = QUEUES.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(queue) = queues.get(path).and_then(Weak::upgrade) { return queue; }
    queues.retain(|_, queue| queue.strong_count() != 0);
    let queue = Arc::new(SettingsQueue { state: Mutex::new(State::default()) });
    queues.insert(path.to_owned(), Arc::downgrade(&queue));
    queue
}

impl SettingsQueue {
    /// Call in invocation order. Each job owns its data and replies later.
    pub fn submit(self: &Arc<Self>, job: impl FnOnce(Completion) + Send + 'static) {
        let start = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.jobs.push_back(Box::new(job));
            if state.running { false } else { state.running = true; true }
        };
        if start { self.dispatch(); }
    }

    fn dispatch(self: &Arc<Self>) {
        let job = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match state.jobs.pop_front() {
                Some(job) => job,
                None => { state.running = false; return; }
            }
        };
        let completion = Completion(self.clone());
        std::thread::spawn(move || job(completion));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn interleaved_updates_do_not_overwrite_each_other() {
        let queue = for_path("/synthetic/settings");
        let settings = Arc::new(Mutex::new(HashMap::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let first = settings.clone();
        queue.submit(move |_completion| {
            let mut snapshot = first.lock().unwrap().clone();
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            snapshot.insert("defaultSaveDir", "new");
            *first.lock().unwrap() = snapshot;
        });
        entered_rx.recv().unwrap();
        let second = settings.clone();
        for_path("/synthetic/settings").submit(move |_completion| {
            let mut snapshot = second.lock().unwrap().clone();
            snapshot.insert("theme", "dark");
            *second.lock().unwrap() = snapshot;
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        let settings = settings.lock().unwrap();
        assert_eq!(settings.get("defaultSaveDir"), Some(&"new"));
        assert_eq!(settings.get("theme"), Some(&"dark"));
    }

    #[test]
    fn core_completion_orders_rapid_reversal_after_failed_write() {
        let queue = for_path("/synthetic/language");
        let (arrive_tx, arrive_rx) = mpsc::channel();
        for desired in ["A", "B", "A"] {
            let arrive = arrive_tx.clone();
            queue.submit(move |completion| { assert!(arrive.send((desired, completion)).is_ok()); });
        }
        let mut current = "A";
        let mut writes = Vec::new();
        for _ in 0..3 {
            let (desired, completion) = arrive_rx.recv().unwrap();
            if desired != current {
                writes.push(desired);
                if desired != "B" { current = desired; }
            }
            drop(completion);
        }
        assert_eq!(writes, vec!["B"]);
        assert_eq!(current, "A");
    }

    #[test]
    fn dropped_continuation_releases_next_operation() {
        let queue = for_path("/synthetic/dropped");
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        queue.submit(move |completion| { assert!(first_tx.send(completion).is_ok()); });
        queue.submit(move |_completion| { second_tx.send(()).unwrap(); });
        let abandoned = first_rx.recv().unwrap();
        assert!(second_rx.try_recv().is_err());
        drop(abandoned);
        second_rx.recv().unwrap();
    }

    #[test]
    fn panic_releases_next_operation() {
        let queue = for_path("/synthetic/panic");
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        queue.submit(move |_completion| {
            release_rx.recv().unwrap();
            panic!("simulated settings failure");
        });
        queue.submit(move |_completion| { done_tx.send(()).unwrap(); });
        release_tx.send(()).unwrap();
        done_rx.recv().unwrap();
    }
}
