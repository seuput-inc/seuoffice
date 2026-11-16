use std::collections::HashSet;

use super::{Context, Object, Parser};
use shell_contract::pdf_sign_options::SignOptions;

#[derive(Debug, PartialEq)]
pub(super) struct PhysicalObject {
    pub(super) number: u64,
    pub(super) generation: u64,
    pub(super) offset: usize,
}

fn pdf_text(value: &str) -> String {
    if value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        let mut text = String::from("(");
        for ch in value.chars() {
            if matches!(ch, '\\' | '(' | ')') {
                text.push('\\');
            }
            text.push(ch);
        }
        text.push(')');
        text
    } else {
        let mut text = String::from("<FEFF");
        for unit in value.encode_utf16() {
            text.push_str(&format!("{unit:04X}"));
        }
        text.push('>');
        text
    }
}

pub(super) fn signature_bytes(
    number: u64,
    options: &SignOptions,
    signing_time: &str,
    range: [usize; 4],
) -> Result<Vec<u8>, String> {
    let [start, first, second, tail] = range;
    let placeholder = second
        .checked_sub(first)
        .and_then(|n| n.checked_sub(2))
        .ok_or("invalid signature placeholder")?;
    let mut text = format!(
        "{number} 0 obj\n<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached"
    );
    if let Some(level) = options.certify_level {
        text.push_str(&format!(" /Reference [<< /Type /SigRef /TransformMethod /DocMDP /DigestMethod /SHA256 /TransformParams << /Type /TransformParams /P {level} /V /1.2 >> >>]"));
    }
    for (key, value) in [
        ("Reason", options.reason.as_deref()),
        ("Location", options.location.as_deref()),
        ("ContactInfo", options.contact_info.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            text.push_str(&format!(" /{key} {}", pdf_text(value)));
        }
    }
    text.push_str(&format!(
        " /M ({signing_time}) /ByteRange [{start} {first:010} {second:010} {tail:010}] /Contents <"
    ));
    text.extend(std::iter::repeat_n('0', placeholder));
    text.push_str("> >>\nendobj\n");
    Ok(text.into_bytes())
}

/// Scan only the appended objects, in physical order, and bind the excluded
/// byte range to the exact hand-serialized signature object. The xref section
/// and graph semantics require separate validation before signing.
pub(super) fn scan(
    original: &[u8],
    revision: &[u8],
    xref_offset: usize,
    signature: u64,
    options: &SignOptions,
    signing_time: &str,
    range: [usize; 4],
) -> Result<Vec<PhysicalObject>, String> {
    if !revision.starts_with(original) || xref_offset > revision.len() {
        return Err("invalid incremental revision".into());
    }
    let mut offset = original.len();
    if original.last() != Some(&b'\n') {
        if revision.get(offset) != Some(&b'\n') {
            return Err("missing revision separator".into());
        }
        offset += 1;
    }
    let expected_signature = signature_bytes(signature, options, signing_time, range)?;
    let mut objects = Vec::new();
    let mut numbers = HashSet::new();
    while offset < xref_offset {
        let start = offset;
        let mut parser = Parser::new(revision);
        parser.offset = start;
        let (number, generation) = parser.indirect_header()?;
        if !number.is_finite()
            || !generation.is_finite()
            || number < 0.0
            || generation < 0.0
            || number.fract() != 0.0
            || generation.fract() != 0.0
            || number > 9_007_199_254_740_991.0
            || generation > 9_007_199_254_740_991.0
        {
            return Err("invalid revision object reference".into());
        }
        let number = number as u64;
        let generation = generation as u64;
        let header = format!("{number} {generation} obj\n");
        if !revision[start..].starts_with(header.as_bytes()) || !numbers.insert(number) {
            return Err("invalid or duplicate revision object".into());
        }
        if number == signature {
            let end = start
                .checked_add(expected_signature.len())
                .ok_or("signature offset overflow")?;
            if generation != 0
                || revision.get(start..end) != Some(expected_signature.as_slice())
                || end != xref_offset
            {
                return Err("physical signature object differs from native operation".into());
            }
            let marker = b" /Contents <";
            let contents = expected_signature
                .windows(marker.len())
                .rposition(|part| part == marker)
                .ok_or("missing signature contents")?;
            if range[1] != start + contents + marker.len() - 1 || range[2] > end {
                return Err("signature range does not enclose signature contents".into());
            }
            offset = end;
        } else {
            parser.offset = start + header.len();
            parser.object()?;
            offset = parser.offset;
            if revision.get(offset..offset + 8) == Some(b"\nendobj\n") {
                offset += 8;
            } else if offset > start
                && revision.get(offset - 1) == Some(&b'\n')
                && revision.get(offset..offset + 7) == Some(b"endobj\n")
            {
                offset += 7;
            } else {
                return Err("invalid physical object boundary".into());
            }
        }
        if offset > xref_offset {
            return Err("revision object overlaps xref".into());
        }
        objects.push(PhysicalObject {
            number,
            generation,
            offset: start,
        });
    }
    if offset != xref_offset || objects.last().map(|object| object.number) != Some(signature) {
        return Err("missing final signature object".into());
    }
    Ok(objects)
}

fn string_bytes(object: &Object) -> Option<Vec<u8>> {
    match object {
        Object::HexString(raw) => Some(crate::host::d2::pdf::pdf_codec::hex_bytes(raw)),
        Object::String(raw) => Some(crate::host::d2::pdf::pdf_codec::literal_bytes(raw)),
        _ => None,
    }
}

pub(super) fn trailer_fields(original: &Context, previous_offset: usize) -> Result<String, String> {
    let Some(Object::Reference(root, generation)) = &original.root else {
        return Err("missing catalog reference".into());
    };
    let mut fields = format!(
        " /Root {} {} R",
        jscompat::number_to_string(*root),
        jscompat::number_to_string(*generation)
    );
    if let Some(Object::Reference(info, generation)) = &original.info {
        fields.push_str(&format!(
            " /Info {} {} R",
            jscompat::number_to_string(*info),
            jscompat::number_to_string(*generation)
        ));
    }
    if let Some(Object::Array(ids)) = &original.id {
        let converted: Option<Vec<Vec<u8>>> = ids.iter().map(string_bytes).collect();
        if let Some(converted) = converted.filter(|ids| !ids.is_empty()) {
            fields.push_str(" /ID [ ");
            for (index, id) in converted.iter().enumerate() {
                if index != 0 {
                    fields.push(' ');
                }
                fields.push('<');
                for byte in id {
                    fields.push_str(&format!("{byte:02X}"));
                }
                fields.push('>');
            }
            fields.push_str(" ]");
        }
    }
    fields.push_str(&format!(" /Prev {previous_offset}"));
    Ok(fields)
}

/// Compare the emitted classic cross-reference table, trailer and EOF against
/// native object offsets and the original trailer, not service-provided data.
pub(super) fn validate_xref_table(
    revision: &[u8],
    xref_offset: usize,
    original: &Context,
    previous_offset: usize,
    size: u64,
    objects: &[PhysicalObject],
) -> Result<(), String> {
    let mut sorted: Vec<&PhysicalObject> = objects.iter().collect();
    sorted.sort_by_key(|object| object.number);
    let mut expected = String::from("xref\n");
    let mut index = 0;
    while index < sorted.len() {
        let start = index;
        index += 1;
        while index < sorted.len()
            && sorted[index - 1].number.checked_add(1) == Some(sorted[index].number)
        {
            index += 1;
        }
        expected.push_str(&format!("{} {}\n", sorted[start].number, index - start));
        for object in &sorted[start..index] {
            expected.push_str(&format!(
                "{:010} {:05} n \n",
                object.offset, object.generation
            ));
        }
    }
    expected.push_str(&format!(
        "trailer\n<< /Size {size}{} >>\nstartxref\n{xref_offset}\n%%EOF\n",
        trailer_fields(original, previous_offset)?
    ));
    if revision.get(xref_offset..) != Some(expected.as_bytes()) {
        return Err("xref table differs from native revision".into());
    }
    Ok(())
}

/// Verify the xref stream's dictionary and every decoded row, including its
/// self-entry. Compression bytes are not trusted as object offsets.
pub(super) fn validate_xref_stream(
    revision: &[u8],
    xref_offset: usize,
    original: &Context,
    previous_offset: usize,
    size: u64,
    objects: &[PhysicalObject],
) -> Result<(), String> {
    use std::io::Read;
    let mut entries: Vec<(u64, usize, u64)> = objects
        .iter()
        .map(|object| (object.number, object.offset, object.generation))
        .collect();
    entries.push((size, xref_offset, 0));
    entries.sort_by_key(|entry| entry.0);
    let mut index = Vec::<u64>::new();
    let mut previous = None;
    let mut expected_rows = Vec::with_capacity(entries.len() * 7);
    for (number, offset, generation) in entries {
        if previous.and_then(|value: u64| value.checked_add(1)) == Some(number) {
            *index.last_mut().ok_or("invalid xref index")? += 1;
        } else {
            index.extend([number, 1]);
        }
        previous = Some(number);
        let offset = u32::try_from(offset).map_err(|_| "xref offset exceeds stream width")?;
        let generation =
            u16::try_from(generation).map_err(|_| "xref generation exceeds stream width")?;
        expected_rows.push(1);
        expected_rows.extend_from_slice(&offset.to_be_bytes());
        expected_rows.extend_from_slice(&generation.to_be_bytes());
    }
    let indices = index
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let prefix = format!(
        "{size} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Index [{indices}] /Filter /FlateDecode{} /Length ",
        size + 1,
        trailer_fields(original, previous_offset)?
    );
    let suffix = format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n");
    let tail = revision.get(xref_offset..).ok_or("invalid xref offset")?;
    if !tail.starts_with(prefix.as_bytes()) {
        return Err("xref stream dictionary differs from native revision".into());
    }
    let digits_start = prefix.len();
    let digits_end = tail[digits_start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map(|at| at + digits_start)
        .ok_or("missing xref stream length")?;
    let digits = std::str::from_utf8(&tail[digits_start..digits_end])
        .map_err(|_| "invalid xref stream length")?;
    let length = digits
        .parse::<usize>()
        .map_err(|_| "invalid xref stream length")?;
    if digits != length.to_string() || !tail[digits_end..].starts_with(b" >>\nstream\n") {
        return Err("invalid xref stream framing".into());
    }
    let body_start = digits_end + b" >>\nstream\n".len();
    let body_end = body_start
        .checked_add(length)
        .ok_or("xref stream length overflow")?;
    let body = tail
        .get(body_start..body_end)
        .ok_or("truncated xref stream")?;
    if tail.get(body_end..) != Some(suffix.as_bytes()) {
        return Err("invalid xref stream suffix".into());
    }
    let mut decoder = flate2::read::ZlibDecoder::new(body);
    let mut rows = Vec::new();
    decoder
        .by_ref()
        .take((expected_rows.len() + 1) as u64)
        .read_to_end(&mut rows)
        .map_err(|error| format!("invalid xref stream compression: {error}"))?;
    if rows != expected_rows {
        return Err("xref stream offsets differ from native revision".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binds_gap_to_unique_final_signature_object() {
        let original = b"%PDF-1.7\n";
        let options = SignOptions::default();
        let prefix = b"4 0 obj\n<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /M (D:20250101000000Z) /ByteRange [0 ";
        let first = original.len() + prefix.len() + 10 + 1 + 10 + 1 + 10 + b"] /Contents ".len();
        let range = [0, first, first + 4, 0];
        let mut bytes = original.to_vec();
        bytes.extend(signature_bytes(4, &options, "D:20250101000000Z", range).unwrap());
        assert_eq!(
            scan(
                original,
                &bytes,
                bytes.len(),
                4,
                &options,
                "D:20250101000000Z",
                range
            )
            .unwrap()[0]
                .offset,
            original.len()
        );
        let mut tampered = bytes.clone();
        tampered[first + 1] = b'F';
        assert!(
            scan(
                original,
                &tampered,
                tampered.len(),
                4,
                &options,
                "D:20250101000000Z",
                range
            )
            .is_err()
        );
    }

    #[test]
    fn xref_offsets_and_trailer_must_match_native_context() {
        let context = Parser::new(b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R /ID [(a\\050b) <00ff>] >>\nstartxref\n1\n%%EOF\n").document().unwrap();
        let objects = vec![
            PhysicalObject {
                number: 4,
                generation: 0,
                offset: 12,
            },
            PhysicalObject {
                number: 5,
                generation: 0,
                offset: 57,
            },
        ];
        let mut bytes = vec![b'x'; 100];
        bytes.extend_from_slice(b"xref\n4 2\n0000000012 00000 n \n0000000057 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /ID [ <612862> <00FF> ] /Prev 9 >>\nstartxref\n100\n%%EOF\n");
        assert!(validate_xref_table(&bytes, 100, &context, 9, 6, &objects).is_ok());
        bytes[110] = b'9';
        assert!(validate_xref_table(&bytes, 100, &context, 9, 6, &objects).is_err());
    }

    #[test]
    fn xref_stream_rows_are_bound_to_actual_offsets() {
        use std::io::Write;
        let context = Parser::new(b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n1\n%%EOF\n").document().unwrap();
        let objects = vec![
            PhysicalObject {
                number: 4,
                generation: 0,
                offset: 12,
            },
            PhysicalObject {
                number: 5,
                generation: 0,
                offset: 57,
            },
        ];
        let rows = [
            1, 0, 0, 0, 12, 0, 0, 1, 0, 0, 0, 57, 0, 0, 1, 0, 0, 0, 100, 0, 0,
        ];
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&rows).unwrap();
        let body = encoder.finish().unwrap();
        let mut bytes = vec![b'x'; 100];
        bytes.extend_from_slice(format!("6 0 obj\n<< /Type /XRef /Size 7 /W [1 4 2] /Index [4 3] /Filter /FlateDecode /Root 1 0 R /Prev 9 /Length {} >>\nstream\n", body.len()).as_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(b"\nendstream\nendobj\nstartxref\n100\n%%EOF\n");
        assert!(validate_xref_stream(&bytes, 100, &context, 9, 6, &objects).is_ok());
        assert!(validate_xref_stream(&bytes, 100, &context, 8, 6, &objects).is_err());
    }

    #[test]
    fn complete_classic_revision_rejects_foreign_page_changes() {
        let mut original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n".to_vec();
        let previous = original.len();
        original.extend_from_slice(
            format!("xref\ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{previous}\n%%EOF\n")
                .as_bytes(),
        );
        let options = SignOptions::default();
        let mut revision = original.clone();
        let page = revision.len();
        revision.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /Annots [5 0 R] >>\nendobj\n",
        );
        let catalog = revision.len();
        revision.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R] /SigFlags 3 >> >>\nendobj\n");
        let widget = revision.len();
        revision.extend_from_slice(b"5 0 obj\n<< /Type /Annot /Subtype /Widget /FT /Sig /T (Signature1) /V 4 0 R /F 132 /Rect [0 0 0 0] /P 3 0 R >>\nendobj\n");
        let signature = revision.len();
        let sample = signature_bytes(4, &options, "D:20250101000000Z", [0, 0, 4, 0]).unwrap();
        let marker = b" /Contents <";
        let first = signature
            + sample
                .windows(marker.len())
                .rposition(|part| part == marker)
                .unwrap()
            + marker.len()
            - 1;
        let xref = signature + sample.len();
        let trailer = format!(
            "xref\n1 1\n{catalog:010} 00000 n \n3 3\n{page:010} 00000 n \n{signature:010} 00000 n \n{widget:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /Prev {previous} >>\nstartxref\n{xref}\n%%EOF\n"
        );
        let total = xref + trailer.len();
        let range = [0, first, first + 4, total - first - 4];
        revision.extend(signature_bytes(4, &options, "D:20250101000000Z", range).unwrap());
        revision.extend_from_slice(trailer.as_bytes());
        assert!(
            super::super::validate_revision(
                &original,
                &revision,
                &options,
                "D:20250101000000Z",
                range
            )
            .is_ok()
        );
        let mut forged = revision.clone();
        let body = b"/Parent 2 0 R /Annots";
        let at = forged
            .windows(body.len())
            .rposition(|slice| slice == body)
            .unwrap();
        forged[at + b"/Parent ".len()] = b'9';
        assert!(
            super::super::validate_revision(
                &original,
                &forged,
                &options,
                "D:20250101000000Z",
                range
            )
            .is_err()
        );
    }
}
