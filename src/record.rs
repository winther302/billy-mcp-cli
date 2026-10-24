//! NDJSON records: one JSON object per line is the only data format passed
//! between commands.

use std::cmp::Ordering;
use std::io::{BufRead, Write};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use serde_json::Value;

pub fn read_ndjson<R: BufRead>(reader: R) -> Result<Vec<Value>> {
    let mut records = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.context("reading input")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) => bail!(
                "input line {} is not a JSON record: {}",
                i + 1,
                line.chars().take(80).collect::<String>()
            ),
        }
    }
    Ok(records)
}

pub fn emit(out: &mut dyn Write, records: &[Value]) -> Result<()> {
    for record in records {
        writeln!(out, "{}", serde_json::to_string(record)?)?;
    }
    Ok(())
}

/// Looks up a dotted path such as `contact.name` or `lines.0.amount`.
pub fn field<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(record, |cur, part| match cur {
        Value::Object(map) => map.get(part),
        Value::Array(items) => items.get(part.parse::<usize>().ok()?),
        _ => None,
    })
}

/// A value as plain text: strings unquoted, null empty.
pub fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(n) => parse_decimal(&n.to_string()),
        Value::String(s) => parse_decimal(s),
        _ => None,
    }
}

pub fn parse_decimal(s: &str) -> Option<Decimal> {
    let s = s.trim();
    Decimal::from_str(s)
        .or_else(|_| Decimal::from_scientific(s))
        .ok()
}

/// A decimal as an exact JSON number (relies on serde_json's arbitrary_precision).
pub fn number(d: Decimal) -> Value {
    serde_json::from_str(&d.to_string()).expect("a decimal is valid JSON")
}

/// Numeric order when both sides are numbers, text order otherwise; missing
/// and null values sort last.
pub fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    let a = a.filter(|v| !v.is_null());
    let b = b.filter(|v| !v.is_null());
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => match (decimal(a), decimal(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => text(a).cmp(&text(b)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_follows_dotted_paths() {
        let r = json!({"contact": {"name": "Acme ApS"}, "lines": [{"amount": 5}]});
        assert_eq!(field(&r, "contact.name"), Some(&json!("Acme ApS")));
        assert_eq!(field(&r, "lines.0.amount"), Some(&json!(5)));
        assert_eq!(field(&r, "contact.missing"), None);
    }

    #[test]
    fn compare_is_numeric_for_numbers() {
        assert_eq!(compare(Some(&json!(9)), Some(&json!(10))), Ordering::Less);
        assert_eq!(
            compare(Some(&json!("b")), Some(&json!("a"))),
            Ordering::Greater
        );
        assert_eq!(compare(None, Some(&json!(1))), Ordering::Greater);
    }

    #[test]
    fn numbers_stay_exact() {
        let v: Value = serde_json::from_str("12500.10").unwrap();
        assert_eq!(
            number(decimal(&v).unwrap() * Decimal::from(3)).to_string(),
            "37500.30"
        );
    }
}
