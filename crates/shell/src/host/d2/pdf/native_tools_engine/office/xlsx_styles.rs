//! Deduplicated workbook styles for extracted spreadsheet cells.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Font {
    pub bold: bool,
    pub italic: bool,
    pub color: Option<String>,
    pub size_pt: Option<f64>,
    pub name: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Edges {
    pub top: Option<String>,
    pub right: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Alignment {
    pub vertical: Option<String>,
    pub horizontal: Option<String>,
    pub wrap: bool,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Style {
    pub number_format: Option<String>,
    pub font: Option<Font>,
    pub fill: Option<String>,
    pub border: Option<Edges>,
    pub align: Option<Alignment>,
}
fn intern<T: PartialEq>(items: &mut Vec<T>, value: T) -> usize {
    if let Some(i) = items.iter().position(|v| *v == value) {
        i
    } else {
        let i = items.len();
        items.push(value);
        i
    }
}
pub(super) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
#[derive(Debug)]
pub(super) struct StylePool {
    formats: Vec<String>,
    fonts: Vec<Font>,
    fills: Vec<String>,
    borders: Vec<Edges>,
    xfs: Vec<String>,
}
impl Default for StylePool {
    fn default() -> Self {
        let mut pool = Self {
            formats: Vec::new(),
            fonts: vec![Font::default()],
            fills: vec!["none".into(), "gray125".into()],
            borders: vec![Edges::default()],
            xfs: Vec::new(),
        };
        pool.cell_xf(&Style::default());
        pool
    }
}
impl StylePool {
    pub fn cell_xf(&mut self, spec: &Style) -> usize {
        let code = spec.number_format.as_deref().unwrap_or("General");
        let format = match code {
            "General" => 0,
            "#,##0" => 3,
            "#,##0.00" => 4,
            "0%" => 9,
            "0.00%" => 10,
            _ => 164 + intern(&mut self.formats, code.to_owned()),
        };
        let font = intern(&mut self.fonts, spec.font.clone().unwrap_or_default());
        let fill = intern(
            &mut self.fills,
            spec.fill.clone().unwrap_or_else(|| "none".into()),
        );
        let border = intern(&mut self.borders, spec.border.clone().unwrap_or_default());
        let mut alignment = String::new();
        if let Some(a) = &spec.align {
            if let Some(v) = &a.horizontal {
                alignment.push_str(&format!(" horizontal=\"{}\"", escape(v)));
            }
            if let Some(v) = &a.vertical {
                alignment.push_str(&format!(" vertical=\"{}\"", escape(v)));
            }
            if a.wrap {
                alignment.push_str(" wrapText=\"1\"");
            }
        }
        let mut attrs = format!(
            "numFmtId=\"{format}\" fontId=\"{font}\" fillId=\"{fill}\" borderId=\"{border}\" xfId=\"0\""
        );
        if format != 0 {
            attrs.push_str(" applyNumberFormat=\"1\"");
        }
        if font != 0 {
            attrs.push_str(" applyFont=\"1\"");
        }
        if fill > 1 {
            attrs.push_str(" applyFill=\"1\"");
        }
        if border != 0 {
            attrs.push_str(" applyBorder=\"1\"");
        }
        if !alignment.is_empty() {
            attrs.push_str(" applyAlignment=\"1\"");
        }
        let entry = if alignment.is_empty() {
            format!("<xf {attrs}/>")
        } else {
            format!("<xf {attrs}><alignment{alignment}/></xf>")
        };
        intern(&mut self.xfs, entry)
    }
    pub fn xml(&self) -> String {
        let formats = if self.formats.is_empty() {
            String::new()
        } else {
            format!(
                "<numFmts count=\"{}\">{}</numFmts>",
                self.formats.len(),
                self.formats
                    .iter()
                    .enumerate()
                    .map(|(i, v)| format!(
                        "<numFmt numFmtId=\"{}\" formatCode=\"{}\"/>",
                        164 + i,
                        escape(v)
                    ))
                    .collect::<String>()
            )
        };
        let fonts = self
            .fonts
            .iter()
            .map(|f| {
                let mut s = String::from("<font>");
                if f.bold {
                    s.push_str("<b/>");
                }
                if f.italic {
                    s.push_str("<i/>");
                }
                s.push_str(&format!("<sz val=\"{}\"/>", f.size_pt.unwrap_or(11.0)));
                if let Some(c) = f
                    .color
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| c.len() == 6 && c.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    s.push_str(&format!("<color rgb=\"FF{}\"/>", c.to_ascii_uppercase()));
                }
                s.push_str(&format!(
                    "<name val=\"{}\"/></font>",
                    escape(f.name.as_deref().unwrap_or("Calibri"))
                ));
                s
            })
            .collect::<String>();
        let fills=self.fills.iter().map(|v|match v.as_str() {"none"=>"<fill><patternFill patternType=\"none\"/></fill>".into(),"gray125"=>"<fill><patternFill patternType=\"gray125\"/></fill>".into(),_=>format!("<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF{}\"/><bgColor rgb=\"FF{}\"/></patternFill></fill>",v.to_ascii_uppercase(),v.to_ascii_uppercase())}).collect::<String>();
        let borders = self
            .borders
            .iter()
            .map(|v| {
                let edge =
                    |name: &str, c: &Option<String>| match c.as_ref().filter(|s| !s.is_empty()) {
                        Some(c) => format!(
                            "<{name} style=\"thin\"><color rgb=\"FF{}\"/></{name}>",
                            c.to_ascii_uppercase()
                        ),
                        None => format!("<{name}/>"),
                    };
                format!(
                    "<border>{}{}{}{}<diagonal/></border>",
                    edge("left", &v.left),
                    edge("right", &v.right),
                    edge("top", &v.top),
                    edge("bottom", &v.bottom)
                )
            })
            .collect::<String>();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{formats}<fonts count=\"{}\">{fonts}</fonts><fills count=\"{}\">{fills}</fills><borders count=\"{}\">{borders}</borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"{}\">{}</cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>",
            self.fonts.len(),
            self.fills.len(),
            self.borders.len(),
            self.xfs.len(),
            self.xfs.concat()
        )
    }
}
