//! ODF's parser options differ from the shared OOXML parser. In particular,
//! valid numeric references remain literal and CDATA arrays are not text nodes
//! to the original ODF helpers. The shared parser still owns tag/attribute
//! parsing, malformed-tag behavior and the nesting/security boundary.
//!
//! Entity/DTD rules translated from fast-xml-parser and @nodable/entities.
//! Unsafe entity patterns translated from is-unsafe.
//! MIT License
//! Copyright (c) 2017 Amit Kumar Gupta
//! Copyright (c) 2026 Nodable
//! Copyright (c) 2026 Natural Intelligence
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use super::Stop;
use jscompat::{Radix, is_js_space, js_regex, parse_int, to_number};
pub(super) use ooxml_xml::{XElement, XNode};
use std::borrow::Cow;
use std::collections::HashMap;

pub(super) fn attr<'a>(node: &'a XElement, name: &str) -> Option<&'a str> {
    node.attrs
        .iter()
        .find(|(key, _)| key == name)
        .or_else(|| {
            node.attrs
                .iter()
                .find(|(key, _)| key == name.split_once(':').map_or(name, |(_, local)| local))
        })
        .map(|(_, value)| value.as_str())
}
pub(super) fn elements(node: &XElement) -> impl Iterator<Item = &XElement> {
    node.children.iter().filter_map(|child| match child {
        XNode::Element(element) => Some(element),
        _ => None,
    })
}
pub(super) fn first<'a>(node: &'a XElement, name: &str) -> Option<&'a XElement> {
    elements(node).find(|child| child.name == name)
}
pub(super) fn descendant<'a>(node: &'a XElement, name: &str) -> Option<&'a XElement> {
    for child in elements(node) {
        if child.name == name {
            return Some(child);
        }
        if let Some(found) = descendant(child, name) {
            return Some(found);
        }
    }
    None
}
pub(super) fn text(node: &XNode, out: &mut String) {
    match node {
        XNode::Text(value) => out.push_str(value),
        XNode::Element(element) => {
            for child in &element.children {
                text(child, out);
            }
        }
    }
}
pub(super) fn text_of(node: &XElement) -> String {
    let mut out = String::new();
    for child in &node.children {
        text(child, &mut out);
    }
    out
}

#[derive(Default)]
struct Entities {
    input: HashMap<String, String>,
    expanded: usize,
    xml11: bool,
}
impl Entities {
    fn decode<'a>(&mut self, value: &'a str, stop: &Stop<'_>) -> Result<Cow<'a, str>, ()> {
        if !value.contains('&') {
            return Ok(Cow::Borrowed(value));
        }
        let mut out = String::with_capacity(value.len());
        let mut at = 0;
        let mut visited = 0usize;
        while let Some(found) = value[at..].find('&') {
            if visited % 1024 == 0 && (stop.0)() {
                return Err(());
            }
            visited += 1;
            let begin = at + found;
            out.push_str(&value[at..begin]);
            let suffix = &value[begin + 1..];
            let end = suffix
                .char_indices()
                .scan(0usize, |units, (offset, character)| {
                    let before = *units;
                    *units += character.len_utf16();
                    Some((offset, character, before))
                })
                .take_while(|(_, _, units)| *units <= 32)
                .find(|(_, character, _)| *character == ';')
                .map(|(end, _, _)| end);
            let Some(end) = end else {
                out.push('&');
                at = begin + 1;
                continue;
            };
            let token = &suffix[..end];
            let replacement = if let Some(number) = token.strip_prefix('#') {
                let code = if let Some(hex) = number
                    .strip_prefix('x')
                    .or_else(|| number.strip_prefix('X'))
                {
                    parse_int(hex, Radix::Sixteen)
                } else {
                    parse_int(number, Radix::Ten)
                };
                (code.is_finite()
                    && (0.0..=1_114_111.0).contains(&code)
                    && (code == 0.0
                        || (55_296.0..=57_343.0).contains(&code)
                        || (!self.xml11
                            && (1.0..=31.0).contains(&code)
                            && ![9.0, 10.0, 13.0].contains(&code))))
                .then_some("")
            } else {
                self.input.get(token).map(String::as_str).or(match token {
                    "amp" => Some("&"),
                    "lt" => Some("<"),
                    "gt" => Some(">"),
                    "quot" => Some("\""),
                    "apos" => Some("'"),
                    _ => None,
                })
            };
            if let Some(replacement) = replacement {
                self.expanded += replacement
                    .encode_utf16()
                    .count()
                    .saturating_sub(token.encode_utf16().count() + 2);
                if self.expanded > 100_000 {
                    return Err(());
                }
                out.push_str(replacement);
                at = begin + end + 2;
            } else {
                out.push('&');
                at = begin + 1;
            }
        }
        out.push_str(&value[at..]);
        Ok(Cow::Owned(out))
    }
}
fn escape(value: &str, out: &mut String) {
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(character),
        }
    }
}
fn quoted_tag<'a>(
    source: &'a str,
    entities: &mut Entities,
    stop: &Stop<'_>,
) -> Result<Cow<'a, str>, ()> {
    if !source.contains(['\'', '"']) {
        return Ok(Cow::Borrowed(source));
    }
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    while let Some(found) = source[at..].find(['\'', '"']) {
        let quote = at + found;
        out.push_str(&source[at..=quote]);
        let close = source[quote + 1..]
            .find(source.as_bytes()[quote] as char)
            .map(|end| quote + 1 + end)
            .ok_or(())?;
        escape(&entities.decode(&source[quote + 1..close], stop)?, &mut out);
        out.push(source.as_bytes()[quote] as char);
        at = close + 1;
    }
    out.push_str(&source[at..]);
    Ok(Cow::Owned(out))
}
fn tag_end(source: &str, at: usize, pi: bool) -> Option<usize> {
    let mut quote = None;
    for (offset, character) in source[at..].char_indices() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '>' && (!pi || source.as_bytes().get(at + offset - 1) == Some(&b'?'))
        {
            return Some(at + offset + 1);
        }
    }
    None
}
fn skip_space(source: &str, at: &mut usize) {
    while let Some(character) = source[*at..]
        .chars()
        .next()
        .filter(|character| is_js_space(*character))
    {
        *at += character.len_utf8();
    }
}
fn name(source: &str, at: &mut usize, quotes_stop: bool, xml11: bool) -> Result<String, ()> {
    skip_space(source, at);
    let start = *at;
    while let Some(character) = source[*at..].chars().next() {
        if is_js_space(character) || (quotes_stop && matches!(character, '\'' | '"')) {
            break;
        }
        *at += character.len_utf8();
    }
    let value = &source[start..*at];
    let start_char = |character: char| {
        matches!(character, 'A'..='Z' | 'a'..='z' | '_' | '\u{c0}'..='\u{d6}' | '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' |
            '\u{370}'..='\u{37d}' | '\u{37f}'..='\u{486}' | '\u{488}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' |
            '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}' | '\u{fdf0}'..='\u{fffd}')
            || (xml11 && matches!(character, '\u{d7}' | '\u{f7}' | '\u{10000}'..='\u{effff}'))
    };
    let mut parts = value.split(':');
    let valid = |part: &str| {
        let mut chars = part.chars();
        chars.next().is_some_and(start_char) && chars.all(|character| start_char(character) ||
            matches!(character, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}') || (xml11 && character == '\u{487}'))
    };
    if !valid(parts.next().unwrap_or("")) {
        return Err(());
    }
    if let Some(local) = parts.next() {
        if !valid(local) || parts.next().is_some() {
            return Err(());
        }
    }
    Ok(value.to_owned())
}
fn identifier<'a>(source: &'a str, at: &mut usize) -> Result<&'a str, ()> {
    let quote = *source.as_bytes().get(*at).ok_or(())?;
    if !matches!(quote, b'\'' | b'"') {
        return Err(());
    }
    *at += 1;
    let begin = *at;
    *at += source[begin..].find(quote as char).ok_or(())?;
    let value = &source[begin..*at];
    *at += 1;
    Ok(value)
}
fn unsafe_entity(value: &str) -> bool {
    macro_rules! any { ($($pattern:literal),+ $(,)?) => { false $(|| js_regex!($pattern, "i").test(value))+ }; }
    any!(
        r"<script[\s>/]",
        r"</script[\s>]",
        r"j[\t\n\r ]*a[\t\n\r ]*v[\t\n\r ]*a[\t\n\r ]*s[\t\n\r ]*c[\t\n\r ]*r[\t\n\r ]*i[\t\n\r ]*p[\t\n\r ]*t[\t\n\r ]*:",
        r"vbscript[\t\n\r ]*:",
        r"data[\t\n\r ]*:[\t\n\r ]*text/html",
        r"data[\t\n\r ]*:[\t\n\r ]*application/xhtml",
        r"data[\t\n\r ]*:[\t\n\r ]*image/svg\+xml",
        r"\bon\w{1,30}\s*=",
        r"(?:&#x0*3[Cc];?|&#0*60;?|&lt;)\s*script",
        r"(?:&#x0*6[Aa];?|&#0*106;?)\s*(?:&#x0*61;?|a)[\s\S]{0,80}script\s*:",
        r"style[\s\S]{0,20}expression\s*\(",
        r"<(?:object|embed)[\s>/]",
        r"<base[\s>]",
        r"<meta[\s\S]{0,40}http-equiv[\s\S]{0,20}refresh",
        r"srcdoc\s*=",
        r"<iframe[\s>/]",
        r"<form[\s>/]",
        r"<!\[CDATA\[",
        r"\]\]>",
        r"<\?(?:xml[\- ]|php|asp)",
        r"<!DOCTYPE(?:[\s[]|$)",
        r#"\bSYSTEM\s+["']"#,
        r#"\bPUBLIC\s+["']"#,
        r"<!ENTITY[\s%]",
        r"(?:&\w{1,20};){3,}",
        r"\bxmlns\s*(?::\w{1,40})?\s*=",
        r"<!--",
        r"-->",
        r"\?>"
    )
}
fn doctype(source: &str, begin: usize, entities: &mut Entities) -> Result<usize, ()> {
    if !source[begin..].starts_with("<!DOCTYPE") {
        return Err(());
    }
    let mut at = begin + 9;
    let mut angles = 1;
    let mut body = false;
    let mut comment = false;
    let mut count = 0;
    while at < source.len() {
        let rest = &source[at..];
        if rest.starts_with('<') && !comment {
            if body && rest.starts_with("<!ENTITY") {
                at += 8;
                let key = name(source, &mut at, true, entities.xml11)?;
                skip_space(source, &mut at);
                let value = identifier(source, &mut at)?;
                if value.encode_utf16().count() > 10_000 {
                    return Err(());
                }
                if !value.contains('&') {
                    if count >= 1000 {
                        return Err(());
                    }
                    count += 1;
                    entities.input.insert(key, value.to_owned());
                }
                at -= 1;
            } else if body && rest.starts_with("<!ELEMENT") {
                at += 9;
                name(source, &mut at, false, entities.xml11)?;
                skip_space(source, &mut at);
                if source[at..].starts_with("EMPTY") {
                    at += 4;
                } else if source[at..].starts_with("ANY") {
                    at += 2;
                } else if source[at..].starts_with('(') {
                    at += source[at..].find(')').ok_or(())?;
                } else {
                    return Err(());
                }
            } else if body && rest.starts_with("<!NOTATION") {
                at += 10;
                name(source, &mut at, false, entities.xml11)?;
                skip_space(source, &mut at);
                let kind = source.get(at..at + 6).ok_or(())?;
                if !kind.eq_ignore_ascii_case("SYSTEM") && !kind.eq_ignore_ascii_case("PUBLIC") {
                    return Err(());
                }
                let public = kind.eq_ignore_ascii_case("PUBLIC");
                at += 6;
                skip_space(source, &mut at);
                let value = identifier(source, &mut at)?;
                if !public && value.is_empty() {
                    return Err(());
                }
                if public {
                    skip_space(source, &mut at);
                    if matches!(source.as_bytes().get(at), Some(b'\'' | b'"')) {
                        identifier(source, &mut at)?;
                    }
                }
                at -= 1;
            } else if body && rest.starts_with("<!ATTLIST") {
                at += 8;
            } else if rest.starts_with("<!--") {
                comment = true;
            } else {
                return Err(());
            }
            angles += 1;
        } else if rest.starts_with('>') {
            if !comment || source[..at].ends_with("--") {
                comment = false;
                angles -= 1;
                if angles == 0 {
                    entities.input.retain(|_, value| !unsafe_entity(value));
                    entities.expanded = 0;
                    return Ok(at + 1);
                }
            }
        } else if rest.starts_with('[') {
            body = true;
        }
        at += source[at..].chars().next().ok_or(())?.len_utf8();
    }
    Err(())
}

pub(super) fn parse(source: &str, stop: &Stop<'_>) -> Result<Option<Vec<XNode>>, String> {
    let normalized = if source.contains('\r') {
        let mut out = String::with_capacity(source.len());
        let mut chars = source.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            } else {
                out.push(character);
            }
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(source)
    };
    let mut entities = Entities::default();
    let mut out = String::with_capacity(normalized.len());
    let mut pending = String::new();
    let mut at = 0;
    let mut doctype_seen = false;
    let mut depth = 0usize;
    let mut detached = false;
    let prepare = (|| -> Result<(), ()> {
        while at < normalized.len() {
            if stop.check().is_err() {
                return Err(());
            }
            let Some(found) = normalized[at..].find('<') else {
                pending.push_str(&normalized[at..]);
                break;
            };
            let begin = at + found;
            pending.push_str(&normalized[at..begin]);
            let rest = &normalized[begin..];
            if rest.starts_with("<!D") {
                if doctype_seen {
                    return Err(());
                }
                doctype_seen = true;
                at = doctype(&normalized, begin, &mut entities)?;
                continue;
            }
            if detached && rest.starts_with("<?") && !pending.is_empty() {
                return Err(());
            }
            if depth > 0
                || rest.starts_with("</")
                || rest.starts_with("<!")
                || rest.starts_with("<?")
            {
                escape(&entities.decode(&pending, stop)?, &mut out);
            }
            pending.clear();
            if rest.starts_with("<!--") {
                let end = rest.find("-->").ok_or(())? + 3;
                out.push_str("<?odf-comment?>");
                at = begin + end;
            } else if rest.starts_with("<![") {
                let end = rest.find("]]>").ok_or(())? + 3;
                // A PI flushes the pending text and retains detached-node errors.
                // Its tree node is removed along with all ignored PIs below.
                out.push_str("<?odf-cdata?>");
                at = begin + end;
            } else {
                let pi = rest.starts_with("<?");
                at = tag_end(&normalized, begin, pi).ok_or(())?;
                let tag = quoted_tag(&normalized[begin..at], &mut entities, stop)?;
                if pi {
                    let parsed = ooxml_xml::parse_ordered(&tag, 100).map_err(|_| ())?;
                    if let Some(XNode::Element(element)) = parsed.first() {
                        if !element.attrs.is_empty() {
                            entities.xml11 = element
                                .attrs
                                .iter()
                                .find(|(key, _)| key == "version")
                                .is_some_and(|(_, value)| to_number(value) == 1.1);
                        }
                    }
                }
                if !pi {
                    if rest.starts_with("</") {
                        if depth == 0 {
                            detached = true;
                        } else {
                            depth -= 1;
                        }
                    } else if !tag[..tag.len() - 1]
                        .trim_end_matches(jscompat::is_js_space)
                        .ends_with('/')
                    {
                        depth += 1;
                    }
                }
                if !pi || !detached {
                    out.push_str(&tag);
                }
            }
        }
        Ok(())
    })();
    stop.check()?;
    if prepare.is_err() {
        return Ok(None);
    }
    let Ok(mut roots) = ooxml_xml::parse_ordered(&out, 100) else {
        return Ok(None);
    };
    fn remove_pi(nodes: &mut Vec<XNode>) {
        nodes.retain(
            |node| !matches!(node, XNode::Element(element) if element.name.starts_with('?')),
        );
        for node in nodes {
            if let XNode::Element(element) = node {
                remove_pi(&mut element.children);
            }
        }
    }
    remove_pi(&mut roots);
    stop.check()?;
    Ok(Some(roots))
}
