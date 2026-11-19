use std::time::Duration;

use serde_json::Value;
use tauri::Webview;

use crate::native::WebviewExt;

/// How long the studio frame may take to say whether it takes a key
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// The script that asks the HWP studio frame whether it takes `key`
/// (`window.__ieumHancomKey(key) === true`, `apps/hwp/studio-plugin/hancom-keys.mjs`).
/// The studio is the frame whose address holds `/studio/`: the page itself or a same-origin
/// descendant of it. A frame of another origin is never reached, and no studio frame means
/// the page takes nothing.
fn takes_expression(key: &str) -> String {
    format!(
        "(() => {{
  const isStudio = (frame) => {{
    try {{ return frame.location.href.includes('/studio/'); }} catch {{ return false; }}
  }};
  const find = (frame) => {{
    for (let i = 0; i < frame.frames.length; i += 1) {{
      const child = frame.frames[i];
      if (isStudio(child)) return child;
      const deeper = find(child);
      if (deeper) return deeper;
    }}
    return null;
  }};
  const studio = isStudio(window) ? window : find(window);
  return !!studio && typeof studio.__ieumHancomKey === 'function'
    && studio.__ieumHancomKey({key}) === true;
}})()",
        key = Value::String(key.to_owned()),
    )
}

/// Whether the studio frame took `key`. Blocks for the answer: call it off the UI thread.
/// A frame that cannot answer in time leaves the key to the shell, and says so.
pub(super) fn editor_takes(webview: &Webview, key: &str) -> bool {
    let answer = webview.page().and_then(|page| {
        page.blocking(ANSWER_TIMEOUT, |done| {
            page.evaluate(&takes_expression(key), true, None, done)
        })
        .map_err(|error| error.to_string())
    });
    match answer {
        Ok(value) => value == Value::Bool(true),
        Err(error) => {
            eprintln!("shell: the HWP editor could not say whether it takes {key}: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expression_quotes_the_key_and_asks_the_studio_frame() {
        let expression = takes_expression("ctrl-shift-s");
        assert!(expression.contains("__ieumHancomKey(\"ctrl-shift-s\") === true"));
        assert!(expression.contains("includes('/studio/')"));
        assert!(expression.starts_with("(() => {"));
        assert!(expression.ends_with("})()"));
    }

    #[test]
    fn a_key_with_quotes_cannot_leave_its_string() {
        let expression = takes_expression("\") || window.x || (\"");
        assert!(expression.contains("__ieumHancomKey(\"\\\") || window.x || (\\\"\") === true"));
    }
}
