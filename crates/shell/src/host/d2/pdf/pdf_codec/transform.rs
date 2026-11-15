use super::{Object, Parser, RefKey, field, serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::host::d2::pdf) enum PayloadKind { String, Stream }

pub(in crate::host::d2::pdf) fn literal_bytes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let byte = raw[i]; i += 1;
        if byte != b'\\' { out.push(byte); continue; }
        if i == raw.len() { break; }
        let escaped = raw[i]; i += 1;
        match escaped {
            b'\n' | b'\r' => {},
            b'n' => out.push(b'\n'), b'r' => out.push(b'\r'), b't' => out.push(b'\t'),
            b'b' => out.push(8), b'f' => out.push(12), 8 => out.push(b'\\'),
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                for _ in 1..3 {
                    if i >= raw.len() || !(b'0'..=b'7').contains(&raw[i]) { break; }
                    value = value * 8 + u16::from(raw[i] - b'0'); i += 1;
                }
                out.push(value as u8);
            }
            byte => out.push(byte),
        }
    }
    out
}
pub(in crate::host::d2::pdf) fn hex_bytes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity((raw.len() + 1) / 2);
    for pair in raw.chunks(2) {
        let text: String = pair.iter().map(|byte| char::from(*byte)).chain((pair.len() == 1).then_some('0')).collect();
        out.push(jscompat::to_int32(jscompat::parse_int(&text, jscompat::Radix::Sixteen)) as u8);
    }
    out
}
fn hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes { out.push(DIGITS[(b >> 4) as usize]); out.push(DIGITS[(b & 15) as usize]); }
    out
}
fn strings(object: &mut Object, number: f64, generation: f64, transform: &mut impl FnMut(f64, f64, PayloadKind, &[u8]) -> Result<Vec<u8>, String>) -> Result<(), String> {
    match object {
        Object::String(raw) => { *object = Object::HexString(hex(&transform(number, generation, PayloadKind::String, &literal_bytes(raw))?)); }
        Object::HexString(raw) => { *object = Object::HexString(hex(&transform(number, generation, PayloadKind::String, &hex_bytes(raw))?)); }
        Object::Array(items) => for item in items { strings(item, number, generation, transform)?; },
        Object::Dictionary(entries) => for (_, value) in entries { strings(value, number, generation, transform)?; },
        _ => {}
    }
    Ok(())
}
fn metadata(dict: &[(Vec<u8>, Object)]) -> bool { matches!(field(dict, b"Type"), Some(Object::Name(name)) if name == b"Metadata") }

/// Predecrypt raw streams in original physical order, then parse object streams
/// and decrypt only strings belonging to physical top-level object numbers.
pub(in crate::host::d2::pdf) fn decrypt(bytes: &[u8], encrypt_metadata: bool, mut transform: impl FnMut(f64, f64, PayloadKind, &[u8]) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
    let (bytes, top_level) = super::predecrypt::run(bytes, encrypt_metadata, |number, generation, bytes| transform(number, generation, PayloadKind::Stream, bytes))?;
    let mut context = Parser::new(&bytes).document()?;
    let skip = match &context.encrypt { Some(Object::Reference(number, generation)) => Some(RefKey::new(*number, *generation)), _ => None };
    for key in context.order.clone() {
        if Some(key) == skip || !top_level.contains(&key.0) { continue; }
        let Some(object) = context.objects.get_mut(&key) else { continue };
        let number = f64::from_bits(key.0); let generation = f64::from_bits(key.1);
        if let Object::Stream(dict, _) = object {
            if !encrypt_metadata && metadata(dict) { continue; }
            for (_, value) in dict { strings(value, number, generation, &mut transform)?; }
        } else { strings(object, number, generation, &mut transform)?; }
    }
    context.encrypt = None;
    serialize(&context)
}

/// Reapply a native-authenticated original protection using its normalized
/// encryption dictionary and optional original file IDs.
pub(in crate::host::d2::pdf) fn reprotect(bytes: &[u8], dictionary: &[u8], file_id: Option<&[Vec<u8>; 2]>, encrypt_metadata: bool, mut transform: impl FnMut(f64, f64, PayloadKind, &[u8]) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
    let mut context = Parser::new(bytes).document()?;
    if context.encrypt.as_ref().and_then(|value| context.lookup(value)).is_some() { return Err("The edited PDF is already encrypted.".into()); }
    let Object::Dictionary(dictionary) = Parser::new(dictionary).object()? else { return Err("invalid PDF encryption dictionary".into()) };
    let next = context.largest_object_number + 1.0;
    context.assign(next, 0.0, Object::Dictionary(dictionary));
    let skip = RefKey::new(next, 0.0);
    context.encrypt = Some(Object::Reference(next, 0.0));
    if let Some(id) = file_id {
        context.id = Some(Object::Array(id.iter().map(|id| Object::HexString(hex(id))).collect()));
    } else if context.id.is_none() {
        let mut id = [0u8; 16]; openssl::rand::rand_bytes(&mut id).map_err(|error| error.to_string())?;
        let id = Object::HexString(hex(&id)); context.id = Some(Object::Array(vec![id.clone(), id]));
    }
    for key in context.order.clone() {
        if key == skip { continue; }
        let Some(object) = context.objects.get_mut(&key) else { continue };
        let number = f64::from_bits(key.0); let generation = f64::from_bits(key.1);
        if let Object::Stream(dict, bytes) = object {
            if !encrypt_metadata && metadata(dict) { continue; }
            *bytes = transform(number, generation, PayloadKind::Stream, bytes)?;
            for (_, value) in dict { strings(value, number, generation, &mut transform)?; }
        } else { strings(object, number, generation, &mut transform)?; }
    }
    serialize(&context)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pdf_lib_string_byte_decoding() {
        assert_eq!(literal_bytes(b"a\\053b\\n\\\r\nx"), b"a+b\n\nx");
        assert_eq!(hex_bytes(b"ABC"), vec![0xab, 0xc0]);
        assert_eq!(hex_bytes(b" 1"), vec![1]);
    }
    #[test]
    fn roundtrips_strings_streams_and_unencrypted_metadata() {
        let plain = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Label (A\\053B) /Data 2 0 R >> endobj\n2 0 obj << /Length 4 >> stream\nABCD\nendstream endobj\n3 0 obj << /Type /Metadata /Length 3 >> stream\nxyz\nendstream endobj\ntrailer << /Root 1 0 R /ID [<11><22>] >>\n%%EOF";
        let cipher = |_: f64, _: f64, _: PayloadKind, bytes: &[u8]| Ok(bytes.iter().map(|byte| byte ^ 0x20).collect());
        let encrypted = reprotect(plain, b"<< /Filter /Standard >>", None, false, cipher).unwrap();
        let decoded = decrypt(&encrypted, false, cipher).unwrap();
        let context = Parser::new(&decoded).document().unwrap();
        let Object::Dictionary(root) = context.objects.get(&RefKey::new(1.0, 0.0)).unwrap() else { panic!("root") };
        assert_eq!(field(root, b"Label"), Some(&Object::HexString(b"412B42".to_vec())));
        assert!(matches!(context.objects.get(&RefKey::new(2.0, 0.0)), Some(Object::Stream(_, data)) if data == b"ABCD"));
        assert!(matches!(context.objects.get(&RefKey::new(3.0, 0.0)), Some(Object::Stream(_, data)) if data == b"xyz"));
        assert!(context.encrypt.is_none());
    }
    #[test]
    fn decrypts_object_stream_before_parsing_without_decrypting_resident_strings_twice() {
        let content = b"2 0 << /Label (secret) >>";
        let encrypted: Vec<_> = content.iter().map(|byte| byte ^ 0x20).collect();
        let mut pdf = format!("%PDF-1.7\n1 0 obj << /Type /Catalog /Child 2 0 R >> endobj\n5 0 obj << /Type /ObjStm /N 1 /First 4 /Length {} >> stream\n", encrypted.len()).into_bytes();
        pdf.extend_from_slice(&encrypted);
        pdf.extend_from_slice(b"\nendstream endobj\n8 0 obj << /Filter /Standard >> endobj\ntrailer << /Root 1 0 R /Encrypt 8 0 R >>\n%%EOF");
        let result = decrypt(&pdf, true, |_, _, _, bytes| Ok(bytes.iter().map(|byte| byte ^ 0x20).collect())).unwrap();
        let context = Parser::new(&result).document().unwrap();
        let Object::Dictionary(child) = context.objects.get(&RefKey::new(2.0, 0.0)).unwrap() else { panic!("object stream child") };
        assert_eq!(field(child, b"Label"), Some(&Object::String(b"secret".to_vec())));
    }
}
