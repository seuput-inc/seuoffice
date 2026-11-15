//! DOCX-to-ODT export through the native document engine.

use chrono::Offset;
use odt_engine::assemble::{ReportKind, docx_to_odt};
use odt_engine::core::HostNow;
use shell_ipc_d2::html::{OdtConversion, ReportItem};

pub(super) fn convert(docx: &[u8]) -> Result<OdtConversion, String> {
    let now = chrono::Local::now();
    let clock = HostNow {
        ms: now.timestamp_millis() as f64,
        offset_minutes: -(now.offset().fix().local_minus_utc() as f64) / 60.0,
    };
    convert_at(docx, clock)
}

pub(crate) fn convert_at(docx: &[u8], clock: HostNow) -> Result<OdtConversion, String> {
    let converted = docx_to_odt(docx, &jscompat::JsMap::new(), clock)
        .map_err(|error| error.to_string())?;
    Ok(OdtConversion {
        odt: converted.odt_bytes,
        report: converted.report.into_iter().map(|item| ReportItem {
            kind: match item.kind {
                ReportKind::Converted => "converted",
                ReportKind::Dropped => "dropped",
            }.to_owned(),
            feature: item.feature.as_str().to_owned(),
            count: item.count as f64,
        }).collect(),
    })
}

pub(super) fn convert_cancellable(
    docx: &[u8], cancel: &shell_contract::runtime::CancelToken,
) -> Result<OdtConversion, String> {
    let now = chrono::Local::now();
    let clock = HostNow {
        ms: now.timestamp_millis() as f64,
        offset_minutes: -(now.offset().fix().local_minus_utc() as f64) / 60.0,
    };
    crate::odt_worker::convert(docx, clock, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_docx_does_not_produce_an_odt() {
        assert!(convert(b"not a document").is_err());
    }
}
