//! Crop-origin and display-rotation normalization for extracted page geometry.
use super::geometry::Rect;
use super::ir::{ImageBlock, PdfChar, RawPath};
use super::paths::{number, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use std::f64::consts::PI;

pub(super) fn origin(s: &mut PdfiumSession, page: u64) -> Result<(f64, f64), PdfiumError> {
    let scratch = s.alloc(16)?;
    let result = (|| {
        for name in ["FPDFPage_GetCropBox", "FPDFPage_GetMediaBox"] {
            if number(
                s,
                name,
                &[
                    ptr(page, 0),
                    ptr(scratch, 0),
                    ptr(scratch, 4),
                    ptr(scratch, 8),
                    ptr(scratch, 12),
                ],
            )? != 0.0
            {
                let b = s.read(scratch, 0, 16)?;
                let n =
                    |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().expect("page box")) as f64;
                return Ok((-n(0).min(n(8)), -n(4).min(n(12))));
            }
        }
        Ok((0.0, 0.0))
    })();
    let free = s.free(scratch);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
fn point(x: f64, y: f64, rotation: i32, width: f64, height: f64) -> (f64, f64) {
    let (w, h) = if rotation % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    match rotation {
        1 => (y, w - x),
        2 => (w - x, h - y),
        3 => (h - y, x),
        _ => (x, y),
    }
}
pub(super) fn transform_rect(
    r: &mut Rect,
    dx: f64,
    dy: f64,
    rotation: i32,
    width: f64,
    height: f64,
) {
    let (ax, ay) = point(r.x0 + dx, r.y0 + dy, rotation, width, height);
    let (bx, by) = point(r.x1 + dx, r.y1 + dy, rotation, width, height);
    *r = Rect {
        x0: ax.min(bx),
        x1: ax.max(bx),
        y0: ay.min(by),
        y1: ay.max(by),
    };
}
#[allow(clippy::too_many_arguments)]
pub(super) fn normalize(
    chars: &mut [PdfChar],
    images: &mut [ImageBlock],
    paths: &mut [RawPath],
    mut content: Option<&mut Rect>,
    dx: f64,
    dy: f64,
    rotation: i32,
    width: f64,
    height: f64,
) {
    let rotation = if (1..=3).contains(&rotation) {
        rotation
    } else {
        0
    };
    for c in chars {
        transform_rect(&mut c.bounds, dx, dy, rotation, width, height);
        transform_rect(&mut c.loose_box, dx, dy, rotation, width, height);
        (c.origin_x, c.origin_y) = point(c.origin_x + dx, c.origin_y + dy, rotation, width, height);
        let a = c.angle + rotation as f64 * (PI / 2.0);
        c.angle = a - 2.0 * PI * jscompat::math_round(a / (2.0 * PI));
    }
    for img in images {
        transform_rect(&mut img.bounds, dx, dy, rotation, width, height);
    }
    if let Some(r) = content.as_mut() {
        transform_rect(r, dx, dy, rotation, width, height);
    }
    for path in paths {
        if let Some(r) = &mut path.clip_box {
            transform_rect(r, dx, dy, rotation, width, height);
        }
        for sub in &mut path.subpaths {
            for p in &mut sub.points {
                *p = point(p.0 + dx, p.1 + dy, rotation, width, height);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_and_quarter_turn_use_display_dimensions() {
        let mut r = Rect {
            x0: 10.0,
            y0: 20.0,
            x1: 30.0,
            y1: 50.0,
        };
        transform_rect(&mut r, -10.0, -20.0, 1, 200.0, 100.0);
        assert_eq!(
            r,
            Rect {
                x0: 0.0,
                y0: 80.0,
                x1: 30.0,
                y1: 100.0
            }
        );
    }
}
