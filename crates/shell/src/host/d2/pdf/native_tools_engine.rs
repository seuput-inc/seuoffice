//! Native orchestration for the PDF tools host.

use std::path::PathBuf;

use serde_json::Map;
use shell_contract::{IpcValue, app_actions::PdfExport, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::{
    ConvertError, Inspect, JobOutcome, OfficeConversion, PdfToolsEngine, StoreError, StoreMethod,
    ToolError,
};

mod exports;
mod extraction_jobs;
mod office;
mod page_tools;
mod structure;

use super::{image_info, page_ops, store, thumbnail};

pub(crate) struct NativePdfToolsEngine {
    user_data: PathBuf,
}

impl NativePdfToolsEngine {
    pub(crate) fn new(user_data: PathBuf) -> Self {
        Self { user_data }
    }

    fn run_structure_job(
        &self,
        job: IpcValue,
        progress: &(dyn Fn(f64, f64) + Sync),
        cancel: &CancelToken,
    ) -> JobOutcome {
        if let Some(result) = extraction_jobs::run(&job, progress, cancel) {
            return result;
        }
        let source = super::job_source::load(&job, cancel)?;
        exports::run(&job, &source, &self.user_data, progress, cancel)
    }

    fn store_result(
        &self,
        user_data: PathBuf,
        method: StoreMethod,
        args: Vec<IpcValue>,
    ) -> Result<IpcValue, StoreError> {
        store::store(user_data, method, args)
    }

    fn inspect(&self, bytes: &[u8]) -> Inspect {
        let result = page_ops::call("pdf.inspectPdf", bytes, Map::new())?;
        if result.get("ok").and_then(|value| value.as_bool()) == Some(true) {
            result
                .get("pages")
                .and_then(|value| value.as_f64())
                .ok_or_else(|| "invalid PDF inspection response".to_owned())
        } else {
            Err(result
                .get("problem")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "invalid PDF inspection response".to_owned())?
                .to_owned())
        }
    }

    fn encrypted_count(&self, bytes: &[u8]) -> Result<f64, String> {
        page_ops::call("pdf.encryptedPageCount", bytes, Map::new())?
            .as_f64()
            .ok_or_else(|| "invalid encrypted PDF page count".to_owned())
    }

    fn image(&self, bytes: &[u8]) -> bool {
        image_info::is_image(bytes)
    }

    fn thumbnail(&self, width: u32, height: u32, bytes: &[u8]) -> Result<String, String> {
        thumbnail::png_data_url(width, height, bytes)
    }
}

impl PdfToolsEngine for NativePdfToolsEngine {
    fn run_job(
        &self,
        job: IpcValue,
        progress: &(dyn Fn(f64, f64) + Sync),
        cancel: &CancelToken,
    ) -> JobOutcome {
        self.run_structure_job(job, progress, cancel)
    }

    fn store(
        &self,
        user_data: PathBuf,
        method: StoreMethod,
        args: Vec<IpcValue>,
    ) -> Result<IpcValue, StoreError> {
        self.store_result(user_data, method, args)
    }

    fn inspect_pdf(&self, bytes: &[u8]) -> Inspect {
        self.inspect(bytes)
    }

    fn encrypted_page_count(&self, bytes: &[u8]) -> Result<f64, String> {
        self.encrypted_count(bytes)
    }

    fn is_image(&self, bytes: &[u8]) -> bool {
        self.image(bytes)
    }

    fn run_tool(
        &self,
        tool: &str,
        inputs: Vec<Vec<u8>>,
        options: IpcValue,
        progress: &(dyn Fn(f64, f64) + Sync),
        cancel: &CancelToken,
    ) -> Result<Vec<Vec<u8>>, ToolError> {
        if let Some(result) = page_tools::run(tool, &inputs, &options, cancel) {
            return result;
        }
        if tool == "compress" {
            let first = inputs
                .first()
                .ok_or_else(|| ToolError::Failed("no input".into()))?;
            let level = match &options {
                IpcValue::Object(fields) => fields
                    .iter()
                    .find_map(|(name, value)| match (name.as_str(), value) {
                        ("compressLevel", IpcValue::String(value)) => Some(value.as_str()),
                        _ => None,
                    })
                    .unwrap_or("medium"),
                _ => "medium",
            };
            let compressed =
                super::compress::run(first, level, progress, cancel).map_err(ToolError::Failed)?;
            if compressed.len() >= first.len() {
                return Err(ToolError::Refusal("not-smaller".into()));
            }
            return Ok(vec![compressed]);
        }
        Err(ToolError::Failed(
            if matches!(tool, "encrypt" | "decrypt" | "repair") {
                format!("{tool} runs as a job")
            } else {
                format!("unknown PDF tool: {tool}")
            },
        ))
    }

    fn png_data_url(&self, width: f64, height: f64, bgra: &[u8]) -> Result<String, String> {
        if !width.is_finite()
            || !height.is_finite()
            || width.fract() != 0.0
            || height.fract() != 0.0
            || width <= 0.0
            || height <= 0.0
            || width > u32::MAX as f64
            || height > u32::MAX as f64
        {
            return Err("invalid thumbnail dimensions".into());
        }
        self.thumbnail(width as u32, height as u32, bgra)
    }

    fn convert_to_office(
        &self,
        format: PdfExport,
        pdf_path: &str,
        password: Option<&str>,
        progress: &(dyn Fn(f64, f64) + Sync),
    ) -> Result<OfficeConversion, ConvertError> {
        office::convert(format, pdf_path, password, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_thumbnail_bitmap_is_a_failure() {
        let engine = NativePdfToolsEngine::new(PathBuf::new());
        assert!(engine.png_data_url(2.0, 1.0, &[0; 4]).is_err());
    }
}
