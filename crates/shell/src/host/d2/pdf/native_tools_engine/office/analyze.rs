//! Native page analysis for PDF to editable Office conversion.
mod blocks;
mod canvas;
mod cards;
mod columns;
mod confidence;
mod decor;
mod floats;
mod footnotes;
mod form;
mod frames;
mod furniture;
mod layout;
mod lists;
mod panels;
mod pictures;
mod shapes;
mod spacing;
mod stream;
mod styling;
mod table;
mod toc;
mod turned;
mod units;
mod zones;
pub(super) mod vector;

use std::collections::HashSet;
use super::geometry::{median,Rect};
use super::ir::{Direction,ExtractedPage,FurnitureHf,IrPage,PageBlock,PageColumn,PageSection};
use super::{chars,lines,rtl};
pub(super) const PAGE_CONFIDENCE_MIN:f64=confidence::PAGE_CONFIDENCE_MIN;

fn background(page:&mut IrPage, fill:&super::ir::Fill)->Result<usize,String>{let index=page.background_panels.len();page.background_panels.push(pictures::solid(fill.bounds,&fill.color,fill.alpha.unwrap_or(255),fill.z)?);Ok(index)}
fn body_bounds(page:&ExtractedPage)->Rect{Rect{x0:0.0,y0:0.0,x1:page.width_pt,y1:page.height_pt}}
fn remove_cell_overlays(images:&mut Vec<super::ir::ImageBlock>,tables:&[super::ir::TableBlock]){images.retain(|img|{let(x,y)=(img.bounds.center_x(),img.bounds.center_y());for t in tables.iter().filter(|t|t.confidence.is_none()){if x<t.bounds.x0||x>t.bounds.x1||y<t.bounds.y0||y>t.bounds.y1{continue;}for cell in t.rows.iter().flatten(){let b=cell.bounds;if cell.v_merge.as_deref()==Some("continue")||x<b.x0||x>b.x1||y<b.y0||y>b.y1{continue;}let overlap:f64=cell.blocks.iter().flat_map(|b|&b.lines).map(|l|img.bounds.intersection_area(l.bounds)).sum();return overlap<0.35*img.bounds.area().max(1.0);}}true});}
fn inside_lattice(img:&super::ir::ImageBlock,tables:&[super::ir::TableBlock])->bool{let(x,y)=(img.bounds.center_x(),img.bounds.center_y());tables.iter().any(|t|t.confidence.is_none()&&x>=t.bounds.x0&&x<=t.bounds.x1&&y>=t.bounds.y0&&y<=t.bounds.y1)}
fn one_page(extracted:&mut ExtractedPage,absolute:bool,list_seq:&mut lists::ListSeq)->Result<IrPage,String>{
    let mut page=IrPage{index:extracted.index,width_pt:extracted.width_pt,height_pt:extracted.height_pt,rotation:extracted.rotation,degraded:extracted.degraded,scanned:extracted.scanned,has_struct_tree:extracted.has_struct_tree,degraded_reason:extracted.degraded_reason.clone(),render:extracted.render.clone(),..Default::default()};
    if extracted.scanned||extracted.degraded{return Ok(page);}
    chars::dedupe_double_drawn(&mut extracted.chars);chars::calibrate_font_sizes(&mut extracted.chars);chars::normalize_regional_fonts(&mut extracted.chars);chars::normalize_cjk_dashes(&mut extracted.chars);
    let mut shapes=shapes::normalize_shapes(&extracted.paths,extracted.cell_data);
    if let Some(color)=shapes::extract_page_background(&mut shapes.fills,extracted.width_pt,extracted.height_pt,extracted.content_box){if !shapes::is_near_white(&color){page.background_color=Some(color);}}
    let tiles=shapes::extract_full_bleed_tiles(&mut shapes,extracted.chars.len(),extracted.width_pt,extracted.height_pt);for tile in &tiles{background(&mut page,tile)?;}
    let boxes:Vec<_>=extracted.chars.iter().map(|c|c.bounds).collect();let panels=shapes::extract_background_panels(&mut shapes.fills,&boxes,extracted.width_pt,extracted.height_pt,extracted.content_box);if !panels.is_empty(){page.background_panels.clear();for fill in &panels{background(&mut page,fill)?;}}
    page.background_render=extracted.background_render.clone();page.decor_images=extracted.decor_images.clone();for image in &mut page.decor_images{image.float=Some(super::ir::FloatPlacement{wrap:"behind".into(),x_offset_pt:image.bounds.x0});}
    let noted=footnotes::detect_footnotes(&extracted.chars,&mut shapes,extracted.index,extracted.width_pt,extracted.height_pt)?;page.footnotes=noted.footnotes;
    let grid_boxes:Vec<_>=table::solve_page_grids(&shapes).iter().map(|g|g.bounds).collect();
    let curved=shapes.curved_fills.clone();let backdrop=shapes::extract_text_backdrops(&mut shapes.fills,&noted.body_chars,&grid_boxes,extracted.width_pt,extracted.height_pt,&curved,page.background_render.is_some()||page.background_color.is_some()||!page.background_panels.is_empty());
    let mut card_candidates=Vec::new();for fill in &backdrop{let index=background(&mut page,fill)?;let rounded=curved.iter().any(|f|f.sequence==fill.sequence&&f.bounds==fill.bounds&&f.color==fill.color);if fill.alpha.is_none(){card_candidates.push(cards::Candidate{panel_index:index,bounds:fill.bounds,color:fill.color.clone(),rounded,z:fill.z});}}
    shapes.curved_fills.retain(|f|!backdrop.iter().any(|b|b.sequence==f.sequence&&b.bounds==f.bounds&&b.color==f.color));
    for frame in frames::extract_empty_frames(&mut shapes.strokes,&noted.body_chars,&grid_boxes){page.background_panels.push(pictures::frame(frame.bounds,&frame.color,frame.width_pt)?);}
    let styled=styling::apply(&noted.body_chars,&shapes,&grid_boxes,extracted.width_pt*extracted.height_pt);
    let build_blocks=|chars:&[super::ir::PdfChar]|blocks::group_into_blocks(&chars::analyze_chars(chars),None,blocks::BlockOptions::default());
    let lattice=table::detect_tables(&shapes,&styled.chars,Some(extracted.height_pt),Some(extracted.width_pt),&build_blocks)?;
    let (upright,turned)=turned::split(&lattice.remaining_chars);let normalized=rtl::normalize_arabic_forms(&upright);let clustered=lines::cluster_combining_marks(&normalized);let raw=lines::group_into_lines(&clustered);let all_units=units::split_into_units(&raw);
    let lattice_boxes:Vec<_>=lattice.tables.iter().map(|t|t.bounds).collect();let forms=form::detect_form_tables(&all_units,&shapes,&lattice_boxes,&build_blocks)?;let (toc_blocks,after_toc)=toc::detect_toc_rows(&forms.remaining_units);
    let mut exclusions=lattice_boxes.clone();exclusions.extend(forms.tables.iter().map(|t|t.bounds));let zone=zones::detect_rule_separated_zones(&after_toc,&shapes,&exclusions,extracted.width_pt,extracted.height_pt);exclusions.extend(zone.tables.iter().map(|t|t.bounds));
    let stream=stream::detect_stream_tables(&zone.remaining_units,&shapes,&exclusions,extracted.width_pt>extracted.height_pt,extracted.cell_data)?;let mut stream_tables=stream.tables;let mut remaining=stream.remaining_units;
    if absolute{let weak:Vec<_>=stream_tables.iter().filter(|t|t.confidence.unwrap_or(1.0)<=0.7).map(|t|t.bounds).collect();if !weak.is_empty(){stream_tables.retain(|t|t.confidence.unwrap_or(1.0)>0.7);let kept:HashSet<_>=remaining.iter().map(|u|u.id).collect();remaining=zone.remaining_units.iter().filter(|u|kept.contains(&u.id)||weak.iter().any(|b|u.bounds.center_x()>=b.x0&&u.bounds.center_x()<=b.x1&&u.bounds.center_y()>=b.y0&&u.bounds.center_y()<=b.y1)).cloned().collect();}}
    let images:Vec<_>=extracted.images.iter().filter(|i|i.bounds.width()>=2.5||i.bounds.height()>=2.5).cloned().collect();let images=floats::suppress_text_shadow_images(&images,&remaining);let(mut floats,mut inline)=floats::classify(&images,&remaining,Some(extracted.width_pt*extracted.height_pt),Some(body_bounds(extracted)));
    if extracted.width_pt>extracted.height_pt{for mut img in inline.drain(..){img.float=Some(super::ir::FloatPlacement{wrap:"behind".into(),x_offset_pt:img.bounds.x0});floats.push(img);}}
    let (mut tables,panel_notes)=panels::merge_side_by_side_panels(lattice.tables);tables.extend(forms.tables.iter().cloned());tables.extend(zone.tables.iter().cloned());tables.extend(stream_tables.iter().cloned());remove_cell_overlays(&mut floats,&tables);remove_cell_overlays(&mut inline,&tables);
    let mut keep=Vec::new();for mut img in inline{if inside_lattice(&img,&tables){img.float=Some(super::ir::FloatPlacement{wrap:"behind".into(),x_offset_pt:img.bounds.x0});floats.push(img);}else{keep.push(img);}}let inline=keep;
    let mut other:Vec<PageBlock>=tables.iter().cloned().map(PageBlock::Table).collect();other.extend(toc_blocks.iter().cloned().map(PageBlock::Text));other.extend(inline.iter().cloned().map(PageBlock::Image));
    let mut elements:Vec<columns::SectionElement<'_>>=remaining.iter().map(|u|columns::SectionElement{bounds:u.bounds,unit:Some(u),block:None}).collect();elements.extend(other.iter().map(|b|columns::SectionElement{bounds:match b{PageBlock::Text(x)=>x.bounds,PageBlock::Table(x)=>x.bounds,PageBlock::Image(x)=>x.bounds},unit:None,block:Some(b)}));
    let leaders=raw.iter().filter(|l|toc::has_dot_leader_run(&l.chars)).count();let leader_page=extracted.width_pt<extracted.height_pt&&leaders>=toc::LEADER_PAGE_MIN_LINES&&(leaders as f64)>=toc::LEADER_PAGE_MIN_SHARE*raw.len().max(1) as f64;
    let mut layout=if leader_page&&!elements.is_empty(){let bounds=elements.iter().map(|e|e.bounds).reduce(Rect::union).unwrap();vec![columns::LayoutSection{bounds,columns:vec![columns::LayoutColumn{bounds,elements:elements.clone()}],gutters:vec![],direction:Direction::Ltr}]}else{columns::detect_sections(&elements,extracted.height_pt,Some(extracted.width_pt))};if !leader_page&&extracted.width_pt<extracted.height_pt{layout=columns::merge_twin_sections(&layout);}
    let plain=!absolute&&layout.iter().all(|s|s.columns.len()==1)&&stream_tables.is_empty()&&zone.tables.is_empty()&&forms.tables.is_empty()&&toc_blocks.is_empty()&&floats.is_empty();let carried=list_seq.last_ordered.clone();
    let mut sections=if plain{let mut analyzed=chars::analyze_chars(&upright);analyzed.sort_by(|a,b|b.baseline.total_cmp(&a.baseline).then_with(||a.bounds.x0.total_cmp(&b.bounds.x0)));let body=if analyzed.is_empty(){None}else{let mut b=blocks::body_context_of(&analyzed);b.body_right=b.body_right.max(extracted.width_pt-b.body_left);Some(b)};let text=blocks::group_into_blocks(&analyzed,body,blocks::BlockOptions{pin_open_leaded_breaks:extracted.width_pt>extracted.height_pt})?;let text=toc::detect_toc_blocks(lists::detect_list_blocks(text,list_seq,None));let nontext=tables.iter().cloned().map(PageBlock::Table).chain(inline.iter().cloned().map(PageBlock::Image)).collect();layout::single_section(layout::merge_blocks(text.into_iter().map(PageBlock::Text).collect(),nontext))}else{
        let x:Vec<_>=layout.iter().filter(|s|s.columns.len()==1).map(|s|s.columns[0].bounds.x0).collect();let fallback=if x.is_empty(){None}else{Some(median(&x))};let mut result=Vec::new();for ls in &layout{let mut columns=Vec::new();let iter:Vec<_>=if ls.direction==Direction::Rtl{ls.columns.iter().rev().collect()}else{ls.columns.iter().collect()};for c in iter{columns.push(layout::assemble_column(c,ls.columns.len()==1,extracted.width_pt,list_seq,extracted.width_pt>extracted.height_pt,Some(if ls.columns.len()>1{c.bounds.x0}else{fallback.unwrap_or(c.bounds.x0)}),absolute)?);}result.push(PageSection{bounds:ls.bounds,columns,gutter_widths_pt:ls.gutters.iter().map(|g|g.hi-g.lo).collect(),direction:ls.direction,column_separator:false});}if result.is_empty()&&!floats.is_empty(){layout::single_section(floats.iter().cloned().map(PageBlock::Image).collect())}else{for image in floats.iter().cloned(){layout::place_float(&mut result,image);}result}
    };
    if sections.is_empty()&&!page.background_panels.is_empty(){let bounds=body_bounds(extracted);sections.push(PageSection{bounds,columns:vec![PageColumn{bounds,blocks:vec![]}],gutter_widths_pt:vec![],direction:Direction::Ltr,column_separator:false});}
    let mut separators=HashSet::new();for (i,ls) in layout.iter().enumerate(){if let Some(section)=sections.get_mut(i){for (index,stroke) in shapes.strokes.iter().enumerate(){if stroke.orientation!="v"||separators.contains(&index){continue;}let x=stroke.bounds.center_x();if !ls.gutters.iter().any(|g|x>=g.lo&&x<=g.hi){continue;}let overlap=stroke.bounds.y1.min(ls.bounds.y1)-stroke.bounds.y0.max(ls.bounds.y0);if overlap>=0.6*ls.bounds.height(){section.column_separator=true;separators.insert(index);}}}}let mut consumed=styled.consumed_strokes;consumed.extend(zone.consumed_strokes);consumed.extend(separators);
    let mut decor_boxes=grid_boxes;decor_boxes.extend(tables.iter().map(|t|t.bounds));let decor=decor::apply_decor_borders(&mut sections,&shapes.strokes,&decor_boxes,&consumed);page.ignored_vertical_decor=decor.ignored_vertical+decor.dropped_bars;
    let mut warnings=styled.warnings;warnings.extend(panel_notes);warnings.extend(spacing::apply(&mut sections));for run in turned{if let Some(b)=turned::block(&run){if let Some(column)=sections.last_mut().and_then(|s|s.columns.last_mut()){column.blocks.push(PageBlock::Text(b));}else{sections=layout::single_section(vec![PageBlock::Text(b)]);}}}
    page.sections=sections;page.blocks=page.sections.iter().flat_map(|s|&s.columns).flat_map(|c|c.blocks.iter().cloned()).collect();if list_seq.last_ordered==carried{list_seq.last_ordered=None;}if extracted.width_pt<extracted.height_pt{cards::detect(&mut page,&card_candidates);}
    let conf:Vec<_>=forms.tables.iter().chain(&zone.tables).chain(&stream_tables).filter_map(|t|t.confidence).collect();let warning_count=if absolute{warnings.iter().filter(|w|!confidence::is_flow_only_warning(w)).count()}else{warnings.len()};page.confidence=Some(confidence::page_confidence(extracted.bad_unicode_ratio,&conf,warning_count));page.warnings=warnings;page.shapes=Some(shapes);Ok(page)
}

pub(super) struct Analyzer { absolute_layout: bool, list_seq: lists::ListSeq }
impl Analyzer {
    pub(super) fn analyze_page(&mut self, page:&mut ExtractedPage)->Result<IrPage,String>{
        one_page(page,self.absolute_layout,&mut self.list_seq)
    }
}
pub(super) fn prepare(pages:&mut [ExtractedPage],absolute_layout:bool,cell_data:bool)->(Analyzer,Vec<FurnitureHf>,usize){
    let furniture=if absolute_layout{None}else{Some(furniture::detect_furniture(pages,cell_data))};
    if let Some(f)=&furniture {for (index,page) in pages.iter_mut().enumerate(){if let Some(drops)=f.drop_indices.get(index){let mut i=0;page.chars.retain(|_|{let keep=!drops.contains(&i);i+=1;keep});}}}
    let dropped=furniture.as_ref().map_or(0,|f|f.dropped_lines);
    (Analyzer{absolute_layout,list_seq:lists::ListSeq::default()},furniture.map_or_else(Vec::new,|f|f.hf),dropped)
}
pub(super) fn classify_pages(pages:&mut [IrPage],producer:Option<&str>,creator:Option<&str>)->Result<(),String>{
    let meta=canvas::DocMeta{producer:producer.map(str::to_owned),creator:creator.map(str::to_owned)};
    canvas::classify_pages(pages,&meta).map(|_|())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::ir::PdfChar;
    use super::super::script::Script;

    #[test]
    fn analyzes_synthetic_glyphs_into_editable_text() {
        let chars = "Text".chars().enumerate().map(|(i,c)| {
            let x=72.0+i as f64*6.0;let bounds=Rect{x0:x,y0:700.0,x1:x+5.0,y1:710.0};
            PdfChar{code:c as u32,text:c.to_string(),bounds,loose_box:bounds,origin_x:x,origin_y:700.0,angle:0.0,font_size:10.0,font_weight:400.0,font_family:"Arial".into(),italic:false,color:"000000".into(),hscale:None,invisible:false,is_generated:false,pdf_char_index:Some(i),text_object_id:None,is_hyphen:false,script:Script::Latin,highlight:None,underline:false,strike:false,note_ref:None,href:None}
        }).collect();
        let mut page=ExtractedPage{index:0,width_pt:612.0,height_pt:792.0,rotation:0,chars,images:vec![],paths:vec![],degraded:false,degraded_reason:None,scanned:false,has_struct_tree:false,render:None,vector_regions:vec![],ocr_text_recovered:false,ocr_image_kept:false,text_lost:false,rotated_dropped:0,cell_data:false,bad_unicode_ratio:0.0,background_render:None,content_box:None,decor_images:vec![]};
        let mut analyzer=Analyzer{absolute_layout:false,list_seq:lists::ListSeq::default()};
        let page=analyzer.analyze_page(&mut page).unwrap();
        let text:String=page.blocks.iter().filter_map(|b|if let PageBlock::Text(t)=b{Some(t)}else{None}).flat_map(|b|&b.lines).flat_map(|l|&l.spans).map(|s|s.text.as_str()).collect();
        assert_eq!(text,"Text");
        assert_eq!(page.confidence,Some(1.0));
    }
}
