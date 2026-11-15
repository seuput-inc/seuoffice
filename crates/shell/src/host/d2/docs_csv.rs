//! CSV import rules from packages/xlsx-gateway/src/gateway/csv-import.ts.

use std::collections::HashMap;

const ROWS: usize = 1_048_576;
const COLS: usize = 16_384;

fn declaration(input: &[u16]) -> (&[u16], Option<u16>) {
    let start = usize::from(input.first() == Some(&0xfeff));
    let s = &input[start..];
    if s.len() >= 6 && s[0..3].iter().zip([b's', b'e', b'p']).all(|(u, b)| *u == b as u16 || *u == b.to_ascii_uppercase() as u16)
        && s[3] == b'=' as u16 && s[4] != b'\r' as u16 && s[4] != b'\n' as u16
    {
        if s.get(5) == Some(&(b'\n' as u16)) { return (&s[6..], Some(s[4])); }
        if s.get(5) == Some(&(b'\r' as u16)) && s.get(6) == Some(&(b'\n' as u16)) { return (&s[7..], Some(s[4])); }
    }
    (input, None)
}

fn sniff(input: &[u16]) -> u16 {
    let (input, declared) = declaration(input);
    if let Some(delimiter) = declared { return delimiter; }
    let mut counts = [0usize; 3];
    let mut quoted = false;
    let mut line = 0;
    let mut i = 0;
    while i < input.len().min(65_536) && line < 20 {
        let c = input[i];
        if c == b'\n' as u16 { line += 1; }
        if c == b'"' as u16 {
            if quoted && input.get(i + 1) == Some(&(b'"' as u16)) { i += 1; } else { quoted = !quoted; }
        } else if !quoted {
            for (at, delimiter) in [b',', b';', b'\t'].into_iter().enumerate() {
                if c == delimiter as u16 { counts[at] += 1; }
            }
        }
        i += 1;
    }
    let mut best = 0;
    for i in 1..3 { if counts[i] > counts[best] { best = i; } }
    [b',', b';', b'\t'][best] as u16
}

fn parse(input: &[u16], delimiter: u16) -> Result<Vec<Vec<String>>, String> {
    let (input, _) = declaration(input);
    let input = input.strip_prefix(&[0xfeff]).unwrap_or(input);
    let mut rows = Vec::new();
    let mut held = Vec::new();
    let mut row = Vec::new();
    let mut field = Vec::new();
    let mut quoted = false;
    let mut count = 0usize;
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        if quoted {
            if c == b'"' as u16 {
                if input.get(i + 1) == Some(&(b'"' as u16)) { field.push(c); i += 1; }
                else { quoted = false; }
            } else if c == b'\r' as u16 {
                if input.get(i + 1) == Some(&(b'\n' as u16)) { i += 1; }
                field.push(b'\n' as u16);
            } else { field.push(c); }
        } else if c == b'"' as u16 && field.is_empty() {
            quoted = true;
        } else if c == delimiter || c == b'\n' as u16 || c == b'\r' as u16 {
            row.push(String::from_utf16_lossy(&field));
            field.clear();
            if row.len() > COLS { return Err(format!("CSV import rejected: too many columns (cap {COLS})")); }
            if c != delimiter {
                if c == b'\r' as u16 && input.get(i + 1) == Some(&(b'\n' as u16)) { i += 1; }
                count += 1;
                if count > ROWS { return Err(format!("CSV import rejected: too many rows (cap {ROWS})")); }
                let finished = std::mem::take(&mut row);
                if finished.iter().all(String::is_empty) { held.push(finished); }
                else { rows.append(&mut held); rows.push(finished); }
            }
        } else { field.push(c); }
        i += 1;
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(String::from_utf16_lossy(&field));
        if row.len() > COLS { return Err(format!("CSV import rejected: too many columns (cap {COLS})")); }
        count += 1;
        if count > ROWS { return Err(format!("CSV import rejected: too many rows (cap {ROWS})")); }
        if !row.iter().all(String::is_empty) { rows.append(&mut held); rows.push(row); }
    }
    Ok(rows)
}

pub(super) fn rows(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    let text = file_parse::csv::decode_csv_buffer(bytes, None);
    let input: Vec<u16> = text.encode_utf16().collect();
    if let (_, Some(delimiter)) = declaration(&input) { return parse(&input, delimiter); }
    let delimiter = sniff(&input);
    if delimiter == b',' as u16 { return parse(&input, delimiter); }
    let guessed = parse(&input, delimiter)?;
    if guessed.first().is_none_or(|row| row.len() != 1) { return Ok(guessed); }
    let body = &guessed[1..];
    let mut widths = HashMap::new();
    for row in body { if row.len() > 1 { *widths.entry(row.len()).or_insert(0usize) += 1; } }
    let dominant = widths.values().copied().max().unwrap_or(0);
    if dominant >= 2 && (dominant as f64) >= (body.len() as f64) * 0.6 { return Ok(guessed); }
    let comma = parse(&input, b',' as u16)?;
    if comma.iter().all(|row| row.len() == 1) { Ok(comma) } else { Ok(guessed) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_honours_delimiter_quotes_and_trailing_empty_rows() {
        let parsed = rows(b"sep=;\r\nName;Note\r\nA;\"hello;\r\nworld\"\r\n\r\n").unwrap();
        assert_eq!(parsed, vec![vec!["Name", "Note"], vec!["A", "hello;\nworld"]]);
    }

    #[test]
    fn prose_is_not_split_on_inconsistent_semicolons() {
        let parsed = rows(b"Title\nhello; world\none; two; three\nend\n").unwrap();
        assert_eq!(parsed, vec![vec!["Title"], vec!["hello; world"], vec!["one; two; three"], vec!["end"]]);
    }
}
