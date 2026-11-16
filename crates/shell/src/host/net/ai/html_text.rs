//! `htmlToText(html, baseUrl)` of `@seuoffice/ai-search` (`html-text.ts`): HTML
//! to readable text for the read_url tool. The page keeps its words and loses
//! everything a reader does not see: script, style, noscript, template and
//! svg are dropped whole; block elements end a line; links become
//! `[text](absolute address)` resolved against the address the page was
//! actually fetched from; images leave only their alt text.

use html5ever::ParseOpts;
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use html5ever::tokenizer::TokenizerOpts;
use jscompat::{js_regex, trim};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Subtrees that never carry page text
const DROPPED: [&str; 5] = ["script", "style", "noscript", "template", "svg"];

/// Elements whose edges become line breaks
const BLOCK: [&str; 41] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "caption",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// The readable text of a page
pub(super) struct HtmlText {
    /// `<title>`, else the first `<h1>`; absent when the page has neither
    pub title: Option<String>,
    pub text: String,
}

/// `value.replace(/\s+/g, ' ')`
fn collapse(value: &str) -> String {
    js_regex!(r"\s+", "g").replace_all(value, |_| " ".to_owned())
}

fn tag_of(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.to_string()),
        _ => None,
    }
}

fn attr_of(node: &Handle, wanted: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find(|attr| str::eq_ignore_ascii_case(&attr.name.local, wanted))
        .map(|attr| attr.value.to_string())
}

fn children_of(node: &Handle) -> Vec<Handle> {
    node.children.borrow().iter().cloned().collect()
}

/// All visible text of a subtree, whitespace collapsed
fn text_of(element: &Handle) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut stack: Vec<Handle> = children_of(element).into_iter().rev().collect();
    while let Some(node) = stack.pop() {
        match &node.data {
            NodeData::Text { contents } => parts.push(contents.borrow().to_string()),
            NodeData::Element { .. } => {
                let dropped = tag_of(&node).is_some_and(|tag| DROPPED.contains(&tag.as_str()));
                if !dropped {
                    stack.extend(children_of(&node).into_iter().rev());
                }
            }
            _ => {}
        }
    }
    trim(&collapse(&parts.join(" "))).to_owned()
}

/// An http(s) absolute address for a link, or `None` when it cannot be one
fn resolve_link(href: &str, base: &reqwest::Url) -> Option<String> {
    let resolved = base.join(href).ok()?;
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

/// One line per block: surrounding whitespace is trimmed and empty lines are dropped
fn tidy_lines(parts: &[String]) -> String {
    parts
        .concat()
        .split('\n')
        .map(|line| trim(&collapse(line)).to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

enum Frame {
    Open(Handle),
    /// the element's own edge, applied on the way out
    Close {
        tag: String,
        /// index in the output where an `<a>`'s inner text begins
        link_at: usize,
        href: Option<String>,
    },
}

/// The readable text of `html`, with relative links resolved against `base`
/// (the address the page was fetched from). An unparsable page comes back as
/// whatever the parser could salvage.
pub(super) fn html_to_text(html: &str, base: &reqwest::Url) -> HtmlText {
    let options = ParseOpts {
        tokenizer: TokenizerOpts {
            discard_bom: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let dom = parse_document(RcDom::default(), options).one(html);
    let mut out: Vec<String> = Vec::new();
    let mut title = String::new();
    let mut first_h1 = String::new();
    let mut stack: Vec<Frame> = children_of(&dom.document)
        .into_iter()
        .rev()
        .map(Frame::Open)
        .collect();
    while let Some(frame) = stack.pop() {
        let node = match frame {
            Frame::Close { tag, link_at, href } => {
                if tag == "a" {
                    let inner = collapse(&out.split_off(link_at).concat());
                    let inner = trim(&inner);
                    let address = href.and_then(|href| resolve_link(&href, base));
                    if !inner.is_empty() {
                        out.push(match address {
                            Some(address) => format!("[{inner}]({address})"),
                            None => inner.to_owned(),
                        });
                    }
                } else if BLOCK.contains(&tag.as_str()) {
                    out.push("\n".to_owned());
                }
                continue;
            }
            Frame::Open(node) => node,
        };
        if let NodeData::Text { contents } = &node.data {
            out.push(collapse(&contents.borrow()));
            continue;
        }
        let Some(tag) = tag_of(&node) else {
            continue;
        };
        if DROPPED.contains(&tag.as_str()) {
            continue;
        }
        let kind = tag.clone();
        match kind.as_str() {
            "title" => {
                if title.is_empty() {
                    title = text_of(&node);
                }
                continue;
            }
            "img" => {
                let alt = collapse(&attr_of(&node, "alt").unwrap_or_default());
                let alt = trim(&alt);
                if !alt.is_empty() {
                    out.push(alt.to_owned());
                }
                continue;
            }
            "br" => {
                out.push("\n".to_owned());
                continue;
            }
            "a" => {
                let href = attr_of(&node, "href");
                stack.push(Frame::Close {
                    tag,
                    link_at: out.len(),
                    href,
                });
            }
            _ => {
                if BLOCK.contains(&kind.as_str()) {
                    out.push("\n".to_owned());
                }
                if kind == "h1" && first_h1.is_empty() {
                    first_h1 = text_of(&node);
                }
                stack.push(Frame::Close {
                    tag,
                    link_at: 0,
                    href: None,
                });
            }
        }
        for child in children_of(&node).into_iter().rev() {
            stack.push(Frame::Open(child));
        }
    }
    let text = tidy_lines(&out);
    let title = if !title.is_empty() {
        Some(title)
    } else if !first_h1.is_empty() {
        Some(first_h1)
    } else {
        None
    };
    HtmlText { title, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(html: &str) -> HtmlText {
        html_to_text(
            html,
            &reqwest::Url::parse("https://example.com/dir/page").unwrap(),
        )
    }

    #[test]
    fn blocks_become_lines_and_links_are_absolute() {
        let page = read(
            "<html><head><title> My  page </title><style>p{}</style></head>\
             <body><h1>Head</h1><p>One <a href=\"../x?y=1\">link  text</a> two</p>\
             <script>var a = 1</script><ul><li>a</li><li>b<br>c</li></ul>\
             <img alt=\" pic \"><a href=\"mailto:a@b.c\">mail</a></body></html>",
        );
        assert_eq!(page.title.as_deref(), Some("My page"));
        assert_eq!(
            page.text,
            "Head\nOne [link text](https://example.com/x?y=1) two\na\nb\nc\npicmail"
        );
    }

    #[test]
    fn heading_names_a_page_without_title() {
        let page = read("<body><h1>Only heading</h1><p>text</p></body>");
        assert_eq!(page.title.as_deref(), Some("Only heading"));
        assert_eq!(read("<p>x</p>").title, None);
    }

    #[test]
    fn dropped_subtrees_leave_no_text() {
        let page = read("<div>a<noscript>hidden</noscript><svg><text>t</text></svg>b</div>");
        assert_eq!(page.text, "ab");
    }
}
