//! The replay markers of the native-only record. Compiled only with the `e2e-replay` feature,
//! which only the real-app test build (the `e2e` job of seuoffice-shell.yml) turns on.
//!
//! Two refusals a native-host spec must show need a receipt the shell handed a renderer, which
//! no scenario can know: the same acknowledgement twice, and one renderer presenting another's.
//! So the arguments of each call that was answered without an error stay in memory, and a later
//! call may name one of them in place of a value: `{"e2eReplay": "self" | "other", "path": [0,
//! "token"]}` stands for that value of the last answered call of the same channel from the
//! sender's own view, or from another view. Such a call still comes from a real page through
//! the bridge, and is authenticated and dispatched like any other.
//!
//! A build without the feature has none of this: a marker is an object like any other and
//! reaches the handler as it was sent. The marker's key is in the executable through
//! `REPLAY_KEY` alone, so its bytes tell the two builds apart: ci/replay-guard.sh fails the
//! bundle job when an executable that ships has them.

use serde_json::{Value, json};
use shell_contract::ids::ViewId;
use shell_contract::ipc::Reply;
use shell_contract::value::IpcValue;

const REPLAY_KEY: &str = "e2eReplay";
const REPLAY_PATH: &str = "path";

/// Says in the run's log that this build takes markers
pub(super) fn announce() {
    eprintln!("shell: e2e: this build replaces {REPLAY_KEY} markers in native-only calls");
}

/// The arguments of a call that was answered without an error
struct Answered {
    view: ViewId,
    channel: String,
    args: Vec<IpcValue>,
}

/// The answered calls, newest last, one per view and channel
#[derive(Default)]
pub(super) struct Memory(Vec<Answered>);

impl Memory {
    /// The call of `view` on `channel` got `reply`: kept unless that is an error
    pub(super) fn note(
        &mut self,
        view: ViewId,
        channel: String,
        args: Vec<IpcValue>,
        reply: &Reply,
    ) {
        if reply.is_err() {
            return;
        }
        let others = |call: &Answered| call.view != view || call.channel != channel;
        self.0.retain(others);
        self.0.push(Answered {
            view,
            channel,
            args,
        });
    }

    /// `args` with every marker in them replaced by the value it names, and one entry for each
    /// replaced marker. An error when a marker names a value no answered call has.
    pub(super) fn resolve(
        &self,
        args: &[IpcValue],
        view: ViewId,
        channel: &str,
    ) -> Result<(Vec<IpcValue>, Vec<Value>), String> {
        let mut replayed = Vec::new();
        let mut filled = Vec::with_capacity(args.len());
        for arg in args {
            filled.push(fill(arg, &self.0, view, channel, &mut replayed)?);
        }
        Ok((filled, replayed))
    }
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

    fn text(value: &str) -> IpcValue {
        IpcValue::String(value.to_owned())
    }

    /// The fields of a marker for the first argument of an answered call
    fn marker(of: &str) -> Vec<(String, IpcValue)> {
        let path = IpcValue::Array(vec![IpcValue::Number(0.0)]);
        vec![
            (REPLAY_KEY.to_owned(), text(of)),
            (REPLAY_PATH.to_owned(), path),
        ]
    }

    #[test]
    fn a_path_reaches_into_an_object_argument() {
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

    #[test]
    fn only_an_object_of_exactly_the_two_marker_fields_is_a_marker() {
        let first = [IpcValue::Number(0.0)];
        let own = IpcValue::Object(marker("self"));
        assert_eq!(replay_marker(&own), Some((false, first.as_slice())));
        let other = IpcValue::Object(marker("other"));
        assert_eq!(replay_marker(&other), Some((true, first.as_slice())));
        assert_eq!(replay_marker(&IpcValue::Object(marker("all"))), None);
        let mut wider = marker("self");
        wider.push(("ok".to_owned(), IpcValue::Bool(true)));
        assert_eq!(replay_marker(&IpcValue::Object(wider)), None);
        assert_eq!(replay_marker(&text(REPLAY_KEY)), None);
    }
}
