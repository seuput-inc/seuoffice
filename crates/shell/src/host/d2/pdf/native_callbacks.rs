//! Request-scoped native PDF OCR and installed-font capabilities.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::pdf::font_cmap::font_covers_text;
use main_core::pdf::font_locate::{FontEnv, FontLocator, Platform, is_truetype};
use serde_json::{Value, json};
use shell_contract::runtime::CancelToken;

const MAX_BUFFER: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

struct RequestFonts {
    locator: FontLocator,
    faces: HashMap<u64, Vec<u8>>,
    next: u64,
}

static RESOURCES: OnceLock<Option<PathBuf>> = OnceLock::new();
static REQUESTS: LazyLock<Mutex<HashMap<u64, RequestFonts>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn configure(resources: Option<PathBuf>) -> Result<(), String> {
    RESOURCES.set(resources).map_err(|_| "PDF callbacks already configured".to_owned())
}

fn requests() -> &'static Mutex<HashMap<u64, RequestFonts>> {
    &REQUESTS
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") { Platform::Darwin }
    else if cfg!(windows) { Platform::Win32 }
    else { Platform::Other }
}

fn font_env() -> FontEnv {
    FontEnv { platform: platform(), home: dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned() }
}

fn string<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing {key}"))
}

fn descriptor(input: &Value) -> Result<u64, String> {
    input.get("descriptor").and_then(Value::as_u64).filter(|id| *id != 0).ok_or_else(|| "invalid font descriptor".to_owned())
}

fn helper() -> Result<Option<PathBuf>, String> {
    let resources = RESOURCES.get().ok_or("PDF callbacks are not configured")?;
    let name = match platform() {
        Platform::Darwin => "vision-ocr",
        Platform::Win32 => "win-ocr.exe",
        Platform::Other => return Ok(None),
    };
    if let Some(path) = resources.as_ref().map(|root| root.join("ocr").join(name)).filter(|path| path.exists()) {
        return Ok(Some(path));
    }
    if cfg!(debug_assertions) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/pdf2docx/ocr-helper").join(name);
        if path.exists() { return Ok(Some(path)); }
    }
    Ok(None)
}

pub(crate) fn ocr_available() -> Result<bool, String> {
    Ok(helper()?.is_some())
}

pub(crate) fn recognize_page(png: &[u8], _width_pt: f64, _height_pt: f64, cancel: &CancelToken) -> Result<Option<Value>, String> {
    let Some(path) = helper()? else { return Ok(None) };
    if png.len() > MAX_BUFFER { return Err("OCR input is too large".into()); }
    recognize(&path, png.to_vec(), cancel).map(Some)
}

pub(crate) fn writeback_font(text: &str, lang: &str) -> Option<Vec<u8>> {
    let region = if lang == "zh-TW" { lang } else { lang.split('-').next().unwrap_or(lang) };
    let regional: &[(&str, &str)] = match region {
        "ko" => &[("MalgunGothic", "Malgun Gothic"), ("AppleSDGothicNeo-Regular", "Apple SD Gothic Neo"), ("NanumGothic", "NanumGothic"), ("NotoSansCJKkr-Regular", "Noto Sans CJK KR")],
        "ja" => &[("YuGothic-Regular", "Yu Gothic"), ("Meiryo", "Meiryo"), ("MSGothic", "MS Gothic"), ("HiraginoSans-W3", "Hiragino Sans"), ("NotoSansCJKjp-Regular", "Noto Sans CJK JP")],
        "zh-TW" => &[("MicrosoftJhengHeiRegular", "Microsoft JhengHei"), ("PMingLiU", "PMingLiU"), ("PingFangTC-Regular", "PingFang TC"), ("NotoSansCJKtc-Regular", "Noto Sans CJK TC")],
        "zh" => &[("MicrosoftYaHei", "Microsoft YaHei"), ("SimSun", "SimSun"), ("PingFangSC-Regular", "PingFang SC"), ("NotoSansCJKsc-Regular", "Noto Sans CJK SC")],
        "th" => &[("LeelawadeeUI", "Leelawadee UI"), ("Tahoma", "Tahoma"), ("Thonburi", "Thonburi")],
        _ => &[],
    };
    let latin = [("ArialMT", "Arial"), ("SegoeUI", "Segoe UI"), ("Helvetica", "Helvetica"), ("LiberationSans-Regular", "Liberation Sans"), ("DejaVuSans", "DejaVu Sans")];
    let mut locator = FontLocator::new(font_env());
    for (ps, family) in regional.iter().chain(latin.iter()) {
        if let Some(bytes) = locator.find_system_font(ps, family) && is_truetype(&bytes) && font_covers_text(&bytes, text) {
            return Some(bytes);
        }
    }
    locator.find_font_covering(text).filter(|bytes| is_truetype(bytes) && font_covers_text(bytes, text))
}

fn read_limited(mut input: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    input.by_ref().take((MAX_BUFFER + 1) as u64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BUFFER { return Err("OCR helper output is too large".into()); }
    Ok(bytes)
}

fn recognize(path: &Path, png: Vec<u8>, cancel: &CancelToken) -> Result<Value, String> {
    if cancel.is_cancelled() { return Err("OCR request cancelled".into()); }
    let mut command = Command::new(path);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|e| format!("starting OCR helper: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("OCR helper stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("OCR helper stdout unavailable")?;
    let writer = std::thread::spawn(move || stdin.write_all(&png));
    let reader = std::thread::spawn(move || read_limited(stdout));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if cancel.is_cancelled() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            let _ = reader.join();
            return Err(if cancel.is_cancelled() { "OCR request cancelled" } else { "OCR helper timed out" }.into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                let _ = reader.join();
                return Err(format!("waiting for OCR helper: {error}"));
            }
        }
    };
    let written = writer.join().map_err(|_| "OCR input thread failed")?;
    let output = reader.join().map_err(|_| "OCR output thread failed")??;
    written.map_err(|e| format!("writing OCR input: {e}"))?;
    if !status.success() { return Err("OCR helper failed".into()); }
    let text = std::str::from_utf8(&output).map_err(|e| e.to_string())?.trim_start_matches('\u{feff}');
    let result: Value = serde_json::from_str(text).map_err(|e| format!("invalid OCR response: {e}"))?;
    let lines = result.get("lines").and_then(Value::as_array).ok_or("invalid OCR lines")?;
    let lines = lines.iter().map(|line| {
        let mut out = serde_json::Map::new();
        out.insert("text".into(), line.get("t").cloned().ok_or("invalid OCR line")?);
        out.insert("confidence".into(), line.get("c").cloned().ok_or("invalid OCR confidence")?);
        out.insert("box".into(), rect(line.get("b"))?);
        if let Some(chars) = line.get("chars") {
            let chars = chars.as_array().ok_or("invalid OCR characters")?.iter().map(|c| Ok(json!({"text": c.get("t").ok_or("invalid OCR character")?, "box": rect(c.get("b"))?}))).collect::<Result<Vec<_>, String>>()?;
            out.insert("chars".into(), Value::Array(chars));
        }
        Ok(Value::Object(out))
    }).collect::<Result<Vec<_>, String>>()?;
    let mut out = serde_json::Map::new();
    out.insert("lines".into(), Value::Array(lines));
    if let Some(paper) = result.get("paper").filter(|value| value.is_number()) { out.insert("paperShare".into(), paper.clone()); }
    Ok(Value::Object(out))
}

fn rect(value: Option<&Value>) -> Result<Value, String> {
    let values = value.and_then(Value::as_array).filter(|a| a.len() == 4).ok_or("invalid OCR rectangle")?;
    if values.iter().any(|value| !value.is_number()) { return Err("invalid OCR rectangle".into()); }
    Ok(json!({"x0": values[0], "y0": values[1], "x1": values[2], "y1": values[3]}))
}

pub(crate) fn dispatch(request_id: u64, operation: &str, input: &Value, _original_input: &Value, cancel: &CancelToken) -> Result<Value, String> {
    if cancel.is_cancelled() { return Err("PDF callback cancelled".into()); }
    if operation == "pdf.ocr" {
        let Some(path) = helper()? else { return Ok(Value::Null) };
        let encoded = string(input, "pngBase64")?;
        if encoded.len() > (MAX_BUFFER + 2) / 3 * 4 { return Err("OCR input is too large".into()); }
        let png = STANDARD.decode(encoded).map_err(|_| "invalid OCR input")?;
        if png.len() > MAX_BUFFER { return Err("OCR input is too large".into()); }
        return recognize(&path, png, cancel);
    }
    let mut all = requests().lock().map_err(|_| "PDF font state was poisoned")?;
    if cancel.is_cancelled() { return Err("PDF callback cancelled".into()); }
    let request = all.entry(request_id).or_insert_with(|| RequestFonts { locator: FontLocator::new(font_env()), faces: HashMap::new(), next: 1 });
    match operation {
        "pdf.fontResolve" => {
            let family = string(input, "family")?;
            let ps_name = string(input, "psName")?;
            let Some(bytes) = request.locator.find_system_font(ps_name, family) else { return Ok(Value::Null) };
            let id = request.next;
            request.next = id.checked_add(1).ok_or("font descriptors exhausted")?;
            request.faces.insert(id, bytes);
            Ok(Value::from(id))
        }
        "pdf.fontFindCovering" => {
            let Some(bytes) = request.locator.find_font_covering(string(input, "text")?) else { return Ok(Value::Null) };
            let id = request.next;
            request.next = id.checked_add(1).ok_or("font descriptors exhausted")?;
            request.faces.insert(id, bytes);
            Ok(Value::from(id))
        }
        "pdf.fontRead" | "pdf.fontCoverage" | "pdf.fontTtc" => {
            let bytes = request.faces.get(&descriptor(input)?).ok_or("unknown font descriptor")?;
            match operation {
                "pdf.fontRead" => Ok(Value::String(STANDARD.encode(bytes))),
                "pdf.fontCoverage" => Ok(Value::Bool(font_covers_text(bytes, string(input, "text")?))),
                _ => Ok(json!({"standalone": true, "trueType": is_truetype(bytes)})),
            }
        }
        _ => Err("unknown PDF callback".into()),
    }
}

pub(crate) fn close_request(request_id: u64) {
    if let Ok(mut requests) = requests().lock() { requests.remove(&request_id); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_descriptors_are_request_scoped_and_revoked() {
        let request_id = u64::MAX - 10;
        let other_id = u64::MAX - 11;
        requests().lock().unwrap().insert(request_id, RequestFonts {
            locator: FontLocator::new(font_env()), faces: HashMap::from([(1, vec![1, 2, 3])]), next: 2,
        });
        let cancel = CancelToken::default();
        let input = json!({"descriptor": 1});
        assert_eq!(dispatch(request_id, "pdf.fontRead", &input, &Value::Null, &cancel).unwrap(), "AQID");
        assert!(dispatch(other_id, "pdf.fontRead", &input, &Value::Null, &cancel).is_err());
        close_request(request_id);
        assert!(dispatch(request_id, "pdf.fontRead", &input, &Value::Null, &cancel).is_err());
        close_request(request_id);
        close_request(other_id);
    }

    #[test]
    fn cancelled_requests_cannot_use_a_font_descriptor() {
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(dispatch(u64::MAX - 12, "pdf.fontRead", &json!({"descriptor": 1}), &Value::Null, &cancel).is_err());
    }
}
