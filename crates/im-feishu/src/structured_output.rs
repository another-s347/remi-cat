//! Feishu Card 2.0 projection for the shared structured output contract.
use remi_output_protocol::{ChartKind, ColumnKind, StructuredOutput};
use serde_json::{json, Value};

pub fn card_elements(output: &StructuredOutput) -> Vec<Value> {
    let mut elements = Vec::new();
    let title = match output {
        StructuredOutput::Table(table) => &table.title,
        StructuredOutput::Chart(chart) => &chart.title,
    };
    if !title.is_empty() {
        elements.push(json!({"tag": "markdown", "content": format!("**{}**", title.replace(['\r', '\n'], " "))}));
    }
    elements.push(match output {
        StructuredOutput::Table(table) => {
            let columns = table.columns.iter().map(|column| json!({
                "name": column.id, "display_name": column.label,
                "data_type": match column.kind { ColumnKind::Text => "text", ColumnKind::Number => "number" }, "width": "auto"
            })).collect::<Vec<_>>();
            let rows = table.rows.iter().map(|row| {
                let mut result = serde_json::Map::new();
                for column in &table.columns {
                    if let Some(value) = row.get(&column.id).filter(|v| !v.is_null()) {
                        let value = if column.kind == ColumnKind::Number { value.clone() } else { Value::String(value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())) };
                        result.insert(column.id.clone(), value);
                    }
                }
                Value::Object(result)
            }).collect::<Vec<_>>();
            json!({"tag": "table", "page_size": table.page_size, "row_height": "low", "columns": columns, "rows": rows})
        }
        StructuredOutput::Chart(chart) => {
            let values = chart.points.iter().map(|point| json!({"label": point.label, "value": point.value, "series": point.series.as_deref().unwrap_or("")})).collect::<Vec<_>>();
            let mut spec = json!({"type": match chart.kind { ChartKind::Bar => "bar", ChartKind::Line => "line", ChartKind::Pie => "pie" }, "data": [{"id": "data", "values": values}]});
            if chart.kind == ChartKind::Pie {
                spec["valueField"] = "value".into(); spec["categoryField"] = "label".into();
            } else {
                spec["xField"] = "label".into(); spec["yField"] = "value".into();
                if chart.points.iter().any(|point| point.series.is_some()) {
                    spec["seriesField"] = "series".into();
                    if chart.kind == ChartKind::Bar { spec["xField"] = json!(["label", "series"]); spec["stack"] = false.into(); }
                }
            }
            json!({"tag": "chart", "chart_spec": spec, "aspect_ratio": "16:9", "preview": true})
        }
    });
    elements
}

/// Recover displayed data for quoting a generated native table or chart.
pub(crate) fn component_markdown(component: &Value) -> Option<String> {
    match component.get("tag")?.as_str()? {
        "table" => {
            let columns = component.get("columns")?.as_array()?;
            let rows = component.get("rows")?.as_array()?;
            let neutral = json!({"columns": columns.iter().map(|column| json!({"id": column["name"], "label": column.get("display_name").unwrap_or(&column["name"]), "kind": if column["data_type"] == "number" { "number" } else { "text" }})).collect::<Vec<_>>(), "rows": rows});
            StructuredOutput::parse("remi-table", &neutral.to_string())
                .ok()
                .map(|output| output.markdown())
        }
        "chart" => {
            let raw = component.get("chart_spec")?;
            let spec = if let Some(text) = raw.as_str() {
                serde_json::from_str::<Value>(text).ok()?
            } else {
                raw.clone()
            };
            let data = spec.get("data")?;
            let dataset = data
                .as_array()
                .and_then(|datasets| datasets.first())
                .unwrap_or(data);
            let values = dataset.get("values")?.as_array()?;
            let points = values
                .iter()
                .map(|point| {
                    let mut result = json!({"label": point["label"], "value": point["value"]});
                    if let Some(series) = point
                        .get("series")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                    {
                        result["series"] = series.into();
                    }
                    result
                })
                .collect::<Vec<_>>();
            let neutral = json!({"kind": spec["type"], "points": points});
            StructuredOutput::parse("remi-chart", &neutral.to_string())
                .ok()
                .map(|output| {
                    format!(
                        "图表（{}）\n\n{}",
                        spec["type"].as_str().unwrap_or("chart"),
                        output.markdown()
                    )
                })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_tables_and_all_chart_types_roundtrip_through_native_elements() {
        let table = StructuredOutput::parse("remi-table", r#"{"columns":[{"id":"name","label":"Name"},{"id":"count","label":"Count","kind":"number"}],"rows":[{"name":"A","count":7},{"name":"B","count":11}]}"#).unwrap();
        let native = card_elements(&table);
        assert_eq!(native[0]["tag"], "table");
        assert_eq!(native[0]["rows"][0]["count"], 7);
        assert!(component_markdown(&native[0]).unwrap().contains("B | 11"));
        for kind in ["bar", "line", "pie"] {
            let chart = StructuredOutput::parse("remi-chart", &format!(r#"{{"kind":"{kind}","points":[{{"label":"A","value":7}},{{"label":"B","value":11}}]}}"#)).unwrap();
            let native = card_elements(&chart);
            assert_eq!(native[0]["chart_spec"]["type"], kind);
            assert!(component_markdown(&native[0])
                .unwrap()
                .contains("B |  | 11"));
        }
    }
    #[test]
    fn grouped_bars_keep_series_in_both_position_and_color_mapping() {
        let chart = StructuredOutput::parse("remi-chart", r#"{"kind":"bar","points":[{"label":"A","value":7,"series":"one"},{"label":"A","value":11,"series":"two"}]}"#).unwrap();
        let native = card_elements(&chart);
        assert_eq!(
            native[0]["chart_spec"]["xField"],
            json!(["label", "series"])
        );
        assert_eq!(native[0]["chart_spec"]["seriesField"], "series");
        assert!(component_markdown(&native[0])
            .unwrap()
            .contains("A | two | 11"));
    }

    #[test]
    fn mixed_card_preserves_order_and_degrades_excess_tables() {
        let table = "```remi-table\n{\"columns\":[{\"id\":\"x\",\"label\":\"X\"}],\"rows\":[{\"x\":\"A\"}]}\n```\n";
        let chart =
            "```remi-chart\n{\"kind\":\"bar\",\"points\":[{\"label\":\"A\",\"value\":7}]}\n```\n";
        let source = format!("before\n{table}middle\n![image](img_v3_example)\n{chart}after");
        let card: Value = serde_json::from_str(&crate::client::card_content(&source)).unwrap();
        let tags = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| element["tag"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            tags,
            ["markdown", "table", "markdown", "img", "chart", "markdown"]
        );
        let card: Value =
            serde_json::from_str(&crate::client::card_content(&table.repeat(6))).unwrap();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements
                .iter()
                .filter(|element| element["tag"] == "table")
                .count(),
            5
        );
        assert!(elements.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("| A |"));
    }
}
