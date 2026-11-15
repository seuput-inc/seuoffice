use super::{
    Stop,
    odf::{color, length},
    xml::{self, XElement, XNode},
};
use pptx_engine::types::TextAlign;
use std::collections::{HashMap, HashSet};

pub(super) const FAMILIES: &[&str] = &["presentation", "graphic", "paragraph", "text"];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Caps {
    All,
    Small,
    Reset,
}
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct TextProps {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub size: Option<f64>,
    pub color: Option<String>,
    pub font: Option<String>,
    pub vertical: Option<&'static str>,
    pub caps: Option<Caps>,
    pub shading: Option<String>,
}
impl TextProps {
    pub fn overlay(&mut self, source: &Self) {
        macro_rules! copy { ($($field:ident),+ $(,)?) => { $(if source.$field.is_some() { self.$field = source.$field; })+ }; }
        copy!(bold, italic, underline, strike, size, vertical, caps);
        for (target, value) in [
            (&mut self.color, &source.color),
            (&mut self.font, &source.font),
            (&mut self.shading, &source.shading),
        ] {
            if let Some(value) = value {
                *target = Some(value.clone());
            }
        }
    }
    pub fn run_format(&self) -> Self {
        let mut out = self.clone();
        for value in [
            &mut out.bold,
            &mut out.italic,
            &mut out.underline,
            &mut out.strike,
        ] {
            if *value == Some(false) {
                *value = None;
            }
        }
        for value in [&mut out.color, &mut out.font, &mut out.shading] {
            if value.as_deref() == Some("") {
                *value = None;
            }
        }
        if out.caps == Some(Caps::Reset) {
            out.caps = None;
        }
        out
    }
}
#[derive(Debug, Default, Clone)]
pub(super) struct ParaProps {
    pub align: Option<TextAlign>,
    pub before: Option<f64>,
    pub after: Option<f64>,
}
impl ParaProps {
    fn overlay(&mut self, source: &Self) {
        if let Some(value) = source.align {
            self.align = Some(value);
        }
        if let Some(value) = source.before {
            self.before = Some(value);
        }
        if let Some(value) = source.after {
            self.after = Some(value);
        }
    }
}
struct Style {
    parent: Option<String>,
    text: TextProps,
    para: ParaProps,
}
#[derive(Default)]
pub(super) struct Styles {
    defs: HashMap<String, HashMap<String, Style>>,
    fonts: HashMap<String, String>,
    text: TextProps,
    para: ParaProps,
}
// Property readers use the literal qualified keys, just like nodeAttrs in the
// source. Only references and geometry use ODF's local-name attribute fallback.
fn raw<'a>(node: &'a XElement, key: &str) -> Option<&'a str> {
    node.attrs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}
impl Styles {
    pub fn new(
        content: &[XNode],
        named: Option<&[XNode]>,
        stop: &Stop<'_>,
    ) -> Result<Self, String> {
        let mut out = Self::default();
        for node in named.unwrap_or(&[]).iter().chain(content) {
            if let XNode::Element(element) = node {
                out.collect(element, stop)?;
            }
        }
        Ok(out)
    }
    fn collect(&mut self, node: &XElement, stop: &Stop<'_>) -> Result<(), String> {
        stop.check()?;
        match node.name.as_str() {
            "style:style" => {
                if let (Some(name), Some(family)) = (
                    xml::attr(node, "style:name").filter(|value| !value.is_empty()),
                    xml::attr(node, "style:family").filter(|value| !value.is_empty()),
                ) {
                    if !matches!(family, "table-column" | "table-cell") {
                        let (text, para) = self.props(node);
                        if !self.defs.contains_key(family) {
                            self.defs.insert(family.to_owned(), HashMap::new());
                        }
                        self.defs
                            .get_mut(family)
                            .expect("style family inserted")
                            .insert(
                                name.to_owned(),
                                Style {
                                    parent: xml::attr(node, "style:parent-style-name")
                                        .map(str::to_owned),
                                    text,
                                    para,
                                },
                            );
                    }
                }
            }
            "style:default-style" => {
                let family = xml::attr(node, "style:family");
                if matches!(family, Some("paragraph" | "text")) {
                    let (text, para) = self.props(node);
                    self.text.overlay(&text);
                    if family == Some("paragraph") {
                        self.para.overlay(&para);
                    }
                }
            }
            "style:font-face" => {
                if let (Some(name), Some(family)) = (
                    xml::attr(node, "style:name").filter(|value| !value.is_empty()),
                    xml::attr(node, "svg:font-family").filter(|value| !value.is_empty()),
                ) {
                    let family = family.strip_prefix(['\'', '"']).unwrap_or(family);
                    let family = family.strip_suffix(['\'', '"']).unwrap_or(family);
                    self.fonts.insert(name.to_owned(), family.to_owned());
                }
            }
            _ => {}
        }
        for child in xml::elements(node) {
            self.collect(child, stop)?;
        }
        Ok(())
    }
    fn props(&self, node: &XElement) -> (TextProps, ParaProps) {
        let mut text = TextProps::default();
        let mut para = ParaProps::default();
        for child in xml::elements(node) {
            match child.name.as_str() {
                "style:text-properties" => text = self.text_props(child),
                "style:paragraph-properties" => {
                    para = ParaProps {
                        align: match raw(child, "fo:text-align") {
                            Some("start" | "left") => Some(TextAlign::Left),
                            Some("center") => Some(TextAlign::Center),
                            Some("end" | "right") => Some(TextAlign::Right),
                            Some("justify") => Some(TextAlign::Justify),
                            _ => None,
                        },
                        before: length(raw(child, "fo:margin-top"))
                            .map(|value| jscompat::math_round(value / 635.0)),
                        after: length(raw(child, "fo:margin-bottom"))
                            .map(|value| jscompat::math_round(value / 635.0)),
                    };
                }
                _ => {}
            }
        }
        (text, para)
    }
    fn text_props(&self, node: &XElement) -> TextProps {
        let underline = raw(node, "style:text-underline-type")
            .or_else(|| raw(node, "style:text-underline-style"));
        let strike = raw(node, "style:text-line-through-type")
            .or_else(|| raw(node, "fo:text-line-through-type"))
            .or_else(|| raw(node, "style:text-line-through-style"));
        let position = raw(node, "style:text-position").map(jscompat::trim);
        let mut caps =
            match raw(node, "style:text-transform").or_else(|| raw(node, "fo:text-transform")) {
                Some("uppercase") => Some(Caps::All),
                Some("lowercase" | "capitalize") => Some(Caps::Reset),
                _ => None,
            };
        if raw(node, "fo:font-variant") == Some("small-caps") {
            caps = Some(Caps::Small);
        }
        TextProps {
            bold: (raw(node, "fo:font-weight") == Some("bold")).then_some(true),
            italic: (raw(node, "fo:font-style") == Some("italic")).then_some(true),
            underline: underline.map(|value| value != "none"),
            strike: strike.map(|value| value != "none"),
            size: length(raw(node, "fo:font-size"))
                .filter(|value| *value > 0.0)
                .map(|value| jscompat::math_round(value / 12_700.0 * 10.0) / 10.0),
            color: color(raw(node, "fo:color")),
            shading: color(raw(node, "fo:background-color")),
            font: raw(node, "style:font-name")
                .filter(|value| !value.is_empty())
                .map(|value| {
                    self.fonts
                        .get(value)
                        .cloned()
                        .unwrap_or_else(|| value.to_owned())
                }),
            vertical: position.and_then(|value| {
                if jscompat::js_regex!(r"^super", "i").test(value) {
                    Some("superscript")
                } else if jscompat::js_regex!(r"^sub", "i").test(value) {
                    Some("subscript")
                } else {
                    None
                }
            }),
            caps,
        }
    }
    pub fn resolve(&self, name: Option<&str>, families: &[&str]) -> (TextProps, ParaProps) {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = name;
        while let Some(name) = current.filter(|value| !value.is_empty()) {
            if !seen.insert(name) {
                break;
            }
            let Some(def) = families.iter().find_map(|family| {
                self.defs
                    .get(*family)
                    .and_then(|definitions| definitions.get(name))
            }) else {
                break;
            };
            chain.push(def);
            current = def.parent.as_deref();
        }
        let (mut text, mut para) = (self.text.clone(), self.para.clone());
        for style in chain.into_iter().rev() {
            text.overlay(&style.text);
            para.overlay(&style.para);
        }
        (text, para)
    }
    pub fn size(roots: Option<&[XNode]>) -> Option<(f64, f64)> {
        fn master(node: &XElement) -> Option<&str> {
            if node.name == "style:master-page" {
                return xml::attr(node, "style:page-layout-name").filter(|value| !value.is_empty());
            }
            xml::elements(node).find_map(master)
        }
        fn layout<'a>(node: &'a XElement, name: &str) -> Option<&'a XElement> {
            if node.name == "style:page-layout" && xml::attr(node, "style:name") == Some(name) {
                return xml::first(node, "style:page-layout-properties");
            }
            xml::elements(node).find_map(|child| layout(child, name))
        }
        let roots = roots?;
        let name = roots.iter().find_map(|node| match node {
            XNode::Element(element) => master(element),
            _ => None,
        })?;
        let props = roots.iter().find_map(|node| match node {
            XNode::Element(element) => layout(element, name),
            _ => None,
        })?;
        let width = length(raw(props, "fo:page-width"))?;
        let height = length(raw(props, "fo:page-height"))?;
        (width > 0.0 && height > 0.0).then_some((width, height))
    }
}
