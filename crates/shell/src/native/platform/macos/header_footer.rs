//! Chromium's print header and footer, laid over a PDF that WebKit printed.
//!
//! WebKit has no `displayHeaderFooter`, so the body PDF is printed first and the templates are
//! stamped onto its pages afterwards. This file holds everything that needs no AppKit: the
//! template substitution, the page that wraps a template, the placement of the rendered strips
//! and their import into the body PDF. `header_footer_view.rs` renders the strips with WebKit.
//!
//! What it copies from Chromium (`components/printing/resources/
//! print_header_footer_template_page.html` and `PrintHeaderAndFooter` in
//! `print_render_frame_helper.cc`):
//! - A template is laid out in CSS pixels in a page-sized box. The header box is as tall as the
//!   top margin and starts 15pt below the page's top edge; the footer box is as tall as the
//!   bottom margin and ends 15pt above the page's bottom edge. The wrapper's CSS is reproduced
//!   in [`STYLE`].
//! - Elements whose class is `date`, `title`, `url`, `pageNumber` or `totalPages` get that
//!   value as their text. Chromium sets `textContent`, so the value is plain text.
//! - `pageNumber` is the page's number in the whole document and `totalPages` the whole
//!   document's page count: a `pageRanges` selection does not renumber (measured on Chromium
//!   1243: ranges "2,4" of five pages print "2/5" and "4/5").
//! - `date` is the short date and short time of the locale (`11/16/26, 4:07 PM`).
//! - An empty template stands for Chromium's built-in one.
//! - Without a font size a template's text is as small as Chromium makes it (about 1px), so
//!   the root font size is 1px; only templates that set their own size show text.

use std::collections::HashMap;
use std::fmt::Display;
use std::ops::RangeInclusive;

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::native::page::{HeaderFooter, PageError};

/// CSS pixels per inch: the unit Chromium lays templates out in.
const PX_PER_INCH: f64 = 96.0;

/// PDF points per CSS pixel.
const POINTS_PER_PX: f64 = 0.75;

/// A band thinner than this cannot hold a template.
const MIN_BAND_PX: f64 = 1.0;

/// The classes that stand for a value, in the order Chromium assigns them (a later class wins
/// on an element that has several).
const CLASSES: [&str; 5] = ["date", "title", "url", "pageNumber", "totalPages"];

/// Chromium's header when the template is empty.
const DEFAULT_HEADER: &str =
    "<div class='date text left'></div><div class='title text center'></div>";

/// Chromium's footer when the template is empty.
const DEFAULT_FOOTER: &str = "<div class='url text left grow'></div><div class='text right'>\
     <span class='pageNumber'></span>/<span class='totalPages'></span></div>";

/// The wrapper page's CSS: Chromium's, with a transparent page so that only the template's own
/// paint lands on the body PDF.
const STYLE: &str = "html{background:transparent}\
body{display:flex;flex-direction:column;margin:0;font-size:1px;background:transparent}\
#header,#footer{display:flex;flex:none}\
#header{align-items:flex-start;padding-top:15pt}\
#footer{align-items:flex-end;padding-bottom:15pt}\
#content{flex:auto}\
.left{flex:none;padding-left:24pt;padding-right:6pt}\
.center{flex:auto;padding-left:24pt;padding-right:24pt;text-align:center}\
.right{flex:none;padding-left:6pt;padding-right:24pt}\
.grow{flex:auto}\
.text{font-size:8pt;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}";

/// A template can load nothing and run nothing: it is text and styling for one page.
const POLICY: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
                      font-src data:";

const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is text, not markup.
const RAW_TEXT: [&str; 4] = ["script", "style", "textarea", "title"];

pub(super) fn failure(what: impl Display) -> PageError {
    PageError::Platform(format!(
        "The header and footer could not be added to the PDF: {what}"
    ))
}

// ---- template substitution ----

/// What the template classes stand for on one page.
pub(super) struct Values<'a> {
    pub page_number: u32,
    pub total_pages: u32,
    pub date: &'a str,
    pub title: &'a str,
    pub url: &'a str,
}

impl Values<'_> {
    fn for_classes(&self, classes: &str) -> Option<String> {
        let mut found = None;
        for class in CLASSES {
            if classes.split_ascii_whitespace().any(|token| token == class) {
                found = Some(class);
            }
        }
        Some(match found? {
            "date" => self.date.to_owned(),
            "title" => self.title.to_owned(),
            "url" => self.url.to_owned(),
            "pageNumber" => self.page_number.to_string(),
            _ => self.total_pages.to_string(),
        })
    }
}

struct Tag<'a> {
    start: usize,
    end: usize,
    /// Where scanning goes on: after the tag, or past the text of a raw-text element.
    resume: usize,
    closing: bool,
    /// Lower case.
    name: String,
    attributes: &'a str,
}

/// The end of the text of a raw-text element that starts at `from`.
fn raw_end(html: &str, from: usize, name: &str) -> usize {
    let needle = format!("</{name}");
    // ASCII lower casing keeps every byte offset.
    html[from..]
        .to_ascii_lowercase()
        .find(&needle)
        .map_or(html.len(), |at| from + at)
}

/// The first tag at or after `from`. Comments and a `<` that starts no tag are skipped.
fn next_tag(html: &str, from: usize) -> Option<Tag<'_>> {
    let bytes = html.as_bytes();
    let mut at = from;
    loop {
        let start = at + html[at..].find('<')?;
        if html[start..].starts_with("<!--") {
            at = start + 4 + html[start + 4..].find("-->")? + 3;
            continue;
        }
        let closing = bytes.get(start + 1) == Some(&b'/');
        let name_start = start + 1 + usize::from(closing);
        if !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            at = start + 1;
            continue;
        }
        let name_len = bytes[name_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'-')
            .count();
        let name_end = name_start + name_len;
        let mut quote = None;
        let mut end = None;
        for (offset, &byte) in bytes[name_end..].iter().enumerate() {
            match (quote, byte) {
                (None, b'>') => {
                    end = Some(name_end + offset + 1);
                    break;
                }
                (None, b'"' | b'\'') => quote = Some(byte),
                (Some(open), _) if open == byte => quote = None,
                _ => {}
            }
        }
        let end = end?;
        let name = html[name_start..name_end].to_ascii_lowercase();
        let raw = !closing && RAW_TEXT.contains(&name.as_str());
        let resume = if raw { raw_end(html, end, &name) } else { end };
        let attributes = &html[name_end..end - 1];
        return Some(Tag {
            start,
            end,
            resume,
            closing,
            name,
            attributes,
        });
    }
}

/// The value of the `class` attribute in an attribute list.
fn class_of(attributes: &str) -> &str {
    let bytes = attributes.as_bytes();
    let is_name_end = |byte: u8| byte.is_ascii_whitespace() || byte == b'=' || byte == b'/';
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        let name_start = at;
        while at < bytes.len() && !is_name_end(bytes[at]) {
            at += 1;
        }
        let name = &attributes[name_start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = "";
        if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let begin = at + 1;
                    let close = attributes[begin..]
                        .find(char::from(quote))
                        .map_or(attributes.len(), |found| begin + found);
                    value = &attributes[begin..close];
                    at = (close + 1).min(attributes.len());
                }
                _ => {
                    let begin = at;
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                        at += 1;
                    }
                    value = &attributes[begin..at];
                }
            }
        }
        if name.eq_ignore_ascii_case("class") {
            return value;
        }
    }
    ""
}

/// Where the content of the element opened by `open` ends: the start of its closing tag, or
/// the end of the template when it is never closed (the parser closes it there).
fn content_end(template: &str, open: &Tag<'_>) -> usize {
    let mut depth = 1_usize;
    let mut scan = open.end;
    while let Some(tag) = next_tag(template, scan) {
        scan = tag.resume;
        if tag.name != open.name {
            continue;
        }
        if tag.closing {
            depth -= 1;
            if depth == 0 {
                return tag.start;
            }
        } else {
            depth += 1;
        }
    }
    template.len()
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// `template` with the content of every element that has a value class replaced by that value
/// as escaped text. An empty template is Chromium's built-in `default`.
pub(super) fn fill(template: &str, default: &str, values: &Values<'_>) -> String {
    let template = if template.is_empty() {
        default
    } else {
        template
    };
    let mut out = String::with_capacity(template.len());
    let (mut written, mut scan) = (0, 0);
    while let Some(tag) = next_tag(template, scan) {
        scan = tag.resume;
        if tag.closing || VOID_ELEMENTS.contains(&tag.name.as_str()) {
            continue;
        }
        let Some(text) = values.for_classes(class_of(tag.attributes)) else {
            continue;
        };
        let close = content_end(template, &tag);
        out.push_str(&template[written..tag.end]);
        escape_into(&mut out, &text);
        written = close;
        scan = close;
    }
    out.push_str(&template[written..]);
    out
}

/// Whether a template paints nothing: no text, and no tag with attributes (which could carry
/// a background, an image or a size).
pub(super) fn is_blank(template: &str) -> bool {
    let mut scan = 0;
    while let Some(tag) = next_tag(template, scan) {
        let text = &template[scan..tag.start];
        let raw = &template[tag.end..tag.resume];
        let styled = !tag.closing && !tag.attributes.trim().is_empty();
        if !text.trim().is_empty() || !raw.trim().is_empty() || styled {
            return false;
        }
        scan = tag.resume;
    }
    template[scan..].trim().is_empty()
}

// ---- the wrapper page ----

/// The page a template is laid out in, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Canvas {
    pub width: f64,
    pub height: f64,
    pub top: f64,
    pub bottom: f64,
}

impl Canvas {
    /// The part of the page the header is rendered from.
    pub(super) fn header_rect(&self) -> (f64, f64, f64, f64) {
        (0.0, 0.0, self.width, self.top)
    }

    /// The part of the page the footer is rendered from.
    pub(super) fn footer_rect(&self) -> (f64, f64, f64, f64) {
        (0.0, self.height - self.bottom, self.width, self.bottom)
    }
}

/// Chromium's wrapper page with the two filled templates in it.
pub(super) fn wrapper_html(canvas: &Canvas, header: &str, footer: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{POLICY}\">\
         <style>{STYLE}body{{width:{width}px;height:{height}px}}\
         #header{{height:{top}px}}#footer{{height:{bottom}px}}</style></head>\
         <body><div id=\"header\">{header}</div><div id=\"content\"></div>\
         <div id=\"footer\">{footer}</div></body></html>",
        width = canvas.width,
        height = canvas.height,
        top = canvas.top,
        bottom = canvas.bottom,
    )
}

// ---- page geometry and placement ----

/// An affine transform in PDF's convention: `x' = a x + c y + e`, `y' = b x + d y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Matrix {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Matrix {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn scale(factor: f64) -> Self {
        Self {
            a: factor,
            d: factor,
            ..Self::IDENTITY
        }
    }

    fn translate(x: f64, y: f64) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    /// This transform first, then `next`.
    fn then(self, next: Self) -> Self {
        Self {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            e: self.e * next.a + self.f * next.c + next.e,
            f: self.e * next.b + self.f * next.d + next.f,
        }
    }

    /// The `cm` operator that applies this transform.
    fn operator(&self) -> String {
        // Rounded, and never `-0.000000`.
        let number = |value: f64| format!("{:.6}", (value * 1e6).round() / 1e6 + 0.0);
        let [a, b, c, d, e, f] = [self.a, self.b, self.c, self.d, self.e, self.f].map(number);
        format!("{a} {b} {c} {d} {e} {f} cm")
    }
}

/// Which margin a strip sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Band {
    Header,
    Footer,
}

/// A page's `MediaBox` and `Rotate`, in points of the page's own user space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Clockwise, a multiple of 90.
    pub rotate: u32,
}

impl Bounds {
    /// The page as a viewer shows it.
    fn visual(&self) -> (f64, f64) {
        if self.rotate % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// From the page as shown (origin at its bottom left) to user space.
    fn upright(&self) -> Matrix {
        let (w, h) = (self.width, self.height);
        let turn = match self.rotate {
            90 => Matrix {
                a: 0.0,
                b: 1.0,
                c: -1.0,
                d: 0.0,
                e: w,
                f: 0.0,
            },
            180 => Matrix {
                a: -1.0,
                b: 0.0,
                c: 0.0,
                d: -1.0,
                e: w,
                f: h,
            },
            270 => Matrix {
                a: 0.0,
                b: -1.0,
                c: 1.0,
                d: 0.0,
                e: 0.0,
                f: h,
            },
            _ => Matrix::IDENTITY,
        };
        turn.then(Matrix::translate(self.x, self.y))
    }

    /// Where a strip goes. `strip` is the strip's own box, as large as the page is wide in
    /// whatever unit WebKit used; the strip is scaled to the page's width, and sits against the
    /// top edge (header) or the bottom edge (footer).
    pub(super) fn placement(&self, band: Band, strip: [f64; 4]) -> Matrix {
        let (visual_width, visual_height) = self.visual();
        let [x, y, width, height] = strip;
        let scale = visual_width / width;
        let rise = match band {
            Band::Header => visual_height - height * scale,
            Band::Footer => 0.0,
        };
        Matrix::translate(-x, -y)
            .then(Matrix::scale(scale))
            .then(Matrix::translate(0.0, rise))
            .then(self.upright())
    }
}

/// An attribute a page inherits from its page tree.
fn inherited<'a>(document: &'a Document, page: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut id = page;
    for _ in 0..32 {
        let node = document.get_dictionary(id).ok()?;
        if let Ok(value) = node.get(key) {
            return document.dereference(value).ok().map(|(_, object)| object);
        }
        id = node.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

/// The `MediaBox` as `[x, y, width, height]`.
pub(super) fn media_box(document: &Document, page: ObjectId) -> Result<[f64; 4], PageError> {
    let numbers: Vec<f64> = inherited(document, page, b"MediaBox")
        .and_then(|object| object.as_array().ok())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| document.dereference(item).ok())
                .filter_map(|(_, object)| object.as_float().ok())
                .map(f64::from)
                .collect()
        })
        .unwrap_or_default();
    let [x0, y0, x1, y1] = numbers[..] else {
        return Err(failure("a page has no valid MediaBox"));
    };
    let (width, height) = ((x1 - x0).abs(), (y1 - y0).abs());
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return Err(failure("a page has an empty MediaBox"));
    }
    Ok([x0.min(x1), y0.min(y1), width, height])
}

fn bounds_of(document: &Document, page: ObjectId) -> Result<Bounds, PageError> {
    let [x, y, width, height] = media_box(document, page)?;
    let rotate = inherited(document, page, b"Rotate")
        .and_then(|object| object.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360);
    let rotate = match u32::try_from(rotate) {
        Ok(turn @ (0 | 90 | 180 | 270)) => turn,
        _ => return Err(failure("a page has an unsupported Rotate")),
    };
    Ok(Bounds {
        x,
        y,
        width,
        height,
        rotate,
    })
}

// ---- the plan ----

/// The page values that do not change from page to page.
pub(super) struct Context {
    pub date: String,
    pub title: String,
    pub url: String,
}

/// One distinct wrapper page to render. Pages whose filled templates and size are the same
/// share one job.
pub(super) struct Job {
    pub html: String,
    pub canvas: Canvas,
    pub header: bool,
    pub footer: bool,
}

/// One body page that gets strips from a job.
pub(super) struct Stamp {
    page: ObjectId,
    bounds: Bounds,
    job: usize,
    header: bool,
    footer: bool,
}

pub(super) struct Plan {
    pub jobs: Vec<Job>,
    pub stamps: Vec<Stamp>,
}

fn selected(ranges: &[RangeInclusive<u32>], page: u32) -> bool {
    ranges.is_empty() || ranges.iter().any(|range| range.contains(&page))
}

/// The jobs and stamps for the pages `ranges` selects. Page numbers and the page count are the
/// body document's, not the selection's.
pub(super) fn plan(
    document: &Document,
    margins_in: [f64; 4],
    templates: &HeaderFooter,
    ranges: &[RangeInclusive<u32>],
    context: &Context,
) -> Result<Plan, PageError> {
    let pages = document.get_pages();
    let total_pages = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    // An empty template is not blank: it stands for Chromium's built-in one.
    let blank = |template: &str| !template.is_empty() && is_blank(template);
    let header_blank = blank(&templates.header_template);
    let footer_blank = blank(&templates.footer_template);
    let [top_in, _, bottom_in, _] = margins_in;
    let (top, bottom) = (top_in * PX_PER_INCH, bottom_in * PX_PER_INCH);
    let mut jobs: Vec<Job> = Vec::new();
    let mut known: HashMap<String, usize> = HashMap::new();
    let mut stamps = Vec::new();
    for (&number, &page) in &pages {
        if !selected(ranges, number) {
            continue;
        }
        let bounds = bounds_of(document, page)?;
        let (width, height) = bounds.visual();
        let canvas = Canvas {
            width: width / POINTS_PER_PX,
            height: height / POINTS_PER_PX,
            top,
            bottom,
        };
        let values = Values {
            page_number: number,
            total_pages,
            date: &context.date,
            title: &context.title,
            url: &context.url,
        };
        let header =
            (!header_blank).then(|| fill(&templates.header_template, DEFAULT_HEADER, &values));
        let footer =
            (!footer_blank).then(|| fill(&templates.footer_template, DEFAULT_FOOTER, &values));
        if header.is_none() && footer.is_none() {
            continue;
        }
        if header.is_some() && top < MIN_BAND_PX {
            return Err(PageError::Invalid(
                "The top margin is too small for a header.".to_owned(),
            ));
        }
        if footer.is_some() && bottom < MIN_BAND_PX {
            return Err(PageError::Invalid(
                "The bottom margin is too small for a footer.".to_owned(),
            ));
        }
        let (has_header, has_footer) = (header.is_some(), footer.is_some());
        let (head, foot) = (
            header.as_deref().unwrap_or(""),
            footer.as_deref().unwrap_or(""),
        );
        let html = wrapper_html(&canvas, head, foot);
        let job = match known.get(&html) {
            Some(&job) => job,
            None => {
                let job = jobs.len();
                known.insert(html.clone(), job);
                jobs.push(Job {
                    html,
                    canvas,
                    header: has_header,
                    footer: has_footer,
                });
                job
            }
        };
        stamps.push(Stamp {
            page,
            bounds,
            job,
            header: has_header,
            footer: has_footer,
        });
    }
    Ok(Plan { jobs, stamps })
}

// ---- merging into the body PDF ----

/// The PDF of each strip of a job, as WebKit rendered it.
#[derive(Default)]
pub(super) struct Strips {
    pub header: Option<Vec<u8>>,
    pub footer: Option<Vec<u8>>,
}

/// A strip imported into the body document as a Form XObject.
struct Form {
    id: ObjectId,
    /// The strip's own box: `[x, y, width, height]`.
    bbox: [f64; 4],
}

/// Copies the objects a strip needs from its own document into the body document.
struct Copier<'a> {
    source: &'a Document,
    target: &'a mut Document,
    ids: HashMap<ObjectId, ObjectId>,
    queue: Vec<ObjectId>,
}

impl Copier<'_> {
    fn id_for(&mut self, old: ObjectId) -> ObjectId {
        if let Some(&new) = self.ids.get(&old) {
            return new;
        }
        let new = self.target.new_object_id();
        self.ids.insert(old, new);
        self.queue.push(old);
        new
    }

    /// `object` with every reference pointing into the target.
    fn remap(&mut self, object: &Object) -> Object {
        match object {
            Object::Reference(old) => Object::Reference(self.id_for(*old)),
            Object::Array(items) => Object::Array(items.iter().map(|i| self.remap(i)).collect()),
            Object::Dictionary(dict) => Object::Dictionary(self.remap_dictionary(dict)),
            Object::Stream(stream) => Object::Stream(Stream {
                dict: self.remap_dictionary(&stream.dict),
                content: stream.content.clone(),
                allows_compression: stream.allows_compression,
                start_position: None,
            }),
            other => other.clone(),
        }
    }

    fn remap_dictionary(&mut self, dictionary: &Dictionary) -> Dictionary {
        let mut copy = Dictionary::new();
        for (key, value) in dictionary {
            copy.set(key.clone(), self.remap(value));
        }
        copy
    }

    /// Copy every object referenced so far, and what those reference in turn.
    fn drain(&mut self) {
        let source = self.source;
        while let Some(old) = self.queue.pop() {
            let new = self.ids[&old];
            let found = source.objects.get(&old);
            let object = found.map_or(Object::Null, |object| self.remap(object));
            self.target.objects.insert(new, object);
        }
    }
}

/// The first page of `bytes` as a Form XObject of `target`.
fn import(target: &mut Document, bytes: &[u8]) -> Result<Form, PageError> {
    let source = Document::load_mem(bytes).map_err(failure)?;
    let page = source
        .get_pages()
        .values()
        .next()
        .copied()
        .ok_or_else(|| failure("a rendered strip has no page"))?;
    let bounds = bounds_of(&source, page)?;
    if bounds.rotate != 0 {
        return Err(failure("a rendered strip is rotated"));
    }
    let content = source.get_page_content(page).map_err(failure)?;
    let mut copier = Copier {
        source: &source,
        target: &mut *target,
        ids: HashMap::new(),
        queue: Vec::new(),
    };
    let resources = inherited(&source, page, b"Resources")
        .map(|resources| copier.remap(resources))
        .unwrap_or_else(|| Object::Dictionary(Dictionary::new()));
    copier.drain();
    let mut dictionary = Dictionary::new();
    dictionary.set("Type", "XObject");
    dictionary.set("Subtype", "Form");
    dictionary.set(
        "BBox",
        Object::Array(
            [
                bounds.x,
                bounds.y,
                bounds.x + bounds.width,
                bounds.y + bounds.height,
            ]
            .into_iter()
            .map(Object::from)
            .collect(),
        ),
    );
    dictionary.set("Resources", resources);
    let mut stream = Stream::new(dictionary, content);
    stream.compress().map_err(failure)?;
    let id = target.add_object(stream);
    Ok(Form {
        id,
        bbox: [bounds.x, bounds.y, bounds.width, bounds.height],
    })
}

/// Give `page` a `Resources` of its own, so that adding to it leaves the pages that share an
/// inherited one alone.
fn own_resources(document: &mut Document, page: ObjectId) -> Result<(), PageError> {
    if document
        .get_dictionary(page)
        .map_err(failure)?
        .has(b"Resources")
    {
        return Ok(());
    }
    if let Some(resources) = inherited(document, page, b"Resources").cloned() {
        document
            .get_dictionary_mut(page)
            .map_err(failure)?
            .set("Resources", resources);
    }
    Ok(())
}

/// Whether the page can see an XObject named `name`.
fn sees_xobject(document: &Document, page: ObjectId, name: &str) -> bool {
    inherited(document, page, b"Resources")
        .and_then(|resources| resources.as_dict().ok())
        .and_then(|resources| resources.get_deref(b"XObject", document).ok())
        .and_then(|xobjects| xobjects.as_dict().ok())
        .is_some_and(|xobjects| xobjects.has(name.as_bytes()))
}

/// Draw `forms` on `page`. The page's own content is wrapped in `q`/`Q` first, so that the
/// strips are drawn in the default graphics state whatever the content leaves behind.
fn draw(
    document: &mut Document,
    page: ObjectId,
    forms: &[(String, ObjectId, Matrix)],
) -> Result<(), PageError> {
    own_resources(document, page)?;
    let mut operators = String::from("Q\n");
    for (name, form, placement) in forms {
        document
            .add_xobject(page, name.as_bytes().to_vec(), *form)
            .map_err(failure)?;
        if !sees_xobject(document, page, name) {
            return Err(failure("a page's resources cannot take a strip"));
        }
        operators.push_str(&format!("q {} /{name} Do Q\n", placement.operator()));
    }
    let bodies = document.get_page_contents(page);
    let open = document.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let close = document.add_object(Stream::new(Dictionary::new(), operators.into_bytes()));
    let mut contents = vec![Object::Reference(open)];
    contents.extend(bodies.into_iter().map(Object::Reference));
    contents.push(Object::Reference(close));
    document
        .get_dictionary_mut(page)
        .map_err(failure)?
        .set("Contents", contents);
    Ok(())
}

/// Draw the rendered strips of each job on the pages stamped with it.
pub(super) fn apply(
    document: &mut Document,
    stamps: &[Stamp],
    strips: &[Strips],
) -> Result<(), PageError> {
    let mut forms: Vec<(Option<Form>, Option<Form>)> = Vec::with_capacity(strips.len());
    for rendered in strips {
        let header = rendered
            .header
            .as_deref()
            .map(|bytes| import(document, bytes))
            .transpose()?;
        let footer = rendered
            .footer
            .as_deref()
            .map(|bytes| import(document, bytes))
            .transpose()?;
        forms.push((header, footer));
    }
    for stamp in stamps {
        let (header, footer) = forms
            .get(stamp.job)
            .ok_or_else(|| failure("a page has no rendered strip"))?;
        let mut placed = Vec::new();
        for (band, wanted, form, tag) in [
            (Band::Header, stamp.header, header, "h"),
            (Band::Footer, stamp.footer, footer, "f"),
        ] {
            match (wanted, form) {
                (false, _) => {}
                (true, None) => return Err(failure("a strip was not rendered")),
                (true, Some(form)) => placed.push((
                    format!("SeuOfficeHF{}{tag}", stamp.job),
                    form.id,
                    stamp.bounds.placement(band, form.bbox),
                )),
            }
        }
        draw(document, stamp.page, &placed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values<'a>(page: u32, total: u32) -> Values<'a> {
        Values {
            page_number: page,
            total_pages: total,
            date: "11/16/26, 4:07 PM",
            title: "Q&A <1>",
            url: "tauri://localhost/x?a=1&b=2",
        }
    }

    fn filled(template: &str) -> String {
        fill(template, "", &values(2, 5))
    }

    #[test]
    fn value_classes_get_their_text_and_nothing_else_changes() {
        let template = "<div style=\"font-size:9px\"><span class=\"pageNumber\"></span>/\
                        <span class='totalPages'>x</span> <b class=\"title url\">t</b></div>";
        assert_eq!(
            filled(template),
            "<div style=\"font-size:9px\"><span class=\"pageNumber\">2</span>/\
             <span class='totalPages'>5</span> <b class=\"title url\">\
             tauri://localhost/x?a=1&amp;b=2</b></div>"
        );
    }

    #[test]
    fn substituted_text_is_escaped() {
        assert_eq!(
            filled("<i class=\"title\"></i>"),
            "<i class=\"title\">Q&amp;A &lt;1&gt;</i>"
        );
    }

    #[test]
    fn a_matched_element_loses_its_content_up_to_its_own_closing_tag() {
        assert_eq!(
            filled("<div class=\"date\"><div>inner</div>old</div><div>tail</div>"),
            "<div class=\"date\">11/16/26, 4:07 PM</div><div>tail</div>"
        );
    }

    #[test]
    fn only_whole_class_names_match() {
        let template = "<span class=\"pageNumbers\">k</span><span class=\"x date\"></span>";
        assert_eq!(
            filled(template),
            "<span class=\"pageNumbers\">k</span><span class=\"x date\">11/16/26, 4:07 PM</span>"
        );
    }

    #[test]
    fn markup_that_is_not_an_element_is_skipped() {
        let template = "<!-- <span class=\"date\"></span> --><script>if (a<b) {}</script>\
                        <br class=\"date\"><span data-x=\"a>b\" class=url></span>";
        assert_eq!(
            filled(template),
            "<!-- <span class=\"date\"></span> --><script>if (a<b) {}</script>\
             <br class=\"date\"><span data-x=\"a>b\" class=url>tauri://localhost/x?a=1&amp;b=2</span>"
        );
    }

    #[test]
    fn an_element_that_is_never_closed_runs_to_the_end() {
        assert_eq!(
            filled("<span class=\"date\">stale"),
            "<span class=\"date\">11/16/26, 4:07 PM"
        );
    }

    #[test]
    fn an_empty_template_is_chromiums_built_in_one() {
        let footer = fill("", DEFAULT_FOOTER, &values(2, 5));
        assert!(
            footer.contains("<span class='pageNumber'>2</span>/<span class='totalPages'>5</span>")
        );
        assert!(footer.contains("tauri://localhost/x?a=1&amp;b=2"));
        assert!(fill("", DEFAULT_HEADER, &values(2, 5)).contains("11/16/26, 4:07 PM"));
    }

    #[test]
    fn a_template_without_text_or_attributes_is_blank() {
        assert!(is_blank(""));
        assert!(is_blank("<span></span>"));
        assert!(is_blank(" <div> <br> </div> "));
        assert!(!is_blank("<span>0</span>"));
        assert!(!is_blank("<div style=\"background:red\"></div>"));
        assert!(!is_blank("<span class=\"pageNumber\"></span>"));
    }

    #[test]
    fn the_wrapper_page_has_the_pages_size_and_both_templates() {
        let canvas = Canvas {
            width: 800.0,
            height: 1000.0,
            top: 96.0,
            bottom: 48.0,
        };
        let html = wrapper_html(&canvas, "<b>H</b>", "<i>F</i>");
        assert!(html.contains("body{width:800px;height:1000px}"));
        assert!(html.contains("#header{height:96px}#footer{height:48px}"));
        assert!(html.contains("<div id=\"header\"><b>H</b></div>"));
        assert!(html.contains("<div id=\"footer\"><i>F</i></div>"));
        assert!(html.contains("default-src 'none'"));
        assert_eq!(canvas.header_rect(), (0.0, 0.0, 800.0, 96.0));
        assert_eq!(canvas.footer_rect(), (0.0, 952.0, 800.0, 48.0));
    }

    fn at(matrix: &Matrix, x: f64, y: f64) -> (f64, f64) {
        (
            matrix.a * x + matrix.c * y + matrix.e,
            matrix.b * x + matrix.d * y + matrix.f,
        )
    }

    fn near(actual: (f64, f64), expected: (f64, f64)) {
        assert!(
            (actual.0 - expected.0).abs() < 1e-3 && (actual.1 - expected.1).abs() < 1e-3,
            "{actual:?} is not {expected:?}"
        );
    }

    fn page(rotate: u32) -> Bounds {
        Bounds {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 800.0,
            rotate,
        }
    }

    #[test]
    fn a_header_strip_sits_against_the_top_edge_and_is_scaled_to_the_width() {
        let strip = [0.0, 0.0, 800.0, 96.0];
        let header = page(0).placement(Band::Header, strip);
        near(at(&header, 0.0, 96.0), (0.0, 800.0));
        near(at(&header, 800.0, 0.0), (600.0, 728.0));
        let footer = page(0).placement(Band::Footer, strip);
        near(at(&footer, 0.0, 0.0), (0.0, 0.0));
        near(at(&footer, 800.0, 96.0), (600.0, 72.0));
    }

    #[test]
    fn the_media_box_origin_and_the_strips_own_origin_are_honoured() {
        let bounds = Bounds {
            x: 10.0,
            y: 20.0,
            ..page(0)
        };
        let footer = bounds.placement(Band::Footer, [5.0, 7.0, 800.0, 96.0]);
        near(at(&footer, 5.0, 7.0), (10.0, 20.0));
        near(at(&footer, 805.0, 103.0), (610.0, 92.0));
    }

    #[test]
    fn a_rotated_page_gets_its_strips_against_the_edge_the_viewer_shows() {
        let strip = [0.0, 0.0, 1_066.666_666_7, 50.0];
        let header = page(90).placement(Band::Header, strip);
        near(at(&header, 0.0, 50.0), (0.0, 0.0));
        near(at(&header, 1_066.666_666_7, 0.0), (37.5, 800.0));
        let footer = page(180).placement(Band::Footer, [0.0, 0.0, 600.0, 40.0]);
        near(at(&footer, 0.0, 0.0), (600.0, 800.0));
        near(at(&footer, 600.0, 40.0), (0.0, 760.0));
        let footer = page(270).placement(Band::Footer, [0.0, 0.0, 800.0, 40.0]);
        near(at(&footer, 0.0, 0.0), (0.0, 800.0));
        near(at(&footer, 800.0, 0.0), (0.0, 0.0));
    }

    fn name(text: &str) -> Object {
        Object::Name(text.as_bytes().to_vec())
    }

    /// A PDF with one page per media box, each drawing a Helvetica text. The font is in the
    /// page tree's `Resources` when `inherit` is set and in every page otherwise.
    fn pdf(media: &[[i64; 4]], inherit: bool) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let tree_id = document.new_object_id();
        let font = document.add_object(Dictionary::from_iter([
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name("Helvetica")),
        ]));
        let fonts = Dictionary::from_iter([("F1", Object::Reference(font))]);
        let resources = Dictionary::from_iter([("Font", Object::Dictionary(fonts))]);
        let mut kids = Vec::new();
        for (index, bounds) in media.iter().enumerate() {
            let text = format!("BT /F1 12 Tf 50 700 Td (page {index}) Tj ET");
            let content = document.add_object(Stream::new(Dictionary::new(), text.into_bytes()));
            let mut page = Dictionary::from_iter([
                ("Type", name("Page")),
                ("Parent", Object::Reference(tree_id)),
                (
                    "MediaBox",
                    Object::Array(bounds.iter().map(|n| Object::Integer(*n)).collect()),
                ),
                ("Contents", Object::Reference(content)),
            ]);
            if !inherit {
                page.set("Resources", resources.clone());
            }
            kids.push(Object::Reference(document.add_object(page)));
        }
        let mut tree = Dictionary::from_iter([
            ("Type", name("Pages")),
            ("Count", Object::Integer(i64::try_from(kids.len()).unwrap())),
            ("Kids", Object::Array(kids)),
        ]);
        if inherit {
            tree.set("Resources", resources);
        }
        document.objects.insert(tree_id, Object::Dictionary(tree));
        let catalog = document.add_object(Dictionary::from_iter([
            ("Type", name("Catalog")),
            ("Pages", Object::Reference(tree_id)),
        ]));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    const A4: [i64; 4] = [0, 0, 600, 800];

    fn context() -> Context {
        Context {
            date: "today".into(),
            title: "Sheet".into(),
            url: "tauri://x".into(),
        }
    }

    fn templates(header: &str, footer: &str) -> HeaderFooter {
        HeaderFooter {
            header_template: header.into(),
            footer_template: footer.into(),
        }
    }

    const NUMBERED: &str = "<span class=\"pageNumber\"></span>";
    const NONE: &str = "<span></span>";
    const MARGINS: [f64; 4] = [1.0, 0.5, 0.5, 0.5];

    #[test]
    fn the_page_number_and_count_are_the_documents_whatever_the_selection() {
        let document = Document::load_mem(&pdf(&[A4, A4, A4], false)).unwrap();
        let header = "<span class=\"pageNumber\"></span>/<span class=\"totalPages\"></span>";
        let plan = plan(
            &document,
            MARGINS,
            &templates(header, NONE),
            &[2..=3],
            &context(),
        );
        let plan = plan.unwrap();
        assert_eq!(plan.stamps.len(), 2);
        assert_eq!(plan.jobs.len(), 2);
        let second = "<span class=\"pageNumber\">2</span>/<span class=\"totalPages\">3</span>";
        assert!(plan.jobs[0].html.contains(second));
        assert!(
            plan.jobs[1]
                .html
                .contains("<span class=\"pageNumber\">3</span>/")
        );
    }

    #[test]
    fn pages_with_the_same_strips_share_one_job() {
        let document = Document::load_mem(&pdf(&[A4, A4, [0, 0, 800, 600]], false)).unwrap();
        let plain = templates("<b>Sheet</b>", NONE);
        let same = plan(&document, MARGINS, &plain, &[], &context()).unwrap();
        assert_eq!(same.stamps.len(), 3);
        assert_eq!(same.jobs.len(), 2, "one job per page size");
        let numbered = plan(
            &document,
            MARGINS,
            &templates(NUMBERED, NONE),
            &[],
            &context(),
        );
        assert_eq!(numbered.unwrap().jobs.len(), 3);
    }

    #[test]
    fn blank_templates_need_no_render() {
        let document = Document::load_mem(&pdf(&[A4], false)).unwrap();
        let none = plan(&document, MARGINS, &templates(NONE, NONE), &[], &context()).unwrap();
        assert!(none.jobs.is_empty() && none.stamps.is_empty());
        let footer = plan(&document, MARGINS, &templates(NONE, "x"), &[], &context()).unwrap();
        assert_eq!(
            (footer.jobs[0].header, footer.jobs[0].footer),
            (false, true)
        );
    }

    #[test]
    fn an_empty_template_is_not_blank_it_is_the_built_in_one() {
        let document = Document::load_mem(&pdf(&[A4], false)).unwrap();
        let plan = plan(&document, MARGINS, &templates("", ""), &[], &context()).unwrap();
        assert_eq!((plan.jobs[0].header, plan.jobs[0].footer), (true, true));
        assert!(
            plan.jobs[0]
                .html
                .contains("<div class='title text center'>Sheet</div>")
        );
    }

    #[test]
    fn a_template_in_a_margin_without_height_is_an_error() {
        let document = Document::load_mem(&pdf(&[A4], false)).unwrap();
        let flat = [0.0, 0.5, 0.5, 0.5];
        let result = plan(&document, flat, &templates("Header", NONE), &[], &context());
        assert!(matches!(result, Err(PageError::Invalid(_))));
        let blank = plan(&document, [0.0; 4], &templates(NONE, NONE), &[], &context());
        assert!(blank.is_ok());
    }

    #[test]
    fn rotate_and_media_box_are_read_from_the_page_or_its_tree() {
        let mut document = Document::load_mem(&pdf(&[[10, 20, 610, 820]], true)).unwrap();
        let page = *document.get_pages().get(&1).unwrap();
        document
            .get_dictionary_mut(page)
            .unwrap()
            .set("Rotate", Object::Integer(-90));
        let bounds = bounds_of(&document, page).unwrap();
        assert_eq!(
            bounds,
            Bounds {
                x: 10.0,
                y: 20.0,
                width: 600.0,
                height: 800.0,
                rotate: 270
            }
        );
        assert_eq!(bounds.visual(), (800.0, 600.0));
    }

    fn stamped(inherit: bool) -> (Document, Vec<Stamp>) {
        let mut document = Document::load_mem(&pdf(&[A4, A4], inherit)).unwrap();
        let t = templates(NUMBERED, NONE);
        let Plan { jobs, stamps } = plan(&document, MARGINS, &t, &[2..=2], &context()).unwrap();
        assert_eq!(
            (jobs.len(), jobs[0].header, jobs[0].footer),
            (1, true, false)
        );
        let strips = [Strips {
            header: Some(pdf(&[[0, 0, 800, 96]], false)),
            footer: None,
        }];
        apply(&mut document, &stamps, &strips).unwrap();
        let mut saved = Vec::new();
        document.save_to(&mut saved).unwrap();
        (Document::load_mem(&saved).unwrap(), stamps)
    }

    #[test]
    fn a_strip_becomes_a_form_drawn_after_the_pages_own_content() {
        let (document, _) = stamped(false);
        let pages = document.get_pages();
        let untouched = document.get_dictionary(pages[&1]).unwrap();
        assert!(matches!(
            untouched.get(b"Contents"),
            Ok(Object::Reference(_))
        ));
        let page = document.get_dictionary(pages[&2]).unwrap();
        let contents = page.get(b"Contents").unwrap().as_array().unwrap();
        assert_eq!(contents.len(), 3);
        let content = String::from_utf8(document.get_page_content(pages[&2]).unwrap()).unwrap();
        let drawn = "Q\nq 0.750000 0.000000 0.000000 0.750000 0.000000 728.000000 cm \
                     /SeuOfficeHF0h Do Q";
        let (own, strip) = (
            content.find("(page 1) Tj").unwrap(),
            content.find(drawn).unwrap(),
        );
        assert!(content.starts_with("q\n") && own < strip, "{content}");
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(
            resources.get(b"Font").is_ok(),
            "the page keeps its own font"
        );
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let form = xobjects
            .get(b"SeuOfficeHF0h")
            .unwrap()
            .as_reference()
            .unwrap();
        let form = document.get_object(form);
        let form = form.unwrap().as_stream().unwrap();
        assert_eq!(
            form.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form"
        );
        let bbox = form.dict.get(b"BBox").unwrap().as_array().unwrap();
        let bbox: Vec<f32> = bbox.iter().map(|n| n.as_float().unwrap()).collect();
        assert_eq!(bbox, [0.0, 0.0, 800.0, 96.0]);
        let text = String::from_utf8(form.decompressed_content().unwrap()).unwrap();
        assert!(text.contains("(page 0) Tj"), "{text}");
        let fonts = form.dict.get(b"Resources").unwrap().as_dict().unwrap();
        let font = fonts
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"F1")
            .unwrap();
        let font = document.dereference(font).unwrap().1.as_dict().unwrap();
        assert_eq!(
            font.get(b"BaseFont").unwrap().as_name().unwrap(),
            b"Helvetica"
        );
    }

    #[test]
    fn a_page_with_inherited_resources_keeps_them_and_leaves_its_siblings_alone() {
        let (document, _) = stamped(true);
        let pages = document.get_pages();
        let sibling = document.get_dictionary(pages[&1]).unwrap();
        assert!(!sibling.has(b"Resources"));
        let page = document.get_dictionary(pages[&2]).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(resources.get(b"Font").is_ok());
        assert!(
            resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .has(b"SeuOfficeHF0h")
        );
    }

    #[test]
    fn a_strip_that_was_asked_for_and_not_rendered_is_an_error() {
        let mut document = Document::load_mem(&pdf(&[A4], false)).unwrap();
        let t = templates(NUMBERED, NONE);
        let Plan { stamps, .. } = plan(&document, MARGINS, &t, &[], &context()).unwrap();
        let result = apply(&mut document, &stamps, &[Strips::default()]);
        assert!(matches!(result, Err(PageError::Platform(_))));
    }
}
