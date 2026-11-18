use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, TimeZone, Timelike};
use main_core::packages::i18n::Lang;
use main_core::shell::projects::cloud_files::MAX_LISTING_BYTES;
use main_core::shell::projects::memory::LocalDateParts;
use shell_contract::ids::ViewId;
use shell_contract::runtime::Views;
use shell_ipc_w::projects::{PageEvent, PageEvents, ProjectTimer, ProjectsHost, ProjectsHostRef};

use super::super::HostContext;

static HOST: OnceLock<Arc<Platform>> = OnceLock::new();

pub(super) fn install(cx: &HostContext) -> ProjectsHostRef {
    let host = Arc::new(Platform {
        cx: cx.clone(), events: Arc::new(Mutex::new(None)), tracked: Mutex::new(HashSet::new()),
        watching: Mutex::new(HashSet::new()), timers: Arc::new((Mutex::new(HashMap::new()), Condvar::new())),
        next_timer: std::sync::atomic::AtomicU64::new(1),
    });
    let timers = host.timers.clone();
    let events = host.events.clone();
    std::thread::Builder::new().name("project-timers".into()).spawn(move || loop {
        let (lock, changed) = &*timers;
        let mut active = lock.lock().expect("project timer lock poisoned");
        let (key, token) = loop {
            let next = active.iter().filter_map(|(key, (token, deadline))| {
                deadline.map(|deadline| (*key, *token, deadline))
            }).min_by_key(|(_, _, deadline)| *deadline);
            match next {
                None => active = changed.wait(active).expect("project timer lock poisoned"),
                Some((key, token, deadline)) if deadline <= Instant::now() => {
                    active.insert(key, (token, None));
                    break (key, token);
                }
                Some((_, _, deadline)) => {
                    active = changed.wait_timeout(active, deadline.saturating_duration_since(Instant::now()))
                        .expect("project timer lock poisoned").0;
                }
            }
        };
        drop(active);
        if let Some(events) = events.lock().expect("project events lock poisoned").clone() {
            let timers = timers.clone();
            events.send_if(PageEvent::Timer(key), Box::new(move || {
                let mut active = timers.0.lock().expect("project timer lock poisoned");
                if active.get(&key).map(|(current, _)| *current) != Some(token) { return false; }
                active.remove(&key);
                true
            }));
        } else {
            let mut active = timers.0.lock().expect("project timer lock poisoned");
            if active.get(&key).map(|(current, _)| *current) == Some(token) { active.remove(&key); }
        }
    }).expect("cannot start project timer service");
    let weak = Arc::downgrade(&host);
    cx.views.on_destroyed(Box::new(move |view| {
        if let Some(host) = weak.upgrade() { host.event(PageEvent::Destroyed(view)); }
    }));
    if HOST.set(host.clone()).is_err() { panic!("project platform installed twice"); }
    host
}

pub(crate) fn page_event(event: PageEvent) {
    if let Some(host) = HOST.get() { host.event(event); }
}

struct Platform {
    cx: HostContext,
    events: Arc<Mutex<Option<Arc<dyn PageEvents>>>>,
    tracked: Mutex<HashSet<ViewId>>,
    watching: Mutex<HashSet<ViewId>>,
    timers: Arc<(Mutex<HashMap<ProjectTimer, (u64, Option<Instant>)>>, Condvar)>,
    next_timer: std::sync::atomic::AtomicU64,
}

impl Platform {
    fn event(&self, event: PageEvent) {
        let deliver = match event {
            PageEvent::Destroyed(view) => {
                self.watching.lock().expect("project watch lock poisoned").remove(&view);
                self.tracked.lock().expect("project tracking lock poisoned").remove(&view)
            }
            PageEvent::Navigated(view) => self.tracked.lock().expect("project tracking lock poisoned").contains(&view),
            PageEvent::Blur(view) | PageEvent::Focus(view) => self.watching.lock().expect("project watch lock poisoned").contains(&view),
            PageEvent::Timer(_) => true,
        };
        if deliver {
            if let Some(events) = self.events.lock().expect("project events lock poisoned").clone() { events.send(event); }
        }
    }
}

fn local(at_ms: f64) -> Result<chrono::DateTime<Local>, String> {
    if !at_ms.is_finite() || at_ms < i64::MIN as f64 || at_ms > i64::MAX as f64 {
        return Err("Invalid time value".into());
    }
    Local.timestamp_millis_opt(at_ms.trunc() as i64).single().ok_or_else(|| "Invalid time value".into())
}

fn number(count: f64) -> String {
    if count.is_nan() { return "NaN".into(); }
    if count.is_infinite() { return if count.is_sign_negative() { "-∞" } else { "∞" }.into(); }
    let rounded = (count.abs() * 1000.0).round() / 1000.0;
    let text = format!("{rounded:.3}");
    let (whole, fraction) = text.split_once('.').expect("fixed decimal has a point");
    let mut grouped = String::new();
    if count.is_sign_negative() { grouped.push('-'); }
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 { grouped.push(','); }
        grouped.push(digit);
    }
    let fraction = fraction.trim_end_matches('0');
    if !fraction.is_empty() { grouped.push('.'); grouped.push_str(fraction); }
    grouped
}

impl ProjectsHost for Platform {
    fn with_request_owner(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), shell_contract::IpcError>,
    ) -> Result<(), shell_contract::IpcError> {
        crate::ipc::native_owner::with_owner_effect(owner, cancel, effect)
            .ok_or_else(|| shell_contract::IpcError {
                name: Some("Error".into()),
                message: "Renderer document is no longer available.".into(),
            })?
    }
    fn attach(&self, events: Arc<dyn PageEvents>) { *self.events.lock().expect("project events lock poisoned") = Some(events); }
    fn track(&self, view: ViewId) { self.tracked.lock().expect("project tracking lock poisoned").insert(view); }
    fn page_destroyed(&self, view: ViewId) -> bool { self.cx.views.label_of_view(view).is_none() }
    fn watch_return(&self, view: ViewId) { self.watching.lock().expect("project watch lock poisoned").insert(view); }
    fn stop_return(&self, view: ViewId) { self.watching.lock().expect("project watch lock poisoned").remove(&view); }
    fn set_timer(&self, key: ProjectTimer, delay_ms: f64) {
        let delay = if delay_ms.is_finite() { delay_ms.max(0.0) } else { 0.0 };
        let token = self.next_timer.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.timers.0.lock().expect("project timer lock poisoned").insert(
            key, (token, Some(Instant::now() + Duration::from_secs_f64(delay / 1000.0))),
        );
        self.timers.1.notify_one();
    }
    fn clear_timer(&self, key: ProjectTimer) {
        self.timers.0.lock().expect("project timer lock poisoned").remove(&key);
        self.timers.1.notify_one();
    }
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        let mut bytes = vec![0; n];
        getrandom::fill(&mut bytes).expect("operating system random source failed");
        bytes
    }
    fn local_date(&self, at_ms: f64) -> LocalDateParts {
        let at = local(at_ms).expect("invalid local date");
        LocalDateParts { full_year: i64::from(at.year()), month: i64::from(at.month0()), date: i64::from(at.day()),
            hours: i64::from(at.hour()), minutes: i64::from(at.minute()), seconds: i64::from(at.second()) }
    }
    fn os_home_dir(&self) -> String {
        dirs::home_dir().expect("home directory unavailable").to_string_lossy().into_owned()
    }
    fn format_number(&self, count: f64, _lang: Lang) -> Result<String, String> { Ok(number(count)) }
    fn format_date_time(&self, at_ms: f64, lang: Lang) -> Result<String, String> {
        let at = local(at_ms)?;
        let hour = match at.hour() % 12 { 0 => 12, hour => hour };
        if lang == Lang::Ko {
            Ok(format!("{}. {}. {}. {} {}:{:02}", at.year(), at.month(), at.day(), if at.hour() < 12 { "오전" } else { "오후" }, hour, at.minute()))
        } else {
            let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            Ok(format!("{} {}, {}, {}:{:02} {}", months[at.month0() as usize], at.day(), at.year(), hour, at.minute(), if at.hour() < 12 { "AM" } else { "PM" }))
        }
    }
    fn run_listing(&self, file: &str, args: &[String], cwd: &str, env: &[(String, String)], timeout_ms: f64, input: &str) -> Result<String, String> {
        let mut command = Command::new(file);
        command.args(args).current_dir(cwd).env_clear().envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let mut stdin = child.stdin.take().ok_or("listing stdin unavailable")?;
        let mut stdout = child.stdout.take().ok_or("listing stdout unavailable")?;
        let bytes = input.as_bytes().to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&bytes));
        let reader = std::thread::spawn(move || {
            let mut data = Vec::new();
            stdout.by_ref().take(MAX_LISTING_BYTES + 1).read_to_end(&mut data).map(|_| data)
        });
        let started = Instant::now();
        let duration = Duration::from_secs_f64(if timeout_ms.is_finite() { timeout_ms.max(0.0) / 1000.0 } else { 0.0 });
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? { break status; }
            if started.elapsed() >= duration {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                let _ = reader.join();
                return Err("listing timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        writer.join().map_err(|_| "listing input thread failed")?.map_err(|error| error.to_string())?;
        let output = reader.join().map_err(|_| "listing output thread failed")?.map_err(|error| error.to_string())?;
        if !status.success() { return Err(format!("listing process exited with {status}")); }
        if output.len() as u64 > MAX_LISTING_BYTES { return Err("listing output too large".into()); }
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
    fn warn(&self, label: &str, detail: Option<&str>) { eprintln!("{label}: {}", detail.unwrap_or("")); }
}
