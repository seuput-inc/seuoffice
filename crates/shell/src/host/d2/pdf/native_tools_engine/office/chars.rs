//! PDF glyph calibration and duplicate-ink cleanup.
use super::geometry::median;
use super::ir::PdfChar;
use std::collections::HashMap;

pub(super) fn calibrate_font_sizes(chars: &mut [PdfChar]) {
    let key = |c: &PdfChar| format!("{}|{:.1}", c.font_family, c.font_size);
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();
    for c in chars.iter() {
        if c.font_size <= 0.0 || jscompat::trim(&c.text).is_empty() {
            continue;
        }
        let h = c.loose_box.height();
        if h > 0.0 {
            groups.entry(key(c)).or_default().push(h);
        }
    }
    let mut corrected = HashMap::new();
    for (k, heights) in groups {
        let declared = k
            .rsplit_once('|')
            .and_then(|(_, s)| s.parse::<f64>().ok())
            .unwrap_or(f64::NAN);
        let effective = median(&heights);
        let ratio = effective / declared;
        if (0.3..=0.9).contains(&ratio) {
            corrected.insert(k, effective);
        }
    }
    for c in chars {
        if let Some(&size) = corrected.get(&key(c)) {
            c.font_size = size;
        }
    }
}
fn ligature(ia: usize, ib: usize, a: &PdfChar, b: &PdfChar) -> bool {
    ia.abs_diff(ib) == 1
        && !(a.text_object_id.is_some()
            && b.text_object_id.is_some()
            && a.text_object_id != b.text_object_id)
        && (a.bounds.x0 - b.bounds.x0).abs() <= 0.05
        && (a.bounds.x1 - b.bounds.x1).abs() <= 0.05
        && (a.bounds.y0 - b.bounds.y0).abs() <= 0.05
        && (a.bounds.y1 - b.bounds.y1).abs() <= 0.05
}
pub(super) fn dedupe_double_drawn(chars: &mut Vec<PdfChar>) {
    let mut codes: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, c) in chars.iter().enumerate() {
        if !c.is_generated && !jscompat::trim(&c.text).is_empty() {
            codes.entry(c.code).or_default().push(i);
        }
    }
    let mut dropped = vec![false; chars.len()];
    for indices in codes.values().filter(|v| v.len() > 1) {
        let cell = 0.3
            * indices
                .iter()
                .map(|&i| chars[i].font_size.max(1.0))
                .fold(1.0, f64::max);
        let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let centers: Vec<_> = indices
            .iter()
            .enumerate()
            .map(|(pos, &i)| {
                let c = &chars[i];
                let x = c.bounds.center_x();
                let y = c.bounds.center_y();
                buckets
                    .entry(((x / cell).floor() as i64, (y / cell).floor() as i64))
                    .or_default()
                    .push(pos);
                (x, y)
            })
            .collect();
        for (a, &ia) in indices.iter().enumerate() {
            if dropped[ia] {
                continue;
            }
            let ca = &chars[ia];
            let (x, y) = centers[a];
            let (cx, cy) = ((x / cell).floor() as i64, (y / cell).floor() as i64);
            let mut candidates = Vec::new();
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if let Some(bucket) =
                        buckets.get(&(cx.saturating_add(dx), cy.saturating_add(dy)))
                    {
                        candidates.extend(bucket.iter().copied().filter(|&b| b > a));
                    }
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
            for b in candidates {
                let ib = indices[b];
                if dropped[ib] {
                    continue;
                }
                let cb = &chars[ib];
                let size_a = ca.font_size.max(1.0);
                let size_b = cb.font_size.max(1.0);
                let ratio = size_b / size_a;
                if !(0.8..=1.25).contains(&ratio)
                    || (x - centers[b].0).hypot(y - centers[b].1) >= 0.3 * size_a.max(size_b)
                    || ca.bounds.overlap_ratio(cb.bounds) < 0.5
                    || ligature(ia, ib, ca, cb)
                {
                    continue;
                }
                dropped[if size_a > size_b * 1.01 { ib } else { ia }] = true;
                if dropped[ia] {
                    break;
                }
            }
        }
    }
    let mut i = 0;
    chars.retain(|_| {
        let keep = !dropped[i];
        i += 1;
        keep
    });
}
pub(super) fn normalize_regional_fonts(chars: &mut [PdfChar]) {
    const SC: &str = "为对说这读们还进关问间门业务经条现发见车书长风华万与从众体单声义写办产乐让论";
    const TC: &str = "為對說這讀們還進關問間門業務經條現發見車書長風華萬與從眾體單聲義寫辦產樂讓論";
    let (mut sc, mut tc) = (0, 0);
    for c in chars.iter() {
        if let Some(ch) = char::from_u32(c.code) {
            if SC.contains(ch) {
                sc += 1;
            } else if TC.contains(ch) {
                tc += 1;
            }
        }
    }
    if sc < 10 || tc > 0 {
        return;
    }
    for c in chars {
        if !jscompat::js_regex!(r"\sTC$").test(&c.font_family) {
            continue;
        }
        let name = &c.font_family;
        c.font_family = if jscompat::js_regex!(r"hei|pingfang", "i").test(name) {
            "SimHei"
        } else if jscompat::js_regex!(r"kai", "i").test(name) {
            "KaiTi"
        } else if jscompat::js_regex!(r"song|sung|ming", "i").test(name) {
            "SimSun"
        } else {
            "FangSong"
        }
        .into();
    }
}
fn cjk_neighbor(c: u32) -> bool {
    matches!(c, 0x1100..=0x115f | 0x2e80..=0x9fff | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xff00..=0xff60)
}
pub(super) fn normalize_cjk_dashes(chars: &mut [PdfChar]) {
    let mut i = 0;
    while i < chars.len() {
        if chars[i].code != 0x2014 {
            i += 1;
            continue;
        }
        let mut end = i;
        while end + 1 < chars.len() && chars[end + 1].code == 0x2014 {
            end += 1;
        }
        if i.checked_sub(1)
            .is_some_and(|j| cjk_neighbor(chars[j].code))
            || chars.get(end + 1).is_some_and(|c| cjk_neighbor(c.code))
        {
            for c in &mut chars[i..=end] {
                c.code = 0x2015;
                c.text = "―".into();
            }
        }
        i = end + 1;
    }
}

pub(super) fn analyze_chars(chars: &[PdfChar]) -> Vec<super::ir::Line> {
    let normalized = super::rtl::normalize_arabic_forms(chars);
    let clustered = super::lines::cluster_combining_marks(&normalized);
    let mut lines = Vec::new();
    for raw in super::lines::group_into_lines(&clustered) {
        let is_rtl = super::rtl::has_rtl(&raw.chars);
        let ordered = if is_rtl {
            super::rtl::reorder_visual_to_logical(&raw.chars)
        } else {
            raw.chars
        };
        let spans = super::spans::build_spans(&super::words::group_into_words(&ordered, !is_rtl));
        if !spans.is_empty() {
            lines.push(super::ir::Line {
                spans,
                bounds: raw.bounds,
                baseline: raw.baseline,
                ends_with_hyphen: raw.ends_with_hyphen,
                hard_break_before: false,
            });
        }
    }
    lines
}
