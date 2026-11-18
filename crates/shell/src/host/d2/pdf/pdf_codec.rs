//! Internal pdf-lib-compatible object graph operations. No signing capability is exposed.
mod predecrypt;
mod transform;

pub(super) use transform::{decrypt, reprotect, PayloadKind, literal_bytes, hex_bytes};
use super::crypto::pdf_lib::{Context, Object, Parser, RefKey, stream};

fn field<'a>(dict: &'a [(Vec<u8>, Object)], name: &[u8]) -> Option<&'a Object> {
    dict.iter().find(|(key, _)| key == name).map(|(_, value)| value)
}
fn set(dict: &mut Vec<(Vec<u8>, Object)>, name: &[u8], value: Object) {
    if let Some((_, old)) = dict.iter_mut().find(|(key, _)| key == name) { *old = value; }
    else { dict.push((name.to_vec(), value)); }
}

fn metadata_text(text: &str) -> Object {
    let mut encoded = String::from("FEFF");
    for unit in text.encode_utf16() { encoded.push_str(&format!("{unit:04X}")); }
    Object::HexString(encoded.into_bytes())
}

fn update_info(context: &mut Context, date: &str) {
    let valid_info = context.info.as_ref().and_then(|info| context.lookup(info)).is_some_and(|info| matches!(info, Object::Dictionary(_)));
    if !valid_info {
        let id = context.largest_object_number + 1.0;
        context.assign(id, 0.0, Object::Dictionary(Vec::new()));
        context.info = Some(Object::Reference(id, 0.0));
    }
    let info = match context.info.as_mut() {
        Some(Object::Reference(number, generation)) => context.objects.get_mut(&RefKey::new(*number, *generation)),
        Some(value) => Some(value),
        None => None,
    };
    if let Some(Object::Dictionary(info)) = info {
        let producer = "pdf-lib (https://github.com/Hopding/pdf-lib)";
        set(info, b"Producer", metadata_text(producer));
        set(info, b"ModDate", Object::String(date.as_bytes().to_vec()));
        if matches!(field(info, b"Creator"), None | Some(Object::Null)) { set(info, b"Creator", metadata_text(producer)); }
        if matches!(field(info, b"CreationDate"), None | Some(Object::Null)) { set(info, b"CreationDate", Object::String(date.as_bytes().to_vec())); }
    }
}
fn name(out: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(b'/');
    for &byte in bytes {
        if (33..=126).contains(&byte) && !b"#%()/<>[]{}".contains(&byte) { out.push(byte); }
        else { out.extend_from_slice(&[b'#', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]); }
    }
}
pub(super) fn number(number: f64) -> String {
    let value = jscompat::number_to_string(number);
    let Some((mantissa, exponent)) = value.split_once('e') else { return value };
    let Ok(exponent) = exponent.parse::<i32>() else { return value };
    let (negative, digits) = mantissa.strip_prefix('-').map_or((false, mantissa), |digits| (true, digits));
    let decimal = digits.find('.').unwrap_or(digits.len()) as i32 + exponent;
    let digits = digits.replace('.', "");
    let mut result = if decimal <= 0 { format!("0.{}{}", "0".repeat((-decimal) as usize), digits) }
        else if decimal as usize >= digits.len() { format!("{}{}", digits, "0".repeat(decimal as usize - digits.len())) }
        else { format!("{}.{}", &digits[..decimal as usize], &digits[decimal as usize..]) };
    if negative { result.insert(0, '-'); }
    result
}
fn dictionary(out: &mut Vec<u8>, dict: &[(Vec<u8>, Object)], stream_length: Option<usize>) -> Result<(), String> {
    out.extend_from_slice(b"<<\n");
    let mut length_written = false;
    for (key, value) in dict {
        name(out, key); out.push(b' ');
        if key == b"Length" && stream_length.is_some() {
            out.extend_from_slice(stream_length.unwrap_or_default().to_string().as_bytes());
            length_written = true;
        } else { write_object(out, value)?; }
        out.push(b'\n');
    }
    if let Some(length) = stream_length.filter(|_| !length_written) {
        out.extend_from_slice(b"/Length "); out.extend_from_slice(length.to_string().as_bytes()); out.push(b'\n');
    }
    out.extend_from_slice(b">>");
    Ok(())
}
pub(super) fn write_object(out: &mut Vec<u8>, object: &Object) -> Result<(), String> {
    match object {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Object::Number(value) => out.extend_from_slice(number(*value).as_bytes()),
        Object::Name(value) => name(out, value),
        Object::String(value) => { out.push(b'('); out.extend_from_slice(value); out.push(b')'); }
        Object::HexString(value) => { out.push(b'<'); out.extend_from_slice(value); out.push(b'>'); }
        Object::Array(values) => {
            out.extend_from_slice(b"[ ");
            for value in values { write_object(out, value)?; out.push(b' '); }
            out.push(b']');
        }
        Object::Dictionary(dict) => dictionary(out, dict, None)?,
        Object::Stream(dict, bytes) => {
            dictionary(out, dict, Some(bytes.len()))?;
            out.extend_from_slice(b"\nstream\n"); out.extend_from_slice(bytes); out.extend_from_slice(b"\nendstream");
        }
        Object::Reference(object, generation) => out.extend_from_slice(format!("{} {} R", number(*object), number(*generation)).as_bytes()),
        Object::Invalid(bytes) => out.extend_from_slice(bytes),
    }
    Ok(())
}
fn serialize(context: &Context) -> Result<Vec<u8>, String> {
    let mut out = b"%PDF-1.7\n%\x81\x81\x81\x81\n\n".to_vec();
    let mut keys = context.order.clone();
    keys.sort_by(|a, b| f64::from_bits(a.0).total_cmp(&f64::from_bits(b.0)));
    let mut offsets = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(object) = context.objects.get(&key) else { continue };
        let id = f64::from_bits(key.0); let generation = f64::from_bits(key.1);
        offsets.push((id, generation, out.len()));
        out.extend_from_slice(format!("{} {} obj\n", number(id), number(generation)).as_bytes());
        write_object(&mut out, object)?;
        out.extend_from_slice(b"\nendobj\n\n");
    }
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (id, generation, offset) in offsets {
        out.extend_from_slice(format!("{} 1\n{:010} {:0>5} n \n", number(id), offset, number(generation)).as_bytes());
    }
    let mut trailer = vec![(b"Size".to_vec(), Object::Number(context.largest_object_number + 1.0))];
    for (name, value) in [(b"Root".as_slice(), &context.root), (b"Encrypt", &context.encrypt), (b"Info", &context.info), (b"ID", &context.id)] {
        if let Some(value) = value { trailer.push((name.to_vec(), value.clone())); }
    }
    out.extend_from_slice(b"trailer\n"); dictionary(&mut out, &trailer, None)?;
    out.extend_from_slice(format!("\n\nstartxref\n{xref}\n%%EOF").as_bytes());
    Ok(out)
}

/// PDFium puts CFF-flavoured sfnt bytes under FontFile. Match text-edit.ts:
/// relabel only descriptors whose decoded program begins with OTTO.
pub(super) fn relabel_opentype_font_files(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut context = Parser::new(bytes).document()?;
    if context.encrypt.as_ref().and_then(|value| context.lookup(value)).is_some() {
        return Err("Input document to `PDFDocument.load` is encrypted".into());
    }
    let now = chrono::Utc::now().format("D:%Y%m%d%H%M%SZ").to_string();
    update_info(&mut context, &now);
    let mut descriptors = Vec::new();
    for key in &context.order {
        let Some(Object::Dictionary(dict)) = context.objects.get(key) else { continue };
        if !matches!(field(dict, b"Type"), Some(Object::Name(kind)) if kind == b"FontDescriptor") { continue; }
        let Some(font) = field(dict, b"FontFile") else { continue };
        let Some(Object::Stream(stream_dict, payload)) = context.lookup(font) else { continue };
        if stream::decode(&context, stream_dict, payload).is_ok_and(|data| data.starts_with(b"OTTO")) {
            descriptors.push((*key, font.clone()));
        }
    }
    if descriptors.is_empty() { return Ok(bytes.to_vec()); }
    for (key, font) in descriptors {
        if let Object::Reference(object, generation) = font {
            if let Some(Object::Stream(dict, _)) = context.objects.get_mut(&RefKey::new(object, generation)) {
                set(dict, b"Subtype", Object::Name(b"OpenType".to_vec()));
            }
            if let Some(Object::Dictionary(dict)) = context.objects.get_mut(&key) {
                dict.retain(|(name, _)| name != b"FontFile");
                set(dict, b"FontFile3", Object::Reference(object, generation));
            }
        } else if let Object::Stream(mut stream_dict, payload) = font {
            set(&mut stream_dict, b"Subtype", Object::Name(b"OpenType".to_vec()));
            if let Some(Object::Dictionary(dict)) = context.objects.get_mut(&key) {
                dict.retain(|(name, _)| name != b"FontFile");
                set(dict, b"FontFile3", Object::Stream(stream_dict, payload));
            }
        }
    }
    serialize(&context)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relabels_only_cff_sfnt_and_preserves_unrelated_invalid_object() {
        let pdf = b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\n2 0 obj << /Type /FontDescriptor /FontFile 3 0 R >> endobj\n3 0 obj << /Length 4 >> stream\nOTTO\nendstream endobj\n4 0 obj not-an-object endobj\ntrailer << /Root 1 0 R >>\n%%EOF";
        let changed = relabel_opentype_font_files(pdf).unwrap();
        let context = Parser::new(&changed).document().unwrap();
        let Object::Dictionary(descriptor) = context.objects.get(&RefKey::new(2.0, 0.0)).unwrap() else { panic!("descriptor") };
        assert!(field(descriptor, b"FontFile").is_none());
        assert!(field(descriptor, b"FontFile3").is_some());
        assert!(matches!(context.objects.get(&RefKey::new(4.0, 0.0)), Some(Object::Invalid(_))));
    }
    #[test]
    fn no_touch_returns_original_bytes() {
        let pdf = b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF";
        assert_eq!(relabel_opentype_font_files(pdf).unwrap(), pdf);
    }
}
