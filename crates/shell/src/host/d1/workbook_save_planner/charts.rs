use jscompat::TryJsRegex;

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn title_xml(text: &str) -> String {
    format!(
        "<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>"
    )
}

fn set_title(xml: &str, title: &str) -> Result<String, String> {
    let text = escape_text(title);
    let xml = xml.replacen(
        "<c:autoTitleDeleted val=\"1\"/>",
        "<c:autoTitleDeleted val=\"0\"/>",
        1,
    );
    let plot = TryJsRegex::new(r"<c:plotArea[\s>]", "").map_err(|e| e.to_string())?;
    let end = plot
        .try_find_iter(&xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .map_or(xml.len(), |m| m.start());
    let titles = TryJsRegex::new(r"<c:title>[\s\S]*?</c:title>", "").map_err(|e| e.to_string())?;
    if let Some(found) = titles
        .try_find_iter(&xml[..end])
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
    {
        let runs = TryJsRegex::new(r"<a:t>[\s\S]*?</a:t>|<a:t/>", "").map_err(|e| e.to_string())?;
        let mut next = String::new();
        let mut last = 0;
        let mut count = 0;
        for run in runs.try_find_iter(found.as_str()) {
            let run = run.map_err(|e| e.to_string())?;
            next.push_str(&found.as_str()[last..run.start()]);
            next.push_str("<a:t>");
            if count == 0 {
                next.push_str(&text);
            }
            next.push_str("</a:t>");
            count += 1;
            last = run.end();
        }
        if count == 0 {
            next = title_xml(&text);
        } else {
            next.push_str(&found.as_str()[last..]);
        }
        return Ok(format!(
            "{}{}{}",
            &xml[..found.start()],
            next,
            &xml[found.end()..]
        ));
    }
    let at = xml
        .find("<c:chart>")
        .ok_or("Chart part has no c:chart element.")?
        + "<c:chart>".len();
    let auto = if xml.contains("<c:autoTitleDeleted ") {
        ""
    } else {
        "<c:autoTitleDeleted val=\"0\"/>"
    };
    Ok(format!(
        "{}{}{auto}{}",
        &xml[..at],
        title_xml(&text),
        &xml[at..]
    ))
}

fn match_span(xml: &str, pattern: &str) -> Result<Option<(usize, usize)>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    let result = re
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(result.map(|m| (m.start(), m.end())))
}

fn splice(xml: &str, start: usize, end: usize, value: &str) -> String {
    format!("{}{}{}", &xml[..start], value, &xml[end..])
}

fn set_gridlines(xml: &str, on: bool) -> Result<String, String> {
    let (start, end) =
        match_span(xml, r"<c:valAx>[\s\S]*?</c:valAx>")?.ok_or("Chart has no value axis.")?;
    let axis = &xml[start..end];
    let grid = match_span(
        axis,
        r"<c:majorGridlines/>|<c:majorGridlines>[\s\S]*?</c:majorGridlines>",
    )?;
    let updated = match (on, grid) {
        (false, Some((a, b))) => splice(axis, a, b, ""),
        (false, None) | (true, Some(_)) => axis.to_owned(),
        (true, None) => {
            let anchor = match_span(axis, r#"<c:axPos val="[^"]*"/>"#)?
                .or(match_span(axis, r#"<c:delete val="[^"]*"/>"#)?)
                .ok_or("Chart axis is missing its position element.")?;
            splice(axis, anchor.1, anchor.1, "<c:majorGridlines/>")
        }
    };
    Ok(splice(xml, start, end, &updated))
}

fn set_gap_width(xml: &str, pct: &str) -> Result<String, String> {
    let (start, end) = match_span(xml, r"<c:barChart>[\s\S]*?</c:barChart>")?
        .ok_or("Only bar charts have a gap width.")?;
    let plot = &xml[start..end];
    let element = format!("<c:gapWidth val=\"{pct}\"/>");
    let updated = if let Some((a, b)) = match_span(plot, r#"<c:gapWidth val="[^"]*"/>"#)? {
        splice(plot, a, b, &element)
    } else {
        let re = TryJsRegex::new(r"</c:ser>|</c:dLbls>", "g").map_err(|e| e.to_string())?;
        let mut at = None;
        for found in re.try_find_iter(plot) {
            at = Some(found.map_err(|e| e.to_string())?.end());
        }
        let at = at.ok_or("This chart has no series.")?;
        splice(plot, at, at, &element)
    };
    Ok(splice(xml, start, end, &updated))
}

fn set_hole_size(xml: &str, pct: &str) -> Result<String, String> {
    let (start, end) = match_span(xml, r"<c:doughnutChart>[\s\S]*?</c:doughnutChart>")?
        .ok_or("Only doughnut charts have a hole size.")?;
    let plot = &xml[start..end];
    let element = format!("<c:holeSize val=\"{pct}\"/>");
    let updated = if let Some((a, b)) = match_span(plot, r#"<c:holeSize val="[^"]*"/>"#)? {
        splice(plot, a, b, &element)
    } else {
        let at = match_span(plot, r"<c:extLst[\s/>]")?
            .map_or(plot.len() - "</c:doughnutChart>".len(), |m| m.0);
        splice(plot, at, at, &element)
    };
    Ok(splice(xml, start, end, &updated))
}

fn set_grouping(xml: &str, grouping: &str) -> Result<String, String> {
    let (start, end) = match_span(xml, r"<c:(barChart|lineChart|areaChart)>[\s\S]*?</c:\1>")?
        .ok_or("Only bar, line, and area charts support stacking.")?;
    let plot = &xml[start..end];
    let kind = if plot.starts_with("<c:barChart>") {
        "barChart"
    } else if plot.starts_with("<c:lineChart>") {
        "lineChart"
    } else {
        "areaChart"
    };
    let value = if grouping == "clustered" && kind != "barChart" {
        "standard"
    } else {
        grouping
    };
    let open = format!("<c:{kind}>");
    let close = format!("</c:{kind}>");
    let inner = &plot[open.len()..plot.len() - close.len()];
    let element = format!("<c:grouping val=\"{value}\"/>");
    let mut inner = if let Some((a, b)) = match_span(inner, r#"<c:grouping val="[^"]*"/>"#)? {
        splice(inner, a, b, &element)
    } else {
        let at = match_span(inner, r#"<c:barDir val="[^"]*"/>"#)?.map_or(0, |m| m.1);
        splice(inner, at, at, &element)
    };
    if kind == "barChart" {
        let overlap = if grouping == "clustered" {
            "-27"
        } else {
            "100"
        };
        let element = format!("<c:overlap val=\"{overlap}\"/>");
        inner = if let Some((a, b)) = match_span(&inner, r#"<c:overlap val="[^"]*"/>"#)? {
            splice(&inner, a, b, &element)
        } else {
            let at = match_span(&inner, r#"<c:axId val="[0-9]+"/>"#)?
                .ok_or("Chart has no axes for a stacking edit.")?
                .0;
            splice(&inner, at, at, &element)
        };
    }
    Ok(splice(xml, start, end, &format!("{open}{inner}{close}")))
}

fn set_value_axis(xml: &str, bounds: &serde_json::Value) -> Result<String, String> {
    let (start, end) =
        match_span(xml, r"<c:valAx>[\s\S]*?</c:valAx>")?.ok_or("Chart has no value axis.")?;
    let axis = &xml[start..end];
    let (a, b) = match_span(axis, r"<c:scaling>[\s\S]*?</c:scaling>")?
        .ok_or("Chart value axis has no scaling element.")?;
    let scaling = &axis[a + "<c:scaling>".len()..b - "</c:scaling>".len()];
    let mut scaling = scaling.to_owned();
    for name in ["max", "min"] {
        let Some(value) = bounds.get(name) else {
            continue;
        };
        let pattern = format!(r#"<c:{name} val="[^"]*"/>"#);
        let existing = match_span(&scaling, &pattern)?;
        if value.is_null() {
            if let Some((a, b)) = existing {
                scaling = splice(&scaling, a, b, "");
            }
            continue;
        }
        let value = value.as_number().ok_or("Invalid chart axis bound.")?;
        let element = format!("<c:{name} val=\"{value}\"/>");
        if let Some((a, b)) = existing {
            scaling = splice(&scaling, a, b, &element);
            continue;
        }
        let pattern = if name == "max" {
            r#"<c:logBase val="[^"]*"/>|<c:orientation val="[^"]*"/>"#
        } else {
            r#"<c:logBase val="[^"]*"/>|<c:orientation val="[^"]*"/>|<c:max val="[^"]*"/>"#
        };
        let re = TryJsRegex::new(pattern, "g").map_err(|e| e.to_string())?;
        let mut at = 0;
        for found in re.try_find_iter(&scaling) {
            at = found.map_err(|e| e.to_string())?.end();
        }
        scaling = splice(&scaling, at, at, &element);
    }
    let axis = splice(axis, a, b, &format!("<c:scaling>{scaling}</c:scaling>"));
    Ok(splice(xml, start, end, &axis))
}

fn set_legend(xml: &str, legend: &str) -> Result<String, String> {
    let xml = if let Some((a, b)) = match_span(xml, r"<c:legend>[\s\S]*?</c:legend>")? {
        splice(xml, a, b, "")
    } else {
        xml.to_owned()
    };
    if legend == "none" {
        return Ok(xml);
    }
    let position = match legend {
        "right" => "r",
        "bottom" => "b",
        "top" => "t",
        "left" => "l",
        _ => return Err("Invalid chart legend position.".into()),
    };
    let at = xml
        .find("</c:plotArea>")
        .ok_or("Chart part has no plot area.")?
        + "</c:plotArea>".len();
    Ok(splice(
        &xml,
        at,
        at,
        &format!("<c:legend><c:legendPos val=\"{position}\"/><c:overlay val=\"0\"/></c:legend>"),
    ))
}

fn set_axis_title(xml: &str, axis: &str, title: Option<&str>) -> Result<String, String> {
    let pattern = format!(r"<c:{axis}>[\s\S]*?</c:{axis}>");
    let mut found = match_span(xml, &pattern)?;
    if found.is_none() && axis == "catAx" {
        found = match_span(xml, r"<c:dateAx>[\s\S]*?</c:dateAx>")?;
    }
    let (start, end) = found.ok_or_else(|| {
        format!(
            "Chart has no {} axis.",
            if axis == "catAx" { "category" } else { "value" }
        )
    })?;
    let axis_xml = &xml[start..end];
    let stripped = if let Some((a, b)) = match_span(axis_xml, r"<c:title>[\s\S]*?</c:title>")? {
        splice(axis_xml, a, b, "")
    } else {
        axis_xml.to_owned()
    };
    let Some(title) = title else {
        return Ok(splice(xml, start, end, &stripped));
    };
    let re = TryJsRegex::new(r#"<c:minorGridlines/>|<c:minorGridlines>[\s\S]*?</c:minorGridlines>|<c:majorGridlines/>|<c:majorGridlines>[\s\S]*?</c:majorGridlines>|<c:axPos val="[^"]*"/>"#, "g").map_err(|e| e.to_string())?;
    let mut at = None;
    for m in re.try_find_iter(&stripped) {
        at = Some(m.map_err(|e| e.to_string())?.end());
    }
    let at = at.ok_or("Chart axis is missing its position element.")?;
    Ok(splice(
        xml,
        start,
        end,
        &splice(&stripped, at, at, &title_xml(&escape_text(title))),
    ))
}

fn plot_span(xml: &str) -> Result<(usize, usize, &str), String> {
    let (a, b) = match_span(
        xml,
        r"<c:(barChart|lineChart|areaChart|pieChart|doughnutChart)>[\s\S]*?</c:\1>",
    )?
    .ok_or("This chart type does not support data labels.")?;
    let kind = [
        "barChart",
        "lineChart",
        "areaChart",
        "pieChart",
        "doughnutChart",
    ]
    .into_iter()
    .find(|kind| xml[a..].starts_with(&format!("<c:{kind}>")))
    .ok_or("This chart type does not support data labels.")?;
    Ok((a, b, kind))
}

fn set_labels(xml: &str, mode: &str) -> Result<String, String> {
    let (a, b, _) = plot_span(xml)?;
    let plot = &xml[a..b];
    let pattern = r"</c:ser><c:dLbls>[\s\S]*?</c:dLbls>";
    let mut plot = if let Some((start, end)) = match_span(plot, pattern)? {
        splice(plot, start, end, "</c:ser>")
    } else {
        plot.to_owned()
    };
    if mode != "none" {
        let value = if mode == "value" { 1 } else { 0 };
        let category = if mode == "category-percent" { 1 } else { 0 };
        let percent = if mode == "percent" || mode == "category-percent" {
            1
        } else {
            0
        };
        let labels = format!(
            "<c:dLbls><c:showLegendKey val=\"0\"/><c:showVal val=\"{value}\"/><c:showCatName val=\"{category}\"/><c:showSerName val=\"0\"/><c:showPercent val=\"{percent}\"/><c:showBubbleSize val=\"0\"/></c:dLbls>"
        );
        let at = plot
            .rfind("</c:ser>")
            .ok_or("This chart has no series to label.")?
            + "</c:ser>".len();
        plot = splice(&plot, at, at, &labels);
    }
    Ok(splice(xml, a, b, &plot))
}

fn set_label_detail(
    xml: &str,
    position: Option<&str>,
    format: Option<&str>,
) -> Result<String, String> {
    let (a, b, _) = plot_span(xml)?;
    let plot = &xml[a..b];
    let num = format.map(|v| {
        format!(
            "<c:numFmt formatCode=\"{}\" sourceLinked=\"0\"/>",
            escape_text(v).replace('"', "&quot;")
        )
    });
    let pos = position
        .map(|v| match v {
            "center" => "ctr",
            "inside-end" => "inEnd",
            "outside-end" => "outEnd",
            _ => v,
        })
        .map(|v| format!("<c:dLblPos val=\"{v}\"/>"));
    let span = match_span(plot, r"</c:ser><c:dLbls>[\s\S]*?</c:dLbls>")?;
    let next = if let Some((start, end)) = span {
        let labels = &plot[start + "</c:ser><c:dLbls>".len()..end - "</c:dLbls>".len()];
        let mut labels = labels.to_owned();
        if let Some(num) = &num {
            labels = if let Some((a, b)) = match_span(&labels, r"<c:numFmt [^>]*/>")? {
                splice(&labels, a, b, num)
            } else {
                format!("{num}{labels}")
            };
        }
        if let Some(pos) = &pos {
            labels = if let Some((a, b)) = match_span(&labels, r#"<c:dLblPos val="[^"]*"/>"#)? {
                splice(&labels, a, b, pos)
            } else {
                let at =
                    match_span(&labels, r"<c:show|<c:separator[>/]")?.map_or(labels.len(), |m| m.0);
                splice(&labels, at, at, pos)
            };
        }
        splice(
            plot,
            start,
            end,
            &format!("</c:ser><c:dLbls>{labels}</c:dLbls>"),
        )
    } else {
        let at = plot
            .rfind("</c:ser>")
            .ok_or("This chart has no series to label.")?
            + "</c:ser>".len();
        let labels = format!(
            "<c:dLbls>{}{}<c:showLegendKey val=\"0\"/><c:showVal val=\"1\"/><c:showCatName val=\"0\"/><c:showSerName val=\"0\"/><c:showPercent val=\"0\"/><c:showBubbleSize val=\"0\"/></c:dLbls>",
            num.unwrap_or_default(),
            pos.unwrap_or_default()
        );
        splice(plot, at, at, &labels)
    };
    Ok(splice(xml, a, b, &next))
}

fn series_span(xml: &str, index: usize) -> Result<(usize, usize), String> {
    let re = TryJsRegex::new(r"<c:ser>[\s\S]*?</c:ser>", "g").map_err(|e| e.to_string())?;
    let found = re
        .try_find_iter(xml)
        .nth(index)
        .transpose()
        .map_err(|e| e.to_string())?;
    found
        .map(|m| (m.start(), m.end()))
        .ok_or_else(|| format!("Chart has no series with index {index}."))
}

fn set_series_color(xml: &str, index: usize, color: &str) -> Result<String, String> {
    let (a, b) = series_span(xml, index)?;
    let series = &xml[a..b];
    let rgb = color.strip_prefix('#').unwrap_or(color).to_uppercase();
    let fill = format!(
        "<c:spPr><a:solidFill><a:srgbClr val=\"{rgb}\"/></a:solidFill><a:ln><a:solidFill><a:srgbClr val=\"{rgb}\"/></a:solidFill></a:ln></c:spPr>"
    );
    let end = series.find("<c:dPt>").unwrap_or(series.len());
    let next = if let Some((a, b)) = match_span(&series[..end], r"<c:spPr>[\s\S]*?</c:spPr>")? {
        splice(series, a, b, &fill)
    } else {
        let anchor = match_span(series, r"<c:tx>[\s\S]*?</c:tx>")?
            .or(match_span(series, r#"<c:order val="[0-9]+"/>"#)?)
            .ok_or("Chart series is missing idx/order structure.")?;
        splice(series, anchor.1, anchor.1, &fill)
    };
    Ok(splice(xml, a, b, &next))
}

fn set_series_explosion(xml: &str, pct: &str) -> Result<String, String> {
    let (a, b) = match_span(xml, r"<c:(pieChart|doughnutChart)>[\s\S]*?</c:\1>")?
        .ok_or("Only pie and doughnut charts support explosion.")?;
    let plot = &xml[a..b];
    let (sa, sb) =
        match_span(plot, r"<c:ser>[\s\S]*?</c:ser>")?.ok_or("This chart has no series.")?;
    let series = &plot[sa..sb];
    let element = format!("<c:explosion val=\"{pct}\"/>");
    let dp = match_span(series, r"<c:dPt[>/]")?.map(|x| x.0);
    let scope = &series[..dp.unwrap_or(series.len())];
    let next = if let Some((x, y)) = match_span(scope, r#"<c:explosion val="[^"]*"/>"#)? {
        splice(series, x, y, &element)
    } else {
        let at = if let Some(at) = dp {
            at
        } else {
            match_span(series, r"<c:dLbls[>/]|<c:cat[>/]|<c:val[>/]")?
                .map_or(series.len() - "</c:ser>".len(), |m| m.0)
        };
        splice(series, at, at, &element)
    };
    Ok(splice(xml, a, b, &splice(plot, sa, sb, &next)))
}

fn point_spans(series: &str) -> Result<Vec<(usize, usize, Option<usize>)>, String> {
    let re = TryJsRegex::new(r"<c:dPt>[\s\S]*?</c:dPt>", "g").map_err(|e| e.to_string())?;
    let idx = TryJsRegex::new(r#"<c:idx val="([0-9]+)"/>"#, "").map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for point in re.try_find_iter(series) {
        let point = point.map_err(|e| e.to_string())?;
        let found = idx
            .try_find_iter(point.as_str())
            .next()
            .transpose()
            .map_err(|e| e.to_string())?;
        let index = found.and_then(|m| m.get(1).and_then(|s| s.parse().ok()));
        out.push((point.start(), point.end(), index));
    }
    Ok(out)
}

fn point_insertion(
    series: &str,
    points: &[(usize, usize, Option<usize>)],
    index: usize,
) -> Result<usize, String> {
    let mut at = None;
    for &(start, end, i) in points {
        if i.is_some_and(|i| i > index) {
            return Ok(start);
        }
        at = Some(end);
    }
    if let Some(at) = at {
        return Ok(at);
    }
    Ok(
        match_span(series, r"<c:dLbls[>/]|<c:cat[>/]|<c:val[>/]|<c:xVal[>/]")?
            .ok_or("Chart series has no data element to anchor a point.")?
            .0,
    )
}

fn set_point_color(series: &str, index: usize, color: &str) -> Result<String, String> {
    let rgb = color.strip_prefix('#').unwrap_or(color).to_uppercase();
    let fill = format!("<a:solidFill><a:srgbClr val=\"{rgb}\"/></a:solidFill>");
    let points = point_spans(series)?;
    if let Some(&(a, b, _)) = points.iter().find(|(_, _, i)| *i == Some(index)) {
        let point = &series[a..b];
        let patched = if let Some((x, y)) = match_span(point, r"<c:spPr>[\s\S]*?</c:spPr>")? {
            let inner = &point[x + "<c:spPr>".len()..y - "</c:spPr>".len()];
            let line = match_span(inner, r"<a:ln[\s>]")?.map_or(inner.len(), |m| m.0);
            let head = &inner[..line];
            let pattern = r"<a:noFill/>|<a:solidFill>[\s\S]*?</a:solidFill>|<a:gradFill[\s\S]*?</a:gradFill>|<a:pattFill[\s\S]*?</a:pattFill>|<a:blipFill[\s\S]*?</a:blipFill>|<a:grpFill/>";
            let head = if let Some((h, t)) = match_span(head, pattern)? {
                splice(head, h, t, &fill)
            } else {
                format!("{head}{fill}")
            };
            splice(
                point,
                x,
                y,
                &format!("<c:spPr>{head}{}</c:spPr>", &inner[line..]),
            )
        } else {
            let at = match_span(point, r"<c:pictureOptions[\s/>]|<c:extLst[\s/>]")?
                .map_or(point.len() - "</c:dPt>".len(), |m| m.0);
            splice(point, at, at, &format!("<c:spPr>{fill}</c:spPr>"))
        };
        return Ok(splice(series, a, b, &patched));
    }
    let at = point_insertion(series, &points, index)?;
    Ok(splice(
        series,
        at,
        at,
        &format!(
            "<c:dPt><c:idx val=\"{index}\"/><c:bubble3D val=\"0\"/><c:spPr>{fill}</c:spPr></c:dPt>"
        ),
    ))
}

fn set_point_explosion(series: &str, index: usize, pct: &str) -> Result<String, String> {
    let element = format!("<c:explosion val=\"{pct}\"/>");
    let points = point_spans(series)?;
    if let Some(&(a, b, _)) = points.iter().find(|(_, _, i)| *i == Some(index)) {
        let point = &series[a..b];
        let patched = if let Some((x, y)) = match_span(point, r#"<c:explosion val="[^"]*"/>"#)? {
            splice(point, x, y, &element)
        } else {
            let at = match_span(
                point,
                r"<c:spPr[\s/>]|<c:pictureOptions[\s/>]|<c:extLst[\s/>]",
            )?
            .map_or(point.len() - "</c:dPt>".len(), |m| m.0);
            splice(point, at, at, &element)
        };
        return Ok(splice(series, a, b, &patched));
    }
    let at = point_insertion(series, &points, index)?;
    Ok(splice(
        series,
        at,
        at,
        &format!("<c:dPt><c:idx val=\"{index}\"/><c:bubble3D val=\"0\"/>{element}</c:dPt>"),
    ))
}

fn sorted_entries(value: &serde_json::Value) -> Result<Vec<(usize, &serde_json::Value)>, String> {
    let object = value.as_object().ok_or("Invalid chart point map.")?;
    let mut entries = object
        .iter()
        .map(|(key, value)| {
            key.parse::<usize>()
                .map(|index| (index, value))
                .map_err(|_| "Invalid chart point index.".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|(index, _)| *index);
    Ok(entries)
}

fn set_point_colors(
    xml: &str,
    series_index: usize,
    points: &serde_json::Value,
) -> Result<String, String> {
    let (a, b) = series_span(xml, series_index)?;
    let mut series = xml[a..b].to_owned();
    for (index, color) in sorted_entries(points)? {
        series = set_point_color(
            &series,
            index,
            color.as_str().ok_or("Invalid chart point color.")?,
        )?;
    }
    Ok(splice(xml, a, b, &series))
}

fn set_point_explosions(xml: &str, points: &serde_json::Value) -> Result<String, String> {
    let (a, b) = match_span(xml, r"<c:(pieChart|doughnutChart)>[\s\S]*?</c:\1>")?
        .ok_or("Only pie and doughnut charts support per-slice explosion.")?;
    let plot = &xml[a..b];
    let (sa, sb) =
        match_span(plot, r"<c:ser>[\s\S]*?</c:ser>")?.ok_or("This chart has no series.")?;
    let mut series = plot[sa..sb].to_owned();
    for (index, pct) in sorted_entries(points)? {
        series = set_point_explosion(
            &series,
            index,
            &pct.as_number()
                .ok_or("Invalid chart point explosion.")?
                .to_string(),
        )?;
    }
    Ok(splice(xml, a, b, &splice(plot, sa, sb, &series)))
}

fn inner_xml(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<c:{tag}>");
    let close = format!("</c:{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_owned())
}

fn points(values: &[String]) -> String {
    let mut result = format!("<c:ptCount val=\"{}\"/>", values.len());
    for (index, value) in values.iter().enumerate() {
        result.push_str(&format!("<c:pt idx=\"{index}\"><c:v>{value}</c:v></c:pt>"));
    }
    result
}

fn numeric_data(reference: Option<&str>, values: &[String]) -> String {
    let entries = points(values);
    match reference {
        Some(reference) => format!(
            "<c:numRef><c:f>{}</c:f><c:numCache><c:formatCode>General</c:formatCode>{entries}</c:numCache></c:numRef>",
            escape_text(reference)
        ),
        None => format!("<c:numLit>{entries}</c:numLit>"),
    }
}

fn string_values(value: &serde_json::Value) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or("Invalid chart categories.")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "Invalid chart category.".into())
        })
        .collect()
}

fn number_values(value: &serde_json::Value) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or("Invalid chart values.")?
        .iter()
        .map(|v| {
            v.as_number()
                .map(ToString::to_string)
                .ok_or_else(|| "Invalid chart value.".into())
        })
        .collect()
}

fn build_series(entry: &serde_json::Value, index: usize, scatter: bool) -> Result<String, String> {
    let name = escape_text(
        entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Invalid chart series name.")?,
    );
    let values = number_values(entry.get("values").ok_or("Invalid chart series values.")?)?;
    let categories = entry
        .get("categories")
        .map(string_values)
        .transpose()?
        .unwrap_or_default();
    let categories = &categories[..categories.len().min(values.len())];
    let color = entry
        .get("color")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.strip_prefix('#').unwrap_or(s).to_uppercase());
    let sp = color.map(|rgb| if scatter { format!("<c:spPr><a:ln><a:solidFill><a:srgbClr val=\"{rgb}\"/></a:solidFill></a:ln></c:spPr>") } else { format!("<c:spPr><a:solidFill><a:srgbClr val=\"{rgb}\"/></a:solidFill></c:spPr>") }).unwrap_or_default();
    let val_ref = entry.get("valuesRef").and_then(serde_json::Value::as_str);
    let data = if scatter {
        let numbers: Vec<_> = categories.iter().map(|s| jscompat::to_number(s)).collect();
        let numeric = categories.len() == values.len()
            && !categories.is_empty()
            && numbers.iter().all(|n| n.is_finite());
        let x: Vec<String> = if numeric {
            numbers
                .iter()
                .map(|v| jscompat::number_to_string(*v))
                .collect()
        } else {
            (0..values.len()).map(|n| n.to_string()).collect()
        };
        let cat_ref = if numeric {
            entry
                .get("categoriesRef")
                .and_then(serde_json::Value::as_str)
        } else {
            None
        };
        format!(
            "<c:xVal>{}</c:xVal><c:yVal>{}</c:yVal><c:smooth val=\"0\"/>",
            numeric_data(cat_ref, &x),
            numeric_data(val_ref, &values)
        )
    } else {
        let cat = if categories.is_empty() {
            String::new()
        } else {
            let cats: Vec<_> = categories.iter().map(|v| escape_text(v)).collect();
            let p = points(&cats);
            match entry
                .get("categoriesRef")
                .and_then(serde_json::Value::as_str)
            {
                Some(r) => format!(
                    "<c:cat><c:strRef><c:f>{}</c:f><c:strCache>{p}</c:strCache></c:strRef></c:cat>",
                    escape_text(r)
                ),
                None => format!("<c:cat><c:strLit>{p}</c:strLit></c:cat>"),
            }
        };
        format!("{cat}<c:val>{}</c:val>", numeric_data(val_ref, &values))
    };
    Ok(format!(
        "<c:ser><c:idx val=\"{index}\"/><c:order val=\"{index}\"/><c:tx><c:v>{name}</c:v></c:tx>{sp}{data}</c:ser>"
    ))
}

fn data_ref(entry: &serde_json::Value, field: &str, existing: &str) -> Option<String> {
    entry
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(escape_text)
        .or_else(|| inner_xml(existing, "f"))
}

fn data_block(
    tag: &str,
    numeric: bool,
    reference: Option<&str>,
    format: &str,
    values: &[String],
) -> String {
    let prefix = if numeric {
        format!("<c:formatCode>{format}</c:formatCode>")
    } else {
        String::new()
    };
    let pts = points(values);
    let kind = if numeric { "num" } else { "str" };
    match reference {
        Some(reference) => format!(
            "<c:{tag}><c:{kind}Ref><c:f>{reference}</c:f><c:{kind}Cache>{prefix}{pts}</c:{kind}Cache></c:{kind}Ref></c:{tag}>"
        ),
        None => format!("<c:{tag}><c:{kind}Lit>{prefix}{pts}</c:{kind}Lit></c:{tag}>"),
    }
}

fn set_series_data(xml: &str, entry: &serde_json::Value) -> Result<String, String> {
    let index = entry
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .ok_or("Invalid chart series index.")? as usize;
    let (a, b) = series_span(xml, index)?;
    let mut series = xml[a..b].to_owned();
    let scatter = match_span(xml, r"<c:scatterChart[\s>]")?.is_some();
    if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
        let tx = format!("<c:tx><c:v>{}</c:v></c:tx>", escape_text(name));
        series = if let Some((a, b)) = match_span(&series, r"<c:tx>[\s\S]*?</c:tx>")? {
            splice(&series, a, b, &tx)
        } else {
            let at = match_span(&series, r#"<c:order val="[0-9]+"/>"#)?
                .ok_or("Chart series is missing idx/order structure.")?
                .1;
            splice(&series, at, at, &tx)
        };
    }
    if let Some(cats) = entry.get("categories") {
        let categories = string_values(cats)?;
        let tag = if scatter { "xVal" } else { "cat" };
        let pattern = format!(r"<c:{tag}/>|<c:{tag}>[\s\S]*?</c:{tag}>");
        let existing_span = match_span(&series, &pattern)?;
        let previous = existing_span.map_or("", |(a, b)| &series[a..b]);
        let reference = data_ref(entry, "categoriesRef", previous);
        let numeric = scatter
            || (match_span(previous, r"<c:num(?:Ref|Lit)[\s>]")?.is_some()
                && !categories.is_empty()
                && categories
                    .iter()
                    .all(|v| !v.trim().is_empty() && jscompat::to_number(v).is_finite()));
        let values: Vec<_> = if scatter {
            categories
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let n = jscompat::to_number(v);
                    if !v.trim().is_empty() && n.is_finite() {
                        jscompat::number_to_string(n)
                    } else {
                        i.to_string()
                    }
                })
                .collect()
        } else if numeric {
            categories
                .iter()
                .map(|v| jscompat::number_to_string(jscompat::to_number(v)))
                .collect()
        } else {
            categories.iter().map(|v| escape_text(v)).collect()
        };
        let fmt = if scatter {
            "General".into()
        } else {
            inner_xml(previous, "formatCode").unwrap_or_else(|| "General".into())
        };
        let block = data_block(tag, numeric, reference.as_deref(), &fmt, &values);
        series = if let Some((a, b)) = existing_span {
            splice(&series, a, b, &block)
        } else if scatter {
            let at = match_span(&series, r"<c:yVal[>/]|<c:smooth[\s/>]")?
                .map_or(series.len() - "</c:ser>".len(), |m| m.0);
            splice(&series, at, at, &block)
        } else {
            let at = match_span(&series, r"<c:val/>|<c:val>")?
                .ok_or("Chart series has no value element.")?
                .0;
            splice(&series, at, at, &block)
        };
    }
    if let Some(values) = entry.get("values") {
        let values = number_values(values)?;
        let tag = if scatter { "yVal" } else { "val" };
        let pattern = format!(r"<c:{tag}/>|<c:{tag}>[\s\S]*?</c:{tag}>");
        let (a, b) = match_span(&series, &pattern)?.ok_or("Chart series has no value element.")?;
        let previous = &series[a..b];
        let reference = data_ref(entry, "valuesRef", previous);
        let fmt = inner_xml(previous, "formatCode").unwrap_or_else(|| "General".into());
        series = splice(
            &series,
            a,
            b,
            &data_block(tag, true, reference.as_deref(), &fmt, &values),
        );
    }
    Ok(splice(xml, a, b, &series))
}

#[derive(Clone)]
struct Plot {
    start: usize,
    end: usize,
    family: String,
    inner: String,
}

fn plots(xml: &str) -> Result<Vec<Plot>, String> {
    let re = TryJsRegex::new(r"<c:(barChart|lineChart|areaChart|bar3DChart|line3DChart|area3DChart|pieChart|pie3DChart|doughnutChart|scatterChart|radarChart)>([\s\S]*?)</c:\1>", "g").map_err(|e| e.to_string())?;
    re.try_find_iter(xml)
        .map(|m| {
            let m = m.map_err(|e| e.to_string())?;
            Ok(Plot {
                start: m.start(),
                end: m.end(),
                family: m.get(1).unwrap_or("").into(),
                inner: m.get(2).unwrap_or("").into(),
            })
        })
        .collect()
}

fn remove_matches(xml: &str, pattern: &str) -> Result<String, String> {
    let re = TryJsRegex::new(pattern, "g").map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut at = 0;
    for found in re.try_find_iter(xml) {
        let found = found.map_err(|e| e.to_string())?;
        out.push_str(&xml[at..found.start()]);
        at = found.end();
    }
    out.push_str(&xml[at..]);
    Ok(out)
}

fn replace_plots(xml: &str, plots: &[Plot], replacement: &str) -> String {
    let mut out = xml.to_owned();
    for plot in plots.iter().rev() {
        out.replace_range(plot.start..plot.end, "");
    }
    out.insert_str(plots[0].start, replacement);
    out
}

fn replace_series(xml: &str, entries: &[serde_json::Value]) -> Result<String, String> {
    let plots = plots(xml)?;
    let plot = plots
        .first()
        .ok_or("This chart type does not support series replacement.")?;
    if plots.len() > 1 || entries.iter().any(|v| v.get("plot").is_some()) {
        return replace_combo_series(xml, &plots, entries);
    }
    if ![
        "barChart",
        "lineChart",
        "areaChart",
        "pieChart",
        "doughnutChart",
        "radarChart",
        "scatterChart",
    ]
    .contains(&plot.family.as_str())
    {
        return Err(format!(
            "Replacing series on a {} is not supported.",
            plot.family
        ));
    }
    let at = plot
        .inner
        .find("<c:ser>")
        .ok_or("This chart has no series.")?;
    let built = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| build_series(entry, i, plot.family == "scatterChart"))
        .collect::<Result<String, _>>()?;
    let rest = remove_matches(&plot.inner[at..], r"<c:ser>[\s\S]*?</c:ser>")?;
    Ok(splice(
        xml,
        plot.start,
        plot.end,
        &format!(
            "<c:{}>{}{built}{rest}</c:{}>",
            plot.family,
            &plot.inner[..at],
            plot.family
        ),
    ))
}

#[derive(Clone)]
struct Axis {
    xml: String,
    is_value: bool,
    id: Option<String>,
    cross: Option<String>,
    right: bool,
}

fn axis_matches(xml: &str) -> Result<Vec<Axis>, String> {
    let re = TryJsRegex::new(r"<c:(catAx|dateAx|valAx)>([\s\S]*?)</c:\1>", "g")
        .map_err(|e| e.to_string())?;
    re.try_find_iter(xml)
        .map(|m| {
            let m = m.map_err(|e| e.to_string())?;
            let s = m.as_str();
            let id = attribute_value(s, "axId")?;
            let cross = attribute_value(s, "crossAx")?;
            let is_value = m.get(1) == Some("valAx");
            Ok(Axis {
                xml: s.into(),
                is_value,
                id,
                cross,
                right: is_value
                    && (s.contains("<c:axPos val=\"r\"/>")
                        || s.contains("<c:crosses val=\"max\"/>")),
            })
        })
        .collect()
}

fn attribute_value(xml: &str, tag: &str) -> Result<Option<String>, String> {
    let pattern = format!(r#"<c:{tag} val="([^"]*)"/>"#);
    let re = TryJsRegex::new(&pattern, "").map_err(|e| e.to_string())?;
    let found = re
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(found.and_then(|m| m.get(1).map(str::to_owned)))
}

#[allow(clippy::type_complexity)]
fn axes_selection(
    axes: &[Axis],
) -> (
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Vec<usize>,
) {
    let val = axes
        .iter()
        .position(|a| a.is_value && !a.right && a.id.is_some())
        .or_else(|| axes.iter().position(|a| a.is_value));
    let cat = val.and_then(|v| {
        axes.iter()
            .position(|a| !a.is_value && a.cross == axes[v].id)
            .or_else(|| axes.iter().position(|a| !a.is_value))
    });
    let rest: Vec<_> = (0..axes.len())
        .filter(|i| Some(*i) != val && Some(*i) != cat)
        .collect();
    let second_val = rest
        .iter()
        .copied()
        .find(|i| axes[*i].is_value && axes[*i].right);
    let second_cat = second_val.and_then(|v| {
        rest.iter()
            .copied()
            .find(|i| !axes[*i].is_value && axes[*i].cross == axes[v].id)
            .or_else(|| rest.iter().copied().find(|i| !axes[*i].is_value))
    });
    let leftovers = rest
        .into_iter()
        .filter(|i| Some(*i) != second_val && Some(*i) != second_cat)
        .collect();
    (cat, val, second_cat, second_val, leftovers)
}

fn area_remainder(inner: &str, plots: &[Plot], axes: &[Axis]) -> Result<(String, String), String> {
    let mut rest = inner.to_owned();
    for plot in plots {
        let whole = format!("<c:{}>{}</c:{}>", plot.family, plot.inner, plot.family);
        rest = rest.replacen(&whole, "", 1);
    }
    for axis in axes {
        rest = rest.replacen(&axis.xml, "", 1);
    }
    rest = remove_matches(&rest, r"<c:(?:catAx|dateAx|valAx|serAx)/>")?;
    let layout = match_span(&rest, r"<c:layout/>|<c:layout>[\s\S]*?</c:layout>")?;
    if let Some((a, b)) = layout {
        Ok((rest[a..b].into(), splice(&rest, a, b, "")))
    } else {
        Ok((String::new(), rest))
    }
}

fn axis_pair(
    inner: &str,
    axes: &[Axis],
) -> Result<(usize, usize, Option<usize>, Option<usize>), String> {
    if match_span(inner, r"<c:serAx[>/]")?.is_some() {
        return Err("This chart has series axes this edit cannot rebuild.".into());
    }
    let (cat, val, second_cat, second_val, leftovers) = axes_selection(axes);
    let (cat, val) = (
        cat.ok_or("Combo chart is missing its primary axes.")?,
        val.ok_or("Combo chart is missing its primary axes.")?,
    );
    if axes[cat].id.is_none() || axes[val].id.is_none() {
        return Err("Combo chart is missing its primary axes.".into());
    }
    if !leftovers.is_empty() {
        return Err("This chart has more axes than this edit can rebuild.".into());
    }
    Ok((cat, val, second_cat, second_val))
}

fn replace_combo_series(
    xml: &str,
    plots: &[Plot],
    entries: &[serde_json::Value],
) -> Result<String, String> {
    if plots
        .iter()
        .any(|p| p.family != "barChart" && p.family != "lineChart")
    {
        return Err("Only bar + line combos support series replacement.".into());
    }
    let (a, b) = match_span(xml, r"<c:plotArea>[\s\S]*?</c:plotArea>")?
        .ok_or("Chart part has no plot area.")?;
    let inner = &xml[a + "<c:plotArea>".len()..b - "</c:plotArea>".len()];
    let axes = axis_matches(inner)?;
    let (cat, val, secondary_cat, secondary_val) = axis_pair(inner, &axes)?;
    let (primary_cat, primary_val) = (
        axes[cat].id.as_deref().unwrap(),
        axes[val].id.as_deref().unwrap(),
    );
    let wants_secondary = entries
        .iter()
        .any(|v| v.get("secondaryAxis").and_then(serde_json::Value::as_bool) == Some(true));
    let (sec_cat, sec_val, sec_xml) = if wants_secondary {
        if let (Some(c), Some(v)) = (secondary_cat, secondary_val) {
            if let (Some(cid), Some(vid)) = (&axes[c].id, &axes[v].id) {
                (
                    cid.clone(),
                    vid.clone(),
                    format!("{}{}", axes[v].xml, axes[c].xml),
                )
            } else {
                synth_secondary(inner)?
            }
        } else {
            synth_secondary(inner)?
        }
    } else {
        (String::new(), String::new(), String::new())
    };
    let default = if plots[0].family == "lineChart" {
        "lineChart"
    } else {
        "barChart"
    };
    let mut groups = String::new();
    for secondary in [false, true] {
        for family in ["barChart", "lineChart"] {
            let selected: Vec<_> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.get("plot")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(default)
                        == family
                        && (e.get("secondaryAxis").and_then(serde_json::Value::as_bool)
                            == Some(true))
                            == secondary
                })
                .collect();
            if selected.is_empty() {
                continue;
            }
            let series = selected
                .into_iter()
                .map(|(i, e)| build_series(e, i, false))
                .collect::<Result<String, _>>()?;
            let old = plots
                .iter()
                .rev()
                .find(|p| p.family == family)
                .map_or("", |p| p.inner.as_str());
            let labels = match_span(old, r"</c:ser><c:dLbls>[\s\S]*?</c:dLbls>")?
                .map(|(a, b)| &old[a + "</c:ser>".len()..b])
                .unwrap_or("");
            let grouping = attribute_value(old, "grouping")?.unwrap_or_else(|| {
                if family == "barChart" {
                    "clustered".into()
                } else {
                    "standard".into()
                }
            });
            let (cid, vid) = if secondary {
                (sec_cat.as_str(), sec_val.as_str())
            } else {
                (primary_cat, primary_val)
            };
            let ids = format!("<c:axId val=\"{cid}\"/><c:axId val=\"{vid}\"/>");
            if family == "barChart" {
                let dir = attribute_value(old, "barDir")?.unwrap_or_else(|| "col".into());
                let gap = match_span(old, r#"<c:gapWidth val="[^"]*"/>"#)?
                    .map_or("", |(a, b)| &old[a..b]);
                let overlap =
                    match_span(old, r#"<c:overlap val="[^"]*"/>"#)?.map_or("", |(a, b)| &old[a..b]);
                groups.push_str(&format!("<c:barChart><c:barDir val=\"{dir}\"/><c:grouping val=\"{grouping}\"/><c:varyColors val=\"0\"/>{series}{labels}{gap}{overlap}{ids}</c:barChart>"));
            } else {
                let marker = match_span(old, r#"<c:marker val="[^"]*"/>"#)?
                    .map_or("<c:marker val=\"1\"/>", |(a, b)| &old[a..b]);
                groups.push_str(&format!("<c:lineChart><c:grouping val=\"{grouping}\"/><c:varyColors val=\"0\"/>{series}{labels}{marker}{ids}</c:lineChart>"));
            }
        }
    }
    let (layout, rest) = area_remainder(inner, plots, &axes)?;
    Ok(splice(
        xml,
        a,
        b,
        &format!(
            "<c:plotArea>{layout}{groups}{}{}{sec_xml}{rest}</c:plotArea>",
            axes[cat].xml, axes[val].xml
        ),
    ))
}

fn synth_secondary(inner: &str) -> Result<(String, String, String), String> {
    let re = TryJsRegex::new(r#"<c:axId val="([0-9]+)"/>"#, "g").map_err(|e| e.to_string())?;
    let mut max = 0u64;
    for m in re.try_find_iter(inner) {
        let m = m.map_err(|e| e.to_string())?;
        if let Some(n) = m.get(1).and_then(|n| n.parse::<u64>().ok()) {
            max = max.max(n);
        }
    }
    let c = (max + 1).to_string();
    let v = (max + 2).to_string();
    let xml = format!(
        "<c:valAx><c:axId val=\"{v}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"r\"/><c:crossAx val=\"{c}\"/><c:crosses val=\"max\"/></c:valAx><c:catAx><c:axId val=\"{c}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"1\"/><c:axPos val=\"b\"/><c:crossAx val=\"{v}\"/></c:catAx>"
    );
    Ok((c, v, xml))
}

fn convert_type(xml: &str, target: &str) -> Result<String, String> {
    let plots = plots(xml)?;
    let first = plots
        .first()
        .ok_or("This chart type cannot be converted.")?;
    let combo = plots.len() > 1;
    if combo
        && plots
            .iter()
            .any(|p| !["barChart", "lineChart", "areaChart"].contains(&p.family.as_str()))
    {
        return Err("Only bar, line, and area combos can be converted.".into());
    }
    if !combo
        && ![
            "barChart",
            "lineChart",
            "areaChart",
            "pieChart",
            "doughnutChart",
        ]
        .contains(&first.family.as_str())
    {
        return Err(format!("Converting a {} is not supported.", first.family));
    }
    let target_family = match target {
        "column" | "bar" => "barChart",
        "line" => "lineChart",
        "area" => "areaChart",
        "pie" => "pieChart",
        "doughnut" => "doughnutChart",
        _ => return Err("Invalid chart type.".into()),
    };
    let from_pie = !combo && ["pieChart", "doughnutChart"].contains(&first.family.as_str());
    let to_pie = ["pieChart", "doughnutChart"].contains(&target_family);
    if !combo && first.family == "barChart" && target_family == "barChart" {
        if let Some((a, b)) = match_span(xml, r#"<c:barDir val="[^"]*"/>"#)? {
            return Ok(splice(
                xml,
                a,
                b,
                &format!(
                    "<c:barDir val=\"{}\"/>",
                    if target == "column" { "col" } else { "bar" }
                ),
            ));
        }
        return Ok(xml.into());
    }
    if !combo && first.family == target_family {
        return Ok(xml.into());
    }
    let inner = plots.iter().map(|p| p.inner.as_str()).collect::<String>();
    let re = TryJsRegex::new(r"<c:ser>[\s\S]*?</c:ser>", "g").map_err(|e| e.to_string())?;
    let mut series = String::new();
    let mut count = 0;
    for m in re.try_find_iter(&inner) {
        let m = m.map_err(|e| e.to_string())?;
        let mut s = m.as_str().to_owned();
        for p in [
            r#"<c:invertIfNegative val="[^"]*"/>"#,
            r"<c:marker>[\s\S]*?</c:marker>",
            r#"<c:smooth val="[^"]*"/>"#,
            r#"<c:shape val="[^"]*"/>"#,
            r#"<c:explosion val="[^"]*"/>"#,
        ] {
            s = remove_matches(&s, p)?;
        }
        if to_pie {
            s = remove_matches(&s, r"<c:trendline>[\s\S]*?</c:trendline>")?;
        }
        series.push_str(&s);
        count += 1;
    }
    if count == 0 {
        return Err("This chart has no series to convert.".into());
    }
    let mut parts = String::new();
    if target_family == "barChart" {
        parts.push_str(&format!(
            "<c:barDir val=\"{}\"/>",
            if target == "bar" { "bar" } else { "col" }
        ));
    }
    if !to_pie {
        let group = attribute_value(&inner, "grouping")?
            .filter(|g| g == "stacked" || g == "percentStacked")
            .unwrap_or_else(|| {
                if target_family == "barChart" {
                    "clustered".into()
                } else {
                    "standard".into()
                }
            });
        parts.push_str(&format!("<c:grouping val=\"{group}\"/>"));
    }
    parts.push_str(&format!(
        "<c:varyColors val=\"{}\"/>{series}",
        if to_pie { 1 } else { 0 }
    ));
    if let Some((a, b)) = match_span(&inner, r"</c:ser><c:dLbls>[\s\S]*?</c:dLbls>")? {
        parts.push_str(&remove_matches(
            &inner[a + "</c:ser>".len()..b],
            r#"<c:dLblPos val="[^"]*"/>"#,
        )?);
    }
    if target_family == "barChart" {
        parts.push_str("<c:gapWidth val=\"150\"/>");
    }
    if target_family == "lineChart" {
        parts.push_str("<c:marker val=\"1\"/>");
    }
    if to_pie {
        parts.push_str("<c:firstSliceAng val=\"0\"/>");
        if target_family == "doughnutChart" {
            parts.push_str("<c:holeSize val=\"50\"/>");
        }
        let rebuilt = replace_plots(
            xml,
            &plots,
            &format!("<c:{target_family}>{parts}</c:{target_family}>"),
        );
        return remove_matches(
            &rebuilt,
            r"<c:(catAx|valAx|dateAx|serAx)>[\s\S]*?</c:\1>|<c:(?:catAx|valAx|dateAx|serAx)/>",
        );
    }
    if !from_pie && !combo {
        let re = TryJsRegex::new(r#"<c:axId val="[0-9]+"/>"#, "g").map_err(|e| e.to_string())?;
        let mut count = 0;
        for m in re.try_find_iter(&inner) {
            parts.push_str(m.map_err(|e| e.to_string())?.as_str());
            count += 1;
        }
        if count == 0 {
            return Err("This chart has no axes to convert.".into());
        }
        return Ok(replace_plots(
            xml,
            &plots,
            &format!("<c:{target_family}>{parts}</c:{target_family}>"),
        ));
    }
    if combo {
        let (a, b) = match_span(xml, r"<c:plotArea>[\s\S]*?</c:plotArea>")?
            .ok_or("Chart part has no plot area.")?;
        let inner = &xml[a + "<c:plotArea>".len()..b - "</c:plotArea>".len()];
        if match_span(inner, r"<c:serAx[>/]")?.is_some() {
            return Err("This combo has series axes this conversion cannot keep.".into());
        }
        let axes = axis_matches(inner)?;
        let (cat, val, _, _, leftover) = axes_selection(&axes);
        let (Some(cat), Some(val)) = (cat, val) else {
            return Err("This combo has axes this conversion cannot keep.".into());
        };
        if !leftover.is_empty() || axes[cat].id.is_none() || axes[val].id.is_none() {
            return Err("This combo has axes this conversion cannot keep.".into());
        }
        let cid = axes[cat].id.as_deref().unwrap();
        let vid = axes[val].id.as_deref().unwrap();
        parts.push_str(&format!("<c:axId val=\"{cid}\"/><c:axId val=\"{vid}\"/>"));
        let retag = |s: &str, partner: &str| -> Result<String, String> {
            Ok(
                if let Some((a, b)) = match_span(s, r#"<c:crossAx val="[0-9]+"/>"#)? {
                    splice(s, a, b, &format!("<c:crossAx val=\"{partner}\"/>"))
                } else {
                    s.into()
                },
            )
        };
        let kept = format!(
            "{}{}",
            retag(&axes[cat].xml, vid)?,
            retag(&axes[val].xml, cid)?
        );
        let (layout, rest) = area_remainder(inner, &plots, &axes)?;
        return Ok(splice(
            xml,
            a,
            b,
            &format!(
                "<c:plotArea>{layout}<c:{target_family}>{parts}</c:{target_family}>{kept}{rest}</c:plotArea>"
            ),
        ));
    }
    let cid = "900000001";
    let vid = "900000002";
    parts.push_str(&format!("<c:axId val=\"{cid}\"/><c:axId val=\"{vid}\"/>"));
    let cat = if target == "bar" { "l" } else { "b" };
    let val = if target == "bar" { "b" } else { "l" };
    let axes = format!(
        "<c:catAx><c:axId val=\"{cid}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{cat}\"/><c:crossAx val=\"{vid}\"/></c:catAx><c:valAx><c:axId val=\"{vid}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{val}\"/><c:majorGridlines/><c:crossAx val=\"{cid}\"/></c:valAx>"
    );
    Ok(splice(
        xml,
        first.start,
        first.end,
        &format!("<c:{target_family}>{parts}</c:{target_family}>{axes}"),
    ))
}

fn apply_edit(xml: &str, edit: &serde_json::Value) -> Result<String, String> {
    let mut next = xml.to_owned();
    if let Some(value) = edit.get("seriesSet") {
        next = replace_series(&next, value.as_array().ok_or("Invalid chart series set.")?)?;
    }
    if let Some(value) = edit.get("chartType") {
        next = convert_type(&next, value.as_str().ok_or("Invalid chart type.")?)?;
    }
    if let Some(value) = edit.get("title") {
        next = set_title(&next, value.as_str().ok_or("Invalid chart title.")?)?;
    }
    if let Some(value) = edit.get("seriesColors") {
        for (index, color) in sorted_entries(value)? {
            next = set_series_color(
                &next,
                index,
                color.as_str().ok_or("Invalid chart series color.")?,
            )?;
        }
    }
    if let Some(value) = edit.get("pointColors") {
        for (index, points) in sorted_entries(value)? {
            next = set_point_colors(&next, index, points)?;
        }
    }
    if let Some(value) = edit.get("legend") {
        next = set_legend(&next, value.as_str().ok_or("Invalid chart legend.")?)?;
    }
    if let Some(value) = edit.get("dataLabels") {
        next = set_labels(&next, value.as_str().ok_or("Invalid chart labels.")?)?;
    }
    if edit.get("dataLabels").and_then(serde_json::Value::as_str) != Some("none")
        && (edit.get("dataLabelPosition").is_some() || edit.get("dataLabelFormat").is_some())
    {
        next = set_label_detail(
            &next,
            edit.get("dataLabelPosition")
                .and_then(serde_json::Value::as_str),
            edit.get("dataLabelFormat")
                .and_then(serde_json::Value::as_str),
        )?;
    }
    if let Some(titles) = edit.get("axisTitles") {
        for (key, kind) in [("category", "catAx"), ("value", "valAx")] {
            if let Some(title) = titles.get(key) {
                next = set_axis_title(&next, kind, title.as_str())?;
            }
        }
    }
    if let Some(value) = edit.get("grouping") {
        next = set_grouping(&next, value.as_str().ok_or("Invalid chart grouping.")?)?;
    }
    if let Some(value) = edit.get("gridlines") {
        next = set_gridlines(&next, value.as_bool().ok_or("Invalid chart gridlines.")?)?;
    }
    if let Some(value) = edit.get("valueAxis") {
        next = set_value_axis(&next, value)?;
    }
    if let Some(value) = edit.get("gapWidthPct") {
        next = set_gap_width(
            &next,
            &value
                .as_number()
                .ok_or("Invalid chart gap width.")?
                .to_string(),
        )?;
    }
    if let Some(value) = edit.get("holeSizePct") {
        next = set_hole_size(
            &next,
            &value
                .as_number()
                .ok_or("Invalid chart hole size.")?
                .to_string(),
        )?;
    }
    if let Some(value) = edit.get("explosionPct") {
        next = set_series_explosion(
            &next,
            &value
                .as_number()
                .ok_or("Invalid chart explosion.")?
                .to_string(),
        )?;
    }
    if let Some(value) = edit.get("pointExplosions") {
        next = set_point_explosions(&next, value)?;
    }
    if let Some(value) = edit.get("series") {
        for entry in value.as_array().ok_or("Invalid chart series.")? {
            next = set_series_data(&next, entry)?;
        }
    }
    Ok(next)
}

pub(super) fn apply(
    pkg: &mut super::package::PackageEditor<'_>,
    _state: &mut super::state::PlannerState,
    resolved: &serde_json::Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    if let Some(edits) = resolved
        .get("chartEdits")
        .and_then(serde_json::Value::as_array)
    {
        for edit in edits {
            let path = edit
                .get("chartPath")
                .and_then(serde_json::Value::as_str)
                .ok_or("Invalid chart path.")?;
            let xml = pkg.read_text(path)?;
            let next = apply_edit(&xml, edit)?;
            pkg.write(path, next)?;
            touched.insert(path.to_owned());
        }
    }
    Ok(())
}

pub(super) fn build_new_chart(chart: &serde_json::Value) -> Result<String, String> {
    let kind = chart
        .get("chartType")
        .and_then(serde_json::Value::as_str)
        .ok_or("Invalid chart type.")?;
    let entries = chart
        .get("series")
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid chart series.")?;
    let pie = kind == "pie" || kind == "doughnut";
    let labels_mode = chart.get("dataLabels").and_then(serde_json::Value::as_str);
    let position = chart
        .get("dataLabelPosition")
        .and_then(serde_json::Value::as_str);
    let fmt = chart
        .get("dataLabelFormat")
        .and_then(serde_json::Value::as_str);
    let labels = if labels_mode == Some("none")
        || (labels_mode.is_none() && position.is_none() && fmt.is_none())
    {
        String::new()
    } else {
        let mode = labels_mode.unwrap_or("value");
        let num = fmt
            .map(|s| {
                format!(
                    "<c:numFmt formatCode=\"{}\" sourceLinked=\"0\"/>",
                    escape_text(s)
                        .replace('"', "&quot;")
                        .replace('\'', "&apos;")
                )
            })
            .unwrap_or_default();
        let pos = position
            .map(|s| {
                format!(
                    "<c:dLblPos val=\"{}\"/>",
                    match s {
                        "center" => "ctr",
                        "inside-end" => "inEnd",
                        "outside-end" => "outEnd",
                        _ => s,
                    }
                )
            })
            .unwrap_or_default();
        format!(
            "<c:dLbls>{num}{pos}<c:showLegendKey val=\"0\"/><c:showVal val=\"{}\"/><c:showCatName val=\"{}\"/><c:showSerName val=\"0\"/><c:showPercent val=\"{}\"/><c:showBubbleSize val=\"0\"/></c:dLbls>",
            u8::from(mode == "value"),
            u8::from(mode == "category-percent"),
            u8::from(mode == "percent" || mode == "category-percent")
        )
    };
    let axis_title = |key: &str| {
        chart
            .get("axisTitles")
            .and_then(|t| t.get(key))
            .and_then(serde_json::Value::as_str)
            .map(|s| title_xml(&escape_text(s)))
            .unwrap_or_default()
    };
    let cat_title = axis_title("category");
    let val_title = axis_title("value");
    let bounds = chart.get("valueAxis");
    let max = bounds
        .and_then(|v| v.get("max"))
        .and_then(serde_json::Value::as_number)
        .map(|n| format!("<c:max val=\"{n}\"/>"))
        .unwrap_or_default();
    let min = bounds
        .and_then(|v| v.get("min"))
        .and_then(serde_json::Value::as_number)
        .map(|n| format!("<c:min val=\"{n}\"/>"))
        .unwrap_or_default();
    let scaling = format!("<c:scaling><c:orientation val=\"minMax\"/>{max}{min}</c:scaling>");
    let grid = if chart.get("gridlines").and_then(serde_json::Value::as_bool) == Some(true) {
        "<c:majorGridlines/>"
    } else {
        ""
    };
    let gap = chart
        .get("gapWidthPct")
        .and_then(serde_json::Value::as_number)
        .filter(|_| matches!(kind, "column" | "bar" | "combo"))
        .map(|n| format!("<c:gapWidth val=\"{n}\"/>"))
        .unwrap_or_default();
    let group = chart
        .get("grouping")
        .and_then(serde_json::Value::as_str)
        .filter(|s| *s == "stacked" || *s == "percentStacked");
    let overlap = if matches!(kind, "column" | "bar") && group.is_some() {
        "<c:overlap val=\"100\"/>"
    } else {
        ""
    };
    let cat_axis = format!(
        "<c:catAx><c:axId val=\"100000001\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"b\"/>{cat_title}<c:crossAx val=\"100000002\"/></c:catAx>"
    );
    let val_axis = format!(
        "<c:valAx><c:axId val=\"100000002\"/>{scaling}<c:delete val=\"0\"/><c:axPos val=\"l\"/>{grid}{val_title}<c:crossAx val=\"100000001\"/></c:valAx>"
    );
    let tagged = entries.iter().any(|e| e.get("plot").is_some());
    let placement = |e: &serde_json::Value, i: usize| {
        if tagged {
            (
                e.get("plot").and_then(serde_json::Value::as_str) == Some("lineChart"),
                e.get("secondaryAxis").and_then(serde_json::Value::as_bool) == Some(true),
            )
        } else {
            let last = entries.len() >= 2 && i == entries.len() - 1;
            (last, last)
        }
    };
    let plot = if kind == "combo" {
        let mut plots = String::new();
        let mut first = true;
        let mut secondary_used = false;
        for secondary in [false, true] {
            for line in [false, true] {
                let selected: Vec<_> = entries
                    .iter()
                    .enumerate()
                    .filter(|(i, e)| placement(e, *i) == (line, secondary))
                    .collect();
                if selected.is_empty() {
                    continue;
                }
                let series = selected
                    .into_iter()
                    .map(|(i, e)| build_added_series(e, i, if line { "line" } else { "column" }))
                    .collect::<Result<String, _>>()?;
                let labs = if first { labels.as_str() } else { "" };
                first = false;
                let (cid, vid) = if secondary {
                    secondary_used = true;
                    (100000003, 100000004)
                } else {
                    (100000001, 100000002)
                };
                if line {
                    plots.push_str(&format!("<c:lineChart><c:grouping val=\"standard\"/><c:varyColors val=\"0\"/>{series}{labs}<c:marker val=\"1\"/><c:axId val=\"{cid}\"/><c:axId val=\"{vid}\"/></c:lineChart>"));
                } else {
                    plots.push_str(&format!("<c:barChart><c:barDir val=\"col\"/><c:grouping val=\"clustered\"/><c:varyColors val=\"0\"/>{series}{labs}{gap}<c:axId val=\"{cid}\"/><c:axId val=\"{vid}\"/></c:barChart>"));
                }
            }
        }
        plots.push_str(&cat_axis);
        plots.push_str(&val_axis);
        if secondary_used {
            plots.push_str("<c:valAx><c:axId val=\"100000004\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"r\"/><c:crossAx val=\"100000003\"/><c:crosses val=\"max\"/></c:valAx><c:catAx><c:axId val=\"100000003\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"1\"/><c:axPos val=\"b\"/><c:crossAx val=\"100000004\"/></c:catAx>");
        }
        plots
    } else {
        let series = entries
            .iter()
            .enumerate()
            .map(|(i, e)| build_added_series(e, i, kind))
            .collect::<Result<String, _>>()?;
        let ids = "<c:axId val=\"100000001\"/><c:axId val=\"100000002\"/>";
        match kind {
            "pie" => format!("<c:pieChart><c:varyColors val=\"1\"/>{series}{labels}</c:pieChart>"),
            "doughnut" => format!(
                "<c:doughnutChart><c:varyColors val=\"1\"/>{series}{labels}<c:holeSize val=\"{}\"/></c:doughnutChart>",
                chart
                    .get("holeSizePct")
                    .and_then(serde_json::Value::as_number)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "50".into())
            ),
            "scatter" => format!(
                "<c:scatterChart><c:scatterStyle val=\"lineMarker\"/>{series}{labels}{ids}</c:scatterChart><c:valAx><c:axId val=\"100000001\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"b\"/>{cat_title}<c:crossAx val=\"100000002\"/></c:valAx>{val_axis}"
            ),
            _ => {
                let (family, header) = match kind {
                    "line" | "area" => (
                        if kind == "line" {
                            "lineChart"
                        } else {
                            "areaChart"
                        },
                        format!(
                            "<c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>",
                            group.unwrap_or("standard")
                        ),
                    ),
                    "radar" => (
                        "radarChart",
                        "<c:radarStyle val=\"marker\"/><c:varyColors val=\"0\"/>".into(),
                    ),
                    _ => (
                        "barChart",
                        format!(
                            "<c:barDir val=\"{}\"/><c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>",
                            if kind == "bar" { "bar" } else { "col" },
                            group.unwrap_or("clustered")
                        ),
                    ),
                };
                format!(
                    "<c:{family}>{header}{series}{labels}{gap}{overlap}{ids}</c:{family}>{cat_axis}{val_axis}"
                )
            }
        }
    };
    let legend = match chart.get("legend").and_then(serde_json::Value::as_str) {
        Some("none") => String::new(),
        Some(value) => format!(
            "<c:legend><c:legendPos val=\"{}\"/><c:overlay val=\"0\"/></c:legend>",
            match value {
                "right" => "r",
                "bottom" => "b",
                "top" => "t",
                "left" => "l",
                _ => value,
            }
        ),
        None if entries.len() > 1 || pie => {
            "<c:legend><c:legendPos val=\"b\"/><c:overlay val=\"0\"/></c:legend>".into()
        }
        None => String::new(),
    };
    let pivot = chart
        .get("pivotSourceName")
        .and_then(serde_json::Value::as_str);
    let source = pivot
        .map(|v| {
            format!(
                "<c:pivotSource><c:name>{}</c:name><c:fmtId val=\"0\"/></c:pivotSource>",
                escape_text(v)
            )
        })
        .unwrap_or_default();
    let pivot_fmts = if pivot.is_some() {
        "<c:pivotFmts><c:pivotFmt><c:idx val=\"0\"/></c:pivotFmt></c:pivotFmts>"
    } else {
        ""
    };
    let title = title_xml(&escape_text(
        chart
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(""),
    ));
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">{source}<c:chart>{pivot_fmts}{title}<c:autoTitleDeleted val=\"0\"/><c:plotArea><c:layout/>{plot}</c:plotArea>{legend}<c:plotVisOnly val=\"1\"/></c:chart></c:chartSpace>"
    ))
}

fn build_added_series(
    entry: &serde_json::Value,
    index: usize,
    kind: &str,
) -> Result<String, String> {
    const PALETTE: [&str; 10] = [
        "4472C4", "ED7D31", "A5A5A5", "FFC000", "5B9BD5", "70AD47", "264478", "9E480E", "636363",
        "997300",
    ];
    let pie = kind == "pie" || kind == "doughnut";
    let stroke = kind == "line" || kind == "radar" || kind == "scatter";
    let mut source = entry.clone();
    if !pie && source.get("color").is_none() {
        source["color"] = serde_json::Value::String(format!("#{}", PALETTE[index % PALETTE.len()]));
    }
    let mut result = build_series(&source, index, kind == "scatter")?;
    if stroke
        && let Some((a, b)) = match_span(
            &result,
            r"<c:spPr><a:solidFill>[\s\S]*?</a:solidFill></c:spPr>",
        )?
    {
            let inner = &result[a + "<c:spPr>".len()..b - "</c:spPr>".len()];
            result = splice(
                &result,
                a,
                b,
                &format!("<c:spPr><a:ln>{inner}</a:ln></c:spPr>"),
            );
        }
    if kind == "scatter" {
        result = result.replace("<c:smooth val=\"0\"/>", "");
    }
    if let Some(explosion) = entry
        .get("explosionPct")
        .filter(|_| pie)
        .and_then(serde_json::Value::as_number)
    {
        let at = match_span(&result, r"</c:spPr>")?
            .or(match_span(&result, r"</c:tx>")?)
            .map_or(0, |m| m.1);
        result = splice(
            &result,
            at,
            at,
            &format!("<c:explosion val=\"{explosion}\"/>"),
        );
    }
    if kind == "line" || kind == "radar" {
        let at = match_span(&result, r"<c:cat[>/]|<c:val[>/]")?
            .ok_or("Chart series has no data element to anchor a point.")?
            .0;
        result = splice(
            &result,
            at,
            at,
            &format!(
                "<c:marker><c:symbol val=\"{}\"/></c:marker>",
                if kind == "line" { "none" } else { "circle" }
            ),
        );
    }
    if pie {
        let len = entry
            .get("values")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        for i in 0..len {
            let color = entry
                .get("pointColors")
                .and_then(|v| v.get(i.to_string()))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("#{}", PALETTE[i % PALETTE.len()]));
            result = set_point_color(&result, i, &color)?;
        }
    } else if let Some(colors) = entry.get("pointColors") {
        for (i, color) in sorted_entries(colors)? {
            result = set_point_color(
                &result,
                i,
                color.as_str().ok_or("Invalid chart point color.")?,
            )?;
        }
    }
    if let Some(explosions) = entry.get("pointExplosions") {
        for (i, value) in sorted_entries(explosions)? {
            result = set_point_explosion(
                &result,
                i,
                &value
                    .as_number()
                    .ok_or("Invalid chart point explosion.")?
                    .to_string(),
            )?;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_chart_title_without_touching_axis_titles() {
        let input = "<c:chart><c:title><a:r><a:t>old</a:t></a:r><a:r><a:t>suffix</a:t></a:r></c:title><c:autoTitleDeleted val=\"1\"/><c:plotArea><c:valAx><c:title><a:t>axis</a:t></c:title></c:valAx></c:plotArea></c:chart>";
        let output = set_title(input, "A&B").unwrap();
        assert!(output.contains("<a:t>A&amp;B</a:t>"));
        assert!(output.contains("<a:t></a:t>"));
        assert!(output.contains("<c:valAx><c:title><a:t>axis</a:t>"));
        assert!(output.contains("<c:autoTitleDeleted val=\"0\"/>"));
    }
    #[test]
    fn inserts_title_and_rejects_missing_chart() {
        let result = set_title("<c:chart><c:plotArea/></c:chart>", "New").unwrap();
        assert!(result.contains("<c:chart><c:title>"));
        assert_eq!(
            set_title("<not-chart/>", "X").unwrap_err(),
            "Chart part has no c:chart element."
        );
    }
    #[test]
    fn chart_creation_and_ordered_edits_preserve_references() {
        let chart = serde_json::json!({"chartType":"column","title":"Sales","series":[{"name":"A","categories":["Q1","Q2"],"values":[2,3],"valuesRef":"Sheet1!$B$1:$B$2","categoriesRef":"Sheet1!$A$1:$A$2"}]});
        let original = build_new_chart(&chart).unwrap();
        let edit = serde_json::json!({"title":"Net","chartType":"line","series":[{"index":0,"values":[4,5]}],"gridlines":true,"legend":"bottom"});
        let changed = apply_edit(&original, &edit).unwrap();
        assert!(changed.contains("<c:lineChart>"));
        assert!(changed.contains("<a:t>Net</a:t>"));
        assert!(changed.contains("<c:f>Sheet1!$B$1:$B$2</c:f>"));
        assert!(changed.contains("<c:pt idx=\"1\"><c:v>5</c:v></c:pt>"));
        assert!(changed.contains("<c:majorGridlines/>"));
        assert!(changed.contains("<c:legendPos val=\"b\"/>"));
    }
    #[test]
    fn pie_creation_emits_point_colors_and_pivot_source() {
        let chart = serde_json::json!({"chartType":"pie","title":"P","pivotSourceName":"[]Sheet1!PivotTable1","series":[{"name":"X","categories":["a","b"],"values":[1,2],"pointColors":{"1":"#abcdef"},"pointExplosions":{"1":20}}]});
        let xml = build_new_chart(&chart).unwrap();
        assert!(xml.contains("<c:name>[]Sheet1!PivotTable1</c:name>"));
        assert!(xml.contains("<a:srgbClr val=\"ABCDEF\"/>"));
        assert!(xml.contains("<c:explosion val=\"20\"/>"));
    }
}
