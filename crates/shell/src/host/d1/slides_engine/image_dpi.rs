fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn plausible(x: f64, y: f64) -> Option<(f64, f64)> {
    ((10.0..=10000.0).contains(&x) && (10.0..=10000.0).contains(&y)).then_some((x, y))
}

fn png(bytes: &[u8]) -> Option<(f64, f64)> {
    let mut at: usize = 8;
    while at.checked_add(12)? <= bytes.len() {
        let len = be32(bytes, at)? as usize;
        let kind = bytes.get(at + 4..at + 8)?;
        if kind == b"IDAT" || kind == b"IEND" { return None; }
        if kind == b"pHYs" && len >= 9 && at.checked_add(17)? <= bytes.len() {
            if bytes[at + 16] != 1 { return None; }
            return plausible(f64::from(be32(bytes, at + 8)?) * 0.0254, f64::from(be32(bytes, at + 12)?) * 0.0254);
        }
        at = at.checked_add(12)?.checked_add(len)?;
    }
    None
}

fn exif(bytes: &[u8]) -> Option<(f64, f64)> {
    if bytes.get(..6)? != b"Exif\0\0" { return None; }
    let tiff = bytes.get(6..)?;
    let little = match tiff.get(..2)? { b"II" => true, b"MM" => false, _ => return None };
    let u16at = |at: usize| -> Option<u16> {
        let value: [u8; 2] = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(value) } else { u16::from_be_bytes(value) })
    };
    let u32at = |at: usize| -> Option<u32> {
        let value: [u8; 4] = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) })
    };
    let ifd = u32at(4)? as usize;
    let count = usize::from(u16at(ifd)?);
    let mut x = None;
    let mut y = None;
    let mut unit = 2;
    for i in 0..count {
        let entry = ifd.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        tiff.get(entry..entry.checked_add(12)?)?;
        let tag = u16at(entry)?;
        if tag == 0x011a || tag == 0x011b {
            let at = u32at(entry + 8)? as usize;
            let numerator = f64::from(u32at(at)?);
            let denominator = f64::from(u32at(at.checked_add(4)?)?);
            let value = (denominator != 0.0).then_some(numerator / denominator);
            if tag == 0x011a { x = value; } else { y = value; }
        } else if tag == 0x0128 { unit = u16at(entry + 8)?; }
    }
    let (x, y) = (x?, y?);
    match unit { 2 => plausible(x, y), 3 => plausible(x * 2.54, y * 2.54), _ => None }
}

fn jpeg(bytes: &[u8]) -> Option<(f64, f64)> {
    let mut at: usize = 2;
    let mut fallback = None;
    while at.checked_add(4)? <= bytes.len() && bytes[at] == 0xff {
        let marker = bytes[at + 1];
        if marker == 0xd8 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 { at += 2; continue; }
        let len = usize::from(be16(bytes, at + 2)?);
        if marker == 0xda || marker == 0xd9 { return fallback; }
        let end = at.checked_add(2)?.checked_add(len)?;
        if end <= bytes.len() {
            let body = bytes.get(at + 4..end)?;
            if marker == 0xe0 && len >= 16 && body.starts_with(b"JFIF\0") {
                let units = *body.get(7)?;
                let x = f64::from(be16(body, 8)?);
                let y = f64::from(be16(body, 10)?);
                if units == 1 { return plausible(x, y); }
                if units == 2 { return plausible(x * 2.54, y * 2.54); }
            }
            if marker == 0xe1 && fallback.is_none() { fallback = exif(body); }
        }
        at = end;
    }
    fallback
}

pub(super) fn read(bytes: &[u8]) -> Option<(f64, f64)> {
    if bytes.len() < 16 { return None; }
    if bytes.starts_with(b"\x89PNG") { png(bytes) }
    else if bytes.starts_with(b"\xff\xd8") { jpeg(bytes) }
    else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_and_jpeg_density_and_truncation() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\x09pHYs\0\0\x0e\xc4\0\0\x0e\xc4\x01\0\0\0\0";
        let (x, y) = read(png).unwrap();
        assert!((x - 96.0).abs() < 0.02 && (y - 96.0).abs() < 0.02);
        let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x02\x01\0\x60\0\x60\0\0\xff\xd9\0\0";
        assert_eq!(read(jpeg), Some((96.0, 96.0)));
        assert_eq!(read(&png[..16]), None);
    }
}
