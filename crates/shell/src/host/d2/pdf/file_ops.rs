//! Filesystem operations behind the editor's existing grant checks.

pub(super) fn copy_pdf_to_path(source: &str, target: &str, cancel: &shell_contract::runtime::CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() { return Err("PDF save cancelled".into()); }
    let bytes = std::fs::read(source).map_err(|error| error.to_string())?;
    if cancel.is_cancelled() { return Err("PDF save cancelled".into()); }
    main_core::pdf::atomic_write::write_pdf_atomically(target, &bytes).map_err(|error| error.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::runtime::CancelToken;

    #[test]
    fn cancelled_copy_stops_before_opening_source() {
        let cancel = CancelToken::default();
        cancel.cancel();
        assert_eq!(copy_pdf_to_path("/missing-source.pdf", "/missing-target.pdf", &cancel).unwrap_err(), "PDF save cancelled");
    }
}
