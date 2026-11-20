//! Hosted comparison against the original shrinkDocument and packaged Electron zlib.
use super::*;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

fn invoke(electron: &str, mode: Option<&str>, input: Option<&Value>) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root");
    let script = root.join("crates/shell/src/host/d2/pdf/native_engine/shrink/oracle.cjs");
    let mut command = Command::new(electron);
    command
        .arg(script)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("PDF_SHRINK_REPO_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(mode) = mode {
        command.arg(mode);
    }
    let mut child = command.spawn().expect("start original Electron runtime");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("oracle stdin")
            .write_all(serde_json::to_string(input).unwrap().as_bytes())
            .expect("write oracle input");
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().expect("wait for oracle");
    assert!(
        output.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle JSON")
}
fn records(value: &Value) -> HashMap<String, &Value> {
    value
        .as_array()
        .expect("stream records")
        .iter()
        .map(|v| (v["ref"].as_str().expect("stream ref").to_owned(), v))
        .collect()
}
fn bytes(value: &Value) -> Vec<u8> {
    STANDARD
        .decode(value["bytesBase64"].as_str().expect("stream bytes"))
        .expect("base64 stream")
}

#[test]
#[ignore = "Hosted only: set PDF_SHRINK_ORACLE_ELECTRON to the original packaged Electron executable"]
fn original_electron_shrink_differential() {
    let electron = std::env::var("PDF_SHRINK_ORACLE_ELECTRON")
        .expect("set PDF_SHRINK_ORACLE_ELECTRON to the original packaged Electron executable");
    let oracle = invoke(&electron, None, None);
    assert!(
        oracle["electron"].is_string(),
        "oracle executable is not Electron: {oracle}"
    );
    eprintln!(
        "original shrink runtime Electron={} Node={} zlib={}",
        oracle["electron"], oracle["node"], oracle["zlib"]
    );
    assert_eq!(
        oracle["objectStreams"], true,
        "original save did not produce object streams"
    );
    let before = records(&oracle["before"]);
    let streams = oracle["before"]
        .as_array()
        .unwrap()
        .iter()
        .map(Stream::from_value)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let cancel = CancelToken::default();
    let planned = plan(streams, &cancel, |s| {
        Ok(bytes(before.get(&s.reference).expect("input stream")))
    })
    .unwrap();
    let actual_counts =
        json!({"streamsRecompressed":planned.streams,"imagesDownsampled":planned.images});
    eprintln!(
        "original counters={} native counters={}",
        oracle["counts"], actual_counts
    );
    let replacements: Vec<Value> = planned
        .replacements
        .iter()
        .map(|r| {
            let mut v = json!({"ref":r.reference,"bytesBase64":STANDARD.encode(&r.bytes)});
            if let Some((w, h)) = r.geometry {
                v["width"] = json!(w);
                v["height"] = json!(h);
            }
            v
        })
        .collect();
    let applied = invoke(
        &electron,
        Some("apply"),
        Some(&json!({"sourceBase64":oracle["sourceBase64"],"replacements":replacements})),
    );
    assert_eq!(
        applied["objectStreams"], true,
        "native replacement save did not produce object streams"
    );
    let expected = records(&oracle["after"]);
    let actual = records(&applied["after"]);
    let mismatch = |message: &str| -> ! {
        panic!(
            "{message}\nElectron={} Node={} zlib={}\nexpected counters={} native counters={}\nsource PDF={}\noriginal output={}\nnative output={}\nnative replacements={}",
            oracle["electron"],
            oracle["node"],
            oracle["zlib"],
            oracle["counts"],
            actual_counts,
            oracle["sourceBase64"],
            oracle["outputBase64"],
            applied["outputBase64"],
            serde_json::to_string(&replacements).unwrap()
        )
    };
    if actual_counts != oracle["counts"] {
        mismatch("shrink counter mismatch");
    }
    let mut reported_codec_difference = false;
    for reference in before.keys() {
        let (Some(want), Some(got)) = (expected.get(reference), actual.get(reference)) else {
            mismatch(&format!("stream {reference} missing after save"))
        };
        for key in [
            "subtype",
            "filter",
            "width",
            "height",
            "bitsPerComponent",
            "colorSpace",
            "decodeParms",
            "matte",
            "maskPresent",
            "maskRef",
            "intent",
            "decode",
        ] {
            if want[key] != got[key] {
                mismatch(&format!(
                    "stream {reference} dictionary differs at {key}: expected {} actual {}",
                    want[key], got[key]
                ));
            }
        }
        let expected_bytes = bytes(want);
        let actual_bytes = bytes(got);
        if expected_bytes != actual_bytes {
            if !reported_codec_difference {
                eprintln!(
                    "zlib byte difference for {reference}: original={} native={}",
                    STANDARD.encode(&expected_bytes),
                    STANDARD.encode(&actual_bytes)
                );
                reported_codec_difference = true;
            }
            if want["filter"] != "/FlateDecode" || want["decodeParms"] == true {
                mismatch(&format!("preserved stream {reference} bytes differ"));
            }
            let decoded_expected = inflate(&expected_bytes, &cancel).unwrap();
            let decoded_actual = inflate(&actual_bytes, &cancel).unwrap();
            if decoded_expected.is_none() || decoded_expected != decoded_actual {
                mismatch(&format!("stream {reference} decoded bytes differ"));
            }
        }
    }
}
