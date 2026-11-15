//! Original editor folding and UTF-16 offset alignment.
use super::super::RADICALS;

pub(super) struct Fold {
    pub units: Vec<char>,
    pub start: Vec<usize>,
    pub end: Vec<usize>,
}

pub(super) fn fold(raw: &str, keep_spaces: bool) -> Fold {
    let mut out = Fold { units: Vec::new(), start: Vec::new(), end: Vec::new() };
    let mut offset = 0;
    let mut in_space = false;
    for ch in raw.chars() {
        let end = offset + ch.len_utf16();
        if jscompat::is_js_space(ch) {
            if keep_spaces && !in_space {
                out.units.push(' '); out.start.push(offset); out.end.push(end);
            }
            in_space = true;
        } else {
            in_space = false;
            let c = RADICALS.get(&ch).copied().unwrap_or(ch);
            for folded in main_core::pdf::font_locate::nfkc(&c.to_string()).chars() {
                if !jscompat::is_js_space(folded) {
                    out.units.push(folded); out.start.push(offset); out.end.push(end);
                }
            }
        }
        offset = end;
    }
    out.start.push(offset);
    out
}

pub(super) fn index_of(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() { None } else { hay.windows(needle.len()).position(|window| window == needle) }
}

pub(super) fn whitespace_clamp(old: &str, new: &str) -> Option<(Option<isize>, Option<isize>)> {
    let old = fold(old, true);
    let new = fold(new, true);
    let max = old.units.len().min(new.units.len());
    let mut prefix = 0;
    while prefix < max && old.units[prefix] == new.units[prefix] { prefix += 1; }
    if prefix == old.units.len() && prefix == new.units.len() { return None }
    let mut suffix = 0;
    while suffix < max - prefix && old.units[old.units.len()-1-suffix] == new.units[new.units.len()-1-suffix] { suffix += 1; }
    let window = |units: &[char], index: usize| (prefix < units.len()-suffix).then(|| units[index]);
    let head = window(&old.units, prefix) == Some(' ') || window(&new.units, prefix) == Some(' ');
    let tail = window(&old.units, old.units.len().saturating_sub(1+suffix)) == Some(' ')
        || window(&new.units, new.units.len().saturating_sub(1+suffix)) == Some(' ');
    if !head && !tail { return None }
    let head_count = || old.units[..prefix].iter().filter(|&&u| u != ' ').count() as isize - 1;
    let tail_count = || old.units[old.units.len()-suffix..].iter().filter(|&&u| u != ' ').count() as isize - 1;
    Some((head.then(head_count), tail.then(tail_count)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn maps_astral_and_compatibility_to_utf16_units() {
        let f = fold("😀 ﬃ", false);
        assert_eq!(f.units, ['😀', 'f', 'f', 'i']);
        assert_eq!(f.start, [0, 3, 3, 3, 4]);
        assert_eq!(f.end, [2, 4, 4, 4]);
    }
    #[test] fn clamps_space_deletion_but_not_pure_insertion_at_space() {
        assert!(whitespace_clamp("phon e", "phone").is_some());
        assert_eq!(whitespace_clamp("a b", "a xb"), None);
    }
}
