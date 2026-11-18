//! Token-aware stylesheet URL rewrites with original non-URL bytes retained.
use cssparser::{Parser, ParserInput, Token};
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use tauri::Url;

use super::map_url;

struct Edit { start: usize, end: usize, replacement: String }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Context { Normal, Url, ImageSet }

fn quoted(value: &str) -> String {
    let mut out = String::new();
    cssparser::serialize_string(value, &mut out).expect("writing to String cannot fail");
    out
}

fn collect<'i>(parser: &mut Parser<'i, '_>, base: Option<&Url>, prefix: &str, context: Context, edits: &mut Vec<Edit>) {
    let mut importing = false;
    loop {
        let start = parser.position().byte_index();
        let token = match parser.next_including_whitespace_and_comments() {
            Ok(token) => token.clone(),
            Err(_) => break,
        };
        let end = parser.position().byte_index();
        match &token {
            Token::AtKeyword(name) => importing = name.eq_ignore_ascii_case("import"),
            Token::WhiteSpace(_) | Token::Comment(_) => {},
            Token::UnquotedUrl(value) => {
                if let Some(mapped) = map_url(value, base, prefix) {
                    edits.push(Edit { start, end, replacement: format!("url({})", quoted(&mapped)) });
                }
                importing = false;
            }
            Token::QuotedString(value) if importing || context == Context::Url || context == Context::ImageSet => {
                if let Some(mapped) = map_url(value, base, prefix) {
                    edits.push(Edit { start, end, replacement: quoted(&mapped) });
                }
                importing = false;
            }
            Token::Function(name) => {
                let nested = if name.eq_ignore_ascii_case("url") { Context::Url }
                    else if name.eq_ignore_ascii_case("image-set") || name.eq_ignore_ascii_case("-webkit-image-set") { Context::ImageSet }
                    else { Context::Normal };
                let _ = parser.parse_nested_block(|inside| {
                    collect(inside, base, prefix, nested, edits);
                    Ok::<(), cssparser::ParseError<'i, ()>>(())
                });
                importing = false;
            }
            Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                let _ = parser.parse_nested_block(|inside| {
                    collect(inside, base, prefix, Context::Normal, edits);
                    Ok::<(), cssparser::ParseError<'i, ()>>(())
                });
                importing = false;
            }
            _ => importing = false,
        }
    }
}

pub(super) fn text(input: &str, base: Option<&Url>, prefix: &str) -> String {
    let mut data = ParserInput::new(input);
    let mut parser = Parser::new(&mut data);
    let mut edits = Vec::new();
    collect(&mut parser, base, prefix, Context::Normal, &mut edits);
    if edits.is_empty() { return input.to_owned(); }
    let mut output = String::with_capacity(input.len());
    let mut last = 0;
    for edit in edits {
        output.push_str(&input[last..edit.start]);
        output.push_str(&edit.replacement);
        last = edit.end;
    }
    output.push_str(&input[last..]);
    output
}

fn charset_label(content_type: Option<&str>) -> Option<&str> {
    content_type?.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['\"', '\'']))
    })
}

fn stylesheet_encoding(bytes: &[u8], content_type: Option<&str>) -> (&'static Encoding, usize) {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) { return (encoding, bom); }
    if let Some(encoding) = charset_label(content_type).and_then(|label| Encoding::for_label(label.as_bytes())) {
        return (encoding, 0);
    }
    if let Some(rest) = bytes.strip_prefix(b"@charset \"") {
        if let Some(end) = rest.windows(2).position(|pair| pair == b"\";") {
            if let Some(encoding) = Encoding::for_label(&rest[..end]) { return (encoding, 0); }
        }
    }
    (UTF_8, 0)
}

// Translate UTF-8 parser offsets back to source bytes. URL boundaries occur at
// CSS token boundaries, so multibyte characters are never split by an edit.
fn source_offsets(bytes: &[u8], encoding: &'static Encoding, bom: usize) -> (String, Vec<usize>) {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut text = String::new();
    let mut offsets = vec![bom];
    let mut last_end = bom;
    for index in bom..bytes.len() {
        let mut buffer = [0u8; 32];
        let (_, _, written, _) = decoder.decode_to_utf8(&bytes[index..index + 1], &mut buffer, false);
        if written == 0 { continue; }
        let chunk = std::str::from_utf8(&buffer[..written]).expect("decoder emits UTF-8");
        let count = chunk.chars().count();
        for (i, ch) in chunk.chars().enumerate() {
            let end = if i + 1 == count { index + 1 } else { index };
            let start = last_end;
            text.push(ch);
            for _ in 1..ch.len_utf8() { offsets.push(start); }
            offsets.push(end);
            last_end = end;
        }
    }
    let mut buffer = [0u8; 32];
    let (_, _, written, _) = decoder.decode_to_utf8(&[], &mut buffer, true);
    if written > 0 {
        let chunk = std::str::from_utf8(&buffer[..written]).expect("decoder emits UTF-8");
        for ch in chunk.chars() {
            let start = last_end;
            text.push(ch);
            for _ in 1..ch.len_utf8() { offsets.push(start); }
            offsets.push(bytes.len());
            last_end = bytes.len();
        }
    }
    (text, offsets)
}

fn encode_replacement(text: &str, encoding: &'static Encoding) -> Vec<u8> {
    if encoding == UTF_16LE { return text.encode_utf16().flat_map(u16::to_le_bytes).collect(); }
    if encoding == UTF_16BE { return text.encode_utf16().flat_map(u16::to_be_bytes).collect(); }
    encoding.encode(text).0.into_owned()
}

pub(super) fn bytes(input: &[u8], base: Option<&Url>, prefix: &str, content_type: Option<&str>) -> Result<Vec<u8>, String> {
    let (encoding, bom) = stylesheet_encoding(input, content_type);
    let (decoded, offsets) = source_offsets(input, encoding, bom);
    let mut data = ParserInput::new(&decoded);
    let mut parser = Parser::new(&mut data);
    let mut edits = Vec::new();
    collect(&mut parser, base, prefix, Context::Normal, &mut edits);
    if edits.is_empty() { return Ok(input.to_vec()); }
    let mut output = Vec::with_capacity(input.len());
    let mut last = 0;
    for edit in edits {
        let start = *offsets.get(edit.start).ok_or("invalid CSS token start")?;
        let end = *offsets.get(edit.end).ok_or("invalid CSS token end")?;
        if start < last || end < start || end > input.len() { return Err("invalid CSS token offsets".into()); }
        output.extend_from_slice(&input[last..start]);
        output.extend_from_slice(&encode_replacement(&edit.replacement, encoding));
        last = end;
    }
    output.extend_from_slice(&input[last..]);
    Ok(output)
}
