//! Ownership, matching and table-cell placement for PDF text objects.
use std::collections::{HashMap, HashSet};
use super::layout::Rect;

#[derive(Clone, Debug)]
pub(super) struct TextObject {
    pub index: usize, pub bounds: Rect, pub text: String, pub size: f64,
    pub baselines: Vec<f64>, pub color: [u8; 3], pub invisible: bool, pub vertical: bool,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct FormChar { pub at: (f64, f64), pub size: f64 }
#[derive(Clone, Debug)]
pub(super) struct Owner { pub id: String, pub bounds: Rect }
#[derive(Clone, Debug)]
pub(super) struct CellText { pub key: String, pub text: String, pub area: Option<Rect> }
#[derive(Clone, Debug)]
pub(super) struct CellPlacement { pub objects: Vec<usize>, pub bounds: Rect, pub area: Option<Rect> }
#[derive(Clone, Debug)]
pub(super) enum CellResult { Shared, Placed(CellPlacement) }
#[derive(Clone, Debug)]
pub(super) struct OcrLine { pub text: String, pub bounds: Rect }

pub(super) fn median(values: &[f64]) -> f64 {
    if values.is_empty() { return 0.0; }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}
pub(super) fn center(r: Rect) -> (f64, f64) { ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0) }
pub(super) fn area(r: Rect) -> f64 { (r.x1 - r.x0).max(0.0) * (r.y1 - r.y0).max(0.0) }
pub(super) fn expand(r: Rect, d: f64) -> Rect { Rect { x0: r.x0-d, y0: r.y0-d, x1: r.x1+d, y1: r.y1+d } }
pub(super) fn contains(r: Rect, p: (f64, f64)) -> bool { p.0 >= r.x0 && p.0 <= r.x1 && p.1 >= r.y0 && p.1 <= r.y1 }
pub(super) fn union(rects: &[Rect]) -> Option<Rect> {
    rects.iter().copied().reduce(|a, r| Rect { x0: a.x0.min(r.x0), y0: a.y0.min(r.y0), x1: a.x1.max(r.x1), y1: a.y1.max(r.y1) })
}
fn overlap_share(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 { if a1 <= a0 { 1.0 } else { (a1.min(b1) - a0.max(b0)).max(0.0) / (a1-a0) } }
pub(super) fn mostly_inside(rect: Rect, outer: Rect) -> bool {
    overlap_share(rect.x0, rect.x1, outer.x0, outer.x1) >= 0.7 && overlap_share(rect.y0, rect.y1, outer.y0, outer.y1) >= 0.5
}
pub(super) fn norm_text(text: &str) -> String {
    main_core::pdf::font_locate::nfkc(text).chars().filter(|ch| !jscompat::is_js_space(*ch)).flat_map(char::to_lowercase).collect()
}
pub(super) fn similarity(a: &str, b: &str) -> f64 {
    if a == b { return 1.0; }
    let a: Vec<_> = a.encode_utf16().collect();
    let b: Vec<_> = b.encode_utf16().collect();
    if a.len() < 2 || b.len() < 2 {
        return if a.is_empty() || b.is_empty() || a.windows(b.len()).any(|w| w == b) || b.windows(a.len()).any(|w| w == a) { 0.5 } else { 0.0 };
    }
    let mut grams = HashMap::new();
    for pair in a.windows(2) { *grams.entry((pair[0], pair[1])).or_insert(0usize) += 1; }
    let mut hits = 0usize;
    for pair in b.windows(2) {
        if let Some(count) = grams.get_mut(&(pair[0], pair[1])) {
            if *count > 0 { hits += 1; *count -= 1; }
        }
    }
    2.0 * hits as f64 / (a.len() + b.len() - 2) as f64
}
pub(super) fn assign_objects(objects: &[TextObject], owners: &[Owner]) -> HashMap<String, Vec<usize>> {
    let mut result: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, object) in objects.iter().enumerate() {
        let mut best: Option<&Owner> = None;
        for owner in owners {
            if !contains(expand(owner.bounds, 1.5), center(object.bounds)) { continue; }
            if best.is_none_or(|b| area(owner.bounds) < area(b.bounds)) { best = Some(owner); }
        }
        if let Some(owner) = best { result.entry(owner.id.clone()).or_default().push(index); }
    }
    result
}
pub(super) fn adopt_hyphens(objects: &[TextObject], owners: &[Owner], owned: &mut HashMap<String, Vec<usize>>) -> HashSet<usize> {
    let taken: HashSet<_> = owned.values().flatten().copied().collect();
    let mut adopted = HashSet::new();
    for (index, object) in objects.iter().enumerate() {
        let normalized = norm_text(&object.text);
        if taken.contains(&index) || normalized.is_empty() || !normalized.chars().all(|c| matches!(c, '-' | '\u{ad}' | '\u{2010}'..='\u{2013}')) { continue; }
        let (_, y) = center(object.bounds);
        let mut best = None;
        let mut best_gap = f64::INFINITY;
        for owner in owners {
            if y < owner.bounds.y0 - 1.0 || y > owner.bounds.y1 + 1.0 { continue; }
            let gap = object.bounds.x0 - owner.bounds.x1;
            if gap < -1.0 || gap > object.size || gap >= best_gap { continue; }
            best = Some(owner);
            best_gap = gap;
        }
        if let Some(owner) = best { owned.entry(owner.id.clone()).or_default().push(index); adopted.insert(index); }
    }
    adopted
}
pub(super) fn reading_order(objects: &[TextObject], indices: &[usize]) -> Vec<usize> {
    let mut result = indices.to_vec();
    result.sort_by(|a, b| {
        let a = &objects[*a]; let b = &objects[*b];
        let dy = b.baselines.first().copied().unwrap_or(0.0) - a.baselines.first().copied().unwrap_or(0.0);
        if dy.abs() > a.size.min(b.size) * 0.3 { dy.total_cmp(&0.0) } else { a.bounds.x0.total_cmp(&b.bounds.x0) }
    });
    result
}
fn overlap(a: Rect, b: Rect) -> f64 { (a.x1.min(b.x1)-a.x0.max(b.x0)).max(0.0) * (a.y1.min(b.y1)-a.y0.max(b.y0)).max(0.0) }
fn inset(r: Rect) -> Rect { Rect { x0:r.x0+2.0, y0:r.y0+1.0, x1:(r.x1-2.0).max(r.x0+2.0), y1:(r.y1-1.0).max(r.y0+1.0) } }
pub(super) fn place_cells_by_area(objects: &[TextObject], indices: &[usize], cells: &[CellText]) -> Result<HashMap<String, CellResult>, String> {
    let mut got: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut straddling = Vec::new();
    for index in indices {
        let object = &objects[*index];
        if norm_text(&object.text).is_empty() { continue; }
        let mut home: Option<&CellText> = None;
        for cell in cells {
            let cell_area = cell.area.ok_or("Table cell has no measured area.")?;
            if home.is_none_or(|best| overlap(object.bounds, cell_area) > overlap(object.bounds, best.area.unwrap_or(cell_area))) { home = Some(cell); }
        }
        let Some(home) = home else { straddling.push(object.bounds); continue };
        let home_area = home.area.ok_or("Table cell has no measured area.")?;
        let reaches_other = cells.iter().any(|cell| cell.key != home.key && cell.area.is_some_and(|area| overlap(object.bounds, expand(area, -1.5)) > 0.0));
        if reaches_other { straddling.push(object.bounds); } else { let _ = home_area; got.entry(&home.key).or_default().push(*index); }
    }
    let mut result = HashMap::new();
    for cell in cells {
        let raw = cell.area.ok_or("Table cell has no measured area.")?;
        if straddling.iter().any(|r| r.x0 < raw.x1 && r.x1 > raw.x0 && r.y0 < raw.y1 && r.y1 > raw.y0) { result.insert(cell.key.clone(), CellResult::Shared); continue; }
        let Some(assigned) = got.remove(cell.key.as_str()) else { continue };
        let area = inset(raw);
        let bounds = union(&assigned.iter().map(|index| objects[*index].bounds).collect::<Vec<_>>()).ok_or("Table cell has no text bounds.")?;
        let top = area.y1.min(bounds.y1);
        result.insert(cell.key.clone(), CellResult::Placed(CellPlacement { objects: assigned, bounds: Rect { y1: area.y0.max(top), ..area }, area: Some(area) }));
    }
    Ok(result)
}

pub(super) fn lines_in_region(lines: &[OcrLine], region: Rect) -> Vec<usize> {
    lines.iter().enumerate().filter_map(|(i, line)| contains(expand(region, 2.0), center(line.bounds)).then_some(i)).collect()
}
pub(super) fn match_lines_to_blocks(lines: &[OcrLine], blocks: &[String]) -> HashMap<usize, Vec<usize>> {
    let texts: Vec<_> = lines.iter().map(|line| norm_text(&line.text)).collect();
    let mut cursor = 0;
    let mut result = HashMap::new();
    for (block_index, block) in blocks.iter().enumerate() {
        let target = norm_text(block);
        if target.is_empty() { continue; }
        let mut best = (0.0, 0, 0);
        for start in cursor..lines.len().min(cursor + 4) {
            let mut combined = String::new();
            for end in start..lines.len() {
                combined.push_str(&texts[end]);
                let score = similarity(&combined, &target);
                if score > best.0 { best = (score, start, end); }
                if combined.encode_utf16().count() as f64 > target.encode_utf16().count() as f64 * 1.6 { break; }
            }
        }
        if best.0 >= 0.45 { result.insert(block_index, (best.1..=best.2).collect()); cursor = best.2 + 1; }
    }
    result
}

pub(super) fn place_cells(objects: &[TextObject], indices: &[usize], cells: &[CellText], table: Rect) -> HashMap<String, CellPlacement> {
    let want: Vec<_> = cells.iter().map(|cell| norm_text(&cell.text)).collect();
    let mut got = vec![Vec::new(); cells.len()];
    let mut used = vec![0usize; cells.len()];
    let mut loose = Vec::new();
    for index in reading_order(objects, indices) {
        let text = norm_text(&objects[index].text);
        if text.is_empty() { continue; }
        let length = text.encode_utf16().count();
        let found = want.iter().enumerate().position(|(i, value)| value.contains(&text) && used[i] + length <= value.encode_utf16().count() + 1);
        if let Some(i) = found { got[i].push(index); used[i] += length; } else { loose.push(index); }
    }
    let mut found = HashMap::new();
    for (i, cell) in cells.iter().enumerate() {
        let length = want[i].encode_utf16().count();
        if length > 0 && used[i] as f64 >= length as f64 * 0.8 {
            if let Some(bounds) = union(&got[i].iter().map(|index| objects[*index].bounds).collect::<Vec<_>>()) { found.insert(cell.key.as_str(), bounds); }
        } else { loose.extend(got[i].iter().copied()); }
    }
    let mut result = HashMap::new();
    for (i, cell) in cells.iter().enumerate() {
        let Some(bounds) = found.get(cell.key.as_str()).copied() else { continue };
        let mut right = table.x1;
        let mut bottom = table.y0;
        let obstacles = found.iter().filter_map(|(key, rect)| (*key != cell.key).then_some(*rect))
            .chain(loose.iter().map(|index| objects[*index].bounds));
        for ob in obstacles {
            let vertical = ob.y0 < bounds.y1 && ob.y1 > bounds.y0;
            let horizontal = ob.x0 < bounds.x1 && ob.x1 > bounds.x0;
            if vertical && ob.x0 >= bounds.x1 - 0.5 { right = right.min(ob.x0 - 2.0); }
            if horizontal && ob.y1 <= bounds.y0 + 0.5 { bottom = bottom.max(ob.y1 + 2.0); }
        }
        result.insert(cell.key.clone(), CellPlacement { objects:got[i].clone(), bounds:Rect { x0:bounds.x0, y0:bounds.y0.min(bottom), x1:bounds.x1.max(right), y1:bounds.y1 }, area:None });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_prefers_smallest_and_normalizes_fullwidth() {
        let bounds = Rect { x0: 4.0, y0: 4.0, x1: 6.0, y1: 6.0 };
        let objects = [TextObject { index: 0, bounds, text: "Ａ".into(), size: 10.0, baselines: vec![5.0], color:[0,0,0], invisible:false, vertical:false }];
        let owners = [Owner { id:"outer".into(), bounds: Rect { x0:0.0,y0:0.0,x1:10.0,y1:10.0 } }, Owner { id:"inner".into(), bounds }];
        assert_eq!(assign_objects(&objects, &owners)["inner"], vec![0]);
        assert_eq!(norm_text("Ａ B"), "ab");
    }
}
