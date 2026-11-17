//! The dialog host's e2e hook: `dialog` steps queue answers, and the native dialogs the app
//! would show take them instead.
//!
//! The hook sits in `host/dialog/owned.rs`, where every file dialog and message box of the
//! shell funnels through, and in `Dialogs::error`. [`intercept_files`] and friends return
//! `None` unless `--e2e` is active, so a normal run shows its native dialogs as before.
//!
//! An answer is taken in the order it was queued. A dialog that finds none queued waits for one
//! ([`UNANSWERED_WAIT`]): the step that triggers a dialog is usually followed by its `dialog`
//! step, and the answer may arrive after the app already asked. A dialog still unanswered after
//! that is cancelled, so an unexpected dialog can never hang a CI run. Every intercepted dialog
//! is written to `e2e-dialogs.jsonl`, answered or not.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use shell_contract::hosts::{FileDialog, MessageBox, MessageBoxAnswer};

use super::report::{DIALOG_LOG_FILE, append_line};
use crate::host::message_box::Spec;

/// How long a dialog waits for its answer before it is cancelled
pub(super) const UNANSWERED_WAIT: Duration = Duration::from_secs(15);

/// The three buttons of the unsaved-changes prompt: Save, Don't Save, Cancel
const SAVE_PROMPT_BUTTONS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// the unsaved-changes prompt's first button
    Save,
    /// its second
    DontSave,
    /// the cancel button of a message box, or a dismissed file dialog
    Cancel,
    /// the path a file dialog returns
    Path(PathBuf),
}

impl Answer {
    /// `save`, `dontSave`, `cancel` or `path:<abs>`
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        match text {
            "save" => Ok(Self::Save),
            "dontSave" => Ok(Self::DontSave),
            "cancel" => Ok(Self::Cancel),
            other => match other.strip_prefix("path:") {
                Some(path) if PathBuf::from(path).is_absolute() => {
                    Ok(Self::Path(PathBuf::from(path)))
                }
                Some(_) => Err("a path: answer needs an absolute path".into()),
                None => Err(format!("unknown dialog answer {other:?}")),
            },
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::Save => "save".into(),
            Self::DontSave => "dontSave".into(),
            Self::Cancel => "cancel".into(),
            Self::Path(path) => format!("path:{}", path.display()),
        }
    }
}

struct State {
    next_ticket: u64,
    pending: VecDeque<(u64, Answer)>,
    consumed: Vec<u64>,
}

/// Answers waiting for a dialog, and which of them a dialog already took
pub(super) struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}

impl Queue {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                next_ticket: 0,
                pending: VecDeque::new(),
                consumed: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues an answer; the ticket tells later whether a dialog took it
    pub(super) fn push(&self, answer: Answer) -> u64 {
        let mut state = self.lock();
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.pending.push_back((ticket, answer));
        drop(state);
        self.changed.notify_all();
        ticket
    }

    /// True once a dialog took the answer of `ticket`, false after `timeout`
    pub(super) fn wait_consumed(&self, ticket: u64, timeout: Duration) -> bool {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| !state.consumed.contains(&ticket))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.consumed.contains(&ticket)
    }

    /// Withdraws an answer no dialog took, so it cannot answer a later one
    pub(super) fn discard(&self, ticket: u64) {
        self.lock().pending.retain(|(queued, _)| *queued != ticket);
    }

    /// The oldest queued answer, waiting up to `wait` for one
    pub(super) fn take(&self, wait: Duration) -> Option<Answer> {
        let state = self.lock();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, wait, |state| state.pending.is_empty())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (ticket, answer) = state.pending.pop_front()?;
        state.consumed.push(ticket);
        drop(state);
        self.changed.notify_all();
        Some(answer)
    }
}

static QUEUE: Queue = Queue::new();

pub(super) fn push(answer: Answer) -> u64 {
    QUEUE.push(answer)
}

pub(super) fn wait_consumed(ticket: u64, timeout: Duration) -> bool {
    QUEUE.wait_consumed(ticket, timeout)
}

pub(super) fn discard(ticket: u64) {
    QUEUE.discard(ticket);
}

fn resolve_files(answer: Option<&Answer>) -> Result<Option<Vec<PathBuf>>, String> {
    match answer {
        Some(Answer::Path(path)) => Ok(Some(vec![path.clone()])),
        Some(Answer::Cancel) | None => Ok(None),
        Some(other) => Err(format!(
            "the answer {} does not fit a file dialog",
            other.label()
        )),
    }
}

fn resolve_message(answer: Option<&Answer>, spec: &Spec) -> Result<MessageBoxAnswer, String> {
    let response = match answer {
        Some(Answer::Cancel) | None => spec.cancel_id,
        Some(Answer::Save) if spec.buttons.len() >= SAVE_PROMPT_BUTTONS => 0,
        Some(Answer::DontSave) if spec.buttons.len() >= SAVE_PROMPT_BUTTONS => 1,
        Some(other) => {
            return Err(format!(
                "the answer {} does not fit a message box with the buttons {:?}",
                other.label(),
                spec.buttons
            ));
        }
    };
    Ok(MessageBoxAnswer {
        response,
        checkbox_checked: false,
    })
}

fn log(entry: &Value) {
    let Some(request) = super::request() else {
        return;
    };
    if let Err(error) = append_line(&request.out, DIALOG_LOG_FILE, entry) {
        eprintln!("shell: e2e: {error}");
    }
}

fn answer_json(answer: Option<&Answer>) -> Value {
    answer.map_or(Value::Null, |answer| Value::from(answer.label()))
}

/// A file dialog (`save` for a save panel). `None` when `--e2e` is not active.
pub(crate) fn intercept_files(
    options: &FileDialog,
    save: bool,
) -> Option<Result<Option<Vec<PathBuf>>, String>> {
    super::request()?;
    let answer = QUEUE.take(UNANSWERED_WAIT);
    let result = resolve_files(answer.as_ref());
    log(&json!({
        "dialog": if save { "save" } else { "open" },
        "title": options.title,
        "defaultPath": options.default_path.as_ref().map(|path| path.display().to_string()),
        "answer": answer_json(answer.as_ref()),
        "unanswered": answer.is_none(),
        "error": result.as_ref().err(),
    }));
    Some(result)
}

/// A message box. `None` when `--e2e` is not active.
pub(crate) fn intercept_message(options: &MessageBox) -> Option<Result<MessageBoxAnswer, String>> {
    super::request()?;
    let spec = Spec::of(options);
    let answer = QUEUE.take(UNANSWERED_WAIT);
    let result = resolve_message(answer.as_ref(), &spec);
    log(&json!({
        "dialog": "message",
        "message": spec.message,
        "detail": spec.detail,
        "buttons": spec.buttons,
        "answer": answer_json(answer.as_ref()),
        "unanswered": answer.is_none(),
        "response": result.as_ref().ok().map(|answered| answered.response),
        "error": result.as_ref().err(),
    }));
    Some(result)
}

/// An error box has no answer: with `--e2e` it is logged instead of shown. True when it was.
pub(crate) fn intercept_error(title: &str, content: &str) -> bool {
    if super::request().is_none() {
        return false;
    }
    log(&json!({ "dialog": "error", "title": title, "message": content }));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn spec(buttons: &[&str]) -> Spec {
        Spec::of(&MessageBox {
            message: "Close?".into(),
            buttons: buttons.iter().map(|b| (*b).to_owned()).collect(),
            ..MessageBox::default()
        })
    }

    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn answers_parse_and_round_trip_their_label() {
        assert_eq!(Answer::parse("save"), Ok(Answer::Save));
        assert_eq!(Answer::parse("dontSave"), Ok(Answer::DontSave));
        assert_eq!(Answer::parse("cancel"), Ok(Answer::Cancel));
        let path = abs("out.docx");
        let answer = Answer::parse(&format!("path:{}", path.display())).unwrap();
        assert_eq!(answer, Answer::Path(path));
        assert_eq!(Answer::parse(&answer.label()), Ok(answer));
        assert!(Answer::parse("path:relative.docx").is_err());
        assert!(Answer::parse("path:").is_err());
        assert!(Answer::parse("yes").is_err());
    }

    #[test]
    fn the_save_prompt_maps_to_its_three_buttons() {
        let prompt = spec(&["Save", "Don't Save", "Cancel"]);
        let response = |answer| resolve_message(Some(&answer), &prompt).unwrap().response;
        assert_eq!(response(Answer::Save), 0);
        assert_eq!(response(Answer::DontSave), 1);
        assert_eq!(response(Answer::Cancel), 2);
        assert_eq!(resolve_message(None, &prompt).unwrap().response, 2);
    }

    #[test]
    fn an_answer_that_does_not_fit_the_box_is_refused() {
        let two = spec(&["Discard and Close", "Cancel"]);
        assert!(resolve_message(Some(&Answer::DontSave), &two).is_err());
        assert!(resolve_message(Some(&Answer::Save), &two).is_err());
        assert_eq!(
            resolve_message(Some(&Answer::Cancel), &two).unwrap().response,
            1
        );
        assert!(resolve_message(Some(&Answer::Path(abs("a"))), &two).is_err());
    }

    #[test]
    fn a_file_dialog_takes_a_path_or_cancels() {
        let path = abs("a.pdf");
        assert_eq!(
            resolve_files(Some(&Answer::Path(path.clone()))),
            Ok(Some(vec![path]))
        );
        assert_eq!(resolve_files(Some(&Answer::Cancel)), Ok(None));
        assert_eq!(resolve_files(None), Ok(None));
        assert!(resolve_files(Some(&Answer::Save)).is_err());
    }

    #[test]
    fn answers_are_taken_in_order_and_report_consumption() {
        let queue = Queue::new();
        let first = queue.push(Answer::Save);
        let second = queue.push(Answer::Cancel);
        assert!(!queue.wait_consumed(first, Duration::from_millis(5)));
        assert_eq!(queue.take(Duration::ZERO), Some(Answer::Save));
        assert!(queue.wait_consumed(first, Duration::ZERO));
        assert!(!queue.wait_consumed(second, Duration::from_millis(5)));
        assert_eq!(queue.take(Duration::ZERO), Some(Answer::Cancel));
        assert_eq!(queue.take(Duration::from_millis(5)), None);
    }

    #[test]
    fn a_waiting_dialog_gets_an_answer_pushed_later() {
        let queue = Arc::new(Queue::new());
        let pusher = queue.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            pusher.push(Answer::DontSave)
        });
        let taken = queue.take(Duration::from_secs(5));
        let ticket = handle.join().unwrap();
        assert_eq!(taken, Some(Answer::DontSave));
        assert!(queue.wait_consumed(ticket, Duration::ZERO));
    }

    #[test]
    fn a_discarded_answer_is_never_taken() {
        let queue = Queue::new();
        let ticket = queue.push(Answer::Save);
        queue.discard(ticket);
        assert_eq!(queue.take(Duration::ZERO), None);
        assert!(!queue.wait_consumed(ticket, Duration::ZERO));
    }
}
