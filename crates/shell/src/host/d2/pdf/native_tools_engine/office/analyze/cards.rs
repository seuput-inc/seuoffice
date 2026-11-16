//! Discover contiguous light-text groups carried by dark background plates.
use super::super::geometry::Rect;
use super::super::ir::{CardRegion, IrPage, PageBlock};
use super::shapes::hex_luminance;
#[derive(Clone)]
pub(super) struct Candidate {
    pub panel_index: usize,
    pub bounds: Rect,
    pub color: String,
    pub rounded: bool,
    pub z: Option<i64>,
}
pub(super) fn detect(page: &mut IrPage, candidates: &[Candidate]) {
    let mut candidates = candidates.to_vec();
    candidates.sort_by(|a, b| a.bounds.area().total_cmp(&b.bounds.area()));
    for c in candidates {
        if page
            .sections
            .iter()
            .filter(|s| s.columns.len() > 1)
            .any(|s| c.bounds.overlap_ratio(s.bounds) >= 0.3)
        {
            continue;
        }
        let mut members = Vec::new();
        let mut flow_index = 0;
        let mut poisoned = false;
        let (mut total, mut light, mut line_count) = (0usize, 0usize, 0usize);
        for (i, b) in page.blocks.iter().enumerate() {
            if matches!(b,PageBlock::Image(img) if img.float.is_some()) {
                continue;
            }
            let bounds = match b {
                PageBlock::Text(t) => t.bounds,
                PageBlock::Table(t) => t.bounds,
                PageBlock::Image(t) => t.bounds,
            };
            let ratio = bounds.overlap_ratio(c.bounds);
            match b {
                PageBlock::Text(t) if t.card_id.is_none() && ratio >= 0.9 => {
                    if t.border.is_some()
                        || t.toc_entry.is_some()
                        || t.lines
                            .iter()
                            .flat_map(|l| &l.spans)
                            .any(|s| s.note_ref.is_some())
                    {
                        poisoned = true;
                    }
                    members.push((i, flow_index));
                    line_count += t.lines.len();
                    for s in t.lines.iter().flat_map(|l| &l.spans) {
                        let n = s.text.trim().encode_utf16().count();
                        total += n;
                        if hex_luminance(&s.color) >= 0.85 {
                            light += n;
                        }
                    }
                }
                PageBlock::Text(_) if ratio > 0.1 => poisoned = true,
                PageBlock::Image(_) | PageBlock::Table(_) if ratio >= 0.3 => poisoned = true,
                _ => (),
            };
            flow_index += 1;
        }
        if poisoned
            || members.is_empty()
            || line_count < 2
            || members.last().unwrap().1 - members[0].1 != members.len() - 1
            || hex_luminance(&c.color) > 0.75
            || total == 0
            || light as f64 / (total as f64) < 0.6
        {
            continue;
        }
        let id = page.cards.len();
        page.cards.push(CardRegion {
            bounds: c.bounds,
            color: c.color,
            rounded: c.rounded,
            z: c.z,
        });
        if let Some(panel) = page.background_panels.get_mut(c.panel_index) {
            panel.card_id = Some(id);
        }
        let indexes: std::collections::HashSet<_> = members.iter().map(|&(i, _)| i).collect();
        for &i in &indexes {
            if let PageBlock::Text(t) = &mut page.blocks[i] {
                t.card_id = Some(id);
            }
        }
        let mut index = 0;
        for b in page
            .sections
            .iter_mut()
            .flat_map(|s| &mut s.columns)
            .flat_map(|c| &mut c.blocks)
        {
            if indexes.contains(&index) {
                if let PageBlock::Text(t) = b {
                    t.card_id = Some(id);
                }
            }
            index += 1;
        }
    }
}
