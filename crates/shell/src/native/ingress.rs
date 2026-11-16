//! Document nonce redemption and the per-document reply outbox.
//!
//! The bridge init script generates a 16 byte nonce per document and sends it in the
//! `x-seuoffice-doc` header of every authenticated call. A header is only an index:
//! it names the committed document of the browser the request arrived on, and means
//! nothing for any other browser or any revoked document.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::http::HeaderMap;

use super::document::DocumentContext;
use super::ids::DocumentId;

/// Header the bridge attaches to every authenticated IPC request.
pub const DOCUMENT_HEADER: &str = "x-seuoffice-doc";

/// Length of a document nonce in bytes.
pub const NONCE_LEN: usize = 16;

/// Decode a 32 digit hex nonce. Anything else is not a nonce.
pub fn parse_nonce(text: &str) -> Option<[u8; NONCE_LEN]> {
    let bytes = text.as_bytes();
    if bytes.len() != NONCE_LEN * 2 {
        return None;
    }
    let mut nonce = [0u8; NONCE_LEN];
    for (slot, pair) in nonce.iter_mut().zip(bytes.chunks_exact(2)) {
        *slot = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(nonce)
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The nonce of a request, when it carries a well-formed one.
pub fn nonce_of(headers: &HeaderMap) -> Option<[u8; NONCE_LEN]> {
    parse_nonce(headers.get(DOCUMENT_HEADER)?.to_str().ok()?)
}

pub(crate) fn nonce_eq(left: &[u8; NONCE_LEN], right: &[u8; NONCE_LEN]) -> bool {
    left.iter().zip(right).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// One frame the bridge pulls from the outbox (`seuoffice-ipc://localhost/outbox/<id>`).
#[derive(Debug)]
pub struct ReplyFrame {
    pub callback: u32,
    /// `Some((index, end))` for a channel message, `None` for a plain callback reply.
    pub channel: Option<(u64, bool)>,
    pub raw: bool,
    pub body: Vec<u8>,
}

const MAX_FRAMES: usize = 8192;
const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
struct Queue {
    frames: VecDeque<ReplyFrame>,
    bytes: usize,
    wake_sent: bool,
}

struct Outbox {
    document: DocumentId,
    revoked: Arc<AtomicBool>,
    queue: Mutex<Queue>,
    wake: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
}

/// Outbound frames for one document. Revocation clears the queue; a frame for a
/// revoked document is refused. Large binary replies stay off `evaluateJavaScript`:
/// a tiny wake tells the bridge to pull.
#[derive(Clone)]
pub struct ReplySink(Arc<Outbox>);

impl ReplySink {
    pub(crate) fn new(
        document: DocumentId, revoked: Arc<AtomicBool>,
        wake: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Self {
        Self(Arc::new(Outbox { document, revoked, queue: Mutex::default(), wake }))
    }

    pub fn document_id(&self) -> DocumentId {
        self.0.document
    }

    pub fn channel_queue(&self) -> Option<ChannelQueue> {
        (!self.0.revoked.load(Ordering::Acquire)).then(|| ChannelQueue { sink: self.clone() })
    }

    pub fn send(&self, callback: u32, raw: bool, body: Vec<u8>) -> Result<(), String> {
        self.push(ReplyFrame { callback, channel: None, raw, body })
    }

    pub fn send_channel(
        &self, callback: u32, index: u64, raw: bool, body: Vec<u8>, end: bool,
    ) -> Result<(), String> {
        self.push(ReplyFrame { callback, channel: Some((index, end)), raw, body })
    }

    fn push(&self, frame: ReplyFrame) -> Result<(), String> {
        let outbox = &self.0;
        if outbox.revoked.load(Ordering::Acquire) {
            return Err("Native document is no longer available.".into());
        }
        let wake = {
            let mut queue = outbox.queue.lock().map_err(|_| "Native reply queue was poisoned.")?;
            if queue.frames.len() >= MAX_FRAMES
                || queue.bytes.saturating_add(frame.body.len()) > MAX_BYTES
            {
                return Err("Native reply queue is full.".into());
            }
            queue.bytes += frame.body.len();
            queue.frames.push_back(frame);
            !std::mem::replace(&mut queue.wake_sent, true)
        };
        if wake && let Err(error) = (outbox.wake)() {
            if let Ok(mut queue) = outbox.queue.lock() {
                queue.wake_sent = false;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Take every queued frame. Called by the outbox scheme handler for the redeemed document.
    pub fn drain(&self) -> Vec<ReplyFrame> {
        let mut queue = self.0.queue.lock().unwrap_or_else(|error| error.into_inner());
        queue.wake_sent = false;
        queue.bytes = 0;
        queue.frames.drain(..).collect()
    }

    pub(crate) fn close(&self) {
        let mut queue = self.0.queue.lock().unwrap_or_else(|error| error.into_inner());
        queue.frames.clear();
        queue.bytes = 0;
    }
}

/// Prepared outside owner gates; enqueueing is not an effect on the document.
#[derive(Clone)]
pub struct ChannelQueue {
    sink: ReplySink,
}

impl ChannelQueue {
    pub fn enqueue(
        &self, callback: u32, index: u64, raw: bool, body: Vec<u8>, end: bool,
    ) -> Result<(), String> {
        self.sink.send_channel(callback, index, raw, body, end)
    }
}

/// The document a request authenticated as, or `None`: absent header, malformed nonce,
/// unknown label, iframe (no nonce), unregistered or revoked document.
pub fn redeem(
    native: &super::Native, webview_label: &str, headers: &HeaderMap,
) -> Option<DocumentContext> {
    let nonce = nonce_of(headers)?;
    native.browser(webview_label)?.core.redeem(&nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_parsing_accepts_only_32_hex_digits() {
        let nonce = parse_nonce("00ff10Ab00000000000000000000000f").unwrap();
        assert_eq!(nonce[0], 0x00);
        assert_eq!(nonce[1], 0xff);
        assert_eq!(nonce[3], 0xab);
        assert_eq!(nonce[15], 0x0f);
        assert!(parse_nonce("00ff").is_none());
        assert!(parse_nonce("zz000000000000000000000000000000").is_none());
        assert!(parse_nonce("").is_none());
    }

    fn sink() -> (ReplySink, Arc<AtomicBool>, Arc<Mutex<u32>>) {
        let revoked = Arc::new(AtomicBool::new(false));
        let wakes = Arc::new(Mutex::new(0u32));
        let counted = wakes.clone();
        let sink = ReplySink::new(
            DocumentId::mint().unwrap(),
            revoked.clone(),
            Box::new(move || {
                *counted.lock().unwrap() += 1;
                Ok(())
            }),
        );
        (sink, revoked, wakes)
    }

    #[test]
    fn wake_is_sent_once_until_the_outbox_is_drained() {
        let (sink, _, wakes) = sink();
        sink.send(1, false, b"a".to_vec()).unwrap();
        sink.send_channel(2, 0, true, b"b".to_vec(), false).unwrap();
        assert_eq!(*wakes.lock().unwrap(), 1);
        let frames = sink.drain();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].channel, Some((0, false)));
        sink.send(3, false, Vec::new()).unwrap();
        assert_eq!(*wakes.lock().unwrap(), 2);
    }

    #[test]
    fn a_revoked_document_refuses_frames_and_drops_queued_ones() {
        let (sink, revoked, _) = sink();
        sink.send(1, false, b"a".to_vec()).unwrap();
        revoked.store(true, Ordering::Release);
        sink.close();
        assert!(sink.send(2, false, Vec::new()).is_err());
        assert!(sink.channel_queue().is_none());
        assert!(sink.drain().is_empty());
    }
}
