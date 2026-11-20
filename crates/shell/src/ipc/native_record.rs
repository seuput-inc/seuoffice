//! The real-app record of the native-only channels: `<out>/native-ipc.jsonl` of a `--e2e` run.
//!
//! The channels of the native namespace (`shell_contract::CHANNELS`, the overlay of
//! tools/shell-inventory/native-channels.ts) have no Electron handler to compare with, so their
//! evidence is what the dispatcher of the real app saw. With `--e2e` every call of one of them
//! leaves two lines: a `call` (the view and surface that sent it and the shape of its
//! arguments) and a `reply` (the shape of the answer, or the refusal text). A text value is
//! written as its length only, so a receipt or a token never leaves the process.
//! crates/shell/ci/native-host-check.mjs reads the file. Without `--e2e` nothing here runs.
//!
//! The record changes no call. The one thing that does, the replay markers of the real-app
//! test build (replay.rs), is compiled only with the `e2e-replay` feature: a build without it
//! hands every argument to its handler as the renderer sent it.

#[cfg(feature = "e2e-replay")]
mod replay;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use shell_contract::ids::RequestId;
use shell_contract::ipc::{ChannelKind, Reply, Sender};
use shell_contract::value::IpcValue;
use shell_contract::{CHANNELS, ChannelNamespace};

const RECORD_FILE: &str = "native-ipc.jsonl";
/// The outcomes of `<app>:saved-input-applied`: the only texts written as they are
const OUTCOME_WORDS: [&str; 3] = ["applied", "recovered", "failed"];
/// A text argument shorter than this is not looked for in a refusal text
const SCRUBBED_FROM: usize = 6;

pub(super) type Deliver = Box<dyn FnOnce(Reply) + Send>;

/// Who sent one message and how
pub(super) struct Call<'a> {
    pub(super) sender: Sender,
    pub(super) kind: ChannelKind,
    pub(super) request: Option<RequestId>,
    /// the call carries the native context of an authenticated document
    pub(super) native: bool,
    pub(super) channel: &'a str,
}

/// What `dispatch` goes on with: the arguments and a delivery that also records the reply.
/// Anything but a native-only channel of a `--e2e` run passes through untouched. With the
/// `e2e-replay` feature the arguments have their replay markers filled in, and the answer is
/// `None` when a marker named a value nothing recorded: that call is answered here.
pub(super) fn observe(
    call: &Call<'_>,
    args: Vec<IpcValue>,
    deliver: Deliver,
) -> Option<(Vec<IpcValue>, Deliver)> {
    match RECORDER.as_ref() {
        Some(recorder) if is_native(call.channel) => recorder.observe(call, args, deliver),
        _ => Some((args, deliver)),
    }
}

/// The recorder of this process; `None` without `--e2e`
static RECORDER: LazyLock<Option<Arc<Recorder>>> = LazyLock::new(|| {
    let request = crate::e2e::request()?;
    #[cfg(feature = "e2e-replay")]
    replay::announce();
    Some(Arc::new(Recorder::new(request.out.join(RECORD_FILE))))
});

fn is_native(channel: &str) -> bool {
    CHANNELS
        .iter()
        .any(|spec| spec.name == channel && spec.namespace == ChannelNamespace::Native)
}

#[derive(Default)]
struct State {
    file: Option<File>,
    /// the file could not be opened: said once, then nothing is written
    unwritable: bool,
    seq: u64,
    #[cfg(feature = "e2e-replay")]
    answered: replay::Memory,
}

struct Recorder {
    path: PathBuf,
    started: Instant,
    state: Mutex<State>,
}

impl Recorder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: Instant::now(),
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn write(&self, state: &mut State, line: &Value) {
        if state.file.is_none() && !state.unwritable {
            if let Some(folder) = self.path.parent() {
                let _ = std::fs::create_dir_all(folder);
            }
            let opened = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path);
            match opened {
                Ok(file) => state.file = Some(file),
                Err(error) => {
                    state.unwritable = true;
                    eprintln!("shell: e2e: cannot open {}: {error}", self.path.display());
                }
            }
        }
        let Some(file) = state.file.as_mut() else {
            return;
        };
        let mut text = line.to_string();
        text.push('\n');
        if let Err(error) = file.write_all(text.as_bytes()) {
            eprintln!("shell: e2e: cannot write {}: {error}", self.path.display());
        }
    }

    fn observe(
        self: &Arc<Self>,
        call: &Call<'_>,
        args: Vec<IpcValue>,
        deliver: Deliver,
    ) -> Option<(Vec<IpcValue>, Deliver)> {
        let view = call.sender.view;
        let mut state = self.lock();
        state.seq += 1;
        let seq = state.seq;
        let mut line = json!({
            "event": "call",
            "seq": seq,
            "ms": self.elapsed_ms(),
            "channel": call.channel,
            "kind": kind_name(call.kind),
            "request": call.request.map(|request| request.0),
            "view": view.0,
            "window": call.sender.window.0,
            "surface": format!("{:?}", call.sender.surface),
            "native": call.native,
        });
        #[cfg(feature = "e2e-replay")]
        let args = match state.answered.resolve(&args, view, call.channel) {
            Ok((args, replayed)) => {
                if !replayed.is_empty() {
                    line["replayed"] = Value::Array(replayed);
                }
                args
            }
            Err(message) => {
                line["args"] = shapes(&args);
                self.write(&mut state, &line);
                let refusal = json!({
                    "event": "reply",
                    "seq": seq,
                    "ms": self.elapsed_ms(),
                    "ok": false,
                    "error": message,
                    "unresolvedReplay": true,
                });
                self.write(&mut state, &refusal);
                drop(state);
                deliver(Err(shell_contract::error::IpcError {
                    name: Some("Error".into()),
                    message,
                }));
                return None;
            }
        };
        line["args"] = shapes(&args);
        self.write(&mut state, &line);
        drop(state);
        let recorder = Arc::clone(self);
        let kept = args.clone();
        #[cfg(feature = "e2e-replay")]
        let channel = call.channel.to_owned();
        let recorded: Deliver = Box::new(move |reply| {
            recorder.replied(seq, &kept, &reply);
            #[cfg(feature = "e2e-replay")]
            recorder.lock().answered.note(view, channel, kept, &reply);
            deliver(reply);
        });
        Some((args, recorded))
    }

    fn replied(&self, seq: u64, args: &[IpcValue], reply: &Reply) {
        let mut state = self.lock();
        let mut line = json!({ "event": "reply", "seq": seq, "ms": self.elapsed_ms() });
        match reply {
            Ok(value) => {
                line["ok"] = Value::Bool(true);
                line["value"] = shape(value);
            }
            Err(error) => {
                line["ok"] = Value::Bool(false);
                line["error"] = Value::from(scrubbed(&error.message, args));
            }
        }
        self.write(&mut state, &line);
    }
}

fn kind_name(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Invoke => "invoke",
        ChannelKind::Send => "send",
        ChannelKind::SendSync => "sendSync",
        ChannelKind::Event => "event",
        ChannelKind::Port => "port",
    }
}

fn shapes(args: &[IpcValue]) -> Value {
    Value::Array(args.iter().map(shape).collect())
}

/// The type of a value and of everything inside it; a text only as its length
fn shape(value: &IpcValue) -> Value {
    match value {
        IpcValue::Undefined => json!({ "type": "undefined" }),
        IpcValue::Null => json!({ "type": "null" }),
        IpcValue::Bool(flag) => json!({ "type": "boolean", "value": flag }),
        IpcValue::Number(_) => json!({ "type": "number" }),
        IpcValue::String(text) if OUTCOME_WORDS.contains(&text.as_str()) => {
            json!({ "type": "string", "length": text.chars().count(), "word": text })
        }
        IpcValue::String(text) => json!({ "type": "string", "length": text.chars().count() }),
        IpcValue::Bytes(_, bytes) => json!({ "type": "bytes", "length": bytes.len() }),
        IpcValue::Date(_) => json!({ "type": "date" }),
        IpcValue::Array(items) => json!({ "type": "array", "items": shapes(items) }),
        IpcValue::Object(fields) => {
            let mut shaped = Map::new();
            for (key, field) in fields {
                shaped.insert(key.clone(), shape(field));
            }
            json!({ "type": "object", "fields": shaped })
        }
    }
}

/// A refusal text without the text arguments of its call: a handler may quote what it refused
fn scrubbed(message: &str, args: &[IpcValue]) -> String {
    fn texts<'a>(value: &'a IpcValue, found: &mut Vec<&'a str>) {
        match value {
            IpcValue::String(text) if is_secret(text) => found.push(text),
            IpcValue::Array(items) => items.iter().for_each(|item| texts(item, found)),
            IpcValue::Object(fields) => fields.iter().for_each(|(_, field)| texts(field, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for arg in args {
        texts(arg, &mut found);
    }
    let mut text = message.to_owned();
    for secret in found {
        text = text.replace(secret, "<text>");
    }
    text
}

/// A text long enough to be a receipt, and not one of the words written as they are
fn is_secret(text: &str) -> bool {
    text.len() >= SCRUBBED_FROM && !OUTCOME_WORDS.contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::Surface;
    use shell_contract::error::IpcError;
    use shell_contract::ids::{ViewId, WindowId};
    use std::sync::mpsc;

    const CHANNEL: &str = "docs:recovery-initialized";

    fn recorder(name: &str) -> (Arc<Recorder>, PathBuf) {
        let unique = format!("seuoffice-native-record-{name}-{}", std::process::id());
        let folder = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&folder);
        (Arc::new(Recorder::new(folder.join(RECORD_FILE))), folder)
    }

    fn call(view: u32, request: u64) -> Call<'static> {
        let sender = Sender {
            view: ViewId(view),
            window: WindowId(1),
            surface: Surface::Docs,
        };
        Call {
            sender,
            kind: ChannelKind::Invoke,
            request: Some(RequestId(request)),
            native: true,
            channel: CHANNEL,
        }
    }

    fn text(value: &str) -> IpcValue {
        IpcValue::String(value.to_owned())
    }

    /// What a native-host spec sends in place of a receipt: the value at `path` of an answered
    /// call, by name
    fn marker(of: &str, path: &[IpcValue]) -> IpcValue {
        IpcValue::Object(vec![
            ("e2eReplay".to_owned(), text(of)),
            ("path".to_owned(), IpcValue::Array(path.to_vec())),
        ])
    }

    fn refused(message: &str) -> Reply {
        Err(IpcError {
            name: Some("Error".into()),
            message: message.to_owned(),
        })
    }

    /// Sends one call through the recorder and answers it as the handler would: what the
    /// handler got (`None` when the recorder answered), and what the renderer was told
    fn exchange(
        recorder: &Arc<Recorder>,
        call: &Call<'_>,
        args: Vec<IpcValue>,
        answer: Reply,
    ) -> (Option<Vec<IpcValue>>, Reply) {
        let (send, receive) = mpsc::channel();
        let deliver: Deliver = Box::new(move |reply| send.send(reply).unwrap());
        let observed = recorder.observe(call, args, deliver);
        let seen = observed.map(|(args, deliver)| {
            deliver(answer);
            args
        });
        (seen, receive.try_recv().unwrap())
    }

    fn lines(folder: &std::path::Path) -> Vec<Value> {
        let written = std::fs::read_to_string(folder.join(RECORD_FILE)).unwrap();
        written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn only_the_native_namespace_is_recorded() {
        assert!(is_native(CHANNEL));
        assert!(is_native("workbook:saved-open-result"));
        assert!(!is_native("docs:print-pdf-buffer"));
        assert!(!is_native("no:such-channel"));
    }

    #[test]
    fn a_call_and_its_reply_are_written_without_their_texts() {
        let (recorder, folder) = recorder("shape");
        let secret = "receipt-7f3a-secret";
        let sent = vec![text(secret), IpcValue::Bool(true), IpcValue::Undefined];
        let answer = Ok(IpcValue::Undefined);
        let (seen, reply) = exchange(&recorder, &call(5, 17), sent.clone(), answer);
        assert_eq!(seen, Some(sent));
        assert_eq!(reply, Ok(IpcValue::Undefined));
        let quoting = refused(&format!("the receipt {secret} does not match"));
        let again = vec![text(secret), IpcValue::Bool(true)];
        let _ = exchange(&recorder, &call(5, 18), again, quoting);

        let written = std::fs::read_to_string(folder.join(RECORD_FILE)).unwrap();
        assert!(!written.contains(secret), "{written}");
        let lines = lines(&folder);
        let shapes = json!([
            { "type": "string", "length": secret.len() },
            { "type": "boolean", "value": true },
            { "type": "undefined" },
        ]);
        assert_eq!(lines[0]["event"], "call");
        assert_eq!(lines[0]["channel"], CHANNEL);
        assert_eq!(lines[0]["kind"], "invoke");
        assert_eq!(lines[0]["request"], 17);
        assert_eq!(lines[0]["view"], 5);
        assert_eq!(lines[0]["surface"], "Docs");
        assert_eq!(lines[0]["native"], true);
        assert_eq!(lines[0]["args"], shapes);
        assert_eq!(lines[1]["event"], "reply");
        assert_eq!(lines[1]["seq"], lines[0]["seq"]);
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["value"], json!({ "type": "undefined" }));
        assert_eq!(lines[3]["ok"], false);
        assert_eq!(lines[3]["error"], "the receipt <text> does not match");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn an_outcome_word_is_kept_and_an_object_keeps_its_keys() {
        let result = IpcValue::Object(vec![
            ("token".to_owned(), text("saved-input-token-1")),
            ("outcome".to_owned(), text("applied")),
        ]);
        let expected = json!({
            "type": "object",
            "fields": {
                "token": { "type": "string", "length": 19 },
                "outcome": { "type": "string", "length": 7, "word": "applied" },
            },
        });
        assert_eq!(shape(&result), expected);
        let refusal = "the saved input was not applied";
        assert_eq!(scrubbed(refusal, &[result]), refusal);
    }

    #[cfg(feature = "e2e-replay")]
    #[test]
    fn a_replay_stands_for_the_value_of_an_answered_call_of_the_named_view() {
        let (recorder, folder) = recorder("replay");
        let (own, foreign) = (call(5, 1), call(6, 2));
        let first = [IpcValue::Number(0.0)];
        let exact = vec![text("receipt-of-view-5"), IpcValue::Bool(true)];
        let accepted = Ok(IpcValue::Undefined);
        let _ = exchange(&recorder, &own, exact.clone(), accepted);
        // a refused call is not kept: its fake receipt cannot be replayed
        let fake = vec![text("a-fake-receipt"), IpcValue::Bool(true)];
        let _ = exchange(&recorder, &own, fake, refused("does not match"));

        let twice = vec![marker("self", &first), IpcValue::Bool(true)];
        let (seen, _) = exchange(&recorder, &own, twice, refused("does not match"));
        assert_eq!(seen, Some(exact.clone()));
        let other = vec![marker("other", &first), IpcValue::Bool(true)];
        let (seen, _) = exchange(&recorder, &foreign, other, refused("does not match"));
        assert_eq!(seen, Some(exact));

        let lines = lines(&folder);
        assert_eq!(lines[4]["replayed"], json!([{ "of": "self", "view": 5 }]));
        assert_eq!(lines[6]["replayed"], json!([{ "of": "other", "view": 5 }]));
        assert_eq!(lines[6]["view"], 6);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[cfg(feature = "e2e-replay")]
    #[test]
    fn a_replay_of_nothing_is_refused_before_the_handler() {
        let (recorder, folder) = recorder("unresolved");
        let args = vec![marker("other", &[IpcValue::Number(0.0)])];
        let accepted = Ok(IpcValue::Undefined);
        let (seen, reply) = exchange(&recorder, &call(5, 1), args, accepted);
        assert_eq!(seen, None);
        assert!(reply.unwrap_err().message.contains("e2e replay"));
        let lines = lines(&folder);
        assert_eq!(lines[1]["ok"], false);
        assert_eq!(lines[1]["unresolvedReplay"], true);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// A build without the `e2e-replay` feature has no replay: whatever a renderer sends, no
    /// value of an answered call is put into another call
    #[cfg(not(feature = "e2e-replay"))]
    #[test]
    fn without_the_replay_feature_a_marker_reaches_the_handler_as_it_was_sent() {
        fn ack(token: IpcValue) -> IpcValue {
            IpcValue::Object(vec![
                ("token".to_owned(), token),
                ("ok".to_owned(), IpcValue::Bool(true)),
            ])
        }
        let (recorder, folder) = recorder("no-replay");
        let answered = vec![ack(text("receipt-of-view-5"))];
        let accepted = Ok(IpcValue::Undefined);
        let _ = exchange(&recorder, &call(5, 1), answered, accepted);

        let path = [IpcValue::Number(0.0), text("token")];
        for (view, of) in [(6, "other"), (5, "self")] {
            let sent = vec![ack(marker(of, &path)), marker(of, &path)];
            let answer = refused("the token is not a text");
            let (seen, reply) = exchange(&recorder, &call(view, 2), sent.clone(), answer);
            assert_eq!(seen, Some(sent), "{of}");
            assert_eq!(reply, refused("the token is not a text"), "{of}");
        }

        let written = std::fs::read_to_string(folder.join(RECORD_FILE)).unwrap();
        assert!(!written.contains("receipt-of-view-5"), "{written}");
        assert!(!written.contains("replayed"), "{written}");
        let lines = lines(&folder);
        let token = &lines[2]["args"][0]["fields"]["token"];
        assert_eq!(token["type"], "object");
        assert_eq!(token["fields"]["e2eReplay"]["length"], 5);
        assert_eq!(lines[2]["view"], 6);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
