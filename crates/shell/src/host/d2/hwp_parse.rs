//! HWP editor reads through the linked rhwp-backed file parser.

use shell_ipc_d2::hwp::HwpMemo;

pub(crate) fn checked_bytes(bytes: Vec<u8>, ext: &str) -> Result<Vec<u8>, String> {
    file_parse::hwp::checked_hwp_bytes(&bytes, ext)?;
    Ok(bytes)
}

pub(crate) fn text(bytes: &[u8]) -> Result<String, String> {
    file_parse::hwp::hwp_to_text(bytes)
}

pub(super) fn memos(bytes: &[u8]) -> Result<Vec<HwpMemo>, String> {
    file_parse::hwp::hwp_memos_of(bytes).map(|memos| memos.into_iter().map(|memo| {
        let field = |name| memo.get(name).and_then(serde_json::Value::as_str).map(str::to_owned);
        HwpMemo {
            author: field("author"),
            date: field("date"),
            text: field("text"),
            place: field("place"),
        }
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_document_is_not_passed_to_rhwp() {
        let error = checked_bytes(b"plain text".to_vec(), "hwpx").unwrap_err();
        assert!(error.contains("Content mismatch"));
    }
}
