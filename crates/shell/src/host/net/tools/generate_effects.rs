//! Generation carries its captured origin, not a lookup of the active view.
//! Producer-specific menu/window-event binding is checked by GenerateWindows.

use crate::ipc::native_owner::{self, NativeOwnerAuthority};
use shell_contract::ipc::NativeRequestOwner;
use shell_ipc_a::generate::host::{GenerateAuthority, GenerateOrigin};

pub(super) const LOST: &str = "The generating document is no longer available.";

pub(super) fn owner(authority: &GenerateAuthority) -> Result<NativeRequestOwner, String> {
    authority.check_cancelled()?;
    if authority.is_closed_event() {
        return Err("Closed-window authority is cleanup-only.".into());
    }
    let native = authority
        .native_context()
        .ok_or("Generation requires a captured native origin.")?;
    let owner = native_owner::authenticated_owner(native)
        .ok_or("Generation native origin is not authenticated.")?;
    if let GenerateOrigin::Renderer { sender, .. } = &authority.origin
        && sender.view != owner.view
    {
        return Err("Generation renderer and native origin do not match.".into());
    }
    native_owner::with_owner_effect(&owner, &authority.cancel, || ())
        .ok_or_else(|| LOST.to_owned())?;
    Ok(owner)
}

/// Only bounded in-memory publication belongs under this gate. Constructors,
/// byte IO, engines and callbacks must run after it has dropped.
pub(super) fn commit<T>(
    authority: &GenerateAuthority,
    effect: impl FnOnce() -> T,
) -> Result<T, String> {
    let owner = owner(authority)?;
    native_owner::with_owner_effect(&owner, &authority.cancel, effect)
        .ok_or_else(|| LOST.to_owned())
}

pub(super) fn compute<T>(
    authority: &GenerateAuthority,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let owner = owner(authority)?;
    let work = native_owner::with_owner_effect(&owner, &authority.cancel, || work)
        .ok_or_else(|| LOST.to_owned())?;
    let value = work()?;
    self::owner(authority)?;
    Ok(value)
}

pub(super) fn same_request(left: &GenerateAuthority, right: &GenerateAuthority) -> bool {
    if !left.cancel.same_token(&right.cancel) {
        return false;
    }
    match (
        left.native_context()
            .and_then(|native| native.authority_ref::<NativeOwnerAuthority>()),
        right
            .native_context()
            .and_then(|native| native.authority_ref::<NativeOwnerAuthority>()),
    ) {
        (Some(left), Some(right)) => std::ptr::eq(left, right),
        _ => false,
    }
}

/// Synchronous tab metadata methods are core-only; they never rendezvous with
/// another thread or wait for a renderer response.
pub(super) fn on_core<T>(
    work: impl FnOnce(&shell_contract::Provided) -> Result<T, String>,
) -> Result<T, String> {
    let mut result = None;
    if !crate::host::w::on_core_local(|provided| result = Some(work(provided))) {
        return Err("Generation metadata access requires the core thread.".into());
    }
    result.ok_or_else(|| "Generation core access did not execute.".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::ipc::{NativeContext, Sender};
    use shell_contract::runtime::CancelToken;
    use shell_contract::{Surface, ViewId, WindowId};

    fn renderer(native: Option<NativeContext>) -> GenerateAuthority {
        GenerateAuthority {
            origin: GenerateOrigin::Renderer {
                sender: Sender {
                    view: ViewId(1),
                    window: WindowId(2),
                    surface: Surface::ShellGenerate,
                },
                native,
            },
            cancel: CancelToken::default(),
        }
    }
    #[test]
    fn native_less_and_fabricated_contexts_never_run_effects() {
        for authority in [renderer(None), renderer(Some(NativeContext::new(())))] {
            assert!(commit(&authority, || panic!("unauthenticated effect")).is_err());
            assert!(compute::<()>(&authority, || panic!("unauthenticated IO")).is_err());
        }
    }
    #[test]
    fn cancellation_is_not_replaced_by_a_new_request_token() {
        let authority = renderer(None);
        authority.cancel.cancel();
        assert!(owner(&authority).is_err());
        assert!(!same_request(&authority, &renderer(None)));
    }
}
