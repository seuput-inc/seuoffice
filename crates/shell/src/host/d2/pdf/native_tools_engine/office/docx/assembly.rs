//! Assemble analyzed PDF pages into DOCX saver blocks and options.
use super::super::geometry::Rect;
use super::super::ir::{Direction, FurnitureHf, IrPage, PageBlock, PageColumn, PageSection};
use super::numbering::Numbering;
use super::{budget, card, fit, flow, furniture, geometry, image, leader, section, table, text};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

fn spacer(height: f64, page_break: bool) -> Value {
    let mut format = json!({"spaceAfter":0,"lineRule":"exact","lineRawTwips":20.0_f64.max((height*20.0).round())});
    if page_break {
        format["pageBreakBefore"] = json!(true);
    }
    json!({"kind":"generated","block":{"type":"paragraph","runs":[],"format":format}})
}
fn column_break() -> Value {
    json!({"kind":"xml","xml":"<w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/></w:pPr><w:r><w:br w:type=\"column\"/></w:r></w:p>"})
}
fn sections(page: &IrPage) -> Vec<PageSection> {
    if !page.sections.is_empty() {
        return page.sections.clone();
    }
    if page.blocks.is_empty() {
        return Vec::new();
    }
    let bounds = page
        .blocks
        .iter()
        .map(flow::bounds)
        .reduce(Rect::union)
        .unwrap_or_default();
    vec![PageSection {
        bounds,
        columns: vec![PageColumn {
            bounds,
            blocks: page.blocks.clone(),
        }],
        gutter_widths_pt: vec![],
        direction: Direction::Ltr,
        column_separator: false,
    }]
}
struct State {
    blocks: Vec<Value>,
    current: Option<section::Signature>,
    continuous: bool,
    breaks: usize,
    title: bool,
    force: bool,
    last_table: bool,
    need_break: bool,
}
impl State {
    fn open(
        &mut self,
        mut sig: section::Signature,
        page_start: bool,
        geo: &geometry::Geometry,
    ) -> bool {
        let Some(current) = self.current.as_ref() else {
            self.current = Some(sig);
            self.force = false;
            return false;
        };
        if !self.force && section::same(current, &sig) && !(page_start && sig.columns > 1) {
            return false;
        }
        self.force = false;
        if !page_start {
            sig.width = current.width;
            sig.height = current.height;
        }
        self.blocks.push(section::section_break(
            current,
            geo,
            self.continuous,
            self.title,
        ));
        self.title = false;
        self.breaks += 1;
        self.last_table = false;
        self.continuous = !page_start;
        if page_start {
            self.need_break = false;
        }
        self.current = Some(sig);
        true
    }
    fn emit_pins(&mut self, pins: Vec<Value>) {
        if pins.is_empty() {
            return;
        }
        if self.need_break {
            self.blocks.push(spacer(0.0, true));
        }
        self.blocks.push(image::pinned(pins));
        self.need_break = false;
        self.last_table = false;
    }
}
fn targets(pages: &[IrPage]) -> HashSet<usize> {
    fn text_targets(t: &super::super::ir::TextBlock, out: &mut HashSet<usize>) {
        for s in t.lines.iter().flat_map(|l| &l.spans) {
            if let Some(n) = s
                .href
                .as_deref()
                .and_then(|h| h.strip_prefix("#_pdfpage"))
                .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1))
            {
                out.insert(n);
            }
        }
    }
    let mut out = HashSet::new();
    for b in pages.iter().flat_map(|p| &p.blocks) {
        match b {
            PageBlock::Text(t) => text_targets(t, &mut out),
            PageBlock::Table(t) => {
                for c in t.rows.iter().flatten() {
                    for b in &c.blocks {
                        text_targets(b, &mut out);
                    }
                }
            }
            _ => (),
        }
    }
    out
}
fn dominant(pages: &[IrPage]) -> Option<String> {
    let flow: Vec<_> = pages.iter().filter(|p| !p.scanned && !p.degraded).collect();
    if flow.is_empty() {
        return None;
    }
    let mut votes: Vec<(String, usize)> = Vec::new();
    for p in &flow {
        if let Some(color) = p.background_color.as_ref().filter(|c| !c.is_empty()) {
            if let Some(v) = votes.iter_mut().find(|(c, _)| c == color) {
                v.1 += 1;
            } else {
                votes.push((color.clone(), 1));
            }
        }
    }
    let dist = |a: &str, b: &str| -> Option<i32> {
        if a.len() != 6 || b.len() != 6 || !a.is_ascii() || !b.is_ascii() {
            return None;
        }
        (0..3)
            .map(|i| {
                Some(
                    (i32::from_str_radix(&a[i * 2..i * 2 + 2], 16).ok()?
                        - i32::from_str_radix(&b[i * 2..i * 2 + 2], 16).ok()?)
                    .abs(),
                )
            })
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().max().unwrap_or(0))
    };
    let (mut best, mut cluster, mut own) = (None, 0, 0);
    for (c, n) in &votes {
        let sum = votes
            .iter()
            .filter(|(x, _)| dist(c, x).is_some_and(|d| d <= 18))
            .map(|(_, n)| *n)
            .sum();
        if sum > cluster || (sum == cluster && *n > own) {
            best = Some(c.clone());
            cluster = sum;
            own = *n;
        }
    }
    if cluster * 2 >= flow.len() {
        best
    } else {
        None
    }
}
fn notes(pages: &[IrPage]) -> Vec<Value> {
    pages
        .iter()
        .flat_map(|p| &p.footnotes)
        .map(|note| {
            let all: Vec<Vec<Value>> = note.blocks.iter().map(text::paragraph_runs).collect();
            let plain = all
                .iter()
                .map(|runs| {
                    runs.iter()
                        .filter_map(|r| r.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let rich: Vec<Vec<Value>> = all
                .into_iter()
                .map(|runs| {
                    runs.into_iter()
                        .filter(|r| {
                            r.get("noteRef").is_none()
                                && r.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| !s.is_empty())
                        })
                        .map(|r| {
                            let mut out = Map::new();
                            for key in [
                                "text",
                                "bold",
                                "italic",
                                "underline",
                                "strike",
                                "color",
                                "sizeHalfPoints",
                                "fontAscii",
                                "font",
                            ] {
                                if let Some(v) = r.get(key) {
                                    out.insert(key.into(), v.clone());
                                }
                            }
                            Value::Object(out)
                        })
                        .collect()
                })
                .collect();
            json!({"id":note.id,"text":plain,"richParas":rich})
        })
        .collect()
}

pub(super) fn assemble(
    pages: &mut [IrPage],
    slots: &[FurnitureHf],
    measure: &impl Fn(&str, &str, f64, bool, bool) -> Option<f64>,
) -> Result<(Vec<Value>, Value), String> {
    let mut geo = geometry::compute(pages);
    let mut options = Map::new();
    furniture::apply(
        slots,
        &mut geo.section,
        &mut options,
        pages.first().map_or(612.0, |p| p.width_pt),
        geo.margin_left_pt,
        geo.margin_right_pt,
        geo.margin_top_pt,
        geo.margin_bottom_pt,
    );
    let mut state = State {
        blocks: vec![],
        current: None,
        continuous: false,
        breaks: 0,
        title: options.get("titlePg") == Some(&json!(true)),
        force: false,
        last_table: false,
        need_break: false,
    };
    let mut numbering = Numbering::default();
    let links = targets(pages);
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    for page in pages.iter_mut() {
        state.need_break = page.index > 0 && !page.flows_from_previous;
        let start = state.blocks.len();
        if let Some(last) = ranges.last_mut() {
            last.2 = start;
        }
        ranges.push((page.index, start, start));
        if (page.scanned || page.degraded)
            && let Some(render) = page.render.as_ref()
        {
            let before = state.need_break;
            let closed = state.open(section::single(page), before, &geo);
            if closed && before && ranges.len() > 1 {
                let len = state.blocks.len();
                let n = ranges.len();
                ranges[n - 2].2 = len;
                ranges[n - 1].1 = len;
            }
            if state.need_break || state.last_table {
                state.blocks.push(spacer(0.0, state.need_break));
            }
            state.last_table = false;
            state
                .blocks
                .push(json!({"kind":"image","image":image::background(render,page)}));
            continue;
        }
        if page.canvas {
            state.force = true;
            let before = state.need_break;
            let closed = state.open(section::single(page), before, &geo);
            if closed && before && ranges.len() > 1 {
                let len = state.blocks.len();
                let n = ranges.len();
                ranges[n - 2].2 = len;
                ranges[n - 1].1 = len;
            }
            let mut pinned: Vec<_> = page
                .background_panels
                .iter()
                .chain(&page.decor_images)
                .chain(page.blocks.iter().filter_map(|b| {
                    if let PageBlock::Image(i) = b {
                        if i.float.is_some() { Some(i) } else { None }
                    } else {
                        None
                    }
                }))
                .collect();
            pinned.sort_by_key(|p| p.z.unwrap_or(0));
            let mut pins = Vec::new();
            if let Some(bg) = &page.background_render {
                pins.push(image::background(bg, page));
            }
            for (i, pin) in pinned.iter().enumerate() {
                pins.push(image::anchored(pin, page, Some(i as i64 + 1)));
            }
            state.emit_pins(pins);
            for b in &page.blocks {
                if flow::is_float(b) {
                    continue;
                }
                match b {
                    PageBlock::Text(t) => {
                        if t.lines.is_empty() && t.border.is_none() {
                            continue;
                        }
                        if state.need_break {
                            state.blocks.push(spacer(0.0, true));
                        }
                        state.blocks.push(text::canvas_text(t, page));
                        state.last_table = false;
                    }
                    PageBlock::Table(t) => {
                        if state.need_break {
                            state.blocks.push(spacer(0.0, true));
                        } else if state.last_table {
                            state.blocks.push(spacer(0.0, false));
                        }
                        let mut t = t.clone();
                        let pos = json!({"xTwips":jscompat::math_round(t.bounds.x0*20.0),"yTwips":jscompat::math_round((page.height_pt-t.bounds.y1)*20.0)});
                        state
                            .blocks
                            .push(table::table(&mut t, 1.0, 0.0, 0.0, Some(pos), measure)?);
                        state.last_table = true;
                    }
                    PageBlock::Image(i) => {
                        if state.need_break {
                            state.blocks.push(spacer(0.0, true));
                        }
                        state
                            .blocks
                            .push(json!({"kind":"image","image":image::anchored(i,page,None)}));
                        state.last_table = false;
                    }
                }
                state.need_break = false;
            }
            if state.need_break {
                state.blocks.push(spacer(0.0, true));
                state.last_table = false;
            }
            state.force = true;
            continue;
        }
        let mut sections = sections(page);
        let budget = budget::compute(page, &sections, &geo);
        let mut lead = budget.lead;
        let mut consumed = 0.0;
        let mut emitted = HashSet::new();
        let mut card_pad = 0.0;
        if sections.is_empty()
            && let Some(bg) = &page.background_render
        {
            let before = state.need_break;
            state.open(section::single(page), before, &geo);
            if state.need_break {
                state.blocks.push(spacer(0.0, true));
            }
            state
                .blocks
                .push(json!({"kind":"image","image":image::background(bg,page)}));
            state.last_table = false;
            state.need_break = false;
        }
        let floats: Vec<_> = sections
            .iter()
            .flat_map(|s| &s.columns)
            .flat_map(|c| &c.blocks)
            .filter_map(|b| match b {
                PageBlock::Image(i) if i.float.is_some() => Some(i.clone()),
                _ => None,
            })
            .collect();
        for (si, s) in sections.iter_mut().enumerate() {
            let before = si == 0 && state.need_break;
            let closed = state.open(section::signature(s, &geo, page), before, &geo);
            if closed && before && ranges.len() > 1 {
                let len = state.blocks.len();
                let n = ranges.len();
                ranges[n - 2].2 = len;
                ranges[n - 1].1 = len;
            }
            if si == 0 {
                let mut pinned: Vec<_> = page
                    .background_panels
                    .iter()
                    .filter(|p| p.card_id.is_none())
                    .chain(floats.iter())
                    .collect();
                pinned.sort_by_key(|p| p.z.unwrap_or(0));
                let mut pins = Vec::new();
                if let Some(bg) = &page.background_render {
                    pins.push(image::background(bg, page));
                }
                for (i, p) in pinned.iter().enumerate() {
                    pins.push(image::anchored(p, page, Some(i as i64 + 1)));
                }
                state.emit_pins(pins);
            }
            let widths = section::column_widths(s, &geo);
            let col_count = s.columns.len();
            let mut tallest = 0.0_f64;
            for (ci, c) in s.columns.iter_mut().enumerate() {
                if ci > 0 && budget.pin_columns {
                    if state.need_break {
                        state.blocks.push(spacer(0.0, true));
                        state.need_break = false;
                    }
                    state.blocks.push(column_break());
                    state.last_table = false;
                }
                let mut col = 0.0;
                let mut at_top = ci > 0 && budget.pin_columns;
                for b in &mut c.blocks {
                    if flow::is_float(b) {
                        continue;
                    }
                    if let (Some(card), PageBlock::Text(t)) = (card::card_of(b, page), &*b) {
                        let id = t.card_id.unwrap();
                        if !emitted.insert(id) {
                            continue;
                        }
                        let members: Vec<_> = page
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                PageBlock::Text(t) if t.card_id == Some(id) => Some(&**t),
                                _ => None,
                            })
                            .collect();
                        let h = card.bounds.height() * budget.height_scale;
                        let offset = s.bounds.y1 - card.bounds.y1;
                        let col_lead = if at_top && offset >= 24.0 {
                            offset * budget.height_scale
                        } else {
                            0.0
                        };
                        at_top = false;
                        let want = (card::spacing(t, card) - card_pad).max(0.0)
                            * budget.spacing_scale
                            + lead
                            + col_lead;
                        let spacing = want.min(budget.usable - consumed - col - h).max(0.0);
                        state.blocks.push(card::save(
                            card,
                            id,
                            &members,
                            state.need_break,
                            spacing,
                            budget.height_scale,
                            page,
                        )?);
                        card_pad = (members
                            .iter()
                            .map(|t| t.bounds.y0)
                            .fold(f64::INFINITY, f64::min)
                            - card.bounds.y0)
                            .max(0.0);
                        state.last_table = false;
                        state.need_break = false;
                        lead = 0.0;
                        col += spacing + h;
                        continue;
                    }
                    let h = flow::height(b, &geo, budget.height_scale);
                    let offset = s.bounds.y1 - flow::bounds(b).y1;
                    let column_lead = if at_top && offset >= 24.0 {
                        offset * budget.height_scale
                    } else {
                        0.0
                    };
                    at_top = false;
                    let measured = (flow::spacing(b) - card_pad).max(0.0);
                    card_pad = 0.0;
                    let surplus = if budget.pay_surplus && col_count == 1 {
                        flow::surplus(b)
                    } else {
                        0.0
                    };
                    let want = (measured * budget.spacing_scale).min((measured - surplus).max(0.0))
                        + lead
                        + column_lead;
                    let spacing = want.min(budget.usable - consumed - col - h).max(0.0);
                    let base = if col_count > 1 && ci > 0 {
                        c.bounds.x0
                    } else {
                        geo.margin_left_pt
                    };
                    match b {
                        PageBlock::Text(t) => {
                            let style = leader::style(t);
                            let width = widths.get(ci).copied().unwrap_or(geo.content_width_pt);
                            if t.toc_entry.is_none() && style.is_none() {
                                let strict = t.lines.iter().any(|l| l.hard_break_before);
                                fit::preflight_fit_block(t, width, strict, measure);
                            }
                            let result = if t.toc_entry.is_some() || style.is_some() {
                                leader::save(
                                    t,
                                    style,
                                    base,
                                    width,
                                    state.need_break,
                                    spacing,
                                    budget.height_scale,
                                )
                            } else {
                                text::text_block(
                                    t,
                                    state.need_break,
                                    spacing,
                                    page.index,
                                    &mut numbering,
                                    budget.height_scale,
                                )
                            };
                            state.blocks.push(result);
                            state.last_table = false;
                        }
                        PageBlock::Table(t) => {
                            if state.need_break {
                                state.blocks.push(spacer(spacing, true));
                            } else if state.last_table || spacing >= 2.0 {
                                state.blocks.push(spacer(spacing, false));
                            }
                            let width: f64 = t.col_widths_pt.iter().sum();
                            let indent = (t.bounds.x0 - base)
                                .max(0.0)
                                .min((geo.content_width_pt - width).max(0.0));
                            let shave = if h >= 0.93 * budget.usable { 12.0 } else { 0.0 };
                            state.blocks.push(table::table(
                                t,
                                budget.height_scale,
                                indent,
                                shave,
                                None,
                                measure,
                            )?);
                            state.last_table = true;
                        }
                        PageBlock::Image(i) => {
                            let mut gap = spacing;
                            if state.need_break {
                                state.blocks.push(spacer(gap, true));
                                gap = 0.0;
                            }
                            state.blocks.push(image::inline(
                                i,
                                &geo,
                                page.width_pt,
                                gap,
                                budget.height_scale,
                            ));
                            state.last_table = false;
                        }
                    }
                    state.need_break = false;
                    lead = 0.0;
                    col += spacing + h;
                }
                tallest = tallest.max(col);
            }
            consumed += tallest;
        }
        if state.need_break {
            state.blocks.push(spacer(0.0, true));
            state.last_table = false;
        } else if state.blocks.len() == start {
            state.open(section::single(page), false, &geo);
            state.blocks.push(spacer(0.0, false));
        }
    }
    if let Some(last) = ranges.last_mut() {
        last.2 = state.blocks.len();
    }
    if state.last_table {
        state.blocks.push(spacer(0.0, false));
    }
    for &(index, start, end) in ranges.iter().rev() {
        if !links.contains(&index) {
            continue;
        }
        let name = format!("_pdfpage{}", index + 1);
        if let Some(block) = state.blocks[start..end]
            .iter_mut()
            .find(|b| b.get("kind") == Some(&json!("generated")))
        {
            let generated = &mut block["block"];
            if generated.get("hiddenBookmarks").is_none() {
                generated["hiddenBookmarks"] = json!([]);
            }
            generated["hiddenBookmarks"]
                .as_array_mut()
                .unwrap()
                .push(json!(name));
        } else {
            let mut holder = spacer(0.0, false);
            holder["block"]["hiddenBookmarks"] = json!([name]);
            state.blocks.insert(start, holder);
        }
    }
    let final_sig = state.current.unwrap_or_else(|| {
        section::single(pages.first().unwrap_or(&IrPage {
            width_pt: 612.0,
            height_pt: 792.0,
            ..Default::default()
        }))
    });
    geo.section
        .insert("pageWidth".into(), json!(final_sig.width));
    geo.section
        .insert("pageHeight".into(), json!(final_sig.height));
    geo.section.insert(
        "orientation".into(),
        json!(if final_sig.width > final_sig.height {
            "landscape"
        } else {
            "portrait"
        }),
    );
    if final_sig.columns > 1 {
        geo.section
            .insert("columns".into(), json!(final_sig.columns));
        geo.section
            .insert("colSpace".into(), json!(final_sig.space));
        if let Some(widths) = final_sig.widths {
            geo.section.insert("colWidths".into(), json!(widths));
        }
        if final_sig.bidi {
            geo.section.insert("bidi".into(), json!(true));
        }
    }
    options.insert("section".into(), Value::Object(geo.section));
    if state.breaks > 0 {
        options.insert(
            "sectionStartType".into(),
            json!(if state.continuous {
                "continuous"
            } else {
                "nextPage"
            }),
        );
    }
    if let Some(n) = numbering.options() {
        options.insert("numbering".into(), n);
    }
    let footnotes = notes(pages);
    if !footnotes.is_empty() {
        options.insert("footnotes".into(), json!(footnotes));
    }
    if let Some(color) = dominant(pages) {
        options.insert("pageColor".into(), json!(color));
    }
    if !state.title {
        options.remove("titlePg");
    }
    Ok((state.blocks, Value::Object(options)))
}

#[cfg(test)]
mod tests {
    use super::super::super::ir::{Alignment, Footnote, Line, Span, TextBlock};
    use super::super::super::script::Script;
    use super::*;

    fn text(value: &str, href: Option<&str>) -> TextBlock {
        let bounds = Rect {
            x0: 72.0,
            y0: 690.0,
            x1: 180.0,
            y1: 710.0,
        };
        let span = Span {
            text: value.into(),
            bounds,
            font_size: 12.0,
            font_family: "Arial".into(),
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
            href: href.map(str::to_owned),
        };
        TextBlock {
            lines: vec![Line {
                spans: vec![span],
                bounds,
                baseline: 690.0,
                ends_with_hyphen: false,
                hard_break_before: false,
            }],
            bounds,
            align: Alignment::Left,
            first_line_indent_pt: 0.0,
            direction: Direction::Ltr,
            list: None,
            stitched_from_line: None,
            toc_entry: None,
            spacing_before_pt: None,
            border: None,
            card_id: None,
        }
    }
    #[test]
    fn page_links_and_footnotes_are_emitted_as_real_save_content() {
        let mut pages = vec![
            IrPage {
                width_pt: 612.0,
                height_pt: 792.0,
                blocks: vec![PageBlock::Text(Box::new(text("first", Some("#_pdfpage2"))))],
                footnotes: vec![Footnote {
                    id: "1".into(),
                    marker: None,
                    blocks: vec![text("note", None)],
                }],
                ..Default::default()
            },
            IrPage {
                index: 1,
                width_pt: 612.0,
                height_pt: 792.0,
                blocks: vec![PageBlock::Text(Box::new(text("second", None)))],
                ..Default::default()
            },
        ];
        let (blocks, options) = assemble(&mut pages, &[], &|_, _, _, _, _| None).unwrap();
        assert!(blocks.iter().any(|b| {
            b["block"]["hiddenBookmarks"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("_pdfpage2")))
        }));
        assert_eq!(options["footnotes"][0]["text"], "note");
        assert!(blocks.iter().any(|b| {
            b["block"]["runs"]
                .as_array()
                .is_some_and(|runs| runs.iter().any(|r| r["link"]["href"] == "#_pdfpage2"))
        }));
    }
}
