//! Native incremental signature assembly over the pdf-lib-compatible context.
use super::{Context, Object, Parser, field, graph, previous_xref, revision, upng};
use crate::host::d2::pdf::pdf_codec::write_object;
use flate2::{Compression, write::ZlibEncoder};
use shell_contract::pdf_sign_options::SignOptions;
use std::io::Write;

type Dict = Vec<(Vec<u8>, Object)>;
type Reference = (f64, f64);
struct Change {
    reference: Reference,
    object: Object,
}

pub(in crate::host::d2::pdf::crypto) struct Assembled {
    pub(in crate::host::d2::pdf::crypto) file: Vec<u8>,
    pub(in crate::host::d2::pdf::crypto) range: [usize; 4],
}
fn reference(value: Reference) -> Object {
    Object::Reference(value.0, value.1)
}
fn name(value: &[u8]) -> Object {
    Object::Name(value.to_vec())
}
fn array(values: impl IntoIterator<Item = f64>) -> Object {
    Object::Array(values.into_iter().map(Object::Number).collect())
}
fn dict(values: impl IntoIterator<Item = (&'static [u8], Object)>) -> Object {
    Object::Dictionary(
        values
            .into_iter()
            .map(|(name, value)| (name.to_vec(), value))
            .collect(),
    )
}
fn set(dict: &mut Dict, key: &[u8], value: Object) {
    if let Some((_, old)) = dict.iter_mut().find(|(name, _)| name == key) {
        *old = value;
    } else {
        dict.push((key.to_vec(), value));
    }
}
fn edit_dict(
    context: &Context,
    value: Option<&Object>,
) -> Result<Option<(Option<Reference>, Dict)>, String> {
    Ok(match value {
        Some(reference @ Object::Reference(n, g)) => match context.lookup(reference) {
            Some(Object::Dictionary(dict)) => Some((Some((*n, *g)), dict.clone())),
            None | Some(Object::Null) => None,
            _ => return Err("invalid referenced PDF dictionary".into()),
        },
        Some(Object::Dictionary(dict)) => Some((None, dict.clone())),
        _ => None,
    })
}
fn edit_array(
    context: &Context,
    value: Option<&Object>,
) -> Result<(Option<Reference>, Vec<Object>), String> {
    Ok(match value {
        Some(reference @ Object::Reference(n, g)) => match context.lookup(reference) {
            Some(Object::Array(items)) => (Some((*n, *g)), items.clone()),
            None | Some(Object::Null) => (None, Vec::new()),
            _ => return Err("invalid referenced PDF array".into()),
        },
        Some(Object::Array(items)) => (None, items.clone()),
        _ => (None, Vec::new()),
    })
}
fn touch(changes: &mut Vec<Change>, reference: Reference, object: Object) {
    if let Some(old) = changes
        .iter_mut()
        .find(|old| old.reference.0 == reference.0)
    {
        *old = Change { reference, object };
    } else {
        changes.push(Change { reference, object });
    }
}
fn form(
    body: Vec<u8>,
    width: f64,
    height: f64,
    resources: Object,
    matrix: Option<[f64; 6]>,
) -> Object {
    let mut entries = vec![
        (b"Type".to_vec(), name(b"XObject")),
        (b"Subtype".to_vec(), name(b"Form")),
        (b"BBox".to_vec(), array([0.0, 0.0, width, height])),
        (b"Resources".to_vec(), resources),
    ];
    if let Some(matrix) = matrix {
        set(&mut entries, b"Matrix", array(matrix));
    }
    Object::Stream(entries, body)
}
fn xobjects(values: impl IntoIterator<Item = (&'static [u8], Object)>) -> Object {
    dict([(b"XObject".as_slice(), dict(values))])
}
fn deflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .map_err(|error| error.to_string())?;
    encoder.finish().map_err(|error| error.to_string())
}
fn image(
    width: usize,
    height: usize,
    channel: &[u8],
    gray: bool,
    mask: Option<Reference>,
) -> Result<Object, String> {
    let mut entries = vec![
        (b"Type".to_vec(), name(b"XObject")),
        (b"Subtype".to_vec(), name(b"Image")),
    ];
    if gray {
        entries.push((b"Height".to_vec(), Object::Number(height as f64)));
        entries.push((b"Width".to_vec(), Object::Number(width as f64)));
    }
    entries.push((b"BitsPerComponent".to_vec(), Object::Number(8.0)));
    if !gray {
        entries.push((b"Width".to_vec(), Object::Number(width as f64)));
        entries.push((b"Height".to_vec(), Object::Number(height as f64)));
    }
    entries.push((
        b"ColorSpace".to_vec(),
        name(if gray { b"DeviceGray" } else { b"DeviceRGB" }),
    ));
    if gray {
        entries.push((b"Decode".to_vec(), array([0.0, 1.0])));
    }
    if let Some(mask) = mask {
        entries.push((b"SMask".to_vec(), reference(mask)));
    }
    entries.push((b"Filter".to_vec(), name(b"FlateDecode")));
    Ok(Object::Stream(entries, deflate(channel)?))
}
fn put_object(
    output: &mut Vec<u8>,
    offsets: &mut Vec<(f64, f64, usize)>,
    id: Reference,
    object: &Object,
) -> Result<(), String> {
    offsets.push((id.0, id.1, output.len()));
    output.extend_from_slice(
        format!(
            "{} {} obj\n",
            jscompat::number_to_string(id.0),
            jscompat::number_to_string(id.1)
        )
        .as_bytes(),
    );
    write_object(output, object)?;
    output.extend_from_slice(b"\nendobj\n");
    Ok(())
}
fn signature(
    number: f64,
    options: &SignOptions,
    time: &str,
    hex_length: usize,
) -> (Vec<u8>, usize, usize) {
    fn text(value: &str) -> String {
        if value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            let mut out = String::from("(");
            for ch in value.chars() {
                if matches!(ch, '\\' | '(' | ')') {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push(')');
            out
        } else {
            let mut out = String::from("<FEFF");
            for unit in value.encode_utf16() {
                out.push_str(&format!("{unit:04X}"));
            }
            out.push('>');
            out
        }
    }
    let mut out = format!(
        "{} 0 obj\n<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached",
        jscompat::number_to_string(number)
    );
    if let Some(level) = options.certify_level {
        out.push_str(&format!(" /Reference [<< /Type /SigRef /TransformMethod /DocMDP /DigestMethod /SHA256 /TransformParams << /Type /TransformParams /P {level} /V /1.2 >> >>]"));
    }
    for (key, value) in [
        ("Reason", &options.reason),
        ("Location", &options.location),
        ("ContactInfo", &options.contact_info),
    ] {
        if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
            out.push_str(&format!(" /{key} {}", text(value)));
        }
    }
    out.push_str(&format!(" /M ({time}) /ByteRange "));
    let range = out.len();
    out.push_str("[0 0000000000 0000000000 0000000000] /Contents ");
    let contents = out.len();
    out.push('<');
    out.extend(std::iter::repeat_n('0', hex_length));
    out.push_str("> >>\nendobj\n");
    (out.into_bytes(), range, contents)
}

/// Builds the exact incremental revision from request-owned input and options.
/// No service-provided revision or digest enters this path.
pub(in crate::host::d2::pdf::crypto) fn assemble(
    original: &[u8],
    options: &SignOptions,
    time: &str,
    placeholder: usize,
) -> Result<Assembled, String> {
    if original.len() > 400 * 1024 * 1024 {
        return Err("PDF exceeds signing size limit".into());
    }
    let mut context = Parser::new(original).document()?;
    if context
        .encrypt
        .as_ref()
        .and_then(|value| context.lookup(value))
        .is_some()
    {
        return Err("PDF is encrypted".into());
    }
    let previous = previous_xref(original)?;
    let mut next = context.largest_object_number.max(previous.size - 1.0);
    let first_new = next + 1.0;
    let mut alloc = || {
        next += 1.0;
        (next, 0.0)
    };
    let signature_ref = alloc();
    let widget_ref = alloc();
    let (signed, field_name) = graph::new_field_name(&context)?;
    if options.certify_level.is_some() && signed > 0 {
        return Err("cannot certify an already signed PDF".into());
    }
    let index = options
        .visible
        .as_ref()
        .map_or(0.0, |visible| visible.page_index);
    let pages = graph::pages(&context)?;
    let page = if index.is_finite()
        && index >= 0.0
        && index.fract() == 0.0
        && index <= usize::MAX as f64
    {
        pages.get(index as usize).copied()
    } else {
        None
    }
    .ok_or("missing signature page")?;
    let root = context.root.clone().ok_or("missing catalog reference")?;
    let Object::Reference(root_number, root_generation) = &root else {
        return Err("invalid catalog reference".into());
    };
    let (_, mut catalog) = edit_dict(&context, Some(&root))?.ok_or("missing catalog")?;
    let mut rectangle = [0.0; 4];
    let mut appearance = None;
    if let Some(visible) = &options.visible {
        let placed = graph::placement(&context, page, visible)?;
        rectangle = placed.rect;
        let (width, height, pixels) = upng::rgba_png(&visible.png)?;
        let image_ref = alloc();
        let mut rgb = Vec::new();
        let mut alpha = Vec::new();
        rgb.try_reserve_exact(pixels.len() / 4 * 3)
            .map_err(|_| "PNG allocation failed")?;
        alpha
            .try_reserve_exact(pixels.len() / 4)
            .map_err(|_| "PNG allocation failed")?;
        for pixel in pixels.chunks_exact(4) {
            rgb.extend_from_slice(&pixel[..3]);
            alpha.push(pixel[3]);
        }
        let mask = if alpha.iter().any(|&a| a < 255) {
            let id = alloc();
            context.assign(id.0, id.1, image(width, height, &alpha, true, None)?);
            Some(id)
        } else {
            None
        };
        context.assign(
            image_ref.0,
            image_ref.1,
            image(width, height, &rgb, false, mask)?,
        );
        let paint = alloc();
        let blank = alloc();
        let layers = alloc();
        let outer = alloc();
        let paint_body = format!(
            "q {} 0 0 {} 0 0 cm /Sig0 Do Q",
            jscompat::number_to_string(placed.width),
            jscompat::number_to_string(placed.height)
        )
        .into_bytes();
        context.assign(
            paint.0,
            paint.1,
            form(
                paint_body,
                placed.width,
                placed.height,
                xobjects([(b"Sig0".as_slice(), reference(image_ref))]),
                None,
            ),
        );
        context.assign(
            blank.0,
            blank.1,
            form(b"% DSBlank".to_vec(), 100.0, 100.0, dict([]), None),
        );
        context.assign(
            layers.0,
            layers.1,
            form(
                b"q 1 0 0 1 0 0 cm /n0 Do Q q 1 0 0 1 0 0 cm /n2 Do Q".to_vec(),
                placed.width,
                placed.height,
                xobjects([
                    (b"n0".as_slice(), reference(blank)),
                    (b"n2".as_slice(), reference(paint)),
                ]),
                None,
            ),
        );
        let matrix = if placed.rotation == 0.0 {
            None
        } else {
            let radians = placed.rotation * std::f64::consts::PI / 180.0;
            let cos = (radians.cos() + 0.5).floor();
            let sin = (radians.sin() + 0.5).floor();
            Some([cos, sin, -sin, cos, 0.0, 0.0])
        };
        context.assign(
            outer.0,
            outer.1,
            form(
                b"/FRM Do".to_vec(),
                placed.width,
                placed.height,
                xobjects([(b"FRM".as_slice(), reference(layers))]),
                matrix,
            ),
        );
        appearance = Some(outer);
    }
    let mut modified = Vec::new();
    let (_, mut page_dict) =
        edit_dict(&context, Some(&reference(page)))?.ok_or("missing editable page")?;
    let (annots_ref, mut annots) = edit_array(&context, field(&page_dict, b"Annots"))?;
    annots.push(reference(widget_ref));
    if let Some(id) = annots_ref {
        touch(&mut modified, id, Object::Array(annots));
    } else {
        set(&mut page_dict, b"Annots", Object::Array(annots));
    }
    touch(&mut modified, page, Object::Dictionary(page_dict));
    let (acro_ref, mut acro) =
        edit_dict(&context, field(&catalog, b"AcroForm"))?.unwrap_or((None, Vec::new()));
    let (fields_ref, mut fields) = edit_array(&context, field(&acro, b"Fields"))?;
    fields.push(reference(widget_ref));
    if let Some(id) = fields_ref {
        touch(&mut modified, id, Object::Array(fields));
    } else {
        set(&mut acro, b"Fields", Object::Array(fields));
    }
    set(&mut acro, b"SigFlags", Object::Number(3.0));
    if let Some(id) = acro_ref {
        touch(&mut modified, id, Object::Dictionary(acro));
    } else {
        set(&mut catalog, b"AcroForm", Object::Dictionary(acro));
    }
    if options.certify_level.is_some() {
        set(
            &mut catalog,
            b"Perms",
            dict([(b"DocMDP".as_slice(), reference(signature_ref))]),
        );
    }
    if acro_ref.is_none() || options.certify_level.is_some() {
        touch(
            &mut modified,
            (*root_number, *root_generation),
            Object::Dictionary(catalog),
        );
    }
    let mut widget = vec![
        (b"Type".to_vec(), name(b"Annot")),
        (b"Subtype".to_vec(), name(b"Widget")),
        (b"FT".to_vec(), name(b"Sig")),
        (b"T".to_vec(), Object::String(field_name.into_bytes())),
        (b"V".to_vec(), reference(signature_ref)),
        (b"F".to_vec(), Object::Number(132.0)),
        (b"Rect".to_vec(), array(rectangle)),
        (b"P".to_vec(), reference(page)),
    ];
    if let Some(id) = appearance {
        set(&mut widget, b"AP", dict([(b"N".as_slice(), reference(id))]));
    }
    let mut output = original.to_vec();
    if original.last() != Some(&b'\n') {
        output.push(b'\n');
    }
    let mut offsets = Vec::new();
    for change in modified
        .into_iter()
        .filter(|change| change.reference.0 != widget_ref.0)
    {
        put_object(&mut output, &mut offsets, change.reference, &change.object)?;
    }
    let mut created: Vec<_> = context
        .order
        .iter()
        .filter_map(|key| {
            let id = (f64::from_bits(key.0), f64::from_bits(key.1));
            (id.0 >= first_new && id != signature_ref && id != widget_ref)
                .then(|| context.objects.get(key).map(|object| (id, object)))
                .flatten()
        })
        .collect();
    created.sort_by(|a, b| a.0.0.total_cmp(&b.0.0));
    for (id, object) in created {
        put_object(&mut output, &mut offsets, id, object)?;
    }
    put_object(
        &mut output,
        &mut offsets,
        widget_ref,
        &Object::Dictionary(widget),
    )?;
    offsets.push((signature_ref.0, signature_ref.1, output.len()));
    let (signature, marker, contents) = signature(
        signature_ref.0,
        options,
        time,
        placeholder
            .checked_mul(2)
            .ok_or("signature placeholder overflow")?,
    );
    let range_at = output.len() + marker;
    let start = output.len() + contents;
    let end = start + placeholder * 2 + 2;
    output.extend_from_slice(&signature);
    let size = next.max(offsets.iter().map(|entry| entry.0).fold(0.0_f64, f64::max)) + 1.0;
    let trailer = revision::trailer_fields(&context, previous.offset)?;
    let xref = output.len();
    offsets.sort_by(|a, b| a.0.total_cmp(&b.0));
    if previous.is_stream {
        let xref_number = size;
        let mut entries = offsets.clone();
        entries.push((xref_number, 0.0, xref));
        entries.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut index = Vec::<(f64, usize)>::new();
        let mut data = Vec::with_capacity(entries.len() * 7);
        for (number, generation, offset) in entries {
            if let Some(last) = index.last_mut() {
                if last.0 + last.1 as f64 == number {
                    last.1 += 1;
                } else {
                    index.push((number, 1));
                }
            } else {
                index.push((number, 1));
            }
            let off = u32::try_from(offset).map_err(|_| "xref offset out of range")?;
            if !generation.is_finite()
                || generation < 0.0
                || generation > u16::MAX as f64
                || generation.fract() != 0.0
            {
                return Err("xref generation out of range".into());
            }
            data.push(1);
            data.extend_from_slice(&off.to_be_bytes());
            data.extend_from_slice(&(generation as u16).to_be_bytes());
        }
        let body = deflate(&data)?;
        let index = index
            .into_iter()
            .map(|(start, count)| format!("{} {count}", jscompat::number_to_string(start)))
            .collect::<Vec<_>>()
            .join(" ");
        output.extend_from_slice(format!("{} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Index [{}] /Filter /FlateDecode{} /Length {} >>\nstream\n", jscompat::number_to_string(xref_number), jscompat::number_to_string(xref_number+1.0), index, trailer, body.len()).as_bytes());
        output.extend_from_slice(&body);
        output.extend_from_slice(b"\nendstream\nendobj\n");
    } else {
        output.extend_from_slice(b"xref\n");
        let mut at = 0;
        while at < offsets.len() {
            let mut end = at + 1;
            while end < offsets.len() && offsets[end - 1].0 + 1.0 == offsets[end].0 {
                end += 1;
            }
            output.extend_from_slice(
                format!(
                    "{} {}\n",
                    jscompat::number_to_string(offsets[at].0),
                    end - at
                )
                .as_bytes(),
            );
            for (_, generation, offset) in &offsets[at..end] {
                output.extend_from_slice(
                    format!(
                        "{:010} {:0>5} n \n",
                        offset,
                        jscompat::number_to_string(*generation)
                    )
                    .as_bytes(),
                );
            }
            at = end;
        }
        output.extend_from_slice(
            format!(
                "trailer\n<< /Size {}{} >>\n",
                jscompat::number_to_string(size),
                trailer
            )
            .as_bytes(),
        );
    }
    output.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
    let range = [0, start, end, output.len() - end];
    let range_text = format!("[0 {:010} {:010} {:010}]", range[1], range[2], range[3]);
    let target = output
        .get_mut(range_at..range_at + range_text.len())
        .ok_or("invalid byte range")?;
    target.copy_from_slice(range_text.as_bytes());
    Ok(Assembled {
        file: output,
        range,
    })
}

#[cfg(test)]
pub(in crate::host::d2::pdf::crypto) mod tests {
    use super::super::RefKey;
    use super::*;
    pub(in crate::host::d2::pdf::crypto) fn classic() -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![0];
        for body in [
            b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >>".as_slice(),
            b"<< /Type /Page /Parent 2 0 R >>".as_slice(),
            b"malformed-object".as_slice(),
        ] {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", offsets.len() - 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
        for offset in offsets.into_iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        pdf
    }

    #[test]
    fn creates_bounded_incremental_revision_without_rewriting_invalid_objects() {
        let original = classic();
        let options = SignOptions {
            reason: Some("approval (한글)".into()),
            ..Default::default()
        };
        let result = assemble(&original, &options, "D:20250101000000Z", 16 * 1024).unwrap();
        assert!(result.file.starts_with(&original));
        assert_eq!(result.range[2] - result.range[1], 32_770);
        assert_eq!(result.range[2] + result.range[3], result.file.len());
        assert!(matches!(
            Parser::new(&result.file)
                .document()
                .unwrap()
                .objects
                .get(&RefKey::new(4.0, 0.0)),
            Some(Object::Invalid(_))
        ));
    }

    #[test]
    fn metadata_cannot_redirect_reserved_byte_range_or_contents() {
        let original = classic();
        let options = SignOptions {
            reason: Some("example /ByteRange [0 1 2 3] /Contents <fake>".into()),
            ..Default::default()
        };
        let assembled = assemble(&original, &options, "D:20250101000000Z", 16 * 1024).unwrap();
        let [_, start, end, tail] = assembled.range;
        assert_eq!(assembled.file[start], b'<');
        assert_eq!(assembled.file[end - 1], b'>');
        assert!(
            assembled.file[start + 1..end - 1]
                .iter()
                .all(|byte| *byte == b'0')
        );
        assert_eq!(end + tail, assembled.file.len());
    }

    #[test]
    fn retains_javascript_number_allocation_for_large_object_ids() {
        let mut original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n9007199254740992 1 obj << /Test 1 >> endobj\n".to_vec();
        let offset = original.len();
        original.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \ntrailer << /Size 9007199254740993 /Root 1 0 R >>\nstartxref\n{offset}\n%%EOF\n").as_bytes());
        let assembled = assemble(
            &original,
            &SignOptions::default(),
            "D:20250101000000Z",
            16 * 1024,
        )
        .unwrap();
        let appended = &assembled.file[original.len()..];
        let created = b"9007199254740992 1 obj\n";
        let signed = b"9007199254740992 0 obj\n";
        assert!(appended.windows(created.len()).any(|part| part == created));
        assert_eq!(
            appended
                .windows(signed.len())
                .filter(|part| *part == signed)
                .count(),
            2
        );
    }

    #[test]
    fn preserves_xref_stream_updates() {
        let mut original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n".to_vec();
        let offset = original.len();
        let body = deflate(&[0; 42]).unwrap();
        original.extend_from_slice(format!("5 0 obj\n<< /Type /XRef /Size 6 /W [1 4 2] /Index [0 6] /Root 1 0 R /Filter /FlateDecode /Length {} >>\nstream\n", body.len()).as_bytes());
        original.extend_from_slice(&body);
        original.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{offset}\n%%EOF\n").as_bytes(),
        );
        let options = SignOptions::default();
        let result = assemble(&original, &options, "D:20250101000000Z", 16 * 1024).unwrap();
        assert!(previous_xref(&result.file).unwrap().is_stream);
    }

    #[test]
    fn edits_page_expanded_from_an_object_stream() {
        let mut original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n".to_vec();
        let body = b"3 0 << /Type /Page /Parent 2 0 R >>";
        original.extend_from_slice(
            format!(
                "4 0 obj << /Type /ObjStm /N 1 /First 4 /Length {} >> stream\n",
                body.len()
            )
            .as_bytes(),
        );
        original.extend_from_slice(body);
        original.extend_from_slice(b"\nendstream endobj\n");
        let offset = original.len();
        original.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \ntrailer << /Size 5 /Root 1 0 R >>\nstartxref\n{offset}\n%%EOF\n").as_bytes());
        let options = SignOptions::default();
        let result = assemble(&original, &options, "D:20250101000000Z", 16 * 1024).unwrap();
        let revised = Parser::new(&result.file).document().unwrap();
        assert!(matches!(
            revised.objects.get(&RefKey::new(3.0, 0.0)),
            Some(Object::Dictionary(dict)) if field(dict, b"Annots").is_some()
        ));
    }

    #[test]
    fn embeds_request_owned_visible_image_and_alpha() {
        fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
            png.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(bytes);
            png.extend_from_slice(&[0; 4]);
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        chunk(
            &mut png,
            b"IDAT",
            &deflate(&[0, 255, 0, 0, 255, 0, 0, 255, 127]).unwrap(),
        );
        chunk(&mut png, b"IEND", &[]);
        let original = classic();
        let options = SignOptions {
            visible: Some(shell_contract::pdf_sign_options::VisibleSignature {
                page_index: 0.0,
                position: "bottom-right".into(),
                width: 80.0,
                height: 40.0,
                png,
            }),
            ..Default::default()
        };
        let result = assemble(&original, &options, "D:20250101000000Z", 16 * 1024).unwrap();
        let revised = Parser::new(&result.file).document().unwrap();
        assert!(revised.objects.values().any(|object| matches!(
            object,
            Object::Stream(dict, _)
                if field(dict, b"Subtype") == Some(&Object::Name(b"Image".to_vec()))
        )));
    }
}
