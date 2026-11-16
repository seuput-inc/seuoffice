//! Worksheet cells, Excel limits, and OOXML serialization.
use super::xlsx_styles::escape;
use std::collections::BTreeMap;
const ROWS: usize = 1_048_576;
const COLS: usize = 16_384;
#[derive(Clone, Debug)]
pub(super) enum Value {
    Text(String),
    Number(f64),
}
#[derive(Clone, Debug)]
pub(super) struct Cell {
    pub row: usize,
    pub col: usize,
    pub value: Option<Value>,
    pub style: usize,
}
#[derive(Clone, Debug, Default)]
pub(super) struct PageSetup {
    pub paper_size: Option<u32>,
    pub orientation: Option<String>,
    pub first_page_number: Option<usize>,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Sheet {
    pub name: String,
    pub cells: Vec<Cell>,
    pub widths: Vec<Option<f64>>,
    pub heights: BTreeMap<usize, f64>,
    pub merges: Vec<String>,
    pub header: Option<String>,
    pub footer: Option<String>,
    pub setup: Option<PageSetup>,
}
pub(super) fn cell_ref(row: usize, mut col: usize) -> String {
    let mut letters = Vec::new();
    col += 1;
    while col > 0 {
        col -= 1;
        letters.push((b'A' + (col % 26) as u8) as char);
        col /= 26;
    }
    letters.reverse();
    format!("{}{}", letters.into_iter().collect::<String>(), row + 1)
}
fn truncate(s: &str, limit: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= limit
        })
        .collect()
}
fn normalize_text(s: &str) -> (String, bool, bool) {
    let t = truncate(s, 32_767);
    let truncated = t.len() != s.len();
    let mut count = 0;
    let value: String = t
        .chars()
        .filter(|c| {
            if *c == '\n' {
                count += 1;
                count <= 253
            } else {
                true
            }
        })
        .collect();
    let lines = value.len() != t.len();
    (value, truncated, lines)
}
fn address(s: &str) -> bool {
    let split = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let (letters, digits) = s.split_at(split);
    if letters.is_empty()
        || digits.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|v| v.is_ascii_digit())
    {
        return false;
    }
    let mut col = 0usize;
    for b in letters.bytes() {
        let Some(v) = col
            .checked_mul(26)
            .and_then(|v| v.checked_add((b.to_ascii_uppercase() - b'A' + 1) as usize))
        else {
            return false;
        };
        col = v;
    }
    let Ok(row) = digits.parse::<usize>() else {
        return false;
    };
    (1..=ROWS).contains(&row) && (1..=COLS).contains(&col)
}
fn valid_merge(s: &str) -> bool {
    let mut p = s.split(':');
    matches!((p.next(),p.next(),p.next()),(Some(a),Some(b),None) if address(a)&&address(b))
}
impl Sheet {
    pub fn normalize(&mut self) -> Vec<String> {
        let (mut dropped, mut truncated, mut lines) = (false, false, false);
        self.cells.retain_mut(|c| {
            if c.row >= ROWS
                || c.col >= COLS
                || matches!(&c.value,Some(Value::Number(v)) if !v.is_finite())
            {
                dropped = true;
                return false;
            }
            if let Some(Value::Text(s)) = &mut c.value {
                let (v, t, l) = normalize_text(s);
                *s = v;
                truncated |= t;
                lines |= l;
            }
            true
        });
        let mut widths = self.widths.len() > COLS;
        self.widths.truncate(COLS);
        for v in &mut self.widths {
            if let Some(n) = *v {
                if !n.is_finite() {
                    *v = None;
                    widths = true;
                } else {
                    let bounded = n.clamp(0.0, 255.0);
                    widths |= bounded != n;
                    *v = Some(bounded);
                }
            }
        }
        let (mut dropped_heights, mut adjusted_heights) = (false, false);
        self.heights.retain(|r, h| {
            if *r >= ROWS || !h.is_finite() {
                dropped_heights = true;
                return false;
            }
            let v = (*h).clamp(0.0, 409.0);
            adjusted_heights |= v != *h;
            *h = v;
            true
        });
        let mut merges = false;
        self.merges.retain(|v| {
            let ok = valid_merge(v);
            merges |= !ok;
            ok
        });
        let mut hf = false;
        for s in [&mut self.header, &mut self.footer].into_iter().flatten() {
            let t = truncate(s, 255);
            hf |= t.len() != s.len();
            *s = t;
        }
        let mut w = Vec::new();
        if dropped {
            w.push(
                "Excel limits: dropped cells outside the 1048576-row by 16384-column worksheet"
                    .into(),
            );
        }
        if truncated {
            w.push("Excel limits: truncated cell text to 32767 characters".into());
        }
        if lines {
            w.push("Excel limits: removed cell line feeds beyond 253".into());
        }
        if hf {
            w.push("Excel limits: truncated header/footer text to 255 characters".into());
        }
        if widths {
            w.push("Excel limits: clamped column widths to 255 characters".into());
        }
        if dropped_heights {
            w.push("Excel limits: dropped row heights outside the 1048576-row worksheet".into());
        }
        if adjusted_heights {
            w.push("Excel limits: clamped row heights to 409 points".into());
        }
        if merges {
            w.push(
                "Excel limits: dropped merges outside the 1048576-row by 16384-column worksheet"
                    .into(),
            );
        }
        w
    }
    pub fn xml(&self) -> String {
        let mut sheet = self.clone();
        sheet.normalize();
        let mut rows: BTreeMap<usize, Vec<&Cell>> = BTreeMap::new();
        let (mut max_row, mut max_col) = (0, 0);
        for c in &sheet.cells {
            max_row = max_row.max(c.row);
            max_col = max_col.max(c.col);
            rows.entry(c.row).or_default().push(c);
        }
        let mut columns = String::new();
        for (i, w) in sheet.widths.iter().enumerate() {
            if let Some(v) = w {
                max_col = max_col.max(i);
                columns.push_str(&format!(
                    "<col min=\"{}\" max=\"{}\" width=\"{}\" customWidth=\"1\"/>",
                    i + 1,
                    i + 1,
                    (v * 100.0).round() / 100.0
                ));
            }
        }
        if !columns.is_empty() {
            columns = format!("<cols>{columns}</cols>");
        }
        let mut row_xml = String::new();
        for (row, mut cells) in rows {
            cells.sort_by_key(|c| c.col);
            let height = sheet.heights.get(&row).map_or(String::new(), |v| {
                format!(" ht=\"{}\" customHeight=\"1\"", (v * 100.0).round() / 100.0)
            });
            row_xml.push_str(&format!("<row r=\"{}\"{height}>", row + 1));
            for cell in cells {
                let r = cell_ref(row, cell.col);
                let style = if cell.style == 0 {
                    String::new()
                } else {
                    format!(" s=\"{}\"", cell.style)
                };
                match &cell.value {
                    None => row_xml.push_str(&format!("<c r=\"{r}\"{style}/>")),
                    Some(Value::Number(v)) => {
                        row_xml.push_str(&format!("<c r=\"{r}\"{style}><v>{v}</v></c>"))
                    }
                    Some(Value::Text(v)) => {
                        let safe:String=v.chars().filter(|c|!matches!(*c,'\u{0}'..='\u{8}'|'\u{b}'|'\u{c}'|'\u{e}'..='\u{1f}')).collect();
                        row_xml.push_str(&format!("<c r=\"{r}\"{style} t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",escape(&safe)));
                    }
                }
            }
            row_xml.push_str("</row>");
        }
        let merges = if sheet.merges.is_empty() {
            String::new()
        } else {
            format!(
                "<mergeCells count=\"{}\">{}</mergeCells>",
                sheet.merges.len(),
                sheet
                    .merges
                    .iter()
                    .map(|v| format!("<mergeCell ref=\"{}\"/>", escape(v)))
                    .collect::<String>()
            )
        };
        let setup = sheet.setup.as_ref().map_or(String::new(), |p| {
            let mut a = String::new();
            if let Some(v) = p.paper_size {
                a.push_str(&format!(" paperSize=\"{v}\""));
            }
            if let Some(v) = &p.orientation {
                a.push_str(&format!(" orientation=\"{}\"", escape(v)));
            }
            if let Some(v) = p.first_page_number {
                a.push_str(&format!(
                    " firstPageNumber=\"{v}\" useFirstPageNumber=\"1\""
                ));
            }
            format!("<pageSetup{a}/>")
        });
        let hf = if sheet.header.is_none() && sheet.footer.is_none() {
            String::new()
        } else {
            format!(
                "<headerFooter>{}{}</headerFooter>",
                sheet.header.as_deref().filter(|s| !s.is_empty()).map_or(
                    String::new(),
                    |s| format!("<oddHeader>{}</oddHeader>", escape(s))
                ),
                sheet.footer.as_deref().filter(|s| !s.is_empty()).map_or(
                    String::new(),
                    |s| format!("<oddFooter>{}</oddFooter>", escape(s))
                )
            )
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:{}\"/>{columns}<sheetData>{row_xml}</sheetData>{merges}{setup}{hf}</worksheet>",
            cell_ref(max_row, max_col)
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_utf16_and_sheet_dimensions() {
        let mut s = Sheet::default();
        s.cells.push(Cell {
            row: 0,
            col: 0,
            value: Some(Value::Text(format!("{}😀", "a".repeat(32766)))),
            style: 0,
        });
        s.merges = vec!["A1:XFD1048576".into(), "A1:XFE1".into()];
        let warnings = s.normalize();
        assert_eq!(s.merges.len(), 1);
        assert_eq!(warnings.len(), 2);
        assert!(s.xml().contains("dimension ref=\"A1:A1\""));
    }
}
