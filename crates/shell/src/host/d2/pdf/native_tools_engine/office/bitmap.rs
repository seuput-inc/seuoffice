//! PDFium bitmap extraction and image-coordinate transformations.
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession, read_number};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Rgba {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
}
fn scalar(s: &mut PdfiumSession, name: &str, bitmap: u64) -> Result<f64, PdfiumError> {
    let v = s.call(name, &[json!({"ref":bitmap})])?;
    read_number(&v).ok_or_else(|| PdfiumError::from(format!("invalid {name} result")))
}
fn pointer(v: &Value) -> Result<Option<u64>, PdfiumError> {
    if v.as_u64() == Some(0) {
        Ok(None)
    } else {
        v.get("ref")
            .and_then(Value::as_u64)
            .map(Some)
            .ok_or_else(|| PdfiumError::from("invalid bitmap buffer reference".to_owned()))
    }
}
pub(super) fn bitmap_to_rgba(
    s: &mut PdfiumSession,
    bitmap: u64,
) -> Result<Option<Rgba>, PdfiumError> {
    let w = scalar(s, "FPDFBitmap_GetWidth", bitmap)?;
    let h = scalar(s, "FPDFBitmap_GetHeight", bitmap)?;
    let stride = scalar(s, "FPDFBitmap_GetStride", bitmap)?;
    let format = scalar(s, "FPDFBitmap_GetFormat", bitmap)?;
    let buffer = pointer(&s.call("FPDFBitmap_GetBuffer", &[json!({"ref":bitmap})])?)?;
    if w <= 0.0 || h <= 0.0 || buffer.is_none() {
        return Ok(None);
    }
    if !w.is_finite()
        || !h.is_finite()
        || !stride.is_finite()
        || w.fract() != 0.0
        || h.fract() != 0.0
        || stride.fract() != 0.0
        || stride < 0.0
    {
        return Err(PdfiumError::from("invalid PDFium bitmap dimensions".to_owned()));
    }
    let (width, height, stride) = (w as usize, h as usize, stride as usize);
    let channels = match format as i32 {
        1 => 1,
        2 => 3,
        3 | 4 => 4,
        _ => return Ok(None),
    };
    let row = width
        .checked_mul(channels)
        .ok_or_else(|| PdfiumError::from("PDF bitmap row overflow".to_owned()))?;
    if stride < row {
        return Err(PdfiumError::from("PDF bitmap stride is shorter than a row".to_owned()));
    }
    let source_len = stride
        .checked_mul(height)
        .ok_or_else(|| PdfiumError::from("PDF bitmap size overflow".to_owned()))?;
    let out_len = width
        .checked_mul(height)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| PdfiumError::from("PDF bitmap size overflow".to_owned()))?;
    let source = s.read(buffer.unwrap(), 0, source_len)?;
    let mut pixels = vec![0; out_len];
    if source.len() != source_len {
        return Err(PdfiumError::from("PDFium returned a short bitmap buffer".to_owned()));
    }
    for y in 0..height {
        for x in 0..width {
            let src = y * stride + x * channels;
            let dst = (y * width + x) * 4;
            if channels == 1 {
                let v = source[src];
                pixels[dst..dst + 4].copy_from_slice(&[v, v, v, 255]);
            } else {
                pixels[dst..dst + 4].copy_from_slice(&[
                    source[src + 2],
                    source[src + 1],
                    source[src],
                    if format == 4.0 { source[src + 3] } else { 255 },
                ]);
            }
        }
    }
    Ok(Some(Rgba {
        pixels,
        width,
        height,
    }))
}
pub(super) fn encode_png(bitmap: &Rgba) -> Result<Vec<u8>, String> {
    let width = u32::try_from(bitmap.width).map_err(|_| "PNG width overflow")?;
    let height = u32::try_from(bitmap.height).map_err(|_| "PNG height overflow")?;
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(&bitmap.pixels, width, height, ExtendedColorType::Rgba8)
        .map_err(|e| e.to_string())?;
    Ok(out)
}
pub(super) fn rotate(bitmap: Rgba, turns: i32) -> Rgba {
    let t = turns.rem_euclid(4);
    if t == 0 {
        return bitmap;
    }
    let (w, h) = (bitmap.width, bitmap.height);
    let (ow, oh) = if t == 2 { (w, h) } else { (h, w) };
    let mut out = vec![0; bitmap.pixels.len()];
    for y in 0..h {
        for x in 0..w {
            let (ox, oy) = match t {
                1 => (h - 1 - y, x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let src = (y * w + x) * 4;
            let dst = (oy * ow + ox) * 4;
            out[dst..dst + 4].copy_from_slice(&bitmap.pixels[src..src + 4]);
        }
    }
    Rgba {
        pixels: out,
        width: ow,
        height: oh,
    }
}
pub(super) fn mirror(bitmap: Rgba, x: bool, y: bool) -> Rgba {
    if !x && !y {
        return bitmap;
    }
    let mut out = vec![0; bitmap.pixels.len()];
    for row in 0..bitmap.height {
        for col in 0..bitmap.width {
            let sy = if y { bitmap.height - 1 - row } else { row };
            let sx = if x { bitmap.width - 1 - col } else { col };
            let src = (sy * bitmap.width + sx) * 4;
            let dst = (row * bitmap.width + col) * 4;
            out[dst..dst + 4].copy_from_slice(&bitmap.pixels[src..src + 4]);
        }
    }
    Rgba {
        pixels: out,
        ..bitmap
    }
}
pub(super) fn crop(bitmap: &Rgba, left: f64, top: f64, right: f64, bottom: f64) -> Option<Rgba> {
    let clamp = |v: f64, max: usize| (jscompat::math_round(v).max(0.0).min(max as f64)) as usize;
    let x0 = clamp(left * bitmap.width as f64, bitmap.width);
    let x1 = clamp((1.0 - right) * bitmap.width as f64, bitmap.width);
    let y0 = clamp(top * bitmap.height as f64, bitmap.height);
    let y1 = clamp((1.0 - bottom) * bitmap.height as f64, bitmap.height);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let mut pixels = vec![0; w * h * 4];
    for y in 0..h {
        let src = ((y0 + y) * bitmap.width + x0) * 4;
        let dst = y * w * 4;
        pixels[dst..dst + w * 4].copy_from_slice(&bitmap.pixels[src..src + w * 4]);
    }
    Some(Rgba {
        pixels,
        width: w,
        height: h,
    })
}

pub(super) fn jpeg_sof_components(data: &[u8]) -> u8 {
    if data.len() < 4 || data[0] != 0xff || data[1] != 0xd8 {
        return 0;
    }
    let mut i = 2;
    while i + 3 < data.len() {
        if data[i] != 0xff {
            return 0;
        }
        let marker = data[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0xd8 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        if marker == 0xda || marker == 0xd9 {
            return 0;
        }
        let len = usize::from(data[i + 2]) * 256 + usize::from(data[i + 3]);
        if len < 2 {
            return 0;
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return data.get(i + 9).copied().unwrap_or(0);
        }
        i += 2 + len;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotate_mirror_and_crop_preserve_pixel_order() {
        let image = Rgba {
            pixels: vec![1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255],
            width: 2,
            height: 2,
        };
        let turned = rotate(image.clone(), 1);
        assert_eq!(
            turned
                .pixels
                .chunks_exact(4)
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            vec![3, 1, 4, 2]
        );
        let reflected = mirror(image.clone(), true, false);
        assert_eq!(
            reflected
                .pixels
                .chunks_exact(4)
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            vec![2, 1, 4, 3]
        );
        assert_eq!(
            crop(&image, 0.5, 0.0, 0.0, 0.5).unwrap().pixels,
            vec![2, 0, 0, 255]
        );
    }
    #[test]
    fn jpeg_components_skip_fill_and_non_frame_segments() {
        let mut jpeg = vec![
            0xff, 0xd8, 0xff, 0xff, 0xe0, 0, 4, 1, 2, 0xff, 0xd0, 0xff, 0xc0, 0, 8, 8, 0, 1, 0, 1,
            3,
        ];
        assert_eq!(jpeg_sof_components(&jpeg), 3);
        *jpeg.last_mut().unwrap() = 4;
        assert_eq!(jpeg_sof_components(&jpeg), 4);
        jpeg.truncate(jpeg.len() - 1);
        assert_eq!(jpeg_sof_components(&jpeg), 0);
        assert_eq!(jpeg_sof_components(&[0xff, 0xd8, 0xff, 0xda, 0, 2]), 0);
    }
}
