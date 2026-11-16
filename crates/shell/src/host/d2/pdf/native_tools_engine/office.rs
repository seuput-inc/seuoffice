//! Native PDF extraction, analysis and editable Office reconstruction.
mod analyze;
mod background;
mod bitmap;
mod chars;
mod content_box;
mod docx;
mod extract;
mod fontname;
mod geometry;
mod glyphs;
mod guards;
mod image_payload;
mod images;
mod inkless;
mod ir;
mod lines;
mod links;
mod matrix;
mod metadata;
mod numbers;
mod ocr;
mod page_geometry;
mod paths;
mod pattern;
mod pptx;
mod pptx_text;
mod quality;
mod render;
mod rtl;
mod scan_overlay;
mod script;
mod shading;
mod spans;
mod widgets;
mod words;
mod xlsx;
mod xlsx_package;
mod xlsx_rowsplit;
mod xlsx_sheet;
mod xlsx_styles;
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use shell_contract::app_actions::PdfExport;
use shell_ipc_d2::pdf::tools::{ConvertError, OfficeConversion};
fn other(e: impl std::fmt::Display) -> ConvertError {
    ConvertError::Other(e.to_string())
}
fn load_error(e: PdfiumError) -> ConvertError {
    match e.code {
        Some(4) => ConvertError::Load("password-required".into()),
        Some(1 | 2 | 3) => ConvertError::Load("corrupt".into()),
        Some(_) => ConvertError::Load("unsupported".into()),
        None => other(e),
    }
}
pub(super) fn convert(
    format: PdfExport,
    pdf_path: &str,
    password: Option<&str>,
    progress: &(dyn Fn(f64, f64) + Sync),
) -> Result<OfficeConversion, ConvertError> {
    let bytes = std::fs::read(pdf_path).map_err(other)?;
    let mut session = PdfiumSession::open(&bytes, password).map_err(load_error)?;
    let doc = session.document();
    let total = paths::number(&mut session, "FPDF_GetPageCount", &[paths::ptr(doc, 0)])
        .map_err(other)? as usize;
    if total == 0 {
        return Err(ConvertError::Load("corrupt".into()));
    }
    let absolute = format == PdfExport::Pptx;
    let cell_data = format == PdfExport::Xlsx;
    let ocr_available = format == PdfExport::Docx
        && crate::host::d2::pdf::native_callbacks::ocr_available().map_err(other)?;
    let producer = metadata::read(&mut session, doc, "Producer").map_err(other)?;
    let creator = metadata::read(&mut session, doc, "Creator").map_err(other)?;
    let mut extracted = Vec::with_capacity(total);
    for index in 0..total {
        extracted.push(extract::page(&mut session, doc, index, cell_data).map_err(other)?);
    }
    let (mut analyzer, furniture, _dropped) = analyze::prepare(&mut extracted, absolute, cell_data);
    let mut pages = Vec::with_capacity(total);
    let mut results = Vec::with_capacity(total);
    for (index, raw) in extracted.iter_mut().enumerate() {
        let mut page = analyzer.analyze_page(raw).map_err(other)?;
        if page.scanned && ocr_available {
            if let Some(hi) = render::by_index(&mut session, doc, index, 3.0).map_err(other)? {
                if let Some((mut synthetic, _confidence)) = ocr::recover(raw, &hi.data) {
                    let mut recognized = analyzer.analyze_page(&mut synthetic).map_err(other)?;
                    if !recognized.scanned && !recognized.degraded {
                        recognized.ocr_recovered = true;
                        page = recognized;
                    }
                }
            }
        }
        guards::apply(&mut page, raw, cell_data, absolute, |underlay| {
            if underlay {
                render::without_text(&mut session, doc, index, 2.0).map_err(|e| e.to_string())
            } else {
                render::by_index(&mut session, doc, index, 2.0).map_err(|e| e.to_string())
            }
        })
        .map_err(other)?;
        let status = if page.ocr_recovered {
            "ocr"
        } else if page.scanned {
            "scanned"
        } else if page.degraded {
            "degraded"
        } else {
            "ok"
        };
        results.push(((index + 1) as f64, status.to_owned()));
        pages.push(page);
        progress((index + 1) as f64, total as f64);
    }
    analyze::classify_pages(&mut pages, producer.as_deref(), creator.as_deref()).map_err(other)?;
    if !cell_data && !absolute {
        guards::stitch(&mut pages);
    }
    if format != PdfExport::Docx {
        docx::substitute_fonts(&mut pages, &mut []);
    }
    let output = match format {
        PdfExport::Docx => docx::rebuild(&pages, &furniture).map_err(other)?,
        PdfExport::Pptx => pptx::rebuild(&pages).map_err(other)?,
        PdfExport::Xlsx => xlsx::rebuild(&pages, &furniture).map_err(other)?.0,
    };
    let scanned = results
        .iter()
        .filter(|(_, status)| status == "scanned")
        .count()
        > total / 2;
    Ok(OfficeConversion {
        bytes: output,
        scanned_document: scanned,
        page_results: results,
    })
}
