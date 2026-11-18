//! Read-only MCP PDF text through the editor's PDFium vendor instance.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::pdf::pdfium::PdfiumError;
use main_core::pdf::read_text::{PdfPageText, PdfTextDoc, ReadPdfTextOptions};
use main_core::shell::mcp::tools::pdf_tools::PdfTextReader as Reader;
use serde_json::{Map, Value};

pub(crate) struct PdfTextReader;

impl Reader for PdfTextReader {
    fn read_pdf_text(&self, bytes: &[u8], options: &ReadPdfTextOptions) -> Result<PdfTextDoc, PdfiumError> {
        let mut input = Map::new();
        input.insert("bytesBase64".into(), Value::String(STANDARD.encode(bytes)));
        if let Some(value) = options.from_page { input.insert("fromPage".into(), Value::from(value)); }
        if let Some(value) = options.to_page { input.insert("toPage".into(), Value::from(value)); }
        if let Some(value) = options.char_budget { input.insert("charBudget".into(), Value::from(value)); }
        if let Some(value) = &options.pages { input.insert("pages".into(), Value::Array(value.iter().copied().map(Value::from).collect())); }
        if let Some(value) = &options.password { input.insert("password".into(), Value::String(value.clone())); }
        let value = crate::engine_host::call("pdf.readPdfDocument", &Value::Object(input)).map_err(|error| PdfiumError(error.message))?;
        let bad = || PdfiumError("PDFium returned an invalid text response".to_owned());
        let page_count = value.get("pageCount").and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok()).ok_or_else(bad)?;
        let info = value.get("info").and_then(Value::as_object).ok_or_else(bad)?;
        let title = info.get("title").and_then(Value::as_str).map(str::to_owned);
        let author = info.get("author").and_then(Value::as_str).map(str::to_owned);
        let pages = value.get("pages").and_then(Value::as_array).ok_or_else(bad)?.iter().map(|page| {
            Ok(PdfPageText {
                page: page.get("page").and_then(Value::as_f64).ok_or_else(bad)?,
                width_pt: page.get("widthPt").and_then(Value::as_f64).ok_or_else(bad)?,
                height_pt: page.get("heightPt").and_then(Value::as_f64).ok_or_else(bad)?,
                chars: page.get("chars").and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok()).ok_or_else(bad)?,
                text: page.get("text").and_then(Value::as_str).ok_or_else(bad)?.to_owned(),
                has_text_layer: page.get("hasTextLayer").and_then(Value::as_bool).ok_or_else(bad)?,
            })
        }).collect::<Result<Vec<_>, PdfiumError>>()?;
        let truncated = value.get("truncated").and_then(Value::as_bool).ok_or_else(bad)?;
        Ok(PdfTextDoc { page_count, title, author, pages, truncated })
    }
}
