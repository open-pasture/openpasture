//! A report as one CSV file: the title and header rows first, a blank line,
//! then each section (its title row, the column row with units in brackets,
//! the rows, the totals, a blank line), then the notes under "Notes".
//! Numbers are plain (no thousands separators) so spreadsheets read them.

use serde_json::Value;

use crate::ReportDoc;

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e15).map_or_else(|| n.to_string(), |f| format!("{f:.0}")),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// "Area (ac)".
pub fn column_heading(label: &str, unit: Option<&str>) -> String {
    match unit {
        Some(u) => format!("{label} ({u})"),
        None => label.to_owned(),
    }
}

pub fn write(doc: &ReportDoc) -> anyhow::Result<String> {
    let mut w = ::csv::WriterBuilder::new().flexible(true).from_writer(Vec::new());
    let blank: [&str; 1] = [""];
    w.write_record([doc.title.as_str()])?;
    for (k, v) in &doc.header {
        w.write_record([k, v])?;
    }
    w.write_record(blank)?;
    for s in &doc.sections {
        w.write_record([s.title.as_str()])?;
        w.write_record(s.columns.iter().map(|c| column_heading(&c.label, c.unit.as_deref())))?;
        for r in s.rows.iter().chain(s.totals.iter()) {
            w.write_record(r.iter().map(cell))?;
        }
        w.write_record(blank)?;
    }
    if !doc.notes.is_empty() {
        w.write_record(["Notes"])?;
        for n in &doc.notes {
            w.write_record([n])?;
        }
    }
    Ok(String::from_utf8(w.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?)?)
}
