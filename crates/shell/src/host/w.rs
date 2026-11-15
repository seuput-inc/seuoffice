//! Batch W's process clock and core-thread access to registered services.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use shell_contract::Hosts;
use shell_contract::ipc::{RegisterError, Registry};
use shell_contract::provided::Provided;
use shell_contract::runtime::{Services, run_on};
use shell_ipc_w::editor_files::{Clock, ClockHost};
use shell_ipc_w::tabs::TabHooks;

use super::HostContext;

/// `Date.now()`
struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }
}

pub fn install(hosts: &mut Hosts, _cx: &HostContext) {
    let clock: ClockHost = Arc::new(SystemClock);
    hosts.insert(clock);
}


thread_local! {
    /// What the batches provide each other, kept on the core thread for the
    /// shell's own hosts whose calls start elsewhere (a menu click, a worker)
    static CORE_PROVIDED: RefCell<Option<Rc<Provided>>> = const { RefCell::new(None) };
}

/// The shell's services, once the batches registered with them
static SERVICES: OnceLock<Services> = OnceLock::new();

/// Runs `job` with the core thread's [`Provided`]: at once on the core
/// thread, else through `run_on(core, ..)`. `None` before the batches registered.
#[allow(dead_code)]
pub(crate) fn on_core<R: Send + 'static>(
    job: impl FnOnce(&Provided) -> R + Send + 'static,
) -> Option<R> {
    let core = SERVICES.get()?.core.clone();
    let run = move || CORE_PROVIDED.with(|p| p.borrow().as_deref().map(job));
    if core.is_current() {
        run()
    } else {
        run_on(core.as_ref(), run)
    }
}

/// Runs a non-Send continuation only on the owning core thread.
/// Returns false before registration or when called from another thread.
pub(crate) fn on_core_local(job: impl FnOnce(&Provided)) -> bool {
    on_core_services_local(|provided, _| job(provided))
}

/// Read-only access to the already installed core services; never posts or waits.
pub(crate) fn on_core_services_local(job: impl FnOnce(&Provided, &Services)) -> bool {
    let Some(services) = SERVICES.get() else {
        return false;
    };
    if !services.core.is_current() {
        return false;
    }
    CORE_PROVIDED.with(|provided| {
        let provided = provided.borrow();
        let Some(provided) = provided.as_deref() else {
            return false;
        };
        job(provided, services);
        true
    })
}

/// The batch that comes before the six: it adds what the shell provides on
/// the core thread. `tabs::register` takes the first `TabHooks` it finds.
/// Posts `job` to the core thread with its Provided and the shell's
/// Services; never waits. Dropped before the batches registered.
#[allow(dead_code)]
pub(crate) fn post_core(job: impl FnOnce(&Provided, &Services) + Send + 'static) {
    let Some(services) = SERVICES.get() else {
        return;
    };
    services.core.post(Box::new(move || {
        CORE_PROVIDED.with(|p| {
            if let (Some(provided), Some(services)) = (p.borrow().as_deref(), SERVICES.get()) {
                job(provided, services);
            }
        })
    }));
}

struct CorePostFailure(Option<Box<dyn FnOnce() + Send>>);
impl Drop for CorePostFailure {
    fn drop(&mut self) {
        if let Some(failure) = self.0.take() { failure(); }
    }
}

/// Posts without waiting, reporting missing registration before or on the core task.
pub(crate) fn post_core_checked(
    job: impl FnOnce(&Provided, &Services) + Send + 'static,
    on_unavailable: impl FnOnce() + Send + 'static,
) {
    let Some(services) = SERVICES.get().cloned() else {
        on_unavailable();
        return;
    };
    let core = services.core.clone();
    let mut failure = CorePostFailure(Some(Box::new(on_unavailable)));
    core.post(Box::new(move || {
        let provided = CORE_PROVIDED.with(|provided| provided.borrow().clone());
        if let Some(provided) = provided {
            failure.0.take();
            job(&provided, &services);
        }
    }));
}

pub fn register_hooks(
    registry: &mut dyn Registry,
    services: &Services,
) -> Result<(), RegisterError> {
    let _ = SERVICES.set(services.clone());
    let provided = registry.provided();
    let hooks = super::w_tabs::hooks(services, provided.clone())?;
    provided.add::<dyn TabHooks>(hooks);
    CORE_PROVIDED.with(|p| *p.borrow_mut() = Some(provided));
    Ok(())
}


#[cfg(test)]
mod checked_post_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn discarded_guard_reports_failure_once_and_disarmed_guard_reports_nothing() {
        let failures = Arc::new(AtomicUsize::new(0));
        let observed = failures.clone();
        let guard = CorePostFailure(Some(Box::new(move || { observed.fetch_add(1, Ordering::SeqCst); })));
        drop(guard);
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        let observed = failures.clone();
        let mut guard = CorePostFailure(Some(Box::new(move || { observed.fetch_add(1, Ordering::SeqCst); })));
        drop(guard.0.take());
        drop(guard);
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bounded_queue_outcomes_settle_one_terminal_path() {
        // Model the actual guard transitions: queue rejection, queued missing
        // registration, discard while entering, or accepted registered job.
        for queued in [false, true] {
            for registered in [false, true] {
                for panic_before_disarm in [false, true] {
                    let failures = Arc::new(AtomicUsize::new(0));
                    let jobs = Arc::new(AtomicUsize::new(0));
                    let observed = failures.clone();
                    let mut guard = CorePostFailure(Some(Box::new(move || { observed.fetch_add(1, Ordering::SeqCst); })));
                    if queued && registered && !panic_before_disarm {
                        drop(guard.0.take());
                        jobs.fetch_add(1, Ordering::SeqCst);
                    }
                    drop(guard);
                    assert_eq!(failures.load(Ordering::SeqCst) + jobs.load(Ordering::SeqCst), 1);
                    assert_eq!(jobs.load(Ordering::SeqCst), usize::from(queued && registered && !panic_before_disarm));
                }
            }
        }
    }

    #[test]
    fn failure_callback_can_reenter_its_own_completion_slot() {
        let slot = Arc::new(std::sync::Mutex::new(Some(7)));
        let other = slot.clone();
        let guard = CorePostFailure(Some(Box::new(move || {
            assert_eq!(other.lock().unwrap().take(), Some(7));
        })));
        drop(guard);
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn unwind_before_entry_reports_failure_but_job_panic_after_entry_does_not() {
        let failures = Arc::new(AtomicUsize::new(0));
        let observed = failures.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = CorePostFailure(Some(Box::new(move || { observed.fetch_add(1, Ordering::SeqCst); })));
            panic!("discarded before registration");
        });
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        let observed = failures.clone();
        let _ = std::panic::catch_unwind(move || {
            let mut guard = CorePostFailure(Some(Box::new(move || { observed.fetch_add(1, Ordering::SeqCst); })));
            drop(guard.0.take());
            panic!("accepted job failed");
        });
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }
}
