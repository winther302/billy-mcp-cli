//! Filters read records and write records. None of them talk to Billy.

use std::io::Write;

use anyhow::{anyhow, bail, Result};
use clap::Args;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use crate::record::{self, field};

#[derive(Args, Debug)]
pub struct WhereArgs {
    /// Conditions that must all hold: field=value, field!=value, field>n, field>=n,
    /// field<n, field<=n, field~text (case-insensitive contains). Numbers compare
    /// numerically, everything else as text (so dates work: entryDate>=2025-07-01).
    #[arg(required = true)]
    conditions: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PickArgs {
    /// Fields to keep, comma- or space-separated; dotted paths reach nested fields
    #[arg(required = true, value_delimiter = ',')]
    fields: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SortArgs {
    /// Field to sort by
    field: String,
    /// Largest first
    #[arg(short, long)]
    reverse: bool,
}

#[derive(Args, Debug)]
pub struct HeadArgs {
    /// Number of records to keep
    #[arg(short = 'n', long = "lines", default_value_t = 10)]
    n: usize,
}

#[derive(Args, Debug)]
pub struct CountArgs {
    /// Count per value of this field
    #[arg(long)]
    by: Option<String>,
}

#[derive(Args, Debug)]
pub struct SumArgs {
    /// Numeric field to add up
    field: String,
    /// Sum per value of this field
    #[arg(long)]
    by: Option<String>,
}

#[derive(Args, Debug)]
pub struct TableArgs {
    /// Maximum column width; longer values are cut with …
    #[arg(long, default_value_t = 40)]
    width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
}

#[derive(Debug)]
struct Condition {
    field: String,
    op: Op,
    value: String,
}

impl Condition {
    fn parse(s: &str) -> Result<Self> {
        let idx = s.find(['=', '!', '<', '>', '~']).ok_or_else(|| {
            anyhow!("condition `{s}` has no operator; use = != > >= < <= or ~, e.g. 'amount>1000'")
        })?;
        let (field, rest) = s.split_at(idx);
        let (op, len) = match rest {
            r if r.starts_with(">=") => (Op::Ge, 2),
            r if r.starts_with("<=") => (Op::Le, 2),
            r if r.starts_with("!=") => (Op::Ne, 2),
            r if r.starts_with('=') => (Op::Eq, 1),
            r if r.starts_with('>') => (Op::Gt, 1),
            r if r.starts_with('<') => (Op::Lt, 1),
            r if r.starts_with('~') => (Op::Contains, 1),
            _ => bail!("condition `{s}`: `!` must be followed by `=`"),
        };
        if field.is_empty() {
            bail!("condition `{s}` has no field name");
        }
        Ok(Self {
            field: field.to_string(),
            op,
            value: rest[len..].to_string(),
        })
    }

    fn matches(&self, rec: &Value) -> bool {
        let value = field(rec, &self.field).filter(|v| !v.is_null());
        let text = value.map(record::text).unwrap_or_default();
        let numbers = value
            .and_then(record::decimal)
            .zip(record::parse_decimal(&self.value));
        let equal = match numbers {
            Some((a, b)) => a == b,
            None => text == self.value,
        };
        let order = || match numbers {
            Some((a, b)) => Some(a.cmp(&b)),
            None => value.map(|_| text.as_str().cmp(self.value.as_str())),
        };
        match self.op {
            Op::Eq => equal,
            Op::Ne => !equal,
            Op::Contains => text.to_lowercase().contains(&self.value.to_lowercase()),
            Op::Gt => order().is_some_and(|o| o.is_gt()),
            Op::Ge => order().is_some_and(|o| o.is_ge()),
            Op::Lt => order().is_some_and(|o| o.is_lt()),
            Op::Le => order().is_some_and(|o| o.is_le()),
        }
    }
}

pub fn where_(a: WhereArgs, records: Vec<Value>) -> Result<Vec<Value>> {
    let conditions = a
        .conditions
        .iter()
        .map(|c| Condition::parse(c))
        .collect::<Result<Vec<_>>>()?;
    Ok(records
        .into_iter()
        .filter(|r| conditions.iter().all(|c| c.matches(r)))
        .collect())
}

pub fn pick(a: PickArgs, records: Vec<Value>) -> Vec<Value> {
    let fields: Vec<&str> = a.fields.iter().flat_map(|f| f.split_whitespace()).collect();
    records
        .iter()
        .map(|r| {
            let map: Map<String, Value> = fields
                .iter()
                .map(|&f| (f.to_string(), field(r, f).cloned().unwrap_or(Value::Null)))
                .collect();
            Value::Object(map)
        })
        .collect()
}

pub fn sort(a: SortArgs, mut records: Vec<Value>) -> Vec<Value> {
    records.sort_by(|x, y| {
        let (x, y) = if a.reverse { (y, x) } else { (x, y) };
        record::compare(field(x, &a.field), field(y, &a.field))
    });
    if a.reverse {
        // Keep missing values last in both directions.
        let (present, missing): (Vec<_>, Vec<_>) = records
            .into_iter()
            .partition(|r| field(r, &a.field).is_some_and(|v| !v.is_null()));
        records = present.into_iter().chain(missing).collect();
    }
    records
}

pub fn head(a: HeadArgs, mut records: Vec<Value>) -> Vec<Value> {
    records.truncate(a.n);
    records
}

pub fn count(a: CountArgs, records: Vec<Value>) -> Vec<Value> {
    let Some(by) = a.by else {
        return vec![json!({ "count": records.len() })];
    };
    let mut groups: Vec<(Value, usize)> = Vec::new();
    for r in &records {
        let key = field(r, &by).cloned().unwrap_or(Value::Null);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => groups.push((key, 1)),
        }
    }
    groups
        .into_iter()
        .map(|(key, n)| json!({ by.as_str(): key, "count": n }))
        .collect()
}

pub fn sum(a: SumArgs, records: Vec<Value>) -> Result<Vec<Value>> {
    let mut groups: Vec<(Value, Decimal, usize)> = Vec::new();
    for (i, r) in records.iter().enumerate() {
        let Some(value) = field(r, &a.field).filter(|v| !v.is_null()) else {
            continue;
        };
        let amount = record::decimal(value)
            .ok_or_else(|| anyhow!("`{}` in record {} is not a number: {value}", a.field, i + 1))?;
        let key = match &a.by {
            Some(by) => field(r, by).cloned().unwrap_or(Value::Null),
            None => Value::Null,
        };
        match groups.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, total, n)) => {
                *total += amount;
                *n += 1;
            }
            None => groups.push((key, amount, 1)),
        }
    }
    if groups.is_empty() && a.by.is_none() {
        groups.push((Value::Null, Decimal::ZERO, 0));
    }
    Ok(groups
        .into_iter()
        .map(|(key, total, n)| {
            let mut map = Map::new();
            if let Some(by) = &a.by {
                map.insert(by.clone(), key);
            }
            map.insert("sum".into(), record::number(total));
            map.insert("count".into(), json!(n));
            Value::Object(map)
        })
        .collect())
}

pub fn table(a: TableArgs, records: &[Value], out: &mut dyn Write) -> Result<()> {
    if records.is_empty() {
        writeln!(out, "(no records)")?;
        return Ok(());
    }
    let mut columns: Vec<String> = Vec::new();
    for r in records {
        match r {
            Value::Object(map) => {
                for key in map.keys() {
                    if !columns.contains(key) {
                        columns.push(key.clone());
                    }
                }
            }
            _ if !columns.iter().any(|c| c == "value") => columns.push("value".into()),
            _ => {}
        }
    }
    let cell = |r: &Value, col: &str| -> Option<Value> {
        match r {
            Value::Object(map) => map.get(col).cloned(),
            other if col == "value" => Some(other.clone()),
            _ => None,
        }
    };
    let width = a.width.max(3);
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|c| {
                    clip(
                        &cell(r, c).map(|v| record::text(&v)).unwrap_or_default(),
                        width,
                    )
                })
                .collect()
        })
        .collect();
    let numeric: Vec<bool> = columns
        .iter()
        .map(|c| {
            records
                .iter()
                .filter_map(|r| cell(r, c).filter(|v| !v.is_null()))
                .all(|v| v.is_number())
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([c.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();

    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let pad = widths[i] - s.chars().count();
                if numeric[i] {
                    format!("{}{s}", " ".repeat(pad))
                } else {
                    format!("{s}{}", " ".repeat(pad))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    writeln!(out, "{}", line(&columns))?;
    writeln!(
        out,
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    )?;
    for row in &rows {
        writeln!(out, "{}", line(row))?;
    }
    writeln!(
        out,
        "({} {})",
        rows.len(),
        if rows.len() == 1 { "row" } else { "rows" }
    )?;
    Ok(())
}

fn clip(s: &str, width: usize) -> String {
    let s = s.replace(['\n', '\t'], " ");
    if s.chars().count() <= width {
        return s;
    }
    let mut cut: String = s.chars().take(width - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs() -> Vec<Value> {
        vec![
            json!({"no": 1310, "name": "Bank", "amount": 100.50, "date": "2025-03-01"}),
            json!({"no": 1310, "name": "Bank", "amount": -20.25, "date": "2025-08-01"}),
            json!({"no": 5820, "name": "Husleje", "amount": 12500, "date": "2025-08-15"}),
        ]
    }

    fn filter(conds: &[&str]) -> usize {
        let a = WhereArgs {
            conditions: conds.iter().map(|s| s.to_string()).collect(),
        };
        where_(a, recs()).unwrap().len()
    }

    #[test]
    fn where_compares_numbers_text_and_dates() {
        assert_eq!(filter(&["no=1310"]), 2);
        assert_eq!(filter(&["amount>100"]), 2);
        assert_eq!(filter(&["amount=12500.00"]), 1);
        assert_eq!(filter(&["date>=2025-08-01"]), 2);
        assert_eq!(filter(&["name~HUS"]), 1);
        assert_eq!(filter(&["no!=1310", "amount<=12500"]), 1);
        assert_eq!(filter(&["missing>1"]), 0);
    }

    #[test]
    fn where_rejects_bad_conditions() {
        assert!(Condition::parse("amount").is_err());
        assert!(Condition::parse("=5").is_err());
        assert!(Condition::parse("a!5").is_err());
    }

    #[test]
    fn sum_is_exact_per_group() {
        let a = SumArgs {
            field: "amount".into(),
            by: Some("no".into()),
        };
        let out = sum(a, recs()).unwrap();
        assert_eq!(out[0].to_string(), r#"{"no":1310,"sum":80.25,"count":2}"#);
        assert_eq!(out[1].to_string(), r#"{"no":5820,"sum":12500,"count":1}"#);
    }

    #[test]
    fn sort_reverse_keeps_missing_last() {
        let mut input = recs();
        input.push(json!({"name": "no amount"}));
        let out = sort(
            SortArgs {
                field: "amount".into(),
                reverse: true,
            },
            input,
        );
        let names: Vec<String> = out.iter().map(|r| record::text(&r["name"])).collect();
        assert_eq!(names, ["Husleje", "Bank", "Bank", "no amount"]);
    }
}
