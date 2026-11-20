//! Hosted-only differential driver. It never accepts keys or signing digests.
use std::io::{self, BufRead, Write};
use serde_json::{json, Value};
use shell_contract::pdf_sign_options::{SignOptions, VisibleSignature};
include!(concat!(env!("OUT_DIR"), "/modules.rs"));

fn bytes(value: &Value, name: &str) -> Result<Vec<u8>, String> {
    let text = value.get(name).and_then(Value::as_str).ok_or_else(|| format!("missing {name}"))?;
    if text.len() % 2 != 0 { return Err(format!("invalid {name} hex")); }
    text.as_bytes().chunks_exact(2).map(|pair| {
        let digit = |b: u8| match b { b'0'..=b'9' => Some(b-b'0'), b'a'..=b'f' => Some(b-b'a'+10), b'A'..=b'F' => Some(b-b'A'+10), _ => None };
        Ok(digit(pair[0]).ok_or("invalid hex")? * 16 + digit(pair[1]).ok_or("invalid hex")?)
    }).collect()
}
fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len()*2);
    for &b in bytes { out.push(H[(b >> 4) as usize] as char); out.push(H[(b & 15) as usize] as char); }
    out
}
fn options(input: &Value) -> Result<SignOptions, String> {
    let text = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_owned);
    let visible = input.get("visible").filter(|value| !value.is_null()).map(|v| -> Result<VisibleSignature, String> {
        Ok(VisibleSignature { page_index: v["pageIndex"].as_f64().ok_or("missing pageIndex")?,
            position: v["position"].as_str().ok_or("missing position")?.to_owned(),
            width: v["width"].as_f64().ok_or("missing width")?, height: v["height"].as_f64().ok_or("missing height")?,
            png: bytes(v, "png")? })
    }).transpose()?;
    Ok(SignOptions { reason: text("reason"), location: text("location"), contact_info: text("contactInfo"),
        certify_level: input.get("certifyLevel").and_then(Value::as_u64).map(|n| u8::try_from(n).map_err(|_| "invalid certifyLevel".to_owned())).transpose()?, visible })
}
fn run(input: Value) -> Result<Value, String> {
    let op = input["op"].as_str().ok_or("missing operation")?;
    match op {
        "inspect" => Ok(host::d2::pdf::native_engine::signatures::inspect(&bytes(&input, "pdf")?)),
        "assemble" => {
            let original = bytes(&input, "pdf")?;
            let options = options(&input["options"])?;
            let time = input["time"].as_str().ok_or("missing time")?;
            let placeholder = input["placeholder"].as_u64().and_then(|n| usize::try_from(n).ok()).ok_or("missing placeholder")?;
            let (file, range) = host::d2::pdf::crypto::assemble(&original, &options, time, placeholder)?;
            Ok(json!({ "pdf": hex(&file), "range": range }))
        }
        "validate" => {
            let original = bytes(&input, "pdf")?;
            let revised = bytes(&input, "revised")?;
            let options = options(&input["options"])?;
            let time = input["time"].as_str().ok_or("missing time")?;
            let raw = input["range"].as_array().filter(|v| v.len() == 4).ok_or("invalid range")?;
            let mut range = [0; 4];
            for (out, value) in range.iter_mut().zip(raw) { *out = value.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or("invalid range")?; }
            host::d2::pdf::crypto::validate(&original, &revised, &options, time, range)?;
            Ok(json!({ "valid": true }))
        }
        _ => Err("unknown operation".into()),
    }
}
fn main() {
    let stdin = io::stdin(); let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let response = line.map_err(|error| error.to_string()).and_then(|line| serde_json::from_str(&line).map_err(|error| error.to_string())).and_then(run);
        let response = match response { Ok(value) => json!({ "ok": true, "value": value }), Err(error) => json!({ "ok": false, "error": error }) };
        writeln!(stdout, "{response}").expect("write proof result"); stdout.flush().expect("flush proof result");
    }
}
