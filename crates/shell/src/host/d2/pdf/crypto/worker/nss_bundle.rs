//! Authenticated, sealed snapshots of the installed NSS helper and its runtime.
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

mod manifest {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/nss_manifest.rs"));
}

const MANIFEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nss-manifest.json"));

struct Snapshot {
    target_fd: i32,
    file: File,
}

pub(super) struct VerifiedNssBundle {
    snapshots: Vec<Snapshot>,
}

fn snapshot(root: &Path, artifact: &manifest::Artifact) -> Result<Snapshot, String> {
    let path = root.join(&artifact.id);
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| format!("NSS artifact {}: {e}", artifact.id))?;
    let metadata = input
        .metadata()
        .map_err(|e| format!("NSS artifact {}: {e}", artifact.id))?;
    if !metadata.is_file() || metadata.len() != artifact.size {
        return Err(format!(
            "NSS artifact {}: invalid size or type",
            artifact.id
        ));
    }
    // SAFETY: the name is NUL terminated and the returned descriptor is uniquely owned.
    let raw = unsafe {
        libc::memfd_create(
            c"seuoffice-nss".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(format!("NSS snapshot: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: successful memfd_create returns a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(raw) };
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 32768];
    loop {
        let n = input
            .read(&mut buffer)
            .map_err(|e| format!("NSS artifact {}: {e}", artifact.id))?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("NSS artifact size overflow")?;
        if total > artifact.size {
            return Err(format!("NSS artifact {}: size changed", artifact.id));
        }
        digest.update(&buffer[..n]);
        file.write_all(&buffer[..n])
            .map_err(|e| format!("NSS snapshot: {e}"))?;
    }
    if total != artifact.size || format!("{:x}", digest.finalize()) != artifact.sha256 {
        return Err(format!("NSS artifact {}: digest mismatch", artifact.id));
    }
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: file is an owned memfd created with MFD_ALLOW_SEALING.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(format!(
            "NSS snapshot seals: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Keep all source descriptors outside the fixed child descriptor range so dup2 cannot overwrite them.
    // SAFETY: fcntl duplicates an open descriptor and returns a new owned descriptor.
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 300) };
    if duplicate < 0 {
        return Err(format!(
            "NSS snapshot descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returns a new owned descriptor.
    let file = unsafe { File::from_raw_fd(duplicate) };
    Ok(Snapshot {
        target_fd: artifact.fd,
        file,
    })
}

pub(super) fn resolve_nss_bundle() -> Result<VerifiedNssBundle, String> {
    let artifacts = manifest::parse(MANIFEST, env!("SEUOFFICE_NSS_TARGET"))?;
    let executable = std::env::current_exe().map_err(|e| format!("NSS executable path: {e}"))?;
    let root = executable
        .parent()
        .ok_or("NSS executable has no parent")?
        .join("../lib/SeuOffice/nss");
    let snapshots = artifacts
        .iter()
        .map(|artifact| snapshot(&root, artifact))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(VerifiedNssBundle { snapshots })
}

fn high_duplicate(file: &File) -> Result<File, String> {
    // SAFETY: file is open; fcntl returns a fresh owned descriptor on success.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 300) };
    if fd < 0 {
        return Err(format!(
            "NSS pipe descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful fcntl returned a fresh descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn pipe() -> Result<(File, File), String> {
    let mut fds = [-1; 2];
    // SAFETY: fds has room for both descriptors; O_CLOEXEC prevents accidental inheritance.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("NSS pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: successful pipe2 returned two distinct owned descriptors.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}
fn fork_exec(
    path: &std::ffi::CStr,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    mappings: &[(i32, i32)],
) -> Result<libc::pid_t, String> {
    let (mut error_read, original_error_write) = pipe()?;
    let error_write = high_duplicate(&original_error_write)?;
    drop(original_error_write);
    let error_fd = error_write.as_raw_fd();
    // An existing descriptor can exceed the current soft limit if the limit was lowered.
    let mut limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if limit < 0 {
        return Err("NSS descriptor limit unavailable".into());
    }
    for entry in
        std::fs::read_dir("/proc/self/fd").map_err(|e| format!("NSS descriptor inventory: {e}"))?
    {
        let entry = entry.map_err(|e| format!("NSS descriptor inventory: {e}"))?;
        let fd = entry
            .file_name()
            .to_string_lossy()
            .parse::<libc::c_long>()
            .map_err(|e| format!("NSS descriptor inventory: {e}"))?;
        limit = limit.max(fd + 1);
    }
    let limit = limit.min(i32::MAX as libc::c_long) as i32;
    let mut keep = [false; 300];
    for &(_, target) in mappings {
        if (3..300).contains(&target) {
            keep[target as usize] = true;
        }
    }
    // SAFETY: after fork the child uses only async-signal-safe syscalls and preallocated data.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("NSS fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            for &(source, target) in mappings {
                if libc::dup2(source, target) < 0 {
                    child_failure(error_fd);
                }
            }
            for fd in 3..300 {
                if !keep[fd as usize] {
                    libc::close(fd);
                }
            }
            let first = if error_fd > 300 {
                libc::syscall(libc::SYS_close_range, 300u32, (error_fd - 1) as u32, 0u32)
            } else {
                0
            };
            let last = libc::syscall(libc::SYS_close_range, (error_fd + 1) as u32, u32::MAX, 0u32);
            if first != 0 || last != 0 {
                for fd in 300..limit {
                    if fd != error_fd {
                        libc::close(fd);
                    }
                }
            }
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            child_failure(error_fd);
        }
    }
    drop(error_write);
    let mut bytes = [0u8; 4];
    let mut received = 0;
    loop {
        match error_read.read(&mut bytes[received..]) {
            Ok(0) => break,
            Ok(n) => {
                received += n;
                if received == bytes.len() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let diagnostic = format!("NSS exec status: {e}");
                return Err(match reap_before_owner(pid, true) {
                    Ok(()) => diagnostic,
                    Err(cleanup) => format!("{diagnostic}; {cleanup}"),
                });
            }
        }
    }
    if received != 0 {
        let diagnostic = if received == 4 {
            format!(
                "NSS exec: {}",
                std::io::Error::from_raw_os_error(i32::from_ne_bytes(bytes))
            )
        } else {
            "NSS exec status truncated".into()
        };
        return Err(match reap_before_owner(pid, false) {
            Ok(()) => diagnostic,
            Err(cleanup) => format!("{diagnostic}; {cleanup}"),
        });
    }
    Ok(pid)
}

fn retry_interrupted(mut operation: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    loop {
        match operation() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn reap_before_owner(pid: libc::pid_t, terminate: bool) -> Result<(), String> {
    if terminate {
        // SAFETY: pid is the unreaped child just created by fork.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("NSS cleanup kill: {error}"));
            }
        }
    }
    retry_interrupted(|| {
        let mut status = 0;
        // SAFETY: pid is the child to reap and status points to writable storage.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
    .map_err(|e| format!("NSS cleanup wait: {e}"))
}

unsafe fn child_failure(error_fd: i32) -> ! {
    // SAFETY: errno is thread-local; write and _exit are async-signal-safe.
    unsafe {
        let bytes = (*libc::__errno_location()).to_ne_bytes();
        loop {
            let written = libc::write(error_fd, bytes.as_ptr().cast(), bytes.len());
            if written >= 0 || *libc::__errno_location() != libc::EINTR {
                break;
            }
        }
        libc::_exit(127);
    }
}

pub(super) struct NssChild {
    pid: libc::pid_t,
    stdin: Option<File>,
    stdout: Option<File>,
    exit: Option<bool>,
    lost: bool,
}

impl NssChild {
    #[cfg(test)]
    pub(super) fn from_test_child(mut child: std::process::Child) -> Self {
        use std::os::fd::OwnedFd;
        let pid = child.id() as libc::pid_t;
        let stdin = child
            .stdin
            .take()
            .map(|pipe| File::from(OwnedFd::from(pipe)));
        let stdout = child
            .stdout
            .take()
            .map(|pipe| File::from(OwnedFd::from(pipe)));
        Self {
            pid,
            stdin,
            stdout,
            exit: None,
            lost: false,
        }
    }
    pub(super) fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }
    pub(super) fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }
    pub(super) fn wait(&mut self) -> Result<bool, String> {
        if let Some(success) = self.exit {
            return Ok(success);
        }
        if self.lost {
            return Err("NSS child was reaped elsewhere".into());
        }
        let mut status = 0;
        loop {
            // SAFETY: pid identifies this owned child and status is writable.
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid {
                let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                self.exit = Some(success);
                return Ok(success);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.lost = true;
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("NSS helper wait: {error}"));
            }
        }
    }
    pub(super) fn terminate_and_reap(&mut self) -> Result<(), String> {
        if self.exit.is_some() || self.lost {
            return Ok(());
        }
        // SAFETY: pid identifies our unreaped child; ESRCH still requires waitpid.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("NSS helper kill: {error}"));
            }
        }
        self.wait().map(|_| ())
    }
}
impl Drop for NssChild {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

impl VerifiedNssBundle {
    pub(super) fn spawn_helper(&self) -> Result<NssChild, String> {
        let (child_in, parent_in) = pipe()?;
        let (parent_out, child_out) = pipe()?;
        #[cfg(not(test))]
        let stderr_path = "/dev/null";
        #[cfg(test)]
        let stderr_path = if env!("CARGO_PKG_NAME") == "nss-launcher-fixture" {
            "/proc/self/fd/2"
        } else {
            "/dev/null"
        };
        let null = OpenOptions::new()
            .write(true)
            .open(stderr_path)
            .map_err(|e| format!("NSS stderr: {e}"))?;
        let child_in = high_duplicate(&child_in)?;
        let child_out = high_duplicate(&child_out)?;
        let null = high_duplicate(&null)?;
        let mut mappings: Vec<(i32, i32)> = self
            .snapshots
            .iter()
            .map(|snapshot| (snapshot.file.as_raw_fd(), snapshot.target_fd))
            .collect();
        mappings.extend([
            (child_in.as_raw_fd(), 0),
            (child_out.as_raw_fd(), 1),
            (null.as_raw_fd(), 2),
        ]);
        let args = [
            c"/proc/self/fd/200",
            c"--inhibit-cache",
            c"--glibc-hwcaps-mask",
            c"",
            c"/proc/self/fd/201",
        ];
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut environment = Vec::new();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("LD_")
                || name.starts_with("DYLD_")
                || name == "GLIBC_TUNABLES"
                || name == "GCONV_PATH"
                || name == "LOCPATH"
            {
                continue;
            }
            let mut bytes = key.as_os_str().as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_os_str().as_bytes());
            environment.push(CString::new(bytes).map_err(|_| "invalid NSS helper environment")?);
        }
        let mut envp: Vec<*const libc::c_char> = environment.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());
        let pid = fork_exec(c"/proc/self/fd/200", &argv, &envp, &mappings)?;
        Ok(NssChild {
            pid,
            stdin: Some(parent_in),
            stdout: Some(parent_out),
            exit: None,
            lost: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_reap_retries_and_unknown_status_is_not_success() {
        let mut calls = 0;
        retry_interrupted(|| {
            calls += 1;
            if calls == 1 {
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(
            retry_interrupted(|| Err(std::io::Error::from_raw_os_error(libc::ECHILD))).is_err()
        );
    }

    #[test]
    fn child_fd_actions_leave_occupied_parent_fd_unchanged_across_concurrent_spawns() {
        let original = File::open("/dev/null").unwrap();
        // SAFETY: fcntl duplicates a valid descriptor into the requested range.
        let occupied = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
        assert!(occupied >= 200);
        // SAFETY: the duplicate is newly owned by this test.
        let occupied = unsafe { File::from_raw_fd(occupied) };
        let target = occupied.as_raw_fd();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let replacement = File::open("/dev/null").unwrap();
                    let replacement = high_duplicate(&replacement).unwrap();
                    let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
                    let envp = [std::ptr::null()];
                    let pid = fork_exec(
                        c"/bin/true",
                        &argv,
                        &envp,
                        &[(replacement.as_raw_fd(), target)],
                    )
                    .unwrap();
                    let mut child = NssChild {
                        pid,
                        stdin: None,
                        stdout: None,
                        exit: None,
                        lost: false,
                    };
                    assert!(child.wait().unwrap());
                    assert!(child.wait().unwrap());
                });
            }
        });
        // SAFETY: occupied remains owned and open for the duration of the test.
        assert_ne!(unsafe { libc::fcntl(target, libc::F_GETFD) }, -1);
    }

    #[test]
    fn unrelated_non_cloexec_descriptor_is_not_inherited() {
        let original = File::open("/dev/null").unwrap();
        // SAFETY: fcntl creates a fresh descriptor without CLOEXEC.
        let leaked = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD, 700) };
        assert!(leaked >= 700);
        // SAFETY: the descriptor is newly owned by the test.
        let leaked = unsafe { File::from_raw_fd(leaked) };
        let command =
            CString::new(format!("test ! -e /proc/self/fd/{}", leaked.as_raw_fd())).unwrap();
        let argv = [
            c"/bin/sh".as_ptr(),
            c"-c".as_ptr(),
            command.as_ptr(),
            std::ptr::null(),
        ];
        let envp = [std::ptr::null()];
        let pid = fork_exec(c"/bin/sh", &argv, &envp, &[]).unwrap();
        let mut child = NssChild {
            pid,
            stdin: None,
            stdout: None,
            exit: None,
            lost: false,
        };
        assert!(child.wait().unwrap());
    }
}
