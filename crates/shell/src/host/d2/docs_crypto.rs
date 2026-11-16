//! Office encryption boundary shared with the native Office codec.

use main_core::packages::office_encryption::{self, OfficeDecryptReason};
use shell_ipc_d2::docs::hosts::DecryptError;

pub(super) fn decrypt(bytes: &[u8], password: &str) -> Result<Vec<u8>, DecryptError> {
    office_encryption::decrypt_office_file(bytes, password).map_err(|error| DecryptError {
        reason: match error.reason {
            OfficeDecryptReason::WrongPassword => "wrong-password",
            OfficeDecryptReason::Unsupported => "unsupported",
        },
        message: error.message,
    })
}

pub(super) fn encrypt(bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
    office_encryption::encrypt_ooxml(bytes, password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_document_round_trips_and_wrong_password_is_typed() {
        let plain = b"PK\x03\x04synthetic document bytes";
        let encrypted = encrypt(plain, "correct horse").unwrap();
        assert_eq!(decrypt(&encrypted, "correct horse").unwrap(), plain);
        assert_eq!(
            decrypt(&encrypted, "wrong").unwrap_err().reason,
            "wrong-password"
        );
    }
}
