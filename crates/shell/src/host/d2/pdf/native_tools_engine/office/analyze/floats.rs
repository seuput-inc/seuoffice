//! Classify image placement and suppress duplicate display-text shadow rasters.
use super::super::geometry::Rect;
use super::super::ir::{FloatPlacement, ImageBlock};
use super::units::LineUnit;

pub(super) fn suppress_text_shadow_images(images:&[ImageBlock],units:&[LineUnit])->Vec<ImageBlock> {
    let chars:Vec<_>=units.iter().flat_map(|u|&u.chars).filter(|c|!c.text.trim().is_empty()&&!c.invisible).collect();
    images.iter().filter(|img| {
        let(w,h)=(img.bounds.width(),img.bounds.height());if w<=0.0||h<=0.0{return true;}
        let inside:Vec<_>=chars.iter().filter(|c|{let(x,y)=(c.bounds.center_x(),c.bounds.center_y());x>=img.bounds.x0&&x<=img.bounds.x1&&y>=img.bounds.y0&&y<=img.bounds.y1}).collect();
        if inside.len()<2{return true;}let bounds=inside.iter().map(|c|c.bounds).reduce(Rect::union).unwrap();
        if img.bounds.area()>bounds.area()*3.5{return true;}
        let mut heights:Vec<_>=inside.iter().map(|c|c.bounds.height()).collect();heights.sort_by(f64::total_cmp);let median=heights[heights.len()/2];
        if median>0.0&&bounds.height()>2.2*median{return true;}
        if bounds.width()<w*0.65 || (bounds.center_x()-img.bounds.center_x()).abs()>w*0.15 || (bounds.center_y()-img.bounds.center_y()).abs()>h*0.3{return true;}
        let mut sizes:Vec<_>=inside.iter().map(|c|c.font_size).collect();sizes.sort_by(f64::total_cmp);sizes[sizes.len()/2]<18.0
    }).cloned().collect()
}
pub(super) fn classify(images:&[ImageBlock],all_units:&[LineUnit],page_area:Option<f64>,page_box:Option<Rect>)->(Vec<ImageBlock>,Vec<ImageBlock>) {
    let units:Vec<_>=all_units.iter().filter(|u|u.chars.iter().any(|c|c.font_size>=2.5)).collect();
    let left=units.iter().map(|u|u.bounds.x0).reduce(f64::min).unwrap_or(0.0);let right=units.iter().map(|u|u.bounds.x1).reduce(f64::max).unwrap_or(0.0);
    let mut floats=Vec::new();let mut inline=Vec::new();
    for (index,img) in images.iter().enumerate() {
        let over:Vec<_>=units.iter().filter(|u|u.bounds.overlap_ratio(img.bounds)>=0.8).collect();
        let beside:Vec<_>=units.iter().filter(|u|u.bounds.vertical_overlap_ratio(img.bounds)>=0.5&&(u.bounds.x1<=img.bounds.x0+2.0||u.bounds.x0>=img.bounds.x1-2.0)).collect();
        let covered=page_box.is_some_and(|p|img.bounds.intersection_area(p)>=p.area()*0.9);
        let overlapped=images.iter().enumerate().any(|(i,other)|i!=index&&img.bounds.overlap_ratio(other.bounds)>=0.6);
        let crossers=units.iter().filter(|u|u.bounds.vertical_overlap_ratio(img.bounds)>=0.5 && u.bounds.x1.min(img.bounds.x1)-u.bounds.x0.max(img.bounds.x0)>=8.0 && u.bounds.overlap_ratio(img.bounds)<0.8).count();
        let edge=page_box.is_some_and(|p|over.len()<=2&&beside.len()<=2&&(img.bounds.y1<=p.y0+0.12*p.height()||img.bounds.y0>=p.y1-0.12*p.height()));
        let tiny=img.bounds.width()<=20.0&&img.bounds.height()<=20.0&&!units.is_empty()&&img.bounds.y1<=units.iter().map(|u|u.bounds.y1).fold(f64::NEG_INFINITY,f64::max)&&img.bounds.y0>=units.iter().map(|u|u.bounds.y0).fold(f64::INFINITY,f64::min);
        let wrap=if img.synthetic||covered||over.len()>=2||overlapped||crossers>=2 {Some("behind")}
            else if beside.len()>=2 {let left_text=beside.iter().filter(|u|u.bounds.x1<=img.bounds.x0+2.0).count();let right_text=beside.len()-left_text;Some(if left_text>right_text||left_text==right_text&&img.bounds.center_x()>(left+right)/2.0{"square-right"}else{"square-left"})}
            else if !beside.is_empty()&&over.is_empty()&&page_area.is_some_and(|area|img.bounds.area()<=area*0.02)||edge||tiny {Some("behind")} else {None};
        let mut img=img.clone();if let Some(wrap)=wrap {img.float=Some(FloatPlacement{wrap:wrap.into(),x_offset_pt:(img.bounds.x0-left).max(0.0)});floats.push(img);}else{inline.push(img);}
    }
    let gallery:Vec<bool>=inline.iter().enumerate().map(|(i,a)|inline.iter().enumerate().any(|(j,b)|i!=j&&a.bounds.vertical_overlap_ratio(b.bounds)>=0.5&&(a.bounds.x1<=b.bounds.x0+2.0||a.bounds.x0>=b.bounds.x1-2.0))).collect();
    let mut kept=Vec::new();for (mut img,is_gallery) in inline.into_iter().zip(gallery){if is_gallery{img.float=Some(FloatPlacement{wrap:"behind".into(),x_offset_pt:(img.bounds.x0-left).max(0.0)});floats.push(img);}else{kept.push(img);}}(floats,kept)
}
