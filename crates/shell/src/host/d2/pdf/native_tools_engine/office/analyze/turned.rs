//! Isolate sufficiently long rotated text runs from the upright content stream.
use super::super::geometry::{Rect, median};
use super::super::ir::{Alignment, Line, PdfChar, TextBlock};
use super::super::{spans, words};
fn blank(c: &PdfChar) -> bool {
    c.code <= 0x20 || c.code == 0xa0 || c.code == 0x3000
}

pub(super) fn split(chars: &[PdfChar]) -> (Vec<PdfChar>, Vec<Vec<PdfChar>>) {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut current = Vec::new();
    let mut last: Option<usize> = None;
    fn close(
        run: &mut Vec<usize>,
        last: &mut Option<usize>,
        runs: &mut Vec<Vec<usize>>,
        chars: &[PdfChar],
    ) {
        while run.last().is_some_and(|&i| blank(&chars[i])) {
            run.pop();
        }
        if run.iter().filter(|&&i| !blank(&chars[i])).count() >= 3 {
            runs.push(std::mem::take(run));
        } else {
            run.clear();
        }
        *last = None;
    }
    for (i, c) in chars.iter().enumerate() {
        if blank(c) {
            if last.is_some() {
                current.push(i);
            }
            continue;
        }
        if c.angle.abs() <= 0.26 {
            close(&mut current, &mut last, &mut runs, chars);
            continue;
        }
        if let Some(prev) = last {
            let p = &chars[prev];
            let em = c.font_size.max(p.font_size).max(1.0);
            if (c.angle - p.angle).abs() > 0.1
                || (c.origin_x - p.origin_x).hypot(c.origin_y - p.origin_y) > 2.0 * em
            {
                close(&mut current, &mut last, &mut runs, chars);
            }
        }
        current.push(i);
        last = Some(i);
    }
    close(&mut current, &mut last, &mut runs, chars);
    let mut taken = vec![false; chars.len()];
    for &i in runs.iter().flatten() {
        taken[i] = true;
    }
    (
        chars
            .iter()
            .enumerate()
            .filter(|(i, _)| !taken[*i])
            .map(|(_, c)| c.clone())
            .collect(),
        runs.iter()
            .map(|r| r.iter().map(|&i| chars[i].clone()).collect())
            .collect(),
    )
}
fn transformed(r: Rect, cos: f64, sin: f64) -> Rect {
    let points = [(r.x0, r.y0), (r.x1, r.y0), (r.x0, r.y1), (r.x1, r.y1)];
    let mut out = Rect {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };
    for (x, y) in points {
        let (x, y) = (x * cos + y * sin, -x * sin + y * cos);
        out.x0 = out.x0.min(x);
        out.x1 = out.x1.max(x);
        out.y0 = out.y0.min(y);
        out.y1 = out.y1.max(y);
    }
    out
}
pub(super) fn block(run: &[PdfChar]) -> Option<TextBlock> {
    let visible: Vec<_> = run.iter().filter(|c| !blank(c)).collect();
    if visible.is_empty() {
        return None;
    }
    let angle = median(&visible.iter().map(|c| c.angle).collect::<Vec<_>>());
    let (cos, sin) = (angle.cos(), angle.sin());
    let upright: Vec<_> = run
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c.bounds = transformed(c.bounds, cos, sin);
            c.loose_box = transformed(c.loose_box, cos, sin);
            let (x, y) = (
                c.origin_x * cos + c.origin_y * sin,
                -c.origin_x * sin + c.origin_y * cos,
            );
            c.origin_x = x;
            c.origin_y = y;
            c.angle = 0.0;
            c
        })
        .collect();
    let bounds = visible.iter().map(|c| c.bounds).reduce(Rect::union)?;
    let mut spans = spans::build_spans(&words::group_into_words(&upright, true));
    if spans.is_empty() {
        return None;
    }
    for span in &mut spans {
        span.bounds = bounds;
    }
    let direction = spans[0].direction;
    Some(TextBlock {
        lines: vec![Line {
            spans,
            bounds,
            baseline: bounds.y0,
            ends_with_hyphen: false,
            hard_break_before: false,
        }],
        bounds,
        align: Alignment::Left,
        first_line_indent_pt: 0.0,
        direction,
        list: None,
        stitched_from_line: None,
        toc_entry: None,
        spacing_before_pt: None,
        border: None,
        card_id: None,
    })
}
