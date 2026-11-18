//! The JPEG/PNG header test used by the PDF tools picker.

/// Matches `readImageInfo`'s JPEG/PNG orientation test, including malformed
/// EXIF entries and multiple APP1 segments before the start-of-frame.
pub(super) fn unrotated_image(bytes: &[u8]) -> bool {
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 13, 10, 26, 10] {
        return is_image(bytes);
    }
    if bytes.len() < 4 || bytes[..2] != [0xff, 0xd8] { return false; }
    let mut at = 2;
    let mut orientation = 1;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff { return false; }
        let marker = bytes[at + 1];
        if marker == 0xff { at += 1; continue; }
        if marker == 0xd8 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 { at += 2; continue; }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if length < 2 || length > bytes.len() - at - 2 { return false; }
        let body = at + 4;
        let end = at + 2 + length;
        if marker == 0xe1 && end - body > 6 && &bytes[body..body + 4] == b"Exif" {
            orientation = exif_orientation(bytes, body + 6, end);
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if end - body < 5 { return false; }
            return orientation == 1 && bytes[body + 1..body + 3] != [0, 0] && bytes[body + 3..body + 5] != [0, 0];
        }
        if marker == 0xda { return false; }
        at = end;
    }
    false
}

fn exif_orientation(bytes: &[u8], tiff: usize, end: usize) -> u16 {
    if end - tiff < 8 { return 1; }
    let little = match &bytes[tiff..tiff + 2] { b"II" => true, b"MM" => false, _ => return 1 };
    let u16_at = |at: usize| {
        let value = [bytes[at], bytes[at + 1]];
        if little { u16::from_le_bytes(value) } else { u16::from_be_bytes(value) }
    };
    let word = [bytes[tiff + 4], bytes[tiff + 5], bytes[tiff + 6], bytes[tiff + 7]];
    let offset = if little { u32::from_le_bytes(word) } else { u32::from_be_bytes(word) };
    let Some(ifd) = tiff.checked_add(offset as usize) else { return 1 };
    if ifd > end || end - ifd < 2 { return 1; }
    for index in 0..usize::from(u16_at(ifd)) {
        let Some(entry) = ifd.checked_add(2 + index * 12) else { return 1 };
        if entry > end || end - entry < 12 { return 1; }
        if u16_at(entry) == 0x0112 {
            let value = u16_at(entry + 8);
            return if (1..=8).contains(&value) { value } else { 1 };
        }
    }
    1
}

pub(super) fn is_image(bytes: &[u8]) -> bool {
    if bytes.len() >= 4 && bytes[..2] == [0xff, 0xd8] {
        let mut at = 2;
        while at + 4 <= bytes.len() {
            if bytes[at] != 0xff { return false; }
            let marker = bytes[at + 1];
            if marker == 0xff { at += 1; continue; }
            if marker == 0xd8 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 { at += 2; continue; }
            let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
            if length < 2 || at + 2 + length > bytes.len() { return false; }
            let body = at + 4;
            let end = at + 2 + length;
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                if end - body < 5 { return false; }
                let height = u16::from_be_bytes([bytes[body + 1], bytes[body + 2]]);
                let width = u16::from_be_bytes([bytes[body + 3], bytes[body + 4]]);
                return width > 0 && height > 0;
            }
            if marker == 0xda { return false; }
            at = end;
        }
        return false;
    }
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 13, 10, 26, 10] {
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return width > 0 && height > 0;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{is_image, unrotated_image};

    #[test]
    fn recognizes_headers_and_rejects_truncation_and_zero_sizes() {
        let png = [0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 2];
        assert!(is_image(&png));
        assert!(!is_image(&png[..23]));
        let mut zero = png;
        zero[19] = 0;
        assert!(!is_image(&zero));
        assert!(is_image(&[0xff, 0xd8, 0xff, 0xc0, 0, 7, 8, 0, 2, 0, 3]));
        assert!(!is_image(&[0xff, 0xd8, 0xff, 0xc0, 0, 8, 8, 0, 2, 0, 3]));
    }

    #[test]
    fn compression_uses_original_exif_orientation_rules() {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1, 0, 30, b'E', b'x', b'i', b'f', 0, 0,
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0,
            0xff, 0xc0, 0, 7, 8, 0, 2, 0, 3];
        assert!(!unrotated_image(&jpeg));
        jpeg[30] = 9;
        assert!(unrotated_image(&jpeg));
        jpeg[30] = 1;
        assert!(unrotated_image(&jpeg));
    }
}
