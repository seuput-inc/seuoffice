//! Copy the selected pages of an already-edited PDF.

use serde_json::Map;
use shell_contract::IpcValue;

pub(super) fn copied_pages(edited: &[u8], positions: &IpcValue, cancel: &shell_contract::runtime::CancelToken) -> Result<Option<(Vec<u8>, f64)>, String> {
    let count = super::super::page_ops::call_cancellable("pdf.pageCount", edited, Map::new(), cancel)?
        .as_f64().ok_or("PDF page count is invalid")?;
    let IpcValue::Array(positions) = positions else { return Ok(None) };
    let mut pages: Vec<f64> = positions.iter().filter_map(|value| match value {
        IpcValue::Number(page) if page.is_finite() && page.fract() == 0.0 && *page >= 0.0 && *page < count => Some(*page),
        _ => None,
    }).collect();
    pages.sort_by(f64::total_cmp);
    pages.dedup_by(|a, b| *a == *b);
    if pages.is_empty() { return Ok(None); }
    let len = pages.len() as f64;
    let pages = IpcValue::Array(pages.into_iter().map(IpcValue::Number).collect());
    Ok(Some((super::super::page_ops::extract_pages_cancellable(edited, &pages, cancel)?, len)))
}
