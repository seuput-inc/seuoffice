//! Dry-run matching and ink inspection for pending text edits.
use super::super::form_draw_tree::FormDrawTree;
use super::{
    ID, PdfiumSession, TextObject, buffer, call, field, handle, integer, norm, offset, ordered,
    page_index, reference, text, walk,
};
use super::{font, matching, rebuild};
use serde_json::json;
use shell_contract::{IpcValue as V, runtime::CancelToken};

fn object(fields: Vec<(&str, V)>) -> V {
    V::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn array(values: impl IntoIterator<Item = f64>) -> V {
    V::Array(values.into_iter().map(V::Number).collect())
}
fn reason(reason: &str) -> V {
    object(vec![("reason", V::String(reason.into()))])
}
fn matched_bounds(objects: &[TextObject]) -> [f64; 4] {
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for obj in objects {
        let r = obj.bounds;
        bounds = [
            bounds[0].min(r[0]),
            bounds[1].min(r[1]),
            bounds[2].max(r[2]),
            bounds[3].max(r[3]),
        ]
    }
    bounds
}
fn rect(edit: &V) -> Option<[f64; 4]> {
    let V::Array(values) = field(edit, "rect")? else {
        return None;
    };
    let n = |i: usize| match values.get(i) {
        Some(V::Number(n)) => Some(*n),
        _ => None,
    };
    Some([n(0)?, n(1)?, n(2)?, n(3)?])
}
fn base_color(
    session: &mut PdfiumSession,
    objects: &[TextObject],
) -> Result<Option<[u8; 3]>, String> {
    let Some(first) = ordered(objects).first().copied() else {
        return Ok(None);
    };
    buffer(session, 16, |session, b| {
        if integer(call(
            session,
            "FPDFPageObj_GetFillColor",
            &[
                reference(first.handle),
                reference(b),
                offset(b, 4),
                offset(b, 8),
                offset(b, 12),
            ],
        )?)? == 0
        {
            return Ok(None);
        }
        let raw = session.read(b, 0, 16).map_err(|e| e.to_string())?;
        Ok(Some([raw[0], raw[4], raw[8]]))
    })
}
fn details(session: &mut PdfiumSession, edit: &V, objects: &[TextObject]) -> Result<V, String> {
    let mut bounds = matched_bounds(objects);
    let old = text(field(edit, "oldText")).ok_or("invalid edited text")?;
    let joined = objects.iter().map(|o| o.text.as_str()).collect::<String>();
    let whole = norm(&joined) == norm(old);
    if !whole {
        let r = rect(edit).ok_or("invalid edited text rectangle")?;
        bounds[0] = r[0];
        bounds[2] = r[2]
    }
    let mut fields = vec![("reason", V::Null), ("bounds", array(bounds))];
    if whole {
        if let Some(colors) = rebuild::match_char_colors(session, objects, old)? {
            let mut ranges: Vec<(usize, usize, [u8; 3])> = Vec::new();
            for (i, color) in colors.into_iter().enumerate() {
                let Some(color) = color else { continue };
                if let Some(last) = ranges
                    .last_mut()
                    .filter(|last| last.1 == i && last.2 == color)
                {
                    last.1 = i + 1
                } else {
                    ranges.push((i, i + 1, color))
                }
            }
            if !ranges.is_empty() {
                fields.push((
                    "colorRuns",
                    V::Array(
                        ranges
                            .into_iter()
                            .map(|(start, end, color)| {
                                object(vec![
                                    ("start", V::Number(start as f64)),
                                    ("end", V::Number(end as f64)),
                                    ("color", array(color.into_iter().map(f64::from))),
                                ])
                            })
                            .collect(),
                    ),
                ))
            }
        }
    }
    if let Some(color) = base_color(session, objects)? {
        fields.push(("baseColor", array(color.into_iter().map(f64::from))))
    }
    Ok(object(fields))
}

pub(super) fn validate(bytes: &[u8], edits: &[V], cancel: &CancelToken) -> Result<V, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut session =
        PdfiumSession::open_with_cancel(bytes, None, cancel.clone()).map_err(|e| e.to_string())?;
    let doc = session.document();
    let count = integer(call(&mut session, "FPDF_GetPageCount", &[reference(doc)])?)?;
    let mut results = vec![object(vec![("reason", V::Null)]); edits.len()];
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let Some(page) = page_index(field(edit, "pageIndex"))
            .filter(|p| i64::try_from(*p).is_ok_and(|p| p < count))
        else {
            results[i] = reason("page does not exist");
            continue;
        };
        if let Some((_, indices)) = groups.iter_mut().find(|(p, _)| *p == page) {
            indices.push(i)
        } else {
            groups.push((page, vec![i]))
        }
    }
    let mut tree: Option<FormDrawTree> = None;
    for (index, indices) in groups {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let Some(page) = handle(call(
            &mut session,
            "FPDF_LoadPage",
            &[reference(doc), json!(index)],
        )?)?
        else {
            for i in indices {
                results[i] = reason(&format!("could not load page {}", index + 1))
            }
            continue;
        };
        let result = (|| {
            let text_page = handle(call(&mut session, "FPDFText_LoadPage", &[reference(page)])?)?
                .ok_or("could not load PDF text page")?;
            let result: Result<(), String> = (|| {
                let mut objects = Vec::new();
                walk(
                    &mut session,
                    page,
                    text_page,
                    false,
                    ID,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    0,
                    &mut objects,
                )?;
                for i in indices {
                    if cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    let edit = &edits[i];
                    let found = match matching::locate(&objects, edit) {
                        Ok(found) => found,
                        Err(error) => {
                            results[i] = reason(&error);
                            continue;
                        }
                    };
                    if matches!(field(edit, "translate"), Some(V::Array(_))) && !found.whole {
                        results[i] = reason("the text block cannot be moved as one unit");
                        continue;
                    }
                    let matches = found
                        .indices
                        .iter()
                        .map(|i| objects[*i].clone())
                        .collect::<Vec<_>>();
                    if matches.iter().any(|o| !o.chain.is_empty()) {
                        if tree.is_none() {
                            tree = Some(FormDrawTree::load(bytes)?)
                        }
                        if matches.iter().any(|o| {
                            !o.chain.is_empty()
                                && !tree
                                    .as_ref()
                                    .is_some_and(|t| t.exclusive(index, &o.ordinals))
                        }) {
                            results[i] =
                                reason("the text lives in a template that is drawn more than once");
                            continue;
                        }
                    }
                    results[i] = details(&mut session, edit, &matches)?;
                }
                Ok(())
            })();
            let closed = call(&mut session, "FPDFText_ClosePage", &[reference(text_page)]);
            result?;
            closed?;
            Ok::<(), String>(())
        })();
        let closed = call(&mut session, "FPDF_ClosePage", &[reference(page)]);
        result?;
        closed?;
    }
    Ok(V::Array(results))
}

pub(super) fn can_draw(text: &str, id: Option<&str>, bold: bool, italic: bool) -> bool {
    let drawn = text.replace('\n', "");
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        let styles = if bold || italic {
            vec![(bold, italic), (false, false)]
        } else {
            vec![(false, false)]
        };
        for (bold, italic) in styles {
            if font::chosen(id, bold, italic)
                .is_some_and(|bytes| main_core::pdf::font_cmap::font_covers_text(&bytes, &drawn))
            {
                return true;
            }
        }
    }
    font::fallback(&drawn).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_include_all_matched_ink() {
        let obj = |bounds| TextObject {
            handle: 0,
            index: 0,
            font: None,
            text: String::new(),
            bounds,
            chain: vec![],
            ordinals: vec![],
        };
        assert_eq!(
            matched_bounds(&[obj([0.0, 2.0, 4.0, 5.0]), obj([8.0, 1.0, 9.0, 6.0])]),
            [0.0, 1.0, 9.0, 6.0]
        );
    }
    #[test]
    fn missing_page_failure_keeps_original_shape() {
        assert_eq!(
            reason("page does not exist"),
            object(vec![("reason", V::String("page does not exist".into()))])
        );
    }
}
