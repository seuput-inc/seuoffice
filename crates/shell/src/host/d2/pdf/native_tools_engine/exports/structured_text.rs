//! Per-page structured text in the PDF structure tree's reading order.
use jscompat::{js_regex, try_js_regex};
use shell_contract::IpcValue as V;
use shell_ipc_d2::pdf::tools::JobFailure;
use std::collections::{BTreeSet, HashMap, HashSet};

fn field<'a>(v: &'a V, key: &str) -> &'a V {
    if let V::Object(fields) = v {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or(&V::Undefined)
    } else {
        &V::Undefined
    }
}
fn text(v: &V) -> &str {
    if let V::String(s) = v { s } else { "" }
}
fn arr(v: &V) -> &[V] {
    if let V::Array(a) = v { a } else { &[] }
}
fn num(v: &V) -> i64 {
    if let V::Number(n) = v { *n as i64 } else { 0 }
}
fn truth(v: &V) -> bool {
    matches!(v, V::Bool(true))
}
fn s<'a>(v: &'a V, key: &str) -> &'a str {
    text(field(v, key))
}
fn a<'a>(v: &'a V, key: &str) -> &'a [V] {
    arr(field(v, key))
}
fn runs(v: &V) -> &[V] {
    a(v, "runs")
}

fn visit_refs(runs: &[V], order: &mut Vec<String>, refs: &mut HashMap<String, String>) {
    for run in runs {
        if s(run, "kind") != "footnoteRef" {
            continue;
        }
        let id = s(run, "id");
        if !order.iter().any(|v| v == id) {
            order.push(id.into());
        }
        let marker = jscompat::trim(s(run, "marker"));
        if !marker.is_empty() {
            refs.entry(id.into()).or_insert_with(|| marker.into());
        }
    }
}
fn markers(tree: &V) -> HashMap<String, String> {
    let mut order = Vec::new();
    let mut refs = HashMap::new();
    for block in a(tree, "blocks") {
        if s(block, "kind") == "table" {
            for cell in a(field(block, "table"), "cells") {
                for p in a(cell, "paragraphs") {
                    visit_refs(arr(p), &mut order, &mut refs);
                }
            }
        } else {
            visit_refs(runs(block), &mut order, &mut refs);
        }
        if matches!(s(block, "kind"), "table" | "image") {
            visit_refs(runs(field(block, "caption")), &mut order, &mut refs);
        }
    }
    for note in a(tree, "footnotes") {
        let id = s(note, "id");
        if !order.iter().any(|v| v == id) {
            order.push(id.into());
        }
    }
    let mut own = HashMap::new();
    for note in a(tree, "footnotes") {
        own.insert(s(note, "id"), jscompat::trim(s(note, "marker")));
    }
    let mut out = HashMap::new();
    let mut next = 0;
    for id in order {
        let label = own
            .get(id.as_str())
            .copied()
            .filter(|v| !v.is_empty())
            .or_else(|| refs.get(&id).map(String::as_str));
        let label = match label {
            Some(v) => v.to_owned(),
            None => {
                next += 1;
                next.to_string()
            }
        };
        out.insert(id, label);
    }
    out
}
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Block,
    Heading,
    Table,
}
fn inline(runs: &[V], mode: Mode, markers: &HashMap<String, String>) -> Result<String, JobFailure> {
    let mut out = String::new();
    for run in runs {
        match s(run, "kind") {
            "text" => {
                let text = try_js_regex!(r"[ \t]*[\r\n]+[ \t]*")
                    .try_replace_all(s(run, "text"), |_| " ".to_owned())
                    .map_err(|e| JobFailure::new("failed", Some(&e.to_string())))?;
                out.push_str(&text);
            }
            "lineBreak" => out.push_str(match mode {
                Mode::Block => "\n",
                Mode::Heading => " ",
                Mode::Table => "<br>",
            }),
            "math" => {
                let t = try_js_regex!(r"\s*[\r\n]+\s*")
                    .try_replace_all(s(run, "latex"), |_| " ".to_owned())
                    .map_err(|e| JobFailure::new("failed", Some(&e.to_string())))?;
                out.push('$');
                out.push_str(jscompat::trim(&t));
                out.push('$');
            }
            "footnoteRef" => {
                let id = s(run, "id");
                out.push_str(&format!(
                    "[^{}]",
                    markers.get(id).map(String::as_str).unwrap_or(id)
                ));
            }
            _ => {}
        }
    }
    let lines: Vec<&str> = out.split('\n').map(jscompat::trim).collect();
    let mut cleaned = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.is_empty() || (i > 0 && !lines[i - 1].is_empty()) {
            cleaned.push(*line);
        }
    }
    let joined = cleaned.join("\n");
    let result = jscompat::trim(&joined).to_owned();
    Ok(if mode == Mode::Table {
        result.replace('|', "\\|")
    } else {
        result
    })
}
struct Writer<'a> {
    parts: Vec<String>,
    list: Option<(Vec<bool>, Vec<i64>)>,
    markers: &'a HashMap<String, String>,
}
impl<'a> Writer<'a> {
    fn push(&mut self, value: String) {
        if !value.is_empty() {
            self.list = None;
            self.parts.push(value);
        }
    }
    fn inline(&self, v: &V, mode: Mode) -> Result<String, JobFailure> {
        inline(runs(v), mode, self.markers)
    }
    fn captioned(&mut self, body: String, captions: &[&V]) -> Result<(), JobFailure> {
        for c in captions.iter().filter(|c| s(c, "position") == "above") {
            let t = self.inline(c, Mode::Block)?;
            self.push(t);
        }
        self.push(body);
        for c in captions.iter().filter(|c| s(c, "position") != "above") {
            let t = self.inline(c, Mode::Block)?;
            self.push(t);
        }
        Ok(())
    }
    fn block(&mut self, block: &V) -> Result<(), JobFailure> {
        match s(block, "kind") {
            "heading" => {
                let t = self.inline(block, Mode::Heading)?;
                if !t.is_empty() {
                    self.push(format!(
                        "{} {t}",
                        "#".repeat(num(field(block, "level")).max(0) as usize)
                    ));
                }
            }
            "paragraph" => {
                let t = self.inline(block, Mode::Block)?;
                self.push(t);
            }
            "listItem" => {
                let t = self.inline(block, Mode::Block)?;
                if t.is_empty() {
                    return Ok(());
                }
                let continuing = self.list.is_some();
                let (mut kinds, mut counters) = self.list.take().unwrap_or_default();
                let level = (num(field(block, "level")).max(0) as usize).min(kinds.len());
                let ordered = truth(field(block, "ordered"));
                let continues = kinds.get(level) == Some(&ordered);
                let printed = jscompat::trim(s(block, "marker"));
                let matched = js_regex!(r"^\(?(\d{1,9})[.)]?\)?$")
                    .exec(printed)
                    .and_then(|m| m.get(1))
                    .and_then(|n| n.parse::<i64>().ok());
                let n = matched.unwrap_or_else(|| if continues { counters[level] + 1 } else { 1 });
                kinds.truncate(level);
                counters.truncate(level);
                kinds.push(ordered);
                counters.push(n);
                let mark = if !ordered {
                    "-".to_owned()
                } else if matched.is_none() && !printed.is_empty() {
                    printed.to_owned()
                } else {
                    format!("{n}.")
                };
                let indent = "  ".repeat(level);
                let t = t.replace('\n', &format!("\n{indent}  "));
                let line = format!("{indent}{mark} {t}");
                if continuing {
                    if let Some(last) = self.parts.last_mut() {
                        last.push('\n');
                        last.push_str(&line);
                    }
                } else {
                    self.parts.push(line);
                }
                self.list = Some((kinds, counters));
            }
            "table" => {
                let table = field(block, "table");
                let rows = num(field(table, "rowCount"));
                let cols = num(field(table, "colCount"));
                let mut body = String::new();
                if rows > 0 && cols > 0 {
                    let mut grid = vec![vec![String::new(); cols as usize]; rows as usize];
                    for cell in a(table, "cells") {
                        let row = num(field(cell, "row"));
                        let col = num(field(cell, "col"));
                        if row < 0 || row >= rows || col < 0 || col >= cols {
                            continue;
                        }
                        let mut paragraphs = Vec::new();
                        for p in a(cell, "paragraphs") {
                            let t = inline(arr(p), Mode::Table, self.markers)?;
                            if !t.is_empty() {
                                paragraphs.push(t);
                            }
                        }
                        grid[row as usize][col as usize] = paragraphs.join("<br>");
                    }
                    let row = |v: &[String]| format!("| {} |", v.join(" | "));
                    let mut lines = vec![row(&grid[0]), row(&vec!["---".into(); cols as usize])];
                    lines.extend(grid.iter().skip(1).map(|r| row(r)));
                    body = lines.join("\n");
                }
                let caption = field(block, "caption");
                let captions = if matches!(caption, V::Undefined) {
                    vec![]
                } else {
                    vec![caption]
                };
                self.captioned(body, &captions)?;
            }
            "image" => {
                let captions = [field(block, "caption"), field(block, "captionTranslation")]
                    .into_iter()
                    .filter(|v| !matches!(v, V::Undefined))
                    .collect::<Vec<_>>();
                self.captioned("[figure]".into(), &captions)?;
            }
            "math" => {
                let t = jscompat::trim(s(block, "latex"));
                if !t.is_empty() {
                    self.push(format!("$$\n{t}\n$$"));
                }
            }
            "pageImage" => {
                let page = num(field(block, "page"));
                self.push(if s(block, "reason") == "scanned" {
                    format!("[page {page} is an image (scanned); no text layer]")
                } else {
                    format!("[page {page} is an image; its text layer could not be read reliably]")
                });
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) fn format(tree: &V) -> Result<V, JobFailure> {
    let mut pages = BTreeSet::new();
    let mut blocks: HashMap<i64, Vec<&V>> = HashMap::new();
    for p in a(tree, "pages") {
        pages.insert(num(field(p, "page")));
    }
    for b in a(tree, "blocks") {
        let page = num(field(b, "page"));
        pages.insert(page);
        if s(b, "kind") != "pageBreak" {
            blocks.entry(page).or_default().push(b);
        }
    }
    let mut notes: HashMap<i64, Vec<&V>> = HashMap::new();
    let mut seen = HashSet::new();
    for n in a(tree, "footnotes") {
        if !seen.insert(s(n, "id")) {
            continue;
        }
        let p = num(field(n, "page"));
        pages.insert(p);
        notes.entry(p).or_default().push(n);
    }
    let mut toc: HashMap<i64, Vec<&V>> = HashMap::new();
    for e in a(tree, "toc") {
        let p = num(field(e, "tocPage"));
        if pages.contains(&p) {
            toc.entry(p).or_default().push(e);
        }
    }
    let marker = markers(tree);
    let mut result = Vec::new();
    for page in pages {
        let mut writer = Writer {
            parts: Vec::new(),
            list: None,
            markers: &marker,
        };
        if let Some(items) = blocks.get(&page) {
            for block in items {
                writer.block(block)?;
            }
        }
        if let Some(items) = toc.get(&page) {
            let lines = items
                .iter()
                .map(|e| {
                    let level = (num(field(e, "level")) - 1).max(0) as usize;
                    let title = jscompat::trim(s(e, "title"));
                    let label = jscompat::trim(s(e, "pageLabel"));
                    format!(
                        "{}- {}{}",
                        "  ".repeat(level),
                        title,
                        if label.is_empty() {
                            String::new()
                        } else {
                            format!(" … {label}")
                        }
                    )
                })
                .collect::<Vec<_>>();
            writer.push(format!("Table of contents:\n{}", lines.join("\n")));
        }
        if let Some(items) = notes.get(&page) {
            let mut lines = Vec::new();
            for note in items {
                let id = s(note, "id");
                let mut texts = Vec::new();
                for p in a(note, "blocks") {
                    let t = writer.inline(p, Mode::Block)?.replace('\n', " ");
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
                let t = texts.join(" ");
                lines.push(format!(
                    "[^{}]{}",
                    marker.get(id).map(String::as_str).unwrap_or(id),
                    if t.is_empty() {
                        String::new()
                    } else {
                        format!(" {t}")
                    }
                ));
            }
            if !lines.is_empty() {
                writer.push(format!("Footnotes:\n{}", lines.join("\n")));
            }
        }
        result.push(V::Object(vec![
            ("page".into(), V::Number(page as f64)),
            ("text".into(), V::String(writer.parts.join("\n\n"))),
        ]));
    }
    Ok(V::Array(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(fields: Vec<(&str, V)>) -> V {
        V::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
    fn st(s: &str) -> V {
        V::String(s.into())
    }
    fn run(s: &str) -> V {
        object(vec![("kind", st("text")), ("text", st(s))])
    }
    #[test]
    fn renders_page_order_lists_tables_and_footnotes() {
        let tree = object(vec![
            (
                "pages",
                V::Array(vec![
                    object(vec![("page", V::Number(2.0))]),
                    object(vec![("page", V::Number(1.0))]),
                ]),
            ),
            (
                "blocks",
                V::Array(vec![
                    object(vec![
                        ("kind", st("heading")),
                        ("page", V::Number(1.0)),
                        ("level", V::Number(2.0)),
                        ("runs", V::Array(vec![run("Title")])),
                    ]),
                    object(vec![
                        ("kind", st("listItem")),
                        ("page", V::Number(1.0)),
                        ("level", V::Number(0.0)),
                        ("ordered", V::Bool(true)),
                        ("runs", V::Array(vec![run("First")])),
                    ]),
                    object(vec![
                        ("kind", st("listItem")),
                        ("page", V::Number(1.0)),
                        ("level", V::Number(0.0)),
                        ("ordered", V::Bool(true)),
                        ("runs", V::Array(vec![run("Second")])),
                    ]),
                    object(vec![
                        ("kind", st("pageImage")),
                        ("page", V::Number(2.0)),
                        ("reason", st("scanned")),
                    ]),
                ]),
            ),
            ("footnotes", V::Array(vec![])),
            ("toc", V::Array(vec![])),
        ]);
        let result = format(&tree).unwrap();
        let pages = arr(&result);
        assert_eq!(
            text(field(&pages[0], "text")),
            "## Title\n\n1. First\n2. Second"
        );
        assert_eq!(
            text(field(&pages[1], "text")),
            "[page 2 is an image (scanned); no text layer]"
        );
    }
}
