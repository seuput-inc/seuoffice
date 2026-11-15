//! Reconstruct inter-block flow gaps without charging floats or micro text.
use super::super::ir::{PageBlock, PageSection};

fn bounds(block: &PageBlock) -> super::super::geometry::Rect {
    match block { PageBlock::Text(b) => b.bounds, PageBlock::Table(b) => b.bounds, PageBlock::Image(b) => b.bounds }
}
pub(super) fn is_micro_text(block: &PageBlock) -> bool {
    matches!(block, PageBlock::Text(t) if !t.lines.is_empty() && t.lines.iter().all(|l| l.spans.iter().all(|s| s.font_size < 2.5)))
}
fn is_float(block: &PageBlock) -> bool { matches!(block, PageBlock::Image(i) if i.float.is_some()) }
fn set_spacing(block: &mut PageBlock, spacing: f64) {
    match block { PageBlock::Text(b) => b.spacing_before_pt = Some(spacing), PageBlock::Table(b) => b.spacing_before_pt = Some(spacing), PageBlock::Image(b) => b.spacing_before_pt = Some(spacing) }
}
pub(super) fn apply(sections: &mut [PageSection]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut previous_section_bottom: Option<f64> = None;
    for section in sections {
        for (ci, column) in section.columns.iter_mut().enumerate() {
            let mut previous = if ci == 0 { previous_section_bottom } else { None };
            for block in &mut column.blocks {
                if is_float(block) || is_micro_text(block) { continue; }
                let rect = bounds(block);
                if let Some(bottom) = previous {
                    let mut gap = bottom - rect.y1;
                    if gap.is_finite() {
                        if gap < -1.0 { warnings.push(format!("overlapping blocks: negative gap {gap:.1}pt clamped to 0")); gap = 0.0; }
                        if gap >= 2.0 { set_spacing(block, gap.min(1584.0)); }
                    }
                }
                previous = Some(rect.y0);
            }
        }
        previous_section_bottom = Some(section.bounds.y0);
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::geometry::Rect;
    use super::super::super::ir::{Direction,FloatPlacement,ImageBlock,PageColumn};
    fn image(y0:f64,y1:f64,float:bool)->PageBlock {
        PageBlock::Image(ImageBlock{bounds:Rect{x0:0.0,y0,x1:20.0,y1},data:vec![],mime:"image/png".into(),pixel_width:1,pixel_height:1,float:float.then(||FloatPlacement{wrap:"behind".into(),x_offset_pt:0.0}),spacing_before_pt:None,z:None,synthetic:false,card_id:None})
    }
    #[test]
    fn floats_do_not_interrupt_spacing_and_overlaps_warn() {
        let bounds=Rect{x0:0.0,y0:0.0,x1:100.0,y1:100.0};
        let mut sections=vec![PageSection{bounds,columns:vec![PageColumn{bounds,blocks:vec![image(80.0,90.0,false),image(70.0,75.0,true),image(60.0,65.0,false),image(50.0,62.0,false)]}],gutter_widths_pt:vec![],direction:Direction::Ltr,column_separator:false}];
        let warnings=apply(&mut sections);
        let PageBlock::Image(second)=&sections[0].columns[0].blocks[2] else{panic!("expected image");};
        assert_eq!(second.spacing_before_pt,Some(15.0));
        assert_eq!(warnings.len(),1);
    }
}
