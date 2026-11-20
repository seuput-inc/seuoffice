//! Parent-side ownership of exactly one private signing worker.
use super::wire::{self, Kind};
use shell_contract::runtime::CancelToken;
use std::sync::mpsc;
use std::time::Duration;
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    OpenCertificate,
    SignDocument,
    VerifySignedDocument,
}

/// `request` contains only document-bound request fields. `authorize_retry`
/// must independently validate the complete first-attempt evidence.
pub(super) fn run(
    request: Vec<Zeroizing<Vec<u8>>>,
    nonce: [u8; 32],
    result_bound: usize,
    operation: Operation,
    cancel: &CancelToken,
    guard: impl Fn() -> Result<(), String>,
    mut authorize_retry: impl FnMut(Option<&[u8]>) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    guard()?;
    if cancel.is_cancelled() {
        return Err("signing request cancelled".into());
    }
    let mut worker = crate::index_worker_process::spawn_pdf_signer()?;
    let mut input = worker
        .child
        .stdin
        .take()
        .ok_or("signing worker stdin unavailable")?;
    let mut output = worker
        .child
        .stdout
        .take()
        .ok_or("signing worker stdout unavailable")?;
    let (reply_tx, reply_rx) = mpsc::channel::<Kind>();
    let (event_tx, event_rx) = mpsc::channel();
    let writer_events = event_tx.clone();
    let writer = std::thread::spawn(move || {
        let result = (|| {
            for frame in request {
                wire::write_frame(&mut input, Kind::Request, &frame, frame.len())?;
            }
            while let Ok(reply) = reply_rx.recv() {
                wire::write_frame(&mut input, reply, &nonce, nonce.len())?;
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = result {
            let _ = writer_events.send(Err(error));
        }
    });
    let reader = std::thread::spawn(move || {
        loop {
            let frame = wire::read_frame(&mut output, result_bound);
            let done = match &frame {
                Ok((kind, _)) => matches!(
                    kind,
                    Kind::Signed | Kind::Opened | Kind::Verified | Kind::Error
                ),
                Err(_) => true,
            };
            if event_tx.send(frame).is_err() || done {
                break;
            }
        }
    });
    let result = (|| {
        let mut admitted = 0u8;
        loop {
            if cancel.is_cancelled() {
                return Err("signing request cancelled".into());
            }
            guard()?;
            let frame = match event_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(frame) => frame?,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("signing worker disconnected".into());
                }
            };
            let (kind, body) = frame;
            if body.len() < nonce.len() || body[..nonce.len()] != nonce {
                return Err("signing worker nonce mismatch".into());
            }
            let body = &body[nonce.len()..];
            match (kind, body) {
                (Kind::Guard, []) => {
                    guard()?;
                    reply_tx
                        .send(Kind::Permit)
                        .map_err(|_| "signing worker input closed")?;
                }
                (Kind::AdmitFirst, body)
                    if operation == Operation::SignDocument && body.is_empty() && admitted == 0 =>
                {
                    authorize_retry(None)?;
                    admitted = 1;
                    reply_tx
                        .send(Kind::Permit)
                        .map_err(|_| "signing worker input closed")?;
                }
                (Kind::AdmitRetry, evidence)
                    if operation == Operation::SignDocument
                        && !evidence.is_empty()
                        && admitted == 1 =>
                {
                    authorize_retry(Some(evidence))?;
                    admitted = 2;
                    reply_tx
                        .send(Kind::Permit)
                        .map_err(|_| "signing worker input closed")?;
                }
                (Kind::Signed, bytes) if operation == Operation::SignDocument && admitted > 0 => {
                    guard()?;
                    return Ok(bytes.to_vec());
                }
                (Kind::Opened, bytes)
                    if operation == Operation::OpenCertificate && admitted == 0 =>
                {
                    guard()?;
                    return Ok(bytes.to_vec());
                }
                (Kind::Verified, bytes)
                    if operation == Operation::VerifySignedDocument
                        && admitted == 0
                        && bytes.is_empty() =>
                {
                    guard()?;
                    return Ok(Vec::new());
                }
                (Kind::Error, bytes) => {
                    let code = std::str::from_utf8(bytes).unwrap_or("");
                    return Err(match code {
                        "cert-password"
                        | "cert-invalid"
                        | "cert-no-key"
                        | "cert-unsupported-key"
                        | "cert-not-exportable"
                        | "cert-store-tool-missing"
                        | "pdf-encrypted" => code.to_owned(),
                        _ => "sign-failed".to_owned(),
                    });
                }
                _ => return Err("invalid signing worker protocol order".into()),
            }
        }
    })();
    drop(reply_tx);
    let cleanup = worker.terminate();
    let writer_result = writer.join();
    let reader_result = reader.join();
    cleanup?;
    if writer_result.is_err() || reader_result.is_err() {
        return Err("signing worker transport panicked".into());
    }
    result
}
