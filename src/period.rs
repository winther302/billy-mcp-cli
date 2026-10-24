//! One period syntax for every command. Billy's own `entryDatePeriod` values
//! are spelled differently per resource, so we always send explicit
//! minEntryDate/maxEntryDate instead.

use anyhow::{anyhow, bail, Result};

#[derive(Debug, PartialEq)]
pub struct Range {
    pub from: String,
    pub to: String,
}

/// Parses 2025, 2025-Q3, 2025-H1, 2025-03, 2025-03-14 or 2025-01-01..2025-06-30.
pub fn parse(spec: &str) -> Result<Range> {
    let s = spec.trim().to_ascii_uppercase();
    if let Some((from, to)) = s.split_once("..") {
        let range = Range {
            from: date(from)?,
            to: date(to)?,
        };
        if range.from > range.to {
            bail!("period `{spec}` ends before it starts");
        }
        return Ok(range);
    }
    let bad = || {
        anyhow!("unknown period `{spec}`; use 2025, 2025-Q3, 2025-H1, 2025-03, 2025-03-14 or 2025-01-01..2025-06-30")
    };
    let parts: Vec<&str> = s.split('-').collect();
    let year = year(parts.first().copied().unwrap_or_default()).ok_or_else(bad)?;
    let (first_month, last_month) = match parts.as_slice() {
        [_] => (1, 12),
        [_, p] if p.starts_with('Q') => match p[1..].parse::<u32>() {
            Ok(q @ 1..=4) => (q * 3 - 2, q * 3),
            _ => return Err(bad()),
        },
        [_, p] if p.starts_with('H') => match p[1..].parse::<u32>() {
            Ok(1) => (1, 6),
            Ok(2) => (7, 12),
            _ => return Err(bad()),
        },
        [_, m] if m.len() == 2 => match m.parse::<u32>() {
            Ok(m @ 1..=12) => (m, m),
            _ => return Err(bad()),
        },
        [_, _, _] => {
            let day = date(&s)?;
            return Ok(Range {
                from: day.clone(),
                to: day,
            });
        }
        _ => return Err(bad()),
    };
    Ok(Range {
        from: format!("{year:04}-{first_month:02}-01"),
        to: format!(
            "{year:04}-{last_month:02}-{:02}",
            days_in_month(year, last_month)
        ),
    })
}

/// Validates a YYYY-MM-DD date and returns it unchanged.
pub fn date(s: &str) -> Result<String> {
    let s = s.trim();
    let bad = || anyhow!("`{s}` is not a date; use YYYY-MM-DD");
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return Err(bad());
    };
    if m.len() != 2 || d.len() != 2 {
        return Err(bad());
    }
    let year = year(y).ok_or_else(bad)?;
    let month: u32 = m.parse().map_err(|_| bad())?;
    let day: u32 = d.parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(bad());
    }
    Ok(s.to_string())
}

fn year(s: &str) -> Option<u32> {
    (s.len() == 4).then(|| s.parse().ok()).flatten()
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(spec: &str) -> (String, String) {
        let range = parse(spec).unwrap();
        (range.from, range.to)
    }

    #[test]
    fn parses_period_shapes() {
        assert_eq!(r("2025"), ("2025-01-01".into(), "2025-12-31".into()));
        assert_eq!(r("2025-q3"), ("2025-07-01".into(), "2025-09-30".into()));
        assert_eq!(r("2025-H1"), ("2025-01-01".into(), "2025-06-30".into()));
        assert_eq!(r("2024-02"), ("2024-02-01".into(), "2024-02-29".into()));
        assert_eq!(r("2025-03-14"), ("2025-03-14".into(), "2025-03-14".into()));
        assert_eq!(
            r("2025-01-01..2025-06-30"),
            ("2025-01-01".into(), "2025-06-30".into())
        );
    }

    #[test]
    fn rejects_bad_periods() {
        for bad in [
            "25",
            "2025-Q5",
            "2025-13",
            "2025-02-30",
            "2025-06-30..2025-01-01",
            "last year",
        ] {
            assert!(parse(bad).is_err(), "{bad} should fail");
        }
    }
}
