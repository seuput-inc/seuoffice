//! Excel's storage spelling for future functions and dynamic-array formulas.

const XLFN: &[&str] = &[
    "AGGREGATE", "BETA.DIST", "BETA.INV", "BINOM.DIST", "BINOM.INV", "CEILING.PRECISE",
    "CHISQ.DIST", "CHISQ.DIST.RT", "CHISQ.INV", "CHISQ.INV.RT", "CHISQ.TEST",
    "CONFIDENCE.NORM", "CONFIDENCE.T", "COVARIANCE.P", "COVARIANCE.S", "ERF.PRECISE",
    "ERFC.PRECISE", "EXPON.DIST", "F.DIST", "F.DIST.RT", "F.INV", "F.INV.RT", "F.TEST",
    "FLOOR.PRECISE", "GAMMA.DIST", "GAMMA.INV", "GAMMALN.PRECISE", "HYPGEOM.DIST",
    "LOGNORM.DIST", "LOGNORM.INV", "MODE.MULT", "MODE.SNGL", "NEGBINOM.DIST",
    "NETWORKDAYS.INTL", "NORM.DIST", "NORM.INV", "NORM.S.DIST", "NORM.S.INV",
    "PERCENTILE.EXC", "PERCENTILE.INC", "PERCENTRANK.EXC", "PERCENTRANK.INC",
    "POISSON.DIST", "QUARTILE.EXC", "QUARTILE.INC", "RANK.AVG", "RANK.EQ", "STDEV.P",
    "STDEV.S", "T.DIST", "T.DIST.2T", "T.DIST.RT", "T.INV", "T.INV.2T", "T.TEST",
    "VAR.P", "VAR.S", "WEIBULL.DIST", "WORKDAY.INTL", "Z.TEST", "ACOT", "ACOTH",
    "ARABIC", "BASE", "BINOM.DIST.RANGE", "BITAND", "BITLSHIFT", "BITOR", "BITRSHIFT",
    "BITXOR", "CEILING.MATH", "COMBINA", "COT", "COTH", "CSC", "CSCH", "DAYS",
    "DECIMAL", "ENCODEURL", "FILTERXML", "FLOOR.MATH", "FORMULATEXT", "GAMMA", "GAUSS",
    "IFNA", "IMCOSH", "IMCOT", "IMCSC", "IMCSCH", "IMSEC", "IMSECH", "IMSINH", "IMTAN",
    "ISFORMULA", "ISOWEEKNUM", "MUNIT", "NUMBERVALUE", "PDURATION", "PERMUTATIONA", "PHI",
    "RRI", "SEC", "SECH", "SHEET", "SHEETS", "SKEW.P", "UNICHAR", "UNICODE", "WEBSERVICE",
    "XOR", "CONCAT", "FORECAST.ETS", "FORECAST.ETS.CONFINT", "FORECAST.ETS.SEASONALITY",
    "FORECAST.ETS.STAT", "FORECAST.LINEAR", "IFS", "MAXIFS", "MINIFS", "SWITCH", "TEXTJOIN",
    "BYCOL", "BYROW", "CHOOSECOLS", "CHOOSEROWS", "DROP", "EXPAND", "HSTACK", "IMAGE",
    "ISOMITTED", "LAMBDA", "LET", "MAKEARRAY", "MAP", "RANDARRAY", "REDUCE", "SCAN",
    "SEQUENCE", "SORTBY", "STOCKHISTORY", "TAKE", "TEXTAFTER", "TEXTBEFORE", "TEXTSPLIT",
    "TOCOL", "TOROW", "UNIQUE", "VSTACK", "WRAPCOLS", "WRAPROWS", "XLOOKUP", "XMATCH",
    "SINGLE", "ANCHORARRAY",
];
const SPILL: &[&str] = &[
    "BYCOL", "BYROW", "CHOOSECOLS", "CHOOSEROWS", "DROP", "EXPAND", "FILTER", "HSTACK",
    "MAKEARRAY", "MAP", "RANDARRAY", "SCAN", "SEQUENCE", "SORT", "SORTBY", "TAKE",
    "TEXTSPLIT", "TOCOL", "TOROW", "UNIQUE", "VSTACK", "WRAPCOLS", "WRAPROWS",
];

struct Call<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    marked: bool,
}

fn name_char(c: char) -> bool {
    matches!(c, '_' | '.' | '\\' | '$') || c.is_alphanumeric()
}

fn calls(formula: &str) -> Vec<Call<'_>> {
    let mut result = Vec::new();
    let mut cursor = 0;
    while cursor < formula.len() {
        let ch = formula[cursor..].chars().next().expect("character boundary");
        if ch == '\'' || ch == '"' {
            cursor += ch.len_utf8();
            while cursor < formula.len() {
                let next = formula[cursor..].chars().next().expect("character boundary");
                cursor += next.len_utf8();
                if next == ch {
                    if formula[cursor..].starts_with(ch) { cursor += ch.len_utf8(); } else { break; }
                }
            }
            continue;
        }
        if !name_char(ch) { cursor += ch.len_utf8(); continue; }
        let start = cursor;
        cursor += ch.len_utf8();
        while cursor < formula.len() {
            let next = formula[cursor..].chars().next().expect("character boundary");
            if !name_char(next) { break; }
            cursor += next.len_utf8();
        }
        let end = cursor;
        if !formula[end..].trim_start_matches(char::is_whitespace).starts_with('(') { continue; }
        let token = &formula[start..end];
        let mut name = token;
        let mut marked = false;
        if name.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("_xlfn.")) {
            name = &name[6..];
            marked = true;
        }
        if name.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("_xlws.")) {
            name = &name[6..];
            marked = true;
        }
        result.push(Call { start, end, name, marked });
    }
    result
}

pub(super) fn spills_dynamic_array(formula: &str) -> bool {
    calls(formula).iter().any(|call| SPILL.contains(&call.name.to_uppercase().as_str()))
}

pub(super) fn with_future_function_markers(formula: &str) -> String {
    let mut out = String::with_capacity(formula.len());
    let mut cursor = 0;
    for call in calls(formula) {
        out.push_str(&formula[cursor..call.start]);
        let canonical = call.name.to_uppercase();
        if call.marked {
            out.push_str(&formula[call.start..call.end]);
        } else if matches!(canonical.as_str(), "FILTER" | "SORT") {
            out.push_str("_xlfn._xlws.");
            out.push_str(&canonical);
        } else if XLFN.contains(&canonical.as_str()) {
            out.push_str("_xlfn.");
            out.push_str(&canonical);
        } else {
            out.push_str(&formula[call.start..call.end]);
        }
        cursor = call.end;
    }
    out.push_str(&formula[cursor..]);
    out
}

const METADATA_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sheetMetadata";
const METADATA_XML: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<metadata xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:xda=\"http://schemas.microsoft.com/office/spreadsheetml/2017/dynamicarray\">",
    "<metadataTypes count=\"1\"><metadataType name=\"XLDAPR\" minSupportedVersion=\"120000\" copy=\"1\" pasteAll=\"1\" pasteValues=\"1\" merge=\"1\" splitFirst=\"1\" rowColShift=\"1\" clearFormats=\"1\" clearComments=\"1\" assign=\"1\" coerce=\"1\" cellMeta=\"1\"/></metadataTypes>",
    "<futureMetadata name=\"XLDAPR\" count=\"1\"><bk><extLst><ext uri=\"{bdbb8cdc-fa1e-496e-a857-3c3f30c029c3}\"><xda:dynamicArrayProperties fDynamic=\"1\" fCollapsed=\"0\"/></ext></extLst></bk></futureMetadata>",
    "<cellMetadata count=\"1\"><bk><rc t=\"1\" v=\"0\"/></bk></cellMetadata></metadata>"
);

pub(super) fn dynamic_array_cell_meta_index(xml: &str) -> Option<usize> {
    let root = super::style_entries::children(xml).into_iter().next()?;
    let sections = super::style_entries::children(root.inner);
    let types = sections.iter().find(|e| e.name == "metadataTypes")?;
    let type_id = super::style_entries::children(types.inner).into_iter()
        .filter(|e| e.name == "metadataType")
        .position(|e| super::styles::attr(e.xml, "name") == Some("XLDAPR"))? + 1;
    let records = sections.iter().find(|e| e.name == "cellMetadata")?;
    super::style_entries::children(records.inner).into_iter().filter(|e| e.name == "bk")
        .position(|record| super::style_entries::children(record.inner).iter()
            .any(|rc| rc.name == "rc" && super::styles::attr(rc.xml, "t").and_then(|v| v.parse::<usize>().ok()) == Some(type_id)))
        .map(|index| index + 1)
}

pub(super) fn ensure_dynamic_array_metadata(pkg: &mut super::package::PackageEditor<'_>, touched: &mut std::collections::HashSet<String>) -> Result<Option<usize>, String> {
    const PATH: &str = "xl/metadata.xml";
    if pkg.has(PATH)? { return Ok(dynamic_array_cell_meta_index(&pkg.read_text(PATH)?)); }
    pkg.add(PATH, METADATA_XML.into());
    touched.insert(PATH.into());
    let rel_path = "xl/_rels/workbook.xml.rels";
    let rels = pkg.read_text(rel_path)?;
    if !rels.contains(&format!("Type=\"{METADATA_REL}\"")) {
        let id = super::sheets::next_free_relationship_id(&rels);
        let entry = format!("<Relationship Id=\"{id}\" Type=\"{METADATA_REL}\" Target=\"metadata.xml\"/>");
        pkg.write(rel_path, rels.replace("</Relationships>", &format!("{entry}</Relationships>")))?;
        touched.insert(rel_path.into());
    }
    let types_path = "[Content_Types].xml";
    let types = pkg.read_text(types_path)?;
    if !types.contains("PartName=\"/xl/metadata.xml\"") {
        let entry = "<Override PartName=\"/xl/metadata.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml\"/>";
        pkg.write(types_path, types.replace("</Types>", &format!("{entry}</Types>")))?;
        touched.insert(types_path.into());
    }
    Ok(Some(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_ignore_quoted_text_and_preserve_existing_markers() {
        assert_eq!(with_future_function_markers("IF(A1=\"filter(\",sort (A1:A3),_xlfn.IFS(A1,1))"),
                   "IF(A1=\"filter(\",_xlfn._xlws.SORT (A1:A3),_xlfn.IFS(A1,1))");
        assert_eq!(with_future_function_markers("'FILTER('!A1+Budget_xlfn.IFS(A1)"),
                   "'FILTER('!A1+Budget_xlfn.IFS(A1)");
    }

    #[test]
    fn dynamic_arrays_are_detected_only_as_calls() {
        assert!(spills_dynamic_array("SUM(_xlfn._xlws.FILTER(A1:A5,B1:B5))"));
        assert!(!spills_dynamic_array("IF(A1=\"SEQUENCE(\",1,0)+SEQUENCE_NAME"));
    }
}
