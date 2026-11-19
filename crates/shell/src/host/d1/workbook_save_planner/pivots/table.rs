use super::{
    AxisLine, PivotFilter, build_axis_items, build_axis_pivot_field, build_pivot_filters,
    escape_attribute,
};
use std::collections::HashSet;

pub(super) struct PivotValue<'a> {
    pub field_index: usize,
    pub aggregation: &'a str,
    pub caption: Option<&'a str>,
    pub number_format: Option<&'a str>,
    pub show_data_as: Option<&'a str>,
    pub base_field: Option<usize>,
    pub base_item: Option<usize>,
    pub calculated_name: Option<&'a str>,
}

pub(super) struct PivotTable<'a> {
    pub name: &'a str,
    pub location: &'a str,
    pub field_names: &'a [String],
    pub row_fields: &'a [usize],
    pub row_items: &'a [Vec<String>],
    pub row_lines: &'a [AxisLine],
    pub row_hidden: &'a [Vec<usize>],
    pub column_fields: &'a [usize],
    pub column_items: &'a [Vec<String>],
    pub column_lines: &'a [AxisLine],
    pub column_hidden: &'a [Vec<usize>],
    pub page_fields: &'a [usize],
    pub page_items: &'a [Vec<String>],
    pub page_selected: &'a [Option<usize>],
    pub values: &'a [PivotValue<'a>],
    pub filters: &'a [PivotFilter<'a>],
    pub groupings: &'a [serde_json::Value],
    pub short_date: &'a str,
}

fn num_format(format: &str, short_date: &str) -> u32 {
    let format = format.trim();
    if format == short_date {
        return 14;
    }
    if format == format!("{short_date} hh:mm") || format == format!("{short_date} h:mm") {
        return 22;
    }
    match format {
        "0" => 1,
        "0.00" => 2,
        "#,##0" => 3,
        "#,##0.00" | "#,##0.00;(#,##0.00)" => 4,
        "0%" => 9,
        "0.00%" => 10,
        "0.00E+00" => 11,
        "# ?/?" => 12,
        "# ??/??" => 13,
        "mm-dd-yy" | "m/d/yyyy" => 14,
        "d-mmm-yy" => 15,
        "h:mm AM/PM" => 18,
        "h:mm:ss AM/PM" => 19,
        "h:mm" => 20,
        "h:mm:ss" => 21,
        "_(* #,##0.00_);_(* (#,##0.00);_(* \"-\"??_);_(@_)" => 164,
        _ => 0,
    }
}

fn data_field(
    value: &PivotValue<'_>,
    source_names: &[String],
    calc_index: usize,
    short_date: &str,
) -> String {
    let calculated = value.calculated_name.is_some();
    let field_name = value.calculated_name.unwrap_or_else(|| {
        source_names
            .get(value.field_index)
            .map_or("", String::as_str)
    });
    let field = if calculated {
        source_names.len() + calc_index
    } else {
        value.field_index
    };
    let prefix = match value.aggregation {
        "sum" => "Sum",
        "count" | "countNums" => "Count",
        "average" => "Average",
        "max" => "Max",
        "min" => "Min",
        "product" => "Product",
        "stdDev" => "StdDev",
        "stdDevp" => "StdDevp",
        "var" => "Var",
        "varp" => "Varp",
        other => other,
    };
    let default_caption = format!("{prefix} of {field_name}");
    let caption = value.caption.unwrap_or(&default_caption);
    let subtotal = if calculated || value.aggregation == "sum" {
        String::new()
    } else {
        format!(" subtotal=\"{}\"", value.aggregation)
    };
    let mode = value.show_data_as;
    let percent = mode.is_some_and(|mode| {
        ![
            "difference",
            "runTotal",
            "rankAscending",
            "rankDescending",
            "index",
        ]
        .contains(&mode)
    });
    let format_id = value
        .number_format
        .filter(|v| !v.is_empty())
        .map(|v| num_format(v, short_date))
        .unwrap_or(if percent { 10 } else { 0 });
    let needs_base = mode.is_some_and(|mode| {
        [
            "percentOf",
            "percentOfParent",
            "difference",
            "percentDiff",
            "runTotal",
            "percentOfRunningTotal",
            "rankAscending",
            "rankDescending",
        ]
        .contains(&mode)
    });
    let needs_item =
        mode.is_some_and(|mode| ["percentOf", "difference", "percentDiff"].contains(&mode));
    let base = if needs_base {
        value.base_field.unwrap_or(0)
    } else {
        0
    };
    let item = if needs_item {
        value.base_item.unwrap_or(0)
    } else {
        0
    };
    let show = mode
        .map(|mode| {
            format!(
                " showDataAs=\"{}\"",
                if mode == "percentOf" { "percent" } else { mode }
            )
        })
        .unwrap_or_default();
    let number = if format_id == 0 {
        String::new()
    } else {
        format!(" numFmtId=\"{format_id}\"")
    };
    format!(
        "<dataField name=\"{}\" fld=\"{field}\"{subtotal}{show} baseField=\"{base}\" baseItem=\"{item}\"{number}/>",
        escape_attribute(caption)
    )
}

pub(super) fn build_pivot_table(cache_id: u64, model: &PivotTable<'_>) -> Result<String, String> {
    let levels = model.row_fields.len();
    let col_levels = model.column_fields.len();
    let tabular = levels >= 2;
    let shape = if tabular {
        " compact=\"0\" outline=\"0\""
    } else {
        ""
    };
    let mut pivot_fields = String::new();
    for index in 0..model.field_names.len() {
        if let Some(level) = model.row_fields.iter().rposition(|field| *field == index) {
            let count = model.row_items.get(level).map_or(0, Vec::len);
            let hidden = model
                .row_hidden
                .get(level)
                .map(|v| v.iter().copied().collect())
                .unwrap_or_else(HashSet::new);
            pivot_fields.push_str(&build_axis_pivot_field("axisRow", count, tabular, &hidden));
        } else if let Some(level) = model
            .column_fields
            .iter()
            .rposition(|field| *field == index)
        {
            let count = model.column_items.get(level).map_or(0, Vec::len);
            let hidden = model
                .column_hidden
                .get(level)
                .map(|v| v.iter().copied().collect())
                .unwrap_or_else(HashSet::new);
            pivot_fields.push_str(&build_axis_pivot_field("axisCol", count, tabular, &hidden));
        } else if model
            .values
            .iter()
            .any(|value| value.calculated_name.is_none() && value.field_index == index)
        {
            pivot_fields.push_str(&format!(
                "<pivotField dataField=\"1\" showAll=\"0\"{shape}/>"
            ));
        } else if let Some(level) = model.page_fields.iter().rposition(|field| *field == index) {
            let count = model.page_items.get(level).map_or(0, Vec::len);
            pivot_fields.push_str(&build_axis_pivot_field(
                "axisPage",
                count,
                tabular,
                &HashSet::new(),
            ));
        } else {
            pivot_fields.push_str(&format!("<pivotField showAll=\"0\"{shape}/>"));
        }
    }
    let calc_count = model
        .values
        .iter()
        .filter(|value| value.calculated_name.is_some())
        .count();
    for _ in 0..calc_count {
        pivot_fields.push_str(&format!(
            "<pivotField dataField=\"1\" showAll=\"0\"{shape}/>"
        ));
    }
    let fields = |values: &[usize]| {
        values
            .iter()
            .map(|v| format!("<field x=\"{v}\"/>"))
            .collect::<String>()
    };
    let row_fields = format!(
        "<rowFields count=\"{}\">{}</rowFields>",
        model.row_fields.len(),
        fields(model.row_fields)
    );
    let rows = build_axis_items(model.row_lines, levels, "rowItems");
    let pages = if model.page_fields.is_empty() {
        String::new()
    } else {
        let mut children = String::new();
        for (index, field) in model.page_fields.iter().enumerate() {
            let item = model
                .page_selected
                .get(index)
                .copied()
                .flatten()
                .map(|item| format!(" item=\"{item}\""))
                .unwrap_or_default();
            children.push_str(&format!("<pageField fld=\"{field}\"{item} hier=\"-1\"/>"));
        }
        format!(
            "<pageFields count=\"{}\">{children}</pageFields>",
            model.page_fields.len()
        )
    };
    let columns = if col_levels > 0 {
        format!(
            "<colFields count=\"{col_levels}\">{}</colFields>{}",
            fields(model.column_fields),
            build_axis_items(model.column_lines, col_levels, "colItems")
        )
    } else if model.values.len() > 1 {
        let items = (0..model.values.len())
            .map(|index| {
                if index == 0 {
                    "<i><x/></i>".to_owned()
                } else {
                    format!("<i i=\"{index}\"><x v=\"{index}\"/></i>")
                }
            })
            .collect::<String>();
        format!(
            "<colFields count=\"1\"><field x=\"-2\"/></colFields><colItems count=\"{}\">{items}</colItems>",
            model.values.len()
        )
    } else {
        "<colItems count=\"1\"><i/></colItems>".to_owned()
    };
    let mut data = String::new();
    let mut calculated = 0;
    for value in model.values {
        data.push_str(&data_field(
            value,
            model.field_names,
            calculated,
            model.short_date,
        ));
        if value.calculated_name.is_some() {
            calculated += 1;
        }
    }
    let filters = build_pivot_filters(model.filters);
    let grouping = if model.groupings.is_empty() {
        String::new()
    } else {
        format!(
            "<extLst><ext uri=\"{{AIO-PIVOT-GROUPINGS}}\" xmlns:aio=\"urn:aioffice:pivot\"><aio:aioPivotGroupings v=\"{}\"/></ext></extLst>",
            escape_attribute(&serde_json::to_string(model.groupings).map_err(|e| e.to_string())?)
        )
    };
    let mode = if tabular {
        "compact=\"0\" compactData=\"0\" outline=\"0\" outlineData=\"0\" "
    } else {
        "outline=\"1\" outlineData=\"1\" "
    };
    let page_count = if model.page_fields.is_empty() {
        String::new()
    } else {
        format!(
            " rowPageCount=\"{}\" colPageCount=\"1\"",
            model.page_fields.len()
        )
    };
    Ok(format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
            "<pivotTableDefinition xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" name=\"{}\" cacheId=\"{cache_id}\" ",
            "applyNumberFormats=\"0\" applyBorderFormats=\"0\" applyFontFormats=\"0\" applyPatternFormats=\"0\" ",
            "applyAlignmentFormats=\"0\" applyWidthHeightFormats=\"1\" dataCaption=\"Values\" updatedVersion=\"8\" createdVersion=\"8\" ",
            "minRefreshableVersion=\"3\" useAutoFormatting=\"1\" itemPrintTitles=\"1\" {mode}multipleFieldFilters=\"0\">",
            "<location ref=\"{}\" firstHeaderRow=\"1\" firstDataRow=\"{}\" firstDataCol=\"{levels}\"{page_count}/>",
            "<pivotFields count=\"{}\">{pivot_fields}</pivotFields>{row_fields}{rows}{pages}{columns}",
            "<dataFields count=\"{}\">{data}</dataFields><pivotTableStyleInfo name=\"PivotStyleLight16\" showRowHeaders=\"1\" ",
            "showColHeaders=\"1\" showRowStripes=\"0\" showColStripes=\"0\" showLastColumn=\"1\"/>{filters}{grouping}</pivotTableDefinition>"
        ),
        escape_attribute(model.name),
        model.location,
        col_levels.max(1),
        model.field_names.len() + calc_count,
        model.values.len(),
        cache_id = cache_id,
        mode = mode,
        levels = levels,
        page_count = page_count,
        pivot_fields = pivot_fields,
        row_fields = row_fields,
        rows = rows,
        pages = pages,
        columns = columns,
        data = data,
        filters = filters,
        grouping = grouping
    ))
}

#[cfg(test)]
mod tests {
    use super::super::AxisLine;
    use super::{PivotTable, PivotValue, build_pivot_table};

    #[test]
    fn writes_multilevel_fields_items_and_value_caption() {
        let fields = vec!["Region".to_owned(), "City".to_owned(), "Amount".to_owned()];
        let members = vec![
            vec!["West".to_owned()],
            vec!["Paris".to_owned(), "Lyon".to_owned()],
        ];
        let lines = vec![
            AxisLine {
                kind: "data".to_owned(),
                members: vec![0, 0],
            },
            AxisLine {
                kind: "data".to_owned(),
                members: vec![0, 1],
            },
        ];
        let values = [PivotValue {
            field_index: 2,
            aggregation: "sum",
            caption: None,
            number_format: None,
            show_data_as: None,
            base_field: None,
            base_item: None,
            calculated_name: None,
        }];
        let model = PivotTable {
            name: "Sales",
            location: "A1:C4",
            field_names: &fields,
            row_fields: &[0, 1],
            row_items: &members,
            row_lines: &lines,
            row_hidden: &[],
            column_fields: &[],
            column_items: &[],
            column_lines: &[],
            column_hidden: &[],
            page_fields: &[],
            page_items: &[],
            page_selected: &[],
            values: &values,
            filters: &[],
            groupings: &[],
            short_date: "m/d/yyyy",
        };
        let xml = build_pivot_table(7, &model).unwrap();
        assert!(xml.contains(
            r#"<location ref="A1:C4" firstHeaderRow="1" firstDataRow="1" firstDataCol="2"/>"#
        ));
        assert!(xml.contains(r#"<rowFields count="2"><field x="0"/><field x="1"/></rowFields>"#));
        assert!(xml.contains(r#"<i r="1"><x v="1"/></i>"#));
        assert!(
            xml.contains(r#"<dataField name="Sum of Amount" fld="2" baseField="0" baseItem="0"/>"#)
        );
    }
}
