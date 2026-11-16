//! Legacy note comments and VML drawing updates.
mod threaded;
use super::{
    cells,
    package::PackageEditor,
    sheets,
    state::{PlannerState, resolve_rel_target},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use jscompat::TryJsRegex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const VML: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";
const EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>";
const VML_HEADER: &str = "<xml xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:o=\"urn:schemas-microsoft-com:office:office\" xmlns:x=\"urn:schemas-microsoft-com:office:excel\"><o:shapelayout v:ext=\"edit\"><o:idmap v:ext=\"edit\" data=\"1\"/></o:shapelayout><v:shapetype id=\"_x0000_t202\" coordsize=\"21600,21600\" o:spt=\"202\" path=\"m,l,21600r21600,l21600,xe\"><v:stroke joinstyle=\"miter\"/><v:path gradientshapeok=\"t\" o:connecttype=\"rect\"/></v:shapetype>";
fn re(s: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(s, "").map_err(|e| e.to_string())
}
fn replace<F>(s: &str, p: &str, mut f: F) -> Result<String, String>
where
    F: FnMut(&str, &[Option<String>]) -> Result<String, String>,
{
    let r = re(p)?;
    let mut out = String::new();
    let mut at = 0;
    for m in r.try_find_iter(s) {
        let m = m.map_err(|e| e.to_string())?;
        out.push_str(&s[at..m.start()]);
        let g = (1..7)
            .map(|i| m.get(i).map(str::to_owned))
            .collect::<Vec<_>>();
        out.push_str(&f(m.as_str(), &g)?);
        at = m.end();
    }
    out.push_str(&s[at..]);
    Ok(out)
}
fn attr(s: &str, n: &str) -> Result<Option<String>, String> {
    let p = format!(r#"\b{n}="([^"]*)""#);
    Ok(re(&p)?
        .try_exec(s)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1).map(str::to_owned)))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn xlsx(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.char_indices() {
        let b = &s.as_bytes()[i..];
        if c == '_'
            && b.get(1) == Some(&b'x')
            && b.get(2..6)
                .is_some_and(|v| v.iter().all(u8::is_ascii_hexdigit))
            && b.get(6) == Some(&b'_')
        {
            out.push_str("_x005F_");
        } else if matches!(c,'\u{0}'..='\u{8}'|'\u{b}'|'\u{c}'|'\u{e}'..='\u{1f}'|'\r'|'\u{fffe}'|'\u{ffff}')
        {
            out.push_str(&format!("_x{:04X}_", c as u32));
        } else {
            out.push(c);
        }
    }
    escape(&out)
}
fn rels_part(path: &str) -> String {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    format!("{dir}/_rels/{file}.rels")
}
fn target(xml: &str, kind: &str) -> Result<Option<String>, String> {
    for m in re(r"<Relationship\b[^>]*/?>")?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        if attr(m.as_str(), "Type")?.as_deref() == Some(kind) {
            return attr(m.as_str(), "Target");
        }
    }
    Ok(None)
}
fn remove_rel(xml: &str, kind: &str) -> Result<String, String> {
    let mut done = false;
    replace(xml, r"<Relationship\b[^>]*/?>", |whole, _| {
        if !done && attr(whole, "Type")?.as_deref() == Some(kind) {
            done = true;
            Ok(String::new())
        } else {
            Ok(whole.into())
        }
    })
}
fn append_rel(xml: &str, id: &str, kind: &str, target: &str) -> String {
    xml.replacen(
        "</Relationships>",
        &format!("<Relationship Id=\"{id}\" Type=\"{kind}\" Target=\"{target}\"/></Relationships>"),
        1,
    )
}
fn free(pkg: &mut PackageEditor<'_>, prefix: &str, suffix: &str) -> Result<String, String> {
    for i in 1..10000 {
        let p = format!("{prefix}{i}{suffix}");
        if !pkg.has(&p)? {
            return Ok(p);
        }
    }
    Err("No free part name for the comments part.".into())
}
fn remove_override(
    pkg: &mut PackageEditor<'_>,
    path: &str,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let xml = pkg.read_text("[Content_Types].xml")?;
    let next = replace(&xml, r"<Override\b[^>]*/>", |s, _| {
        if attr(s, "PartName")?.as_deref() == Some(format!("/{path}").as_str()) {
            Ok(String::new())
        } else {
            Ok(s.into())
        }
    })?;
    if next != *xml {
        pkg.write("[Content_Types].xml", next)?;
        touched.insert("[Content_Types].xml".into());
    }
    Ok(())
}
fn note_ref(note: &Value) -> Result<String, String> {
    Ok(cells::address(
        cells::coordinate(note, "row")? + 1,
        cells::coordinate(note, "column")?,
    ))
}
#[derive(Clone)]
struct Legacy {
    author: String,
    text: String,
}
fn comments(notes: &[Value], overrides: &HashMap<String, Legacy>) -> Result<String, String> {
    let mut authors = Vec::<String>::new();
    let mut body = String::new();
    for note in notes {
        let key = note_ref(note)?;
        let fallback = Legacy {
            author: note
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            text: note
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
        };
        let entry = overrides.get(&key).unwrap_or(&fallback);
        let id = authors
            .iter()
            .position(|v| v == &entry.author)
            .unwrap_or_else(|| {
                authors.push(entry.author.clone());
                authors.len() - 1
            });
        body.push_str(&format!("<comment ref=\"{key}\" authorId=\"{id}\"><text><t xml:space=\"preserve\">{}</t></text></comment>",xlsx(&entry.text)));
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<comments xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><authors>{}</authors><commentList>{body}</commentList></comments>",
        authors
            .iter()
            .map(|s| format!("<author>{}</author>", xlsx(s)))
            .collect::<String>()
    ))
}
#[derive(Clone)]
struct Picture {
    fill: String,
    width: Option<String>,
    height: Option<String>,
}
fn strip_shapes(xml: &str) -> Result<String, String> {
    replace(xml, r"<v:shape\b(?![^>]*/>)[\s\S]*?</v:shape>", |s, _| {
        Ok(if s.contains("ObjectType=\"Note\"") {
            String::new()
        } else {
            s.into()
        })
    })
}
fn carried(xml: &str) -> Result<HashMap<String, Picture>, String> {
    let mut map = HashMap::new();
    for m in re(r"<v:shape\b[\s\S]*?</v:shape>")?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        let s = m.as_str();
        if !s.contains("ObjectType=\"Note\"") {
            continue;
        }
        let fill = re(r"<v:fill\b[^>]*/>")?
            .try_exec(s)
            .map_err(|e| e.to_string())?
            .map(|m| m.as_str().to_owned())
            .filter(|f| {
                f.contains("type=\"frame\"") && (f.contains("relid=\"") || f.contains("r:id=\""))
            })
            .or_else(|| {
                re(r#"<v:imagedata\b[^>]*\br:id="([^"]+)"[^>]*/>"#)
                    .ok()?
                    .try_exec(s)
                    .ok()
                    .flatten()
                    .map(|m| m.as_str().to_owned())
            });
        let capture = |p: &str| -> Result<Option<String>, String> {
            Ok(re(p)?
                .try_exec(s)
                .map_err(|e| e.to_string())?
                .and_then(|m| m.get(1).map(str::to_owned)))
        };
        let (Some(fill), Some(row), Some(col)) = (
            fill,
            capture(r"<x:Row>(\d+)</x:Row>")?,
            capture(r"<x:Column>(\d+)</x:Column>")?,
        ) else {
            continue;
        };
        let style = attr(s, "style")?.unwrap_or_default();
        let prop = |name: &str| {
            style
                .split(';')
                .find_map(|v| v.trim().strip_prefix(name).map(str::to_owned))
        };
        map.insert(
            format!("{col}:{row}"),
            Picture {
                fill,
                width: prop("width:"),
                height: prop("height:"),
            },
        );
    }
    Ok(map)
}
fn shape(
    note: &Value,
    index: usize,
    first: usize,
    picture: Option<&Picture>,
) -> Result<String, String> {
    let row = cells::coordinate(note, "row")?;
    let col = cells::coordinate(note, "column")?;
    let (size, paint) = if let Some(p) = picture {
        (
            format!(
                "{}{}",
                p.width
                    .as_ref()
                    .map(|v| format!("width:{v};"))
                    .unwrap_or_default(),
                p.height
                    .as_ref()
                    .map(|v| format!("height:{v};"))
                    .unwrap_or_default()
            ),
            format!(" o:insetmode=\"auto\">{}", p.fill),
        )
    } else {
        ("width:108pt;height:60pt;".into()," fillcolor=\"#ffffe1\" o:insetmode=\"auto\"><v:fill color2=\"#ffffe1\"/><v:shadow on=\"t\" color=\"black\" obscured=\"t\"/>".into())
    };
    Ok(format!(
        "<v:shape id=\"_x0000_s{}\" type=\"#_x0000_t202\" style=\"position:absolute;margin-left:80pt;margin-top:2pt;{size}z-index:{};visibility:hidden\"{paint}<v:path o:connecttype=\"none\"/><v:textbox style=\"mso-direction-alt:auto\"><div style=\"text-align:left\"></div></v:textbox><x:ClientData ObjectType=\"Note\"><x:MoveWithCells/><x:SizeWithCells/><x:Anchor>{},15,{row},2,{},15,{},2</x:Anchor><x:AutoFill>False</x:AutoFill><x:Row>{row}</x:Row><x:Column>{col}</x:Column></x:ClientData></v:shape>",
        first + index,
        index + 1,
        col + 1,
        col + 4,
        row + 4
    ))
}
fn picture(
    pkg: &mut PackageEditor<'_>,
    vml: &str,
    p: &Value,
    touched: &mut HashSet<String>,
) -> Result<Picture, String> {
    let mime = p
        .get("mediaType")
        .and_then(Value::as_str)
        .ok_or("Note picture type missing")?;
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        _ => return Err("Invalid note picture type".into()),
    };
    let media = free(pkg, "xl/media/image", &format!(".{ext}"))?;
    let bytes = STANDARD
        .decode(
            p.get("base64")
                .and_then(Value::as_str)
                .ok_or("Note picture data missing")?,
        )
        .map_err(|e| e.to_string())?;
    pkg.add_binary(&media, bytes)?;
    touched.insert(media.clone());
    let ct = pkg.read_text("[Content_Types].xml")?;
    if !ct.contains(&format!("Extension=\"{ext}\"")) {
        pkg.write(
            "[Content_Types].xml",
            ct.replacen(
                "</Types>",
                &format!("<Default Extension=\"{ext}\" ContentType=\"{mime}\"/></Types>"),
                1,
            ),
        )?;
        touched.insert("[Content_Types].xml".into());
    }
    let rp = rels_part(vml);
    let exists = pkg.has(&rp)?;
    let xml = if exists {
        (*pkg.read_text(&rp)?).clone()
    } else {
        EMPTY.into()
    };
    let id = sheets::next_free_relationship_id(&xml);
    let target = format!("../media/{}", media.rsplit('/').next().unwrap_or(""));
    let next = append_rel(
        &xml,
        &id,
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image",
        &target,
    );
    if exists {
        pkg.write(&rp, next)?
    } else {
        pkg.add(&rp, next)
    }
    touched.insert(rp);
    let w = p
        .get("widthPx")
        .and_then(Value::as_f64)
        .ok_or("Note picture width missing")?
        .round()
        .max(1.)
        * 0.75;
    let h = p
        .get("heightPx")
        .and_then(Value::as_f64)
        .ok_or("Note picture height missing")?
        .round()
        .max(1.)
        * 0.75;
    let scale = (255. / w.max(h)).min(1.);
    let pt = |v: f64| {
        format!(
            "{}pt",
            jscompat::number_to_string((v * scale * 100.).round().max(100.) / 100.)
        )
    };
    let title = p
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|v| format!(" o:title=\"{}\"", escape(v)))
        .unwrap_or_default();
    Ok(Picture {
        fill: format!(
            "<v:fill o:relid=\"{id}\"{title} recolor=\"t\" rotate=\"t\" type=\"frame\"/>"
        ),
        width: Some(pt(w)),
        height: Some(pt(h)),
    })
}

fn apply_sheet(
    pkg: &mut PackageEditor<'_>,
    path: &str,
    notes: &[Value],
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let relpath = rels_part(path);
    let has_rels = pkg.has(&relpath)?;
    let mut rels = if has_rels {
        (*pkg.read_text(&relpath)?).clone()
    } else {
        EMPTY.into()
    };
    let comments_path = target(&rels, COMMENTS)?
        .map(|t| resolve_rel_target(path, &t))
        .transpose()?;
    let vml_path = target(&rels, VML)?
        .map(|t| resolve_rel_target(path, &t))
        .transpose()?;
    let existing = match &comments_path {
        Some(p) if pkg.has(p)? => Some(pkg.read_text(p)?),
        _ => None,
    };
    let threaded = threaded::sync(
        pkg,
        path,
        &rels,
        notes,
        existing.as_deref().map(String::as_str),
        touched,
    )?;
    let mut changed = threaded.rels_xml != rels;
    rels = threaded.rels_xml;
    if threaded.part_removed
        && let Some(part) = threaded.part_path
    {
            rels = remove_rel(&rels, threaded::REL_TYPE)?;
            changed = true;
            remove_override(pkg, &part, touched)?;
        }
    if notes.is_empty() {
        let Some(comments_path) = comments_path else {
            if changed {
                if has_rels {
                    pkg.write(&relpath, rels)?
                } else {
                    pkg.add(&relpath, rels)
                }
                touched.insert(relpath);
            }
            return Ok(());
        };
        pkg.remove(&comments_path);
        touched.insert(comments_path.clone());
        rels = remove_rel(&rels, COMMENTS)?;
        remove_override(pkg, &comments_path, touched)?;
        if let Some(vml) = match vml_path {
            Some(p) if pkg.has(&p)? => Some(p),
            _ => None,
        } {
            let remaining = strip_shapes(&pkg.read_text(&vml)?)?;
            if remaining.contains("<v:shape") {
                pkg.write(&vml, remaining)?;
            } else {
                pkg.remove(&vml);
                rels = remove_rel(&rels, VML)?;
                let xml = pkg.read_text(path)?;
                let next = replace(&xml, r"<legacyDrawing\b[^>]*/>", |_, _| Ok(String::new()))?;
                if next != *xml {
                    pkg.write(path, next)?;
                    touched.insert(path.into());
                }
            }
            touched.insert(vml);
        }
        pkg.write(&relpath, rels)?;
        touched.insert(relpath);
        return Ok(());
    }
    let comments_path = if let Some(comments_path) = comments_path {
        pkg.write(&comments_path, comments(notes, &threaded.legacy)?)?;
        comments_path
    } else {
        let p = free(pkg, "xl/comments", ".xml")?;
        let id = sheets::next_free_relationship_id(&rels);
        rels = append_rel(
            &rels,
            &id,
            COMMENTS,
            &format!("../{}", p.strip_prefix("xl/").unwrap_or(&p)),
        );
        changed = true;
        pkg.add(&p, comments(notes, &threaded.legacy)?);
        p
    };
    touched.insert(comments_path.clone());
    let (vml, original, new_vml) = if let Some(vml) = match vml_path {
        Some(p) if pkg.has(&p)? => Some(p),
        _ => None,
    } {
        let original = (*pkg.read_text(&vml)?).clone();
        (vml, original, false)
    } else {
        let vml = free(pkg, "xl/drawings/vmlDrawing", ".vml")?;
        let id = sheets::next_free_relationship_id(&rels);
        rels = append_rel(
            &rels,
            &id,
            VML,
            &format!("../drawings/{}", vml.rsplit('/').next().unwrap_or("")),
        );
        changed = true;
        let xml = pkg.read_text(path)?;
        if !xml.contains("<legacyDrawing") {
            let mut next = (*xml).clone();
            if !next.contains("xmlns:r=") {
                let root = re(r"<worksheet\b")?
                    .try_exec(&next)
                    .map_err(|e| e.to_string())?
                    .ok_or("Worksheet has no root element.")?
                    .end();
                next.insert_str(root," xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"");
            }
            let anchor=re(r"<legacyDrawingHF\b|<picture\b|<oleObjects\b|<controls\b|<webPublishItems\b|<tableParts\b|<extLst\b")?.try_exec(&next).map_err(|e|e.to_string())?.map(|m|m.start()).or_else(||next.rfind("</worksheet>")).ok_or("Worksheet has no closing element.")?;
            next.insert_str(anchor, &format!("<legacyDrawing r:id=\"{id}\"/>"));
            pkg.write(path, next)?;
            touched.insert(path.into());
        }
        (vml, format!("{VML_HEADER}</xml>"), true)
    };
    let previous = carried(&original)?;
    let mut remaining = strip_shapes(&original)?;
    let mut first = 1025usize;
    for m in re(r#"\bid="_x0000_s(\d+)""#)?.try_find_iter(&remaining) {
        let m = m.map_err(|e| e.to_string())?;
        if let Some(n) = m.get(1).and_then(|s| s.parse::<usize>().ok()) {
            first = first.max(n + 1)
        }
    }
    let mut shapes = String::new();
    for (i, note) in notes.iter().enumerate() {
        let row = cells::coordinate(note, "row")?;
        let col = cells::coordinate(note, "column")?;
        let selected = match note.get("picture") {
            Some(Value::Null) => None,
            Some(value) => Some(picture(pkg, &vml, value, touched)?),
            None => previous.get(&format!("{col}:{row}")).cloned(),
        };
        shapes.push_str(&shape(note, i, first, selected.as_ref())?);
    }
    let end = remaining
        .rfind("</xml>")
        .ok_or_else(|| format!("{vml} is not a VML drawing."))?;
    remaining.insert_str(end, &shapes);
    if new_vml {
        pkg.add(&vml, remaining)
    } else {
        pkg.write(&vml, remaining)?
    }
    touched.insert(vml);
    let ct = pkg.read_text("[Content_Types].xml")?;
    let mut next = (*ct).clone();
    if !next.contains(&format!("PartName=\"/{comments_path}\"")) {
        next=next.replacen("</Types>",&format!("<Override PartName=\"/{comments_path}\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml\"/></Types>"),1);
    }
    if !re(r#"<Default\b[^>]*Extension="vml""#)?
        .try_test(&next)
        .map_err(|e| e.to_string())?
    {
        next=next.replacen("</Types>","<Default Extension=\"vml\" ContentType=\"application/vnd.openxmlformats-officedocument.vmlDrawing\"/></Types>",1);
    }
    if next != *ct {
        pkg.write("[Content_Types].xml", next)?;
        touched.insert("[Content_Types].xml".into());
    }
    if changed {
        if has_rels {
            pkg.write(&relpath, rels)?
        } else {
            pkg.add(&relpath, rels)
        }
        touched.insert(relpath);
    }
    Ok(())
}

pub(super) fn apply(
    pkg: &mut PackageEditor<'_>,
    state: &mut PlannerState,
    resolved: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    for item in resolved
        .get("noteStates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = item
            .get("sheetName")
            .and_then(Value::as_str)
            .ok_or("Note sheet name missing")?;
        let notes = item
            .get("notes")
            .and_then(Value::as_array)
            .ok_or("Note snapshot missing")?;
        let path = state.path_for(pkg, name)?;
        apply_sheet(pkg, &path, notes, touched)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_escape_and_reuse_author() {
        let n = vec![
            serde_json::json!({"row":0,"column":0,"author":"A","text":"x&\r"}),
            serde_json::json!({"row":1,"column":0,"author":"A","text":"ok"}),
        ];
        let x = comments(&n, &HashMap::new()).unwrap();
        assert!(x.contains("x&amp;_x000D_"));
        assert_eq!(x.matches("<author>").count(), 1);
    }
    #[test]
    fn foreign_shapes_survive() {
        let s = "<xml><v:shape><x:ClientData ObjectType=\"Note\"/></v:shape><v:shape id=\"button\"/></xml>";
        assert!(strip_shapes(s).unwrap().contains("button"));
    }
}
