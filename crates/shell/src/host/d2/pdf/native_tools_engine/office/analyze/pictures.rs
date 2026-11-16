//! Encode background panels and empty frames into real transparent PNG images.
use super::super::bitmap::{Rgba, encode_png};
use super::super::geometry::Rect;
use super::super::ir::{FloatPlacement, ImageBlock};
fn rgb(color: &str) -> [u8; 3] {
    let mut out = [0; 3];
    if color.len() >= 6 && color.is_ascii() {
        for (i, v) in out.iter_mut().enumerate() {
            *v = u8::from_str_radix(&color[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
    }
    out
}
fn image(
    bounds: Rect,
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    z: Option<i64>,
) -> Result<ImageBlock, String> {
    let data = encode_png(&Rgba {
        pixels,
        width,
        height,
    })?;
    Ok(ImageBlock {
        bounds,
        data,
        mime: "image/png".into(),
        float: Some(FloatPlacement {
            wrap: "behind".into(),
        }),
        spacing_before_pt: None,
        z,
        synthetic: false,
        card_id: None,
    })
}
pub(super) fn solid(
    bounds: Rect,
    color: &str,
    alpha: u8,
    z: Option<i64>,
) -> Result<ImageBlock, String> {
    let [r, g, b] = rgb(color);
    image(
        bounds,
        vec![
            r, g, b, alpha, r, g, b, alpha, r, g, b, alpha, r, g, b, alpha,
        ],
        2,
        2,
        z,
    )
}
pub(super) fn frame(bounds: Rect, color: &str, width_pt: f64) -> Result<ImageBlock, String> {
    let x = if bounds.width().is_finite() {
        bounds.width().max(0.0)
    } else {
        0.0
    };
    let y = if bounds.height().is_finite() {
        bounds.height().max(0.0)
    } else {
        0.0
    };
    let shrink = 1.0_f64.max(x * 2.0 / 2048.0).max(y * 2.0 / 2048.0);
    let w = ((x * 2.0 / shrink).round() as usize).max(2);
    let h = ((y * 2.0 / shrink).round() as usize).max(2);
    let bw = if width_pt.is_finite() {
        (width_pt * 2.0).round().max(1.0) as usize
    } else {
        1
    };
    let [r, g, b] = rgb(color);
    let mut pixels = vec![0; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            if x < bw || x >= w.saturating_sub(bw) || y < bw || y >= h.saturating_sub(bw) {
                let offset = (y * w + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    image(bounds, pixels, w, h, None)
}
