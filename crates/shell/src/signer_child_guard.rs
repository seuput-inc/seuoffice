//! The isolated Linux signer owns each helper's termination status.

fn validate_disposition(handler: usize, flags: libc::c_int) -> Result<(), String> {
    if handler != libc::SIG_DFL || flags & libc::SA_NOCLDWAIT != 0 {
        return Err("private signing worker requires owned child termination status".to_owned());
    }
    Ok(())
}

pub(crate) fn require_owned_children() -> Result<(), String> {
    let mut disposition = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: the null new-action pointer only queries SIGCHLD; disposition
    // receives one initialized sigaction on success. No handler is changed.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), disposition.as_mut_ptr()) } != 0 {
        return Err(format!(
            "querying signing worker child status: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful sigaction initialized the output structure.
    let disposition = unsafe { disposition.assume_init() };
    validate_disposition(disposition.sa_sigaction, disposition.sa_flags)
}

#[cfg(test)]
mod tests {
    use super::validate_disposition;

    #[test]
    fn rejects_automatic_or_foreign_child_reaping() {
        assert!(validate_disposition(libc::SIG_DFL, 0).is_ok());
        assert!(validate_disposition(libc::SIG_DFL, libc::SA_NOCLDSTOP).is_ok());
        assert!(validate_disposition(libc::SIG_IGN, 0).is_err());
        assert!(validate_disposition(libc::SIG_DFL, libc::SA_NOCLDWAIT).is_err());
        assert!(validate_disposition(libc::SIG_DFL.wrapping_add(2), 0).is_err());
    }
}
