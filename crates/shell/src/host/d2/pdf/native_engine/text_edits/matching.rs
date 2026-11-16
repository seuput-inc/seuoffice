//! Resolve page text edits without discarding untouched engine codepoints.
use super::{RADICALS, TextObject, field, number, text};
use shell_contract::IpcValue as V;

struct Fold {
    units: Vec<char>,
    starts: Vec<usize>,
    ends: Vec<usize>,
}
fn fold(raw: &str, keep_spaces: bool) -> Fold {
    let mut units = Vec::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut space = false;
    for (at, ch) in raw.char_indices() {
        if jscompat::is_js_space(ch) {
            if keep_spaces && !space {
                units.push(' ');
                starts.push(at);
                ends.push(at + ch.len_utf8())
            }
            space = true;
            continue;
        }
        space = false;
        let folded = RADICALS.get(&ch).copied().unwrap_or(ch);
        for ch in main_core::pdf::font_locate::nfkc(&folded.to_string()).chars() {
            if !jscompat::is_js_space(ch) {
                units.push(ch);
                starts.push(at);
                ends.push(at + raw[at..].chars().next().map(char::len_utf8).unwrap_or(0));
            }
        }
    }
    starts.push(raw.len());
    Fold {
        units,
        starts,
        ends,
    }
}
fn find(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        None
    } else {
        hay.windows(needle.len()).position(|w| w == needle)
    }
}
fn common(old: &Fold, new: &Fold) -> (usize, usize) {
    let max = old.units.len().min(new.units.len());
    let mut p = 0;
    while p < max && old.units[p] == new.units[p] {
        p += 1
    }
    let mut s = 0;
    while s < max - p && old.units[old.units.len() - 1 - s] == new.units[new.units.len() - 1 - s] {
        s += 1
    }
    (p, s)
}
fn split(f: &Fold, i: usize) -> bool {
    i > 0 && i < f.units.len() && f.starts[i] < f.ends[i - 1]
}
fn merge(engine: &str, old: &str, new: &str) -> String {
    let e = fold(engine, false);
    let o = fold(old, false);
    let n = fold(new, false);
    if e.units.len() != o.units.len() {
        return new.into();
    }
    let (mut p, mut s) = common(&o, &n);
    while p > 0 && (split(&e, p) || split(&n, p)) {
        p -= 1
    }
    while s > 0 && (split(&e, e.units.len() - s) || split(&n, n.units.len() - s)) {
        s -= 1
    }
    let mut out = String::new();
    let mut rank = 0usize;
    let mut previous_end = 0usize;
    for ch in new.chars() {
        if jscompat::is_js_space(ch) {
            out.push(ch);
            continue;
        }
        let m = main_core::pdf::font_locate::nfkc(&ch.to_string())
            .chars()
            .filter(|c| !jscompat::is_js_space(*c))
            .count();
        if m == 0 {
            out.push(ch);
            continue;
        }
        let prefix = rank + m <= p;
        let suffix = rank >= n.units.len() - s;
        if prefix || suffix {
            let at = if prefix {
                Some(rank)
            } else {
                e.units
                    .len()
                    .checked_sub(n.units.len())
                    .and_then(|shift| rank.checked_add(shift))
                    .or_else(|| rank.checked_sub(n.units.len() - e.units.len()))
            };
            if let Some(at) = at.filter(|at| at + m <= e.ends.len()) {
                let end = e.ends[at + m - 1];
                let start = e.starts[at].max(previous_end);
                if start <= end {
                    out.push_str(&engine[start..end])
                }
                previous_end = previous_end.max(end)
            } else {
                out.push(ch)
            }
        } else {
            out.push(ch)
        }
        rank += m
    }
    out
}
fn splice_lines(engine: &str, eng: &Fold, next: &Fold, p: usize, s: usize, new: &str) -> String {
    let positions = next
        .units
        .iter()
        .enumerate()
        .filter_map(|(i, c)| (*c != ' ').then_some(i))
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    let (mut k, mut rank, mut offset) = (0, 0, 0);
    for line in new.split('\n') {
        let end = offset + line.len();
        while k < next.units.len() && next.starts[k] < offset {
            if next.units[k] != ' ' {
                rank += 1
            }
            k += 1
        }
        let first = k;
        let g1 = rank;
        while k < next.units.len() && next.starts[k] < end {
            if next.units[k] != ' ' {
                rank += 1
            }
            k += 1
        }
        let g2 = rank;
        offset = end + 1;
        let prefix = k <= p;
        let suffix = first >= next.units.len() - s;
        if g1 == g2 || (!prefix && !suffix) {
            out.push(line.to_owned());
            continue;
        }
        let convert = |n: usize| {
            if prefix {
                Some(n)
            } else if eng.units.len() >= positions.len() {
                n.checked_add(eng.units.len() - positions.len())
            } else {
                n.checked_sub(positions.len() - eng.units.len())
            }
        };
        let rendered = convert(g1).zip(convert(g2)).and_then(|(a, b)| {
            if b > eng.units.len() || a > b || split(eng, a) || split(eng, b) {
                return None;
            }
            let mut text = String::new();
            for i in a..b {
                if i > a {
                    if eng.starts[i] < eng.ends[i - 1] {
                        continue;
                    }
                    let gap = &engine[eng.ends[i - 1]..eng.starts[i]];
                    text.push_str(gap);
                    let g = g1 + i - a;
                    if !gap.chars().any(jscompat::is_js_space)
                        && positions
                            .get(g)
                            .zip(positions.get(g - 1))
                            .is_some_and(|(x, y)| x - y > 1)
                    {
                        text.push(' ')
                    }
                }
                text.push_str(&engine[eng.starts[i]..eng.ends[i]])
            }
            Some(text)
        });
        out.push(rendered.unwrap_or_else(|| line.to_owned()))
    }
    out.join("\n")
}
fn splice(engine: &str, old: &str, new: &str) -> String {
    let old = fold(old, true);
    let next = fold(new, true);
    let eng = fold(engine, false);
    let (mut p, mut s) = common(&old, &next);
    if new.contains('\n') {
        return splice_lines(engine, &eng, &next, p, s, new);
    }
    loop {
        let np = old.units[..p].iter().filter(|&&c| c != ' ').count();
        let ns = old.units[old.units.len() - s..]
            .iter()
            .filter(|&&c| c != ' ')
            .count();
        let split_eng = split(&eng, np) || split(&eng, eng.units.len().saturating_sub(ns));
        let split_new = split(&next, p) || split(&next, next.units.len() - s);
        if !split_eng && !split_new {
            let pref_space = p > 0 && old.units[p - 1] == ' ';
            let suff_space = s > 0 && old.units[old.units.len() - s] == ' ';
            let a = if p == 0 {
                0
            } else if pref_space {
                eng.starts.get(np).copied().unwrap_or(engine.len())
            } else {
                eng.ends.get(np.saturating_sub(1)).copied().unwrap_or(0)
            };
            let b = if s == 0 {
                engine.len()
            } else if suff_space || ns == 0 {
                eng.ends
                    .get(eng.units.len().saturating_sub(ns + 1))
                    .copied()
                    .unwrap_or(0)
            } else {
                eng.starts
                    .get(eng.units.len().saturating_sub(ns))
                    .copied()
                    .unwrap_or(engine.len())
            };
            let x = next.starts[p];
            let y = next.starts[next.units.len() - s];
            if a <= b && x <= y {
                return format!("{}{}{}", &engine[..a], &new[x..y], &engine[b..]);
            }
            return new.into();
        }
        if p > 0 {
            p -= 1
        } else if s > 0 {
            s -= 1
        } else {
            return new.into();
        }
    }
}
fn rect(value: Option<&V>) -> Option<[f64; 4]> {
    let V::Array(v) = value? else { return None };
    Some([
        number(v.first())?,
        number(v.get(1))?,
        number(v.get(2))?,
        number(v.get(3))?,
    ])
}
fn overlap(a: [f64; 4], b: [f64; 4]) -> f64 {
    (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0)
}
fn area(a: [f64; 4]) -> f64 {
    ((a[2] - a[0]) * (a[3] - a[1])).max(1e-6)
}
fn union(objects: &[TextObject], indices: &[usize]) -> [f64; 4] {
    let mut r = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for i in indices {
        let a = objects[*i].bounds;
        r = [
            r[0].min(a[0]),
            r[1].min(a[1]),
            r[2].max(a[2]),
            r[3].max(a[3]),
        ]
    }
    r
}
fn order(objects: &[TextObject], indices: &[usize]) -> Vec<usize> {
    let mut sorted = indices.to_vec();
    sorted.sort_by(|a, b| objects[*b].bounds[3].total_cmp(&objects[*a].bounds[3]));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for i in sorted {
        let r = objects[i].bounds;
        let same = rows.last().and_then(|r| r.first()).is_some_and(|first| {
            let a = objects[*first].bounds;
            (a[3].min(r[3]) - a[1].max(r[1])) >= 0.5 * (a[3] - a[1]).min(r[3] - r[1])
        });
        if !same {
            rows.push(vec![])
        }
        if let Some(row) = rows.last_mut() {
            row.push(i)
        }
    }
    for row in &mut rows {
        row.sort_by(|a, b| objects[*a].bounds[0].total_cmp(&objects[*b].bounds[0]))
    }
    rows.into_iter().flatten().collect()
}
fn join(objects: &[TextObject], indices: &[usize]) -> String {
    indices.iter().map(|i| objects[*i].text.as_str()).collect()
}
fn layers(objects: &[TextObject], indices: &[usize]) -> Vec<Vec<usize>> {
    let mut sorted = indices.to_vec();
    sorted.sort_by(|a, b| objects[*a].bounds[0].total_cmp(&objects[*b].bounds[0]));
    let mut heights = sorted
        .iter()
        .map(|i| objects[*i].bounds[3] - objects[*i].bounds[1])
        .collect::<Vec<_>>();
    heights.sort_by(f64::total_cmp);
    let margin = heights.get(heights.len() / 2).copied().unwrap_or(0.0) * 0.5;
    let mut result: Vec<Vec<usize>> = Vec::new();
    for i in sorted {
        let r = objects[i].bounds;
        let mut choices = Vec::new();
        for (k, layer) in result.iter().enumerate() {
            let t = objects[*layer.last().unwrap_or(&i)].bounds;
            let overlap = t[2].min(r[2]) - t[0].max(r[0]);
            if overlap > 0.3 * (t[2] - t[0]).min(r[2] - r[0]) {
                continue;
            }
            choices.push((k, (r[0] - t[2]).abs(), layer.len()))
        }
        choices.sort_by(|a, b| a.1.total_cmp(&b.1));
        if choices.is_empty() {
            result.push(vec![i]);
            continue;
        }
        let base = choices[0].1;
        let mut pick = choices[0];
        for candidate in choices.into_iter().skip(1) {
            if candidate.1 - base > margin {
                break;
            }
            if candidate.2 > pick.2 {
                pick = candidate
            }
        }
        result[pick.0].push(i)
    }
    result
}
pub(super) struct Match {
    pub indices: Vec<usize>,
    pub replacement: String,
    pub whole: bool,
}
pub(super) fn locate(objects: &[TextObject], edit: &V) -> Result<Match, String> {
    let r = rect(field(edit, "rect")).ok_or("invalid edited text rectangle")?;
    let old = text(field(edit, "oldText")).ok_or("invalid edited text")?;
    let mut new = text(field(edit, "newText")).ok_or("invalid replacement text")?;
    if new.trim().is_empty() {
        new = ""
    }
    let old_key = fold(old, false).units;
    let hits = objects
        .iter()
        .enumerate()
        .filter_map(|(i, o)| (overlap(o.bounds, r) / area(o.bounds) >= 0.5).then_some(i))
        .collect::<Vec<_>>();
    let mut candidates = vec![hits.clone()];
    if hits.len() > 1 {
        candidates.extend(layers(objects, &hits))
    }
    for set in candidates {
        if set.is_empty() {
            continue;
        }
        let mut x = set.clone();
        x.sort_by(|a, b| objects[*a].bounds[0].total_cmp(&objects[*b].bounds[0]));
        for indices in [set.clone(), x, order(objects, &set)] {
            let joined = join(objects, &indices);
            if fold(&joined, false).units == old_key {
                let replacement = if new.is_empty() {
                    String::new()
                } else if field(edit, "lineLeading").is_some() {
                    merge(&joined, old, new)
                } else {
                    splice(&joined, old, new)
                };
                return Ok(Match {
                    indices: set,
                    replacement,
                    whole: true,
                });
            }
        }
    }
    let mut containers = objects
        .iter()
        .enumerate()
        .map(|(i, o)| (i, overlap(o.bounds, r) / area(r)))
        .filter(|(_, c)| *c >= 0.5)
        .collect::<Vec<_>>();
    containers.sort_by(|a, b| b.1.total_cmp(&a.1));
    if let Some((i, _)) = containers.first().copied() {
        let raw = &objects[i].text;
        let eng = fold(raw, false);
        if let Some(at) = find(&eng.units, &old_key) {
            let start = eng.starts[at];
            let end = eng.ends[at + old_key.len() - 1];
            let replacement = format!(
                "{}{}{}",
                &raw[..start],
                splice(&raw[start..end], old, new),
                &raw[end..]
            );
            return Ok(Match {
                indices: vec![i],
                replacement,
                whole: false,
            });
        }
    }
    let pad = 2.0_f64.max(number(field(edit, "fontSize")).unwrap_or(0.0) * 0.2);
    let padded = [r[0] - pad, r[1] - pad, r[2] + pad, r[3] + pad];
    let touching = order(
        objects,
        &objects
            .iter()
            .enumerate()
            .filter_map(|(i, o)| (overlap(o.bounds, padded) > 0.0).then_some(i))
            .collect::<Vec<_>>(),
    );
    let raw = join(objects, &touching);
    let eng = fold(&raw, false);
    let mut owner = Vec::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut raw_pos = 0;
    for (j, &i) in touching.iter().enumerate() {
        starts.push(raw_pos);
        raw_pos += objects[i].text.len();
        ends.push(raw_pos);
        owner.extend(std::iter::repeat_n(
            j,
            fold(&objects[i].text, false).units.len(),
        ));
    }
    let mut best = None;
    let mut start = 0;
    while let Some(rel) = find(&eng.units[start..], &old_key) {
        let at = start + rel;
        let last = at + old_key.len() - 1;
        let set = &touching[owner[at]..=owner[last]];
        let bounds = union(objects, set);
        let score = overlap(bounds, r) / area(bounds);
        if score > 0.0 && best.is_none_or(|(_, prev)| score > prev) {
            best = Some((at, score))
        }
        start = at + 1;
        if start >= eng.units.len() {
            break;
        }
    }
    if let Some((at, _)) = best {
        let last = at + old_key.len() - 1;
        let first_obj = owner[at];
        let last_obj = owner[last];
        let indices = touching[first_obj..=last_obj].to_vec();
        let set_raw = &raw[starts[first_obj]..ends[last_obj]];
        let whole = fold(set_raw, false).units == old_key;
        if whole {
            let replacement = if new.is_empty() {
                String::new()
            } else if field(edit, "lineLeading").is_some() {
                merge(set_raw, old, new)
            } else {
                splice(set_raw, old, new)
            };
            return Ok(Match {
                indices,
                replacement,
                whole: true,
            });
        }
        if field(edit, "origin").is_none() && field(edit, "lineLeading").is_none() {
            let a = eng.starts[at];
            let b = eng.ends[last];
            return Ok(Match {
                indices,
                replacement: format!(
                    "{}{}{}",
                    &raw[starts[first_obj]..a],
                    splice(&raw[a..b], old, new),
                    &raw[b..ends[last_obj]]
                ),
                whole: false,
            });
        }
    }
    Err("the edited text could not be located on the page".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragment_preserves_unedited_engine_text() {
        let object = TextObject {
            handle: 1,
            index: 0,
            font: None,
            text: "A B C".into(),
            bounds: [0.0, 0.0, 50.0, 10.0],
            chain: vec![],
            ordinals: vec![],
        };
        let edit = V::Object(vec![
            (
                "rect".into(),
                V::Array(vec![
                    V::Number(10.0),
                    V::Number(0.0),
                    V::Number(20.0),
                    V::Number(10.0),
                ]),
            ),
            ("oldText".into(), V::String("B".into())),
            ("newText".into(), V::String("X".into())),
        ]);
        let matched = locate(&[object], &edit).expect("match");
        assert_eq!(matched.replacement, "A X C");
        assert!(!matched.whole);
    }
}

#[cfg(test)]
mod splice_tests {
    use super::*;
    #[test]
    fn unchanged_engine_variants_survive_replacement() {
        assert_eq!(splice("Ａ B", "A B", "A C"), "Ａ C");
    }
    #[test]
    fn whitespace_only_replacement_deletes_the_matched_run() {
        let object = TextObject {
            handle: 1,
            index: 0,
            font: None,
            text: "Hello".into(),
            bounds: [0.0, 0.0, 50.0, 10.0],
            chain: vec![],
            ordinals: vec![],
        };
        let edit = V::Object(vec![
            (
                "rect".into(),
                V::Array(vec![
                    V::Number(0.0),
                    V::Number(0.0),
                    V::Number(50.0),
                    V::Number(10.0),
                ]),
            ),
            ("oldText".into(), V::String("Hello".into())),
            ("newText".into(), V::String(" ".into())),
        ]);
        assert_eq!(locate(&[object], &edit).expect("match").replacement, "");
    }
}
