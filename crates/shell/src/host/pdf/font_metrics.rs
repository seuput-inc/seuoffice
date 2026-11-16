//! Installed-family checks and unkerned advances used by PDF-to-DOCX fitting.
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use main_core::packages::font_metrics::sfnt::FontScan;
use main_core::pdf::font_locate::{FaceRef, FontEnv, Platform, norm, style_score};

struct ParsedFace {
    units: u16,
    advances: Vec<u16>,
    cmap: Vec<u8>,
    format: u16,
}

type CacheEntry = ((String, i64), Option<Arc<ParsedFace>>);

struct State {
    scan: FontScan,
    cache: VecDeque<CacheEntry>,
}

pub(crate) struct FontMetrics(Mutex<State>);

impl FontMetrics {
    pub(crate) fn new() -> Self {
        let platform = if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Other
        };
        let home = dirs::home_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Self(Mutex::new(State {
            scan: FontScan::new(FontEnv { platform, home }),
            cache: VecDeque::new(),
        }))
    }

    pub(crate) fn is_family_installed(&self, family: &str) -> bool {
        if norm(family).is_empty() {
            return false;
        }
        self.0
            .lock()
            .expect("font metrics")
            .scan
            .font_index()
            .family_faces(family)
            .next()
            .is_some()
    }

    /// Total unkerned isolated-codepoint width in points; unmeasurable text returns None.
    pub(crate) fn advance_widths(
        &self,
        text: &str,
        family: &str,
        size_pt: f64,
        bold: bool,
        italic: bool,
    ) -> Option<f64> {
        self.face(family, bold, italic)?.width_pt(text, size_pt)
    }

    fn face(&self, family: &str, bold: bool, italic: bool) -> Option<Arc<ParsedFace>> {
        let mut state = self.0.lock().expect("font metrics");
        let mut want = Vec::new();
        if bold {
            want.push("bold".to_owned());
        }
        if italic {
            want.push("italic".to_owned());
        }
        let mut best: Option<(&FaceRef, i64)> = None;
        for face in state.scan.font_index().family_faces(family) {
            let score = style_score(face, &want);
            if best.is_none_or(|(_, previous)| score > previous) {
                best = Some((face, score));
            }
        }
        let face = best?.0.clone();
        let key = (face.path.clone(), face.offset);
        if let Some((_, value)) = state.cache.iter().find(|(candidate, _)| *candidate == key) {
            value.clone()
        } else {
            let parsed = match parse_face(&face) {
                Ok(parsed) => parsed.map(Arc::new),
                Err(_) => return None,
            };
            if state.cache.len() >= 8 {
                state.cache.pop_front();
            }
            state.cache.push_back((key, parsed.clone()));
            parsed
        }
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn read_at(file: &mut File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn table(
    file: &mut File,
    tables: &HashMap<[u8; 4], (u32, u32)>,
    tag: &[u8; 4],
    max: u32,
) -> io::Result<Option<Vec<u8>>> {
    let Some(&(offset, length)) = tables.get(tag) else {
        return Ok(None);
    };
    if length < 2 || length > max {
        return Ok(None);
    }
    read_at(file, u64::from(offset), length as usize).map(Some)
}

fn parse_face(face: &FaceRef) -> io::Result<Option<ParsedFace>> {
    let mut file = File::open(&face.path)?;
    let offset = u64::try_from(face.offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative face offset"))?;
    let head = read_at(&mut file, offset, 12)?;
    if !matches!(
        u32_at(&head, 0),
        Some(0x0001_0000 | 0x7472_7565 | 0x4f54_544f)
    ) {
        return Ok(None);
    }
    let count = usize::from(u16_at(&head, 4).unwrap_or(0));
    if count == 0 || count > 64 {
        return Ok(None);
    }
    let directory = read_at(&mut file, offset + 12, 16 * count)?;
    let mut tables = HashMap::new();
    for entry in directory.chunks_exact(16) {
        let tag: [u8; 4] = entry[..4].try_into().expect("fixed table tag");
        tables.insert(
            tag,
            (
                u32_at(entry, 8).expect("fixed table offset"),
                u32_at(entry, 12).expect("fixed table length"),
            ),
        );
    }
    let Some(head) = table(&mut file, &tables, b"head", 512)? else {
        return Ok(None);
    };
    let Some(hhea) = table(&mut file, &tables, b"hhea", 512)? else {
        return Ok(None);
    };
    let Some(units) = u16_at(&head, 18).filter(|n| *n != 0) else {
        return Ok(None);
    };
    let Some(count) = u16_at(&hhea, 34).filter(|n| *n != 0) else {
        return Ok(None);
    };
    let Some(hmtx) = table(&mut file, &tables, b"hmtx", 1 << 19)? else {
        return Ok(None);
    };
    if hmtx.len() < 4 * usize::from(count) {
        return Ok(None);
    }
    let Some(cmap) = table(&mut file, &tables, b"cmap", 1 << 20)? else {
        return Ok(None);
    };
    let Some((format, offset)) = select_cmap(&cmap) else {
        return Ok(None);
    };
    let advances = (0..usize::from(count))
        .map(|i| u16_at(&hmtx, 4 * i).expect("checked hmtx size"))
        .collect();
    Ok(Some(ParsedFace {
        units,
        advances,
        cmap: cmap[offset..].to_vec(),
        format,
    }))
}

fn select_cmap(cmap: &[u8]) -> Option<(u16, usize)> {
    let count = usize::from(u16_at(cmap, 2)?);
    let mut best: Option<(u8, usize)> = None;
    for i in 0..count {
        let pos = 4 + 8 * i;
        let (Some(platform), Some(encoding), Some(offset)) = (
            u16_at(cmap, pos),
            u16_at(cmap, pos + 2),
            u32_at(cmap, pos + 4),
        ) else {
            break;
        };
        let rank = match (platform, encoding) {
            (3, 10) => 4,
            (0, 4..) => 3,
            (3, 1) => 2,
            (0, _) => 1,
            _ => 0,
        };
        if rank > 0 && best.is_none_or(|(old, _)| rank > old) {
            best = Some((rank, offset as usize));
        }
    }
    let (_, offset) = best?;
    if cmap.len().checked_sub(offset)? < 4 {
        return None;
    }
    let format = u16_at(cmap, offset)?;
    matches!(format, 4 | 12).then_some((format, offset))
}

impl ParsedFace {
    fn width_pt(&self, text: &str, size_pt: f64) -> Option<f64> {
        let scale = size_pt * 20.0 / f64::from(self.units);
        let mut total = 0.0;
        for character in text.chars() {
            total += self.advance(character as u32, scale);
        }
        if total.is_nan() {
            None
        } else {
            Some(total / 20.0)
        }
    }

    fn advance(&self, cp: u32, scale: f64) -> f64 {
        let glyph = self.glyph(cp).unwrap_or(0);
        if glyph == 0 && cp != 0 {
            return f64::NAN;
        }
        let index = (glyph as usize).min(self.advances.len() - 1);
        f64::from(self.advances[index]) * scale
    }

    fn glyph(&self, cp: u32) -> Option<u32> {
        let sub = &self.cmap;
        if self.format == 12 {
            let mut lo = 0usize;
            let mut hi = usize::try_from(u32_at(sub, 12)?).ok()?;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let pos = 16usize.checked_add(mid.checked_mul(12)?)?;
                let (start, end, first) = (
                    u32_at(sub, pos)?,
                    u32_at(sub, pos + 4)?,
                    u32_at(sub, pos + 8)?,
                );
                if cp < start {
                    hi = mid;
                } else if cp > end {
                    lo = mid + 1;
                } else {
                    return Some(first.saturating_add(cp - start));
                }
            }
            return Some(0);
        }
        if cp > 0xffff {
            return Some(0);
        }
        let count_x2 = usize::from(u16_at(sub, 6)?);
        let end = 14usize;
        let start = end.checked_add(count_x2)?.checked_add(2)?;
        let delta = start.checked_add(count_x2)?;
        let range = delta.checked_add(count_x2)?;
        let mut lo = 0usize;
        let mut hi = count_x2 / 2;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let step = mid * 2;
            let end_cp = u32::from(u16_at(sub, end + step)?);
            if cp > end_cp {
                lo = mid + 1;
                continue;
            }
            let start_cp = u32::from(u16_at(sub, start + step)?);
            if cp < start_cp {
                hi = mid;
                continue;
            }
            let d = u32::from(u16_at(sub, delta + step)?);
            let r = usize::from(u16_at(sub, range + step)?);
            if r == 0 {
                return Some((cp + d) & 0xffff);
            }
            let pos = range
                .checked_add(step)?
                .checked_add(r)?
                .checked_add(2 * usize::try_from(cp - start_cp).ok()?)?;
            let glyph = u32::from(u16_at(sub, pos)?);
            return Some(if glyph == 0 { 0 } else { (glyph + d) & 0xffff });
        }
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmap_prefers_full_unicode_even_when_bmp_is_first() {
        let mut cmap = vec![0; 44];
        cmap[2..4].copy_from_slice(&2u16.to_be_bytes());
        cmap[4..6].copy_from_slice(&3u16.to_be_bytes());
        cmap[6..8].copy_from_slice(&1u16.to_be_bytes());
        cmap[8..12].copy_from_slice(&20u32.to_be_bytes());
        cmap[12..14].copy_from_slice(&3u16.to_be_bytes());
        cmap[14..16].copy_from_slice(&10u16.to_be_bytes());
        cmap[16..20].copy_from_slice(&24u32.to_be_bytes());
        cmap[20..22].copy_from_slice(&4u16.to_be_bytes());
        cmap[24..26].copy_from_slice(&12u16.to_be_bytes());
        assert_eq!(select_cmap(&cmap), Some((12, 24)));
    }

    #[test]
    fn format_twelve_maps_astral_codepoints_and_missing_glyphs() {
        let mut cmap = vec![0; 28];
        cmap[12..16].copy_from_slice(&1u32.to_be_bytes());
        cmap[16..20].copy_from_slice(&0x1f600u32.to_be_bytes());
        cmap[20..24].copy_from_slice(&0x1f602u32.to_be_bytes());
        cmap[24..28].copy_from_slice(&42u32.to_be_bytes());
        let face = ParsedFace {
            units: 1000,
            advances: vec![500],
            cmap,
            format: 12,
        };
        assert_eq!(face.glyph(0x1f601), Some(43));
        assert_eq!(face.glyph(0x41), Some(0));
        assert_eq!(face.width_pt("😀😁", 12.0), Some(12.0));
        assert_eq!(face.width_pt("😀A", 12.0), None);
    }

    #[test]
    fn malformed_cmap_does_not_map_or_panic() {
        let face = ParsedFace {
            units: 1000,
            advances: vec![500],
            cmap: vec![0; 8],
            format: 12,
        };
        assert_eq!(face.glyph(0x41), None);
    }
}
