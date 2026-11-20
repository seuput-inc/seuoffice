use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use main_core::shell::file_index::extract_worker::{WorkerRequest, WorkerResponse};
use main_core::shell::file_index::indexer::{
    FileIndexer, IndexerConfig, IndexerHost, NameOnlyCall, TimerKind, WorkerId,
};
use main_core::shell::file_index::store::FileIndexStore;

use super::index_worker::{encode_request, response};
use crate::index_worker_process::GuardedWorker;

pub(super) trait IndexSources: Send + Sync {
    fn roots(&self) -> Vec<String>;
    fn extra_paths(&self) -> Vec<String>;
    fn name_only(&self, paths: &[String]) -> HashSet<String>;
}

enum Event {
    Refresh,
    RefreshIfStale(f64),
    Timer(TimerKind),
    Immediate,
    Worker(WorkerId, WorkerResponse),
    WorkerDrop(WorkerId),
    Shutdown(Option<mpsc::SyncSender<Result<(), String>>>),
}

fn timer_key(kind: TimerKind) -> (u8, u64) {
    match kind {
        TimerKind::Rescan => (0, 0),
        TimerKind::Idle => (1, 0),
        TimerKind::Request(id) => (2, id.to_bits()),
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as f64
}

struct Worker {
    process: GuardedWorker,
    input: mpsc::Sender<WorkerRequest>,
    reader: std::thread::JoinHandle<()>,
    writer: std::thread::JoinHandle<()>,
}

struct Driver {
    sender: mpsc::Sender<Event>,
    sources: Arc<dyn IndexSources>,
    timers: HashMap<(u8, u64), (TimerKind, Instant)>,
    workers: HashMap<WorkerId, Worker>,
    next_worker: WorkerId,
    termination_errors: Vec<String>,
}

impl IndexerHost for Driver {
    fn now_ms(&mut self) -> f64 {
        now_ms()
    }
    fn roots(&mut self) -> Vec<String> {
        self.sources.roots()
    }
    fn extra_paths(&mut self) -> Vec<String> {
        self.sources.extra_paths()
    }
    fn name_only(&mut self, paths: &[String]) -> Option<NameOnlyCall> {
        Some(NameOnlyCall::Done(Ok(self.sources.name_only(paths))))
    }
    fn set_timer(&mut self, kind: TimerKind, ms: f64) {
        let delay = if ms.is_finite() { ms.max(0.0) } else { 0.0 };
        self.timers.insert(
            timer_key(kind),
            (
                kind,
                Instant::now() + Duration::from_secs_f64(delay / 1000.0),
            ),
        );
    }
    fn clear_timer(&mut self, kind: TimerKind) {
        self.timers.remove(&timer_key(kind));
    }
    fn set_immediate(&mut self) {
        let _ = self.sender.send(Event::Immediate);
    }
    fn spawn_worker(&mut self, heap_limit_mb: f64) -> Result<WorkerId, String> {
        self.next_worker = self.next_worker.wrapping_add(1);
        let id = self.next_worker;
        let mut process = crate::index_worker_process::spawn(heap_limit_mb)?;
        let (Some(input), Some(stdout)) = (process.child.stdin.take(), process.child.stdout.take())
        else {
            return Err("index worker pipes unavailable".into());
        };
        let sender = self.sender.clone();
        let reader = std::thread::Builder::new()
            .name(format!("file-index-reader-{id}"))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = Vec::new();
                    match reader
                        .by_ref()
                        .take(256 * 1024 * 1024 + 1)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) => break,
                        Ok(_) if line.len() <= 256 * 1024 * 1024 && line.ends_with(b"\n") => {
                            let parsed = serde_json::from_slice(&line)
                                .map_err(|error| error.to_string())
                                .and_then(|value| response(&value));
                            match parsed {
                                Ok(value) => {
                                    if sender.send(Event::Worker(id, value)).is_err() {
                                        return;
                                    }
                                }
                                Err(error) => {
                                    eprintln!("[file-index] invalid worker response: {error}");
                                    break;
                                }
                            }
                        }
                        Ok(_) => {
                            eprintln!("[file-index] worker response exceeds limit");
                            break;
                        }
                        Err(error) => {
                            eprintln!("[file-index] worker read failed: {error}");
                            break;
                        }
                    }
                }
                let _ = sender.send(Event::WorkerDrop(id));
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                drop(input);
                let _ = process.terminate();
                return Err(error.to_string());
            }
        };
        let (send, receive) = mpsc::channel::<WorkerRequest>();
        let events = self.sender.clone();
        let writer = std::thread::Builder::new()
            .name(format!("file-index-writer-{id}"))
            .spawn(move || {
                let mut input = input;
                for request in receive {
                    let result = serde_json::to_writer(&mut input, &encode_request(&request))
                        .map_err(std::io::Error::from)
                        .and_then(|()| input.write_all(b"\n"))
                        .and_then(|()| input.flush());
                    if let Err(error) = result {
                        eprintln!("[file-index] worker write failed: {error}");
                        let _ = events.send(Event::WorkerDrop(id));
                        return;
                    }
                }
            });
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                let _ = process.terminate();
                let _ = reader.join();
                return Err(error.to_string());
            }
        };
        self.workers.insert(
            id,
            Worker {
                process,
                input: send,
                reader,
                writer,
            },
        );
        Ok(id)
    }
    fn post(&mut self, worker: WorkerId, request: &WorkerRequest) -> Result<(), String> {
        self.workers
            .get(&worker)
            .ok_or("index worker is unavailable")?
            .input
            .send(request.clone())
            .map_err(|error| error.to_string())
    }
    fn terminate(&mut self, id: WorkerId) {
        let result = self
            .workers
            .get_mut(&id)
            .map(|worker| worker.process.terminate());
        match result {
            Some(Ok(())) => {
                if let Some(worker) = self.workers.remove(&id) {
                    drop(worker.input);
                    if worker.reader.join().is_err() {
                        self.termination_errors
                            .push("index reader thread panicked".into());
                    }
                    if worker.writer.join().is_err() {
                        self.termination_errors
                            .push("index writer thread panicked".into());
                    }
                }
            }
            Some(Err(error)) => {
                eprintln!("[file-index] worker termination: {error}");
                self.termination_errors.push(error);
            }
            None => {}
        }
    }
}

pub(super) struct IndexScheduler {
    sender: mpsc::Sender<Event>,
    pub store: Arc<Mutex<FileIndexStore>>,
    progress: Arc<Mutex<main_core::shell::file_index::indexer::IndexProgress>>,
}

impl IndexScheduler {
    pub(super) fn new(
        store: FileIndexStore,
        sources: Arc<dyn IndexSources>,
    ) -> Result<Self, String> {
        let store = Arc::new(Mutex::new(store));
        let indexed = store
            .lock()
            .map_err(|error| error.to_string())?
            .count()
            .map_err(|error| error.to_string())?;
        let progress = Arc::new(Mutex::new(
            main_core::shell::file_index::indexer::IndexProgress {
                indexed,
                pending: 0,
                scanning: false,
            },
        ));
        let (sender, receive) = mpsc::channel();
        let mut driver = Driver {
            sender: sender.clone(),
            sources,
            timers: HashMap::new(),
            workers: HashMap::new(),
            next_worker: 0,
            termination_errors: Vec::new(),
        };
        let shared_store = store.clone();
        let shared_progress = progress.clone();
        std::thread::Builder::new()
            .name("file-index-scheduler".into())
            .spawn(move || {
                let mut indexer = FileIndexer::new(IndexerConfig::default());
                let mut stopping = false;
                loop {
                    let next = driver
                        .timers
                        .values()
                        .min_by_key(|(_, deadline)| *deadline)
                        .copied();
                    let event = match next {
                        Some((kind, deadline)) => match receive
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        {
                            Ok(event) => event,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                driver.timers.remove(&timer_key(kind));
                                Event::Timer(kind)
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        },
                        None => match receive.recv() {
                            Ok(event) => event,
                            Err(_) => break,
                        },
                    };
                    if stopping && !matches!(&event, Event::Shutdown(_)) {
                        continue;
                    }
                    let event = match event {
                        Event::Shutdown(done) => {
                            stopping = true;
                            driver.termination_errors.clear();
                            indexer.stop(&mut driver);
                            let remaining: Vec<_> = driver.workers.keys().copied().collect();
                            for worker in remaining {
                                driver.terminate(worker);
                            }
                            let result = if driver.termination_errors.is_empty() {
                                Ok(())
                            } else {
                                Err(driver.termination_errors.join("; "))
                            };
                            let stopped = result.is_ok() || done.is_none();
                            if let Some(done) = done {
                                let _ = done.send(result);
                            }
                            if stopped {
                                break;
                            }
                            driver.timers.clear();
                            continue;
                        }
                        Event::WorkerDrop(worker) => {
                            driver.terminate(worker);
                            Event::WorkerDrop(worker)
                        }
                        other => other,
                    };
                    let store = shared_store.lock().expect("file index lock poisoned");
                    let result = match event {
                        Event::Refresh => {
                            indexer.refresh(&mut driver);
                            Ok(())
                        }
                        Event::RefreshIfStale(age) => {
                            indexer.refresh_if_stale(&mut driver, &store, age)
                        }
                        Event::Timer(kind) => indexer.on_timer(&mut driver, &store, kind),
                        Event::Immediate => indexer.on_immediate(&mut driver, &store),
                        Event::Worker(worker, response) => {
                            if driver.workers.contains_key(&worker) {
                                indexer.on_worker_message(&mut driver, &store, response)
                            } else {
                                Ok(())
                            }
                        }
                        Event::WorkerDrop(worker) => {
                            indexer.on_worker_drop(&mut driver, &store, worker)
                        }
                        Event::Shutdown(_) => {
                            unreachable!("shutdown is handled before taking the store lock")
                        }
                    };
                    if let Err(error) = result {
                        eprintln!("[file-index] {error}");
                    }
                    match indexer.progress(&store) {
                        Ok(value) => {
                            *shared_progress
                                .lock()
                                .expect("index progress lock poisoned") = value
                        }
                        Err(error) => eprintln!("[file-index] progress: {error}"),
                    }
                }
                indexer.stop(&mut driver);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            sender,
            store,
            progress,
        })
    }
    pub(super) fn refresh(&self) {
        let _ = self.sender.send(Event::Refresh);
    }
    pub(super) fn refresh_if_stale(&self, age: f64) {
        let _ = self.sender.send(Event::RefreshIfStale(age));
    }
    pub(super) fn shutdown(&self) -> Result<(), String> {
        let (done, wait) = mpsc::sync_channel(1);
        self.sender
            .send(Event::Shutdown(Some(done)))
            .map_err(|_| "index scheduler stopped before shutdown".to_owned())?;
        wait.recv()
            .map_err(|_| "index scheduler dropped shutdown acknowledgement".to_owned())?
    }
    pub(super) fn progress(&self) -> main_core::shell::file_index::indexer::IndexProgress {
        self.progress
            .lock()
            .expect("index progress lock poisoned")
            .clone()
    }
}

impl Drop for IndexScheduler {
    fn drop(&mut self) {
        let _ = self.sender.send(Event::Shutdown(None));
    }
}
