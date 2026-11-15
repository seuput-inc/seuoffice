//! Read a structure job's already-authorized source on the native side.

use sha2::{Digest, Sha256};
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::JobFailure;

pub(super) struct Source {
    pub bytes: Vec<u8>,
    pub doc_hash: String,
}

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, value)| value),
        _ => None,
    }
}

pub(super) fn load(job: &IpcValue, cancel: &CancelToken) -> Result<Source, JobFailure> {
    if cancel.is_cancelled() { return Err(JobFailure::new("cancelled", None)); }
    let path = field(job, "source").and_then(|source| field(source, "pdfPath"));
    let Some(IpcValue::String(path)) = path else { return Err(JobFailure::new("failed", Some("invalid PDF job source"))) };
    let bytes = std::fs::read(path).map_err(|error| JobFailure::new("failed", Some(&main_core::node_fs::node_error(&error, "open", path).message)))?;
    if cancel.is_cancelled() { return Err(JobFailure::new("cancelled", None)); }
    let doc_hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(Source { bytes, doc_hash })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_job_does_not_open_source() {
        let token = CancelToken::default();
        token.cancel();
        let job = IpcValue::Object(vec![("source".into(), IpcValue::Object(vec![("pdfPath".into(), IpcValue::String("/missing.pdf".into()))]))]);
        let failure = match load(&job, &token) { Ok(_) => panic!("cancelled job read a source"), Err(failure) => failure };
        assert_eq!(failure.error, "cancelled");
    }
}
