//! Structure job execution and collision-safe image output.

use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::{JobFailure, JobOutcome};

use super::super::{binary_job, image_output::{self, ExtractedImage}, job_source};
use super::structure;

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value { IpcValue::Object(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, value)| value), _ => None }
}
fn text<'a>(value: &'a IpcValue, key: &str) -> Result<&'a str, JobFailure> {
    match field(value, key) { Some(IpcValue::String(value)) => Ok(value), _ => Err(JobFailure::new("failed", Some("invalid structure job"))) }
}
fn number(value: &IpcValue, key: &str) -> Result<f64, JobFailure> {
    match field(value, key) { Some(IpcValue::Number(value)) => Ok(*value), _ => Err(JobFailure::new("failed", Some("invalid PDF image result"))) }
}
fn images(value: IpcValue) -> Result<Vec<ExtractedImage>, JobFailure> {
    let Some(IpcValue::Array(images)) = field(&value, "images") else { return Err(JobFailure::new("failed", Some("invalid PDF image result"))); };
    images.iter().map(|image| {
        let Some(IpcValue::Bytes(_, png)) = field(image, "png") else { return Err(JobFailure::new("failed", Some("invalid PDF image result"))); };
        Ok(ExtractedImage { page: number(image, "page")?, width: number(image, "width")?, height: number(image, "height")?, png: png.clone() })
    }).collect()
}

pub(super) fn run(job: &IpcValue, progress: &(dyn Fn(f64, f64) + Sync), cancel: &CancelToken) -> Option<JobOutcome> {
    let op = match field(job, "op") { Some(IpcValue::String(op)) => op.as_str(), _ => return None };
    if matches!(op, "encrypt" | "decrypt" | "repair") { return Some(binary_job::run(job, cancel)); }
    if !matches!(op, "tree" | "fingerprints" | "blankPages" | "images") { return None; }
    Some((|| {
        let source = job_source::load(job, cancel)?;
        let result = structure::extract(job, &source, op, progress, cancel)?;
        if cancel.is_cancelled() { return Err(JobFailure::new("cancelled", None)); }
        if op == "images" {
            let dir = text(job, "outDir")?;
            let written = image_output::write_images(dir, text(job, "baseName")?, &images(result)?, progress, cancel)?;
            return Ok(IpcValue::Object(vec![("docHash".into(), IpcValue::String(source.doc_hash)), ("outDir".into(), IpcValue::String(dir.to_owned())), ("images".into(), written)]));
        }
        if op == "blankPages" { return Ok(result); }
        let IpcValue::Object(mut fields) = result else { return Err(JobFailure::new("failed", Some("invalid PDF structure response"))); };
        fields.retain(|(key, _)| key != "docHash");
        fields.push(("docHash".into(), IpcValue::String(source.doc_hash)));
        Ok(IpcValue::Object(fields))
    })())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_tree_job_never_opens_its_source() {
        let cancel = CancelToken::default();
        cancel.cancel();
        let job = IpcValue::Object(vec![
            ("op".into(), IpcValue::String("tree".into())),
            ("source".into(), IpcValue::Object(vec![
                ("pdfPath".into(), IpcValue::String("/does-not-exist.pdf".into())),
            ])),
        ]);
        let failure = run(&job, &|_, _| {}, &cancel).unwrap().unwrap_err();
        assert_eq!(failure.error, "cancelled");
    }
}