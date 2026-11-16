//! Text and image quality verdicts for PDF page extraction.
use super::geometry::Rect;
use super::ir::{ImageBlock, PdfChar};
use super::matrix::intersection;

pub(super) fn whitespace(code: u32) -> bool {
    matches!(code, 0x20 | 0xa0 | 0x0d | 0x0a | 0x09)
}
pub(super) fn control(code: u32) -> bool {
    (code < 0x20 && !matches!(code, 0x09 | 0x0a | 0x0d)) || (0x7f..=0x9f).contains(&code)
}
pub(super) fn hyphen_marker(c: &PdfChar) -> bool {
    c.is_hyphen && c.code == 2
}
fn bad(code: u32) -> bool {
    matches!(code, 0xfffd | 0xfffe) || (0xe000..=0xf8ff).contains(&code) || control(code)
}
fn mojibake(chars: &[&PdfChar]) -> bool {
    if chars.len() < 40 {
        return false;
    }
    let symbols = chars
        .iter()
        .filter(|c| (0xa0..=0xbf).contains(&c.code))
        .count();
    let accents = chars
        .iter()
        .filter(|c| (0xc0..=0xff).contains(&c.code))
        .count();
    symbols as f64 / chars.len() as f64 >= 0.08 && accents as f64 / chars.len() as f64 >= 0.15
}
pub(super) struct Quality {
    pub degraded: bool,
    pub reason: Option<&'static str>,
    pub scanned: bool,
    pub bad_unicode_ratio: f64,
}
pub(super) fn assess(chars: &[PdfChar], images: &[ImageBlock], page: Rect) -> Quality {
    let text: Vec<_> = chars
        .iter()
        .filter(|c| !c.is_generated && !whitespace(c.code))
        .collect();
    let ratio = if text.len() >= 10 {
        text.iter()
            .filter(|c| bad(c.code) && !hyphen_marker(c))
            .count() as f64
            / text.len() as f64
    } else {
        0.0
    };
    let area = (page.x1 - page.x0).max(0.0) * (page.y1 - page.y0).max(0.0);
    if text.len() <= 2
        && area > 0.0
        && images.iter().any(|i| {
            !i.synthetic
                && intersection(i.bounds, page)
                    .is_some_and(|r| (r.x1 - r.x0) * (r.y1 - r.y0) / area > 0.8)
        })
    {
        return Quality {
            degraded: false,
            reason: None,
            scanned: true,
            bad_unicode_ratio: ratio,
        };
    }
    let reason = if text.len() < 10 {
        None
    } else if ratio > 0.15 || mojibake(&text) {
        Some("bad-tounicode")
    } else if text.iter().filter(|c| c.angle.abs() > 0.26).count() as f64 / text.len() as f64 > 0.3
    {
        Some("vertical-text")
    } else {
        None
    };
    Quality {
        degraded: reason.is_some(),
        reason,
        scanned: false,
        bad_unicode_ratio: ratio,
    }
}
