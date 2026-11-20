use super::{
    Converted, Stop,
    odf::{self, Odf},
    styles::{FAMILIES, Styles, TextProps},
    xml::{self, XElement, XNode},
};
use jscompat::{JsNum, Slot};
use pptx_engine::{
    blank::create_blank_pptx,
    deck::{self, OpenedPptx},
    insert::{self, NewElementOptions, NewPictureOptions},
    placeholder::BulletType,
    types::{EmuRect, Paragraph, ParagraphBullet, TextRun},
};
use std::collections::BTreeMap;

#[derive(Default)]
struct Drops(BTreeMap<String, usize>);
impl Drops {
    fn add(&mut self, name: &str) {
        *self.0.entry(name.to_owned()).or_default() += 1;
    }
}
struct Context<'a, 'b> {
    styles: &'a Styles,
    drops: Drops,
    images: usize,
    stop: &'a Stop<'b>,
}
#[derive(Clone, PartialEq)]
struct Run {
    text: String,
    format: TextProps,
    href: Option<String>,
}
enum Image {
    Inline(String),
    File(String),
}
struct PendingImage {
    href: String,
    bounds: EmuRect,
    slide: usize,
}
struct ReadyImage<'a, 'b> {
    pending: PendingImage,
    entry: &'a ooxml_pkg::jszip::SelectiveEntry<'b>,
    order: usize,
}
fn image_ref(frame: &XElement) -> Option<Image> {
    let image = xml::descendant(frame, "draw:image")?;
    let mut base64 = None;
    for child in xml::elements(image).filter(|child| child.name == "office:binary-data") {
        base64 = Some(
            xml::text_of(child)
                .chars()
                .filter(|character| !jscompat::is_js_space(*character))
                .collect::<String>(),
        );
    }
    if let Some(encoded) = base64.filter(|value| !value.is_empty()) {
        return Some(Image::Inline(encoded));
    }
    xml::attr(image, "xlink:href")
        .filter(|value| !value.is_empty())
        .map(|href| Image::File(href.to_owned()))
}
fn runs(node: &XElement, ctx: &mut Context<'_, '_>) -> Result<Vec<Run>, String> {
    let inherited = ctx
        .styles
        .resolve(
            xml::attr(node, "text:style-name"),
            &["paragraph", "presentation", "graphic", "paragraph", "text"],
        )
        .0;
    let mut out = Vec::new();
    walk(&node.children, &inherited, None, ctx, &mut out)?;
    Ok(out)
}
fn push(out: &mut Vec<Run>, text: String, props: &TextProps, href: Option<&str>) {
    if text.is_empty() {
        return;
    }
    let format = props.run_format();
    let href = href.filter(|value| !value.is_empty());
    if let Some(previous) = out
        .last_mut()
        .filter(|previous| previous.format == format && previous.href.as_deref() == href)
    {
        previous.text.push_str(&text);
    } else {
        out.push(Run {
            text,
            format,
            href: href.map(str::to_owned),
        });
    }
}
fn walk(
    nodes: &[XNode],
    inherited: &TextProps,
    href: Option<&str>,
    ctx: &mut Context<'_, '_>,
    out: &mut Vec<Run>,
) -> Result<(), String> {
    for node in nodes {
        ctx.stop.check()?;
        let element = match node {
            XNode::Text(value) => {
                push(out, value.clone(), inherited, href);
                continue;
            }
            XNode::Element(element) => element,
        };
        match element.name.as_str() {
            "text:span" => {
                let mut props = inherited.clone();
                props.overlay(
                    &ctx.styles
                        .resolve(xml::attr(element, "text:style-name"), FAMILIES)
                        .0,
                );
                walk(&element.children, &props, href, ctx, out)?;
            }
            "text:a" => {
                let target = xml::attr(element, "xlink:href").unwrap_or("");
                let target = if jscompat::js_regex!(r"^(https?|mailto|ftp):", "i").test(target) {
                    Some(target)
                } else {
                    href
                };
                walk(&element.children, inherited, target, ctx, out)?;
            }
            "text:tab" => push(out, "\t".to_owned(), inherited, href),
            "text:line-break" => push(out, "\n".to_owned(), inherited, href),
            "text:s" => {
                let value = jscompat::to_number(xml::attr(element, "text:c").unwrap_or("1"));
                let value = if value == 0.0 || value.is_nan() {
                    1.0
                } else {
                    value
                };
                let count = value.clamp(1.0, 1000.0) as usize;
                push(out, " ".repeat(count), inherited, href);
            }
            "text:soft-page-break"
            | "text:bookmark"
            | "text:bookmark-start"
            | "text:bookmark-end"
            | "text:reference-mark"
            | "text:reference-mark-start"
            | "text:reference-mark-end"
            | "text:sequence" => {}
            "text:note" => ctx.drops.add("footnotes"),
            "office:annotation" => ctx.drops.add("comments"),
            "draw:frame" => {
                if image_ref(element).is_some() {
                    ctx.images += 1;
                } else if xml::descendant(element, "draw:text-box").is_some() {
                    ctx.drops.add("textboxes-flattened");
                    walk(&element.children, inherited, href, ctx, out)?;
                } else {
                    ctx.drops.add("shapes");
                }
            }
            "draw:image" | "draw:object" | "math:math" => ctx.drops.add("objects"),
            "text:page-number" | "text:page-count" | "text:sequence-ref" | "text:bookmark-ref" => {
                push(out, xml::text_of(element), inherited, href)
            }
            _ => walk(&element.children, inherited, href, ctx, out)?,
        }
    }
    Ok(())
}
fn paragraph(
    node: &XElement,
    depth: usize,
    ctx: &mut Context<'_, '_>,
) -> Result<Paragraph, String> {
    let before = ctx.images;
    let runs = runs(node, ctx)?;
    if ctx.images > before {
        ctx.drops.add("images");
        ctx.images = before;
    }
    let mut para = Paragraph {
        runs: Slot::of(
            runs.into_iter()
                .map(|run| {
                    let mut native = TextRun {
                        text: Slot::of(run.text),
                        ..Default::default()
                    };
                    if run.format.bold == Some(true) {
                        native.bold = Slot::of(true);
                    }
                    if run.format.italic == Some(true) {
                        native.italic = Slot::of(true);
                    }
                    if run.format.underline == Some(true) {
                        native.underline = Slot::of(true);
                    }
                    if run.format.strike == Some(true) {
                        native.strike = Slot::of(true);
                    }
                    if let Some(value) = run.format.size {
                        native.font_size = Slot::of(JsNum(value));
                    }
                    if let Some(value) = run.format.font {
                        native.font_family = Slot::of(value);
                    }
                    if let Some(value) = run.format.color {
                        native.color = Slot::of(format!("#{value}"));
                    }
                    native
                })
                .collect(),
        ),
        ..Default::default()
    };
    let props = ctx
        .styles
        .resolve(xml::attr(node, "text:style-name"), FAMILIES)
        .1;
    if let Some(value) = props.align {
        para.align = Slot::of(value);
    }
    if let Some(value) = props.before {
        para.space_before = Slot::of(JsNum(value / 20.0));
    }
    if let Some(value) = props.after {
        para.space_after = Slot::of(JsNum(value / 20.0));
    }
    if depth > 0 {
        let bullet = ParagraphBullet {
            r#type: Slot::of(BulletType::Char),
            char: Slot::of("•".to_owned()),
            ..Default::default()
        };
        para.bullet = Slot::of(bullet);
        para.mar_l = Slot::of(JsNum(457_200.0 * depth as f64));
        para.indent = Slot::of(JsNum(-457_200.0));
    }
    Ok(para)
}
fn paragraphs(
    node: &XElement,
    depth: usize,
    ctx: &mut Context<'_, '_>,
    out: &mut Vec<Paragraph>,
) -> Result<(), String> {
    for child in xml::elements(node) {
        ctx.stop.check()?;
        match child.name.as_str() {
            "text:p" | "text:h" => out.push(paragraph(child, depth, ctx)?),
            "text:list" => paragraphs(child, depth + 1, ctx, out)?,
            "text:list-item" => {
                for inner in xml::elements(child) {
                    match inner.name.as_str() {
                        "text:p" | "text:h" => out.push(paragraph(inner, depth, ctx)?),
                        "text:list" => paragraphs(inner, depth + 1, ctx, out)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn bounds(frame: &XElement, size: (f64, f64)) -> Option<EmuRect> {
    let lengths =
        ["svg:x", "svg:y", "svg:width", "svg:height"].map(|key| odf::length(xml::attr(frame, key)));
    let (x, y, width, height) = if let [Some(x), Some(y), Some(width), Some(height)] = lengths {
        (x, y, width.max(1.0), height.max(1.0))
    } else {
        let (x, y, w, h) = match xml::attr(frame, "presentation:class")? {
            "title" => (0.08, 0.04, 0.84, 0.14),
            "subtitle" => (0.08, 0.22, 0.84, 0.1),
            "outline" | "text" => (0.08, 0.22, 0.84, 0.7),
            // The source indexes a plain object, including inherited keys.
            // Preserve its nonfinite box through the engine's own normalization.
            "constructor"
            | "__defineGetter__"
            | "__defineSetter__"
            | "hasOwnProperty"
            | "__lookupGetter__"
            | "__lookupSetter__"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "toString"
            | "valueOf"
            | "__proto__"
            | "toLocaleString" => (f64::NAN, f64::NAN, f64::NAN, f64::NAN),
            _ => return None,
        };
        (
            jscompat::math_round(x * size.0),
            jscompat::math_round(y * size.1),
            jscompat::math_round(w * size.0),
            jscompat::math_round(h * size.1),
        )
    };
    Some(EmuRect {
        x: JsNum(x),
        y: JsNum(y),
        cx: JsNum(width),
        cy: JsNum(height),
    })
}
fn place_image(
    opened: &mut OpenedPptx,
    slide: usize,
    bounds: EmuRect,
    bytes: Vec<u8>,
    path: &str,
    ctx: &mut Context<'_, '_>,
    now: f64,
) -> Result<(), String> {
    ctx.stop.check()?;
    if bytes.is_empty() {
        ctx.drops.add("images");
        return Ok(());
    }
    let Some(ext) = odf::image_ext(&bytes, path) else {
        ctx.drops.add("images");
        return Ok(());
    };
    insert::add_picture(
        opened,
        slide,
        &NewPictureOptions {
            bytes,
            ext: ext.to_owned(),
            offset: bounds,
            name: None,
            descr: None,
        },
        &jscompat::random_uuid(),
        now,
    )?;
    ctx.stop.check()
}
pub(super) fn convert(bytes: &[u8], stop: &Stop<'_>) -> Result<Converted, String> {
    let pack = Odf::load(bytes, stop)?;
    let presentation = pack.presentation().ok_or("not-a-presentation")?;
    let styles = Styles::new(&pack.content, pack.styles.as_deref(), stop)?;
    let size = Styles::size(pack.styles.as_deref()).unwrap_or((12_192_000.0, 6_858_000.0));
    let mut ctx = Context {
        styles: &styles,
        drops: Drops::default(),
        images: 0,
        stop,
    };
    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    };
    stop.check()?;
    let mut opened = deck::open_pptx(&create_blank_pptx(now)?)?;
    deck::set_slide_size(&mut opened, size.0, size.1)?;
    let mut slides = 0;
    let mut pending = Vec::new();
    for page in xml::elements(presentation).filter(|page| page.name == "draw:page") {
        stop.check()?;
        let index = if slides == 0 {
            0
        } else {
            let after = opened.deck.slides.len() as f64 - 1.0;
            let Some(index) = deck::insert_blank_slide(&mut opened, after)? else {
                break;
            };
            index
        };
        if opened.deck.slides.get(index).is_none() {
            break;
        }
        slides += 1;
        for child in xml::elements(page) {
            stop.check()?;
            match child.name.as_str() {
                "draw:frame" => {
                    if xml::attr(child, "presentation:class") == Some("notes") {
                        ctx.drops.add("notes");
                        continue;
                    }
                    let Some(bounds) = bounds(child, size) else {
                        ctx.drops.add("frames-without-geometry");
                        continue;
                    };
                    if let Some(textbox) = xml::descendant(child, "draw:text-box") {
                        let mut text = Vec::new();
                        paragraphs(textbox, 0, &mut ctx, &mut text)?;
                        if text.is_empty() {
                            let run = TextRun {
                                text: Slot::of(String::new()),
                                ..Default::default()
                            };
                            let para = Paragraph {
                                runs: Slot::of(vec![run]),
                                ..Default::default()
                            };
                            text.push(para);
                        }
                        insert::add_element(
                            &mut opened.deck.slides[index],
                            &NewElementOptions {
                                kind: "textbox".to_owned(),
                                offset: bounds,
                                paragraphs: Some(text),
                                fill_color: None,
                                stroke: None,
                                body_pr: None,
                                adjustments: None,
                                flip_h: None,
                                flip_v: None,
                            },
                            &jscompat::random_uuid(),
                            now,
                        )?;
                    } else if let Some(reference) = image_ref(child) {
                        match reference {
                            // This branch has no await in the source.
                            Image::Inline(encoded) => match jscompat::atob(&encoded) {
                                Ok(bytes) => place_image(
                                    &mut opened,
                                    index,
                                    bounds,
                                    bytes,
                                    "",
                                    &mut ctx,
                                    now,
                                )?,
                                Err(_) => ctx.drops.add("images"),
                            },
                            Image::File(href) => pending.push(PendingImage {
                                href,
                                bounds,
                                slide: index,
                            }),
                        }
                    } else if xml::descendant(child, "table:table").is_some() {
                        ctx.drops.add("tables");
                    } else {
                        ctx.drops.add("shapes");
                    }
                }
                "presentation:notes" => ctx.drops.add("notes"),
                "draw:rect"
                | "draw:line"
                | "draw:circle"
                | "draw:ellipse"
                | "draw:custom-shape"
                | "draw:connector"
                | "draw:control"
                | "draw:object"
                | "draw:page-thumbnail"
                | "math:math" => ctx.drops.add("shapes"),
                _ => {}
            }
        }
    }
    let mut ready = Vec::with_capacity(pending.len());
    for (order, pending) in pending.into_iter().enumerate() {
        stop.check()?;
        // URI and declared-size rejections happen before the source's first
        // asynchronous ZIP tick, including errors in a later frame.
        if let Some(entry) = pack.image_entry(&pending.href)? {
            ready.push(ReadyImage {
                pending,
                entry,
                order,
            });
        } else {
            ctx.drops.add("images");
        }
    }
    // Parallel JSZip workers consume equally sized input blocks each turn.
    // Preserve completion order, which can differ from document order, without
    // allocating an inflater and output buffer for every queued picture.
    ready.sort_unstable_by_key(|task| (task.entry.worker_input_chunks(), task.order));
    for task in ready {
        if let Some(bytes) = odf::image_bytes(task.entry, stop)? {
            place_image(
                &mut opened,
                task.pending.slide,
                task.pending.bounds,
                bytes,
                &task.pending.href,
                &mut ctx,
                now,
            )?;
        } else {
            ctx.drops.add("images");
        }
    }
    stop.check()?;
    let bytes = deck::save_pptx(&mut opened)?;
    stop.check()?;
    Ok(Converted {
        bytes,
        slides,
        warnings: Vec::new(),
        dropped: ctx.drops.0.into_iter().collect(),
    })
}
