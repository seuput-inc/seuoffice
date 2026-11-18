//! The scenario file of `--e2e`: parsing and the expansion of the folder placeholders.
//!
//! The file is `{"deadlineMs"?: n, "steps": [...]}`. It is parsed by hand into [`Step`] values
//! (the crate has no `serde` derive dependency), and every error names the step it is in.
//! Nothing here touches the app, so all of it is unit tested.
//!
//! Placeholders: in every string field `${FIXTURES}` and `${SEUOFFICE_E2E_FIXTURES}` stand for
//! the environment variable `SEUOFFICE_E2E_FIXTURES`, and `${WORK}` and `${SEUOFFICE_E2E_WORK}`
//! for `SEUOFFICE_E2E_WORK`. A placeholder whose variable is unset is an error, so a scenario
//! never runs with a literal `${WORK}` in a path. Query expressions are expanded too: a
//! JavaScript template literal that must keep one of these four names has to be written
//! differently.

use std::path::PathBuf;

use serde_json::{Map, Value};

use super::dialog::Answer;

pub(super) const FIXTURES_ENV: &str = "SEUOFFICE_E2E_FIXTURES";
pub(super) const WORK_ENV: &str = "SEUOFFICE_E2E_WORK";

pub(super) const DEFAULT_DEADLINE_MS: u64 = 600_000;
pub(super) const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
pub(super) const DEFAULT_DIALOG_TIMEOUT_MS: u64 = 15_000;
pub(super) const DEFAULT_EXPORT_TIMEOUT_MS: u64 = 60_000;
pub(super) const DEFAULT_QUERY_TIMEOUT_MS: u64 = 10_000;

const PLACEHOLDERS: [(&str, &str); 4] = [
    ("${FIXTURES}", FIXTURES_ENV),
    ("${WORK}", WORK_ENV),
    ("${SEUOFFICE_E2E_FIXTURES}", FIXTURES_ENV),
    ("${SEUOFFICE_E2E_WORK}", WORK_ENV),
];

/// What `newDocument` creates: the contract's names, as the shell's home kinds spell them
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DocKind {
    Docs,
    Sheets,
    Slides,
    Markdown,
    Html,
    Mindmap,
    Diagram,
    Hwp,
    Pdf,
}

impl DocKind {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "docs" => Self::Docs,
            "sheets" => Self::Sheets,
            "slides" => Self::Slides,
            "markdown" => Self::Markdown,
            "html" => Self::Html,
            "mindmap" => Self::Mindmap,
            "diagram" => Self::Diagram,
            "hwp" => Self::Hwp,
            "pdf" => Self::Pdf,
            _ => return None,
        })
    }

    /// The name `ShellTabs::new_document` takes (`newDocTab` and its kin)
    pub(super) fn home_kind(self) -> &'static str {
        match self {
            Self::Docs => "docx",
            Self::Sheets => "xlsx",
            Self::Slides => "pptx",
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Mindmap => "xmind",
            Self::Diagram => "drawio",
            Self::Hwp => "hwp",
            Self::Pdf => "pdf",
        }
    }
}

/// The accelerators of `shortcut`: Cmd on macOS, Ctrl on Windows; `Quit` is Cmd+Q and Alt+F4
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shortcut {
    Save,
    SaveAs,
    SelectAll,
    Copy,
    Paste,
    Undo,
    CloseTab,
    Print,
    Quit,
}

impl Shortcut {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "save" => Self::Save,
            "saveAs" => Self::SaveAs,
            "selectAll" => Self::SelectAll,
            "copy" => Self::Copy,
            "paste" => Self::Paste,
            "undo" => Self::Undo,
            "closeTab" => Self::CloseTab,
            "print" => Self::Print,
            "quit" => Self::Quit,
            _ => return None,
        })
    }

    /// The letter key of the accelerator, lower case
    pub(super) fn letter(self) -> char {
        match self {
            Self::Save | Self::SaveAs => 's',
            Self::SelectAll => 'a',
            Self::Copy => 'c',
            Self::Paste => 'v',
            Self::Undo => 'z',
            Self::CloseTab => 'w',
            Self::Print => 'p',
            Self::Quit => 'q',
        }
    }

    pub(super) fn shift(self) -> bool {
        self == Self::SaveAs
    }
}

/// One character of `type.text` as a key: control characters are the keys they stand for
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TypedKey {
    Return,
    Tab,
    Escape,
    Backspace,
    Char(char),
}

pub(super) fn typed_keys(text: &str) -> Vec<TypedKey> {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' => TypedKey::Return,
            '\t' => TypedKey::Tab,
            '\u{1b}' => TypedKey::Escape,
            '\u{8}' => TypedKey::Backspace,
            other => TypedKey::Char(other),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    Open {
        path: PathBuf,
    },
    NewDocument {
        kind: DocKind,
    },
    WaitReady {
        timeout_ms: u64,
    },
    Type {
        text: String,
    },
    Ime {
        keys: String,
    },
    Shortcut(Shortcut),
    Clipboard {
        text: Option<String>,
        html: Option<String>,
    },
    Query {
        js: String,
        name: String,
        timeout_ms: u64,
    },
    Dialog {
        answer: Answer,
        timeout_ms: u64,
    },
    ExportPdf {
        path: PathBuf,
        timeout_ms: u64,
    },
    Screenshot {
        name: String,
    },
    /// A native left click at a point of the element the CSS selector names in the top
    /// document, `fx` and `fy` from its top left
    Click {
        selector: String,
        fx: f64,
        fy: f64,
        timeout_ms: u64,
    },
    Wait {
        ms: u64,
    },
}

impl Step {
    /// The contract's name of the op, as `e2e.jsonl` records it
    pub(super) fn op(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::NewDocument { .. } => "newDocument",
            Self::WaitReady { .. } => "waitReady",
            Self::Type { .. } => "type",
            Self::Ime { .. } => "ime",
            Self::Shortcut(_) => "shortcut",
            Self::Clipboard { .. } => "clipboard",
            Self::Query { .. } => "query",
            Self::Dialog { .. } => "dialog",
            Self::ExportPdf { .. } => "exportPdf",
            Self::Screenshot { .. } => "screenshot",
            Self::Click { .. } => "click",
            Self::Wait { .. } => "wait",
        }
    }

    /// The `as` name of a query, which the record carries so a checker can find it
    pub(super) fn name(&self) -> Option<&str> {
        match self {
            Self::Query { name, .. } => Some(name),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Scenario {
    pub(super) deadline_ms: u64,
    pub(super) steps: Vec<Step>,
}

pub(super) type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Replaces the folder placeholders of `text`
pub(super) fn expand(text: &str, env: Env<'_>) -> Result<String, String> {
    let mut out = text.to_owned();
    for (token, variable) in PLACEHOLDERS {
        if !out.contains(token) {
            continue;
        }
        let value = env(variable)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{token} is used but {variable} is not set"))?;
        out = out.replace(token, &value);
    }
    Ok(out)
}

pub(super) fn parse(text: &str, env: Env<'_>) -> Result<Scenario, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| format!("the scenario is not valid JSON: {error}"))?;
    let root = root
        .as_object()
        .ok_or("the scenario must be a JSON object")?;
    let deadline_ms = match root.get("deadlineMs") {
        None | Some(Value::Null) => DEFAULT_DEADLINE_MS,
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms > 0)
            .ok_or("deadlineMs must be a positive integer")?,
    };
    let raw_steps = root
        .get("steps")
        .and_then(Value::as_array)
        .ok_or("steps must be an array")?;
    if raw_steps.is_empty() {
        return Err("steps is empty".into());
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (index, raw) in raw_steps.iter().enumerate() {
        steps.push(parse_step(raw, env).map_err(|error| format!("step {index}: {error}"))?);
    }
    Ok(Scenario { deadline_ms, steps })
}

/// The fields of one step object
struct Fields<'a> {
    object: &'a Map<String, Value>,
    env: Env<'a>,
}

impl Fields<'_> {
    fn optional(&self, key: &str) -> Result<Option<String>, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => expand(text, self.env).map(Some),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn string(&self, key: &str) -> Result<String, String> {
        self.optional(key)?
            .ok_or_else(|| format!("{key} is required"))
    }

    fn absolute_path(&self, key: &str) -> Result<PathBuf, String> {
        let path = PathBuf::from(self.string(key)?);
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(format!("{key} must be an absolute path"))
        }
    }

    /// A position inside an element, from its top left (0) to its bottom right (1)
    fn fraction(&self, key: &str, default: f64) -> Result<f64, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_f64()
                .filter(|fraction| (0.0..=1.0).contains(fraction))
                .ok_or_else(|| format!("{key} must be a number from 0 to 1")),
        }
    }

    fn millis(&self, key: &str, default: u64) -> Result<u64, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_u64()
                .ok_or_else(|| format!("{key} must be a non-negative integer")),
        }
    }

    /// A name that becomes part of an artifact's file name or a record's `as`
    fn name(&self, key: &str) -> Result<String, String> {
        let name = self.string(key)?;
        let plain = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !name.starts_with('.');
        if plain {
            Ok(name)
        } else {
            Err(format!(
                "{key} must be letters, digits, '-', '_' or '.' and not start with '.'"
            ))
        }
    }
}

fn parse_step(raw: &Value, env: Env<'_>) -> Result<Step, String> {
    let object = raw.as_object().ok_or("a step must be an object")?;
    let fields = Fields { object, env };
    let op = fields.string("op")?;
    Ok(match op.as_str() {
        "open" => Step::Open {
            path: fields.absolute_path("path")?,
        },
        "newDocument" => {
            let kind = fields.string("kind")?;
            Step::NewDocument {
                kind: DocKind::parse(&kind)
                    .ok_or_else(|| format!("unknown document kind {kind:?}"))?,
            }
        }
        "waitReady" => Step::WaitReady {
            timeout_ms: fields.millis("timeoutMs", DEFAULT_READY_TIMEOUT_MS)?,
        },
        "type" => Step::Type {
            text: fields.string("text")?,
        },
        "ime" => {
            let source = fields.string("source")?;
            if source != "ko-2set" {
                return Err(format!("unsupported input source {source:?}"));
            }
            let keys = fields.string("keys")?;
            if keys.is_empty() || !keys.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err("keys must be one or more ASCII letters".into());
            }
            Step::Ime { keys }
        }
        "shortcut" => {
            let name = fields.string("name")?;
            Step::Shortcut(
                Shortcut::parse(&name).ok_or_else(|| format!("unknown shortcut {name:?}"))?,
            )
        }
        "clipboard" => {
            let text = fields.optional("text")?;
            let html = fields.optional("html")?;
            if text.is_none() && html.is_none() {
                return Err("text or html is required".into());
            }
            Step::Clipboard { text, html }
        }
        "query" => Step::Query {
            js: fields.string("js")?,
            name: fields.name("as")?,
            timeout_ms: fields.millis("timeoutMs", DEFAULT_QUERY_TIMEOUT_MS)?,
        },
        "dialog" => Step::Dialog {
            answer: Answer::parse(&fields.string("answer")?)?,
            timeout_ms: fields.millis("timeoutMs", DEFAULT_DIALOG_TIMEOUT_MS)?,
        },
        "exportPdf" => Step::ExportPdf {
            path: fields.absolute_path("path")?,
            timeout_ms: fields.millis("timeoutMs", DEFAULT_EXPORT_TIMEOUT_MS)?,
        },
        "screenshot" => Step::Screenshot {
            name: fields.name("name")?,
        },
        "click" => Step::Click {
            selector: fields.string("selector")?,
            fx: fields.fraction("fx", 0.5)?,
            fy: fields.fraction("fy", 0.5)?,
            timeout_ms: fields.millis("timeoutMs", DEFAULT_QUERY_TIMEOUT_MS)?,
        },
        "wait" => Step::Wait {
            ms: fields.millis("ms", 0)?,
        },
        other => return Err(format!("unknown op {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    fn env(name: &str) -> Option<String> {
        match name {
            FIXTURES_ENV => Some(work()),
            WORK_ENV => Some(work()),
            _ => None,
        }
    }

    fn parse_text(text: &str) -> Result<Scenario, String> {
        parse(text, &env)
    }

    #[test]
    fn every_op_parses_with_its_fields() {
        let scenario = parse_text(
            r#"{
              "deadlineMs": 90000,
              "steps": [
                {"op":"open","path":"${FIXTURES}/a.docx"},
                {"op":"newDocument","kind":"sheets"},
                {"op":"waitReady","timeoutMs":5000},
                {"op":"type","text":"hi\n"},
                {"op":"ime","source":"ko-2set","keys":"gksrmf"},
                {"op":"shortcut","name":"saveAs"},
                {"op":"clipboard","text":"t","html":"<b>t</b>"},
                {"op":"query","js":"1+1","as":"prep-sum"},
                {"op":"dialog","answer":"dontSave"},
                {"op":"exportPdf","path":"${WORK}/out.pdf"},
                {"op":"screenshot","name":"shot1"},
                {"op":"wait","ms":250}
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(scenario.deadline_ms, 90_000);
        let ops: Vec<&str> = scenario.steps.iter().map(Step::op).collect();
        assert_eq!(
            ops,
            [
                "open",
                "newDocument",
                "waitReady",
                "type",
                "ime",
                "shortcut",
                "clipboard",
                "query",
                "dialog",
                "exportPdf",
                "screenshot",
                "wait"
            ]
        );
        assert_eq!(
            scenario.steps[0],
            Step::Open {
                path: PathBuf::from(format!("{}/a.docx", work()))
            }
        );
        assert_eq!(
            scenario.steps[1],
            Step::NewDocument {
                kind: DocKind::Sheets
            }
        );
        assert_eq!(scenario.steps[2], Step::WaitReady { timeout_ms: 5000 });
        assert_eq!(scenario.steps[5], Step::Shortcut(Shortcut::SaveAs));
        assert_eq!(scenario.steps[8].op(), "dialog");
        assert_eq!(scenario.steps[7].name(), Some("prep-sum"));
        assert_eq!(scenario.steps[0].name(), None);
    }

    #[test]
    fn a_click_names_an_element_and_a_point_inside_it() {
        let scenario = parse_text(
            r##"{"steps":[{"op":"click","selector":"#a iframe"},
                {"op":"click","selector":"canvas","fx":0.25,"fy":1,"timeoutMs":500}]}"##,
        )
        .unwrap();
        assert_eq!(
            scenario.steps[0],
            Step::Click {
                selector: "#a iframe".into(),
                fx: 0.5,
                fy: 0.5,
                timeout_ms: DEFAULT_QUERY_TIMEOUT_MS
            }
        );
        assert_eq!(scenario.steps[1].op(), "click");
        assert!(
            parse_text(r#"{"steps":[{"op":"click","selector":"a","fx":1.5}]}"#)
                .unwrap_err()
                .contains("fx must be a number from 0 to 1")
        );
        assert!(
            parse_text(r#"{"steps":[{"op":"click"}]}"#)
                .unwrap_err()
                .contains("selector is required")
        );
    }

    #[test]
    fn defaults_apply_when_fields_are_left_out() {
        let scenario = parse_text(r#"{"steps":[{"op":"waitReady"},{"op":"wait"}]}"#).unwrap();
        assert_eq!(scenario.deadline_ms, DEFAULT_DEADLINE_MS);
        assert_eq!(
            scenario.steps[0],
            Step::WaitReady {
                timeout_ms: DEFAULT_READY_TIMEOUT_MS
            }
        );
        assert_eq!(scenario.steps[1], Step::Wait { ms: 0 });
    }

    #[test]
    fn errors_name_the_step() {
        let error =
            parse_text(r#"{"steps":[{"op":"wait"},{"op":"shortcut","name":"nope"}]}"#).unwrap_err();
        assert!(error.starts_with("step 1:"), "{error}");
        assert!(error.contains("nope"), "{error}");
        assert!(parse_text("not json").unwrap_err().contains("JSON"));
        assert!(parse_text(r#"{"steps":[]}"#).unwrap_err().contains("empty"));
        assert!(
            parse_text(r#"{"steps":[{"op":"fly"}]}"#)
                .unwrap_err()
                .contains("unknown op")
        );
        assert!(parse_text(r#"{"deadlineMs":0,"steps":[{"op":"wait"}]}"#).is_err());
    }

    #[test]
    fn paths_must_be_absolute_and_ime_keys_letters() {
        assert!(
            parse_text(r#"{"steps":[{"op":"open","path":"relative/a.docx"}]}"#)
                .unwrap_err()
                .contains("absolute")
        );
        assert!(parse_text(r#"{"steps":[{"op":"ime","source":"ko-2set","keys":"g k"}]}"#).is_err());
        assert!(parse_text(r#"{"steps":[{"op":"ime","source":"ja-kana","keys":"gk"}]}"#).is_err());
        assert!(parse_text(r#"{"steps":[{"op":"clipboard"}]}"#).is_err());
    }

    #[test]
    fn artifact_names_cannot_leave_the_output_folder() {
        for bad in ["../x", "a/b", ".hidden", "", "a b"] {
            let text = format!(r#"{{"steps":[{{"op":"screenshot","name":"{bad}"}}]}}"#);
            assert!(parse_text(&text).is_err(), "{bad:?}");
        }
        assert!(parse_text(r#"{"steps":[{"op":"screenshot","name":"docs-1_a.b"}]}"#).is_ok());
    }

    #[test]
    fn placeholders_expand_from_the_environment() {
        let env = |name: &str| match name {
            FIXTURES_ENV => Some("/fx".to_owned()),
            WORK_ENV => Some("/wk".to_owned()),
            _ => None,
        };
        assert_eq!(
            expand("${FIXTURES}/a ${WORK}/b ${SEUOFFICE_E2E_WORK}/c", &env).unwrap(),
            "/fx/a /wk/b /wk/c"
        );
        assert_eq!(expand("no placeholder", &env).unwrap(), "no placeholder");
        // an unrelated `${...}` is left alone
        assert_eq!(expand("${other}", &env).unwrap(), "${other}");
    }

    #[test]
    fn an_unset_variable_is_an_error_only_when_used() {
        let nothing = |_: &str| None;
        assert_eq!(expand("plain", &nothing).unwrap(), "plain");
        let error = expand("${WORK}/x", &nothing).unwrap_err();
        assert!(error.contains(WORK_ENV), "{error}");
        let empty = |_: &str| Some(String::new());
        assert!(expand("${FIXTURES}", &empty).is_err());
    }

    #[test]
    fn control_characters_are_the_keys_they_stand_for() {
        assert_eq!(
            typed_keys("a\n\t\u{1b}\u{8}-"),
            [
                TypedKey::Char('a'),
                TypedKey::Return,
                TypedKey::Tab,
                TypedKey::Escape,
                TypedKey::Backspace,
                TypedKey::Char('-'),
            ]
        );
    }

    #[test]
    fn shortcuts_name_a_letter_and_shift_only_for_save_as() {
        assert_eq!(Shortcut::Save.letter(), 's');
        assert!(Shortcut::SaveAs.shift());
        assert!(!Shortcut::Save.shift());
        assert_eq!(Shortcut::CloseTab.letter(), 'w');
        assert_eq!(Shortcut::Quit.letter(), 'q');
        assert_eq!(DocKind::Markdown.home_kind(), "md");
        assert_eq!(DocKind::Mindmap.home_kind(), "xmind");
    }
}
