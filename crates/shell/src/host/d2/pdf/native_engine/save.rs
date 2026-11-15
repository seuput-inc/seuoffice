//! Ordered PDF edit, validation, protection and atomic save orchestration.

use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::editor::hosts::{DecryptError, SaveSkips};
use super::{annotation_choice, annotation_delete, blank_pages, decrypt, document_edits, image_edits, redaction, save_policy, save_verify, text_edits};

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value { IpcValue::Object(entries) => entries.iter().find(|(key, _)| key == name).map(|(_, value)| value), _ => None }
}
fn list(value: Option<&IpcValue>) -> &[IpcValue] {
    match value { Some(IpcValue::Array(values)) => values, _ => &[] }
}
fn text(value: Option<&IpcValue>) -> Option<&str> {
    match value { Some(IpcValue::String(value)) => Some(value), _ => None }
}
fn cancelled(cancel: &CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() { Err("PDF save cancelled".into()) } else { Ok(()) }
}
fn decrypt_error(error: DecryptError) -> String {
    match error { DecryptError::WrongPassword => "wrong password".into(), DecryptError::CertificateEncryption => "certificate encryption".into(), DecryptError::Other(message) => message }
}
fn ordinary(request: &IpcValue) -> IpcValue {
    match request {
        IpcValue::Object(entries) => IpcValue::Object(entries.iter().filter(|(key, _)| !matches!(key.as_str(), "redactions" | "annotations" | "copyOnly")).cloned().collect()),
        value => value.clone(),
    }
}

fn edited(source: &[u8], request: &IpcValue, cancel: &CancelToken) -> Result<(Vec<u8>, SaveSkips), String> {
    cancelled(cancel)?;
    let password = text(field(request, "decryption").and_then(|value| field(value, "userPassword")))
        .or_else(|| text(field(request, "encryption").and_then(|value| field(value, "userPassword"))));
    let (mut bytes, protection) = if let Some(password) = password {
        let (document, protection) = decrypt::decrypt_with_protection(source, password).map_err(decrypt_error)?;
        (document.bytes, protection)
    } else { (source.to_vec(), None) };
    cancelled(cancel)?;
    let deletes = list(field(request, "annotDeletes"));
    if !deletes.is_empty() { bytes = annotation_delete::apply(&bytes, deletes, cancel)?; }
    let blanks = list(field(request, "blankPages"));
    if !blanks.is_empty() { bytes = blank_pages::append(&bytes, blanks, cancel)?; }
    let mut skips = SaveSkips::default();
    let edits = list(field(request, "textEdits"));
    if !edits.is_empty() { (bytes, skips.text_edits) = text_edits::apply_text_edits(&bytes, edits, cancel)?; }
    let inserts = list(field(request, "textInserts"));
    if !inserts.is_empty() { (bytes, skips.text_inserts) = text_edits::apply_text_inserts(&bytes, inserts, cancel)?; }
    let images = list(field(request, "imageEdits"));
    if !images.is_empty() { (bytes, skips.image_edits) = image_edits::apply(&bytes, images, cancel)?; }
    let now = main_core::packages::pdf_structure::files::system_now_ms();
    bytes = document_edits::apply_document_edits(&bytes, request, now, cancel)?;
    let regions = list(field(request, "redactions"));
    if !regions.is_empty() { bytes = redaction::apply(&bytes, regions, cancel)?; }
    cancelled(cancel)?;
    let preserved = if matches!(field(request, "protectionDirty"), Some(IpcValue::Bool(false))) { protection } else { None };
    if let Some(protection) = preserved {
        bytes = decrypt::reprotect(&bytes, &protection)?;
    } else if let Some(encryption) = field(request, "encryption").filter(|value| !matches!(value, IpcValue::Undefined | IpcValue::Null)) {
        let user = text(field(encryption, "userPassword")).ok_or("invalid PDF user password")?;
        let owner = text(field(encryption, "ownerPassword"));
        let permissions = field(encryption, "permissions").ok_or("invalid PDF permissions")?;
        bytes = decrypt::encrypt_fresh(&bytes, user, owner, permissions)?;
    }
    cancelled(cancel)?;
    Ok((bytes, skips))
}

fn verify(bytes: &[u8], request: &IpcValue, skips: &SaveSkips, cancel: &CancelToken) -> Result<(), String> {
    let planned = save_verify::verification(request, skips);
    let mut failures = Vec::new();
    if !planned.texts.is_empty() { failures.extend(text_edits::verify_text_edits(bytes, &planned.texts, planned.password.as_deref(), cancel)?); }
    if !planned.images.is_empty() { failures.extend(image_edits::verify(bytes, &planned.images, planned.password.as_deref(), cancel)?); }
    if failures.is_empty() { return Ok(()); }
    let mut pages: Vec<f64> = failures.iter().filter_map(|failure| match field(failure, "pageIndex") { Some(IpcValue::Number(value)) => Some(value + 1.0), _ => None }).collect();
    pages.sort_by(f64::total_cmp);
    pages.dedup_by(|a, b| *a == *b);
    let pages = pages.into_iter().map(jscompat::number_to_string).collect::<Vec<_>>().join(",");
    let reason = failures.first().and_then(|failure| text(field(failure, "reason"))).unwrap_or("");
    Err(format!("save-verify-failed pages={pages}: {reason}; the file was not written"))
}

pub(super) fn save_to_path(source: &[u8], target: &str, request: &IpcValue, preserve_unchanged: bool, cancel: &CancelToken) -> Result<SaveSkips, String> {
    let (bytes, skips) = prepared(source, request, preserve_unchanged, cancel)?;
    cancelled(cancel)?;
    main_core::pdf::atomic_write::write_pdf_atomically(target, &bytes).map_err(|error| error.message)?;
    Ok(skips)
}

pub(super) fn prepared(source: &[u8], request: &IpcValue, preserve_unchanged: bool, cancel: &CancelToken) -> Result<(Vec<u8>, SaveSkips), String> {
    cancelled(cancel)?;
    if preserve_unchanged && save_policy::unchanged_disk_copy(request) { return Ok((source.to_vec(), SaveSkips::default())); }
    let (bytes, skips) = edited(source, request, cancel)?;
    let choice = field(request, "annotations").unwrap_or(&IpcValue::Undefined);
    let bytes = annotation_choice::apply(&bytes, choice, cancel)?;
    verify(&bytes, request, &skips, cancel)?;
    Ok((bytes, skips))
}

pub(super) fn bytes_with_edits(source: &[u8], request: &IpcValue, preserve_unchanged: bool, cancel: &CancelToken) -> Result<Vec<u8>, String> {
    cancelled(cancel)?;
    let request = ordinary(request);
    if preserve_unchanged && !save_policy::has_edits(&request) { return Ok(source.to_vec()); }
    let (bytes, skips) = edited(source, &request, cancel)?;
    verify(&bytes, &request, &skips, cancel)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_errors_keep_original_save_messages() {
        assert_eq!(decrypt_error(DecryptError::WrongPassword), "wrong password");
        assert_eq!(decrypt_error(DecryptError::CertificateEncryption), "certificate encryption");
        assert_eq!(decrypt_error(DecryptError::Other("bad PDF".into())), "bad PDF");
    }
}
