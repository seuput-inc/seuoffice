//! Confirm tiny suspect glyphs against the rendered source page.
use super::geometry::Rect;
use super::ir::PdfChar;
use super::render;
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
fn luminance(color: &str) -> f64 {
    let v = u32::from_str_radix(color, 16).unwrap_or(0);
    (0.2126 * ((v >> 16) & 255) as f64
        + 0.7152 * ((v >> 8) & 255) as f64
        + 0.0722 * (v & 255) as f64)
        / 255.0
}
pub(super) fn mark(
    s: &mut PdfiumSession,
    page: u64,
    chars: &mut [PdfChar],
) -> Result<(), PdfiumError> {
    let mut sizes: Vec<_> = chars
        .iter()
        .filter(|c| c.code > 0x20 && c.font_size > 0.0)
        .map(|c| c.font_size)
        .collect();
    if sizes.is_empty() {
        return Ok(());
    }
    sizes.sort_by(f64::total_cmp);
    let median = sizes[sizes.len() / 2];
    let mut candidates: Vec<_> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.code > 0x20
                && c.font_size > 0.0
                && c.font_size <= 0.7 * median
                && luminance(&c.color) <= 0.5
                && c.bounds.width() > 0.0
                && c.bounds.height() > 0.0
        })
        .map(|(i, _)| i)
        .collect();
    candidates.sort_by(|&a, &b| {
        chars[b]
            .origin_y
            .total_cmp(&chars[a].origin_y)
            .then_with(|| chars[a].bounds.x0.total_cmp(&chars[b].bounds.x0))
    });
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for i in candidates {
        if let Some(cluster) = clusters.last_mut() {
            let tail = &chars[*cluster.last().expect("nonempty glyph cluster")];
            if (tail.origin_y - chars[i].origin_y).abs() <= 2.0
                && chars[i].bounds.x0 - tail.bounds.x1 <= 24.0
            {
                cluster.push(i);
                continue;
            }
        }
        clusters.push(vec![i]);
    }
    for group in clusters.iter().take(12) {
        let b = group
            .iter()
            .map(|&i| chars[i].bounds)
            .reduce(Rect::union)
            .expect("nonempty glyph cluster");
        let region = Rect {
            x0: b.x0 - 1.0,
            y0: b.y0 - 1.0,
            x1: b.x1 + 1.0,
            y1: b.y1 + 1.0,
        };
        let Some(png) = render::page(s, page, Some(region), 3.0)? else {
            continue;
        };
        let img = image::load_from_memory(&png.data)
            .map_err(|e| PdfiumError::from(e.to_string()))?
            .to_rgba8();
        let dark = img
            .pixels()
            .any(|p| p[0] <= 120 && p[1] <= 120 && p[2] <= 120);
        if !dark {
            for &i in group {
                chars[i].invisible = true;
            }
        }
    }
    Ok(())
}
