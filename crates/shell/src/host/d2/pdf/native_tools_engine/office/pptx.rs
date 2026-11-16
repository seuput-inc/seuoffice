//! Rebuild analyzed PDF pages with the native PowerPoint engine.
use super::geometry::{Rect, rect_union_all};
use super::ir::{
    Alignment, Fill, ImageBlock, IrPage, PageBlock, PageRender, TableBlock, TableCell, TextBlock,
};
use super::pptx_text::text_block_paragraph;
use jscompat::{JsNum, JsObject, math_round};
use pptx_engine::deck::{
    OpenedPptx, append_raw_elements, insert_blank_slide, open_pptx, save_pptx, set_slide_size,
};
use pptx_engine::insert::{
    self, BodyWrap, BorderScope, CellAnchor, NewElementBodyPr, NewElementOptions,
    NewPictureOptions, NewTableBorder, NewTableCellSpec, NewTableGridOptions,
};
use pptx_engine::types::{EmuRect, Insets, Slide};

const EMU: f64 = 12700.0;
fn now_ms() -> Result<f64, String> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as f64)
}
struct Mapper {
    scale: f64,
    ox: f64,
    oy: f64,
    height: f64,
}
impl Mapper {
    fn new(page: &IrPage, w: f64, h: f64) -> Self {
        let scale = (w / page.width_pt.max(1.0)).min(h / page.height_pt.max(1.0));
        Self {
            scale,
            ox: (w - page.width_pt * scale) / 2.0,
            oy: (h - page.height_pt * scale) / 2.0,
            height: page.height_pt,
        }
    }
    fn len(&self, pt: f64) -> JsNum {
        JsNum(math_round(pt * self.scale * EMU))
    }
    fn rect(&self, b: Rect) -> EmuRect {
        EmuRect {
            x: JsNum(math_round((self.ox + b.x0 * self.scale) * EMU)),
            y: JsNum(math_round(
                (self.oy + (self.height - b.y1) * self.scale) * EMU,
            )),
            cx: JsNum(math_round(b.width() * self.scale * EMU).max(1.0)),
            cy: JsNum(math_round(b.height() * self.scale * EMU).max(1.0)),
        }
    }
}
fn options(kind: &str, offset: EmuRect) -> NewElementOptions {
    NewElementOptions {
        kind: kind.into(),
        offset,
        paragraphs: None,
        fill_color: None,
        stroke: None,
        body_pr: None,
        adjustments: None,
        flip_h: None,
        flip_v: None,
    }
}
fn add_shape(slide: &mut Slide, opts: &NewElementOptions) -> Result<(), String> {
    insert::add_element(slide, opts, &jscompat::random_uuid(), now_ms()?)?;
    Ok(())
}
fn add_picture(
    opened: &mut OpenedPptx,
    index: usize,
    data: &[u8],
    mime: &str,
    offset: EmuRect,
) -> Result<(), String> {
    let opts = NewPictureOptions {
        bytes: data.to_vec(),
        ext: if mime == "image/jpeg" { "jpg" } else { "png" }.into(),
        offset,
        name: None,
        descr: None,
    };
    insert::add_picture(opened, index, &opts, &jscompat::random_uuid(), now_ms()?)?
        .ok_or("PDF image could not be inserted in presentation")?;
    Ok(())
}
fn full_page(
    opened: &mut OpenedPptx,
    index: usize,
    img: &PageRender,
    w: f64,
    h: f64,
) -> Result<(), String> {
    add_picture(
        opened,
        index,
        &img.data,
        &img.mime,
        EmuRect {
            x: JsNum(0.0),
            y: JsNum(0.0),
            cx: JsNum(math_round(w * EMU)),
            cy: JsNum(math_round(h * EMU)),
        },
    )
}
fn image(
    opened: &mut OpenedPptx,
    index: usize,
    image: &ImageBlock,
    mapper: &Mapper,
) -> Result<(), String> {
    add_picture(
        opened,
        index,
        &image.data,
        &image.mime,
        mapper.rect(image.bounds),
    )
}
fn add_text(slide: &mut Slide, block: &TextBlock, map: &Mapper) -> Result<(), String> {
    if !block.lines.is_empty() {
        let pad = 4.0_f64.max(block.bounds.width().max(1.0) * map.scale * 0.04);
        let mut offset = map.rect(block.bounds);
        let pad_emu = math_round(pad * EMU);
        match block.align {
            Alignment::Center => offset.x.0 -= math_round(pad_emu / 2.0),
            Alignment::Right => offset.x.0 -= pad_emu,
            _ => {}
        }
        offset.cx.0 += pad_emu;
        let mut opts = options("textbox", offset);
        opts.paragraphs = Some(vec![text_block_paragraph(block, map.scale, true)]);
        opts.body_pr = Some(NewElementBodyPr {
            wrap: Some(BodyWrap::None),
            anchor: Some(CellAnchor::T),
            insets_emu: Some(Insets {
                l: JsNum(0.0),
                t: JsNum(0.0),
                r: JsNum(0.0),
                b: JsNum(0.0),
            }),
            auto_fit: None,
        });
        add_shape(slide, &opts)?;
    }
    if let Some(border) = &block.border {
        let b = block.bounds;
        let rect = if border.side == "left" {
            let x1 = b.x0 - border.space_pt;
            Rect {
                x0: x1 - border.width_pt,
                x1,
                y0: b.y0,
                y1: b.y1,
            }
        } else {
            let x0 = b.x0 + border.indent_left_pt.unwrap_or(0.0);
            let x1 = (b.x1 - border.indent_right_pt.unwrap_or(0.0)).max(x0 + 1.0);
            let y = if border.side == "top" {
                b.y1 + border.space_pt
            } else {
                b.y0 - border.space_pt
            };
            Rect {
                x0,
                x1,
                y0: y - border.width_pt / 2.0,
                y1: y + border.width_pt / 2.0,
            }
        };
        let mut opts = options("rect", map.rect(rect));
        opts.fill_color = Some(format!("#{}", border.color));
        add_shape(slide, &opts)?;
    }
    Ok(())
}
fn col_starts(row: &[TableCell]) -> Vec<usize> {
    let mut col = 0;
    row.iter()
        .map(|c| {
            let start = col;
            col += c.grid_span.max(1);
            start
        })
        .collect()
}
fn table_options(table: &TableBlock, map: &Mapper) -> NewTableGridOptions {
    let tops: Vec<f64> = table
        .rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            row.iter()
                .find(|c| c.v_merge.as_deref() != Some("continue"))
                .or_else(|| row.first())
                .map_or(table.bounds.y1 - r as f64, |c| c.bounds.y1)
        })
        .collect();
    let heights = tops
        .iter()
        .enumerate()
        .map(|(r, top)| {
            map.len((top - tops.get(r + 1).copied().unwrap_or(table.bounds.y0)).max(1.0))
        })
        .collect();
    let mut rows = Vec::new();
    for (r, row) in table.rows.iter().enumerate() {
        let starts = col_starts(row);
        let mut specs = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            let span = cell.grid_span.max(1);
            if cell.v_merge.as_deref() == Some("continue") {
                specs.push(NewTableCellSpec {
                    v_merge: Some(true),
                    ..Default::default()
                });
                for _ in 1..span {
                    specs.push(NewTableCellSpec {
                        v_merge: Some(true),
                        h_merge: Some(true),
                        ..Default::default()
                    });
                }
                continue;
            }
            let mut spec = NewTableCellSpec::default();
            if span > 1 {
                spec.grid_span = Some(JsNum(span as f64));
            }
            if cell.v_merge.as_deref() == Some("restart") {
                let mut count = 1;
                for next in table.rows.iter().skip(r + 1) {
                    let starts2 = col_starts(next);
                    let Some(pos) = starts2.iter().position(|&n| n == starts[i]) else {
                        break;
                    };
                    if next[pos].v_merge.as_deref() != Some("continue") {
                        break;
                    }
                    count += 1;
                }
                if count > 1 {
                    spec.row_span = Some(JsNum(count as f64));
                }
            }
            spec.fill_color = cell
                .fill
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("#{s}"));
            spec.anchor = cell.v_align.as_ref().map(|s| {
                if s == "center" {
                    CellAnchor::Ctr
                } else {
                    CellAnchor::B
                }
            });
            if !cell.blocks.is_empty() {
                spec.paragraphs = Some(
                    cell.blocks
                        .iter()
                        .map(|b| text_block_paragraph(b, map.scale, false))
                        .collect(),
                );
                let ink = rect_union_all(&cell.blocks.iter().map(|b| b.bounds).collect::<Vec<_>>());
                let anchored = spec.anchor.is_some();
                spec.margins_emu = Some(Insets {
                    l: map.len((ink.x0 - cell.bounds.x0).max(0.0)),
                    t: if anchored {
                        JsNum(0.0)
                    } else {
                        map.len((cell.bounds.y1 - ink.y1).max(0.0))
                    },
                    r: map.len((cell.bounds.x1 - ink.x1).max(0.0)),
                    b: if anchored {
                        JsNum(0.0)
                    } else {
                        map.len((ink.y0 - cell.bounds.y0).max(0.0))
                    },
                });
            } else {
                spec.margins_emu = Some(Insets {
                    l: JsNum(0.0),
                    t: JsNum(0.0),
                    r: JsNum(0.0),
                    b: JsNum(0.0),
                });
            }
            specs.push(spec);
            for _ in 1..span {
                specs.push(NewTableCellSpec {
                    h_merge: Some(true),
                    ..Default::default()
                });
            }
        }
        rows.push(specs);
    }
    let border = if table.confidence.is_none() {
        Some(NewTableBorder {
            color: format!("#{}", table.border_color.as_deref().unwrap_or("000000")),
            width_emu: map.len(0.75),
            scope: Some(BorderScope::All),
        })
    } else if table.separator_rule.is_some() {
        Some(NewTableBorder {
            color: format!("#{}", table.border_color.as_deref().unwrap_or("000000")),
            width_emu: map.len(1.0),
            scope: Some(BorderScope::InsideV),
        })
    } else {
        None
    };
    NewTableGridOptions {
        offset: map.rect(table.bounds),
        col_widths_emu: table.col_widths_pt.iter().map(|v| map.len(*v)).collect(),
        row_heights_emu: heights,
        cells: rows,
        border,
    }
}
fn fill(slide: &mut Slide, f: &Fill, mapper: &Mapper, curved: bool) -> Result<(), String> {
    let kind = if curved {
        f.geometry.as_deref().unwrap_or("roundRect")
    } else {
        "rect"
    };
    let mut opts = options(kind, mapper.rect(f.bounds));
    let alpha = f.alpha.map(|v| format!("{v:02x}")).unwrap_or_default();
    opts.fill_color = Some(format!("#{}{alpha}", f.color));
    if kind == "roundRect" {
        if let Some(radius) = f.corner_radius_pt {
            let shortest = f.bounds.width().min(f.bounds.height()).max(1.0);
            let mut adjust = JsObject::default();
            adjust.set(
                "adj",
                JsNum(math_round((radius / shortest * 100000.0).min(50000.0))),
            );
            opts.adjustments = Some(adjust);
        }
    }
    add_shape(slide, &opts)
}
fn emit_page(
    opened: &mut OpenedPptx,
    index: usize,
    page: &IrPage,
    w: f64,
    h: f64,
) -> Result<(), String> {
    let mapper = Mapper::new(page, w, h);
    if page.scanned || page.degraded {
        if let Some(img) = &page.render {
            full_page(opened, index, img, w, h)?;
        }
        return Ok(());
    }
    if let Some(color) = page.background_color.as_ref().filter(|v| !v.is_empty()) {
        let mut opts = options(
            "rect",
            EmuRect {
                x: JsNum(0.0),
                y: JsNum(0.0),
                cx: JsNum(math_round(w * EMU)),
                cy: JsNum(math_round(h * EMU)),
            },
        );
        opts.fill_color = Some(format!("#{color}"));
        add_shape(&mut opened.deck.slides[index], &opts)?;
    }
    if let Some(bg) = &page.background_render {
        full_page(opened, index, bg, w, h)?;
    }
    enum Underlay<'a> {
        Image(&'a ImageBlock),
        Fill(&'a Fill, bool),
    }
    let mut underlays: Vec<(i64, usize, usize, Underlay<'_>)> = Vec::new();
    let mut order = 0;
    for img in page.background_panels.iter().chain(&page.decor_images) {
        underlays.push((img.z.unwrap_or(0), 0, order, Underlay::Image(img)));
        order += 1;
    }
    for block in &page.blocks {
        if let PageBlock::Image(img) = block {
            if img.float.as_ref().is_some_and(|f| f.wrap == "behind") {
                underlays.push((img.z.unwrap_or(0), 0, order, Underlay::Image(img)));
                order += 1;
            }
        }
    }
    if let Some(shapes) = &page.shapes {
        for f in &shapes.fills {
            underlays.push((
                f.z.unwrap_or(0),
                f.sequence.unwrap_or(0),
                order,
                Underlay::Fill(f, false),
            ));
            order += 1;
        }
        for f in &shapes.curved_fills {
            underlays.push((
                f.z.unwrap_or(0),
                f.sequence.unwrap_or(0),
                order,
                Underlay::Fill(f, true),
            ));
            order += 1;
        }
    }
    underlays.sort_by_key(|(z, seq, ord, _)| (*z, *seq, *ord));
    for (_, _, _, u) in underlays {
        match u {
            Underlay::Image(img) => image(opened, index, img, &mapper)?,
            Underlay::Fill(f, curved) => fill(&mut opened.deck.slides[index], f, &mapper, curved)?,
        }
    }
    for block in &page.blocks {
        match block {
            PageBlock::Image(img) => {
                if !img.float.as_ref().is_some_and(|f| f.wrap == "behind") {
                    image(opened, index, img, &mapper)?;
                }
            }
            PageBlock::Text(text) => add_text(&mut opened.deck.slides[index], text, &mapper)?,
            PageBlock::Table(table) => {
                let opts = table_options(table, &mapper);
                let xml = insert::build_table_grid_xml(
                    &opened.deck.slides[index],
                    &opts,
                    &jscompat::random_uuid(),
                );
                append_raw_elements(opened, index, &[xml])?
                    .ok_or("PDF table could not be inserted in presentation")?;
            }
        }
    }
    for note in &page.footnotes {
        for block in &note.blocks {
            add_text(&mut opened.deck.slides[index], block, &mapper)?;
        }
    }
    Ok(())
}
pub(super) fn rebuild(pages: &[IrPage]) -> Result<Vec<u8>, String> {
    let mut opened = open_pptx(&pptx_engine::blank::create_blank_pptx(now_ms()?)?)?;
    let (w, h) = pages
        .first()
        .map_or((720.0, 540.0), |p| (p.width_pt, p.height_pt));
    if !set_slide_size(&mut opened, math_round(w * EMU), math_round(h * EMU))? {
        return Err("could not set presentation size".into());
    }
    for i in 1..pages.len() {
        insert_blank_slide(&mut opened, (i - 1) as f64)?
            .ok_or("could not add presentation slide")?;
    }
    for (i, page) in pages.iter().enumerate() {
        emit_page(&mut opened, i, page, w, h)?;
    }
    save_pptx(&mut opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonuniform_pages_are_letterboxed_and_slides_reopen() {
        let first = IrPage {
            index: 0,
            width_pt: 720.0,
            height_pt: 540.0,
            ..Default::default()
        };
        let second = IrPage {
            index: 1,
            width_pt: 360.0,
            height_pt: 540.0,
            ..Default::default()
        };
        let mapper = Mapper::new(&second, 720.0, 540.0);
        let rect = mapper.rect(Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 360.0,
            y1: 540.0,
        });
        assert_eq!(rect.x.0, 180.0 * EMU);
        let bytes = rebuild(&[first, second]).unwrap();
        let deck = open_pptx(&bytes).unwrap();
        assert_eq!(deck.deck.slides.len(), 2);
        assert_eq!(deck.deck.size.cx.0, 720.0 * EMU);
    }

    #[test]
    fn vertical_and_horizontal_merges_keep_grid_shape() {
        let cell = |y0, y1, merge| TableCell {
            bounds: Rect {
                x0: 0.0,
                y0,
                x1: 100.0,
                y1,
            },
            grid_span: 2,
            v_merge: merge,
            fill: None,
            v_align: None,
            soft_edges: None,
            blocks: Vec::new(),
        };
        let table = TableBlock {
            bounds: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0,
                y1: 40.0,
            },
            col_widths_pt: vec![50.0, 50.0],
            rows: vec![
                vec![cell(20.0, 40.0, Some("restart".into()))],
                vec![cell(0.0, 20.0, Some("continue".into()))],
            ],
            confidence: None,
            border_color: None,
            form: false,
            separator_rule: None,
            spacing_before_pt: None,
        };
        let page = IrPage {
            width_pt: 100.0,
            height_pt: 40.0,
            ..Default::default()
        };
        let opts = table_options(&table, &Mapper::new(&page, 100.0, 40.0));
        assert_eq!(opts.cells[0][0].grid_span.unwrap().0, 2.0);
        assert_eq!(opts.cells[0][0].row_span.unwrap().0, 2.0);
        assert_eq!(opts.cells[1][1].v_merge, Some(true));
        assert_eq!(opts.cells[1][1].h_merge, Some(true));
    }
}
