//! Office password protection detection and byte-level codec boundary.
mod agile;

use crate::hwp::cfb::{self, Container};
use crate::packages::compound_file;
use std::io::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeDecryptReason {
    WrongPassword,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficeDecryptError {
    pub reason: OfficeDecryptReason,
    pub message: String,
}

impl std::fmt::Display for OfficeDecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OfficeDecryptError {}

fn unsupported(message: impl Into<String>) -> OfficeDecryptError {
    OfficeDecryptError {
        reason: OfficeDecryptReason::Unsupported,
        message: message.into(),
    }
}

fn wrong_password() -> OfficeDecryptError {
    OfficeDecryptError {
        reason: OfficeDecryptReason::WrongPassword,
        message: "The password is incorrect".into(),
    }
}

const ENCRYPTED_PACKAGE: &[u8] = b"E\0n\0c\0r\0y\0p\0t\0e\0d\0P\0a\0c\0k\0a\0g\0e\0";

pub fn is_cfb_header(bytes: &[u8]) -> bool {
    compound_file::is_cfb_header(bytes)
}

pub fn is_encrypted_ooxml(bytes: &[u8]) -> bool {
    is_cfb_header(bytes)
        && bytes
            .windows(ENCRYPTED_PACKAGE.len())
            .any(|w| w == ENCRYPTED_PACKAGE)
}

fn content<'a>(container: &'a Container, name: &str) -> Option<&'a [u8]> {
    let entry = container.file_index.get(cfb::find(container, name)?)?;
    entry.content.as_deref()
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn legacy_is_encrypted(container: &Container) -> Option<bool> {
    if cfb::find(container, "/EncryptionInfo").is_some() {
        return Some(true);
    }
    if let Some(workbook) = content(container, "Workbook").or_else(|| content(container, "Book")) {
        let mut offset = usize::from(u16_at(workbook, 2)?).checked_add(4)?;
        let mut record = u16_at(workbook, offset)?;
        if record == 134 {
            offset = offset.checked_add(4)?;
            record = u16_at(workbook, offset)?;
        }
        if record == 47 {
            return Some(true);
        }
    }
    if let Some(word) = content(container, "WordDocument") {
        if u16_at(word, 10)? & 0x0100 != 0 {
            return Some(true);
        }
    }
    if let Some(presentation) = content(container, "PowerPoint Document") {
        let current = content(container, "Current User")?;
        let offset = usize::try_from(u32_at(current, 16)?).ok()?.checked_add(4)?;
        if u32_at(presentation, offset)? == 0x20 {
            return Some(true);
        }
    }
    Some(false)
}

pub fn is_encrypted_office_file(bytes: &[u8]) -> bool {
    if !is_cfb_header(bytes) || compound_file::assert_compound_file(bytes).is_err() {
        return false;
    }
    if is_encrypted_ooxml(bytes) {
        return true;
    }
    cfb::read(bytes)
        .ok()
        .and_then(|file| legacy_is_encrypted(&file))
        .unwrap_or(false)
}

pub fn decrypt_office_file(bytes: &[u8], password: &str) -> Result<Vec<u8>, OfficeDecryptError> {
    if is_cfb_header(bytes) {
        compound_file::assert_compound_file(bytes)
            .map_err(|error| unsupported(format!("{error:?}")))?;
    }
    if password.is_empty() {
        return Err(unsupported("options.password is required"));
    }
    let container = cfb::read(bytes).map_err(|error| unsupported(error.0))?;
    let info = content(&container, "/EncryptionInfo");
    if let Some(info) = info {
        let package = content(&container, "/EncryptedPackage")
            .ok_or_else(|| unsupported("Missing EncryptedPackage"))?;
        return match u16_at(info, 2) {
            Some(4) => agile::decrypt(info, package, password),
            Some(2) => agile::decrypt_standard(info, package, password),
            Some(3) => Err(unsupported("Unsupported encryption algorithms")),
            _ => Err(unsupported("ECMA-376 Encrypted file unrecognized Version")),
        };
    }
    match legacy_is_encrypted(&container) {
        Some(false) => return Ok(bytes.to_vec()),
        None => return Err(unsupported("Unreadable Office encryption header")),
        Some(true) => {}
    }
    msoffice_crypto::decrypt_binary_office(bytes, password).or_else(|error| match error {
        msoffice_crypto::Error::WrongPassword => Err(wrong_password()),
        msoffice_crypto::Error::NotEncrypted => Ok(bytes.to_vec()),
        other => Err(unsupported(other.to_string())),
    })
}

pub fn encrypt_ooxml(bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
    if password.is_empty() {
        return Err("options.password is required".into());
    }
    if password.encode_utf16().count() > 255 {
        return Err("The maximum password length is 255".into());
    }
    if u32::try_from(bytes.len()).is_err() {
        return Err("Office package length exceeds uint32".into());
    }
    let mut output = Cursor::new(Vec::new());
    msoffice_crypto::encrypt_application_bytes_to(Cursor::new(bytes), &mut output, password)
        .map_err(|error| error.to_string())?;
    Ok(output.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agile_round_trip_and_password_error() {
        let plain = b"PK\x03\x04synthetic package";
        let encrypted = encrypt_ooxml(plain, "test password").unwrap();
        assert!(is_encrypted_ooxml(&encrypted));
        assert!(is_encrypted_office_file(&encrypted));
        assert_eq!(
            decrypt_office_file(&encrypted, "test password").unwrap(),
            &plain[..]
        );
        assert_eq!(
            decrypt_office_file(&encrypted, "incorrect")
                .unwrap_err()
                .reason,
            OfficeDecryptReason::WrongPassword,
        );
    }

    #[test]
    fn decrypts_upstream_agile_fixture() {
        let encrypted = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vendor/msoffice-crypto/tests/fixtures/agile_encrypted.docx"
        ));
        let plain = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vendor/msoffice-crypto/tests/fixtures/plain.docx"
        ));
        assert_eq!(
            decrypt_office_file(encrypted, "testpass").unwrap(),
            &plain[..]
        );
    }

    #[test]
    fn standard_encryption_round_trip_and_wrong_password() {
        let plain = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vendor/msoffice-crypto/tests/fixtures/plain.docx"
        ));
        let encrypted =
            msoffice_crypto::encrypt_ooxml_standard(plain, "standard password").unwrap();
        assert_eq!(
            decrypt_office_file(&encrypted, "standard password").unwrap(),
            &plain[..]
        );
        assert_eq!(
            decrypt_office_file(&encrypted, "wrong").unwrap_err().reason,
            OfficeDecryptReason::WrongPassword,
        );
    }

    #[test]
    fn validates_passwords_and_damaged_containers() {
        assert!(encrypt_ooxml(b"", "").is_err());
        assert!(encrypt_ooxml(b"", &"😀".repeat(128)).is_err());
        let damaged = compound_file::OLE2_MAGIC;
        assert!(is_cfb_header(&damaged));
        assert!(!is_encrypted_office_file(&damaged));
        assert_eq!(
            decrypt_office_file(&damaged, "password")
                .unwrap_err()
                .reason,
            OfficeDecryptReason::Unsupported,
        );
    }

    #[test]
    fn agile_rejects_decrypted_non_zip_as_wrong_password() {
        let encrypted = encrypt_ooxml(b"not a ZIP", "password").unwrap();
        assert_eq!(
            decrypt_office_file(&encrypted, "password")
                .unwrap_err()
                .reason,
            OfficeDecryptReason::WrongPassword,
        );
    }
}
