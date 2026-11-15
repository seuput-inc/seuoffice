use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use main_core::packages::i18n::Lang;
use main_core::shell::quick_look::{ParsedFile, QuickLookCache, QuickLookEngines, ZipArchive};
use shell_contract::hosts::OsShell;
use shell_contract::ids::ViewId;
use shell_contract::ipc::{EventSink, Target};
use shell_contract::runtime::Workers;
use shell_contract::IpcValue;

use super::*;

#[derive(Default)]
struct Window(AtomicUsize);
impl QuickLookWindow for Window {
    fn attach(&self, _: QuickLookControl) {}
    fn lang(&self) -> Lang { Lang::En }
    fn theme(&self) -> String { "system".into() }
    fn create(&self, _: &QuickLookWindowOptions) -> ViewId {
        self.0.fetch_add(1, Ordering::SeqCst);
        ViewId(7)
    }
    fn is_visible(&self) -> bool { false }
    fn set_title(&self, _: &str) {}
    fn show_inactive(&self) {}
    fn hide(&self) {}
    fn destroy(&self) {}
    fn open_in_editor(&self, _: &str) {}
}

struct Runtime;
impl QuickLookRuntime for Runtime {
    fn attach(&self, _: QuickLookControl) {}
    fn windows_explorer(&self) -> bool { true }
    fn sync(&self, _: &QuickLookSettings) {}
    fn refresh_tray(&self) {}
    fn stop(&self) {}
}

struct Engine {
    entered: Option<Arc<Barrier>>,
    release: Option<Arc<Barrier>>,
}
impl QuickLookEngines for Engine {
    fn parse_file_to_text(&self, _: &str) -> Result<ParsedFile, String> {
        if let Some(gate) = &self.entered { gate.wait(); }
        if let Some(gate) = &self.release { gate.wait(); }
        Ok(ParsedFile { ok: true, text: Some("sample".into()), error: None })
    }
    fn open_zip(&self, _: Vec<u8>) -> Option<Box<dyn ZipArchive>> { None }
}

struct Shell;
impl OsShell for Shell {
    fn open_external(&self, _: &str) -> Result<(), String> { Ok(()) }
    fn open_path(&self, _: &Path) -> String { String::new() }
    fn show_item_in_folder(&self, _: &Path) {}
    fn trash_item(&self, _: &Path) -> Result<(), String> { Ok(()) }
    fn trash_item_authorized(&self, path: &std::path::Path, admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<(), String> {
        admit(&mut || self.trash_item(path))
    }
}
struct Events;
impl EventSink for Events {
    fn emit(&self, _: Target, _: &'static str, _: Vec<IpcValue>) {}
}
struct TestWorkers(Mutex<Option<Arc<Barrier>>>);
impl Workers for TestWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        let gate = self.0.lock().unwrap().take();
        std::thread::spawn(move || {
            if let Some(gate) = gate { gate.wait(); }
            job();
        });
    }
}

fn control(window: Arc<Window>, engine: Engine, gate: Option<Arc<Barrier>>) -> QuickLookControl {
    QuickLookControl(Arc::new(Inner {
        state: Mutex::default(), window, runtime: Arc::new(Runtime), engines: Arc::new(engine),
        os_shell: Arc::new(Shell), cache: Mutex::new(QuickLookCache::default()),
        events: Arc::new(Events), workers: Arc::new(TestWorkers(Mutex::new(gate))),
        pending: Pending::default(),
    }))
}

fn settings(enabled: bool) -> QuickLookSettings {
    QuickLookSettings { explorer_space_preview: enabled, keep_in_tray: false }
}

fn sample_path() -> String {
    std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../README.md"))
        .unwrap().to_string_lossy().into_owned()
}

fn wait_idle(control: &QuickLookControl) {
    let mut jobs = control.0.pending.jobs.lock().unwrap();
    while *jobs != 0 {
        jobs = control.0.pending.idle.wait(jobs).unwrap();
    }
}

#[test]
fn queued_explorer_space_cannot_reopen_after_disable_and_reenable() {
    let gate = Arc::new(Barrier::new(2));
    let window = Arc::new(Window::default());
    let control = control(window.clone(), Engine { entered: None, release: None }, Some(gate.clone()));
    control.sync_runtime(&settings(true));
    control.explorer_space(&sample_path());
    control.sync_runtime(&settings(false));
    control.sync_runtime(&settings(true));
    gate.wait();
    wait_idle(&control);
    assert_eq!(window.0.load(Ordering::SeqCst), 0);
    control.explorer_space(&sample_path());
    wait_idle(&control);
    assert_eq!(window.0.load(Ordering::SeqCst), 1);
}

#[test]
fn in_flight_extraction_cannot_publish_into_a_later_generation() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let window = Arc::new(Window::default());
    let control = control(window.clone(), Engine { entered: Some(entered.clone()), release: Some(release.clone()) }, None);
    control.sync_runtime(&settings(true));
    let path = sample_path();
    control.explorer_space(&path);
    entered.wait();
    control.sync_runtime(&settings(false));
    control.sync_runtime(&settings(true));
    // A later selection of the same path must not validate the older extraction.
    control.0.lock().current_path = Some(path);
    release.wait();
    wait_idle(&control);
    assert_eq!(window.0.load(Ordering::SeqCst), 0);
}
