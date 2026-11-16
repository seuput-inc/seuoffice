// Pixel conversion derived from @pdf-lib/upng 1.0.1, UPNG.js.
// MIT License, Copyright (c) 2017 Photopea.
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

mod inflate;

#[derive(Debug, PartialEq)]
pub(super) enum Transparency {
    Gray(u16),
    Rgb([u16; 3]),
    Palette(Vec<u8>),
}

#[derive(Clone, Copy)]
struct Frame {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    blend: u8,
}

/// UPNG-compatible PNG container and first-frame conversion. This remains
/// private to differential testing: raw inflater and malformed-container
/// equivalence must be proven before using it as a signer authorization check.
pub(super) fn rgba_png(input: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    fn word(data: &[u8], offset: usize) -> Result<usize, String> {
        let first = *data.get(offset).ok_or("invalid PNG numeric value")?;
        Ok((usize::from(first) << 24)
            | (usize::from(data.get(offset + 1).copied().unwrap_or(0)) << 16)
            | (usize::from(data.get(offset + 2).copied().unwrap_or(0)) << 8)
            | usize::from(data.get(offset + 3).copied().unwrap_or(0)))
    }
    fn short(data: &[u8], offset: usize) -> Result<u16, String> {
        Ok((u16::from(data.get(offset).copied().unwrap_or(0)) << 8)
            | u16::from(data.get(offset + 1).copied().unwrap_or(0)))
    }
    fn bytes(data: &[u8], length: usize) -> Result<Vec<u8>, String> {
        let mut result = Vec::new();
        result
            .try_reserve_exact(length)
            .map_err(|_| "PNG allocation failed")?;
        result.resize(length, 0);
        let count = data.len().min(length);
        result[..count].copy_from_slice(&data[..count]);
        Ok(result)
    }
    fn inflate(
        data: &[u8],
        width: usize,
        height: usize,
        bits: usize,
        interlace: u8,
    ) -> Result<Vec<u8>, String> {
        let line = width
            .checked_mul(bits)
            .and_then(|value| value.checked_add(7))
            .ok_or("PNG dimensions overflow")?
            / 8;
        let capacity = line
            .checked_add(1 + usize::from(interlace))
            .and_then(|value| value.checked_mul(height))
            .ok_or("PNG dimensions overflow")?;
        inflate::decode(data, capacity)
    }

    if !input.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("not a PNG file".into());
    }
    let (mut width, mut height, mut color, mut depth, mut interlace) = (0, 0, 0, 0, 0);
    let mut palette = Vec::new();
    let mut transparency = None;
    let mut idat = Vec::new();
    let mut fdat = Vec::new();
    let mut frames = Vec::new();
    let mut animated = false;
    let mut offset = 8usize;
    while offset < input.len() {
        let length = word(input, offset)?;
        let start = offset.checked_add(8).ok_or("PNG chunk overflow")?;
        let end = start.checked_add(length).ok_or("PNG chunk overflow")?;
        let Some(kind) = input.get(offset + 4..start) else {
            break;
        };
        let body = input.get(start..).unwrap_or(&[]);
        match kind {
            b"IHDR" => {
                width = word(body, 0)?;
                height = word(body, 4)?;
                depth = *body.get(8).ok_or("truncated PNG header")?;
                color = *body.get(9).ok_or("truncated PNG header")?;
                interlace = *body.get(12).ok_or("truncated PNG header")?;
            }
            b"IDAT" => {
                let remaining = input.len().saturating_sub(idat.len());
                idat.extend_from_slice(&bytes(body, length.min(remaining))?);
            }
            b"PLTE" => palette = bytes(body, length)?,
            b"tRNS" => {
                transparency = match color {
                    0 => Some(Transparency::Gray(short(body, 0)?)),
                    2 => Some(Transparency::Rgb([
                        short(body, 0)?,
                        short(body, 2)?,
                        short(body, 4)?,
                    ])),
                    3 => Some(Transparency::Palette(bytes(body, length)?)),
                    _ => None,
                }
            }
            b"acTL" => animated = true,
            b"fcTL" => frames.push(Frame {
                width: word(body, 4)?,
                height: word(body, 8)?,
                x: word(body, 12)?,
                y: word(body, 16)?,
                blend: *body.get(25).ok_or("truncated PNG frame")?,
            }),
            b"fdAT" => {
                if !animated {
                    return Err("PNG frame data without animation".into());
                }
                let remaining = input.len().saturating_sub(fdat.len());
                let frame_length = length.saturating_sub(4).min(remaining);
                fdat.extend_from_slice(&bytes(body.get(4..).unwrap_or(&[]), frame_length)?);
            }
            b"IEND" => break,
            _ => {}
        }
        offset = end.checked_add(4).ok_or("PNG chunk overflow")?;
    }
    let channels = match color {
        0 | 3 => 1usize,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return Err("unknown PNG color type".into()),
    };
    let bits = channels
        .checked_mul(usize::from(depth))
        .ok_or("PNG dimensions overflow")?;
    // UPNG's IDAT destination is a zero-initialized array as long as the
    // entire PNG, unlike the tightly sliced fdAT frame buffer.
    idat.resize(input.len(), 0);
    let mut compressed = inflate(&idat, width, height, bits, interlace)?;
    let data = unfilter(&mut compressed, width, height, bits, interlace)?;
    if !animated {
        return Ok((
            width,
            height,
            pixels(
                &data,
                width,
                height,
                color,
                depth,
                &palette,
                transparency.as_ref(),
            )?,
        ));
    }
    if frames.len() != 1 {
        return Err("animated PNGs are not supported".into());
    }
    let frame = frames[0];
    let frame_data = if fdat.is_empty() {
        data
    } else {
        let mut compressed = inflate(&fdat, frame.width, frame.height, bits, interlace)?;
        unfilter(&mut compressed, width, height, bits, interlace)?
    };
    let source = pixels(
        &frame_data,
        frame.width,
        frame.height,
        color,
        depth,
        &palette,
        transparency.as_ref(),
    )?;
    let length = width
        .checked_mul(height)
        .and_then(|value| value.checked_mul(4))
        .ok_or("PNG dimensions overflow")?;
    let mut canvas = Vec::new();
    canvas
        .try_reserve_exact(length)
        .map_err(|_| "PNG allocation failed")?;
    canvas.resize(length, 0);
    for y in 0..frame.height.min(height) {
        for x in 0..frame.width.min(width) {
            let from = (y * frame.width + x) * 4;
            let Some(to) = frame
                .y
                .checked_add(y)
                .and_then(|y| y.checked_mul(width))
                .and_then(|value| frame.x.checked_add(x).and_then(|x| value.checked_add(x)))
                .and_then(|value| value.checked_mul(4))
            else {
                continue;
            };
            if to.checked_add(4).is_none_or(|end| end > canvas.len()) {
                continue;
            }
            if frame.blend == 0 {
                canvas[to..to + 4].copy_from_slice(&source[from..from + 4]);
            } else if frame.blend == 1 {
                let alpha = f64::from(source[from + 3]) * (1.0 / 255.0);
                let inverse = if alpha == 0.0 { 0.0 } else { 1.0 / alpha };
                canvas[to + 3] = (255.0 * alpha) as u8;
                for channel in 0..3 {
                    canvas[to + channel] =
                        ((f64::from(source[from + channel]) * alpha) * inverse) as u8;
                }
            }
        }
    }
    Ok((width, height, canvas))
}

/// The input is UPNG's unfiltered, deinterlaced pixel data, not PNG bytes.
/// This ports decodeImage separately from container/inflate/filter handling.
pub(super) fn pixels(
    data: &[u8],
    width: usize,
    height: usize,
    color: u8,
    depth: u8,
    palette: &[u8],
    transparency: Option<&Transparency>,
) -> Result<Vec<u8>, String> {
    let channels = match color {
        0 | 3 => 1usize,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return Err("unknown PNG color type".into()),
    };
    let bits = channels
        .checked_mul(usize::from(depth))
        .ok_or("PNG dimensions overflow")?;
    let line = width
        .checked_mul(bits)
        .and_then(|n| n.checked_add(7))
        .ok_or("PNG dimensions overflow")?
        / 8;
    let area = width.checked_mul(height).ok_or("PNG dimensions overflow")?;
    let length = area.checked_mul(4).ok_or("PNG dimensions overflow")?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| "PNG allocation failed")?;
    output.resize(length, 0);
    let byte = |offset: usize| data.get(offset).copied().unwrap_or(0);
    let ushort =
        |offset: usize| u16::from(byte(offset)) * 256 + u16::from(byte(offset.saturating_add(1)));
    for y in 0..height {
        for x in 0..width {
            let pixel = y * width + x;
            let row = y * line;
            let out = &mut output[pixel * 4..pixel * 4 + 4];
            match (color, depth) {
                (6, 8) => {
                    for channel in 0..4 {
                        out[channel] = byte(pixel * 4 + channel);
                    }
                }
                (6, 16) => {
                    for channel in 0..4 {
                        out[channel] = byte(pixel * 8 + channel * 2);
                    }
                }
                (2, 8 | 16) => {
                    let stride = if depth == 16 { 6 } else { 3 };
                    let step = if depth == 16 { 2 } else { 1 };
                    let offset = pixel * stride;
                    for channel in 0..3 {
                        out[channel] = byte(offset + channel * step);
                    }
                    out[3] = 255;
                    if let Some(Transparency::Rgb(values)) = transparency {
                        let equal = (0..3).all(|channel| {
                            if depth == 16 {
                                ushort(offset + channel * 2) == values[channel]
                            } else {
                                u16::from(byte(offset + channel)) == values[channel]
                            }
                        });
                        if equal {
                            out[3] = 0;
                        }
                    }
                }
                (3, 1 | 2 | 4 | 8) => {
                    let index = if depth == 8 {
                        usize::from(byte(pixel))
                    } else {
                        let per_byte = 8 / usize::from(depth);
                        let shift = 8 - usize::from(depth) * (x % per_byte + 1);
                        usize::from((byte(row + x / per_byte) >> shift) & ((1 << depth) - 1))
                    };
                    for channel in 0..3 {
                        out[channel] = palette.get(index * 3 + channel).copied().unwrap_or(0);
                    }
                    out[3] = match transparency {
                        Some(Transparency::Palette(alpha)) => {
                            alpha.get(index).copied().unwrap_or(255)
                        }
                        _ => 255,
                    };
                }
                (4, 8 | 16) => {
                    let step = if depth == 16 { 2 } else { 1 };
                    let offset = pixel * step * 2;
                    let gray = byte(offset);
                    out.copy_from_slice(&[gray, gray, gray, byte(offset + step)]);
                }
                (0, 1 | 2 | 4 | 8 | 16) => {
                    let raw = if depth < 8 {
                        let per_byte = 8 / usize::from(depth);
                        let shift = 8 - usize::from(depth) * (x % per_byte + 1);
                        u16::from((byte(row + x / per_byte) >> shift) & ((1 << depth) - 1))
                    } else if depth == 8 {
                        u16::from(byte(row + x))
                    } else {
                        ushort(row + x * 2)
                    };
                    let gray = if depth == 16 {
                        byte(row + x * 2)
                    } else {
                        (raw * (255 / ((1u16 << depth) - 1))) as u8
                    };
                    // UPNG.js uses x << i here; i is hoisted but undefined in
                    // this branch, so JavaScript coerces the shift count to 0.
                    let compared = if depth == 16 { ushort(row + x) } else { raw };
                    let alpha = if matches!(transparency, Some(Transparency::Gray(value)) if *value == compared)
                    {
                        0
                    } else {
                        255
                    };
                    out.copy_from_slice(&[gray, gray, gray, alpha]);
                }
                _ => {} // UPNG leaves unsupported color/depth combinations zero-filled.
            }
        }
    }
    Ok(output)
}

/// Reconstruct scanlines in place as UPNG.decode._filterZero does. The
/// compressed-buffer offset matters for the first-row initialization.
fn filter_zero(
    data: &mut [u8],
    offset: usize,
    width: usize,
    height: usize,
    bits: usize,
) -> Result<(), String> {
    fn get(data: &[u8], index: isize) -> u8 {
        usize::try_from(index)
            .ok()
            .and_then(|index| data.get(index))
            .copied()
            .unwrap_or(0)
    }
    fn put(data: &mut [u8], index: usize, value: u8) {
        if let Some(byte) = data.get_mut(index) {
            *byte = value;
        }
    }
    fn paeth(a: u8, b: u8, c: u8) -> u8 {
        let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
        let p = a + b - c;
        let (pa, pb, pc) = ((p - a).pow(2), (p - b).pow(2), (p - c).pow(2));
        if pa <= pb && pa <= pc {
            a as u8
        } else if pb <= pc {
            b as u8
        } else {
            c as u8
        }
    }
    let line = width
        .checked_mul(bits)
        .and_then(|n| n.checked_add(7))
        .ok_or("PNG dimensions overflow")?
        / 8;
    let bytes_per_pixel = bits.checked_add(7).ok_or("PNG dimensions overflow")? / 8;
    let first = get(data, offset as isize);
    if first > 1 {
        put(
            data,
            offset,
            match first {
                2 | 3 => 0,
                4 => 1,
                _ => 0,
            },
        );
    }
    if first == 3 {
        for x in bytes_per_pixel..line {
            let value = get(data, (x + 1) as isize)
                .wrapping_add(get(data, (x + 1 - bytes_per_pixel) as isize) >> 1);
            put(data, x + 1, value);
        }
    }
    for y in 0..height {
        let dest = offset
            .checked_add(y.checked_mul(line).ok_or("PNG dimensions overflow")?)
            .ok_or("PNG dimensions overflow")?;
        let source = dest.checked_add(y + 1).ok_or("PNG dimensions overflow")?;
        let filter = get(data, source as isize - 1);
        for x in 0..line {
            let raw = get(data, (source + x) as isize);
            let left = if x < bytes_per_pixel {
                0
            } else {
                get(data, (dest + x - bytes_per_pixel) as isize)
            };
            let above = get(data, (dest + x) as isize - line as isize);
            let upper_left = if x < bytes_per_pixel {
                0
            } else {
                get(data, (dest + x - bytes_per_pixel) as isize - line as isize)
            };
            let value = match filter {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(above),
                3 => raw.wrapping_add(((u16::from(left) + u16::from(above)) >> 1) as u8),
                _ => raw.wrapping_add(paeth(left, above, upper_left)),
            };
            put(data, dest + x, value);
        }
    }
    Ok(())
}

/// Adam7 reconstruction, including UPNG's packed-sample placement.
pub(super) fn unfilter(
    data: &mut [u8],
    width: usize,
    height: usize,
    bits: usize,
    interlace: u8,
) -> Result<Vec<u8>, String> {
    if interlace != 1 {
        if interlace == 0 {
            filter_zero(data, 0, width, height, bits)?;
        }
        return Ok(data.to_vec());
    }
    let line = width
        .checked_mul(bits)
        .and_then(|n| n.checked_add(7))
        .ok_or("PNG dimensions overflow")?
        / 8;
    let mut image = vec![0; line.checked_mul(height).ok_or("PNG dimensions overflow")?];
    let rows = [0, 0, 4, 0, 2, 0, 1];
    let cols = [0, 4, 0, 2, 0, 1, 0];
    let row_step = [8, 8, 8, 4, 4, 2, 2];
    let col_step = [8, 8, 4, 4, 2, 2, 1];
    let mut offset = 0usize;
    for pass in 0..7 {
        let pass_width = (cols[pass]..width).step_by(col_step[pass]).count();
        let pass_height = (rows[pass]..height).step_by(row_step[pass]).count();
        let pass_line = pass_width
            .checked_mul(bits)
            .and_then(|n| n.checked_add(7))
            .ok_or("PNG dimensions overflow")?
            / 8;
        filter_zero(data, offset, pass_width, pass_height, bits)?;
        for (y, row) in (rows[pass]..height).step_by(row_step[pass]).enumerate() {
            let mut bit = (offset + y * pass_line) * 8;
            for col in (cols[pass]..width).step_by(col_step[pass]) {
                if matches!(bits, 1 | 2 | 4) {
                    let source = data.get(bit / 8).copied().unwrap_or(0);
                    let shift = 8 - bits - bit % 8;
                    let value = (source >> shift) & ((1 << bits) - 1);
                    image[row * line + col * bits / 8] |= value << (8 - bits - col * bits % 8);
                } else if bits >= 8 {
                    let bytes = bits / 8;
                    for channel in 0..bytes {
                        image[row * line + col * bytes + channel] =
                            data.get(bit / 8 + channel).copied().unwrap_or(0);
                    }
                }
                bit += bits;
            }
        }
        if pass_width != 0 && pass_height != 0 {
            offset = offset
                .checked_add(
                    pass_height
                        .checked_mul(pass_line + 1)
                        .ok_or("PNG dimensions overflow")?,
                )
                .ok_or("PNG dimensions overflow")?;
        }
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        width: u32,
        height: u32,
        depth: u8,
        color: u8,
        interlace: u8,
        chunks: &[(&[u8; 4], &[u8])],
        raw: &[u8],
    ) -> Vec<u8> {
        fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            png.extend_from_slice(&(body.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(body);
            let mut crc = !0u32;
            for byte in kind.iter().chain(body) {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
                }
            }
            png.extend_from_slice(&(!crc).to_be_bytes());
        }
        let len = raw.len() as u16;
        let mut data = vec![0x78, 0x01, 0x01];
        data.extend_from_slice(&len.to_le_bytes());
        data.extend_from_slice(&(!len).to_le_bytes());
        data.extend_from_slice(raw);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in raw {
            a = (a + u32::from(*byte)) % 65521;
            b = (b + a) % 65521;
        }
        data.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[depth, color, 0, 0, interlace]);
        chunk(&mut png, b"IHDR", &header);
        for (kind, body) in chunks {
            chunk(&mut png, kind, body);
        }
        chunk(&mut png, b"IDAT", &data);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn png_container_corpus_covers_palette_transparency_and_adam7() {
        let palette = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let alpha = [255, 0, 17];
        let packed = fixture(
            3,
            1,
            2,
            3,
            0,
            &[(b"PLTE", &palette), (b"tRNS", &alpha)],
            &[0, 0b0110_0000],
        );
        assert_eq!(
            rgba_png(&packed).unwrap(),
            (3, 1, vec![4, 5, 6, 0, 7, 8, 9, 17, 1, 2, 3, 255])
        );
        let adam7 = fixture(1, 1, 8, 0, 1, &[], &[0, 173]);
        assert_eq!(rgba_png(&adam7).unwrap(), (1, 1, vec![173, 173, 173, 255]));
        let rgba16 = fixture(
            1,
            1,
            16,
            6,
            0,
            &[],
            &[0, 0x12, 0xff, 0x34, 0xee, 0x56, 0xdd, 0x78, 0xcc],
        );
        assert_eq!(
            rgba_png(&rgba16).unwrap(),
            (1, 1, vec![0x12, 0x34, 0x56, 0x78])
        );
        let mut bad_crc = rgba16;
        bad_crc[29] ^= 0x80;
        assert_eq!(
            rgba_png(&bad_crc).unwrap(),
            (1, 1, vec![0x12, 0x34, 0x56, 0x78])
        );
        let transparent = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let rgb16 = fixture(
            1,
            1,
            16,
            2,
            0,
            &[(b"tRNS", &transparent)],
            &[0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc],
        );
        assert_eq!(rgba_png(&rgb16).unwrap(), (1, 1, vec![0x12, 0x56, 0x9a, 0]));
    }

    #[test]
    fn one_frame_apng_is_composited_but_multiple_frames_are_rejected() {
        let animation = [0, 0, 0, 1, 0, 0, 0, 0];
        let mut frame = [0u8; 26];
        frame[7] = 1;
        frame[11] = 1;
        let single = fixture(
            1,
            1,
            8,
            6,
            0,
            &[(b"acTL", &animation), (b"fcTL", &frame)],
            &[0, 4, 5, 6, 128],
        );
        assert_eq!(rgba_png(&single).unwrap(), (1, 1, vec![4, 5, 6, 128]));
        let two = fixture(
            1,
            1,
            8,
            6,
            0,
            &[(b"acTL", &animation), (b"fcTL", &frame), (b"fcTL", &frame)],
            &[0, 4, 5, 6, 128],
        );
        assert!(rgba_png(&two).is_err());
    }

    #[test]
    fn gray16_transparency_uses_upngs_byte_offset() {
        let rgba = pixels(
            &[0x12, 0x34, 0x56, 0x78],
            2,
            1,
            0,
            16,
            &[],
            Some(&Transparency::Gray(0x3456)),
        )
        .unwrap();
        assert_eq!(rgba, [0x12, 0x12, 0x12, 255, 0x56, 0x56, 0x56, 0]);
    }

    #[test]
    fn palette_and_alpha_unpack_most_significant_samples_first() {
        let rgba = pixels(
            &[0b0110_0000],
            3,
            1,
            3,
            2,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9],
            Some(&Transparency::Palette(vec![255, 0, 17])),
        )
        .unwrap();
        assert_eq!(rgba, [4, 5, 6, 0, 7, 8, 9, 17, 1, 2, 3, 255]);
    }

    #[test]
    fn reconstructs_sub_and_up_filtered_rows() {
        let mut rows = [1, 10, 10, 10, 2, 1, 2, 3];
        let decoded = unfilter(&mut rows, 3, 2, 8, 0).unwrap();
        assert_eq!(&decoded[..6], &[10, 20, 30, 11, 22, 33]);
    }

    #[test]
    fn reconstructs_single_pixel_adam7() {
        let mut passes = [0, 173];
        assert_eq!(unfilter(&mut passes, 1, 1, 8, 1).unwrap(), [173]);
    }

    #[test]
    fn gray16_transparency_differs_from_standard_png_decoder() {
        fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            png.extend_from_slice(&(body.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(body);
            let mut crc = !0u32;
            for byte in kind.iter().chain(body) {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            png.extend_from_slice(&(!crc).to_be_bytes());
        }
        let raw = [0, 0x12, 0x34, 0x56, 0x78];
        let mut adler_a = 1u32;
        let mut adler_b = 0u32;
        for byte in raw {
            adler_a = (adler_a + u32::from(byte)) % 65521;
            adler_b = (adler_b + adler_a) % 65521;
        }
        let mut zlib = vec![0x78, 0x01, 1, 5, 0, 0xfa, 0xff];
        zlib.extend_from_slice(&raw);
        zlib.extend_from_slice(&((adler_b << 16) | adler_a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 1, 16, 0, 0, 0, 0]);
        chunk(&mut png, b"tRNS", &[0x34, 0x56]);
        chunk(&mut png, b"IDAT", &zlib);
        chunk(&mut png, b"IEND", &[]);
        let standard = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        let upng = pixels(
            &raw[1..],
            2,
            1,
            0,
            16,
            &[],
            Some(&Transparency::Gray(0x3456)),
        )
        .unwrap();
        assert_eq!(standard.as_raw()[7], 255);
        assert_eq!(upng[7], 0);
        assert_eq!(rgba_png(&png).unwrap(), (2, 1, upng));
    }
}
