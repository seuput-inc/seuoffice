//! Native outline reader for Generate's map conversions. The arena mirrors
//! xmind-engine's drafts without recursive parsing or unbounded tree depth.
use std::collections::HashSet;

use main_core::shell::generate::convert::XmindTopic;
use serde_json::{Value, json};

const MAX_TOPIC_DEPTH: usize = 256;
const KIND_MARKS: [&str; 6] = [
    "(summary)",
    "(callout)",
    "(floating)",
    "(other)",
    "(boundary)",
    "(relationship)",
];

#[derive(Default)]
struct Draft {
    title: String,
    labels: Vec<String>,
    link: Option<String>,
    quoted: Vec<String>,
    body: Vec<String>,
    children: Vec<usize>,
    parent: Option<usize>,
    depth: usize,
}

#[derive(Default)]
struct Section {
    title: Option<String>,
    body: Vec<String>,
    items_at: Option<usize>,
    outer: Vec<usize>,
    deepest: Vec<usize>,
}

#[derive(Clone, Copy)]
struct Item {
    indent: usize,
    content: usize,
    draft: usize,
}
struct Fence {
    character: u8,
    length: usize,
    floor: Option<usize>,
}
struct Outline {
    drafts: Vec<Draft>,
    sections: Vec<Section>,
}

fn leading_blanks(line: &str) -> usize {
    line.bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count()
}

fn column_at(line: &str, count: usize) -> usize {
    line.bytes().take(count).fold(0, |column, byte| {
        if byte == b'\t' {
            column + 4 - column % 4
        } else {
            column + 1
        }
    })
}

fn without_indent(line: &str, columns: usize) -> &str {
    let mut at = 0;
    let mut column = 0;
    for byte in line.bytes() {
        if column >= columns || !matches!(byte, b' ' | b'\t') {
            break;
        }
        column = if byte == b'\t' {
            column + 4 - column % 4
        } else {
            column + 1
        };
        at += 1;
    }
    &line[at..]
}

fn heading(rest: &str) -> Option<(usize, usize)> {
    let level = rest.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let tail = &rest[level..];
    if !tail.is_empty() && !matches!(tail.as_bytes()[0], b' ' | b'\t') {
        return None;
    }
    Some((level, level + leading_blanks(tail)))
}

fn marker_length(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let gap = |at| at == bytes.len() || matches!(bytes[at], b' ' | b'\t');
    if bytes
        .first()
        .is_some_and(|byte| matches!(*byte, b'-' | b'+' | b'*'))
        && gap(1)
    {
        return 1;
    }
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if (1..=9).contains(&digits)
        && bytes
            .get(digits)
            .is_some_and(|byte| matches!(*byte, b'.' | b')'))
        && gap(digits + 1)
    {
        digits + 1
    } else {
        0
    }
}

fn rule(rest: &str) -> bool {
    let Some(first @ (b'*' | b'-' | b'_')) = rest.bytes().next() else {
        return false;
    };
    let mut count = 0;
    for byte in rest.bytes() {
        if byte == first {
            count += 1;
        } else if !matches!(byte, b' ' | b'\t') {
            return false;
        }
    }
    count >= 3
}

fn opening_fence(rest: &str) -> Option<(u8, usize)> {
    let character = *rest.as_bytes().first()?;
    if !matches!(character, b'`' | b'~') {
        return None;
    }
    let length = rest.bytes().take_while(|byte| *byte == character).count();
    (length >= 3 && !(character == b'`' && rest[length..].contains('`')))
        .then_some((character, length))
}

fn closes_fence(rest: &str, fence: &Fence) -> bool {
    let run = rest
        .bytes()
        .take_while(|byte| *byte == fence.character)
        .count();
    run >= fence.length && jscompat::trim(&rest[run..]).is_empty()
}

fn trim_end(text: &str) -> &str {
    text.trim_end_matches(jscompat::is_js_space)
}

fn without_closing_hashes(text: &str) -> &str {
    let text = trim_end(text);
    let start = text.len() - text.bytes().rev().take_while(|byte| *byte == b'#').count();
    if start == text.len() {
        return text;
    }
    if start == 0 || matches!(text.as_bytes()[start - 1], b' ' | b'\t') {
        trim_end(&text[..start])
    } else {
        text
    }
}

fn punctuation(byte: u8) -> bool {
    byte.is_ascii_punctuation()
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\\'
            && chars
                .peek()
                .is_some_and(|next| next.is_ascii() && punctuation(*next as u8))
        {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn starts_word(text: &str, at: usize) -> bool {
    at == 0 || matches!(text.as_bytes()[at - 1], b' ' | b'\t')
}

fn read_topic_line(text: &str, item: bool) -> Draft {
    let mut rest = jscompat::trim(text);
    if item
        && let Some(mark) = KIND_MARKS.iter().find(|mark| rest.starts_with(**mark))
    {
        let tail = &rest[mark.len()..];
        let gap = leading_blanks(tail);
        if gap > 0 || tail.is_empty() {
            rest = &tail[gap..];
        }
    }
    let mut link = None;
    if rest.ends_with('>')
        && let Some(open) = rest.rfind('<')
    {
        let inner = &rest[open + 1..rest.len() - 1];
        let is_web = ["http://", "https://", "mailto:"].iter().any(|prefix| {
            inner
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        });
        if starts_word(rest, open)
            && !inner
                .chars()
                .any(|ch| ch == '>' || jscompat::is_js_space(ch))
            && is_web
        {
            link = Some(inner.to_owned());
            rest = trim_end(&rest[..open]);
        }
    }
    let mut labels = Vec::new();
    if rest.ends_with(']') {
        let bytes = rest.as_bytes();
        let mut escaped = vec![false; bytes.len()];
        let mut at = 0;
        while at + 1 < bytes.len() {
            if bytes[at] == b'\\' && punctuation(bytes[at + 1]) {
                escaped[at + 1] = true;
                at += 2;
            } else {
                at += 1;
            }
        }
        let close = bytes.len() - 1;
        let mut open = if escaped[close] {
            None
        } else {
            close.checked_sub(1)
        };
        while let Some(at) = open {
            if !escaped[at] && matches!(bytes[at], b'[' | b']') {
                break;
            }
            open = at.checked_sub(1);
        }
        if let Some(open) = open.filter(|at| bytes[*at] == b'[' && starts_word(rest, *at)) {
            let mut start = open + 1;
            for at in open + 1..=close {
                if at < close && (bytes[at] != b',' || escaped[at]) {
                    continue;
                }
                let label = unescape(jscompat::trim(&rest[start..at]));
                if !label.is_empty() {
                    labels.push(label);
                }
                start = at + 1;
            }
            if !labels.is_empty() {
                rest = trim_end(&rest[..open]);
            }
        }
    }
    Draft {
        title: unescape(rest),
        labels,
        link,
        ..Default::default()
    }
}

fn blank_edges(lines: &[String]) -> &[String] {
    let start = lines
        .iter()
        .position(|line| !jscompat::trim(line).is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !jscompat::trim(line).is_empty())
        .map_or(start, |at| at + 1);
    &lines[start..end]
}

impl Outline {
    fn add(&mut self, section: usize, mut parent: Option<usize>, mut draft: Draft) -> usize {
        while parent.is_some_and(|at| self.drafts[at].depth >= MAX_TOPIC_DEPTH) {
            parent = parent.and_then(|at| self.drafts[at].parent);
        }
        draft.parent = parent;
        draft.depth = parent.map_or(0, |at| self.drafts[at].depth + 1);
        let at = self.drafts.len();
        if draft.depth == MAX_TOPIC_DEPTH {
            self.sections[section].deepest.push(at);
        }
        self.drafts.push(draft);
        if let Some(parent) = parent {
            self.drafts[parent].children.push(at);
        } else {
            let section = &mut self.sections[section];
            section.items_at.get_or_insert(section.body.len());
            section.outer.push(at);
        }
        at
    }

    fn body(&mut self, section: usize, sink: Option<usize>) -> &mut Vec<String> {
        match sink {
            Some(at) => &mut self.drafts[at].body,
            None => &mut self.sections[section].body,
        }
    }

    fn read(text: &str) -> Self {
        let mut out = Self {
            drafts: Vec::new(),
            sections: vec![Section::default()],
        };
        let mut section = 0;
        let mut headings: Vec<(usize, usize)> = Vec::new();
        let mut items: Vec<Item> = Vec::new();
        let mut sink = None;
        let mut dedent = 0;
        let mut note_item = None;
        let mut after_blank = false;
        let mut fence: Option<Fence> = None;
        let source = text.strip_prefix('\u{feff}').unwrap_or(text);
        for line in split_lines(source) {
            let blanks = leading_blanks(line);
            let column = column_at(line, blanks);
            let rest = &line[blanks..];
            if let Some(open) = &fence {
                if rest.is_empty() || open.floor.is_none_or(|floor| column > floor) {
                    out.body(section, sink)
                        .push(without_indent(line, dedent).to_owned());
                    if closes_fence(rest, open) {
                        fence = None;
                    }
                    after_blank = false;
                    continue;
                }
                fence = None;
            }
            if rest.is_empty() {
                out.body(section, sink).push(String::new());
                after_blank = true;
                note_item = None;
                continue;
            }
            let code = items.is_empty() && column >= 4;
            let header = if column <= 3 { heading(rest) } else { None };
            let is_rule = !code && header.is_none() && rule(rest);
            let marker = if code || header.is_some() || is_rule {
                0
            } else {
                marker_length(rest)
            };
            if let Some((level, length)) = header {
                let title = without_closing_hashes(&rest[length..]);
                items.clear();
                note_item = None;
                dedent = 0;
                if level == 1 {
                    section = out.sections.len();
                    out.sections.push(Section {
                        title: Some(unescape(title)),
                        ..Default::default()
                    });
                    headings.clear();
                    sink = None;
                } else {
                    while headings.last().is_some_and(|(above, _)| *above >= level) {
                        headings.pop();
                    }
                    let parent = headings.last().map(|(_, at)| *at);
                    let at = out.add(section, parent, read_topic_line(title, false));
                    headings.push((level, at));
                    sink = Some(at);
                }
            } else if marker > 0 {
                while items.last().is_some_and(|item| item.indent >= column) {
                    items.pop();
                }
                let parent = items
                    .last()
                    .map(|item| item.draft)
                    .or_else(|| headings.last().map(|(_, at)| *at));
                let after = &rest[marker..];
                let gap = leading_blanks(after);
                let at = out.add(section, parent, read_topic_line(&after[gap..], true));
                let item = Item {
                    indent: column,
                    content: column + marker + if (1..=4).contains(&gap) { gap } else { 1 },
                    draft: at,
                };
                items.push(item);
                sink = Some(at);
                dedent = item.content;
                note_item = Some(at);
            } else if !code && rest.starts_with('>') {
                settle(column, &mut items, &headings, &mut sink, &mut dedent);
                if let Some(at) =
                    note_item.filter(|at| items.last().is_some_and(|item| item.draft == *at))
                {
                    out.drafts[at].quoted.push(
                        rest[if rest.as_bytes().get(1) == Some(&b' ') {
                            2
                        } else {
                            1
                        }..]
                            .to_owned(),
                    );
                } else {
                    note_item = None;
                    out.body(section, sink)
                        .push(without_indent(line, dedent).to_owned());
                }
            } else {
                let opening = if code { None } else { opening_fence(rest) };
                let continues = opening.is_none() && !is_rule && !after_blank && !items.is_empty();
                if !continues {
                    settle(column, &mut items, &headings, &mut sink, &mut dedent);
                }
                note_item = None;
                out.body(section, sink)
                    .push(without_indent(line, dedent).to_owned());
                if let Some((character, length)) = opening {
                    fence = Some(Fence {
                        character,
                        length,
                        floor: items.last().map(|item| item.indent),
                    });
                }
            }
            after_blank = false;
        }
        if out.sections.len() > 1 && out.sections[0].outer.is_empty() {
            let first = out.sections.remove(0);
            let body = blank_edges(&first.body);
            if !body.is_empty() {
                let next = &mut out.sections[0];
                let mut combined = body.to_vec();
                combined.push(String::new());
                combined.append(&mut next.body);
                next.body = combined;
                if let Some(at) = &mut next.items_at {
                    *at += body.len() + 1;
                }
            }
        }
        out
    }

    fn roots(&mut self, untitled: &str) -> Vec<(String, usize)> {
        let mut roots = Vec::with_capacity(self.sections.len());
        for section in &mut self.sections {
            let title = section
                .title
                .as_deref()
                .filter(|title| !title.is_empty())
                .unwrap_or(untitled)
                .to_owned();
            let only = if section.outer.len() == 1 {
                section.outer.first().copied()
            } else {
                None
            };
            let root = if let Some(root) = only {
                let at = section.items_at.unwrap_or(section.body.len());
                let note = note_of(&self.drafts[root], &section.body[..at], &section.body[at..]);
                self.drafts[root].body = note.into_iter().collect();
                self.drafts[root].quoted.clear();
                root
            } else {
                let mut done = HashSet::new();
                for &deep in &section.deepest {
                    let above = self.drafts[deep]
                        .parent
                        .and_then(|parent| self.drafts[parent].parent);
                    let Some(above) = above.filter(|above| done.insert(*above)) else {
                        continue;
                    };
                    let children = std::mem::take(&mut self.drafts[above].children);
                    let mut raised = Vec::new();
                    for child in children {
                        raised.push(child);
                        raised.append(&mut self.drafts[child].children);
                    }
                    self.drafts[above].children = raised;
                }
                let root = self.drafts.len();
                self.drafts.push(Draft {
                    title: title.clone(),
                    body: std::mem::take(&mut section.body),
                    children: std::mem::take(&mut section.outer),
                    ..Default::default()
                });
                root
            };
            roots.push((title, root));
        }
        roots
    }
}

fn split_lines(source: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = source.as_bytes();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        if matches!(bytes[at], b'\r' | b'\n') {
            lines.push(&source[start..at]);
            if bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n') {
                at += 1;
            }
            start = at + 1;
        }
        at += 1;
    }
    lines.push(&source[start..]);
    lines
}

fn settle(
    column: usize,
    items: &mut Vec<Item>,
    headings: &[(usize, usize)],
    sink: &mut Option<usize>,
    dedent: &mut usize,
) {
    while items.last().is_some_and(|item| column <= item.indent) {
        items.pop();
    }
    *sink = items
        .last()
        .map(|item| item.draft)
        .or_else(|| headings.last().map(|(_, at)| *at));
    *dedent = items.last().map_or(0, |item| item.content);
}

fn note_of(draft: &Draft, above: &[String], after: &[String]) -> Option<String> {
    let parts = [
        blank_edges(above).join("\n"),
        draft.quoted.join("\n"),
        blank_edges(&draft.body).join("\n"),
        blank_edges(after).join("\n"),
    ];
    let parts: Vec<_> = parts.into_iter().filter(|part| !part.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn new_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn sheets(outline: &str, untitled: &str) -> Result<Vec<Value>, String> {
    let mut parsed = Outline::read(outline);
    let roots = parsed.roots(untitled);
    let mut sheets = Vec::with_capacity(roots.len());
    let mut topics: Vec<Option<Value>> = (0..parsed.drafts.len()).map(|_| None).collect();
    for (title, root) in roots {
        let sheet_id = new_id()?;
        let root_id = new_id()?;
        let theme = default_theme()?;
        let mut pending = vec![(root, root_id, false)];
        while let Some((at, id, visited)) = pending.pop() {
            let draft = &parsed.drafts[at];
            if !visited {
                let mut topic = json!({"id":id});
                if at == root {
                    topic["class"] = json!("topic");
                }
                topic["title"] = json!(draft.title);
                if !draft.labels.is_empty() {
                    topic["labels"] = json!(draft.labels);
                }
                if let Some(link) = &draft.link {
                    topic["href"] = json!(link);
                }
                if let Some(note) = note_of(draft, &[], &[]) {
                    topic["notes"] = json!({"plain":{"content":note}});
                }
                topics[at] = Some(topic);
                pending.push((at, String::new(), true));
                let children = draft
                    .children
                    .iter()
                    .map(|child| Ok((*child, new_id()?, false)))
                    .collect::<Result<Vec<_>, String>>()?;
                pending.extend(children);
            } else if !draft.children.is_empty() {
                let children = draft
                    .children
                    .iter()
                    .map(|child| {
                        topics[*child]
                            .take()
                            .ok_or_else(|| "outline topic was not built".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(topic) = &mut topics[at] {
                    topic["children"] = object([("attached", Value::Array(children))]);
                }
            }
        }
        let root = topics[root]
            .take()
            .ok_or("outline central topic was not built")?;
        sheets.push(object([
            ("id", Value::String(sheet_id)),
            ("class", Value::String("sheet".into())),
            ("rootTopic", root),
            ("title", Value::String(title)),
            ("theme", theme),
        ]));
    }
    Ok(sheets)
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// The default loaded by the package index (`pill/paper/light`). These are
/// writtenThemeProperties plus the new sheet's default line settings; ids
/// remain fresh for every level, as themeTable writes them.
fn default_theme() -> Result<Value, String> {
    let properties = [
        (
            "map",
            json!({"svg:fill":"#faf8f3","multi-line-colors":"#bb4a36 #106758 #b48427 #264684","color-list":"#faf8f3 #bb4a36 #106758 #b48427 #264684 #1f2328","line-tapered":"none"}),
        ),
        (
            "centralTopic",
            json!({"fo:font-size":"18pt","fo:font-weight":"700","fo:color":"#faf8f3","svg:fill":"#1f2328","fill-pattern":"solid","line-width":"2pt","line-color":"#1f2328","border-line-color":"#1f2328","border-line-width":"0pt","shape-class":"org.xmind.topicShape.roundedRect","line-class":"org.xmind.branchConnection.curve"}),
        ),
        (
            "mainTopic",
            json!({"fo:font-size":"14pt","fo:font-weight":"600","fo:color":"#1f2328","svg:fill":"#e8e7e3","fill-pattern":"solid","line-width":"1pt","line-color":"inherited","border-line-color":"inherited","border-line-width":"0pt","shape-class":"org.xmind.topicShape.roundedRect","line-class":"org.xmind.branchConnection.curve"}),
        ),
        (
            "subTopic",
            json!({"fo:font-size":"13pt","fo:font-weight":"400","fo:color":"#404346","svg:fill":"inherited","fill-pattern":"none","line-width":"1pt","line-color":"inherited","border-line-color":"inherited","border-line-width":"0pt","shape-class":"org.xmind.topicShape.roundedRect","line-class":"org.xmind.branchConnection.curve"}),
        ),
    ];
    let mut table = serde_json::Map::new();
    for (name, properties) in properties {
        table.insert(
            name.to_owned(),
            object([("id", Value::String(new_id()?)), ("properties", properties)]),
        );
    }
    Ok(Value::Object(table))
}

/// Mirrors JSON.stringify for this generated string-only model without
/// recursively walking hundreds of nested children objects.
fn serialize(sheets: &[Value]) -> Result<Vec<u8>, String> {
    enum Piece<'a> {
        Value(&'a Value),
        Quoted(&'a str),
        Byte(u8),
    }
    let mut pending = vec![Piece::Byte(b']')];
    for (index, sheet) in sheets.iter().enumerate().rev() {
        pending.push(Piece::Value(sheet));
        if index > 0 {
            pending.push(Piece::Byte(b','));
        }
    }
    let mut out = vec![b'['];
    while let Some(piece) = pending.pop() {
        match piece {
            Piece::Byte(byte) => out.push(byte),
            Piece::Quoted(text) => {
                serde_json::to_writer(&mut out, text).map_err(|error| error.to_string())?
            }
            Piece::Value(Value::Object(fields)) => {
                out.push(b'{');
                pending.push(Piece::Byte(b'}'));
                for (index, (key, value)) in fields.iter().enumerate().rev() {
                    pending.push(Piece::Value(value));
                    pending.push(Piece::Byte(b':'));
                    pending.push(Piece::Quoted(key));
                    if index > 0 {
                        pending.push(Piece::Byte(b','));
                    }
                }
            }
            Piece::Value(Value::Array(items)) => {
                out.push(b'[');
                pending.push(Piece::Byte(b']'));
                for (index, item) in items.iter().enumerate().rev() {
                    pending.push(Piece::Value(item));
                    if index > 0 {
                        pending.push(Piece::Byte(b','));
                    }
                }
            }
            Piece::Value(value) => {
                serde_json::to_writer(&mut out, value).map_err(|error| error.to_string())?
            }
        }
    }
    Ok(out)
}

fn check_structure(sheets: &[Value]) -> Result<(), String> {
    if sheets.is_empty() {
        return Err("the file has no sheet".into());
    }
    for (index, sheet) in sheets.iter().enumerate() {
        let mut seen = HashSet::new();
        let mut pending = vec![(&sheet["rootTopic"], 0)];
        while let Some((topic, depth)) = pending.pop() {
            let id = topic
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| format!("sheet {index}: a topic has no id"))?;
            if !seen.insert(id) {
                return Err(format!(
                    "sheet {index}: topic id {id} is used more than once"
                ));
            }
            if let Some(children) = topic
                .get("children")
                .and_then(|children| children.get("attached"))
                .and_then(Value::as_array)
            {
                if !children.is_empty() && depth >= MAX_TOPIC_DEPTH {
                    return Err(format!(
                        "sheet {index}: topics are nested deeper than {MAX_TOPIC_DEPTH} levels"
                    ));
                }
                pending.extend(children.iter().map(|child| (child, depth + 1)));
            }
        }
    }
    Ok(())
}

/// `outlineToSheets(outline, {untitled})[0].rootTopic`, projected only after
/// every sheet has been parsed and given real fresh source identifiers.
pub(super) fn first_sheet_root(outline: &str, untitled: &str) -> Result<XmindTopic, String> {
    let sheets = sheets(outline, untitled)?;
    let first = sheets.first().ok_or("outline has no sheet")?;
    fn project(topic: &Value) -> XmindTopic {
        XmindTopic {
            title: topic
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            attached: topic
                .get("children")
                .and_then(|children| children.get("attached"))
                .and_then(Value::as_array)
                .map(|children| children.iter().map(project).collect())
                .unwrap_or_default(),
        }
    }
    Ok(project(&first["rootTopic"]))
}

/// Generate's validation builds the real package, then applies the native
/// counterpart of the engine's opening gate before handing the outline off.
pub(super) fn create_xmind_document_from_outline(
    outline: &str,
    untitled: &str,
) -> Result<(), String> {
    let sheets = sheets(outline, untitled)?;
    check_structure(&sheets).map_err(|error| format!("xmind: cannot create, {error}"))?;
    let content = serialize(&sheets)?;
    let metadata = serde_json::to_vec(&json!({"creator":{"name":"SeuOffice","version":"0.1.0"}}))
        .map_err(|error| error.to_string())?;
    let manifest = br#"{"file-entries":{"content.json":{},"metadata.json":{}}}"#.to_vec();
    let package = ooxml_pkg::Package::from_parts([
        ("content.json", content),
        ("metadata.json", metadata),
        ("manifest.json", manifest),
    ]);
    let now = main_core::packages::pdf_structure::files::system_now_ms();
    let bytes = ooxml_pkg::write::generate(&package, now).map_err(|error| error.to_string())?;
    ooxml_pkg::assert_zip_inflates_within_limits(&bytes, &ooxml_pkg::DEFAULT_ZIP_LIMITS)
        .map_err(|error| format!("xmind: cannot create, {error}"))?;
    let entries = ooxml_pkg::jszip::load(&bytes, false)
        .map_err(|error| format!("xmind: cannot create, the archive cannot be read: {error}"))?;
    if entries.len() != 3
        || entries
            .iter()
            .any(|entry| entry.is_dir || entry.name != entry.unsafe_original_name)
    {
        return Err(
            "xmind: cannot create, the archive has entries a save could not carry over by name"
                .into(),
        );
    }
    let reopened = entries
        .iter()
        .find(|entry| entry.name == "content.json")
        .ok_or("xmind: cannot create, the archive has no content.json")?;
    let reopened = reopened
        .content
        .as_ref()
        .map_err(|error| format!("xmind: cannot create, content.json cannot be parsed: {error}"))?;
    // Exact round-trip bytes prove the generated, already structure-checked
    // JSON is the same document. No shallow serde parser depth cap is applied.
    let expected = package
        .bytes("content.json")
        .map_err(|error| error.to_string())?
        .ok_or("xmind: cannot create, generated content.json missing")?;
    if reopened.as_slice() != expected {
        return Err("xmind: cannot create, content.json changed while reopening".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(topic: &Value) -> Value {
        let mut out = json!({"title": topic["title"]});
        for key in ["labels", "href", "notes"] {
            if let Some(value) = topic.get(key) {
                out[key] = value.clone();
            }
        }
        if let Some(children) = topic
            .get("children")
            .and_then(|children| children.get("attached"))
            .and_then(Value::as_array)
        {
            out["children"] = Value::Array(children.iter().map(shape).collect());
        }
        out
    }

    #[test]
    fn synthetic_source_oracle_cases_keep_the_full_reader_state() {
        // Expected shapes observed from the original TypeScript engine.
        let cases = [
            (
                "",
                json!([{"sheet":"Untitled","root":{"title":"Untitled"}}]),
            ),
            (
                "before\n# One\n- a\n# Two\n- b",
                json!([{"sheet":"One","root":{"title":"a","notes":{"plain":{"content":"before"}}}},{"sheet":"Two","root":{"title":"b"}}]),
            ),
            (
                "- before\n# One\n- a",
                json!([{"sheet":"Untitled","root":{"title":"before"}},{"sheet":"One","root":{"title":"a"}}]),
            ),
            (
                "## A\n#### B\n### C\n###### D\n## E",
                json!([{"sheet":"Untitled","root":{"title":"Untitled","children":[{"title":"A","children":[{"title":"B"},{"title":"C","children":[{"title":"D"}]}]},{"title":"E"}]}}]),
            ),
            (
                "- Root\n\t- tab\n   2) sibling\n    + deep\n- Last",
                json!([{"sheet":"Untitled","root":{"title":"Untitled","children":[{"title":"Root","children":[{"title":"tab"},{"title":"sibling","children":[{"title":"deep"}]}]},{"title":"Last"}]}}]),
            ),
            (
                "    - code\n\n- a\n  ```js\n  # code\n- b",
                json!([{"sheet":"Untitled","root":{"title":"Untitled","notes":{"plain":{"content":"    - code"}},"children":[{"title":"a","notes":{"plain":{"content":"```js\n# code"}}},{"title":"b"}]}}]),
            ),
            (
                "- (summary) A\n- (callout) B\n- (floating) C\n- (other) D\n- (boundary) E\n- (relationship) F\n- (summary)x",
                json!([{"sheet":"Untitled","root":{"title":"Untitled","children":[{"title":"A"},{"title":"B"},{"title":"C"},{"title":"D"},{"title":"E"},{"title":"F"},{"title":"(summary)x"}]}}]),
            ),
            (
                "- Root\n  * * *\n  - Child\n---\n- Sibling",
                json!([{"sheet":"Untitled","root":{"title":"Untitled","notes":{"plain":{"content":"---"}},"children":[{"title":"Root","notes":{"plain":{"content":"* * *"}},"children":[{"title":"Child"}]},{"title":"Sibling"}]}}]),
            ),
            (
                "~~~\n# not heading\n- not item\n~~~\n# Sheet\n- a",
                json!([{"sheet":"Sheet","root":{"title":"a","notes":{"plain":{"content":"~~~\n# not heading\n- not item\n~~~"}}}}]),
            ),
        ];
        for (text, expected) in cases {
            let made = sheets(text, "Untitled").unwrap();
            let actual: Vec<_> = made
                .iter()
                .map(|sheet| json!({"sheet":sheet["title"],"root":shape(&sheet["rootTopic"])}))
                .collect();
            assert_eq!(Value::Array(actual), expected, "{text}");
            assert_eq!(
                serialize(&made).unwrap(),
                serde_json::to_vec(&made).unwrap()
            );
            create_xmind_document_from_outline(text, "Untitled").unwrap();
        }
    }

    #[test]
    fn default_theme_and_fresh_identifiers_match_package_index_creation() {
        let first = sheets("", "Untitled").unwrap();
        let second = sheets("", "Untitled").unwrap();
        assert_ne!(first[0]["id"], second[0]["id"]);
        assert_eq!(
            first[0]["theme"]["map"]["properties"]["line-tapered"],
            "none"
        );
        assert_eq!(
            first[0]["theme"]["centralTopic"]["properties"]["fo:font-size"],
            "18pt"
        );
        assert_eq!(
            first[0]["theme"]["mainTopic"]["properties"]["fo:font-size"],
            "14pt"
        );
        assert_eq!(
            first[0]["theme"]["subTopic"]["properties"]["fo:font-size"],
            "13pt"
        );
        for name in ["map", "centralTopic", "mainTopic", "subTopic"] {
            assert_ne!(
                first[0]["theme"][name]["id"],
                second[0]["theme"][name]["id"]
            );
        }
    }

    #[test]
    fn first_sheet_central_topic_and_order_match_the_original_consumer() {
        let root = first_sheet_root("Intro\r\n# Sheet\r\n## Root\r\n- (boundary) A\r\n  1) Child\r\n- (relationship) B\r\n# Other\r\n- Ignored", "Untitled").unwrap();
        assert_eq!(root.title.as_deref(), Some("Root"));
        assert_eq!(
            root.attached
                .iter()
                .map(|topic| topic.title.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("A"), Some("B")]
        );
        assert_eq!(root.attached[0].attached[0].title.as_deref(), Some("Child"));
    }

    #[test]
    fn labels_links_quotes_and_fences_remain_source_data() {
        let sheets = sheets("before\n# Sheet\n- Root [a\\, b, c] <https://example.com>\n  > note\n  ```\n  - not a topic\n  ```\n\nafter", "Untitled").unwrap();
        let root = &sheets[0]["rootTopic"];
        assert_eq!(root["title"], "Root");
        assert_eq!(root["labels"], json!(["a, b", "c"]));
        assert_eq!(root["href"], "https://example.com");
        assert!(root.get("children").is_none());
        assert_eq!(
            root["notes"]["plain"]["content"],
            "before\n\nnote\n\n```\n- not a topic\n```\n\nafter"
        );
    }

    #[test]
    fn empty_and_deep_outlines_build_real_reopenable_archives() {
        create_xmind_document_from_outline("", "빈 문서").unwrap();
        let nested = (0..1000)
            .map(|level| format!("{}- t{level}", " ".repeat(level)))
            .collect::<Vec<_>>()
            .join("\n");
        for (text, expected_count) in [(nested.clone(), 1000), (format!("- first\n{nested}"), 1002)]
        {
            let sheets = sheets(&text, "Untitled").unwrap();
            check_structure(&sheets).unwrap();
            let mut count = 0;
            let mut depth = 0;
            let mut pending = vec![(&sheets[0]["rootTopic"], 0)];
            while let Some((topic, at)) = pending.pop() {
                count += 1;
                depth = depth.max(at);
                if let Some(children) = topic["children"]["attached"].as_array() {
                    pending.extend(children.iter().map(|child| (child, at + 1)));
                }
            }
            assert_eq!(depth, MAX_TOPIC_DEPTH);
            assert_eq!(count, expected_count);
            create_xmind_document_from_outline(&text, "Untitled").unwrap();
        }
    }
}
