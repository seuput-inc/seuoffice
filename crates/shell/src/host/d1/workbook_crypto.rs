//! Workbook password flow over the shared Office encryption codec.

use main_core::packages::office_encryption::{OfficeDecryptReason, decrypt_office_file};
use shell_ipc_d1::sheets::engine::{UnlockFailure, Unlocked};

pub(super) fn unlock(
    bytes: &[u8],
    ask: &mut dyn FnMut(bool) -> Result<Option<String>, UnlockFailure>,
) -> Result<Option<Unlocked>, UnlockFailure> {
    const DEFAULT: &str = "VelvetSweatshop";
    match decrypt_office_file(bytes, DEFAULT) {
        Ok(bytes) => return Ok(Some(Unlocked { bytes, password: DEFAULT.into(), asked: false })),
        Err(error) if error.reason == OfficeDecryptReason::WrongPassword => {}
        Err(_) => return Err(UnlockFailure::Decrypt),
    }
    let mut retry = false;
    loop {
        let Some(password) = ask(retry)? else { return Ok(None); };
        if password.is_empty() { retry = true; continue; }
        match decrypt_office_file(bytes, &password) {
            Ok(bytes) => return Ok(Some(Unlocked { bytes, password, asked: true })),
            Err(error) if error.reason == OfficeDecryptReason::WrongPassword => retry = true,
            Err(_) => return Err(UnlockFailure::Decrypt),
        }
    }
}
