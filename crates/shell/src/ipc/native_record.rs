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
//! Two refusals a scenario must show need a receipt the shell handed a renderer, which no
//! scenario can know: the same acknowledgement twice, and one renderer presenting another's.
//! So the arguments of each call that was answered without an error stay in memory, and a later
//! call may name one of them in place of a value: `{"e2eReplay": "self" | "other", "path": [0,
//! "token"]}` stands for that value of the last answered call of the same channel from the
//! sender's own view, or from another view. Such a call still comes from a real page through
//! the bridge, and is authenticated and dispatched like any other.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use shell_contract::error::IpcError;
use shell_contract::ids::{RequestId, ViewId};
use shell_contract::ipc::{ChannelKind, Reply, Sender};
use shell_contract::value::IpcValue;
use shell_contract::{CHANNELS, ChannelNamespace};

const RECORD_FILE: &str = "native-ipc.jsonl";
const REPLAY_KEY: &str = "e2eReplay";
const REPLAY_PATH: &str = "path";
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

/// What `dispatch` goes on with: the arguments, replayed values filled in, and a delivery that
/// also records the reply. `None` when the call named a value nothing recorded: it is answered
/// here. Anything but a native-only channel of a `--e2e` run passes through untouched.
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
    Some(Arc::new(Recorder::new(request.out.join(RECORD_FILE))))
});

fn is_native(channel: &str) -> bool {
    CHANNELS
        .iter()
        .any(|spec| spec.name == channel && spec.namespace == ChannelNamespace::Native)
}

/// The arguments of a call that was answered without an error
struct Answered {
    view: ViewId,
    channel: String,
    args: Vec<IpcValue>,
}

#[derive(Default)]
struct State {
    file: Option<File>,
    /// the file could not be opened: said once, then nothing is written
    unwritable: bool,
    seq: u64,
    /// newest last, one per view and channel
    answered: Vec<Answered>,
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
        let mut replayed = Vec::new();
        let filled = args
            .iter()
            .map(|arg| fill(arg, &state.answered, view, call.channel, &mut replayed))
            .collect::<Result<Vec<_>, String>>();
        let args = match filled {
            Ok(args) => args,
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
                deliver(Err(IpcError {
                    name: Some("Error".into()),
                    message,
                }));
                return None;
            }
        };
        line["args"] = shapes(&args);
        if !replayed.is_empty() {
            line["replayed"] = Value::Array(replayed);
        }
        self.write(&mut state, &line);
        drop(state);
        let recorder = Arc::clone(self);
        let channel = call.channel.to_owned();
        let kept = args.clone();
        let recorded: Deliver = Box::new(move |reply| {
            recorder.replied(seq, view, channel, kept, &reply);
            deliver(reply);
        });
        Some((args, recorded))
    }

    fn replied(&self, seq: u64, view: ViewId, channel: String, args: Vec<IpcValue>, reply: &Reply) {
        let mut state = self.lock();
        let mut line = json!({ "event": "reply", "seq": seq, "ms": self.elapsed_ms() });
        match reply {
            Ok(value) => {
                line["ok"] = Value::Bool(true);
                line["value"] = shape(value);
            }
            Err(error) => {
                line["ok"] = Value::Bool(false);
                line["error"] = Value::from(scrubbed(&error.message, &args));
            }
        }
        self.write(&mut state, &line);
        if reply.is_ok() {
            let others = |call: &Answered| call.view != view || call.channel != channel;
            state.answered.retain(others);
            state.answered.push(Answered {
                view,
                channel,
                args,
            });
        }
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

/// `{"e2eReplay": "self" | "other", "path": [...]}`: whether another view's call is meant, and
/// where the value is in that call's arguments
fn replay_marker(value: &IpcValue) -> Option<(bool, &[IpcValue])> {
    fn field<'a>(fields: &'a [(String, IpcValue)], name: &str) -> Option<&'a IpcValue> {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, field)| field)
    }
    let IpcValue::Object(fields) = value else {
        return None;
    };
    if fields.len() != 2 {
        return None;
    }
    let IpcValue::String(of) = field(fields, REPLAY_KEY)? else {
        return None;
    };
    let IpcValue::Array(path) = field(fields, REPLAY_PATH)? else {
        return None;
    };
    match of.as_str() {
        "self" => Some((false, path.as_slice())),
        "other" => Some((true, path.as_slice())),
        _ => None,
    }
}

/// `value` with every replay marker inside it replaced by the value it names
fn fill(
    value: &IpcValue,
    answered: &[Answered],
    view: ViewId,
    channel: &str,
    replayed: &mut Vec<Value>,
) -> Result<IpcValue, String> {
    if let Some((other, path)) = replay_marker(value) {
        let (of, whose) = if other {
            ("other", "another view")
        } else {
            ("self", "this view")
        };
        let missing = |what: &str| format!("e2e replay: '{channel}' has no {what}");
        let source = answered
            .iter()
            .rev()
            .find(|call| call.channel == channel && (call.view != view) == other);
        let Some(source) = source else {
            return Err(missing(&format!("answered call from {whose}")));
        };
        let Some(found) = walk(&source.args, path) else {
            return Err(missing("such value in its answered call"));
        };
        replayed.push(json!({ "of": of, "view": source.view.0 }));
        return Ok(found.clone());
    }
    match value {
        IpcValue::Array(items) => {
            let mut filled = Vec::with_capacity(items.len());
            for item in items {
                filled.push(fill(item, answered, view, channel, replayed)?);
            }
            Ok(IpcValue::Array(filled))
        }
        IpcValue::Object(fields) => {
            let mut filled = Vec::with_capacity(fields.len());
            for (key, field) in fields {
                let field = fill(field, answered, view, channel, replayed)?;
                filled.push((key.clone(), field));
            }
            Ok(IpcValue::Object(filled))
        }
        other => Ok(other.clone()),
    }
}

/// The value at `path`: an argument index, then object keys and array indexes
fn walk<'a>(args: &'a [IpcValue], path: &[IpcValue]) -> Option<&'a IpcValue> {
    let (first, rest) = path.split_first()?;
    let mut at = args.get(index_of(first)?)?;
    for step in rest {
        at = match (at, step) {
            (IpcValue::Object(fields), IpcValue::String(key)) => {
                &fields.iter().find(|(name, _)| name == key)?.1
            }
            (IpcValue::Array(items), index) => items.get(index_of(index)?)?,
            _ => return None,
        };
    }
    Some(at)
}

fn index_of(step: &IpcValue) -> Option<usize> {
    match step {
        IpcValue::Number(index) if *index >= 0.0 && index.fract() == 0.0 => Some(*index as usize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::Surface;
    use shell_contract::ids::WindowId;
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

    /// The first argument of an answered call, by name
    fn replay(of: &str) -> IpcValue {
        let path = IpcValue::Array(vec![IpcValue::Number(0.0)]);
        IpcValue::Object(vec![
            (REPLAY_KEY.to_owned(), text(of)),
            (REPLAY_PATH.to_owned(), path),
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

    #[test]
    fn a_replay_stands_for_the_value_of_an_answered_call_of_the_named_view() {
        let (recorder, folder) = recorder("replay");
        let (own, foreign) = (call(5, 1), call(6, 2));
        let exact = vec![text("receipt-of-view-5"), IpcValue::Bool(true)];
        let accepted = Ok(IpcValue::Undefined);
        let _ = exchange(&recorder, &own, exact.clone(), accepted);
        // a refused call is not kept: its fake receipt cannot be replayed
        let fake = vec![text("a-fake-receipt"), IpcValue::Bool(true)];
        let _ = exchange(&recorder, &own, fake, refused("does not match"));

        let twice = vec![replay("self"), IpcValue::Bool(true)];
        let (seen, _) = exchange(&recorder, &own, twice, refused("does not match"));
        assert_eq!(seen, Some(exact.clone()));
        let other = vec![replay("other"), IpcValue::Bool(true)];
        let (seen, _) = exchange(&recorder, &foreign, other, refused("does not match"));
        assert_eq!(seen, Some(exact));

        let lines = lines(&folder);
        assert_eq!(lines[4]["replayed"], json!([{ "of": "self", "view": 5 }]));
        assert_eq!(lines[6]["replayed"], json!([{ "of": "other", "view": 5 }]));
        assert_eq!(lines[6]["view"], 6);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_replay_of_nothing_is_refused_before_the_handler() {
        let (recorder, folder) = recorder("unresolved");
        let args = vec![replay("other")];
        let accepted = Ok(IpcValue::Undefined);
        let (seen, reply) = exchange(&recorder, &call(5, 1), args, accepted);
        assert_eq!(seen, None);
        assert!(reply.unwrap_err().message.contains("e2e replay"));
        let lines = lines(&folder);
        assert_eq!(lines[1]["ok"], false);
        assert_eq!(lines[1]["unresolvedReplay"], true);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_replay_reaches_into_an_object_argument() {
        let result = IpcValue::Object(vec![
            ("token".to_owned(), text("recovery-token")),
            ("ok".to_owned(), IpcValue::Bool(true)),
        ]);
        let args = [result];
        let token = text("recovery-token");
        let first = IpcValue::Number(0.0);
        assert_eq!(walk(&args, &[first.clone(), text("token")]), Some(&token));
        assert_eq!(walk(&args, &[first.clone(), text("none")]), None);
        assert_eq!(walk(&args, &[IpcValue::Number(1.0)]), None);
        assert_eq!(walk(&args, &[IpcValue::Number(-1.0)]), None);
        assert_eq!(walk(&args, &[]), None);
    }
}
