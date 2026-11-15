//! Cross-process subscription serialization and informational usage accounting.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STALE: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(250);
const TIMEOUT: Duration = Duration::from_secs(1800);
const WINDOW_MS: f64 = 3_600_000.0;
const LOCK: &str = "headless-subscription.lock";
const LEDGER: &str = "headless-subscription-ledger.json";

#[derive(Debug)]
pub enum AcquireError {
    Cancelled,
    Timeout,
    Io(String),
}

pub struct Options {
    pub window_ms: f64,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub is_process_alive: Arc<dyn Fn(u32) -> bool + Send + Sync>,
    pub stale: Duration,
    pub heartbeat: Duration,
    pub poll: Duration,
    pub queue_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            window_ms: WINDOW_MS,
            now: Arc::new(now),
            is_process_alive: Arc::new(alive),
            stale: STALE,
            heartbeat: HEARTBEAT,
            poll: POLL,
            queue_timeout: TIMEOUT,
        }
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}
fn seen(path: &Path) -> Option<(String, SystemTime)> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some((fs::read_to_string(path).ok()?, modified))
}
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        CloseHandle(handle);
        true
    }
}
fn exclusive(path: &Path, data: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(data.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}
fn random_token() -> Result<String, AcquireError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| AcquireError::Io(e.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn runs(dir: &Path, current: f64, window: f64) -> Vec<f64> {
    let parsed = fs::read_to_string(dir.join(LEDGER))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("runs")?
                .as_array()?
                .iter()
                .map(|n| n.as_f64().filter(|n| n.is_finite()))
                .collect::<Option<Vec<_>>>()
        });
    let mut runs: Vec<_> = parsed
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.min(current))
        .filter(|n| *n > current - window)
        .collect();
    runs.sort_by(f64::total_cmp);
    runs
}
pub fn usage(dir: &Path) -> usize {
    runs(dir, now(), WINDOW_MS).len()
}

pub struct Turn {
    path: PathBuf,
    content: String,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.heartbeat.take() {
            let _ = thread.join();
        }
        if fs::read_to_string(&self.path).ok().as_deref() == Some(&self.content) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn acquire(
    dir: &Path,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Turn, AcquireError> {
    acquire_with_options(dir, cancelled, Options::default())
}

pub fn acquire_with_options(
    dir: &Path,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    options: Options,
) -> Result<Turn, AcquireError> {
    fs::create_dir_all(dir).map_err(|error| AcquireError::Io(error.to_string()))?;
    let path = dir.join(LOCK);
    let token = random_token()?;
    let content =
        serde_json::json!({"token": token, "pid": std::process::id(), "acquiredAt": now()})
            .to_string();
    let started = Instant::now();
    let mut denied: Option<Instant> = None;
    let mut suspect: Option<(String, SystemTime, Instant)> = None;
    loop {
        if cancelled() {
            return Err(AcquireError::Cancelled);
        }
        let attempt = exclusive(&path, &content);
        match attempt {
            Ok(()) => break,
            Err(error) => {
                let held = error.kind() == io::ErrorKind::AlreadyExists
                    || (matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
                    ) && seen(&path).is_some());
                if held {
                    denied = None;
                    if let Some((raw, modified)) = seen(&path) {
                        let pid = serde_json::from_str::<serde_json::Value>(&raw)
                            .ok()
                            .and_then(|v| v.get("pid")?.as_u64())
                            .and_then(|v| u32::try_from(v).ok())
                            .filter(|v| *v > 0);
                        let gone = pid.is_some_and(|pid| !(options.is_process_alive)(pid));
                        let stale = modified.elapsed().is_ok_and(|age| age > options.stale);
                        let stable = suspect.as_ref().is_some_and(|(prior, at, since)| {
                            prior == &raw && *at == modified && since.elapsed() >= options.heartbeat
                        });
                        if gone || (stale && stable) {
                            let guard = dir.join("headless-subscription.lock.takeover");
                            match exclusive(&guard, &std::process::id().to_string()) {
                                Ok(()) => {
                                    let same = seen(&path).is_some_and(|(again, at)| {
                                        again == raw && (gone || at == modified)
                                    });
                                    let removed = same && fs::remove_file(&path).is_ok();
                                    let _ = fs::remove_file(&guard);
                                    if removed {
                                        continue;
                                    }
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        io::ErrorKind::AlreadyExists
                                            | io::ErrorKind::PermissionDenied
                                            | io::ErrorKind::ResourceBusy
                                    ) =>
                                {
                                    if seen(&guard).is_some_and(|(_, at)| {
                                        at.elapsed().is_ok_and(|age| age > options.stale)
                                    }) {
                                        let _ = fs::remove_file(&guard);
                                    }
                                }
                                Err(error) => return Err(AcquireError::Io(error.to_string())),
                            }
                            suspect = None;
                        } else if stale
                            && !suspect
                                .as_ref()
                                .is_some_and(|(prior, at, _)| prior == &raw && *at == modified)
                        {
                            suspect = Some((raw, modified, Instant::now()));
                        } else if !stale {
                            suspect = None;
                        }
                    } else {
                        suspect = None;
                    }
                } else if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
                ) {
                    if denied.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                        return Err(AcquireError::Io(format!(
                            "Cannot create the lock file {}",
                            path.display()
                        )));
                    }
                } else {
                    return Err(AcquireError::Io(error.to_string()));
                }
            }
        }
        if started.elapsed() >= options.queue_timeout {
            return Err(AcquireError::Timeout);
        }
        std::thread::sleep(options.poll);
    }
    let (stop, rx) = mpsc::channel();
    let (beat_path, beat_content) = (path.clone(), content.clone());
    let heartbeat_interval = options.heartbeat;
    let heartbeat = std::thread::spawn(move || {
        while matches!(
            rx.recv_timeout(heartbeat_interval),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            if fs::read_to_string(&beat_path).ok().as_deref() != Some(&beat_content) {
                break;
            }
            if let Ok(file) = File::open(&beat_path) {
                let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
            }
        }
    });
    let turn = Turn {
        path,
        content,
        stop: Some(stop),
        heartbeat: Some(heartbeat),
    };
    let current = (options.now)();
    let mut records = runs(dir, current, options.window_ms);
    records.push(current);
    let ledger = dir.join(LEDGER);
    let temp = dir.join(format!("{LEDGER}.{token}.tmp"));
    if fs::write(
        &temp,
        serde_json::json!({"version": 1, "runs": records}).to_string(),
    )
    .is_ok()
    {
        for attempt in 0..5 {
            match fs::rename(&temp, &ledger) {
                Ok(()) => break,
                Err(error)
                    if attempt < 4
                        && matches!(
                            error.kind(),
                            io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
                        ) =>
                {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => break,
            }
        }
    }
    let _ = fs::remove_file(temp);
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn queued_cancellation_does_not_charge_or_release_the_holder() {
        let dir = std::env::temp_dir().join(format!("headless-ledger-{}", random_token().unwrap()));
        let holder = acquire(&dir, Arc::new(|| false)).unwrap();
        assert_eq!(usage(&dir), 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_dir = dir.clone();
        let (started, received) = mpsc::channel();
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = std::thread::spawn(move || {
            acquire(
                &worker_dir,
                Arc::new(move || {
                    if checks.fetch_add(1, Ordering::SeqCst) == 1 {
                        let _ = started.send(());
                    }
                    worker_cancel.load(Ordering::SeqCst)
                }),
            )
        });
        received.recv().unwrap();
        cancel.store(true, Ordering::SeqCst);
        assert!(matches!(
            worker.join().unwrap(),
            Err(AcquireError::Cancelled)
        ));
        assert_eq!(usage(&dir), 1);
        assert!(dir.join(LOCK).exists());
        drop(holder);
        let next = acquire(&dir, Arc::new(|| false)).unwrap();
        assert_eq!(usage(&dir), 2);
        drop(next);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dead_owner_is_taken_over_and_start_is_recorded() {
        let dir = std::env::temp_dir().join(format!("headless-ledger-{}", random_token().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(LOCK),
            "{\"token\":\"old\",\"pid\":123,\"acquiredAt\":0}",
        )
        .unwrap();
        let options = Options {
            now: Arc::new(|| 200.0),
            is_process_alive: Arc::new(|_| false),
            ..Options::default()
        };
        let turn = acquire_with_options(&dir, Arc::new(|| false), options).unwrap();
        assert_eq!(runs(&dir, 200.0, 150.0), vec![200.0]);
        assert!(
            !fs::read_to_string(dir.join(LOCK))
                .unwrap()
                .contains("\"old\"")
        );
        drop(turn);
        assert!(!dir.join(LOCK).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_ledger_is_informational_and_future_entries_expire_from_now() {
        let dir = std::env::temp_dir().join(format!("headless-ledger-{}", random_token().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(LEDGER), "{\"runs\":[100,10000,-1]}").unwrap();
        assert_eq!(runs(&dir, 200.0, 150.0), vec![100.0, 200.0]);
        fs::write(dir.join(LEDGER), "{\"runs\":[\"bad\"]}").unwrap();
        assert!(runs(&dir, 200.0, 150.0).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
