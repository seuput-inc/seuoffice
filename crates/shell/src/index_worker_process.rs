//! Private indexing worker process and optional operating-system limits.
//!
//! By default only V8's requested old-generation heap limit is applied. An
//! explicit OS budget limits job-wide committed bytes on Windows, or each
//! process's virtual address space independently on Unix. It is not an
//! aggregate physical-memory limit on Unix. A process group lets normal
//! shutdown terminate descendants that remain in that group.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const FLAG: &str = "--internal-file-index-worker";
const HEAP_PREFIX: &str = "--file-index-heap-mb=";
const PDF_IMAGE_FLAG: &str = "--internal-pdf-image-decode";
const PDF_SIGN_FLAG: &str = "--internal-pdf-sign-worker";
const ODT_FLAG: &str = "--internal-odt-convert-worker";

fn private_mode(args: &[std::ffi::OsString]) -> Result<Option<&'static str>, String> {
    if args
        .iter()
        .any(|arg| arg.to_str().is_some_and(|s| s.starts_with("--type=")))
    {
        return Ok(None);
    }
    let modes = [FLAG, PDF_IMAGE_FLAG, PDF_SIGN_FLAG, ODT_FLAG];
    let mut selected = None;
    for mode in modes {
        if args.iter().any(|arg| arg == mode) {
            if selected.is_some() {
                return Err("incompatible private worker modes".into());
            }
            selected = Some(mode);
        }
    }
    Ok(selected)
}

pub(crate) fn dispatch_if_odt_worker() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().collect();
    if private_mode(&args)? != Some(ODT_FLAG) {
        return Ok(false);
    }
    let output = isolate_protocol_stdout()?;
    crate::odt_worker::run_child(output)?;
    Ok(true)
}

/// Invoke before CEF and before any threads are started.
pub(crate) fn dispatch_if_sign_worker() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().collect();
    if private_mode(&args)? != Some(PDF_SIGN_FLAG) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    crate::signer_child_guard::require_owned_children()?;
    let output = isolate_protocol_stdout()?;
    crate::host::d2::pdf::crypto::run_sign_worker(output)?;
    Ok(true)
}

/// Invoke before CEF and before any threads are started.
pub(crate) fn dispatch_if_decode_worker() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().collect();
    if private_mode(&args)? != Some(PDF_IMAGE_FLAG) {
        return Ok(false);
    }
    let output = isolate_protocol_stdout()?;
    crate::host::d2::pdf::native_engine::image_edits::run_image_decode_worker(output)?;
    Ok(true)
}

pub(crate) struct GuardedDecodeWorker {
    pub child: Child,
    guard: platform::Guard,
    terminated: bool,
}

impl GuardedDecodeWorker {
    pub fn terminate(&mut self) -> Result<(), String> {
        if self.terminated {
            return Ok(());
        }
        self.guard.terminate(&mut self.child)?;
        self.child
            .wait()
            .map_err(|e| format!("waiting for image decoder: {e}"))?;
        self.terminated = true;
        Ok(())
    }
}

impl Drop for GuardedDecodeWorker {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("[pdf-image-decoder] cleanup: {error}");
        }
    }
}

pub(crate) fn spawn_pdf_image_decoder() -> Result<GuardedDecodeWorker, String> {
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg(PDF_IMAGE_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (child, guard) = platform::spawn(&mut command, None)?;
    Ok(GuardedDecodeWorker {
        child,
        guard,
        terminated: false,
    })
}

pub(crate) fn spawn_pdf_signer() -> Result<GuardedDecodeWorker, String> {
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg(PDF_SIGN_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (child, guard) = platform::spawn(&mut command, None)?;
    Ok(GuardedDecodeWorker {
        child,
        guard,
        terminated: false,
    })
}

pub(crate) fn spawn_odt_worker() -> Result<GuardedDecodeWorker, String> {
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg(ODT_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (child, guard) = platform::spawn(&mut command, None)?;
    Ok(GuardedDecodeWorker {
        child,
        guard,
        terminated: false,
    })
}

#[cfg(all(test, unix))]
pub(crate) fn spawn_stalled_odt_test_worker() -> Result<GuardedDecodeWorker, String> {
    let mut command = Command::new("sh");
    command
        .args(["-c", "exec sleep 30"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let (child, guard) = platform::spawn(&mut command, None)?;
    Ok(GuardedDecodeWorker {
        child,
        guard,
        terminated: false,
    })
}

fn heap_mb(value: f64) -> Result<u32, String> {
    if !value.is_finite() || value < 1.0 || value.fract() != 0.0 || value > u32::MAX as f64 {
        return Err("file-index worker heap limit must be a positive integer number of MiB".into());
    }
    Ok(value as u32)
}

/// Called before ordinary shell startup. CEF subprocesses must enter CEF, not
/// run the indexing protocol even if their inherited arguments include FLAG.
pub(crate) fn worker_heap_mb() -> Result<Option<u32>, String> {
    let args: Vec<_> = std::env::args_os().collect();
    if private_mode(&args)? != Some(FLAG) {
        return Ok(None);
    }
    let mut found = None;
    for arg in &args {
        if let Some(text) = arg.to_str().and_then(|s| s.strip_prefix(HEAP_PREFIX)) {
            if found.is_some() {
                return Err("duplicate file-index worker heap limit".into());
            }
            let number = text
                .parse::<f64>()
                .map_err(|_| "invalid file-index worker heap limit")?;
            found = Some(heap_mb(number)?);
        }
    }
    let heap = found.ok_or("missing file-index worker heap limit")?;
    if std::env::var_os("SEUOFFICE_USER_DATA").is_none() {
        return Err("file-index worker profile is missing".into());
    }
    Ok(Some(heap))
}

/// Reserve the protocol pipe and send incidental native stdout to stderr.
/// Call once, before starting CEF or any worker threads.
pub(crate) fn isolate_protocol_stdout() -> Result<std::fs::File, String> {
    platform::isolate_protocol_stdout()
}

/// Owns the OS containment handle and the worker's private profile.
pub(crate) struct GuardedWorker {
    pub child: Child,
    profile: PathBuf,
    guard: platform::Guard,
    terminated: bool,
}

impl GuardedWorker {
    /// Idempotent. Kill descendants before waiting or removing their profile.
    pub fn terminate(&mut self) -> Result<(), String> {
        if self.terminated {
            return Ok(());
        }
        self.guard.terminate(&mut self.child)?;
        self.child
            .wait()
            .map_err(|e| format!("waiting for index worker: {e}"))?;
        std::fs::remove_dir_all(&self.profile)
            .map_err(|e| format!("removing index worker profile: {e}"))?;
        self.terminated = true;
        Ok(())
    }
}
impl Drop for GuardedWorker {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("[file-index] worker cleanup: {error}");
        }
    }
}

pub(crate) fn spawn(heap_limit_mb: f64) -> Result<GuardedWorker, String> {
    spawn_with_budget(heap_limit_mb, None)
}

/// An optional OS budget in bytes. Windows applies it to aggregate committed
/// memory for the job; Unix applies it to virtual address space per process.
/// Callers must choose a value compatible with CEF's sparse reservations.
pub(crate) fn spawn_with_budget(
    heap_limit_mb: f64,
    os_budget: Option<std::num::NonZeroU64>,
) -> Result<GuardedWorker, String> {
    let heap = heap_mb(heap_limit_mb)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| format!("creating index worker profile: {e}"))?;
    let suffix = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let profile = std::env::temp_dir().join(format!("seuoffice-index-{suffix}"));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&profile)
        .map_err(|e| format!("creating index worker profile: {e}"))?;
    let result = (|| {
        let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .arg(FLAG)
            .arg(format!("{HEAP_PREFIX}{heap}"))
            .arg(format!("--js-flags=--max-old-space-size={heap}"))
            .env("SEUOFFICE_USER_DATA", &profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let (child, guard) = platform::spawn(&mut command, os_budget)?;
        Ok(GuardedWorker {
            child,
            profile: profile.clone(),
            guard,
            terminated: false,
        })
    })();
    if result.is_err() {
        if let Err(error) = std::fs::remove_dir_all(&profile) {
            eprintln!("[file-index] profile cleanup: {error}");
        }
    }
    result
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;

    pub fn isolate_protocol_stdout() -> Result<std::fs::File, String> {
        // SAFETY: fcntl returns a new descriptor; File takes its ownership.
        let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(format!(
                "duplicating worker protocol: {}",
                std::io::Error::last_os_error()
            ));
        }
        let protocol = unsafe { std::fs::File::from_raw_fd(fd) };
        if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
            return Err(format!(
                "redirecting worker stdout: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(protocol)
    }

    pub struct Guard {
        pgid: libc::pid_t,
    }
    pub fn spawn(
        command: &mut Command,
        budget: Option<std::num::NonZeroU64>,
    ) -> Result<(Child, Guard), String> {
        let budget = budget
            .map(|v| libc::rlim_t::try_from(v.get()).map_err(|_| "index worker limit overflow"))
            .transpose()?;
        // SAFETY: the closure only invokes async-signal-safe libc calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(bytes) = budget {
                    let mut old = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                    if libc::getrlimit(libc::RLIMIT_AS, old.as_mut_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let old = old.assume_init();
                    let limit = bytes.min(old.rlim_max);
                    let desired = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &desired) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("starting guarded index worker: {e}"))?;
        let pgid = match child_id(&child) {
            Ok(id) => id,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok((child, Guard { pgid }))
    }
    fn child_id(child: &Child) -> Result<libc::pid_t, String> {
        libc::pid_t::try_from(child.id()).map_err(|_| "index worker PID overflow".into())
    }
    impl Guard {
        pub fn terminate(&mut self, child: &mut Child) -> Result<(), String> {
            // SAFETY: this group was created by the child before exec and has a positive ID.
            if unsafe { libc::kill(-self.pgid, libc::SIGKILL) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("terminating index worker group: {e}"));
                }
            }
            match child.kill() {
                Ok(()) => Ok(()),
                Err(error) => match child.try_wait() {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => Err(format!("terminating index worker: {error}")),
                    Err(wait_error) => Err(format!(
                        "checking index worker after termination failed: {wait_error}"
                    )),
                },
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::System::Threading::{
        CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };
    use windows::core::PCWSTR;

    #[link(name = "ucrt")]
    unsafe extern "C" {
        fn _dup2(source: i32, target: i32) -> i32;
    }

    pub fn isolate_protocol_stdout() -> Result<std::fs::File, String> {
        // SAFETY: the duplicated handle is independently owned by File.
        unsafe {
            let output = GetStdHandle(STD_OUTPUT_HANDLE)
                .map_err(|e| format!("getting worker stdout: {e}"))?;
            let stderr = GetStdHandle(STD_ERROR_HANDLE)
                .map_err(|e| format!("getting worker stderr: {e}"))?;
            let process = GetCurrentProcess();
            let mut duplicate = HANDLE::default();
            DuplicateHandle(
                process,
                output,
                process,
                &mut duplicate,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .map_err(|e| format!("duplicating worker protocol: {e}"))?;
            let protocol = std::fs::File::from_raw_handle(duplicate.0);
            if _dup2(2, 1) != 0 {
                return Err(format!(
                    "redirecting worker CRT stdout: {}",
                    std::io::Error::last_os_error()
                ));
            }
            SetStdHandle(STD_OUTPUT_HANDLE, stderr)
                .map_err(|e| format!("redirecting worker stdout: {e}"))?;
            Ok(protocol)
        }
    }

    pub struct Guard {
        job: OwnedHandle,
    }
    fn handle(owned: &OwnedHandle) -> HANDLE {
        HANDLE(owned.as_raw_handle())
    }
    fn owned(raw: HANDLE) -> OwnedHandle {
        unsafe { OwnedHandle::from_raw_handle(raw.0) }
    }
    pub fn spawn(
        command: &mut Command,
        budget: Option<std::num::NonZeroU64>,
    ) -> Result<(Child, Guard), String> {
        let limit = budget
            .map(|v| usize::try_from(v.get()).map_err(|_| "index worker job limit overflow"))
            .transpose()?;
        // SAFETY: owned handles are closed by OwnedHandle; structure size matches its class.
        let job =
            owned(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(|e| e.to_string())?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(limit) = limit {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = limit;
        }
        unsafe {
            SetInformationJobObject(
                handle(&job),
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of_val(&info) as u32,
            )
        }
        .map_err(|e| format!("setting index worker job limit: {e}"))?;
        command.creation_flags(CREATE_SUSPENDED.0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("starting suspended index worker: {e}"))?;
        let setup = (|| {
            unsafe { AssignProcessToJobObject(handle(&job), HANDLE(child.as_raw_handle())) }
                .map_err(|e| format!("assigning index worker job: {e}"))?;
            let snapshot = owned(
                unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
                    .map_err(|e| format!("finding worker thread: {e}"))?,
            );
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            unsafe { Thread32First(handle(&snapshot), &mut entry) }
                .map_err(|e| format!("finding worker thread: {e}"))?;
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    break;
                }
                unsafe { Thread32Next(handle(&snapshot), &mut entry) }
                    .map_err(|e| format!("finding worker thread: {e}"))?;
            }
            let thread = owned(
                unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                    .map_err(|e| format!("opening worker thread: {e}"))?,
            );
            if unsafe { ResumeThread(handle(&thread)) } != 1 {
                return Err("could not resume index worker's initial thread".to_owned());
            }
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = unsafe { TerminateJobObject(handle(&job), 1) };
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, Guard { job }))
    }
    impl Guard {
        pub fn terminate(&mut self, child: &mut Child) -> Result<(), String> {
            if let Err(error) = unsafe { TerminateJobObject(handle(&self.job), 1) } {
                if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                    return Err(format!("terminating index worker job: {error}"));
                }
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                // SAFETY: the output buffer has the size and type required by this information class.
                unsafe {
                    QueryInformationJobObject(
                        Some(handle(&self.job)),
                        JobObjectBasicAccountingInformation,
                        &mut accounting as *mut _ as _,
                        std::mem::size_of_val(&accounting) as u32,
                        None,
                    )
                }
                .map_err(|e| format!("checking index worker job: {e}"))?;
                if accounting.ActiveProcesses == 0 {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(
                        "index worker descendants did not terminate within 10 seconds".into(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub struct Guard;
    pub fn isolate_protocol_stdout() -> Result<std::fs::File, String> {
        Err("index worker protocol isolation is unavailable on this platform".into())
    }
    pub fn spawn(
        _: &mut Command,
        _: Option<std::num::NonZeroU64>,
    ) -> Result<(Child, Guard), String> {
        Err("index worker process containment is unavailable on this platform".into())
    }
    impl Guard {
        pub fn terminate(&mut self, _: &mut Child) -> Result<(), String> {
            Err("index worker memory guard is unavailable on this platform".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heap_limit_requires_positive_finite_whole_mebibytes() {
        assert_eq!(heap_mb(256.0).unwrap(), 256);
        for value in [0.0, -1.0, 1.5, f64::NAN, f64::INFINITY] {
            assert!(heap_mb(value).is_err());
        }
    }

    #[test]
    fn private_modes_are_exclusive_and_cef_takes_precedence() {
        let args = |items: &[&str]| {
            items
                .iter()
                .map(|&item| std::ffi::OsString::from(item))
                .collect::<Vec<_>>()
        };
        assert_eq!(private_mode(&args(&[ODT_FLAG])).unwrap(), Some(ODT_FLAG));
        assert!(private_mode(&args(&[ODT_FLAG, PDF_IMAGE_FLAG])).is_err());
        assert!(private_mode(&args(&[ODT_FLAG, PDF_SIGN_FLAG])).is_err());
        assert!(private_mode(&args(&[ODT_FLAG, FLAG])).is_err());
        assert_eq!(
            private_mode(&args(&[ODT_FLAG, PDF_SIGN_FLAG, "--type=renderer"])).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn inherited_address_space_limit_and_group_termination() {
        use std::io::Read;
        let mut command = Command::new("sh");
        command
            .args(["-c", "ulimit -v; exec sleep 30"])
            .stdout(Stdio::piped());
        let budget = std::num::NonZeroU64::new(4 * 1024 * 1024 * 1024).unwrap();
        let (mut child, mut guard) = platform::spawn(&mut command, Some(budget)).unwrap();
        let mut line = String::new();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
        let reported_kib: u64 = line.trim().parse().unwrap();
        assert!(reported_kib <= budget.get() / 1024);
        guard.terminate(&mut child).unwrap();
        assert!(!child.wait().unwrap().success());
        let mut remaining = String::new();
        stdout.read_to_string(&mut remaining).unwrap();
    }
}
