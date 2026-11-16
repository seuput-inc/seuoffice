//! Tolerant pdf-lib-style field and page traversal.
use super::super::super::crypto::pdf_lib::{Context, Object, Parser, RefKey};
use std::collections::HashSet;

type Dict = [(Vec<u8>, Object)];
pub(super) struct Field<'a> {
    pub name: String,
    pub value: &'a Dict,
    pub widget: &'a Dict,
    pub reference: Option<RefKey>,
}
fn get<'a>(dict: &'a Dict, key: &[u8]) -> Option<&'a Object> {
    dict.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}
fn resolve<'a>(ctx: &'a Context, object: Option<&'a Object>) -> Option<&'a Object> {
    ctx.lookup(object?)
}
fn dict<'a>(ctx: &'a Context, object: Option<&'a Object>) -> Option<&'a Dict> {
    match resolve(ctx, object)? {
        Object::Dictionary(v) => Some(v),
        _ => None,
    }
}
fn array<'a>(ctx: &'a Context, object: Option<&'a Object>) -> Option<&'a [Object]> {
    match resolve(ctx, object)? {
        Object::Array(v) => Some(v),
        _ => None,
    }
}
fn reference(value: &Object) -> Option<RefKey> {
    match value {
        Object::Reference(n, g) => Some(RefKey::new(*n, *g)),
        _ => None,
    }
}
pub(super) fn item<'a>(ctx: &'a Context, dict: &'a Dict, key: &[u8]) -> Option<&'a Object> {
    resolve(ctx, get(dict, key))
}
pub(super) fn name<'a>(ctx: &'a Context, dict: &'a Dict, key: &[u8]) -> Option<&'a str> {
    match item(ctx, dict, key)? {
        Object::Name(v) => std::str::from_utf8(v).ok(),
        _ => None,
    }
}
fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]) {
        let be = bytes[0] == 0xfe;
        let units = bytes[2..]
            .chunks(2)
            .map(|p| {
                if be {
                    u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)])
                } else {
                    u16::from_le_bytes([p[0], *p.get(1).unwrap_or(&0)])
                }
            })
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units);
    }
    const LOW: [char; 8] = [
        '\u{02d8}', '\u{02c7}', '\u{02c6}', '\u{02d9}', '\u{02dd}', '\u{02db}', '\u{02da}',
        '\u{02dc}',
    ];
    const HIGH: [char; 33] = [
        '\u{fffd}', '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}',
        '\u{0192}', '\u{2044}', '\u{2039}', '\u{203a}', '\u{2212}', '\u{2030}', '\u{201e}',
        '\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}', '\u{201a}', '\u{2122}', '\u{fb01}',
        '\u{fb02}', '\u{0141}', '\u{0152}', '\u{0160}', '\u{0178}', '\u{017d}', '\u{0131}',
        '\u{0142}', '\u{0153}', '\u{0161}', '\u{017e}', '\u{fffd}',
    ];
    bytes
        .iter()
        .map(|b| match *b {
            0x16 => '\u{17}',
            0x18..=0x1f => LOW[usize::from(*b - 0x18)],
            0x7f..=0x9f => HIGH[usize::from(*b - 0x7f)],
            0xa0 => '\u{20ac}',
            0xad => '\u{fffd}',
            other => char::from(other),
        })
        .collect()
}
pub(super) fn bytes(object: &Object) -> Option<Vec<u8>> {
    match object {
        Object::String(raw) => Some(super::super::super::pdf_codec::literal_bytes(raw)),
        Object::HexString(raw) => Some(super::super::super::pdf_codec::hex_bytes(raw)),
        _ => None,
    }
}
pub(super) fn text(ctx: &Context, dict: &Dict, key: &[u8]) -> Option<String> {
    bytes(item(ctx, dict, key)?).map(|b| decode_text(&b))
}
pub(super) fn number(ctx: &Context, dict: &Dict, key: &[u8]) -> Option<f64> {
    match item(ctx, dict, key)? {
        Object::Number(v) => Some(*v),
        _ => None,
    }
}
pub(super) fn context(bytes: &[u8]) -> Option<Context> {
    let ctx = Parser::new(bytes).document().ok()?;
    if !matches!(
        resolve(&ctx, ctx.encrypt.as_ref()),
        None | Some(Object::Null)
    ) {
        return None;
    }
    Some(ctx)
}
pub(super) fn fields(ctx: &Context) -> Vec<Field<'_>> {
    fn visit<'a>(
        ctx: &'a Context,
        node: &'a Object,
        parent: &str,
        inherited: Option<&'a str>,
        seen: &mut HashSet<usize>,
        out: &mut Vec<Field<'a>>,
    ) {
        let Some(d) = dict(ctx, Some(node)) else {
            return;
        };
        let id = d.as_ptr() as usize;
        if !seen.insert(id) {
            return;
        }
        let partial = text(ctx, d, b"T").filter(|s| !s.is_empty());
        let name = match partial {
            Some(partial) if !parent.is_empty() => format!("{parent}.{partial}"),
            Some(partial) => partial,
            None => parent.to_owned(),
        };
        let kind = name_value(ctx, d, b"FT").or(inherited);
        if kind == Some("Sig") {
            if let Some(value) = dict(ctx, get(d, b"V")) {
                if get(value, b"ByteRange").is_some() {
                    out.push(Field {
                        name: name.clone(),
                        value,
                        widget: d,
                        reference: reference(node),
                    });
                }
            }
        }
        if let Some(kids) = array(ctx, get(d, b"Kids")) {
            for child in kids {
                visit(ctx, child, &name, kind, seen, out);
            }
        }
    }
    fn name_value<'a>(ctx: &'a Context, d: &'a Dict, key: &[u8]) -> Option<&'a str> {
        name(ctx, d, key)
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(catalog) = dict(ctx, ctx.root.as_ref()) {
        if let Some(acro) = dict(ctx, get(catalog, b"AcroForm")) {
            if let Some(fields) = array(ctx, get(acro, b"Fields")) {
                for node in fields {
                    visit(ctx, node, "", None, &mut seen, &mut out);
                }
            }
        }
    }
    out
}
pub(super) fn page_index(ctx: &Context, field: &Field<'_>) -> i32 {
    fn walk<'a>(
        ctx: &'a Context,
        node: &'a Object,
        seen: &mut HashSet<usize>,
        out: &mut Vec<(&'a Dict, Option<RefKey>)>,
    ) {
        let Some(d) = dict(ctx, Some(node)) else {
            return;
        };
        if !seen.insert(d.as_ptr() as usize) {
            return;
        }
        if name(ctx, d, b"Type") == Some("Page") {
            out.push((d, reference(node)));
            return;
        }
        if let Some(kids) = array(ctx, get(d, b"Kids")) {
            for kid in kids {
                walk(ctx, kid, seen, out);
            }
        }
    }
    let mut pages = Vec::new();
    if let Some(root) = dict(ctx, ctx.root.as_ref()) {
        if let Some(node) = get(root, b"Pages") {
            walk(ctx, node, &mut HashSet::new(), &mut pages);
        }
    }
    if let Some(pref) = get(field.widget, b"P").and_then(reference) {
        if let Some(index) = pages.iter().position(|(_, r)| *r == Some(pref)) {
            return index as i32;
        }
    }
    if let Some(target) = field.reference {
        for (index, (page, _)) in pages.iter().enumerate() {
            if array(ctx, get(page, b"Annots"))
                .is_some_and(|annots| annots.iter().any(|item| reference(item) == Some(target)))
            {
                return index as i32;
            }
        }
    }
    -1
}
pub(super) fn byte_range(ctx: &Context, dict: &Dict) -> Option<[usize; 4]> {
    let Object::Array(values) = item(ctx, dict, b"ByteRange")? else {
        return None;
    };
    if values.len() != 4 {
        return None;
    }
    let mut out = [0; 4];
    for (i, v) in values.iter().enumerate() {
        let Object::Number(n) = v else { return None };
        if !n.is_finite() || *n < 0.0 || n.fract() != 0.0 || *n > usize::MAX as f64 {
            return None;
        }
        out[i] = *n as usize;
    }
    Some(out)
}
pub(super) fn contents(ctx: &Context, dict: &Dict) -> Option<Vec<u8>> {
    bytes(item(ctx, dict, b"Contents")?)
}
pub(super) fn certified(ctx: &Context, value: &Dict) -> Option<u8> {
    let refs = array(ctx, get(value, b"Reference"))?;
    for entry in refs {
        let Some(d) = dict(ctx, Some(entry)) else {
            continue;
        };
        if name(ctx, d, b"TransformMethod") != Some("DocMDP") {
            continue;
        }
        let level = dict(ctx, get(d, b"TransformParams"))
            .and_then(|p| number(ctx, p, b"P"))
            .unwrap_or(2.0);
        return Some(if level == 1.0 {
            1
        } else if level == 3.0 {
            3
        } else {
            2
        });
    }
    None
}
