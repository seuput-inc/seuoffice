//! Reapply untouched fill colors by folded character rank.
use super::super::{PdfiumSession, TextObject, ordered};
use super::{fold::fold, stroke};
use std::collections::{HashMap, HashSet};

pub(in super::super) fn matched(
    s: &mut PdfiumSession,
    objects: &[TextObject],
    target: &str,
) -> Result<Option<Vec<Option<[u8; 3]>>>, String> {
    if objects.len() < 2 {
        return Ok(None);
    }
    let mut colors = HashMap::new();
    let mut distinct = HashSet::new();
    for obj in objects {
        let color =
            stroke::color(s, obj.handle, "FPDFPageObj_GetFillColor")?.map(|c| [c[0], c[1], c[2]]);
        if let Some(c) = color {
            distinct.insert(c);
        }
        colors.insert(obj.handle, color);
    }
    if distinct.len() <= 1 {
        return Ok(None);
    }
    let mut units = Vec::new();
    let mut inherited = Vec::new();
    for obj in ordered(objects) {
        let f = fold(&obj.text, false);
        for ch in f.units {
            units.push(ch);
            inherited.push(colors[&obj.handle]);
        }
    }
    let target_fold = fold(target, false);
    let max = units.len().min(target_fold.units.len());
    let mut prefix = 0;
    while prefix < max && units[prefix] == target_fold.units[prefix] {
        prefix += 1
    }
    let mut suffix = 0;
    while suffix < max - prefix
        && units[units.len() - 1 - suffix]
            == target_fold.units[target_fold.units.len() - 1 - suffix]
    {
        suffix += 1
    }
    let mut result = vec![None; target.encode_utf16().count()];
    for i in 0..target_fold.units.len() {
        let color = if i < prefix {
            inherited[i]
        } else if i >= target_fold.units.len() - suffix {
            inherited[units.len() - (target_fold.units.len() - i)]
        } else {
            None
        };
        if let Some(color) = color {
            for slot in &mut result[target_fold.start[i]..target_fold.end[i]] {
                *slot = Some(color)
            }
        }
    }
    Ok(result.iter().any(Option::is_some).then_some(result))
}
