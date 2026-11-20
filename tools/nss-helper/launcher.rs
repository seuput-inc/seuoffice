//! Standalone hosted fixture including the production resolver and launcher unchanged.
#[path = "nss_bundle.rs"]
mod nss_bundle;
#[cfg(test)]
#[path = "nss_exchange.rs"]
mod nss_exchange;
mod signer_child_guard;

fn disposition_probe(no_wait: bool) -> Result<bool, String> {
    // This mode is executed only in a dedicated fixture subprocess.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = if no_wait { libc::SIG_DFL } else { libc::SIG_IGN };
    action.sa_flags = if no_wait { libc::SA_NOCLDWAIT } else { 0 };
    // SAFETY: action is initialized and this process exits after the guard check.
    unsafe { libc::sigemptyset(&mut action.sa_mask); }
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(signer_child_guard::require_owned_children().is_err())
}

fn run() -> Result<bool, String> {
    match std::env::args().nth(1).as_deref() {
        Some("--test-sigchld-ignore") => return disposition_probe(false),
        Some("--test-sigchld-no-wait") => return disposition_probe(true),
        Some(_) => return Err("unknown fixture argument".into()),
        None => {}
    }
    signer_child_guard::require_owned_children()?;
    let bundle = nss_bundle::resolve_nss_bundle()?;
    let mut child = bundle.spawn_helper()?;
    let mut input = child.take_stdin().ok_or("missing helper stdin")?;
    let mut output = child.take_stdout().ok_or("missing helper stdout")?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut input);
    });
    std::io::copy(&mut output, &mut std::io::stdout().lock()).map_err(|e| e.to_string())?;
    child.wait()
}
fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => { eprintln!("{error}"); std::process::exit(2); }
    }
}
