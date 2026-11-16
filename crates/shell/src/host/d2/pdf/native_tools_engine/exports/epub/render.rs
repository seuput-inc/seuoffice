//! Reflow document blocks as EPUB XHTML.
use super::{array, field, flag, number, string};
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::tools::JobFailure;
use std::collections::HashMap;

pub(super) fn text(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r')
                || ((' '..='\u{d7ff}').contains(&c))
                || (('\u{e000}'..='\u{fffd}').contains(&c))
                || (c >= '\u{10000}')
        })
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub(super) fn attr(s: &str) -> String {
    text(s)
        .replace('"', "&quot;")
        .replace('\t', "&#9;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
}
pub(super) fn plain(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub(super) fn runs_text(runs: &[IpcValue]) -> String {
    runs.iter()
        .map(|r| match string(r, "kind") {
            "text" => string(r, "text"),
            "lineBreak" => "\n",
            "math" => string(r, "latex"),
            _ => "",
        })
        .collect()
}
pub(super) fn language(s: &str) -> Result<Option<String>, JobFailure> {
    let tag = s.trim().replace('_', "-");
    if tag.is_empty() {
        return Ok(None);
    }
    let result = crate::engine_host::call("pdf.canonicalLocale", &serde_json::json!({"tag":tag}))
        .map_err(|e| JobFailure::new("failed", Some(&e.message)))?;
    match result {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => Ok(Some(s)),
        _ => Err(JobFailure::new(
            "failed",
            Some("invalid canonical locale response"),
        )),
    }
}
fn attrs(b: &IpcValue, classes: &[&str]) -> Result<String, JobFailure> {
    let mut a = String::new();
    if let Some(lang) = language(string(b, "lang"))? {
        a.push_str(&format!(
            " lang=\"{}\" xml:lang=\"{}\"",
            attr(&lang),
            attr(&lang)
        ));
    }
    let mut cls = classes.to_vec();
    if field(b, "translationOf").is_some() {
        cls.push("translation")
    }
    if !cls.is_empty() {
        a.push_str(&format!(" class=\"{}\"", cls.join(" ")));
    }
    Ok(a)
}
pub(super) fn document(title: &str, lang: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"{}\" xml:lang=\"{}\">\n<head>\n<meta charset=\"UTF-8\"/>\n<title>{}</title>\n<link rel=\"stylesheet\" type=\"text/css\" href=\"style.css\"/>\n</head>\n<body>\n{body}</body>\n</html>\n",
        attr(lang),
        attr(lang),
        text(&plain(title))
    )
}
#[derive(Clone)]
pub(super) struct Chapter<'a> {
    pub blocks: Vec<&'a IpcValue>,
    pub heading: Option<&'a IpcValue>,
}
fn paired(b: &IpcValue, prev: Option<&IpcValue>) -> bool {
    field(b, "translationOf").is_some()
        && prev.is_some_and(|p| string(b, "translationOf") == string(p, "id"))
}
pub(super) fn chapters<'a>(tree: &'a IpcValue, by_page: bool) -> Vec<Chapter<'a>> {
    let blocks = array(tree, "blocks");
    let mut level = 0;
    let mut prev = None;
    for b in blocks {
        if string(b, "kind") == "heading" && !paired(b, prev) {
            let n = number(b, "level") as usize;
            if n == 1 {
                level = 1;
                break;
            } else if n == 2 {
                level = 2
            }
        }
        if string(b, "kind") != "pageBreak" {
            prev = Some(b)
        }
    }
    let mut out = Vec::new();
    let mut cur = Chapter {
        blocks: Vec::new(),
        heading: None,
    };
    prev = None;
    for b in blocks {
        if level != 0
            && string(b, "kind") == "heading"
            && number(b, "level") as usize == level
            && !paired(b, prev)
        {
            let mut carry = Vec::new();
            while cur
                .blocks
                .last()
                .is_some_and(|x| string(x, "kind") == "pageBreak")
            {
                carry.push(cur.blocks.pop().unwrap());
            }
            carry.reverse();
            if cur.heading.is_some() || !cur.blocks.is_empty() {
                out.push(cur)
            }
            carry.push(b);
            cur = Chapter {
                blocks: carry,
                heading: Some(b),
            };
        } else {
            cur.blocks.push(b)
        }
        if string(b, "kind") != "pageBreak" {
            prev = Some(b)
        }
    }
    if cur.heading.is_some() || !cur.blocks.is_empty() || out.is_empty() {
        out.push(cur)
    }
    if !by_page {
        return out;
    }
    let mut pages = Vec::new();
    for ch in out {
        let mut current = Chapter {
            heading: ch.heading,
            blocks: Vec::new(),
        };
        for b in ch.blocks {
            if string(b, "kind") == "pageBreak"
                && current
                    .blocks
                    .iter()
                    .any(|b| string(b, "kind") != "pageBreak")
            {
                pages.push(current);
                current = Chapter {
                    heading: None,
                    blocks: vec![b],
                };
            } else {
                current.blocks.push(b)
            }
        }
        pages.push(current)
    }
    pages
}
pub(super) struct Heading {
    pub level: usize,
    pub id: String,
    pub text: String,
}
pub(super) struct Rendered {
    pub body: String,
    pub headings: Vec<Heading>,
    pub math: bool,
    pub title: Option<String>,
}
pub(super) trait Host {
    fn marker(&self, b: &IpcValue) -> String;
    fn page_href(&self, page: usize) -> Option<String>;
    fn image(&mut self, b: &IpcValue) -> Result<String, JobFailure>;
    fn heading_id(&mut self) -> String;
    fn math(&mut self, latex: &str, display: bool) -> Result<Option<String>, JobFailure>;
    fn page_alt(&self, page: usize) -> String;
    fn footnote(&self, id: &str) -> Option<&IpcValue>;
}
struct Use {
    index: usize,
    ids: Vec<String>,
}
struct Writer<'a> {
    host: &'a mut dyn Host,
    out: String,
    lists: Vec<(usize, bool)>,
    pending: String,
    notes: HashMap<String, Use>,
    note_order: Vec<String>,
    headings: Vec<Heading>,
    has_math: bool,
    title: Option<String>,
}
impl Writer<'_> {
    fn inline(&mut self, runs: &[IpcValue], link_notes: bool) -> Result<String, JobFailure> {
        let mut out = String::new();
        for r in runs {
            match string(r, "kind") {
                "text" => {
                    let mut s = text(string(r, "text"));
                    if flag(r, "italic") {
                        s = format!("<i>{s}</i>")
                    }
                    if flag(r, "bold") {
                        s = format!("<b>{s}</b>")
                    }
                    let h = string(r, "href");
                    let href = if let Some(p) =
                        h.strip_prefix("#page=").and_then(|v| v.parse().ok())
                    {
                        self.host.page_href(p)
                    } else {
                        tauri::Url::parse(h)
                            .ok()
                            .filter(|u| matches!(u.scheme(), "http" | "https" | "mailto" | "ftp"))
                            .map(|u| {
                                u.to_string()
                                    .chars()
                                    .map(|c| {
                                        if c.is_whitespace()
                                            || matches!(
                                                c,
                                                '"' | '<'
                                                    | '>'
                                                    | '`'
                                                    | '{'
                                                    | '}'
                                                    | '|'
                                                    | '\\'
                                                    | '^'
                                            )
                                        {
                                            format!("%{:02X}", c as u32)
                                        } else {
                                            c.to_string()
                                        }
                                    })
                                    .collect()
                            })
                    };
                    if let Some(href) = href {
                        s = format!("<a href=\"{}\">{s}</a>", attr(&href))
                    }
                    out.push_str(&s);
                }
                "lineBreak" => out.push_str("<br/>"),
                "math" => {
                    let latex = string(r, "latex");
                    if let Some(xml) = self.host.math(latex, false)? {
                        self.has_math = true;
                        out.push_str(&xml)
                    } else {
                        out.push_str(&format!("<code class=\"math-tex\">{}</code>", text(latex)))
                    }
                }
                "footnoteRef" => {
                    let id = string(r, "id");
                    let marker = string(r, "marker");
                    if !link_notes {
                        if !marker.is_empty() {
                            out.push_str(&format!("<sup>{}</sup>", text(marker)))
                        }
                        continue;
                    }
                    let Some(note) = self.host.footnote(id) else {
                        if !marker.is_empty() {
                            out.push_str(&format!("<sup>{}</sup>", text(marker)))
                        }
                        continue;
                    };
                    let label = if string(note, "marker").is_empty() {
                        marker.to_owned()
                    } else {
                        string(note, "marker").to_owned()
                    };
                    let use_ = self.notes.entry(id.into()).or_insert_with(|| {
                        let n = self.note_order.len() + 1;
                        self.note_order.push(id.into());
                        Use {
                            index: n,
                            ids: Vec::new(),
                        }
                    });
                    let refid = if use_.ids.is_empty() {
                        format!("fnref-{}", use_.index)
                    } else {
                        format!("fnref-{}-{}", use_.index, use_.ids.len() + 1)
                    };
                    use_.ids.push(refid.clone());
                    let label = if label.is_empty() {
                        use_.index.to_string()
                    } else {
                        label
                    };
                    out.push_str(&format!("<sup><a id=\"{refid}\" href=\"#fn-{}\" epub:type=\"noteref\" role=\"doc-noteref\">{}</a></sup>",use_.index,text(&label)));
                }
                _ => return Err(JobFailure::new("failed", Some("unknown PDF inline run"))),
            }
        }
        Ok(out)
    }
    fn close_lists(&mut self) {
        while let Some((_, ordered)) = self.lists.pop() {
            self.out.push_str(if ordered {
                "</li></ol>\n"
            } else {
                "</li></ul>\n"
            })
        }
    }
    fn table(&mut self, b: &IpcValue) -> Result<String, JobFailure> {
        let table =
            field(b, "table").ok_or_else(|| JobFailure::new("failed", Some("missing table")))?;
        let rows = number(table, "rowCount").max(0.) as usize;
        let cols = number(table, "colCount").max(0.) as usize;
        let caption = field(b, "caption");
        let cls = if caption.is_some_and(|c| string(c, "position") == "below") {
            vec!["caption-below"]
        } else {
            vec![]
        };
        let mut s = format!("<table{}>\n", attrs(b, &cls)?);
        if let Some(c) = caption {
            s.push_str(&format!(
                "<caption{}>{}</caption>\n",
                attrs(c, &[])?,
                self.inline(array(c, "runs"), true)?
            ));
        }
        s.push_str("<tbody>\n");
        for row in 0..rows {
            let mut cells = array(table, "cells")
                .iter()
                .filter(|c| {
                    number(c, "row") == row as f64
                        && number(c, "col") >= 0.
                        && (number(c, "col") as usize) < cols
                })
                .collect::<Vec<_>>();
            cells.sort_by_key(|c| number(c, "col") as usize);
            s.push_str("<tr>");
            for c in cells {
                let tag = if flag(c, "header") { "th" } else { "td" };
                let rs = (number(c, "rowSpan").max(1.) as usize).min(rows - row);
                let col = number(c, "col") as usize;
                let cs = (number(c, "colSpan").max(1.) as usize).min(cols - col);
                let span = format!(
                    "{}{}",
                    if rs > 1 {
                        format!(" rowspan=\"{rs}\"")
                    } else {
                        String::new()
                    },
                    if cs > 1 {
                        format!(" colspan=\"{cs}\"")
                    } else {
                        String::new()
                    }
                );
                let paras = array(c, "paragraphs");
                let mut content = String::new();
                for p in paras {
                    let runs = match p {
                        IpcValue::Array(a) => a.as_slice(),
                        _ => &[],
                    };
                    let value = self.inline(runs, true)?;
                    if paras.len() == 1 {
                        content.push_str(&value)
                    } else {
                        content.push_str(&format!("<p>{value}</p>"))
                    }
                }
                s.push_str(&format!("<{tag}{span}>{content}</{tag}>"));
            }
            s.push_str("</tr>\n");
        }
        s.push_str("</tbody>\n</table>\n");
        Ok(s)
    }
    fn image(&mut self, b: &IpcValue) -> Result<String, JobFailure> {
        let captions = [field(b, "caption"), field(b, "captionTranslation")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut cap = String::new();
        for c in &captions {
            let content = self.inline(array(c, "runs"), true)?;
            if captions.len() == 1 {
                cap = format!("<figcaption{}>{content}</figcaption>", attrs(c, &[])?)
            } else {
                cap.push_str(&format!("<p{}>{content}</p>", attrs(c, &[])?));
            }
        }
        if captions.len() > 1 {
            cap = format!("<figcaption>{cap}</figcaption>")
        }
        let has = !flag(b, "placeholder")
            && matches!(field(b,"data"),Some(IpcValue::Bytes(_,v)) if !v.is_empty());
        if !has && cap.is_empty() {
            return Ok(String::new());
        }
        let alt = captions
            .first()
            .map(|c| plain(&runs_text(array(c, "runs"))))
            .unwrap_or_default();
        let img = if has {
            format!(
                "<img src=\"{}\" alt=\"{}\"/>",
                attr(&self.host.image(b)?),
                attr(&alt)
            )
        } else {
            String::new()
        };
        let above = captions
            .first()
            .is_some_and(|c| string(c, "position") == "above");
        Ok(format!(
            "<figure{}>{}{}</figure>\n",
            attrs(b, &[])?,
            if above { &cap } else { &img },
            if above { &img } else { &cap }
        ))
    }
}

pub(super) fn render(ch: &Chapter<'_>, host: &mut dyn Host) -> Result<Rendered, JobFailure> {
    let mut w = Writer {
        host,
        out: String::new(),
        lists: Vec::new(),
        pending: String::new(),
        notes: HashMap::new(),
        note_order: Vec::new(),
        headings: Vec::new(),
        has_math: false,
        title: None,
    };
    let mut previous = None;
    for b in &ch.blocks {
        let kind = string(b, "kind");
        if kind == "pageBreak" {
            let id = w.host.marker(b);
            w.pending.push_str(&format!("<span id=\"{}\" epub:type=\"pagebreak\" role=\"doc-pagebreak\" aria-label=\"{}\"></span>",attr(&id),number(b,"page")));
            continue;
        }
        if kind == "listItem" {
            let markers = std::mem::take(&mut w.pending);
            if !w.lists.is_empty()
                && previous.is_some_and(|p| string(p, "kind") == "listItem")
                && paired(b, previous)
            {
                let content = w.inline(array(b, "runs"), true)?;
                w.out
                    .push_str(&format!("<p{}>{markers}{content}</p>", attrs(b, &[])?));
                previous = Some(*b);
                continue;
            }
            let level = number(b, "level").max(0.) as usize;
            let ordered = flag(b, "ordered");
            while let Some(&(l, o)) = w.lists.last() {
                if l < level || (l == level && o == ordered) {
                    break;
                }
                w.out
                    .push_str(if o { "</li></ol>\n" } else { "</li></ul>\n" });
                w.lists.pop();
            }
            if w.lists.last().is_some_and(|(l, _)| *l == level) {
                w.out.push_str("</li>\n")
            } else {
                w.out.push_str(if ordered { "<ol>\n" } else { "<ul>\n" });
                w.lists.push((level, ordered));
            }
            let digits = string(b, "marker")
                .trim()
                .trim_start_matches('(')
                .chars()
                .take_while(char::is_ascii_digit)
                .take(9)
                .collect::<String>();
            let value = if ordered && !digits.is_empty() {
                format!(" value=\"{}\"", digits.parse::<usize>().unwrap_or(0))
            } else {
                String::new()
            };
            let content = w.inline(array(b, "runs"), true)?;
            w.out
                .push_str(&format!("<li{value}{}>{markers}{content}", attrs(b, &[])?));
            previous = Some(*b);
            continue;
        }
        w.close_lists();
        if !w.pending.is_empty() {
            w.out.push_str(&std::mem::take(&mut w.pending));
            w.out.push('\n');
        }
        match kind {
            "heading" => {
                let id = w.host.heading_id();
                let label = plain(&runs_text(array(b, "runs")));
                let level = (number(b, "level") as usize).clamp(1, 6);
                if !label.is_empty() && !paired(b, previous) {
                    w.headings.push(Heading {
                        level,
                        id: id.clone(),
                        text: label.clone(),
                    });
                }
                if w.title.is_none() && !label.is_empty() {
                    w.title = Some(label);
                }
                let content = w.inline(array(b, "runs"), true)?;
                w.out.push_str(&format!(
                    "<h{level} id=\"{id}\"{}>{content}</h{level}>\n",
                    attrs(b, &[])?
                ));
            }
            "paragraph" => {
                if field(b, "figure").is_none() {
                    let content = w.inline(array(b, "runs"), true)?;
                    w.out
                        .push_str(&format!("<p{}>{content}</p>\n", attrs(b, &[])?));
                }
            }
            "table" => {
                let s = w.table(b)?;
                w.out.push_str(&s);
            }
            "image" => {
                let s = w.image(b)?;
                w.out.push_str(&s);
            }
            "pageImage" => {
                let url = w.host.image(b)?;
                let alt = w.host.page_alt(number(b, "page") as usize);
                w.out.push_str(&format!(
                    "<figure class=\"page-image\"><img src=\"{}\" alt=\"{}\"/></figure>\n",
                    attr(&url),
                    attr(&alt)
                ));
            }
            "math" => {
                let latex = string(b, "latex");
                if let Some(xml) = w.host.math(latex, true)? {
                    w.has_math = true;
                    w.out
                        .push_str(&format!("<div class=\"math\">{xml}</div>\n"))
                } else {
                    w.out.push_str(&format!(
                        "<pre class=\"math-tex\"><code>{}</code></pre>\n",
                        text(latex)
                    ))
                }
            }
            _ => {
                return Err(JobFailure::new(
                    "failed",
                    Some("unknown PDF structure block"),
                ));
            }
        }
        previous = Some(*b);
    }
    w.close_lists();
    if !w.pending.is_empty() {
        w.out.push_str(&w.pending);
        w.out.push('\n');
    }
    let mut asides = String::new();
    for id in w.note_order.clone() {
        let Some(note) = w.host.footnote(&id).cloned() else {
            continue;
        };
        let Some(used) = w.notes.get(&id) else {
            continue;
        };
        let index = used.index;
        let first = used.ids[0].clone();
        let label = if string(&note, "marker").is_empty() {
            index.to_string()
        } else {
            string(&note, "marker").to_owned()
        };
        let back = format!(
            "<a href=\"#{first}\" role=\"doc-backlink\">{}</a>",
            text(&label)
        );
        let mut paragraphs = Vec::new();
        for (i, p) in array(&note, "blocks").iter().enumerate() {
            let body = w.inline(array(p, "runs"), false)?;
            paragraphs.push(format!(
                "<p{}>{}{body}</p>",
                attrs(p, &[])?,
                if i == 0 {
                    format!("{back} ")
                } else {
                    String::new()
                }
            ));
        }
        if paragraphs.is_empty() {
            paragraphs.push(format!("<p>{back}</p>"));
        }
        asides.push_str(&format!("<aside id=\"fn-{index}\" epub:type=\"footnote\" role=\"doc-footnote\" class=\"footnote\">\n{}\n</aside>\n",paragraphs.join("\n")));
    }
    let title = ch
        .heading
        .map(|h| plain(&runs_text(array(h, "runs"))))
        .filter(|s| !s.is_empty())
        .or(w.title);
    Ok(Rendered {
        body: format!("<section>\n{}</section>\n{asides}", w.out),
        headings: w.headings,
        math: w.has_math,
        title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obj(fields: Vec<(&str, IpcValue)>) -> IpcValue {
        IpcValue::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
    #[test]
    fn translated_heading_stays_in_original_chapter() {
        let a = obj(vec![
            ("kind", IpcValue::String("heading".into())),
            ("id", IpcValue::String("a".into())),
            ("level", IpcValue::Number(1.)),
        ]);
        let translated = obj(vec![
            ("kind", IpcValue::String("heading".into())),
            ("id", IpcValue::String("a@ko".into())),
            ("translationOf", IpcValue::String("a".into())),
            ("level", IpcValue::Number(1.)),
        ]);
        let tree = obj(vec![("blocks", IpcValue::Array(vec![a, translated]))]);
        let parts = chapters(&tree, false);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].blocks.len(), 2);
    }
}
