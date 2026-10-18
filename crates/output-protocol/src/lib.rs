//! Validated, channel-neutral structured output carried in Markdown fences.
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    #[default]
    Text,
    Number,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableColumn {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub kind: ColumnKind,
}
fn default_page_size() -> u8 {
    5
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTable {
    #[serde(default)]
    pub title: String,
    pub columns: Vec<TableColumn>,
    pub rows: Vec<BTreeMap<String, serde_json::Value>>,
    #[serde(default = "default_page_size")]
    pub page_size: u8,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartPoint {
    pub label: String,
    pub value: serde_json::Number,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputChart {
    #[serde(default)]
    pub title: String,
    pub kind: ChartKind,
    pub points: Vec<ChartPoint>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredOutput {
    Table(OutputTable),
    Chart(OutputChart),
}
#[derive(Debug, Clone)]
pub struct StructuredSpan {
    pub start: usize,
    pub end: usize,
    pub result: Result<StructuredOutput, String>,
}

impl OutputTable {
    pub fn validate(&self) -> Result<(), String> {
        if self.columns.is_empty() || self.columns.len() > 20 || self.rows.len() > 100 {
            return Err("table requires 1-20 columns and at most 100 rows".into());
        }
        if !(1..=10).contains(&self.page_size) {
            return Err("page_size must be 1-10".into());
        }
        let mut ids = HashSet::new();
        for column in &self.columns {
            if matches!(
                column.id.as_str(),
                "__proto__" | "constructor" | "prototype"
            ) || column.id.is_empty()
                || column.id.len() > 40
                || !column
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !ids.insert(&column.id)
            {
                return Err(
                    "column ids must be unique ASCII letters, digits or underscores (1-40 bytes)"
                        .into(),
                );
            }
            if column.label.is_empty() || column.label.len() > 200 {
                return Err("column labels require 1-200 bytes".into());
            }
        }
        for row in &self.rows {
            if row.keys().any(|key| !ids.contains(key)) {
                return Err("row contains an unknown column".into());
            }
            for column in &self.columns {
                if let Some(value) = row.get(&column.id) {
                    if value.is_array() || value.is_object() {
                        return Err("table cells must be scalar values".into());
                    }
                    if column.kind == ColumnKind::Number && !value.is_null() && !value.is_number() {
                        return Err("number columns require numeric cells".into());
                    }
                }
            }
        }
        Ok(())
    }
}
impl OutputChart {
    pub fn validate(&self) -> Result<(), String> {
        if self.points.is_empty() || self.points.len() > 200 {
            return Err("chart requires 1-200 points".into());
        }
        let mut keys = HashSet::new();
        let mut positive = false;
        for point in &self.points {
            if point.label.is_empty()
                || point.label.len() > 200
                || point
                    .series
                    .as_ref()
                    .is_some_and(|s| s.is_empty() || s.len() > 200)
            {
                return Err("chart labels and series require 1-200 bytes".into());
            }
            if !keys.insert((&point.label, &point.series)) {
                return Err("chart label/series pairs must be unique".into());
            }
            let value = point.value.as_f64().ok_or("chart value must be finite")?;
            if !value.is_finite() {
                return Err("chart value must be finite".into());
            }
            if self.kind == ChartKind::Pie && (value < 0.0 || point.series.is_some()) {
                return Err("pie charts require nonnegative values without series".into());
            }
            positive |= value > 0.0;
        }
        if self.kind == ChartKind::Pie && !positive {
            return Err("pie charts need a positive value".into());
        }
        Ok(())
    }
}
impl StructuredOutput {
    pub fn parse(language: &str, json: &str) -> Result<Self, String> {
        if json.len() > 16_384 {
            return Err("structured output block exceeds 16 KiB".into());
        }
        match language {
            "remi-table" => {
                let table: OutputTable = serde_json::from_str(json).map_err(|e| e.to_string())?;
                table.validate()?;
                Ok(Self::Table(table))
            }
            "remi-chart" => {
                let chart: OutputChart = serde_json::from_str(json).map_err(|e| e.to_string())?;
                chart.validate()?;
                Ok(Self::Chart(chart))
            }
            _ => Err("unknown structured output block".into()),
        }
    }
    pub fn markdown(&self) -> String {
        fn cell(value: &str) -> String {
            value
                .replace('\\', "\\\\")
                .replace('|', "\\|")
                .replace(['\r', '\n'], " ")
        }
        let (title, labels, numeric, rows) = match self {
            Self::Table(table) => (
                table.title.as_str(),
                table
                    .columns
                    .iter()
                    .map(|c| c.label.clone())
                    .collect::<Vec<_>>(),
                table
                    .columns
                    .iter()
                    .map(|c| c.kind == ColumnKind::Number)
                    .collect::<Vec<_>>(),
                table
                    .rows
                    .iter()
                    .map(|row| {
                        table
                            .columns
                            .iter()
                            .map(|column| {
                                row.get(&column.id)
                                    .map(|v| {
                                        v.as_str().map(str::to_string).unwrap_or_else(|| {
                                            if v.is_null() {
                                                String::new()
                                            } else {
                                                v.to_string()
                                            }
                                        })
                                    })
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
            ),
            Self::Chart(chart) => (
                chart.title.as_str(),
                vec!["Category".into(), "Series".into(), "Value".into()],
                vec![false, false, true],
                chart
                    .points
                    .iter()
                    .map(|p| {
                        vec![
                            p.label.clone(),
                            p.series.clone().unwrap_or_default(),
                            p.value.to_string(),
                        ]
                    })
                    .collect(),
            ),
        };
        let mut result = if title.is_empty() {
            String::new()
        } else {
            format!("**{}**\n\n", cell(title))
        };
        result.push_str(&format!(
            "| {} |\n| {} |\n",
            labels
                .iter()
                .map(|s| cell(s))
                .collect::<Vec<_>>()
                .join(" | "),
            numeric
                .iter()
                .map(|n| if *n { "---:" } else { "---" })
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        for row in rows {
            result.push_str(&format!(
                "| {} |\n",
                row.iter().map(|s| cell(s)).collect::<Vec<_>>().join(" | ")
            ));
        }
        result
    }
}

pub fn structured_spans(text: &str) -> Vec<StructuredSpan> {
    let mut pending = None::<(usize, String, String)>;
    let mut spans = Vec::new();
    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if matches!(info.trim(), "remi-table" | "remi-chart") =>
            {
                pending = Some((range.start, info.trim().to_string(), String::new()))
            }
            Event::Text(value) if pending.is_some() => pending.as_mut().unwrap().2.push_str(&value),
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, language, value)) = pending.take() {
                    spans.push(StructuredSpan {
                        start,
                        end: range.end,
                        result: StructuredOutput::parse(&language, &value),
                    });
                }
            }
            _ => {}
        }
    }
    spans
}
pub fn render_structured_fallbacks(text: &str) -> String {
    let mut result = text.to_string();
    for span in structured_spans(text).into_iter().rev() {
        if let Ok(output) = span.result {
            result.replace_range(span.start..span.end, &output.markdown());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tables_keep_numbers_and_fall_back_without_losing_rows() {
        let output = StructuredOutput::parse("remi-table", r#"{"columns":[{"id":"name","label":"Name"},{"id":"count","label":"Count","kind":"number"}],"rows":[{"name":"A|B","count":7},{"name":"next\nline","count":null}]}"#).unwrap();
        let markdown = output.markdown();
        assert!(markdown.contains("A\\|B | 7"));
        assert!(markdown.contains("next line"));
        assert!(markdown.contains("---:"));
        let source = format!(
            "Before\n\n```remi-table\n{}\n```\nAfter",
            match &output {
                StructuredOutput::Table(table) => serde_json::to_string(table).unwrap(),
                _ => unreachable!(),
            }
        );
        let rendered = render_structured_fallbacks(&source);
        assert!(
            rendered.contains("Before") && rendered.contains("After") && rendered.contains("A\\|B")
        );
        assert!(!rendered.contains("remi-table"));
    }
    #[test]
    fn invalid_shapes_and_vendor_scripts_are_rejected() {
        for json in [
            r#"{"columns":[{"id":"x","label":"X","kind":"number"}],"rows":[{"x":"not a number"}]}"#,
            r#"{"columns":[{"id":"x","label":"X"},{"id":"x","label":"Duplicate"}],"rows":[]}"#,
            r#"{"columns":[{"id":"x","label":"X"}],"rows":[{"extra":1}]}"#,
            r#"{"columns":[{"id":"x","label":"X"}],"rows":[],"callback":"javascript"}"#,
        ] {
            assert!(
                StructuredOutput::parse("remi-table", json).is_err(),
                "{json}"
            );
        }
        for json in [
            r#"{"kind":"pie","points":[{"label":"A","value":-1}]}"#,
            r#"{"kind":"pie","points":[{"label":"A","value":0}]}"#,
            r#"{"kind":"bar","points":[{"label":"A","value":1},{"label":"A","value":2}]}"#,
            r#"{"kind":"line","points":[{"label":"A","value":1}],"formatter":"function(){}"}"#,
        ] {
            assert!(
                StructuredOutput::parse("remi-chart", json).is_err(),
                "{json}"
            );
        }
        assert!(StructuredOutput::parse("remi-chart", &" ".repeat(16_385)).is_err());
    }
    #[test]
    fn chart_series_are_data_and_code_examples_are_not_executed() {
        let json = r#"{"kind":"line","points":[{"label":"A","value":2,"series":"one"},{"label":"A","value":3,"series":"two"}]}"#;
        assert!(StructuredOutput::parse("remi-chart", json)
            .unwrap()
            .markdown()
            .contains("A | two | 3"));
        let example = format!("````markdown\n```remi-chart\n{json}\n```\n````");
        assert!(structured_spans(&example).is_empty());
        assert_eq!(render_structured_fallbacks(&example), example);
        let invalid = "```remi-chart\nnot JSON\n```";
        assert!(structured_spans(invalid)[0].result.is_err());
        assert_eq!(render_structured_fallbacks(invalid), invalid);
    }
}
