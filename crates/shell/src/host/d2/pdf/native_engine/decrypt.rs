//! Password-aware PDF decryption and preservation of source protection.
use super::super::pdf_codec;
use lopdf::{
    Document, Object, StringFormat,
    encryption::{DecryptionError, EncryptionState},
};
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::editor::hosts::{DecryptError, Decrypted};

mod raw;

/// Native-only security material. It stays inside the save invocation.
pub(super) struct Protection {
    state: EncryptionState,
    file_id: Option<[Vec<u8>; 2]>,
    dictionary: Vec<u8>,
}
impl Protection {
    pub(super) fn encrypt_metadata(&self) -> bool {
        self.state.encrypt_metadata()
    }
    pub(super) fn dictionary(&self) -> &[u8] {
        &self.dictionary
    }
    pub(super) fn file_id(&self) -> Option<&[Vec<u8>; 2]> {
        self.file_id.as_ref()
    }
}
pub(super) struct Authenticated {
    pub protection: Protection,
    pub permissions: IpcValue,
    pub needed_password: bool,
    pub unlocked_as: &'static str,
}
#[derive(Clone, Copy)]
pub(super) enum PayloadKind {
    String,
    Stream,
}
/// Apply only one payload's cipher. The document codec owns exclusions and traversal.
pub(super) fn transform_payload(
    protection: &Protection,
    number: f64,
    generation: f64,
    kind: PayloadKind,
    bytes: &[u8],
    encrypt: bool,
) -> Result<Vec<u8>, String> {
    if bytes.is_empty() && (!encrypt || protection.state.revision() <= 4) {
        return Ok(Vec::new());
    }
    let id = (
        jscompat::to_int32(number) as u32,
        jscompat::to_int32(generation) as u16,
    );
    let mut object = match kind {
        PayloadKind::String => Object::String(bytes.to_vec(), StringFormat::Hexadecimal),
        PayloadKind::Stream => {
            Object::Stream(lopdf::Stream::new(lopdf::Dictionary::new(), bytes.to_vec()))
        }
    };
    if encrypt {
        lopdf::encryption::encrypt_object(&protection.state, id, &mut object)
    } else {
        lopdf::encryption::decrypt_object(&protection.state, id, &mut object)
    }
    .map_err(|e| e.to_string())?;
    match object {
        Object::String(bytes, _) => Ok(bytes),
        Object::Stream(stream) => Ok(stream.content),
        _ => Err("Invalid PDF payload type.".into()),
    }
}
fn permissions(bits: i32) -> IpcValue {
    IpcValue::Object(vec![
        ("printing".into(), IpcValue::Bool(bits & (0x4 | 0x800) != 0)),
        (
            "modifying".into(),
            IpcValue::Bool(bits & (0x8 | 0x400) != 0),
        ),
        (
            "annotating".into(),
            IpcValue::Bool(bits & (0x20 | 0x100) != 0),
        ),
        ("copying".into(), IpcValue::Bool(bits & 0x10 != 0)),
    ])
}
fn password_bytes(password: &str) -> &[u8] {
    let bytes = password.as_bytes();
    let nul = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    &bytes[..nul.min(127)]
}
fn wrong(error: &lopdf::Error) -> bool {
    matches!(
        error,
        lopdf::Error::InvalidPassword
            | lopdf::Error::Decryption(DecryptionError::IncorrectPassword)
    )
}
fn other(error: impl std::fmt::Display) -> DecryptError {
    DecryptError::Other(error.to_string())
}
#[cfg(test)]
fn plain_pdf(doc: &mut Document) -> Result<Vec<u8>, String> {
    doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}
/// The public decrypt IPC exposes no saved security material.
pub(super) fn decrypt_document(bytes: &[u8], password: &str) -> Result<Decrypted, DecryptError> {
    decrypt_with_protection(bytes, password).map(|(document, _)| document)
}
/// Authenticate the first raw security dictionary without parsing document content.
pub(super) fn authenticate(
    bytes: &[u8],
    password: &str,
) -> Result<Option<Authenticated>, DecryptError> {
    let Some(raw_dict) = raw::encrypt_dictionary(bytes) else {
        return Ok(None);
    };
    if raw::certificate_encrypted(raw_dict).map_err(other)? {
        return Err(DecryptError::CertificateEncryption);
    }
    let bits = raw::permissions(raw_dict).map_err(other)?;
    let (dictionary, normalized) = raw::normalize(raw_dict).map_err(other)?;
    let security = Document::pdf_crypt_security(&dictionary, &raw::first_id(bytes));
    let pw = password_bytes(password);
    let owner = match security.authenticate_pdf_crypt_owner(pw) {
        Ok(()) => true,
        Err(error) if wrong(&error) => false,
        Err(error) => return Err(other(error)),
    };
    if !owner {
        match security.authenticate_pdf_crypt_user(pw) {
            Ok(()) => {}
            Err(error) if wrong(&error) => return Err(DecryptError::WrongPassword),
            Err(error) => return Err(other(error)),
        }
    }
    let state = Document::pdf_crypt_state(&security, pw, owner).map_err(other)?;
    let protection = Protection {
        state,
        file_id: raw::file_id(bytes),
        dictionary: normalized,
    };
    Ok(Some(Authenticated {
        protection,
        permissions: permissions(bits),
        needed_password: !password.is_empty(),
        unlocked_as: if owner { "owner" } else { "user" },
    }))
}
/// Carry the validated source protection only until the current save finishes.
pub(super) fn decrypt_with_protection(
    bytes: &[u8],
    password: &str,
) -> Result<(Decrypted, Option<Protection>), DecryptError> {
    let Some(auth) = authenticate(bytes, password)? else {
        return Ok((
            Decrypted {
                bytes: bytes.to_vec(),
                permissions: permissions(-1),
                needed_password: false,
                unlocked_as: "user".into(),
            },
            None,
        ));
    };
    let plain = pdf_codec::decrypt(
        bytes,
        auth.protection.encrypt_metadata(),
        |number, generation, kind, payload| {
            transform_payload(
                &auth.protection,
                number,
                generation,
                codec_kind(kind),
                payload,
                false,
            )
        },
    )
    .map_err(other)?;
    let decrypted = Decrypted {
        bytes: plain,
        permissions: auth.permissions,
        needed_password: auth.needed_password,
        unlocked_as: auth.unlocked_as.into(),
    };
    Ok((decrypted, Some(auth.protection)))
}
fn codec_kind(kind: pdf_codec::PayloadKind) -> PayloadKind {
    match kind {
        pdf_codec::PayloadKind::String => PayloadKind::String,
        pdf_codec::PayloadKind::Stream => PayloadKind::Stream,
    }
}
/// Encrypt an edited plain PDF using the validated source file key and entries.
pub(super) fn reprotect(bytes: &[u8], protection: &Protection) -> Result<Vec<u8>, String> {
    pdf_codec::reprotect(
        bytes,
        protection.dictionary(),
        protection.file_id(),
        protection.encrypt_metadata(),
        |number, generation, kind, payload| {
            transform_payload(
                protection,
                number,
                generation,
                codec_kind(kind),
                payload,
                true,
            )
        },
    )
}
/// Apply a new R6 protection after all document edits have finished.
pub(super) fn encrypt_fresh(
    bytes: &[u8],
    user_password: &str,
    owner_password: Option<&str>,
    permissions: &IpcValue,
) -> Result<Vec<u8>, String> {
    fn enabled(permissions: &IpcValue, name: &str) -> bool {
        let IpcValue::Object(fields) = permissions else {
            return false;
        };
        let Some((_, value)) = fields.iter().find(|(key, _)| key == name) else {
            return false;
        };
        match value {
            IpcValue::Undefined | IpcValue::Null => false,
            IpcValue::Bool(value) => *value,
            IpcValue::Number(value) => *value != 0.0 && !value.is_nan(),
            IpcValue::String(value) => !value.is_empty(),
            _ => true,
        }
    }
    let mut bits: i32 = -1;
    for (name, mask) in [
        ("printing", 0x804),
        ("modifying", 0x408),
        ("annotating", 0x120),
        ("copying", 0x10),
    ] {
        if !enabled(permissions, name) {
            bits &= !mask
        }
    }
    let mut key = [0u8; 32];
    openssl::rand::rand_bytes(&mut key).map_err(|e| e.to_string())?;
    let owner = owner_password
        .filter(|value| !value.is_empty())
        .unwrap_or(user_password);
    let state = EncryptionState::pdf_crypt_new_r6(
        &key,
        password_bytes(user_password),
        password_bytes(owner),
        lopdf::Permissions::from_bits_retain((bits as i64) as u64),
    )
    .map_err(|e| e.to_string())?;
    let mut dictionary = state.encode().map_err(|e| e.to_string())?;
    dictionary.remove(b"EncryptMetadata");
    let mut filter = lopdf::Dictionary::new();
    filter.set("CFM", Object::Name(b"AESV3".to_vec()));
    filter.set("AuthEvent", Object::Name(b"DocOpen".to_vec()));
    filter.set("Length", Object::Integer(32));
    let mut filters = lopdf::Dictionary::new();
    filters.set("StdCF", Object::Dictionary(filter));
    dictionary.set("CF", Object::Dictionary(filters));
    dictionary.set("Length", Object::Integer(256));
    let protection = Protection {
        state,
        file_id: None,
        dictionary: raw::serialize_dictionary(&dictionary)?,
    };
    reprotect(bytes, &protection)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_bytes_keep_permissions_and_do_not_require_password() {
        let data = b"not parsed because unencrypted";
        let (out, protection) = decrypt_with_protection(data, "secret").unwrap();
        assert_eq!(out.bytes, data);
        assert!(!out.needed_password);
        assert_eq!(out.unlocked_as, "user");
        assert!(protection.is_none());
        assert_eq!(out.permissions, permissions(-1));
    }
    #[test]
    fn security_password_is_truncated_at_nul_and_127_bytes() {
        assert_eq!(password_bytes("abc\0ignored"), b"abc");
        assert_eq!(password_bytes(&"x".repeat(150)).len(), 127);
    }
    #[test]
    fn permission_bits_allow_high_quality_print_or_assembly() {
        assert_eq!(
            permissions(0x800 | 0x400),
            IpcValue::Object(vec![
                ("printing".into(), IpcValue::Bool(true)),
                ("modifying".into(), IpcValue::Bool(true)),
                ("annotating".into(), IpcValue::Bool(false)),
                ("copying".into(), IpcValue::Bool(false))
            ])
        );
    }
    fn protected_fixture(user: &str, owner: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.4");
        doc.trailer.set(
            "ID",
            Object::Array(vec![
                Object::String(vec![1; 16], StringFormat::Hexadecimal),
                Object::String(vec![1; 16], StringFormat::Hexadecimal),
            ]),
        );
        let pages = doc.add_object(
            lopdf::dictionary! {"Type"=>"Pages","Count"=>0,"Kids"=>Vec::<Object>::new()},
        );
        let root=doc.add_object(lopdf::dictionary!{"Type"=>"Catalog","Pages"=>pages,"Label"=>Object::string_literal("content")});
        doc.trailer.set("Root", root);
        let state = EncryptionState::try_from(lopdf::encryption::EncryptionVersion::V1 {
            document: &doc,
            owner_password: owner,
            user_password: user,
            permissions: lopdf::Permissions::all(),
        })
        .unwrap();
        doc.encrypt_checked(&state).unwrap();
        plain_pdf(&mut doc).unwrap()
    }
    #[test]
    fn owner_is_distinct_from_empty_user_and_roundtrips_original_protection() {
        let bytes = protected_fixture("", "owner");
        let (owner, protection) = decrypt_with_protection(&bytes, "owner").unwrap();
        assert_eq!(owner.unlocked_as, "owner");
        assert!(owner.needed_password);
        let (user, _) = decrypt_with_protection(&bytes, "").unwrap();
        assert_eq!(user.unlocked_as, "user");
        assert!(!user.needed_password);
        assert!(matches!(
            decrypt_document(&bytes, "incorrect"),
            Err(DecryptError::WrongPassword)
        ));
        let encrypted = reprotect(&owner.bytes, &protection.unwrap()).unwrap();
        assert_eq!(
            decrypt_document(&encrypted, "owner").unwrap().unlocked_as,
            "owner"
        );
        assert_eq!(
            decrypt_document(&encrypted, "").unwrap().unlocked_as,
            "user"
        );
    }
    #[test]
    fn same_password_is_classified_as_owner() {
        let bytes = protected_fixture("same", "same");
        assert_eq!(
            decrypt_document(&bytes, "same").unwrap().unlocked_as,
            "owner"
        );
    }
    #[test]
    fn fresh_r6_protection_accepts_both_passwords_and_permissions() {
        let input =
            b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF";
        let limits = IpcValue::Object(vec![("printing".into(), IpcValue::Bool(true))]);
        let sealed = encrypt_fresh(input, "user", Some("owner"), &limits).unwrap();
        let user = decrypt_document(&sealed, "user").unwrap();
        assert_eq!(user.unlocked_as, "user");
        assert_eq!(user.permissions, permissions(-1 & !0x408 & !0x120 & !0x10));
        assert_eq!(
            decrypt_document(&sealed, "owner").unwrap().unlocked_as,
            "owner"
        );
        assert!(matches!(
            decrypt_document(&sealed, "bad"),
            Err(DecryptError::WrongPassword)
        ));
    }
}
