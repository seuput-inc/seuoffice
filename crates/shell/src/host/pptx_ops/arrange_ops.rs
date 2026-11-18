use std::cmp::Ordering;
use jscompat::{JsNum, math_round};
use pptx_engine::align::{align_rects, distribute_rects, AlignPoint, AlignRect};
use pptx_engine::animation::element_spid;
use pptx_engine::deck::{connection_site_for_side, set_element_connection, update_connectors_for_moved, ConnectionPatch, ConnectionSide};
use pptx_engine::identity::{element_durable_id, matches_element_ref};
use pptx_engine::insert::{add_element, NewElementOptions, NewStroke};
use pptx_engine::types::{ConnectionEnd, EmuRect, Slide, SlideElement, Transform};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use super::registry::{field, op_name, record, resolve_slide, string, OpContext, SlideTarget};

fn js_string(v: &IpcValue) -> String {
    match v { IpcValue::String(s) => s.clone(), IpcValue::Number(n) => jscompat::number_to_string(*n), IpcValue::Bool(b) => b.to_string(), IpcValue::Null => "null".into(), IpcValue::Undefined => "undefined".into(), IpcValue::Array(a) => a.iter().map(js_string).collect::<Vec<_>>().join(","), _ => "[object Object]".into() }
}
fn items(slide: &Slide) -> &[SlideElement] { slide.elements.as_deref().unwrap_or(&[]) }
fn available(slide: &Slide) -> Result<String, String> { items(slide).iter().map(|e| Ok(match element_durable_id(e)? { Some(d) => format!("{} ({d})", e.id()), None => e.id().to_owned() })).collect::<Result<Vec<_>, String>>().map(|a| a.join(", ")) }
fn index_of(slide: &Slide, id: &str) -> Result<Option<usize>, String> { for (i, e) in items(slide).iter().enumerate() { if matches_element_ref(e, id)? { return Ok(Some(i)); } } Ok(None) }
fn top_level(op: &IpcValue, ctx: &mut OpContext<'_>, min: usize) -> Result<(SlideTarget, Vec<usize>), String> {
    let target = resolve_slide(ctx, op, false)?;
    let SlideTarget::Slide(slide_index) = &target else { unreachable!() };
    let slide = ctx.slide(&target); let name = op_name(op);
    let ids = match field(op, "els") { Some(IpcValue::Array(a)) if a.len() >= min => a, _ => return Err(format!("op \"{name}\" needs \"els\": at least {min} element ids.")) };
    let mut indices = Vec::with_capacity(ids.len());
    for raw in ids {
        let id = js_string(raw);
        let Some(i) = index_of(slide, &id)? else {
            for el in items(slide) { if let SlideElement::Group(g) = el { for child in g.children.as_deref().unwrap_or(&[]) { if matches_element_ref(child, &id)? { return Err(format!("op \"{name}\": \"{id}\" is inside a group — arrange the group itself, or ungroupElement first.")); } } } }
            return Err(format!("op \"{name}\": no element \"{id}\" on slide {slide_index}. Available: [{}].", available(slide)?));
        };
        if indices.contains(&i) { return Err(format!("op \"{name}\": \"els\" lists the same element twice.")); }
        indices.push(i);
    }
    Ok((target, indices))
}
fn to_slide(op: &IpcValue) -> Result<bool, String> {
    match field(op, "to") { None | Some(IpcValue::Undefined) => Ok(false), Some(IpcValue::String(s)) if s == "selection" => Ok(false), Some(IpcValue::String(s)) if s == "slide" => Ok(true), _ => Err(format!("op \"{}\": \"to\" must be \"selection\" (default) or \"slide\".", op_name(op))) }
}
fn transform(e: &SlideElement) -> Result<Transform, String> { pptx_engine::with_element!(e, e => *e.transform).ok_or_else(|| "element has no transform".into()) }
fn rects(slide: &Slide, indices: &[usize]) -> Result<Vec<AlignRect>, String> { indices.iter().map(|i| { let r = transform(&items(slide)[*i])?.offset; Ok(AlignRect { x: r.x.0, y: r.y.0, w: r.cx.0, h: r.cy.0 }) }).collect() }
fn offset_value(id: &str, o: EmuRect) -> IpcValue { IpcValue::Object(vec![("id".into(), IpcValue::String(id.into())), ("x".into(), IpcValue::Number(o.x.0)), ("y".into(), IpcValue::Number(o.y.0)), ("cx".into(), IpcValue::Number(o.cx.0)), ("cy".into(), IpcValue::Number(o.cy.0))]) }
fn move_to(slide: &mut Slide, indices: &[usize], positions: &[AlignPoint]) -> Result<usize, String> {
    let mut moved = Vec::new();
    for (&i, p) in indices.iter().zip(positions) {
        let e = &mut slide.elements.as_mut().ok_or("slide has no elements")?[i];
        let mut t = transform(e)?; let x = math_round(p.x.0); let y = math_round(p.y.0);
        if x == t.offset.x.0 && y == t.offset.y.0 { continue; }
        t.offset.x = JsNum(x); t.offset.y = JsNum(y); moved.push(e.id().to_owned());
        pptx_engine::with_element!(e, el => { el.transform.set(t); el.dirty_transform.set(true); });
    }
    if !moved.is_empty() { update_connectors_for_moved(slide, &moved); }
    Ok(moved.len())
}
fn mode(op: &IpcValue) -> Result<&'static str, String> { match field(op, "mode").and_then(string) { Some("left") => Ok("left"), Some("centerH") => Ok("center-h"), Some("right") => Ok("right"), Some("top") => Ok("top"), Some("centerV") => Ok("center-v"), Some("bottom") => Ok("bottom"), _ => Err("op \"alignElements\" needs \"mode\": left/centerH/right/top/centerV/bottom.".into()) } }
fn axis(op: &IpcValue) -> Result<&str, String> { match field(op, "axis").and_then(string) { Some("horizontal") => Ok("horizontal"), Some("vertical") => Ok("vertical"), _ => Err("op \"distributeElements\" needs \"axis\": \"horizontal\" or \"vertical\".".into()) } }
fn side(op: &IpcValue, key: &str) -> Result<Option<ConnectionSide>, String> { match field(op, key) { None | Some(IpcValue::Undefined) => Ok(None), Some(IpcValue::String(s)) => match s.as_str() { "top" => Ok(Some(ConnectionSide::Top)), "left" => Ok(Some(ConnectionSide::Left)), "bottom" => Ok(Some(ConnectionSide::Bottom)), "right" => Ok(Some(ConnectionSide::Right)), _ => Err(format!("op \"addConnector\": \"{key}\" must be top/left/bottom/right.")) }, _ => Err(format!("op \"addConnector\": \"{key}\" must be top/left/bottom/right.")) } }
fn side_name(s: ConnectionSide) -> &'static str { match s { ConnectionSide::Top => "top", ConnectionSide::Left => "left", ConnectionSide::Bottom => "bottom", ConnectionSide::Right => "right" } }
fn midpoint(el: &SlideElement, side: ConnectionSide) -> Result<(f64, f64), String> { let t = transform(el)?; let r = t.offset; let (x,y) = match side { ConnectionSide::Top => (0.0,-r.cy.0/2.0), ConnectionSide::Left => (-r.cx.0/2.0,0.0), ConnectionSide::Bottom => (0.0,r.cy.0/2.0), ConnectionSide::Right => (r.cx.0/2.0,0.0) }; let angle = t.rot.0 / 60000.0 * std::f64::consts::PI / 180.0; Ok((r.x.0+r.cx.0/2.0+x*angle.cos()-y*angle.sin(),r.y.0+r.cy.0/2.0+x*angle.sin()+y*angle.cos())) }
fn endpoint(op: &IpcValue, slide: &Slide, slide_index: usize, key: &str) -> Result<usize, String> {
    let id = field(op, key).and_then(string).filter(|s| !s.is_empty()).ok_or_else(|| format!("op \"addConnector\" needs \"{key}\": an element id on slide {slide_index}."))?;
    let Some(i) = index_of(slide, id)? else { return Err(format!("op \"addConnector\": no element \"{id}\" on slide {slide_index}. Available: [{}].", available(slide)?)); };
    if element_spid(&items(slide)[i]).is_none() { return Err(format!("op \"addConnector\": \"{id}\" has no shape id to attach to.")); } Ok(i)
}
fn arrow(op: &IpcValue) -> Result<&str, String> { match field(op, "arrow") { None | Some(IpcValue::Undefined) => Ok("end"), Some(IpcValue::String(s)) if ["none", "end", "both"].contains(&s.as_str()) => Ok(s), _ => Err("op \"addConnector\": \"arrow\" must be none/end/both.".into()) } }
fn connector_kind(op: &IpcValue) -> Result<&str, String> { match field(op, "kind") { None | Some(IpcValue::Undefined) => Ok("straight"), Some(IpcValue::String(s)) if ["straight", "elbow", "curved"].contains(&s.as_str()) => Ok(s), _ => Err("op \"addConnector\": \"kind\" must be straight/elbow/curved.".into()) } }
fn line(op: &IpcValue) -> Result<(String, f64, Option<String>), String> {
    let l = field(op, "line"); let value = |k| l.and_then(|v| field(v, k)).filter(|v| !matches!(v, IpcValue::Undefined));
    let color = match value("color") { None => "#000000", Some(IpcValue::String(s)) if { let hex = s.trim().strip_prefix('#').unwrap_or(s.trim()); (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|c| c.is_ascii_hexdigit()) } => s, Some(v) => return Err(format!("op \"addConnector\": \"line.color\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\"), got {}.", js_string(v))) };
    let finite = |key| -> Result<Option<f64>, String> { match value(key) { None => Ok(None), Some(IpcValue::Number(n)) if n.is_finite() => Ok(Some(*n)), _ => Err(format!("op \"addConnector\": \"line.{key}\" must be a finite number.")) } };
    let pt = finite("widthPt")?; let emu = finite("widthEmu")?;
    let dash = value("dash").map(js_string); if dash.as_deref().is_some_and(|d| !["solid","dash","dot","lgDash","dashDot","lgDashDot","sysDash","sysDot","sysDashDot"].contains(&d)) { return Err("op \"addConnector\": \"line.dash\" must be one of solid/dash/dot/lgDash/dashDot/lgDashDot/sysDash/sysDot/sysDashDot.".into()); }
    let width = math_round(emu.unwrap_or_else(|| pt.map_or(12700.0, |p| p * 12700.0))); if !width.is_finite() || width <= 0.0 { return Err("op \"addConnector\": line width must be a finite number > 0.".into()); }
    Ok((color.to_owned(), width, dash.filter(|d| d != "solid")))
}
fn connector_targets(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(SlideTarget, usize, usize), String> {
    let target = resolve_slide(ctx, op, false)?; let SlideTarget::Slide(index) = &target else { unreachable!() }; let slide = ctx.slide(&target);
    let from = endpoint(op, slide, *index, "from")?; let to = endpoint(op, slide, *index, "to")?;
    if from == to { return Err("op \"addConnector\": \"from\" and \"to\" must differ.".into()); }
    connector_kind(op)?; side(op, "fromSide")?; side(op, "toSide")?; arrow(op)?; line(op)?;
    Ok((target, from, to))
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    Some(match op_name(op) {
        "alignElements" => (|| { mode(op)?; let slide = to_slide(op)?; top_level(op, ctx, if slide { 1 } else { 2 })?; Ok(()) })(),
        "distributeElements" => (|| { axis(op)?; let slide = to_slide(op)?; top_level(op, ctx, if slide { 1 } else { 3 })?; Ok(()) })(),
        "addConnector" => connector_targets(op, ctx).map(|_| ()),
        _ => return None,
    })
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if !matches!(op_name(op), "alignElements" | "distributeElements" | "addConnector") { return None; }
    Some((|| {
        let mut result = record(op);
        if op_name(op) == "addConnector" {
            let (target, from, to) = connector_targets(op, ctx)?;
            let slide = ctx.slide(&target); let a = &items(slide)[from]; let b = &items(slide)[to];
            let candidates = [ConnectionSide::Top, ConnectionSide::Left, ConnectionSide::Bottom, ConnectionSide::Right];
            let mut best = (side(op, "fromSide")?.unwrap_or(ConnectionSide::Right), side(op, "toSide")?.unwrap_or(ConnectionSide::Left)); let mut distance = f64::INFINITY;
            for s in candidates { if side(op, "fromSide")?.is_some_and(|v| v != s) { continue; } for t in candidates { if side(op, "toSide")?.is_some_and(|v| v != t) { continue; } let p = midpoint(a,s)?; let q = midpoint(b,t)?; let d = (p.0-q.0).powi(2)+(p.1-q.1).powi(2); if d < distance { distance=d; best=(s,t); } } }
            let p = midpoint(a,best.0)?; let q = midpoint(b,best.1)?; let start = ConnectionEnd { id: element_spid(a).ok_or("missing shape id")?, idx: JsNum(connection_site_for_side(a,best.0)) }; let end = ConnectionEnd { id: element_spid(b).ok_or("missing shape id")?, idx: JsNum(connection_site_for_side(b,best.1)) };
            let offset = EmuRect { x: JsNum(math_round(p.0.min(q.0))), y: JsNum(math_round(p.1.min(q.1))), cx: JsNum(math_round((p.0-q.0).abs())), cy: JsNum(math_round((p.1-q.1).abs())) };
            let arrow = arrow(op)?; let (color,width,dash) = line(op)?; let kind = match (connector_kind(op)?, arrow) { ("straight", "none") => "line", ("straight", _) => "lineArrow", ("elbow", _) => "lineBent", _ => "lineCurved" };
            let opts = NewElementOptions { kind: kind.into(), offset, paragraphs: None, fill_color: None, stroke: Some(NewStroke { color, width_emu: JsNum(width) }), body_pr: None, adjustments: None, flip_h: None, flip_v: None };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as f64;
            let slide = ctx.slide_mut(&target); let inserted = add_element(slide, &opts, &jscompat::random_uuid(), now)?; let id = inserted.id.as_deref().unwrap_or_default().to_owned();
            let el = slide.elements.as_mut().and_then(|e| e.last_mut()).ok_or("inserted connector missing")?;
            if let SlideElement::Text(e) = el { let anchor = e.anchor.as_mut().ok_or("connector anchor missing")?; let xml = jscompat::js_regex!(r"<a:headEnd\b[^>]*/>|<a:tailEnd\b[^>]*/>", "g").replace_all(&anchor.original_xml, |_| String::new()); let ends = format!("{}{}{}", dash.map(|d| format!("<a:prstDash val=\"{d}\"/>")).unwrap_or_default(), if arrow == "both" { "<a:headEnd type=\"triangle\" w=\"med\" len=\"med\"/>" } else { "" }, if arrow != "none" { "<a:tailEnd type=\"triangle\" w=\"med\" len=\"med\"/>" } else { "" }); anchor.original_xml = xml.replacen("</a:ln>", &format!("{ends}</a:ln>"), 1).into(); let mut t = (*e.transform).ok_or("connector transform missing")?; t.flip_h = p.0 > q.0; t.flip_v = p.1 > q.1; e.transform.set(t); e.dirty_transform.set(true); }
            set_element_connection(slide, &id, &ConnectionPatch { start: Some(Some(start)), end: Some(Some(end)) });
            result.created = Some(vec![IpcValue::String(id)]); result.after = IpcValue::Object(vec![("fromSide".into(), IpcValue::String(side_name(best.0).into())), ("toSide".into(), IpcValue::String(side_name(best.1).into())), ("offset".into(), IpcValue::Object(vec![("x".into(), IpcValue::Number(offset.x.0)), ("y".into(), IpcValue::Number(offset.y.0)), ("cx".into(), IpcValue::Number(offset.cx.0)), ("cy".into(), IpcValue::Number(offset.cy.0))]))]);
        } else {
            let slide_ref = to_slide(op)?; let is_align = op_name(op) == "alignElements"; let (target, indices) = top_level(op, ctx, if slide_ref { 1 } else if is_align { 2 } else { 3 })?;
            let slide = ctx.slide(&target); let rects = rects(slide, &indices)?; let before = indices.iter().map(|i| Ok(offset_value(items(slide)[*i].id(), transform(&items(slide)[*i])?.offset))).collect::<Result<Vec<_>, String>>()?;
            let size = ctx.opened.deck.size;
            let positions = if is_align { align_rects(&rects, mode(op)?, slide_ref.then_some(AlignRect { x: 0.0, y: 0.0, w: size.cx.0, h: size.cy.0 })) } else if !slide_ref { distribute_rects(&rects, axis(op)?) } else { let horizontal = axis(op)? == "horizontal"; let span = if horizontal { size.cx.0 } else { size.cy.0 }; let gap = (span - rects.iter().map(|r| if horizontal { r.w } else { r.h }).sum::<f64>()) / (rects.len()+1) as f64; let mut order: Vec<usize> = (0..rects.len()).collect(); order.sort_by(|a,b| (if horizontal { rects[*a].x } else { rects[*a].y }).partial_cmp(&(if horizontal { rects[*b].x } else { rects[*b].y })).unwrap_or(Ordering::Equal)); let mut pos: Vec<_> = rects.iter().map(|r| AlignPoint { x: JsNum(r.x), y: JsNum(r.y) }).collect(); let mut cursor=gap; for i in order { if horizontal { pos[i].x=JsNum(cursor); cursor+=rects[i].w+gap; } else { pos[i].y=JsNum(cursor); cursor+=rects[i].h+gap; } } pos };
            let moved = move_to(ctx.slide_mut(&target), &indices, &positions)?; result.before = IpcValue::Array(before); let field_name = if is_align { "mode" } else { "axis" }; result.after = IpcValue::Object(vec![(field_name.into(), field(op, field_name).cloned().unwrap_or(IpcValue::Undefined)), ("to".into(), IpcValue::String(if slide_ref { "slide" } else { "selection" }.into())), ("moved".into(), IpcValue::Number(moved as f64))]);
        }
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connector_width_rejects_overflow_and_rounds_points() {
        let make = |n| IpcValue::Object(vec![("line".into(), IpcValue::Object(vec![("widthPt".into(), IpcValue::Number(n))]))]);
        assert_eq!(line(&make(1.5)).unwrap().1, 19050.0);
        assert!(line(&make(1e308)).is_err());
        assert!(line(&make(0.0)).is_err());
    }
    #[test]
    fn alignment_respects_slide_reference_and_invalid_modes() {
        let rects = [AlignRect { x: 5.0, y: 10.0, w: 20.0, h: 30.0 }];
        let points = align_rects(&rects, "center-h", Some(AlignRect { x: 0.0, y: 0.0, w: 100.0, h: 80.0 }));
        assert_eq!(points[0].x.0, 40.0);
        let op = IpcValue::Object(vec![("mode".into(), IpcValue::String("constructor".into()))]);
        assert!(mode(&op).is_err());
    }
    #[test]
    fn connector_is_glued_to_inserted_shapes() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        let mut opts = NewElementOptions { kind: "rect".into(), offset: EmuRect { x: JsNum(0.0), y: JsNum(0.0), cx: JsNum(100.0), cy: JsNum(100.0) }, paragraphs: None, fill_color: None, stroke: None, body_pr: None, adjustments: None, flip_h: None, flip_v: None };
        let first = add_element(&mut opened.deck.slides[0], &opts, "00000000-0000-4000-8000-000000000001", 1.0).unwrap();
        opts.offset.x = JsNum(500.0);
        let second = add_element(&mut opened.deck.slides[0], &opts, "00000000-0000-4000-8000-000000000002", 2.0).unwrap();
        let mut ctx = OpContext { opened: &mut opened, parts: Default::default(), touched_parts: Default::default(), withhold: false };
        let op = IpcValue::Object(vec![("op".into(), IpcValue::String("addConnector".into())), ("target".into(), IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0))])), ("from".into(), IpcValue::String(first.id.as_deref().unwrap().into())), ("to".into(), IpcValue::String(second.id.as_deref().unwrap().into()))]);
        validate(&op, &mut ctx).unwrap().unwrap();
        let result = apply(&op, &mut ctx).unwrap().unwrap();
        assert_eq!(field(&result.after, "fromSide").and_then(string), Some("right"));
        assert_eq!(field(&result.after, "toSide").and_then(string), Some("left"));
        let created = result.created.unwrap();
        let id = string(&created[0]).unwrap();
        let connector = items(&ctx.opened.deck.slides[0]).iter().find(|e| e.id() == id).unwrap();
        let xml = pptx_engine::with_element!(connector, e => e.anchor.as_ref().unwrap().original_xml.to_string());
        assert!(xml.contains("<a:stCxn"));
        assert!(xml.contains("<a:endCxn"));
    }
}
