//! CSV row scanning from packages/xlsx-gateway/src/gateway/csv-import.ts.

use shell_contract::{IpcError, IpcValue};
use shell_ipc_d1::sheets::engine::{EngineResult, ImportControl, engine_error};
use std::collections::HashMap;

const MAX_ROWS: usize = 1_048_576;
const MAX_COLUMNS: usize = 16_384;
const PROGRESS_INTERVAL: usize = 256 * 1024;

fn declaration(input: &str) -> (&str, Option<char>) {
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let Some(head) = text.get(..4) else {
        return (input, None);
    };
    if !head.eq_ignore_ascii_case("sep=") {
        return (input, None);
    }
    let rest = &text[4..];
    let Some(delimiter) = rest.chars().next() else {
        return (input, None);
    };
    if delimiter == '\r' || delimiter == '\n' {
        return (input, None);
    }
    let tail = &rest[delimiter.len_utf8()..];
    let Some(tail) = tail
        .strip_prefix("\r\n")
        .or_else(|| tail.strip_prefix('\n'))
    else {
        return (input, None);
    };
    (tail, Some(delimiter))
}

pub(super) fn cancelled(control: Option<&ImportControl<'_>>) -> EngineResult<()> {
    if control.is_some_and(|control| (control.cancelled)()) {
        Err(IpcError {
            name: Some("AbortError".into()),
            message: "This operation was aborted".into(),
        })
    } else {
        Ok(())
    }
}

pub(super) fn progress(control: Option<&ImportControl<'_>>, phase: &str, fraction: f64) {
    if let Some(control) = control {
        (control.on_progress)(IpcValue::Object(vec![
            ("phase".into(), IpcValue::String(phase.into())),
            ("fraction".into(), IpcValue::Number(fraction)),
        ]));
    }
}

/// Calls `row` once per non-trailing row, including blank rows in the middle.
pub(super) fn rows(
    input: &str,
    delimiter: char,
    control: Option<&ImportControl<'_>>,
    mut row: impl FnMut(Vec<String>) -> EngineResult<()>,
) -> EngineResult<()> {
    let (text, _) = declaration(input);
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    cancelled(control)?;
    let total = input.encode_utf16().count().max(1);
    let mut chars = text.chars().peekable();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut held: Vec<Vec<String>> = Vec::new();
    let mut quoted = false;
    let mut count = 0usize;
    let mut offset = 0usize;
    let mut next_report = PROGRESS_INTERVAL;
    while let Some(ch) = chars.next() {
        if offset >= next_report {
            cancelled(control)?;
            progress(control, "read", offset as f64 / total as f64);
            next_report = offset + PROGRESS_INTERVAL;
        }
        offset += ch.len_utf16();
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    offset += 1;
                    cell.push('"');
                }
                '"' => quoted = false,
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                        offset += 1;
                    }
                    cell.push('\n');
                }
                _ => cell.push(ch),
            }
            continue;
        }
        if ch == '"' && cell.is_empty() {
            quoted = true;
            continue;
        }
        if ch == delimiter {
            cells.push(std::mem::take(&mut cell));
            if cells.len() > MAX_COLUMNS {
                return Err(engine_error(format!(
                    "CSV import rejected: too many columns (cap {MAX_COLUMNS})"
                )));
            }
        } else if ch == '\n' || ch == '\r' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
                offset += 1;
            }
            cells.push(std::mem::take(&mut cell));
            if cells.len() > MAX_COLUMNS {
                return Err(engine_error(format!(
                    "CSV import rejected: too many columns (cap {MAX_COLUMNS})"
                )));
            }
            count += 1;
            if count > MAX_ROWS {
                return Err(engine_error(format!(
                    "CSV import rejected: too many rows (cap {MAX_ROWS})"
                )));
            }
            let finished = std::mem::take(&mut cells);
            if finished.iter().all(String::is_empty) {
                held.push(finished);
            } else {
                for pending in held.drain(..) {
                    row(pending)?;
                }
                row(finished)?;
            }
        } else {
            cell.push(ch);
        }
    }
    if !cell.is_empty() || !cells.is_empty() {
        cells.push(cell);
        if cells.len() > MAX_COLUMNS {
            return Err(engine_error(format!(
                "CSV import rejected: too many columns (cap {MAX_COLUMNS})"
            )));
        }
        count += 1;
        if count > MAX_ROWS {
            return Err(engine_error(format!(
                "CSV import rejected: too many rows (cap {MAX_ROWS})"
            )));
        }
        if cells.iter().any(|cell| !cell.is_empty()) {
            for pending in held {
                row(pending)?;
            }
            row(cells)?;
        }
    }
    Ok(())
}

pub(super) fn sniff(input: &str) -> char {
    let (text, declared) = declaration(input);
    if let Some(declared) = declared {
        return declared;
    }
    let mut counts = [0usize; 3];
    let mut quoted = false;
    let mut lines = 0usize;
    let mut offset = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if offset >= 64 * 1024 || lines >= 20 {
            break;
        }
        offset += ch.len_utf16();
        if ch == '\n' {
            lines += 1;
            continue;
        }
        if ch == '"' {
            if quoted && chars.peek() == Some(&'"') {
                chars.next();
                offset += 1;
            } else {
                quoted = !quoted;
            }
        } else if !quoted {
            if let Some(index) = [',', ';', '\t']
                .iter()
                .position(|&candidate| candidate == ch)
            {
                counts[index] += 1;
            }
        }
    }
    let mut best = 0;
    for index in 1..3 {
        if counts[index] > counts[best] {
            best = index;
        }
    }
    [',', ';', '\t'][best]
}

pub(super) fn delimiter(input: &str) -> EngineResult<char> {
    if let (_, Some(declared)) = declaration(input) {
        return Ok(declared);
    }
    let sniffed = sniff(input);
    if sniffed == ',' {
        return Ok(sniffed);
    }
    let mut widths = HashMap::<usize, usize>::new();
    let mut first = None;
    let mut body = 0usize;
    rows(input, sniffed, None, |row| {
        if first.is_none() {
            first = Some(row.len());
        } else {
            body += 1;
            if row.len() > 1 {
                *widths.entry(row.len()).or_default() += 1;
            }
        }
        Ok(())
    })?;
    if first != Some(1) {
        return Ok(sniffed);
    }
    let dominant = widths.values().copied().max().unwrap_or(0);
    if dominant >= 2 && (dominant as f64) >= body as f64 * 0.6 {
        return Ok(sniffed);
    }
    let mut all_single = true;
    rows(input, ',', None, |row| {
        if row.len() != 1 {
            all_single = false;
        }
        Ok(())
    })?;
    Ok(if all_single { ',' } else { sniffed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_quoted_newlines_and_internal_blank_rows() {
        let mut output = Vec::new();
        rows("sep=;\nA;\"B\r\nC\"\n;\nD;E\n\n", ';', None, |row| {
            output.push(row);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            output,
            vec![vec!["A", "B\nC"], vec!["", ""], vec!["D", "E"]]
        );
        assert_eq!(
            delimiter("A prose title\nfirst;second\nthird;fourth\n").unwrap(),
            ';'
        );
        assert_eq!(
            delimiter("A prose title\nwords; not a table\nmore words\n").unwrap(),
            ','
        );
    }
}
