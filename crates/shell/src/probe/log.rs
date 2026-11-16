//! The raw record of a probe run: one JSON object per line in `<out>/<probe>.jsonl`, every
//! line stamped with milliseconds since the run began and the wall clock. Nothing here decides
//! anything; `ci/probe-check.mjs` reads the lines.
//!
//! Records come from four sources (`src`): `rust` (what the probe asked and got), `native`
//! (the shell's own event stream of a browser), `page` (what a fixture page reported through
//! the sink scheme) and `scheme` (a request one of the shell's schemes served).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

struct Recorder {
    file: Mutex<File>,
    started: Instant,
    dir: PathBuf,
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// Open `<dir>/<name>.jsonl`. Called once, before any webview exists.
pub fn init(dir: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = dir.join(format!("{name}.jsonl"));
    let file = File::create(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    RECORDER
        .set(Recorder { file: Mutex::new(file), started: Instant::now(), dir: dir.to_owned() })
        .map_err(|_| "the probe record was opened twice".to_owned())
}

/// Where the artifacts of the run (PDFs, PNGs) go.
pub fn out_dir() -> Option<&'static Path> {
    RECORDER.get().map(|recorder| recorder.dir.as_path())
}

/// Milliseconds since `init`.
pub fn now_ms() -> f64 {
    RECORDER.get().map_or(0.0, |recorder| recorder.started.elapsed().as_secs_f64() * 1000.0)
}

fn wall_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis())
}

/// Append one record. `fields` is an object (anything else is stored under `value`).
pub fn record(src: &str, ev: &str, fields: Value) {
    let Some(recorder) = RECORDER.get() else { return };
    let mut map = match fields {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("value".into(), other);
            map
        }
    };
    map.insert("t".into(), json!((now_ms() * 10.0).round() / 10.0));
    map.insert("wall".into(), json!(wall_ms()));
    map.insert("src".into(), json!(src));
    map.insert("ev".into(), json!(ev));
    let mut line = Value::Object(map).to_string();
    line.push('\n');
    let mut file = recorder.file.lock().unwrap_or_else(|error| error.into_inner());
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

/// One entry of a [`Timeline`].
#[derive(Clone, Debug)]
pub struct Rec {
    pub kind: String,
    pub v: Value,
}

/// An in-memory list of records a probe thread can wait on.
#[derive(Default)]
pub struct Timeline {
    recs: Mutex<Vec<Rec>>,
    changed: Condvar,
}

impl Timeline {
    pub fn push(&self, kind: &str, v: Value) {
        let rec = Rec { kind: kind.to_owned(), v };
        self.recs.lock().unwrap_or_else(|error| error.into_inner()).push(rec);
        self.changed.notify_all();
    }

    pub fn snapshot(&self) -> Vec<Rec> {
        self.recs.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    pub fn len(&self) -> usize {
        self.recs.lock().unwrap_or_else(|error| error.into_inner()).len()
    }

    /// Block until `ready` holds for the records so far, or `timeout` passes.
    pub fn wait(&self, timeout: Duration, ready: impl Fn(&[Rec]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut recs = self.recs.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if ready(&recs) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .changed
                .wait_timeout(recs, deadline - now)
                .unwrap_or_else(|error| error.into_inner());
            recs = guard;
        }
    }

    /// Wait for a record of `kind` at index `from` or later.
    pub fn wait_kind(&self, kind: &str, from: usize, timeout: Duration) -> bool {
        self.wait(timeout, |recs| recs.iter().skip(from).any(|rec| rec.kind == kind))
    }
}

/// What the fixture pages reported, by webview label (`view` of each record).
pub fn pages() -> &'static Timeline {
    static PAGES: LazyLock<Timeline> = LazyLock::new(Timeline::default);
    &PAGES
}

/// Wait for a page record `ev` from the webview `label`, newer than index `from` of [`pages`].
pub fn wait_page(label: &str, ev: &str, from: usize, timeout: Duration) -> Option<Rec> {
    let timeline = pages();
    timeline.wait(timeout, |recs| {
        recs.iter().skip(from).any(|rec| rec.kind == ev && rec.v["view"] == label)
    });
    timeline
        .snapshot()
        .into_iter()
        .skip(from)
        .find(|rec| rec.kind == ev && rec.v["view"] == label)
}
