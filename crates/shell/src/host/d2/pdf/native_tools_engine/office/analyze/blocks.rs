//! Paragraph grouping, layout inference and authored line breaks.
use super::super::{
    geometry::{Rect, median},
    ir::{Alignment, Direction, Line, TextBlock},
    rtl::first_strong_dir,
    script,
};
use jscompat::TryJsRegex;
#[derive(Clone, Copy)]
pub(super) struct BodyContext {
    pub body_left: f64,
    pub body_right: f64,
}
#[derive(Clone, Copy, Default)]
pub(super) struct BlockOptions {
    pub pin_open_leaded_breaks: bool,
}
fn med(v: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<_> = v.collect();
    let n = median(&v);
    if n == 0. { 12. } else { n }
}
fn size(l: &Line) -> f64 {
    med(l.spans.iter().map(|s| s.font_size))
}
fn text(l: &Line) -> String {
    l.spans.iter().map(|s| s.text.as_str()).collect()
}
fn re(pattern: &str, text: &str) -> Result<bool, String> {
    TryJsRegex::new(pattern, "u")
        .map_err(|e| e.to_string())?
        .try_test(text)
        .map_err(|e| e.to_string())
}
fn lower(l: &Line) -> Result<bool, String> {
    let t = text(l);
    let Some(c) = t.trim_start_matches(jscompat::is_js_space).chars().next() else {
        return Ok(false);
    };
    let t = c.to_string();
    Ok(re(r"^\p{Ll}$", &t)? && re(r"\p{Script=Latin}", &t)?)
}
pub(super) fn body_context_of(lines: &[Line]) -> BodyContext {
    if lines.is_empty() {
        return BodyContext {
            body_left: 0.,
            body_right: 0.,
        };
    }
    BodyContext {
        body_left: lines
            .iter()
            .map(|l| l.bounds.x0)
            .fold(f64::INFINITY, f64::min),
        body_right: lines
            .iter()
            .map(|l| l.bounds.x1)
            .fold(f64::NEG_INFINITY, f64::max),
    }
}
fn word(l: &Line) -> f64 {
    let Some(span) = l.spans.first() else {
        return 0.;
    };
    let t = span.text.trim_matches(jscompat::is_js_space);
    let n = t.chars().count().max(1) as f64;
    let w = span.bounds.width();
    if script::no_space(span.script) {
        return w / n;
    }
    match t.find(' ') {
        Some(i) if i > 0 => w * (t[..i].encode_utf16().count() as f64 / n),
        _ => w,
    }
}
fn paragraph_break(
    prev: &Line,
    cur: &Line,
    gap: f64,
    height: f64,
    body: BodyContext,
) -> Result<bool, String> {
    let a = size(prev);
    let b = size(cur);
    let fs = a.max(b);
    let tol = 0.4 * fs;
    let smaller = a.min(b);
    if smaller > 0. && fs / smaller >= 1.5 {
        return Ok(true);
    }
    let allowed = (1.5 * gap).max(0.45 * height).min(2. * height);
    if prev.bounds.y0 - cur.bounds.y1 > allowed {
        return Ok(true);
    }
    if cur.hard_break_before || prev.ends_with_hyphen {
        return Ok(false);
    }
    let indent = cur.bounds.x0 - prev.bounds.x0;
    if (prev.bounds.x0 - body.body_left).abs() <= tol
        && indent > 0.8 * fs
        && indent < 4. * fs
        && cur.bounds.x1 >= body.body_right - 2. * fs
    {
        return Ok(true);
    }
    let leftover = body.body_right - prev.bounds.x1;
    let bold = |l: &Line| l.spans.iter().all(|s| s.bold);
    let composer = lower(cur)? && bold(prev) == bold(cur) && leftover < word(cur) + 3. * fs;
    Ok((prev.bounds.x0 - body.body_left).abs() <= tol
        && (cur.bounds.x0 - body.body_left).abs() <= tol
        && leftover > 3. * fs
        && cur.bounds.x1 - prev.bounds.x1 > fs
        && !composer)
}
fn hyphenate(lines: &mut [Line], right: f64) -> Result<(), String> {
    for i in 0..lines.len().saturating_sub(1) {
        if lines[i].ends_with_hyphen {
            continue;
        }
        let t = text(&lines[i]);
        let t = t.trim_end_matches(jscompat::is_js_space);
        if t.ends_with('\u{ad}') {
            lines[i].ends_with_hyphen = true;
            continue;
        }
        if re(r"\p{Script=Latin}[-‐]$", t)?
            && lower(&lines[i + 1])?
            && right - lines[i].bounds.x1 < word(&lines[i + 1]) + 0.5 * size(&lines[i])
        {
            lines[i].ends_with_hyphen = true
        }
    }
    Ok(())
}
fn verse_chain(a: &Line, b: &Line) -> bool {
    if a.ends_with_hyphen {
        return false;
    }
    let sa = size(a);
    let sb = size(b);
    let fs = sa.max(sb);
    if fs / sa.min(sb).max(1.) >= 1.5 || (a.bounds.x0 - b.bounds.x0).abs() > 0.5 * fs {
        return false;
    }
    let pitch = a.baseline - b.baseline;
    if pitch <= 0. || pitch > 1.9 * fs {
        return false;
    }
    let mixed = |l: &Line| {
        l.spans
            .iter()
            .any(|s| script::no_space(s.script) || s.direction == Direction::Rtl)
    };
    !mixed(a) && !mixed(b)
}
fn verse(lines: &[Line]) -> Result<bool, String> {
    if lines.len() < 3 {
        return Ok(false);
    }
    let fs = med(lines.iter().map(size));
    let max = lines
        .iter()
        .map(|l| l.bounds.x1)
        .fold(f64::NEG_INFINITY, f64::max);
    let near = lines.iter().filter(|l| l.bounds.x1 >= max - fs).count() as f64 / lines.len() as f64;
    let evidence = lines
        .windows(2)
        .filter(|w| max - w[0].bounds.x1 >= fs.max(word(&w[1]) + 0.5 * fs))
        .count() as f64
        / (lines.len() - 1) as f64;
    if near <= 1. / 3. && evidence >= 0.5 {
        return Ok(true);
    }
    if lines.len() < 4 || near > 0.5 {
        return Ok(false);
    }
    let mut punct = 0;
    for line in &lines[..lines.len() - 1] {
        if re(
            r#"[.,;:!?…—–"'“”‘’»«)\]]\s*$"#,
            text(line).trim_end_matches(jscompat::is_js_space),
        )? {
            punct += 1
        }
    }
    Ok(punct as f64 / (lines.len() - 1) as f64 >= 0.8)
}
pub(super) fn is_verse_stack(lines: &[Line]) -> Result<bool, String> {
    if lines.len() < 3 || !lines.windows(2).all(|w| verse_chain(&w[0], &w[1])) {
        return Ok(false);
    }
    verse(lines)
}
fn mark_verse(lines: &mut [Line]) -> Result<(), String> {
    let mut start = 0;
    while start < lines.len() {
        let mut end = start;
        while end + 1 < lines.len() && verse_chain(&lines[end], &lines[end + 1]) {
            end += 1
        }
        if verse(&lines[start..=end])? {
            for line in &mut lines[start + 1..=end] {
                line.hard_break_before = true
            }
        }
        start = end + 1
    }
    Ok(())
}
fn split_items(group: Vec<Line>, left: f64, right: f64) -> Vec<Vec<Line>> {
    if group.len() < 2 {
        return vec![group];
    }
    let fs = med(group.iter().map(size));
    let tol = 0.4 * fs;
    let x = group[0].bounds.x0;
    if x - left >= 4. * fs || group.iter().any(|l| (l.bounds.x0 - x).abs() > tol) {
        return vec![group];
    }
    let max = group
        .iter()
        .map(|l| l.bounds.x1)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut misses = Vec::new();
    for w in group.windows(2) {
        if w[0].ends_with_hyphen || w[0].bounds.width() >= 0.85 * (right - left) {
            continue;
        }
        let miss = max - w[0].bounds.x1 - word(&w[1]) - 0.5 * fs;
        if miss >= 0. {
            misses.push(miss)
        }
    }
    if !misses.iter().any(|m| *m >= 3. * fs)
        && !(!misses.is_empty() && (misses.len() as f64) >= (group.len() - 1) as f64 / 3.)
    {
        return vec![group];
    }
    group
        .into_iter()
        .map(|mut l| {
            l.hard_break_before = false;
            vec![l]
        })
        .collect()
}
fn spread(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        - v.iter().copied().fold(f64::INFINITY, f64::min)
}
fn infer(lines: &[Line], body: BodyContext) -> (Alignment, f64) {
    let fs = med(lines.iter().map(size));
    let tol = fs.max((body.body_right - body.body_left) * 0.02);
    if lines.len() == 1 {
        let l = &lines[0];
        let left = l.bounds.x0 - body.body_left;
        let right = body.body_right - l.bounds.x1;
        if left <= tol {
            return (Alignment::Left, 0.);
        }
        if right <= tol && left > 2. * tol {
            return (Alignment::Right, 0.);
        }
        if (left - right).abs() <= 2. * tol && left > 2. * tol {
            return (Alignment::Center, 0.);
        }
        return (
            Alignment::Left,
            if left > 0.5 * fs && left < 4. * fs {
                left
            } else {
                0.
            },
        );
    }
    let left: Vec<_> = lines.iter().map(|l| l.bounds.x0).collect();
    let right: Vec<_> = lines.iter().map(|l| l.bounds.x1).collect();
    let centers: Vec<_> = lines.iter().map(|l| l.bounds.center_x()).collect();
    let la = spread(&left[1..]) <= tol;
    let ra = spread(&right) <= tol;
    let ca = spread(&centers) <= tol;
    let floating = left.iter().copied().fold(f64::INFINITY, f64::min) - body.body_left > tol;
    if ca && spread(&left) > tol && spread(&right) > tol && floating {
        return (Alignment::Center, 0.);
    }
    let mut align = Alignment::Left;
    if !la && ra {
        align = Alignment::Right
    } else if !la && !ra && ca {
        align = Alignment::Center
    } else if la && !ra && lines.len() >= 4 {
        let non = &right[..right.len() - 1];
        if spread(non) <= 0.3 * fs
            && right[right.len() - 1] <= non.iter().copied().fold(f64::NEG_INFINITY, f64::max) + tol
        {
            align = Alignment::Justify
        }
    }
    let para = left[1..].iter().copied().fold(f64::INFINITY, f64::min);
    let indent = left[0] - para;
    (
        align,
        if matches!(align, Alignment::Left | Alignment::Justify)
            && la
            && indent > 0.5 * fs
            && indent < 4. * fs
        {
            indent
        } else {
            0.
        },
    )
}
fn hard_breaks(group: &mut [Line], align: Alignment, left: f64, right: f64, pin: bool) {
    if group.len() < 2 {
        return;
    }
    let gl = group
        .iter()
        .map(|l| l.bounds.x0)
        .fold(f64::INFINITY, f64::min);
    let gr = group
        .iter()
        .map(|l| l.bounds.x1)
        .fold(f64::NEG_INFINITY, f64::max);
    let display = group.len() <= 3 && group.iter().all(|l| size(l) >= 24.);
    for i in 1..group.len() {
        let prev = &group[i - 1];
        if prev.ends_with_hyphen {
            continue;
        }
        let fs = size(prev);
        let leftover = match align {
            Alignment::Right => prev.bounds.x0 - left,
            Alignment::Center => right - left - prev.bounds.width(),
            _ => right - prev.bounds.x1,
        };
        let need = word(&group[i]) + 0.5 * fs;
        let open = (prev.baseline - group[i].baseline).abs() >= 1.35 * fs;
        let zone = match align {
            Alignment::Right => prev.bounds.x0 - gl,
            Alignment::Center => gr - gl - prev.bounds.width(),
            _ => gr - prev.bounds.x1,
        };
        if display
            || (leftover > fs && leftover >= 0.4 * (right - left) && leftover >= need)
            || (pin && open && group.len() <= 3)
            || (open && zone > fs && zone >= need)
        {
            group[i].hard_break_before = true
        }
    }
}
pub(super) fn group_into_blocks(
    raw: &[Line],
    body: Option<BodyContext>,
    options: BlockOptions,
) -> Result<Vec<TextBlock>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = body.unwrap_or_else(|| body_context_of(raw));
    let wrap_right = ctx.body_right.min(
        raw.iter()
            .map(|l| l.bounds.x1)
            .fold(f64::NEG_INFINITY, f64::max),
    );
    let wrap_left = ctx.body_left.max(
        raw.iter()
            .map(|l| l.bounds.x0)
            .fold(f64::INFINITY, f64::min),
    );
    let mut lines = raw.to_vec();
    hyphenate(&mut lines, wrap_right)?;
    mark_verse(&mut lines)?;
    let gaps: Vec<_> = lines
        .windows(2)
        .map(|w| w[0].bounds.y0 - w[1].bounds.y1)
        .filter(|g| *g > 0.)
        .collect();
    let gap = median(&gaps);
    let height = med(lines.iter().map(|l| l.bounds.height()));
    let mut groups = Vec::new();
    let mut current: Vec<Line> = Vec::new();
    for line in lines {
        if let Some(prev) = current.last() {
            if paragraph_break(prev, &line, gap, height, ctx)? {
                groups.push(std::mem::take(&mut current))
            }
        }
        current.push(line)
    }
    groups.push(current);
    let mut result = Vec::new();
    for group in groups
        .into_iter()
        .flat_map(|g| split_items(g, wrap_left, wrap_right))
    {
        let (align, indent) = infer(&group, ctx);
        let direction = first_strong_dir(&group);
        let mut group = group;
        if direction == Direction::Ltr {
            hard_breaks(
                &mut group,
                align,
                wrap_left,
                wrap_right,
                options.pin_open_leaded_breaks,
            )
        }
        let mut pieces = Vec::new();
        if align == Alignment::Justify {
            let mut piece = Vec::new();
            for mut line in group {
                if line.hard_break_before && !piece.is_empty() {
                    pieces.push(std::mem::take(&mut piece))
                }
                line.hard_break_before = false;
                piece.push(line)
            }
            if !piece.is_empty() {
                pieces.push(piece)
            }
        } else {
            pieces.push(group)
        }
        for (i, piece) in pieces.into_iter().enumerate() {
            let bounds = piece
                .iter()
                .map(|l| l.bounds)
                .reduce(Rect::union)
                .unwrap_or_default();
            result.push(TextBlock {
                lines: piece,
                bounds,
                align,
                first_line_indent_pt: if i == 0 { indent } else { 0. },
                direction,
                list: None,
                stitched_from_line: None,
                toc_entry: None,
                spacing_before_pt: None,
                border: None,
                card_id: None,
            })
        }
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::super::super::{ir::Span, script::Script};
    use super::*;
    fn line(label: &str, y: f64, font: f64) -> Line {
        let bounds = Rect {
            x0: 0.,
            y0: y,
            x1: 100.,
            y1: y + 10.,
        };
        Line {
            spans: vec![Span {
                text: label.into(),
                bounds,
                font_size: font,
                font_family: "A".into(),
                bold: false,
                italic: false,
                color: "000000".into(),
                direction: Direction::Ltr,
                script: Script::Latin,
                highlight: None,
                underline: false,
                strike: false,
                char_scale: None,
                char_spacing_pt: None,
                note_ref: None,
                invisible: false,
                href: None,
            }],
            bounds,
            baseline: y,
            ends_with_hyphen: false,
            hard_break_before: false,
        }
    }
    #[test]
    fn different_display_sizes_split_paragraphs() {
        let blocks = group_into_blocks(
            &[line("Title", 80., 24.), line("Body", 66., 12.)],
            None,
            BlockOptions::default(),
        )
        .unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lines[0].spans[0].text, "Title");
        assert_eq!(blocks[1].lines[0].spans[0].text, "Body");
    }
    #[test]
    fn empty_lines_produce_no_blocks() {
        assert!(
            group_into_blocks(&[], None, BlockOptions::default())
                .unwrap()
                .is_empty()
        );
        assert!(!is_verse_stack(&[]).unwrap());
    }
}
