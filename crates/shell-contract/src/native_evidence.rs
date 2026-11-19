//! Receipts of executed synthetic native-contract behavior (tools/ipc-diff's
//! `--native-evidence`, tools/shell-inventory/native-channels.ts).
//!
//! A test that drove a native-only handler through some of its contract's
//! required scenarios calls [`record`] after its last assertion: a failing
//! assertion panics first, so a failed scenario never leaves a passed receipt.
//! Without `SEUOFFICE_NATIVE_EVIDENCE` nothing is written; with it, each call
//! writes the observed answers as an artifact and then a receipt naming it,
//! one pair of files per call, which tools/ipc-diff/native-evidence.mjs merges.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The folder the receipts go to; unset means no evidence is collected
pub const EVIDENCE_DIR: &str = "SEUOFFICE_NATIVE_EVIDENCE";

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Records that `channel` passed `scenarios` with `observed` (a JSON value, as
/// text: the answers and events the test asserted). Panics when the folder is
/// named but cannot be written, so a lost receipt fails the run.
pub fn record(channel: &str, scenarios: &[&str], observed_json: &str) {
    let Some(dir) = std::env::var_os(EVIDENCE_DIR).map(PathBuf::from) else {
        return;
    };
    let stem = format!(
        "{}-{}-{}",
        channel.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let artifact = dir.join(format!("{stem}.artifact.json"));
    let receipt = dir.join(format!("{stem}.receipt.json"));
    write(&artifact, observed_json);
    let scenarios = scenarios
        .iter()
        .map(|scenario| json_string(scenario))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &receipt,
        &format!(
            "{{\"channel\":{},\"kind\":\"synthetic\",\"status\":\"passed\",\"scenarios\":[{scenarios}],\"artifact\":{}}}",
            json_string(channel),
            json_string(&artifact.to_string_lossy()),
        ),
    );
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("native evidence folder {parent:?}: {error}"));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("native evidence {path:?}: {error}"));
}

/// A JSON string literal of `text`
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::json_string;

    #[test]
    fn json_strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(json_string(r#"C:\a "b""#), r#""C:\\a \"b\"""#);
        assert_eq!(json_string("a\nb"), "\"a\\u000ab\"");
    }
}
