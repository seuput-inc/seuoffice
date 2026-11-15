//! Raw stream pass corresponding to pdf-crypt.ts scanRawObjects/predecryptStreams.
use std::collections::{HashMap, HashSet};
use jscompat::js_regex;

struct RawObject { number: f64, generation: f64, dict_start: usize, dict_end: usize, data: Option<(usize, usize)> }
struct Edit { start: usize, end: usize, replacement: Vec<u8> }
fn bits(value: f64) -> u64 { if value == 0.0 { 0 } else { value.to_bits() } }

pub(super) fn run(bytes: &[u8], encrypt_metadata: bool, mut transform: impl FnMut(f64, f64, &[u8]) -> Result<Vec<u8>, String>) -> Result<(Vec<u8>, HashSet<u64>), String> {
    let text: String = bytes.iter().map(|byte| char::from(*byte)).collect();
    // UTF-8 string boundaries -> offsets of the original Latin-1 bytes.
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let positions: Vec<usize> = text.char_indices().map(|(offset, _)| offset).chain(std::iter::once(text.len())).collect();
    for (index, byte) in bytes.iter().enumerate() { offsets.extend(std::iter::repeat_n(index, char::from(*byte).len_utf8())); }
    offsets.push(bytes.len());
    let header = js_regex!(r"(?:^|[\r\n\s])(\d+)\s+(\d+)\s+obj\b", "g");
    let headers: Vec<_> = header.find_iter(&text).map(|m| (m.start(), m.end(), jscompat::to_number(m.get(1).unwrap_or("")), jscompat::to_number(m.get(2).unwrap_or("")))).collect();
    let stream = js_regex!(r"\bstream(\r\n|\n|\r)");
    let indirect = js_regex!(r"/Length\s+(\d+)\s+(\d+)\s+R");
    let direct = js_regex!(r"/Length\s+(\d+)");
    let end_stream = js_regex!(r"^\s*endstream");
    let mut objects = Vec::new();
    for (index, &(_, start, number, generation)) in headers.iter().enumerate() {
        let limit = headers.get(index + 1).map_or(text.len(), |header| header.0);
        let slice = &text[start..limit];
        let Some(found) = stream.exec(slice) else {
            let end = start + slice.find("endobj").unwrap_or(slice.len());
            objects.push(RawObject { number, generation, dict_start: start, dict_end: end, data: None });
            continue;
        };
        let dict_end = start + found.start();
        let data_text = start + found.end();
        let dict = &text[start..dict_end];
        let data_start = offsets[data_text];
        let indirect_match = indirect.exec(dict);
        let direct_match = if indirect_match.is_none() { direct.exec(dict) } else { None };
        let candidate = direct_match.as_ref().and_then(|m| m.get(1)).map(jscompat::to_number)
            .filter(|n| *n >= 0.0 && *n <= usize::MAX as f64).and_then(|n| data_start.checked_add(n as usize))
            .filter(|end| *end <= bytes.len());
        let end = candidate.filter(|&end| {
            let text_at = positions[end];
            let tail = positions[end.saturating_add(24).min(bytes.len())];
            end_stream.test(&text[text_at..tail])
        }).or_else(|| {
            let found = text[data_text..].find("endstream")? + data_text;
            let mut end = offsets[found];
            if end >= 2 && &bytes[end-2..end] == b"\r\n" { end -= 2; }
            else if end > 0 && matches!(bytes[end-1], b'\r' | b'\n') { end -= 1; }
            Some(end)
        });
        objects.push(RawObject { number, generation, dict_start: start, dict_end, data: end.map(|end| (data_start, end)) });
    }
    let mut edits = Vec::new();
    let mut indirect_lengths = HashMap::new();
    for object in &objects {
        let Some((start, end)) = object.data else { continue };
        let dict = &text[object.dict_start..object.dict_end];
        if js_regex!(r"/Type\s*/XRef\b").test(dict) || (!encrypt_metadata && js_regex!(r"/Type\s*/Metadata\b").test(dict)) { continue; }
        let plain = transform(object.number, object.generation, &bytes[start..end])?;
        let length = plain.len();
        edits.push(Edit { start, end, replacement: plain });
        if let Some(m) = indirect.exec(dict) {
            indirect_lengths.insert(bits(jscompat::to_number(m.get(1).unwrap_or(""))), length);
        } else if let Some(m) = direct.exec(dict) {
            if let Some(range) = m.range(1) {
                edits.push(Edit { start: offsets[object.dict_start + range.start], end: offsets[object.dict_start + range.end], replacement: length.to_string().into_bytes() });
            }
        }
    }
    let integer = js_regex!(r"^-?\d+$");
    for object in &objects {
        let Some(length) = indirect_lengths.get(&bits(object.number)) else { continue };
        if object.data.is_some() { continue; }
        let raw = &text[object.dict_start..object.dict_end];
        let body = jscompat::trim(raw);
        if !integer.test(body) { continue; }
        let offset = body.as_ptr() as usize - raw.as_ptr() as usize;
        edits.push(Edit { start: offsets[object.dict_start + offset], end: offsets[object.dict_start + offset + body.len()], replacement: length.to_string().into_bytes() });
    }
    let top = objects.iter().map(|object| bits(object.number)).collect();
    edits.sort_by_key(|edit| edit.start);
    let mut result = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor || edit.end > bytes.len() { return Err("overlapping PDF stream edits".into()); }
        result.extend_from_slice(&bytes[cursor..edit.start]); result.extend_from_slice(&edit.replacement); cursor = edit.end;
    }
    result.extend_from_slice(&bytes[cursor..]);
    Ok((result, top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_direct_and_indirect_lengths_and_skips_metadata() {
        let source = b"%PDF-1.7\n1 0 obj << /Length 4 >> stream\nABCD\nendstream endobj\n2 0 obj << /Length 3 0 R >> stream\nEFGH\nendstream endobj\n3 0 obj 4 endobj\n4 0 obj << /Type /Metadata /Length 4 >> stream\nIJKL\nendstream endobj";
        let (result, top) = run(source, false, |_, _, payload| Ok(payload[..2].to_vec())).unwrap();
        assert!(top.contains(&bits(1.0)) && top.contains(&bits(4.0)));
        assert!(result.windows(b"/Length 2 >> stream\nAB\nendstream".len()).any(|w| w == b"/Length 2 >> stream\nAB\nendstream"));
        assert!(result.windows(b"3 0 obj 2 endobj".len()).any(|w| w == b"3 0 obj 2 endobj"));
        assert!(result.windows(b"stream\nIJKL\nendstream".len()).any(|w| w == b"stream\nIJKL\nendstream"));
    }
}
