//! Measurements and coverage checks for embedded TrueType faces.
use super::layout::Measurer;
use std::cell::RefCell;
use std::collections::HashMap;

pub(super) fn usable(bytes: &[u8], text: &str) -> bool {
    main_core::pdf::font_locate::is_truetype(bytes)
        && main_core::pdf::font_cmap::font_covers_text(bytes, text)
}

pub(super) struct FaceMetrics<'a> {
    bytes: &'a [u8],
    units: u16,
    metrics: u16,
    hmtx: usize,
    cmap: Option<(usize, u16)>,
    cache: RefCell<HashMap<u32, f64>>,
    ascent: f64,
    descent: f64,
}
impl<'a> FaceMetrics<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Result<Self, String> {
        let mut face = Self {
            bytes,
            units: 1000,
            metrics: 0,
            hmtx: 0,
            cmap: None,
            cache: RefCell::new(HashMap::new()),
            ascent: 0.6,
            descent: 0.1,
        };
        let head = face.table(b"head").ok_or("font lacks head/hhea/hmtx")?;
        let hhea = face.table(b"hhea").ok_or("font lacks head/hhea/hmtx")?;
        face.hmtx = face.table(b"hmtx").ok_or("font lacks head/hhea/hmtx")?;
        face.units = match face.u16(head + 18) {
            0 => 1000,
            n => n,
        };
        face.metrics = face.u16(hhea + 34);
        let os2 = face.table(b"OS/2");
        let (asc, desc) = if let Some(os2) = os2 {
            (face.i16(os2 + 68)?, face.i16(os2 + 70)?)
        } else {
            (face.i16(hhea + 4)?, face.i16(hhea + 6)?)
        };
        face.ascent = (f64::from(asc) / f64::from(face.units)).clamp(0.6, 1.1);
        face.descent = (-f64::from(desc) / f64::from(face.units)).clamp(0.1, 0.4);
        face.cmap = face.cmap_subtable();
        Ok(face)
    }
    fn u16(&self, at: usize) -> u16 {
        self.bytes
            .get(at..at.saturating_add(2))
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes)
            .unwrap_or(0)
    }
    fn i16(&self, at: usize) -> Result<i16, String> {
        self.bytes
            .get(at..at.saturating_add(2))
            .and_then(|b| b.try_into().ok())
            .map(i16::from_be_bytes)
            .ok_or_else(|| "font metrics are truncated".into())
    }
    fn u32(&self, at: usize) -> u32 {
        self.bytes
            .get(at..at.saturating_add(4))
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0)
    }
    fn table(&self, tag: &[u8; 4]) -> Option<usize> {
        for i in 0..usize::from(self.u16(4)) {
            let at = 12 + i * 16;
            if self.bytes.get(at..at + 4) == Some(tag.as_slice()) {
                return usize::try_from(self.u32(at + 8)).ok();
            }
        }
        None
    }
    fn cmap_subtable(&self) -> Option<(usize, u16)> {
        let cmap = self.table(b"cmap")?;
        let mut best = None;
        for i in 0..usize::from(self.u16(cmap + 2)) {
            let rec = cmap + 4 + i * 8;
            let (platform, encoding) = (self.u16(rec), self.u16(rec + 2));
            if platform != 0 && !(platform == 3 && matches!(encoding, 1 | 10)) {
                continue;
            }
            let offset = cmap.checked_add(usize::try_from(self.u32(rec + 4)).ok()?)?;
            let format = self.u16(offset);
            if format == 12 {
                return Some((offset, format));
            }
            if format == 4 && best.is_none() {
                best = Some((offset, format));
            }
        }
        best
    }
    fn glyph(&self, cp: u32) -> usize {
        let Some((offset, format)) = self.cmap else {
            return 0;
        };
        if format == 12 {
            for i in 0..usize::try_from(self.u32(offset + 12)).unwrap_or(0) {
                let group = offset
                    .saturating_add(16)
                    .saturating_add(i.saturating_mul(12));
                let start = self.u32(group);
                if cp < start {
                    return 0;
                }
                if cp <= self.u32(group + 4) {
                    return usize::try_from(self.u32(group + 8).saturating_add(cp - start))
                        .unwrap_or(0);
                }
            }
            return 0;
        }
        if cp > 0xffff {
            return 0;
        }
        let seg_x2 = usize::from(self.u16(offset + 6));
        let ends = offset + 14;
        let starts = ends + seg_x2 + 2;
        let deltas = starts + seg_x2;
        let range_offsets = deltas + seg_x2;
        for segment in (0..seg_x2).step_by(2) {
            if cp > u32::from(self.u16(ends + segment)) {
                continue;
            }
            let start = u32::from(self.u16(starts + segment));
            if cp < start {
                return 0;
            }
            let Ok(delta) = self.i16(deltas + segment) else {
                return 0;
            };
            let range = usize::from(self.u16(range_offsets + segment));
            let base = if range == 0 {
                cp
            } else {
                let index =
                    range_offsets + segment + range + usize::try_from(cp - start).unwrap_or(0) * 2;
                let glyph = u32::from(self.u16(index));
                if glyph == 0 {
                    return 0;
                }
                glyph
            };
            return usize::from((i64::from(base) + i64::from(delta)).rem_euclid(65536) as u16);
        }
        0
    }
    fn advance(&self, cp: u32) -> f64 {
        if let Some(width) = self.cache.borrow().get(&cp) {
            return *width;
        }
        let glyph = self.glyph(cp);
        let index = glyph.min(usize::from(self.metrics.saturating_sub(1)));
        let width = f64::from(self.u16(self.hmtx + index * 4)) / f64::from(self.units);
        self.cache.borrow_mut().insert(cp, width);
        width
    }
}
impl Measurer for FaceMetrics<'_> {
    fn measure(&self, text: &str) -> f64 {
        text.chars().map(|ch| self.advance(u32::from(ch))).sum()
    }
    fn ascent(&self) -> f64 {
        self.ascent
    }
    fn descent(&self) -> f64 {
        self.descent
    }
}
