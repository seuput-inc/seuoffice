//! Private, byte-only DOCX conversion worker.
use odt_engine::core::HostNow;
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::html::{OdtConversion, ReportItem};
use std::io::{Read, Write};
use std::sync::mpsc;

const MAGIC: &[u8; 8] = b"ODTWRK01";

enum Event {
    Written(Result<(), String>),
    Read(Result<OdtConversion, String>),
    Cancelled,
}

fn write_blob(out: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let len = u64::try_from(bytes.len()).map_err(|_| "ODT worker length overflow")?;
    out.write_all(&len.to_le_bytes())
        .and_then(|()| out.write_all(bytes))
        .map_err(|e| e.to_string())
}

fn read_blob(input: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut length = [0; 8];
    input.read_exact(&mut length).map_err(|e| e.to_string())?;
    let length =
        usize::try_from(u64::from_le_bytes(length)).map_err(|_| "ODT worker length overflow")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|e| format!("allocating ODT worker payload: {e}"))?;
    bytes.resize(length, 0);
    input.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn read_response(input: &mut impl Read) -> Result<OdtConversion, String> {
    let mut status = [0];
    input.read_exact(&mut status).map_err(|e| e.to_string())?;
    let result = match status[0] {
        0 => {
            let odt = read_blob(input)?;
            let report = read_blob(input)?;
            let rows: Vec<(String, String, f64)> =
                serde_json::from_slice(&report).map_err(|e| e.to_string())?;
            Ok(OdtConversion {
                odt,
                report: rows
                    .into_iter()
                    .map(|(kind, feature, count)| ReportItem {
                        kind,
                        feature,
                        count,
                    })
                    .collect(),
            })
        }
        1 => Err(String::from_utf8(read_blob(input)?).map_err(|e| e.to_string())?),
        _ => Err("invalid ODT worker response".into()),
    };
    let mut trailing = [0];
    if input.read(&mut trailing).map_err(|e| e.to_string())? != 0 {
        return Err("trailing ODT worker response".into());
    }
    result
}

pub(crate) fn run_child(mut output: std::fs::File) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut magic = [0; 8];
    input.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if &magic != MAGIC {
        return Err("invalid ODT worker request".into());
    }
    let mut ms = [0; 8];
    let mut offset = [0; 8];
    input
        .read_exact(&mut ms)
        .and_then(|()| input.read_exact(&mut offset))
        .map_err(|e| e.to_string())?;
    let docx = read_blob(&mut input)?;
    let mut trailing = [0];
    if input.read(&mut trailing).map_err(|e| e.to_string())? != 0 {
        return Err("trailing ODT worker request".into());
    }
    let clock = HostNow {
        ms: f64::from_le_bytes(ms),
        offset_minutes: f64::from_le_bytes(offset),
    };
    match crate::host::d2::odt_export::convert_at(&docx, clock) {
        Ok(result) => {
            let rows: Vec<_> = result
                .report
                .into_iter()
                .map(|item| (item.kind, item.feature, item.count))
                .collect();
            let report = serde_json::to_vec(&rows).map_err(|e| e.to_string())?;
            output.write_all(&[0]).map_err(|e| e.to_string())?;
            write_blob(&mut output, &result.odt)?;
            write_blob(&mut output, &report)?;
        }
        Err(error) => {
            output.write_all(&[1]).map_err(|e| e.to_string())?;
            write_blob(&mut output, error.as_bytes())?;
        }
    }
    output.flush().map_err(|e| e.to_string())
}

pub(crate) fn convert(
    docx: &[u8],
    clock: HostNow,
    cancel: &CancelToken,
) -> Result<OdtConversion, String> {
    if cancel.is_cancelled() {
        return Err("ODT conversion cancelled".into());
    }
    let worker = crate::index_worker_process::spawn_odt_worker()?;
    run_worker(worker, docx, clock, cancel)
}

fn run_worker(
    mut worker: crate::index_worker_process::GuardedDecodeWorker,
    docx: &[u8],
    clock: HostNow,
    cancel: &CancelToken,
) -> Result<OdtConversion, String> {
    let mut input = worker
        .child
        .stdin
        .take()
        .ok_or("ODT worker stdin unavailable")?;
    let mut output = worker
        .child
        .stdout
        .take()
        .ok_or("ODT worker stdout unavailable")?;
    let (tx, rx) = mpsc::channel();
    let cancelled = tx.clone();
    let _subscription = cancel.on_cancel(move || {
        let _ = cancelled.send(Event::Cancelled);
    });
    std::thread::scope(|scope| {
        let written = tx.clone();
        let writer = scope.spawn(move || {
            let result = (|| {
                input
                    .write_all(MAGIC)
                    .and_then(|()| input.write_all(&clock.ms.to_le_bytes()))
                    .and_then(|()| input.write_all(&clock.offset_minutes.to_le_bytes()))
                    .map_err(|e| e.to_string())?;
                write_blob(&mut input, docx)
            })();
            drop(input);
            let _ = written.send(Event::Written(result));
        });
        let reader = scope.spawn(move || {
            let _ = tx.send(Event::Read(read_response(&mut output)));
        });
        let result = loop {
            match rx.recv() {
                Ok(Event::Written(Ok(()))) => continue,
                Ok(Event::Written(Err(error))) | Ok(Event::Read(Err(error))) => break Err(error),
                Ok(Event::Read(Ok(result))) => {
                    break if cancel.is_cancelled() {
                        Err("ODT conversion cancelled".into())
                    } else {
                        Ok(result)
                    };
                }
                Ok(Event::Cancelled) => break Err("ODT conversion cancelled".into()),
                Err(_) => break Err("ODT worker transport ended unexpectedly".into()),
            }
        };
        let cleanup = worker.terminate();
        let writer_result = writer.join();
        let reader_result = reader.join();
        cleanup?;
        if writer_result.is_err() || reader_result.is_err() {
            return Err("ODT worker transport panicked".into());
        }
        if cancel.is_cancelled() {
            return Err("ODT conversion cancelled".into());
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_preserves_bytes_and_report() {
        let mut bytes = vec![0];
        write_blob(&mut bytes, b"odt bytes").unwrap();
        write_blob(
            &mut bytes,
            br#"[["converted","chart",2.0],["dropped","ink",1.0]]"#,
        )
        .unwrap();
        let result = read_response(&mut bytes.as_slice()).unwrap();
        assert_eq!(result.odt, b"odt bytes");
        assert_eq!(
            result.report,
            vec![
                ReportItem {
                    kind: "converted".into(),
                    feature: "chart".into(),
                    count: 2.0
                },
                ReportItem {
                    kind: "dropped".into(),
                    feature: "ink".into(),
                    count: 1.0
                },
            ]
        );
    }

    #[test]
    fn failed_and_truncated_responses_are_errors() {
        let mut bytes = vec![1];
        write_blob(&mut bytes, b"invalid DOCX").unwrap();
        assert_eq!(
            read_response(&mut bytes.as_slice()).unwrap_err(),
            "invalid DOCX"
        );
        assert!(read_response(&mut [0, 2, 0].as_slice()).is_err());
    }

    #[test]
    fn cancelled_request_does_not_start_worker() {
        let token = CancelToken::default();
        token.cancel();
        let clock = HostNow {
            ms: 0.0,
            offset_minutes: 0.0,
        };
        assert_eq!(
            convert(b"unused", clock, &token).unwrap_err(),
            "ODT conversion cancelled"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_stalled_child() {
        let worker = crate::index_worker_process::spawn_stalled_odt_test_worker().unwrap();
        let token = CancelToken::default();
        let worker_token = token.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let bytes = vec![0; 8 * 1024 * 1024];
            let clock = HostNow {
                ms: 42.0,
                offset_minutes: -60.0,
            };
            started_tx.send(()).unwrap();
            run_worker(worker, &bytes, clock, &worker_token)
        });
        started_rx.recv().unwrap();
        token.cancel();
        assert_eq!(
            join.join().unwrap().unwrap_err(),
            "ODT conversion cancelled"
        );
    }
}
