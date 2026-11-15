//! Borderless table detection, ported from packages/pdf2docx/src/analyze/stream.ts.
use std::collections::HashSet;
use super::super::chars::analyze_chars;
use super::super::geometry::{median, merge_intervals, Interval, Rect};
use super::super::ir::{Fill, PageShapes, TableBlock, TableCell};
use super::blocks::{group_into_blocks, is_verse_stack, BlockOptions};
use super::shapes::is_near_white;
use super::units::{cluster_unit_rows, LineUnit, UnitRow};
use super::zones::build_stack_cell;

#[derive(Clone)]
struct Entry { units: Vec<LineUnit>, bounds: Rect }
#[derive(Clone)]
struct Candidate { rows: Vec<UnitRow>, bounds: Rect, columns: Vec<Interval>, entries: Vec<Vec<Option<Entry>>>, font_size: f64 }
#[derive(Clone, Copy)]
struct PassOptions { absorb: bool, strong_grid: bool, max_gap: f64, strong_only: bool, relax_kv: bool }
pub(super) struct DetectedStreamTables { pub tables: Vec<TableBlock>, pub remaining_units: Vec<LineUnit> }

fn med(values: impl Iterator<Item=f64>) -> f64 { median(&values.collect::<Vec<_>>()) }
fn key_value_row(row: &UnitRow) -> bool {
    if row.units.len() < 2 { return false; }
    let mut units: Vec<_> = row.units.iter().collect();
    units.sort_by(|a,b| a.bounds.x0.total_cmp(&b.bounds.x0));
    let gap = units.windows(2).map(|w| w[1].bounds.x0-w[0].bounds.x1).fold(f64::INFINITY, f64::min);
    let size = med(units.iter().map(|u| u.font_size));
    gap >= 2.5 * if size == 0.0 { 12.0 } else { size }
}
fn row_fits(run: &[UnitRow], row: &UnitRow) -> bool {
    let x0 = run.iter().map(|r|r.bounds.x0).fold(f64::INFINITY,f64::min);
    let x1 = run.iter().map(|r|r.bounds.x1).fold(f64::NEG_INFINITY,f64::max);
    row.bounds.width() <= 0.5*(x1-x0) && row.units.iter().all(|u| u.bounds.x0>=x0-2.0 && u.bounds.x1<=x1+2.0)
}
fn flush(run: &mut Vec<UnitRow>, runs: &mut Vec<Vec<UnitRow>>) {
    while run.last().is_some_and(|r|r.units.len()<2) { run.pop(); }
    if run.len()>=2 { runs.push(std::mem::take(run)); } else { run.clear(); }
}
fn find_candidates(rows: &[UnitRow], o: PassOptions) -> Vec<Vec<UnitRow>> {
    let mut runs=Vec::new(); let mut run: Vec<UnitRow>=Vec::new();
    for row in rows {
        let anchors:Vec<_>=run.iter().filter(|r|r.units.len()>=2).collect();
        let strong=!anchors.is_empty() && med(anchors.iter().map(|r|r.units.len() as f64))>=5.0;
        let mut gap_ok=false;
        if let Some(prev)=run.last() {
            let gap=prev.bounds.y0-row.bounds.y1; let h=prev.bounds.height().max(row.bounds.height());
            gap_ok=gap<=o.max_gap*h;
            if !gap_ok && row.units.len()>=5 && prev.units.len()>=5 { gap_ok=gap<=3.5*h; }
            if !gap_ok && o.relax_kv && key_value_row(row) && key_value_row(prev) { gap_ok=gap<=4.0*h; }
            if !gap_ok && run.len()>=2 && strong {
                let pitch=med(run.windows(2).map(|w|w[0].bounds.y1-w[1].bounds.y1));
                gap_ok=pitch>0.0 && prev.bounds.y1-row.bounds.y1<=1.35*pitch;
            }
        }
        if row.units.len()>=2 && (run.is_empty() || gap_ok) { run.push(row.clone()); continue; }
        if o.absorb && !run.is_empty() && gap_ok && (!o.strong_only || strong) && row_fits(&run,row) { run.push(row.clone()); continue; }
        flush(&mut run,&mut runs);
        if row.units.len()>=2 { run.push(row.clone()); }
    }
    flush(&mut run,&mut runs); runs
}
fn solve_columns(rows: &[UnitRow]) -> Option<Candidate> {
    let units:Vec<_>=rows.iter().flat_map(|r|r.units.iter()).collect();
    let bounds=units.iter().map(|u|u.bounds).reduce(Rect::union)?;
    let size=med(units.iter().map(|u|u.font_size)); let font_size=if size==0.0 {12.0} else {size};
    let columns=merge_intervals(&units.iter().map(|u|Interval{lo:u.bounds.x0,hi:u.bounds.x1}).collect::<Vec<_>>(),0.4*font_size);
    if columns.len()<2 { return None; }
    let entries=rows.iter().map(|row| {
        let mut per:Vec<Option<Entry>>=vec![None;columns.len()];
        for unit in &row.units {
            let cx=unit.bounds.center_x();
            if let Some(col)=columns.iter().position(|c|cx>=c.lo && cx<=c.hi) {
                if let Some(e)=&mut per[col] { e.bounds=e.bounds.union(unit.bounds); e.units.push(unit.clone()); }
                else { per[col]=Some(Entry{units:vec![unit.clone()],bounds:unit.bounds}); }
            }
        }
        per
    }).collect();
    Some(Candidate{rows:rows.to_vec(),bounds,columns,entries,font_size})
}
fn kv_share(c: &Candidate, col: usize) -> f64 {
    let entries:Vec<_>=c.entries.iter().filter_map(|r|r[col].as_ref()).collect();
    if entries.is_empty() {return 0.0;}
    let count=entries.iter().filter(|e| {
        let text=e.units.iter().map(|u|u.chars.iter().map(|ch|ch.text.as_str()).collect::<String>()).collect::<Vec<_>>().join(" ");
        let trimmed=text.trim(); let prefix=trimmed.split([':', '：']).next().unwrap_or("");
        prefix.encode_utf16().count()>=1 && prefix.encode_utf16().count()<=40 && prefix.len()<trimmed.len()
    }).count(); count as f64/entries.len() as f64
}
fn align(c:&Candidate,col:usize)->f64 {
    let boxes:Vec<_>=c.entries.iter().filter_map(|r|r[col].as_ref().map(|e|e.bounds)).collect();
    if boxes.len()<2 {return 1.0;}
    let hits=|f:fn(Rect)->f64| { let m=med(boxes.iter().copied().map(f)); boxes.iter().filter(|&&b|(f(b)-m).abs()<=0.4*c.font_size).count() };
    hits(|b|b.x0).max(hits(|b|b.x1)).max(hits(|b|b.center_x())) as f64/boxes.len() as f64
}
fn collect_candidates(run:&[UnitRow],out:&mut Vec<Candidate>,depth:usize,retry:bool) {
    let lo=run.iter().position(|r|r.units.len()>=2).unwrap_or(run.len());
    let hi=run.iter().rposition(|r|r.units.len()>=2).map_or(lo,|i|i+1);
    let piece=&run[lo..hi]; if piece.len()<2{return;}
    if let Some(c)=solve_columns(piece) {
        let min_align=if retry && !c.columns.iter().enumerate().all(|(i,_)|kv_share(&c,i)>=0.6) { (0..c.columns.len()).map(|i|align(&c,i)).fold(f64::INFINITY,f64::min) } else {1.0};
        if min_align>=0.6 || depth>=2 || piece.len()<4 {out.push(c);return;}
    }
    if depth>=2 || piece.len()<4{return;}
    let mut cut=0; let mut widest=f64::NEG_INFINITY;
    for i in 1..piece.len() {let gap=piece[i-1].bounds.y0-piece[i].bounds.y1;if gap>widest {widest=gap;cut=i;}}
    if cut>0 {collect_candidates(&piece[..cut],out,depth+1,retry);collect_candidates(&piece[cut..],out,depth+1,retry);}
}
struct Stats { fill:f64, width:f64, words:f64, full:f64 }
fn entry_stats(c:&Candidate)->Stats {
    let (mut fill,mut width,mut words,mut full,mut n)=(0.0,0.0,0.0,0.0,0.0);
    for row in &c.entries {for (col,e) in row.iter().enumerate() {if let Some(e)=e {
        let ratio=e.bounds.width()/(c.columns[col].hi-c.columns[col].lo).max(1.0);
        fill+=ratio; if ratio>=0.85 {full+=1.0;} width+=e.bounds.width()/c.font_size.max(1.0);
        words+=e.units.iter().map(|u|u.word_count).sum::<usize>() as f64;n+=1.0;
    }}}
    if n==0.0 {return Stats{fill:0.0,width:0.0,words:0.0,full:0.0};}
    Stats{fill:fill/n,width:width/n,words:words/n,full:full/n}
}
fn page_columns(c:&Candidate,total:usize)->bool {
    if c.rows.iter().map(|r|r.units.len()).sum::<usize>() as f64/(total.max(1) as f64)<0.7{return false;}
    let pitches:Vec<_>=c.rows.windows(2).map(|w|w[0].baseline-w[1].baseline).collect();let pitch=median(&pitches);
    if pitch<=0.0 || med(pitches.iter().map(|p|(p-pitch).abs()))>0.2*pitch{return false;}
    let s=entry_stats(c);s.width>=8.0 && s.words>=4.0
}
fn verse(c:&Candidate)->Result<bool,String> {
    for col in 0..c.columns.len() {
        let entries:Vec<_>=c.entries.iter().filter_map(|r|r[col].as_ref()).collect();
        if entries.len()<c.rows.len() || med(entries.iter().map(|e|e.units.iter().map(|u|u.word_count).sum::<usize>() as f64))<3.0 {return Ok(false);}
        let chars:Vec<_>=entries.iter().flat_map(|e|e.units.iter().flat_map(|u|u.chars.iter().cloned())).collect();
        let mut lines=analyze_chars(&chars);lines.sort_by(|a,b|b.baseline.total_cmp(&a.baseline));
        if !is_verse_stack(&lines)? {return Ok(false);}
    } Ok(true)
}
fn absorb_orphans(c:&mut Candidate,all:&[UnitRow]) {
    let mut ids:HashSet<usize>=c.rows.iter().flat_map(|r|r.units.iter().map(|u|u.id)).collect();
    loop {let mut changed=false;let top=c.rows[0].baseline;let bottom=c.rows[c.rows.len()-1].baseline;
        for row in all {
            if row.units.iter().any(|u|ids.contains(&u.id)) {continue;}
            let above=row.baseline-top;let below=bottom-row.baseline;let max=1.9*c.font_size;
            if !((above>0.0 && above<=max)||(below>0.0 && below<=max)) || row.units.len()>c.columns.len() || !row.units.iter().all(|u|u.bounds.center_x()>=c.bounds.x0-c.font_size && u.bounds.center_x()<=c.bounds.x1+c.font_size) {continue;}
            ids.extend(row.units.iter().map(|u|u.id));c.bounds=c.bounds.union(row.bounds);c.rows.push(row.clone());c.rows.sort_by(|a,b|b.baseline.total_cmp(&a.baseline));changed=true;
        }
        if !changed {break;}
    }
}
fn x_boundaries(c:&Candidate)->Vec<f64> {let mut xs=vec![c.bounds.x0];xs.extend(c.columns.windows(2).map(|w|(w[0].hi+w[1].lo)/2.0));xs.push(c.bounds.x1);xs}
fn table(bounds:Rect,xs:&[f64],rows:Vec<Vec<TableCell>>,confidence:f64)->TableBlock {
    TableBlock{bounds,col_widths_pt:xs.windows(2).map(|w|w[1]-w[0]).collect(),rows,confidence:Some(confidence),border_color:None,form:false,separator_rule:None,spacing_before_pt:None}
}
fn verse_table(c:&Candidate,confidence:f64)->TableBlock {
    let xs=x_boundaries(c);let all:Vec<_>=c.rows.iter().flat_map(|r|r.units.iter()).collect();
    let cells=(0..c.columns.len()).map(|i| {
        let units:Vec<_>=all.iter().filter(|u|u.bounds.center_x()>=xs[i] && u.bounds.center_x()<xs[i+1]+if i+1==c.columns.len(){1.0}else{0.0}).map(|u|(*u).clone()).collect();
        build_stack_cell(&units,Rect{x0:xs[i],x1:xs[i+1],y0:c.bounds.y0,y1:c.bounds.y1},c.bounds.height(),false)
    }).collect();table(c.bounds,&xs,vec![cells],confidence)
}
fn cell_fill(bounds:Rect,fills:&[Fill])->Option<String> {
    fills.iter().find(|f|bounds.center_x()>=f.bounds.x0 && bounds.center_x()<=f.bounds.x1 && bounds.center_y()>=f.bounds.y0 && bounds.center_y()<=f.bounds.y1 && !is_near_white(&f.color) && bounds.intersection_area(f.bounds)/bounds.area().max(1.0)>=0.6).map(|f|f.color.clone())
}
fn build_table(c:&Candidate,confidence:f64,fills:&[Fill])->Result<TableBlock,String> {
    let xs=x_boundaries(c);let mut ys=vec![c.bounds.y1];ys.extend(c.rows.windows(2).map(|w|(w[1].bounds.y1+w[0].bounds.y0)/2.0));ys.push(c.bounds.y0);
    let mut rows=Vec::new();for (r,row) in c.entries.iter().enumerate() {let mut cells=Vec::new();for (col,e) in row.iter().enumerate() {
        let bounds=Rect{x0:xs[col],x1:xs[col+1],y0:ys[r+1],y1:ys[r]};
        let blocks=if let Some(e)=e {let mut units:Vec<_>=e.units.iter().collect();units.sort_by(|a,b|a.bounds.x0.total_cmp(&b.bounds.x0));let chars:Vec<_>=units.iter().flat_map(|u|u.chars.iter().cloned()).collect();group_into_blocks(&analyze_chars(&chars),None,BlockOptions::default())?}else{Vec::new()};
        cells.push(TableCell{bounds,grid_span:1,v_merge:None,fill:cell_fill(bounds,fills),v_align:None,soft_edges:None,blocks});
    }rows.push(cells);}Ok(table(c.bounds,&xs,rows,confidence))
}
fn detect_pass(units:&[LineUnit],shapes:&PageShapes,excluded:&[Rect],o:PassOptions)->Result<DetectedStreamTables,String> {
    let rows=cluster_unit_rows(units);let mut candidates=Vec::new();
    for run in find_candidates(&rows,o) {collect_candidates(&run,&mut candidates,0,o.relax_kv);}
    let mut tables=Vec::new();let mut consumed=HashSet::new();
    for mut c in candidates {
        if excluded.iter().any(|b|c.bounds.intersection_area(*b)>0.0) || (o.strong_grid && (c.rows.len()<3 || c.columns.len()<3)) {continue;}
        let min_align=(0..c.columns.len()).map(|i|align(&c,i)).fold(f64::INFINITY,f64::min);
        let kv=min_align<0.6 && o.relax_kv && (0..c.columns.len()).all(|i|kv_share(&c,i)>=0.6);
        if min_align<0.6 && !kv {continue;}
        let widths=med(c.entries.iter().flat_map(|r|r.iter().filter_map(|e|e.as_ref().map(|e|e.bounds.width()))));
        let gaps=med(c.columns.windows(2).map(|w|w[1].lo-w[0].hi));if widths/gaps.max(0.1)>10.0 {continue;}
        let s=entry_stats(&c);if s.fill>=0.85 && (s.width>=10.0 || s.words>=5.0) {continue;}if page_columns(&c,units.len()){continue;}
        let row_h=med(c.rows.iter().map(|r|r.bounds.height()));let row_h=if row_h==0.0{c.font_size}else{row_h};let y0=c.bounds.y0-row_h;let y1=c.bounds.y1+row_h;
        let stroke=shapes.strokes.iter().any(|s|s.orientation=="h" && s.bounds.center_y()>=y0 && s.bounds.center_y()<=y1 && s.bounds.x1.min(c.bounds.x1)-s.bounds.x0.max(c.bounds.x0)>=0.5*c.bounds.width());
        let fill=shapes.fills.iter().any(|f|f.bounds.y1<=y1 && f.bounds.y0>=y0-row_h && f.bounds.height()<=2.5*row_h && f.bounds.x1.min(c.bounds.x1)-f.bounds.x0.max(c.bounds.x0)>=0.5*c.bounds.width());
        let mut confidence=0.3;if c.rows.len()>=3 {confidence+=0.15+(0.02*(c.rows.len()-3) as f64).min(0.1);}confidence+=0.15*((if kv {min_align.max(0.6)}else{min_align})-0.6)/0.4;if stroke{confidence+=0.2;}if fill{confidence+=0.15;}let confidence=confidence.min(1.0);if confidence<0.5{continue;}
        if verse(&c)? {absorb_orphans(&mut c,&rows);tables.push(verse_table(&c,confidence));} else {if (s.width>=10.0 || s.words>=5.0) && s.full>=0.5 {continue;}tables.push(build_table(&c,confidence,&shapes.fills)?);}
        consumed.extend(c.rows.iter().flat_map(|r|r.units.iter().map(|u|u.id)));
    }
    tables.sort_by(|a,b|b.bounds.y1.total_cmp(&a.bounds.y1));Ok(DetectedStreamTables{tables,remaining_units:units.iter().filter(|u|!consumed.contains(&u.id)).cloned().collect()})
}
fn crosses(b:Rect,g:Interval)->bool {b.x0<g.hi && b.x1>g.lo}
fn center_in(u:&LineUnit,b:Rect)->bool {u.bounds.center_x()>=b.x0 && u.bounds.center_x()<=b.x1 && u.bounds.center_y()>=b.y0 && u.bounds.center_y()<=b.y1}
fn page_gutters(units:&[LineUnit])->Vec<Interval> {
    let prose:Vec<_>=units.iter().filter(|u|u.bounds.width()>=10.0*u.font_size && u.word_count>=5).collect();
    let spans=merge_intervals(&prose.iter().map(|u|Interval{lo:u.bounds.x0,hi:u.bounds.x1}).collect::<Vec<_>>(),0.0);
    spans.windows(2).filter_map(|w|{let g=Interval{lo:w[0].hi,hi:w[1].lo};(g.hi-g.lo>=9.0 && prose.iter().filter(|u|u.bounds.x1<=g.lo).count()>=3 && prose.iter().filter(|u|u.bounds.x0>=g.hi).count()>=3).then_some(g)}).collect()
}
fn side_by_side(table:&TableBlock,own:&[LineUnit],g:Interval)->bool {
    if table.bounds.x0>=g.lo || table.bounds.x1<=g.hi || own.iter().any(|u|crosses(u.bounds,g)){return false;}
    let rows=cluster_unit_rows(own);let shared=rows.iter().filter(|r|r.units.iter().any(|u|u.bounds.x1<=g.lo) && r.units.iter().any(|u|u.bounds.x0>=g.hi)).count();!rows.is_empty() && (shared as f64)/(rows.len() as f64)<0.5
}
fn split_side_by_side(units:&[LineUnit],res:DetectedStreamTables,shapes:&PageShapes,excluded:&[Rect],o:PassOptions)->Result<DetectedStreamTables,String> {
    if res.tables.is_empty(){return Ok(res);}let remaining:HashSet<_>=res.remaining_units.iter().map(|u|u.id).collect();
    for g in page_gutters(units) {
        let split:Vec<_>=res.tables.iter().enumerate().filter_map(|(i,t)|{let own:Vec<_>=units.iter().filter(|u|!remaining.contains(&u.id)&&center_in(u,t.bounds)).cloned().collect();side_by_side(t,&own,g).then_some(i)}).collect();
        if split.is_empty(){continue;}let kept:Vec<_>=res.tables.iter().enumerate().filter(|(i,_)|!split.contains(i)).map(|(_,t)|t.clone()).collect();
        let mut claimed=excluded.to_vec();claimed.extend(kept.iter().map(|t|t.bounds));
        let free:Vec<_>=units.iter().filter(|u|remaining.contains(&u.id)||split.iter().any(|&i|center_in(u,res.tables[i].bounds))).cloned().collect();
        let left:Vec<_>=free.iter().filter(|u|u.bounds.x1<=g.lo).cloned().collect();let right:Vec<_>=free.iter().filter(|u|u.bounds.x0>=g.hi).cloned().collect();
        let a=detect_pass(&left,shapes,&claimed,o)?;let b=detect_pass(&right,shapes,&claimed,o)?;
        let flow:HashSet<_>=free.iter().filter(|u|crosses(u.bounds,g)).chain(a.remaining_units.iter()).chain(b.remaining_units.iter()).map(|u|u.id).collect();
        let mut tables=kept;tables.extend(a.tables);tables.extend(b.tables);tables.sort_by(|a,b|b.bounds.y1.total_cmp(&a.bounds.y1));
        return Ok(DetectedStreamTables{tables,remaining_units:units.iter().filter(|u|flow.contains(&u.id)).cloned().collect()});
    }Ok(res)
}
fn split_valley(units:&[LineUnit])->Option<(Vec<LineUnit>,Vec<LineUnit>)> {
    if units.len()<2{return None;}let spans=merge_intervals(&units.iter().map(|u|Interval{lo:u.bounds.x0,hi:u.bounds.x1}).collect::<Vec<_>>(),0.0);
    let mut best=0.0;let mut cut=0.0;for w in spans.windows(2){let gap=w[1].lo-w[0].hi;if gap>best{best=gap;cut=(w[0].hi+w[1].lo)/2.0;}}if best<40.0{return None;}
    let (left,right):(Vec<_>,Vec<_>)=units.iter().cloned().partition(|u|u.bounds.center_x()<cut);if left.is_empty()||right.is_empty(){None}else{Some((left,right))}
}
fn region_pass(units:&[LineUnit],shapes:&PageShapes,claimed:&[Rect],depth:usize)->Result<DetectedStreamTables,String> {
    let o=PassOptions{absorb:true,strong_grid:true,max_gap:3.5,strong_only:false,relax_kv:false};let result=detect_pass(units,shapes,claimed,o)?;
    if !result.tables.is_empty() || depth>=3{return Ok(result);}let Some((left,right))=split_valley(units) else{return Ok(result)};
    let mut a=region_pass(&left,shapes,claimed,depth+1)?;let b=region_pass(&right,shapes,claimed,depth+1)?;a.tables.extend(b.tables);a.remaining_units.extend(b.remaining_units);Ok(a)
}
pub(super) fn detect_stream_tables(units:&[LineUnit],shapes:&PageShapes,exclusions:&[Rect],slide_regions:bool,relax_key_value:bool)->Result<DetectedStreamTables,String> {
    let o=PassOptions{absorb:true,strong_grid:false,max_gap:1.6,strong_only:true,relax_kv:relax_key_value};
    let base=split_side_by_side(units,detect_pass(units,shapes,exclusions,o)?,shapes,exclusions,o)?;
    if !slide_regions{return Ok(base);}let mut claimed=exclusions.to_vec();claimed.extend(base.tables.iter().map(|t|t.bounds));
    let region=region_pass(&base.remaining_units,shapes,&claimed,0)?;if region.tables.is_empty(){return Ok(base);}
    let mut tables=base.tables;tables.extend(region.tables);tables.sort_by(|a,b|b.bounds.y1.total_cmp(&a.bounds.y1));Ok(DetectedStreamTables{tables,remaining_units:region.remaining_units})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: usize) -> Vec<LineUnit> {
        (0..rows).flat_map(|row| (0..2).map(move |col| {
            let x = col as f64 * 30.0;
            let y = 100.0 - row as f64 * 12.0;
            LineUnit { id: row * 2 + col, chars: Vec::new(),
                bounds: Rect { x0: x, x1: x + 10.0, y0: y, y1: y + 8.0 },
                baseline: y, line_index: row, word_count: 1, font_size: 10.0 }
        })).collect()
    }

    fn shapes() -> PageShapes {
        PageShapes { strokes: Vec::new(), fills: Vec::new(), ignored_paths: 0, curved_fills: Vec::new() }
    }

    #[test]
    fn aligned_grid_is_consumed_but_overlapping_lattice_keeps_flow() {
        let units = grid(3);
        let found = detect_stream_tables(&units, &shapes(), &[], false, false).unwrap();
        assert_eq!(found.tables.len(), 1);
        assert_eq!(found.tables[0].rows.len(), 3);
        assert!(found.remaining_units.is_empty());
        let excluded = [Rect { x0: 0.0, x1: 40.0, y0: 70.0, y1: 110.0 }];
        let blocked = detect_stream_tables(&units, &shapes(), &excluded, false, false).unwrap();
        assert!(blocked.tables.is_empty());
        assert_eq!(blocked.remaining_units.iter().map(|u| u.id).collect::<Vec<_>>(), (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn two_rows_need_vector_evidence() {
        let units = grid(2);
        let mut shapes = shapes();
        assert!(detect_stream_tables(&units, &shapes, &[], false, false).unwrap().tables.is_empty());
        shapes.strokes.push(super::super::super::ir::Stroke {
            bounds: Rect { x0: 0.0, x1: 40.0, y0: 108.0, y1: 108.5 },
            orientation: "h".into(), width_pt: 0.5, color: "000000".into(), from_form: false,
        });
        assert_eq!(detect_stream_tables(&units, &shapes, &[], false, false).unwrap().tables.len(), 1);
    }
}
