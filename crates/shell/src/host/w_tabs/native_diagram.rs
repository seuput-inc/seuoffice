use std::io::{Cursor, Read};
use shell_contract::ipc::{NativeEditorCommand, NativeInvocationOperation, NativeInvocationSelection};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::runtime::{CancelToken, Services};
use shell_ipc_d1::diagram::{Detected, DiagramRecoveryLookup, detect_format, open_refusal};
use main_core::packages::i18n::Lang;

fn admit(effects: &OwnedEffectsHost, operation: &NativeInvocationOperation, expected: &NativeEditorCommand) -> Result<(), String> {
    effects.with_native_command_preparation_effect(operation, expected, &[], operation.cancel(), operation.shutdown(), &mut || Ok(()))
}

fn is_visio_drawing(bytes: &[u8]) -> bool {
    if bytes.is_empty() || ooxml_pkg::assert_zip_inflates_within_limits(bytes, &ooxml_pkg::DEFAULT_ZIP_LIMITS).is_err() { return false; }
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else { return false; };
    let mut has_document = false;
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else { return false; };
        if entry.name().eq_ignore_ascii_case("visio/document.xml") { has_document = true; }
    }
    if !has_document { return false; }
    let Ok(mut content_types) = archive.by_name("[Content_Types].xml") else { return false; };
    let mut data = Vec::new();
    if content_types.read_to_end(&mut data).is_err() { return false; }
    String::from_utf8_lossy(&data).to_ascii_lowercase().contains("application/vnd.ms-visio.drawing.main+xml")
}


pub(super) fn check(
    services: &Services, effects: OwnedEffectsHost, lookup: DiagramRecoveryLookup, lang: Lang,
    path: String, operation: NativeInvocationOperation, expected: NativeEditorCommand,
    done: Box<dyn FnOnce(Result<Option<String>, String>)>,
) {
    if !operation.is_root_native_command()
        || !matches!(&expected, NativeEditorCommand::OpenFile { surface: shell_contract::Surface::Diagram, path: selected } if selected.as_os_str() == std::ffi::OsStr::new(&path))
        || operation.selection() != &NativeInvocationSelection::NativeCommand(expected.clone()) {
        return done(Err("The diagram path does not match the selected native command".into()));
    }
    if let Err(error) = admit(&effects, &operation, &expected) { return done(Err(error)); }
    let core_effects = effects.clone();
    let core_operation = operation.clone();
    let core_expected = expected.clone();
    let reply = shell_ipc_w::home_files::back_to_core(services.core.clone(), move |result| {
        done(admit(&core_effects, &core_operation, &core_expected).and(result));
    });
    services.workers.spawn(Box::new(move || {
        let result = (|| -> Result<Option<String>, String> {
            admit(&effects, &operation, &expected)?;
            let format = match detect_format(&path) {
                Some(format) => format,
                None => return open_refusal(lang, "unsupported").map(Some),
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => return open_refusal(lang, "unreadable").map(Some),
            };
            if format == Detected::Vsdx {
                return if is_visio_drawing(&bytes) { Ok(None) } else { open_refusal(lang, "visio_unreadable").map(Some) };
            }
            let name = jscompat::node_path::host::basename(&path, None);
            let cancel: &CancelToken = operation.cancel();
            let pending: &CancelToken = operation.shutdown();
            match crate::host::d1::diagram::open_native_command_preparation(&name, &bytes, &operation, &expected, None, &[], cancel, pending)? {
                Ok(_) => Ok(None),
                Err(failure) => {
                    admit(&effects, &operation, &expected)?;
                    if lookup.has_copy(&path) { Ok(None) } else { open_refusal(lang, &failure.reason).map(Some) }
                }
            }
        })();
        reply(result);
    }));
}

pub(super) fn check_owned(
    services: &Services, effects: OwnedEffectsHost, lookup: DiagramRecoveryLookup, lang: Lang,
    path: String, origin: shell_contract::ipc::NativeRequestOwner,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
    done: Box<dyn FnOnce(Result<Option<String>, String>)>,
) {
    let admit = |effect: &OwnedEffectsHost, origin: &shell_contract::ipc::NativeRequestOwner,
                 participants: &[shell_contract::ipc::NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken| {
        effect.with_owned_effect_many(origin, participants, cancel, pending, &mut || Ok(()))
    };
    if let Err(error) = admit(&effects, &origin, &participants, &cancel, &pending) { return done(Err(error)); }
    let core_effects = effects.clone();
    let core_origin = origin.clone();
    let core_participants = participants.clone();
    let core_cancel = cancel.clone();
    let core_pending = pending.clone();
    let reply = shell_ipc_w::home_files::back_to_core(services.core.clone(), move |result| {
        done(admit(&core_effects, &core_origin, &core_participants, &core_cancel, &core_pending).and(result));
    });
    services.workers.spawn(Box::new(move || {
        let result = (|| -> Result<Option<String>, String> {
            admit(&effects, &origin, &participants, &cancel, &pending)?;
            let format = match detect_format(&path) {
                Some(format) => format,
                None => return open_refusal(lang, "unsupported").map(Some),
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => return open_refusal(lang, "unreadable").map(Some),
            };
            if format == Detected::Vsdx {
                return if is_visio_drawing(&bytes) { Ok(None) } else { open_refusal(lang, "visio_unreadable").map(Some) };
            }
            let name = jscompat::node_path::host::basename(&path, None);
            match crate::host::d1::diagram::open_owned_preparation(&name, &bytes, &origin, &participants, &cancel, &pending)? {
                Ok(_) => Ok(None),
                Err(failure) => {
                    admit(&effects, &origin, &participants, &cancel, &pending)?;
                    if lookup.has_copy(&path) { Ok(None) } else { open_refusal(lang, &failure.reason).map(Some) }
                }
            }
        })();
        reply(result);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(document: Option<&str>, content_types: Option<(&str, &str)>) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        if let Some(name) = document {
            writer.start_file(name, options).unwrap();
            writer.write_all(b"<VisioDocument/>").unwrap();
        }
        if let Some((name, value)) = content_types {
            writer.start_file(name, options).unwrap();
            writer.write_all(value.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn visio_requires_valid_archive_document_and_content_type() {
        let content_type = "application/vnd.ms-visio.drawing.main+xml";
        assert!(!is_visio_drawing(&[]));
        assert!(!is_visio_drawing(b"not a zip"));
        assert!(!is_visio_drawing(&archive(None, Some(("[Content_Types].xml", content_type)))));
        assert!(!is_visio_drawing(&archive(Some("visio/document.xml"), Some(("[Content_Types].xml", "not a drawing")))));
        assert!(!is_visio_drawing(&archive(Some("visio/document.xml"), Some(("[content_types].xml", content_type)))));
        assert!(is_visio_drawing(&archive(Some("VISIO/DOCUMENT.XML"), Some(("[Content_Types].xml", content_type)))));
    }
}
