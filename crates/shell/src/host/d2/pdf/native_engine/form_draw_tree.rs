//! Form XObject paint census and safe page-object lifting.
use std::collections::{HashMap, HashSet};

use jscompat::js_regex;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde_json::{Value, json};

use super::super::crypto::pdf_lib::{self, Object as PdfLibObject};
use super::super::pdfium_session::{PdfiumSession, wire_number};

pub(super) struct FormDrawTree {
    pages: Vec<Option<Vec<ObjectId>>>,
    children: HashMap<ObjectId, Option<Vec<ObjectId>>>,
    draws: HashMap<ObjectId, usize>,
}

fn dictionary<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Dictionary> {
    doc.dereference(value).ok()?.1.as_dict().ok()
}

fn page_resources(doc: &Document, page: ObjectId) -> Option<&Dictionary> {
    let mut current = page;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        let node = doc.get_dictionary(current).ok()?;
        if let Ok(value) = node.get(b"Resources") {
            return dictionary(doc, value);
        }
        current = node.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn codec_value(doc: &Document, value: &Object) -> Option<PdfLibObject> {
    match doc.dereference(value).ok()?.1 {
        Object::Null => Some(PdfLibObject::Null),
        Object::Name(name) => Some(PdfLibObject::Name(name.clone())),
        Object::Integer(number) if number.unsigned_abs() <= (1u64 << 53) => {
            Some(PdfLibObject::Number(*number as f64))
        }
        Object::Real(number) => Some(PdfLibObject::Number(f64::from(*number))),
        Object::Array(values) => Some(PdfLibObject::Array(
            values
                .iter()
                .map(|value| codec_value(doc, value))
                .collect::<Option<_>>()?,
        )),
        Object::Dictionary(dictionary) => {
            let mut fields = Vec::new();
            if let Ok(value) = dictionary.get(b"EarlyChange") {
                fields.push((b"EarlyChange".to_vec(), codec_value(doc, value)?));
            }
            Some(PdfLibObject::Dictionary(fields))
        }
        _ => None,
    }
}

fn decode_stream(doc: &Document, stream: &Stream) -> Option<Vec<u8>> {
    if stream.dict.get(b"Filter").is_err() {
        return Some(stream.content.clone());
    }
    let mut fields = Vec::new();
    for name in [b"Filter".as_slice(), b"DecodeParms".as_slice()] {
        if let Ok(value) = stream.dict.get(name) {
            fields.push((name.to_vec(), codec_value(doc, value)?));
        }
    }
    pdf_lib::stream::decode(&pdf_lib::Context::default(), &fields, &stream.content).ok()
}

fn page_content(doc: &Document, page: ObjectId) -> Option<Vec<u8>> {
    let page = doc.get_dictionary(page).ok()?;
    let Ok(contents) = page.get(b"Contents") else {
        return Some(Vec::new());
    };
    let contents = doc.dereference(contents).ok()?.1;
    let mut bytes = Vec::new();
    match contents {
        Object::Array(parts) => {
            for part in parts {
                let stream = doc.dereference(part).ok()?.1.as_stream().ok()?;
                bytes.extend(decode_stream(doc, stream)?);
                bytes.push(b'\n');
            }
        }
        Object::Stream(stream) => bytes.extend(decode_stream(doc, stream)?),
        _ => return None,
    }
    Some(bytes)
}

fn pdf_name(name: &str) -> Option<Vec<u8>> {
    let mut bytes = name
        .chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect::<Option<Vec<_>>>()?;
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let mut index = 0;
    let mut output = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (digit(bytes[index + 1]), digit(bytes[index + 2])) {
                bytes[output] = high * 16 + low;
                output += 1;
                index += 3;
                continue;
            }
        }
        bytes[output] = bytes[index];
        output += 1;
        index += 1;
    }
    bytes.truncate(output);
    Some(bytes)
}

fn form_children(
    doc: &Document,
    resources: Option<&Dictionary>,
    content: &[u8],
) -> Option<Vec<ObjectId>> {
    let text: String = content.iter().copied().map(char::from).collect();
    let code = js_regex!(r"\bBI\b[\s\S]*?\bEI\b", "g").replace_all(&text, |_| " ".to_owned());
    let xobjects = resources
        .and_then(|r| r.get(b"XObject").ok())
        .and_then(|value| dictionary(doc, value));
    let mut children = Vec::new();
    for found in js_regex!(r"/([^\s/\[\]<>(){}%]+)\s+Do\b", "g").find_iter(&code) {
        let Some(name) = found.get(1) else { continue };
        let Some(name) = pdf_name(name) else { continue };
        let Some(id) = xobjects
            .and_then(|dict| dict.get(&name).ok())
            .and_then(|value| value.as_reference().ok())
        else {
            continue;
        };
        let Some(stream) = doc
            .get_object(id)
            .ok()
            .and_then(|value| value.as_stream().ok())
        else {
            continue;
        };
        if stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            == Some(b"Form".as_slice())
        {
            children.push(id);
        }
    }
    Some(children)
}

fn cache_children(
    doc: &Document,
    id: ObjectId,
    cache: &mut HashMap<ObjectId, Option<Vec<ObjectId>>>,
) {
    cache.entry(id).or_insert_with(|| {
        doc.get_object(id)
            .ok()
            .and_then(|value| value.as_stream().ok())
            .and_then(|stream: &Stream| {
                let resources = stream
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|value| dictionary(doc, value));
                form_children(doc, resources, &decode_stream(doc, stream)?)
            })
    });
}

fn count_paint(
    doc: &Document,
    id: ObjectId,
    path: &mut HashSet<ObjectId>,
    cache: &mut HashMap<ObjectId, Option<Vec<ObjectId>>>,
    draws: &mut HashMap<ObjectId, usize>,
) {
    let count = draws.entry(id).or_default();
    *count = count.saturating_add(1);
    if !path.insert(id) {
        return;
    }
    cache_children(doc, id, cache);
    let count = cache.get(&id).and_then(Option::as_ref).map_or(0, Vec::len);
    for index in 0..count {
        if let Some(child) = cache
            .get(&id)
            .and_then(Option::as_ref)
            .and_then(|children| children.get(index))
            .copied()
        {
            count_paint(doc, child, path, cache, draws);
        }
    }
    path.remove(&id);
}

impl FormDrawTree {
    pub(super) fn load(bytes: &[u8]) -> Result<Self, String> {
        let doc = Document::load_mem(bytes).map_err(|error| error.to_string())?;
        let pages: Vec<_> = doc
            .get_pages()
            .into_values()
            .map(|page| {
                let content = page_content(&doc, page)?;
                form_children(&doc, page_resources(&doc, page), &content)
            })
            .collect();
        let mut children = HashMap::new();
        let mut draws = HashMap::new();
        for page in pages.iter().flatten() {
            for &id in page {
                count_paint(&doc, id, &mut HashSet::new(), &mut children, &mut draws);
            }
        }
        Ok(Self {
            pages,
            children,
            draws,
        })
    }

    pub(super) fn exclusive(&self, page: usize, ordinals: &[usize]) -> bool {
        let mut level = self.pages.get(page).and_then(Option::as_ref);
        for &ordinal in ordinals {
            let Some(id) = level.and_then(|forms| forms.get(ordinal)) else {
                return false;
            };
            if self.draws.get(id) != Some(&1) {
                return false;
            }
            let Some(Some(children)) = self.children.get(id) else {
                return false;
            };
            level = Some(children);
        }
        true
    }
}

fn integer(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<i64, String> {
    session
        .call(name, args)
        .map_err(|error| error.to_string())?
        .as_i64()
        .ok_or_else(|| "invalid PDFium integer result".into())
}

fn object_index(
    session: &mut PdfiumSession,
    page: u64,
    object: u64,
) -> Result<Option<i64>, String> {
    let count = integer(session, "FPDFPage_CountObjects", &[json!({"ref": page})])?;
    if count < 0 {
        return Err("could not count page objects".into());
    }
    for index in 0..count {
        let value = session
            .call("FPDFPage_GetObject", &[json!({"ref": page}), json!(index)])
            .map_err(|error| error.to_string())?;
        if value.get("ref").and_then(Value::as_u64) == Some(object) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn matrix(session: &mut PdfiumSession, object: u64) -> Result<[f64; 6], String> {
    let buffer = session.alloc(24).map_err(|error| error.to_string())?;
    let result = (|| {
        let ok = integer(
            session,
            "FPDFPageObj_GetMatrix",
            &[json!({"ref": object}), json!({"ref": buffer})],
        )?;
        if ok == 0 {
            return Ok([1., 0., 0., 1., 0., 0.]);
        }
        let bytes = session
            .read(buffer, 0, 24)
            .map_err(|error| error.to_string())?;
        if bytes.len() != 24 {
            return Err("invalid PDFium matrix".into());
        }
        let mut matrix = [0.; 6];
        for (out, bytes) in matrix.iter_mut().zip(bytes.chunks_exact(4)) {
            *out = f64::from(f32::from_le_bytes(
                bytes.try_into().map_err(|_| "invalid PDFium matrix")?,
            ));
        }
        Ok(matrix)
    })();
    let freed = session.free(buffer).map_err(|error| error.to_string());
    match result {
        Err(error) => Err(error),
        Ok(matrix) => {
            freed?;
            Ok(matrix)
        }
    }
}

/// Move each nested form and finally the target to page level in paint order.
/// Like the original consumer, this does not transform inherited clip paths.
/// The single and nested clipping behavior is recorded by `form_draw_tree/clip-oracle.ts`.
pub(super) fn lift(
    session: &mut PdfiumSession,
    page: u64,
    object: u64,
    chain: &[u64],
) -> Result<(), String> {
    for (child, parent) in chain
        .windows(2)
        .map(|pair| (pair[1], pair[0]))
        .chain(chain.last().copied().map(|parent| (object, parent)))
    {
        if object_index(session, page, child)?.is_some() {
            continue;
        }
        let parent_matrix = matrix(session, parent)?;
        let index = object_index(session, page, parent)?.map(|index| index + 1);
        let indexed = index.is_some()
            && session
                .supports("FPDFPage_InsertObjectAtIndex")
                .map_err(|error| error.to_string())?;
        if integer(
            session,
            "FPDFFormObj_RemoveObject",
            &[json!({"ref": parent}), json!({"ref": child})],
        )? == 0
        {
            return Err("could not detach the object from its template".into());
        }
        if let Some(index) = index.filter(|_| indexed) {
            if integer(
                session,
                "FPDFPage_InsertObjectAtIndex",
                &[json!({"ref": page}), json!({"ref": child}), json!(index)],
            )? == 0
            {
                session
                    .call(
                        "FPDFPage_InsertObject",
                        &[json!({"ref": page}), json!({"ref": child})],
                    )
                    .map_err(|error| error.to_string())?;
                return Err("FPDFPage_InsertObjectAtIndex failed".into());
            }
        } else {
            session
                .call(
                    "FPDFPage_InsertObject",
                    &[json!({"ref": page}), json!({"ref": child})],
                )
                .map_err(|error| error.to_string())?;
        }
        let mut args = vec![json!({"ref": child})];
        args.extend(parent_matrix.into_iter().map(wire_number));
        session
            .call("FPDFPageObj_Transform", &args)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Stream, dictionary};

    fn form(doc: &mut Document, content: &[u8], xobjects: Dictionary) -> ObjectId {
        doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! { "XObject" => xobjects },
            },
            content.to_vec(),
        ))
    }

    fn fixture() -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let shared = form(&mut doc, b"q Q\n", dictionary! {});
        let a = form(&mut doc, b"/Child Do\n", dictionary! { "Child" => shared });
        let b = form(&mut doc, b"/Child Do\n", dictionary! { "Child" => shared });
        let unique_child = form(&mut doc, b"q Q\n", dictionary! {});
        let unique = form(
            &mut doc,
            b"/Only Do\n",
            dictionary! { "Only" => unique_child },
        );
        let pages_id = doc.new_object_id();
        let mut pages = Vec::new();
        for (content, name, target) in [
            (b"/First Do\n".as_slice(), "First", a),
            (b"/Second Do\n".as_slice(), "Second", b),
            (b"(/Ghost Do) Tj /Unique Do\n".as_slice(), "Unique", unique),
        ] {
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
            let mut xobjects = Dictionary::new();
            xobjects.set(name, target);
            let page = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! { "XObject" => xobjects },
                "Contents" => content_id,
            });
            pages.push(Object::Reference(page));
        }
        doc.set_object(
            pages_id,
            dictionary! { "Type" => "Pages", "Count" => pages.len() as i64, "Kids" => pages },
        );
        let root = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", root);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn decodes_pdf_lib_stream_filters_for_census() {
        let doc = Document::with_version("1.7");
        let hex = Stream::new(
            dictionary! { "Filter" => "ASCIIHexDecode" },
            b"2f4c65616620446f0a>".to_vec(),
        );
        let run = Stream::new(
            dictionary! { "Filter" => "RunLengthDecode" },
            vec![
                8, b'/', b'L', b'e', b'a', b'f', b' ', b'D', b'o', b'\n', 128,
            ],
        );
        assert_eq!(decode_stream(&doc, &hex).unwrap(), b"/Leaf Do\n");
        assert_eq!(decode_stream(&doc, &run).unwrap(), b"/Leaf Do\n");
    }

    #[test]
    fn form_names_follow_pdf_lib_hex_and_latin1_rules() {
        assert_eq!(pdf_name("A#20B"), Some(b"A B".to_vec()));
        assert_eq!(pdf_name("A#2fB"), Some(b"A#2fB".to_vec()));
        assert_eq!(pdf_name("caf\u{e9}"), Some(b"caf\xe9".to_vec()));
    }

    #[test]
    fn original_scanner_strips_inline_images_but_matches_literal_text() {
        let mut doc = Document::with_version("1.7");
        let child = form(&mut doc, b"q Q\n", dictionary! {});
        let resources = dictionary! { "XObject" => dictionary! { "X" => child } };
        let found =
            form_children(&doc, Some(&resources), b"BI /X Do EI (/X Do) Tj /X Do\n").unwrap();
        assert_eq!(found, vec![child, child]);
    }

    #[test]
    fn counts_shared_nested_forms_across_pages_and_preserves_ordinals() {
        let tree = FormDrawTree::load(&fixture()).unwrap();
        assert!(tree.exclusive(0, &[0]));
        assert!(!tree.exclusive(0, &[0, 0]));
        assert!(!tree.exclusive(1, &[0, 0]));
        assert!(tree.exclusive(2, &[0, 0]));
        assert!(!tree.exclusive(2, &[1]));
        assert!(tree.exclusive(999, &[]));
    }

    #[test]
    fn counts_real_repeated_and_cyclic_form_references() {
        let mut doc = Document::with_version("1.7");
        let leaf = form(&mut doc, b"q Q\n", dictionary! {});
        let cycle = doc.new_object_id();
        doc.set_object(cycle, Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Loop" => cycle, "Leaf" => leaf } },
        }, b"/Leaf Do /Loop Do\n".to_vec()));
        let mut children = HashMap::new();
        let mut draws = HashMap::new();
        count_paint(&doc, cycle, &mut HashSet::new(), &mut children, &mut draws);
        assert_eq!(draws.get(&cycle), Some(&2));
        assert_eq!(draws.get(&leaf), Some(&1));
        count_paint(&doc, leaf, &mut HashSet::new(), &mut children, &mut draws);
        assert_eq!(draws.get(&leaf), Some(&2));
    }

    #[test]
    fn cyclic_and_unreadable_children_are_not_exclusive() {
        let cycle = (1, 0);
        let mut tree = FormDrawTree {
            pages: vec![Some(vec![cycle]), None],
            children: HashMap::from([(cycle, Some(vec![cycle]))]),
            draws: HashMap::from([(cycle, 2)]),
        };
        assert!(!tree.exclusive(0, &[0]));
        tree.draws.insert(cycle, 1);
        tree.children.insert(cycle, None);
        assert!(!tree.exclusive(0, &[0]));
        assert!(!tree.exclusive(1, &[0]));
    }
}
