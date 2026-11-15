//! Reconstruct word boundaries without injecting spaces into CJK or Thai text.
use super::geometry::{Rect, median};
use super::ir::PdfChar;
use super::lines::space;
use super::script::no_space;

#[derive(Debug, Clone)]
pub(super) struct Word { pub chars: Vec<PdfChar>, pub text: String, pub bounds: Rect, pub space_before: bool, pub space_advance_pt: Option<f64> }
fn gap(a: &PdfChar, b: &PdfChar) -> f64 { b.loose_box.x0-a.loose_box.x1 }
pub(super) fn space_gap_threshold(font: f64, tracking: f64) -> f64 { let base = 0.18*font; if tracking > 0.06*font { base.max(tracking+0.1*font) } else { base } }
pub(super) fn letter_spaced(chars: &[PdfChar]) -> bool {
    let mut sorted: Vec<_> = chars.iter().collect(); sorted.sort_by(|a,b| a.loose_box.x0.partial_cmp(&b.loose_box.x0).unwrap_or(std::cmp::Ordering::Equal));
    let mut gaps = Vec::new(); let mut sizes = Vec::new(); let mut prev: Option<&PdfChar> = None; let mut saw_space = false;
    for c in sorted { if space(c.code) { saw_space = true; continue; } if c.code <= 0x1f { continue; } sizes.push(c.font_size); if let Some(p) = prev { if !saw_space { gaps.push(gap(p,c)); } } prev=Some(c); saw_space=false; }
    if gaps.len() < 4 { return false; } let font = median(&sizes).max(1.0); let med = median(&gaps);
    if med < 0.5*font || med > 2.0*font { return false; }
    gaps.iter().filter(|&&g| g >= 0.55*med && g <= 1.6*med).count() as f64 / gaps.len() as f64 >= 0.75
}
pub(super) fn median_char_gap(chars: &[PdfChar]) -> f64 {
    let mut gaps = Vec::new();
    for pair in chars.windows(2) { let a=&pair[0]; let b=&pair[1]; if space(a.code) || space(b.code) { continue; } let g=gap(a,b); let font=a.font_size.min(b.font_size).max(1.0); if g>0.0 && g<=0.15*font { gaps.push(g); } }
    median(&gaps)
}
fn tracked_pitch(chars: &[PdfChar]) -> Option<f64> {
    let mut pitches=Vec::new(); let mut negative=0; let mut prev: Option<&PdfChar>=None;
    for c in chars { if space(c.code) || c.code<=0x1f { continue; } if let Some(p)=prev { if !no_space(p.script) && !no_space(c.script) { let font=p.font_size.min(c.font_size); let font=if font==0.0 { 1.0 } else { font }; if gap(p,c)< -0.05*font { negative+=1; } pitches.push(c.origin_x-p.origin_x); } } prev=Some(c); }
    if pitches.len()<6 || (negative as f64)<pitches.len() as f64/2.0 { return None; } let med=median(&pitches); if med<=0.0 { return None; }
    let near=pitches.iter().filter(|&&p| (p-med).abs()<=0.25*med).count(); (near as f64 / pitches.len() as f64 >= 0.7).then_some(med)
}
fn real_gap_floor(chars: &[PdfChar]) -> Option<f64> {
    let mut gaps=Vec::new(); let mut previous: Option<&PdfChar>=None; let mut seen=false;
    for c in chars { if space(c.code) { if !c.is_generated { seen=true; } continue; } if c.code<=0x1f { continue; } if let Some(p)=previous { if seen && !no_space(c.script) && !no_space(p.script) { gaps.push(gap(p,c)); } } seen=false; previous=Some(c); }
    (!gaps.is_empty()).then(|| 0.5*median(&gaps))
}
fn flush(words: &mut Vec<Word>, current: &mut Vec<PdfChar>, space_before: bool, advance: Option<f64>) {
    if current.is_empty() { return; } let chars=std::mem::take(current); let text=chars.iter().map(|c| c.text.as_str()).collect(); let bounds=chars.iter().map(|c| c.bounds).reduce(Rect::union).unwrap(); words.push(Word { chars, text, bounds, space_before, space_advance_pt: advance });
}
pub(super) fn group_into_words(chars: &[PdfChar], infer: bool) -> Vec<Word> {
    let infer = infer && !letter_spaced(chars); let line_gap=median_char_gap(chars); let pitch=if infer { tracked_pitch(chars) } else { None }; let floor=real_gap_floor(chars);
    let mut words=Vec::new(); let mut current=Vec::new(); let mut pending=false; let mut pending_generated=true; let mut pending_char: Option<&PdfChar>=None; let mut pending_count=0usize; let mut previous: Option<&PdfChar>=None; let mut before=false; let mut advance=None;
    for c in chars {
        if c.code==2 { continue; }
        if space(c.code) { if !pending { pending_generated=true; pending_char=None; pending_count=0; } pending=true; pending_generated &= c.is_generated; pending_count+=1; if !c.is_generated && pending_char.is_none() { pending_char=Some(c); } continue; }
        if pending && pending_generated && (no_space(c.script) || previous.is_some_and(|p| no_space(p.script))) {
            let boundary=previous.is_some_and(|p| no_space(p.script)!=no_space(c.script)); let font=previous.map_or(c.font_size,|p| p.font_size.min(c.font_size)).max(1.0); let wide=previous.is_some_and(|p| gap(p,c)>=0.75*font); if !(boundary && wide) { pending=false; }
        }
        let under=match (floor,previous) { (Some(f),Some(p)) => !no_space(c.script) && !no_space(p.script) && gap(p,c)<f, _ => false };
        if pending && pending_generated && under { pending=false; }
        let inferred=previous.is_some_and(|p| { let mut font=p.font_size.min(c.font_size); if font==0.0 { font=p.font_size.max(c.font_size); } infer && !current.is_empty() && ((!under && !no_space(p.script) && !no_space(c.script) && gap(p,c)>space_gap_threshold(font.max(1.0),line_gap)) || pitch.is_some_and(|v| !no_space(p.script) && !no_space(c.script) && c.origin_x-p.origin_x>=1.35*v)) });
        if pending || inferred || no_space(c.script) || previous.is_some_and(|p| no_space(p.script)) {
            flush(&mut words,&mut current,before,advance); before=pending || inferred; advance=if pending && pending_count==1 { pending_char.map(|s| c.origin_x-s.origin_x) } else { None }; pending=false;
        }
        current.push(c.clone()); previous=Some(c);
    }
    flush(&mut words,&mut current,before,advance); words
}
