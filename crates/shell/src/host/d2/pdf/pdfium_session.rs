//! Request-scoped PDFium primitive transport. Runs only on native worker threads.
use base64::Engine;
use serde_json::{Value, json};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

const SERVICE: &str = "pdf.pdfiumSession";
const MAX_ID: u64 = 9_007_199_254_740_991;
static NEXT: AtomicU64 = AtomicU64::new(1);
static SESSIONS: LazyLock<Mutex<HashMap<u64, Arc<Transport>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn wire_number(value: f64) -> Value {
    let special = if value.is_nan() {
        Some("NaN")
    } else if value == f64::INFINITY {
        Some("Infinity")
    } else if value == f64::NEG_INFINITY {
        Some("-Infinity")
    } else if value == 0.0 && value.is_sign_negative() {
        Some("-0")
    } else {
        None
    };
    match special {
        Some(value) => json!({"type":"number","value":value}),
        None => json!(value),
    }
}
pub(crate) fn read_number(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    if value.get("type").and_then(Value::as_str) != Some("number") {
        return None;
    }
    match value.get("value").and_then(Value::as_str)? {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "-0" => Some(-0.0),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PdfiumError {
    pub message: String,
    pub code: Option<u32>,
}
impl std::fmt::Display for PdfiumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for PdfiumError {}
impl From<String> for PdfiumError {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}
struct Transport {
    commands: Mutex<mpsc::Receiver<Value>>,
    results: mpsc::Sender<Result<Value, PdfiumError>>,
    request_id: Mutex<Option<u64>>,
    cancel: CancelToken,
}

pub(crate) struct PdfiumSession {
    id: u64,
    document: u64,
    commands: mpsc::Sender<Value>,
    results: mpsc::Receiver<Result<Value, PdfiumError>>,
    cancel: CancelToken,
    _subscription: CancelSubscription,
}
impl PdfiumSession {
    pub(crate) fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, PdfiumError> {
        Self::open_with_cancel(bytes, password, CancelToken::default())
    }
    pub(crate) fn open_with_cancel(
        bytes: &[u8],
        password: Option<&str>,
        cancel: CancelToken,
    ) -> Result<Self, PdfiumError> {
        let internal = CancelToken::default();
        let forwarded = internal.clone();
        let subscription = cancel.on_cancel(move || forwarded.cancel());
        let cancel = internal;
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| PdfiumError::from("PDFium session identities exhausted".to_owned()))?;
        if id > MAX_ID {
            return Err(PdfiumError::from(
                "PDFium session identities exhausted".to_owned(),
            ));
        }
        let (commands, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        let transport = Arc::new(Transport {
            commands: Mutex::new(receiver),
            results: sender.clone(),
            request_id: Mutex::new(None),
            cancel: cancel.clone(),
        });
        SESSIONS
            .lock()
            .map_err(|_| PdfiumError::from("PDFium session registry poisoned".to_owned()))?
            .insert(id, transport);
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            let result = crate::engine_host::call_without_timeout_cancellable(
                SERVICE,
                &json!({"session":id}),
                &worker_cancel,
            );
            if let Err(error) = result {
                let _ = sender.send(Err(PdfiumError::from(error.message)));
            }
            if let Ok(mut sessions) = SESSIONS.lock() {
                sessions.remove(&id);
            }
        });
        let mut session = Self {
            id,
            document: 0,
            commands,
            results,
            cancel,
            _subscription: subscription,
        };
        let value=session.exchange(json!({"op":"open","bytes":{"type":"bytes","base64":base64::engine::general_purpose::STANDARD.encode(bytes)},"password":password}))?;
        session.document = value
            .get("ref")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .ok_or_else(|| PdfiumError::from("invalid PDFium document handle".to_owned()))?;
        Ok(session)
    }
    pub(crate) fn document(&self) -> u64 {
        self.document
    }
    fn exchange(&mut self, command: Value) -> Result<Value, PdfiumError> {
        if self.cancel.is_cancelled() {
            return Err(PdfiumError::from("PDFium session cancelled".to_owned()));
        }
        self.commands
            .send(command)
            .map_err(|_| PdfiumError::from("PDFium session closed".to_owned()))?;
        loop {
            match self.results.recv_timeout(Duration::from_millis(100)) {
                Ok(_) if self.cancel.is_cancelled() => {
                    return Err(PdfiumError::from("PDFium session cancelled".to_owned()));
                }
                Ok(value) => return value,
                Err(mpsc::RecvTimeoutError::Timeout) if !self.cancel.is_cancelled() => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(PdfiumError::from("PDFium session cancelled".to_owned()));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(PdfiumError::from("PDFium session closed".to_owned()));
                }
            }
        }
    }
    pub(crate) fn call(&mut self, name: &str, args: &[Value]) -> Result<Value, PdfiumError> {
        self.exchange(json!({"op":"call","name":name,"args":args}))
    }
    pub(crate) fn batch(
        &mut self,
        calls: &[(&str, Vec<Value>)],
    ) -> Result<Vec<Value>, PdfiumError> {
        let commands: Vec<_> = calls
            .iter()
            .map(|(name, args)| json!({"name":name,"args":args}))
            .collect();
        self.exchange(json!({"op":"batch","calls":commands}))?
            .as_array()
            .cloned()
            .ok_or_else(|| PdfiumError::from("invalid PDFium batch response".to_owned()))
    }
    pub(crate) fn supports(&mut self, name: &str) -> Result<bool, PdfiumError> {
        self.exchange(json!({"op":"supports","name":name}))?
            .as_bool()
            .ok_or_else(|| PdfiumError::from("invalid PDFium capability response".to_owned()))
    }
    pub(crate) fn alloc(&mut self, size: usize) -> Result<u64, PdfiumError> {
        self.exchange(json!({"op":"alloc","size":size}))?
            .get("ref")
            .and_then(Value::as_u64)
            .ok_or_else(|| PdfiumError::from("invalid PDFium buffer handle".to_owned()))
    }
    pub(crate) fn read(
        &mut self,
        buffer: u64,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, PdfiumError> {
        let value = self.exchange(json!({"op":"read","ref":buffer,"offset":offset,"len":len}))?;
        if value.get("type").and_then(Value::as_str) != Some("bytes") {
            return Err(PdfiumError::from("invalid PDFium byte response".to_owned()));
        }
        let bytes = value
            .get("base64")
            .and_then(Value::as_str)
            .ok_or_else(|| PdfiumError::from("invalid PDFium byte response".to_owned()))?;
        base64::engine::general_purpose::STANDARD
            .decode(bytes)
            .map_err(|e| PdfiumError::from(e.to_string()))
    }
    pub(crate) fn write(
        &mut self,
        buffer: u64,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), PdfiumError> {
        self.exchange(json!({"op":"write","ref":buffer,"offset":offset,"bytes":{"type":"bytes","base64":base64::engine::general_purpose::STANDARD.encode(bytes)}}))?;
        Ok(())
    }
    pub(crate) fn free(&mut self, buffer: u64) -> Result<(), PdfiumError> {
        self.exchange(json!({"op":"free","ref":buffer}))?;
        Ok(())
    }
}

pub(crate) fn render_page_png(
    bytes: &[u8],
    password: Option<&str>,
    page_index: usize,
    scale: f64,
    cancel: &CancelToken,
) -> Result<Option<Vec<u8>>, PdfiumError> {
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    if !scale.is_finite() || scale <= 0.0 || page_index as u128 > MAX_ID as u128 {
        return Err(PdfiumError::from("invalid PDFium page or scale".to_owned()));
    }
    let mut session = PdfiumSession::open_with_cancel(bytes, password, cancel.clone())?;
    let document = json!({"ref":session.document()});
    let page = session.call("FPDF_LoadPage", &[document, json!(page_index)])?;
    if page == json!(0) {
        return Ok(None);
    }
    let page_id = page
        .get("ref")
        .and_then(Value::as_u64)
        .ok_or_else(|| PdfiumError::from("invalid PDFium page".to_owned()))?;
    let page = json!({"ref":page_id});
    let width = session
        .call("FPDF_GetPageWidthF", &[page.clone()])?
        .as_f64()
        .ok_or_else(|| PdfiumError::from("invalid PDF page width".to_owned()))?;
    let height = session
        .call("FPDF_GetPageHeightF", &[page.clone()])?
        .as_f64()
        .ok_or_else(|| PdfiumError::from("invalid PDF page height".to_owned()))?;
    let (w, h) = ((width * scale).ceil(), (height * scale).ceil());
    if !w.is_finite()
        || !h.is_finite()
        || w < 1.0
        || h < 1.0
        || w > 32768.0
        || h > 32768.0
        || w * h > 67_108_864.0
    {
        return Err(PdfiumError::from(
            "invalid PDFium render dimensions".to_owned(),
        ));
    }
    let (w, h) = (w as u32, h as u32);
    let bitmap = session.call("FPDFBitmap_Create", &[json!(w), json!(h), json!(1)])?;
    if bitmap == json!(0) {
        return Ok(None);
    }
    let bitmap_id = bitmap
        .get("ref")
        .and_then(Value::as_u64)
        .ok_or_else(|| PdfiumError::from("invalid PDFium bitmap".to_owned()))?;
    let bitmap = json!({"ref":bitmap_id});
    session.call(
        "FPDFBitmap_FillRect",
        &[
            bitmap.clone(),
            json!(0),
            json!(0),
            json!(w),
            json!(h),
            json!(u32::MAX),
        ],
    )?;
    session.call(
        "FPDF_RenderPageBitmap",
        &[
            bitmap.clone(),
            page,
            json!(0),
            json!(0),
            json!(w),
            json!(h),
            json!(0),
            json!(0),
        ],
    )?;
    let stride = session
        .call("FPDFBitmap_GetStride", &[bitmap.clone()])?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| PdfiumError::from("invalid PDFium bitmap stride".to_owned()))?;
    let format = session
        .call("FPDFBitmap_GetFormat", &[bitmap.clone()])?
        .as_u64()
        .ok_or_else(|| PdfiumError::from("invalid PDFium bitmap format".to_owned()))?;
    let memory = session
        .call("FPDFBitmap_GetBuffer", &[bitmap])?
        .get("ref")
        .and_then(Value::as_u64)
        .ok_or_else(|| PdfiumError::from("invalid PDFium bitmap buffer".to_owned()))?;
    let len = stride
        .checked_mul(h as usize)
        .ok_or_else(|| PdfiumError::from("invalid PDFium bitmap length".to_owned()))?;
    let pixels = session.read(memory, 0, len)?;
    let channels = match format {
        1 => 1,
        2 => 3,
        3 | 4 => 4,
        _ => {
            return Err(PdfiumError::from(
                "unsupported PDFium bitmap format".to_owned(),
            ));
        }
    };
    if stride < (w as usize) * channels {
        return Err(PdfiumError::from("invalid PDFium bitmap stride".to_owned()));
    }
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for row in pixels.chunks_exact(stride) {
        if cancel.is_cancelled() {
            return Err(PdfiumError::from("PDFium session cancelled".to_owned()));
        }
        for pixel in row[..(w as usize) * channels].chunks_exact(channels) {
            match format {
                1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
                2 | 3 => rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
                4 => rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]),
                _ => unreachable!(),
            }
        }
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgba, w, h, ExtendedColorType::Rgba8)
        .map_err(|e| PdfiumError::from(e.to_string()))?;
    Ok(Some(png))
}
impl Drop for PdfiumSession {
    fn drop(&mut self) {
        if self.commands.send(json!({"op":"close"})).is_ok() {
            let cancel = self.cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                cancel.cancel();
            });
        } else {
            self.cancel.cancel();
        }
        if let Ok(mut sessions) = SESSIONS.lock() {
            sessions.remove(&self.id);
        }
    }
}

pub(crate) fn dispatch(
    request_id: u64,
    operation: &str,
    input: &Value,
    original: &Value,
    cancel: &CancelToken,
) -> Result<Value, String> {
    if operation != "pdfium.next" {
        return Err("invalid PDFium callback".into());
    }
    let id = original
        .get("session")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .ok_or("invalid PDFium session")?;
    let transport = SESSIONS
        .lock()
        .map_err(|_| "PDFium session registry poisoned")?
        .get(&id)
        .cloned()
        .ok_or("PDFium session closed")?;
    {
        let mut owner = transport
            .request_id
            .lock()
            .map_err(|_| "PDFium session owner poisoned")?;
        match *owner {
            Some(owner) if owner != request_id => {
                return Err("PDFium session belongs to another request".into());
            }
            None => *owner = Some(request_id),
            _ => {}
        }
    }
    if let Some(response) = input.get("response") {
        let result = if let Some(error) = response.get("error") {
            Err(PdfiumError {
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("PDFium operation failed")
                    .to_owned(),
                code: error
                    .get("code")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
            })
        } else {
            Ok(response
                .get("ok")
                .cloned()
                .ok_or("invalid PDFium response")?)
        };
        transport
            .results
            .send(result)
            .map_err(|_| "PDFium client closed")?;
    }
    let receiver = transport
        .commands
        .lock()
        .map_err(|_| "PDFium session queue poisoned")?;
    loop {
        if cancel.is_cancelled() || transport.cancel.is_cancelled() {
            return Err("PDFium session cancelled".into());
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => return Ok(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("PDFium client closed".into()),
        }
    }
}
pub(crate) fn close_request(request_id: u64) {
    if let Ok(sessions) = SESSIONS.lock() {
        for transport in sessions.values() {
            if transport
                .request_id
                .lock()
                .ok()
                .is_some_and(|owner| *owner == Some(request_id))
            {
                transport.cancel.cancel();
                let _ = transport
                    .results
                    .send(Err(PdfiumError::from("PDFium session closed".to_owned())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callbacks_bind_one_request_and_deliver_typed_response() {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = mpsc::channel();
        let (result_send, result_receive) = mpsc::channel();
        let transport = Arc::new(Transport {
            commands: Mutex::new(receive),
            results: result_send,
            request_id: Mutex::new(None),
            cancel: CancelToken::default(),
        });
        SESSIONS.lock().unwrap().insert(id, transport);
        send.send(json!({"op":"alloc","size":16})).unwrap();
        let cancel = CancelToken::default();
        assert_eq!(
            dispatch(
                91,
                "pdfium.next",
                &json!({}),
                &json!({"session":id}),
                &cancel
            )
            .unwrap(),
            json!({"op":"alloc","size":16})
        );
        send.send(json!({"op":"close"})).unwrap();
        assert_eq!(
            dispatch(
                91,
                "pdfium.next",
                &json!({"response":{"ok":{"ref":7}}}),
                &json!({"session":id}),
                &cancel
            )
            .unwrap(),
            json!({"op":"close"})
        );
        assert_eq!(result_receive.recv().unwrap().unwrap(), json!({"ref":7}));
        assert!(
            dispatch(
                92,
                "pdfium.next",
                &json!({}),
                &json!({"session":id}),
                &cancel
            )
            .unwrap_err()
            .contains("another request")
        );
        SESSIONS.lock().unwrap().remove(&id);
    }
    #[test]
    fn cancellation_is_forwarded_without_cancelling_caller_on_drop() {
        let external = CancelToken::default();
        let internal = CancelToken::default();
        let forwarded = internal.clone();
        let subscription = external.on_cancel(move || forwarded.cancel());
        let (send, _receive) = mpsc::channel();
        let (_result, results) = mpsc::channel();
        let session = PdfiumSession {
            id: 0,
            document: 0,
            commands: send,
            results,
            cancel: internal,
            _subscription: subscription,
        };
        drop(session);
        assert!(!external.is_cancelled());
    }
    #[test]
    fn cancellation_rejects_an_outstanding_exchange() {
        let external = CancelToken::default();
        let internal = CancelToken::default();
        let forwarded = internal.clone();
        let subscription = external.on_cancel(move || forwarded.cancel());
        let (send, receive) = mpsc::channel();
        let (result, results) = mpsc::channel();
        let mut session = PdfiumSession {
            id: 0,
            document: 0,
            commands: send,
            results,
            cancel: internal,
            _subscription: subscription,
        };
        let thread = std::thread::spawn(move || session.exchange(json!({"op":"alloc","size":4})));
        assert_eq!(receive.recv().unwrap(), json!({"op":"alloc","size":4}));
        external.cancel();
        let _ = result.send(Ok(json!({"ref":7})));
        assert!(
            thread
                .join()
                .unwrap()
                .unwrap_err()
                .message
                .contains("cancelled")
        );
    }
    #[test]
    fn numeric_wire_preserves_special_values() {
        assert!(read_number(&wire_number(f64::NAN)).unwrap().is_nan());
        assert_eq!(
            read_number(&wire_number(f64::INFINITY)),
            Some(f64::INFINITY)
        );
        assert!(read_number(&wire_number(-0.0)).unwrap().is_sign_negative());
    }
}
