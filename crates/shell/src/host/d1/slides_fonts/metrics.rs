use std::sync::Arc;

use ttf_parser::{Face, GlyphId};
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_segmentation::UnicodeSegmentation;

/// A single face. Bytes are owned by the request-scoped registry, so a deck's
/// embedded faces cannot outlive or contaminate another deck generation.
#[derive(Clone)]
pub(super) struct FontFace {
    pub(super) bytes: Arc<[u8]>,
    pub(super) index: u32,
    pub(super) family: String,
    pub(super) weight_range: Option<String>,
    pub(super) private: bool,
    pub(super) substituted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Metrics {
    pub(super) ascent: f64,
    pub(super) descent: f64,
    pub(super) line_height: f64,
    pub(super) external_leading: Option<f64>,
}

impl FontFace {
    pub(super) fn parse(&self) -> Result<Face<'_>, String> {
        Face::parse(&self.bytes, self.index)
            .map_err(|error| format!("invalid font face: {error:?}"))
    }

    pub(super) fn metrics(&self, size: f64) -> Result<Metrics, String> {
        let face = self.parse()?;
        let em = f64::from(face.units_per_em());
        if em == 0.0 {
            return Err("font has zero units per em".into());
        }
        // opentype.js exposes hhea ascender/descender, not ttf-parser's
        // policy-dependent ascender() and descender().
        let hhea = face.tables().hhea;
        let (ascender, descender, gap) = if self.family.to_lowercase().starts_with("carlito") {
            let os2 = ttf_parser::RawFace::parse(&self.bytes, self.index)
                .ok()
                .and_then(|raw| raw.table(ttf_parser::Tag::from_bytes(b"OS/2")));
            match os2.and_then(|table| {
                Some((
                    u16::from_be_bytes(table.get(74..76)?.try_into().ok()?),
                    u16::from_be_bytes(table.get(76..78)?.try_into().ok()?),
                ))
            }) {
                Some((ascender, descender)) if ascender != 0 => {
                    (f64::from(ascender), f64::from(descender), 0.0)
                }
                _ => (
                    f64::from(hhea.ascender),
                    f64::from(hhea.descender).abs(),
                    f64::from(hhea.line_gap.max(0)),
                ),
            }
        } else {
            (
                f64::from(hhea.ascender),
                f64::from(hhea.descender).abs(),
                f64::from(hhea.line_gap.max(0)),
            )
        };
        let scale = size / em;
        let ascent = ascender * scale;
        let descent = descender * scale;
        let external = gap * scale;
        Ok(Metrics {
            ascent,
            descent,
            line_height: ascent + descent,
            external_leading: (external > 0.0).then_some(external),
        })
    }
    pub(super) fn contains(&self, ch: char) -> bool {
        self.parse()
            .ok()
            .and_then(|face| face.glyph_index(ch))
            .is_some_and(|id| id.0 != 0)
    }

    /// HarfBuzz-compatible shaping for the four complex scripts. The caller
    /// separately applies the original renderer's ground-truth refinement.
    pub(super) fn shaped_units(&self, text: &str, require_coverage: bool) -> Option<(i64, i32)> {
        let face = rustybuzz::Face::from_slice(&self.bytes, self.index)?;
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        let shaped = rustybuzz::shape(&face, &[], buffer);
        if require_coverage && shaped.glyph_infos().iter().any(|glyph| glyph.glyph_id == 0) {
            return None;
        }
        Some((
            shaped
                .glyph_positions()
                .iter()
                .map(|position| i64::from(position.x_advance))
                .sum(),
            face.units_per_em(),
        ))
    }

    /// Unshaped glyph metrics used by opentype.js's safe-advance path. A
    /// supplied kerning callback must implement its GPOS/kern lookup policy.
    pub(super) fn glyph_advance(
        &self,
        text: &str,
        size: f64,
        bold: bool,
        kerning: bool,
        pair: impl Fn(&Face<'_>, GlyphId, GlyphId) -> i16,
    ) -> Result<Option<f64>, String> {
        let mut face = self.parse()?;
        let variable = face.tables().hvar.is_some()
            && face
                .variation_axes()
                .into_iter()
                .any(|axis| axis.tag == ttf_parser::Tag::from_bytes(b"wght"));
        if variable {
            let _ = face.set_variation(
                ttf_parser::Tag::from_bytes(b"wght"),
                if bold { 700.0 } else { 400.0 },
            );
        }
        let mut units = 0f64;
        let mut previous = None;
        for ch in text.chars() {
            let Some(glyph) = face.glyph_index(ch).filter(|glyph| glyph.0 != 0) else {
                return Ok(None);
            };
            units += f64::from(face.glyph_hor_advance(glyph).unwrap_or(0));
            if !variable
                && kerning
                && let Some(prev) = previous
            {
                    units += f64::from(pair(&face, prev, glyph));
                }
            previous = Some(glyph);
        }
        Ok(Some(units / f64::from(face.units_per_em()) * size))
    }
}

pub(super) fn heuristic_metrics(size: f64) -> Metrics {
    Metrics {
        ascent: size * 0.8,
        descent: size * 0.2,
        line_height: size * 1.2,
        external_leading: None,
    }
}

pub(super) fn heuristic_width(text: &str, size: f64, bold: bool) -> f64 {
    fn joined(ch: char) -> bool {
        matches!(ch as u32, 0x200d | 0xfe0f | 0x1f1e6..=0x1f1ff | 0x1f3fb..=0x1f3ff)
    }
    fn advance(ch: char) -> f64 {
        let code = ch as u32;
        if matches!(code, 0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xa000..=0xa4cf | 0xa960..=0xa97f | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1b000..=0x1b2ff | 0x20000..=0x3fffd | 0x2600..=0x27bf | 0x25a0..=0x25ff | 0x203b | 0x2460..=0x24ff)
            || code >= 0x1f000
        {
            1.0
        } else if char::from_u32(code & 0xffff).is_some_and(|value| "iIlj.,:;'!|".contains(value)) {
            0.28
        } else if char::from_u32(code & 0xffff).is_some_and(|value| " ftr".contains(value)) {
            0.32
        } else if char::from_u32(code & 0xffff).is_some_and(|value| "mwMW".contains(value)) {
            0.82
        } else {
            0.52
        }
    }
    let mut em = 0.0;
    for cluster in text.graphemes(true) {
        if cluster.encode_utf16().count() > 1 && cluster.chars().any(joined) {
            em += 1.0;
            continue;
        }
        for ch in cluster.chars() {
            if !matches!(
                get_general_category(ch),
                GeneralCategory::NonspacingMark
                    | GeneralCategory::EnclosingMark
                    | GeneralCategory::Format
            ) {
                em += advance(ch);
            }
        }
    }
    em * size * if bold { 1.04 } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_carlito_metrics_coverage_and_kerning_toggle() {
        let bytes: Arc<[u8]> = Arc::from(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packages/ui/src/fonts/Carlito-Regular.ttf"
            ))
            .as_slice(),
        );
        let face = FontFace {
            bytes,
            index: 0,
            family: "Carlito".into(),
            private: false,
            substituted: false,
            weight_range: None,
        };
        let metrics = face.metrics(24.0).unwrap();
        assert!(metrics.ascent > 0.0 && metrics.descent > 0.0);
        assert!(face.contains('A'));
        assert!(
            face.glyph_advance("A", 24.0, false, true, |_, _, _| 0)
                .unwrap()
                .unwrap()
                > 0.0
        );
    }
    #[test]
    fn fallback_distinguishes_combining_spacing_and_emoji_clusters() {
        assert_eq!(
            heuristic_width("A\u{301}", 10.0, false),
            heuristic_width("A", 10.0, false)
        );
        assert!(heuristic_width("क\u{93e}", 10.0, false) > heuristic_width("क", 10.0, false));
        assert_eq!(heuristic_width("👩‍💻", 10.0, false), 10.0);
    }
}
