//! The protected-authentication path of a hardware token. Only the NSS provider asks for it,
//! and NSS exists on Linux alone, which is not a shell target. macOS and Windows sign through
//! their certificate stores, whose worker never sends a token confirmation, so this answer is
//! a denial rather than a dialog.
use super::crypto::ProtectedTokenInteraction;
use shell_contract::{ipc::NativeRequestOwner, runtime::CancelToken};

pub(super) struct UnavailableTokenPath;

impl ProtectedTokenInteraction for UnavailableTokenPath {
    fn confirm(
        &self,
        _owner: &NativeRequestOwner,
        _request_cancel: &CancelToken,
        _entry_cancel: &CancelToken,
        _token_name: &str,
    ) -> Result<bool, String> {
        Err("hardware token authentication is not available on this platform".into())
    }
}
