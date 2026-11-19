//! Import HTML spreadsheets and delegate binary legacy workbooks to the sidecar.
mod scanner;
mod xlsx;

use super::workbook_rpc::Rpc;
use encoding_rs::{Encoding, REPLACEMENT, UTF_16BE, UTF_16LE, WINDOWS_1252};
use jscompat::TryJsRegex;
use shell_ipc_d1::sheets::engine::{EngineResult, LegacyConversion, engine_error};
use std::fs::File;
use std::io::Read;

const PROBE_BYTES: usize = 8192;
const MAX_HTML_IMPORT_BYTES: u64 = 32 * 1024 * 1024;
fn looks_like_html(head: &[u8]) -> bool {
    if head.is_empty() || head.starts_with(b"PK") || head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]) {
        return false;
    }
    let text = if head.starts_with(&[0xff, 0xfe]) {
        UTF_16LE.decode(&head[2..]).0.into_owned()
    } else if head.starts_with(&[0xfe, 0xff]) {
        UTF_16BE.decode(&head[2..]).0.into_owned()
    } else {
        WINDOWS_1252.decode(head).0.into_owned()
    };
    let text = text.trim_start_matches('\u{feff}').to_ascii_lowercase();
    if text.contains("urn:schemas-microsoft-com:office:spreadsheet") {
        return false;
    }
    if text.starts_with("<!doctype html")
        || text.contains("<html")
        || text.contains("urn:schemas-microsoft-com:office:excel")
    {
        return true;
    }
    let table = TryJsRegex::new(r"<table[\s>]", "").expect("static regex");
    let cell = TryJsRegex::new(r"<(?:td|th)[\s>]", "").expect("static regex");
    table.try_test(&text).unwrap_or(false) && cell.try_test(&text).unwrap_or(false)
}
fn decode_html(bytes: &[u8], preferred: Option<&str>) -> String {
    let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff]);
    if !bom {
        let head = WINDOWS_1252.decode(&bytes[..bytes.len().min(4096)]).0;
        let pattern = TryJsRegex::new(r#"<meta\b[^>]*charset\s*=\s*["']?\s*([a-z0-9._:-]+)"#, "i")
            .expect("static regex");
        if let Ok(Some(found)) = pattern.try_exec(&head)
            && let Some(label) = found.get(1)
            && let Some(encoding) =
                Encoding::for_label(label.as_bytes()).filter(|e| *e != REPLACEMENT)
        {
            let (text, _, errors) = encoding.decode(bytes);
            if !errors && !text.contains('\u{fffd}') {
                return text.into_owned();
            }
        }
    }
    file_parse::csv::decode_csv_buffer(bytes, preferred)
}
pub(super) fn convert(
    rpc: &Rpc,
    path: &str,
    target: &str,
    preferred_charset: Option<&str>,
) -> EngineResult<LegacyConversion> {
    let mut file = File::open(path).map_err(|e| engine_error(e.to_string()))?;
    let mut head = [0u8; PROBE_BYTES];
    let n = file
        .read(&mut head)
        .map_err(|e| engine_error(e.to_string()))?;
    if !looks_like_html(&head[..n]) {
        let result = rpc.convert_workbook(path, target)?;
        return Ok(LegacyConversion::Workbook {
            formulas_dropped: result.formulas_dropped,
        });
    }
    if file
        .metadata()
        .map_err(|e| engine_error(e.to_string()))?
        .len()
        > MAX_HTML_IMPORT_BYTES
    {
        return Ok(LegacyConversion::HtmlTooLarge);
    }
    let bytes = std::fs::read(path).map_err(|e| engine_error(e.to_string()))?;
    if bytes.len() as u64 > MAX_HTML_IMPORT_BYTES {
        return Ok(LegacyConversion::HtmlTooLarge);
    }
    let text = decode_html(&bytes, preferred_charset);
    let sheets = scanner::parse(&text).map_err(engine_error)?;
    if sheets.is_empty() {
        return Ok(LegacyConversion::HtmlNoTable);
    }
    xlsx::write(target, &sheets)?;
    Ok(LegacyConversion::Html)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sniff_does_not_confuse_real_containers_or_spreadsheetml_with_html() {
        assert!(!looks_like_html(b"PK\x03\x04<html><table><td>x"));
        assert!(!looks_like_html(b"<?xml version='1.0'?><Workbook xmlns='urn:schemas-microsoft-com:office:spreadsheet'><Table>"));
        assert!(looks_like_html(b"<table><tr><td>hello</td></tr></table>"));
    }
    #[test]
    fn bom_and_declared_charset_take_precedence() {
        assert_eq!(decode_html(&[0xff, 0xfe, b'A', 0], Some("euc-kr")), "A");
        assert_eq!(
            decode_html(b"<meta charset=windows-1252>\x80", None),
            "<meta charset=windows-1252>€"
        );
    }
}
