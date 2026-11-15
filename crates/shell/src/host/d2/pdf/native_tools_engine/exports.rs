//! Native orchestration for PDF structure exports.
use std::path::Path;
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::{JobFailure, JobOutcome};
use super::structure;
use super::super::job_source::Source;

mod asset_output;
mod atomic;
mod epub;
mod markdown;
mod structured_text;
mod translation;
mod writeback;

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value { IpcValue::Object(fields) => fields.iter().find(|(key,_)|key==name).map(|(_,value)|value), _ => None }
}
fn text<'a>(value: &'a IpcValue, name: &str) -> Result<&'a str, JobFailure> {
    match field(value,name) { Some(IpcValue::String(value)) => Ok(value), _ => Err(JobFailure::new("failed",Some("invalid PDF export job"))) }
}
fn active(cancel: &CancelToken) -> Result<(),JobFailure> {
    if cancel.is_cancelled() { Err(JobFailure::new("cancelled",None)) } else { Ok(()) }
}
fn output(path: &str, bytes: &[u8], cancel: &CancelToken) -> Result<(),JobFailure> {
    atomic::write(path,bytes,cancel)
}
fn tree_for_job(job:&IpcValue, source:&Source, progress:&(dyn Fn(f64,f64)+Sync), cancel:&CancelToken) -> Result<(IpcValue,Option<IpcValue>),JobFailure> {
    if let Some(tree)=field(job,"tree").filter(|value|!matches!(value,IpcValue::Null|IpcValue::Undefined)) { return Ok((tree.clone(),None)); }
    let extracted=structure::extract(job,source,"tree",progress,cancel)?;
    active(cancel)?;
    let tree=field(&extracted,"tree").cloned().ok_or_else(||JobFailure::new("failed",Some("invalid PDF structure response")))?;
    Ok((tree,Some(extracted)))
}
fn object(fields:Vec<(&str,IpcValue)>)->IpcValue { IpcValue::Object(fields.into_iter().map(|(name,value)|(name.to_owned(),value)).collect()) }

/// Run an already-authorized job with the configured per-user result store.
pub(super) fn run(job:&IpcValue, source:&Source, user_data:&Path, progress:&(dyn Fn(f64,f64)+Sync), cancel:&CancelToken)->JobOutcome {
    active(cancel)?;
    let op=text(job,"op")?;
    let (tree, extracted)=tree_for_job(job,source,progress,cancel)?;
    let tree=translation::with_ai_readings(tree,&source.doc_hash,user_data)?;
    active(cancel)?;
    match op {
        "structuredText" => {
            let pages=structured_text::format(&tree)?;
            active(cancel)?;
            let mut fields=vec![("docHash",IpcValue::String(source.doc_hash.clone())),("pages",pages)];
            if let Some(extracted)=extracted { fields.push(("extracted",extracted)); }
            Ok(object(fields))
        }
        "markdown" => {
            let path=text(job,"outPath")?;
            let (tree,report)=translation::apply(tree,&source.doc_hash,field(job,"translation").filter(|v|!matches!(v,IpcValue::Null|IpcValue::Undefined)),user_data)?;
            let conversion=markdown::convert(&tree)?;
            let (written,reused)=asset_output::write(path,&conversion,cancel)?;
            let mut fields=vec![("docHash",IpcValue::String(source.doc_hash.clone())),("outPath",IpcValue::String(path.to_owned())),("warnings",conversion.warnings),("assetsWritten",IpcValue::Number(written as f64)),("assetsReused",IpcValue::Number(reused as f64))];
            if let Some(report)=report { fields.push(("translation",report)); }
            Ok(object(fields))
        }
        "epub" => {
            let path=text(job,"outPath")?;
            let cover=epub::cover(job,source,progress,cancel)?;
            let (tree,report)=translation::apply(tree,&source.doc_hash,field(job,"translation").filter(|v|!matches!(v,IpcValue::Null|IpcValue::Undefined)),user_data)?;
            let bytes=epub::write(&tree,job,cover)?;
            output(path,&bytes,cancel)?;
            let mut fields=vec![("docHash",IpcValue::String(source.doc_hash.clone())),("outPath",IpcValue::String(path.to_owned()))];
            if let Some(report)=report { fields.push(("translation",report)); }
            Ok(object(fields))
        }
        "writeBack" => {
            let path=text(job,"outPath")?;
            let (pdf,report)=writeback::apply(job,source,&tree,user_data,progress,cancel)?;
            output(path,&pdf,cancel)?;
            Ok(object(vec![("docHash",IpcValue::String(source.doc_hash.clone())),("outPath",IpcValue::String(path.to_owned())),("report",report)]))
        }
        _ => Err(JobFailure::new("failed",Some("invalid PDF export operation"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::BytesKind;

    #[test]
    fn cached_tree_keeps_embedded_image_bytes_and_skips_extraction() {
        let tree=IpcValue::Object(vec![("image".into(),IpcValue::Bytes(BytesKind::Uint8Array,vec![0,255]))]);
        let job=object(vec![("tree",tree.clone())]);
        let source=Source { bytes:Vec::new(), doc_hash:"hash".into() };
        let (actual,extracted)=tree_for_job(&job,&source,&|_,_|{},&CancelToken::default()).unwrap();
        assert_eq!(actual,tree);
        assert!(extracted.is_none());
    }
    #[test]
    fn cancelled_export_stops_before_tree_access() {
        let cancel=CancelToken::default(); cancel.cancel();
        let source=Source { bytes:Vec::new(), doc_hash:"hash".into() };
        let result=run(&IpcValue::Null,&source,Path::new("."),&|_,_|{},&cancel);
        assert_eq!(result.unwrap_err().error,"cancelled");
    }
}
