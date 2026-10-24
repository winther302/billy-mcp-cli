//! Writes happen in steps so every change can be read before it is made:
//! `draft` and `approve` only describe changes as records, and `apply` checks
//! the whole batch against Billy, then executes it only with --confirm-cvr.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Args;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::{env, period, record};

/// Set while executing `billy run`, where a model may be the author.
pub static IN_RUN: AtomicBool = AtomicBool::new(false);

const DEFAULT_API_TYPE: &str = "billy-cli";
const DEFAULT_DAYBOOK: &str = "Default journal";
const LINES_EMBED: &str = "daybookTransaction.lines:embed";

#[derive(Args, Debug)]
pub struct DraftArgs {
    /// Entry date (YYYY-MM-DD)
    #[arg(long)]
    date: String,
    /// Description shown in Billy
    #[arg(long)]
    text: String,
    /// One line per --line: debit positive, credit negative; lines must sum to 0
    #[arg(long = "line", required = true, value_name = "ACCOUNT:AMOUNT[:TEXT]")]
    lines: Vec<String>,
    /// Voucher number; a draft with the same marker, date and voucher is never created twice
    #[arg(long)]
    voucher: Option<String>,
    /// Daybook name or id [default: the only daybook, else "Default journal"]
    #[arg(long)]
    daybook: Option<String>,
    /// Marker stored on the transaction, to find it again with `daybook --api-type`
    #[arg(long, default_value = DEFAULT_API_TYPE)]
    api_type: String,
}

#[derive(Args, Debug)]
pub struct ApproveArgs {}

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// The company's CVR number; without it apply is a dry run that changes nothing
    #[arg(long, value_name = "CVR")]
    confirm_cvr: Option<String>,
}

/// Describes one balanced draft. Offline: nothing is checked against Billy yet.
pub fn draft(a: DraftArgs) -> Result<Vec<Value>> {
    let date = period::date(&a.date)?;
    if a.text.trim().is_empty() {
        bail!("--text must not be empty");
    }
    let mut lines = Vec::new();
    let mut balance = Decimal::ZERO;
    for spec in &a.lines {
        let (account, signed, text) = parse_line(spec)?;
        balance += signed;
        lines.push(json!({
            "accountNo": account,
            "side": if signed.is_sign_positive() { "debit" } else { "credit" },
            "amount": record::number(signed.abs()),
            "text": text,
        }));
    }
    if lines.len() < 2 {
        bail!("a draft needs at least two lines (a debit and a credit)");
    }
    if !balance.is_zero() {
        bail!("lines do not balance: they sum to {balance}, but debits (+) must equal credits (-)");
    }
    let mut map = Map::new();
    map.insert("op".into(), json!("create-draft"));
    map.insert("entryDate".into(), json!(date));
    if let Some(voucher) = a.voucher {
        map.insert("voucherNo".into(), json!(voucher));
    }
    map.insert("description".into(), json!(a.text.trim()));
    if let Some(daybook) = a.daybook {
        map.insert("daybook".into(), json!(daybook));
    }
    map.insert("apiType".into(), json!(a.api_type));
    map.insert("lines".into(), Value::Array(lines));
    Ok(vec![Value::Object(map)])
}

/// Parses ACCOUNT:AMOUNT[:TEXT] into account, signed amount and optional text.
fn parse_line(spec: &str) -> Result<(String, Decimal, Option<String>)> {
    let bad = || {
        anyhow!("line `{spec}` must be ACCOUNT:AMOUNT[:TEXT], e.g. 5820:12500.00 or 1310:-12500.00:Husleje")
    };
    let (account, rest) = spec.split_once(':').ok_or_else(bad)?;
    let (amount, text) = match rest.split_once(':') {
        Some((amount, text)) => (
            amount,
            Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        ),
        None => (rest, None),
    };
    let account = account.trim();
    if account.is_empty() {
        return Err(bad());
    }
    let signed = record::parse_decimal(amount).ok_or_else(bad)?;
    if signed.is_zero() {
        bail!("line `{spec}` has amount 0");
    }
    if signed.normalize().scale() > 2 {
        bail!("line `{spec}`: amounts have at most 2 decimals");
    }
    Ok((account.to_string(), signed, text))
}

/// Turns daybook records into approval requests.
pub fn approve(records: Vec<Value>) -> Result<Vec<Value>> {
    records
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let id = r.get("id").and_then(Value::as_str).ok_or_else(|| {
                anyhow!(
                    "record {} has no id; pipe daybook records into approve, \
                     e.g. `billy daybook --state draft | billy approve`",
                    i + 1
                )
            })?;
            if r.get("state").and_then(Value::as_str) == Some("voided") {
                bail!("record {} ({id}) is voided and cannot be approved", i + 1);
            }
            Ok(json!({
                "op": "approve-draft",
                "id": id,
                "entryDate": r.get("entryDate"),
                "voucherNo": r.get("voucherNo"),
                "description": r.get("description"),
            }))
        })
        .collect()
}

enum Action {
    Create(Value),
    Approve(String),
    Skip(String),
}

struct Planned {
    summary: Value,
    action: Action,
}

impl Planned {
    fn result(&self, status: &str, id: Option<&str>) -> Value {
        let mut out = self.summary.clone();
        out["status"] = json!(status);
        if let Some(id) = id {
            out["id"] = json!(id);
        }
        if let Action::Skip(reason) = &self.action {
            out["reason"] = json!(reason);
        }
        out
    }
}

/// What `apply` needs to know about the company to check a change.
struct Books {
    org_id: String,
    cvr: String,
    currency: String,
    locked_through: Option<String>,
    accounts: HashMap<String, Value>,
    daybooks: Vec<Value>,
    existing: HashMap<(String, String), Vec<Value>>,
}

impl Books {
    fn load(client: &Client) -> Result<Self> {
        let mut payload = client.get("organization", &Vec::new())?;
        let org = payload
            .get_mut("organization")
            .map(Value::take)
            .filter(Value::is_object)
            .ok_or_else(|| anyhow!("response for /organization has no organization"))?;
        let text = |key: &str| org.get(key).map(record::text).filter(|t| !t.is_empty());
        let accounts = client
            .list_all("accounts", &Vec::new())?
            .into_iter()
            .filter_map(|acc| Some((record::text(acc.get("accountNo")?), acc)))
            .collect();
        Ok(Self {
            org_id: text("id").ok_or_else(|| anyhow!("organization has no id"))?,
            cvr: text("registrationNo").unwrap_or_default().replace(' ', ""),
            currency: text("baseCurrencyId").unwrap_or_else(|| "DKK".into()),
            locked_through: text("fiscalLockingDate"),
            accounts,
            daybooks: client.list_all("daybooks", &Vec::new())?,
            existing: HashMap::new(),
        })
    }

    fn check_unlocked(&self, date: &str) -> Result<()> {
        match &self.locked_through {
            Some(lock) if date <= lock.as_str() => {
                bail!("{date} is in a locked period; Billy is locked through {lock}")
            }
            _ => Ok(()),
        }
    }

    /// The daybook's id and name.
    fn daybook(&self, wanted: Option<&str>) -> Result<(String, String)> {
        let open: Vec<&Value> = self
            .daybooks
            .iter()
            .filter(|d| d.get("isArchived").and_then(Value::as_bool) != Some(true))
            .collect();
        let name = |d: &Value| record::text(&d["name"]);
        let found = match wanted {
            Some(w) => open
                .iter()
                .find(|d| record::text(&d["id"]) == w || name(d).eq_ignore_ascii_case(w)),
            None if open.len() == 1 => open.first(),
            None => open.iter().find(|d| name(d) == DEFAULT_DAYBOOK),
        };
        if let Some(d) = found {
            return Ok((record::text(&d["id"]), name(d)));
        }
        let names = open.iter().map(|d| name(d)).collect::<Vec<_>>().join(", ");
        match wanted {
            Some(w) => bail!("no daybook named `{w}`; daybooks are: {names}"),
            None => bail!("choose a daybook with --daybook; daybooks are: {names}"),
        }
    }

    /// Daybook transactions already in Billy with this marker on this date.
    fn existing(&mut self, client: &Client, api_type: &str, date: &str) -> Result<&[Value]> {
        let key = (api_type.to_string(), date.to_string());
        if !self.existing.contains_key(&key) {
            let query = vec![
                ("apiType", api_type.to_string()),
                ("minEntryDate", date.to_string()),
                ("maxEntryDate", date.to_string()),
                ("include", LINES_EMBED.to_string()),
            ];
            let found = client.list_all("daybookTransactions", &query)?;
            self.existing.insert(key.clone(), found);
        }
        Ok(&self.existing[&key])
    }

    /// Checks a create-draft record; returns the plan and a key for spotting
    /// the same draft twice in one batch.
    fn plan_create(&mut self, client: &Client, r: &Value) -> Result<(Planned, String)> {
        let get = |key: &str| {
            r.get(key)
                .filter(|v| !v.is_null())
                .map(record::text)
                .filter(|t| !t.trim().is_empty())
        };
        let date = period::date(&get("entryDate").ok_or_else(|| anyhow!("missing entryDate"))?)?;
        self.check_unlocked(&date)?;
        let description = get("description").ok_or_else(|| anyhow!("missing description"))?;
        let voucher = get("voucherNo");
        let api_type = get("apiType").unwrap_or_else(|| DEFAULT_API_TYPE.to_string());
        let (daybook_id, daybook_name) = self.daybook(get("daybook").as_deref())?;
        let Some(Value::Array(lines)) = r.get("lines") else {
            bail!("missing lines");
        };

        let mut body_lines = Vec::new();
        let mut signature = Vec::new();
        let mut balance = Decimal::ZERO;
        let mut debits = Decimal::ZERO;
        for (i, line) in lines.iter().enumerate() {
            let n = i + 1;
            let account_no = line
                .get("accountNo")
                .map(record::text)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| anyhow!("line {n} has no accountNo"))?;
            let account = self.accounts.get(&account_no).ok_or_else(|| {
                anyhow!("line {n}: there is no account {account_no}; see `billy accounts`")
            })?;
            if account.get("isArchived").and_then(Value::as_bool) == Some(true) {
                bail!("line {n}: account {account_no} is archived");
            }
            let amount = line
                .get("amount")
                .and_then(record::decimal)
                .filter(|d| d.is_sign_positive() && !d.is_zero() && d.normalize().scale() <= 2)
                .ok_or_else(|| {
                    anyhow!("line {n}: amount must be positive with at most 2 decimals")
                })?;
            let side = line
                .get("side")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "debit" | "credit"))
                .ok_or_else(|| anyhow!("line {n}: side must be debit or credit"))?;
            if side == "debit" {
                balance += amount;
                debits += amount;
            } else {
                balance -= amount;
            }
            let account_id = record::text(&account["id"]);
            signature.push(format!("{account_id}|{side}|{}", amount.normalize()));
            let text = line
                .get("text")
                .filter(|t| !t.is_null())
                .map(record::text)
                .unwrap_or_else(|| description.clone());
            body_lines.push(json!({
                "accountId": account_id,
                "text": text,
                "amount": record::number(amount),
                "side": side,
                "currencyId": self.currency,
            }));
        }
        if body_lines.len() < 2 {
            bail!("a draft needs at least two lines");
        }
        if !balance.is_zero() {
            bail!("lines do not balance: debits minus credits is {balance}");
        }
        signature.sort();

        let summary = json!({
            "op": "create-draft",
            "entryDate": &date,
            "voucherNo": &voucher,
            "description": &description,
            "daybook": daybook_name,
            "lines": body_lines.len(),
            "amount": record::number(debits),
        });
        let key = match &voucher {
            Some(v) => format!("{api_type}|{date}|voucher:{v}"),
            None => format!("{api_type}|{date}|{description}|{}", signature.join(",")),
        };

        let mut tx = json!({
            "organizationId": self.org_id,
            "daybookId": daybook_id,
            "entryDate": &date,
            "description": &description,
            "apiType": &api_type,
            "state": "draft",
            "lines": body_lines,
        });
        if let Some(v) = &voucher {
            tx["voucherNo"] = json!(v);
        }
        let duplicate = self
            .existing(client, &api_type, &date)?
            .iter()
            .filter(|t| t.get("state").and_then(Value::as_str) != Some("voided"))
            .find(|t| match &voucher {
                Some(v) => t.get("voucherNo").map(record::text).as_deref() == Some(v.as_str()),
                None => {
                    t.get("description").map(record::text).as_deref() == Some(description.as_str())
                        && line_signature(t) == signature
                }
            });
        let action = match duplicate {
            Some(t) => Action::Skip(format!("already in Billy as {}", record::text(&t["id"]))),
            None => Action::Create(json!({ "daybookTransaction": tx })),
        };
        Ok((Planned { summary, action }, key))
    }

    fn plan_approve(&self, client: &Client, r: &Value) -> Result<Planned> {
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing id"))?;
        let query = vec![("include", LINES_EMBED.to_string())];
        let mut payload = client.get(&format!("daybookTransactions/{id}"), &query)?;
        let tx = payload
            .get_mut("daybookTransaction")
            .map(Value::take)
            .filter(Value::is_object)
            .ok_or_else(|| anyhow!("Billy has no daybook transaction {id}"))?;
        let lines = tx["lines"].as_array().map_or(0, Vec::len);
        let summary = json!({
            "op": "approve-draft",
            "id": id,
            "entryDate": tx["entryDate"],
            "voucherNo": tx["voucherNo"],
            "description": tx["description"],
            "lines": lines,
        });
        let action = match record::text(&tx["state"]).as_str() {
            "approved" => Action::Skip("already approved".into()),
            "draft" => {
                if lines == 0 {
                    bail!("draft {id} has no lines, so there is nothing to approve");
                }
                self.check_unlocked(&record::text(&tx["entryDate"]))?;
                Action::Approve(id.to_string())
            }
            other => bail!("transaction {id} is {other}; only drafts can be approved"),
        };
        Ok(Planned { summary, action })
    }
}

fn line_signature(tx: &Value) -> Vec<String> {
    let mut signature: Vec<String> = tx
        .get("lines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|l| {
            let amount = l
                .get("amount")
                .and_then(record::decimal)
                .map(|d| d.normalize());
            format!(
                "{}|{}|{}",
                record::text(&l["accountId"]),
                record::text(&l["side"]),
                amount.map(|d| d.to_string()).unwrap_or_default()
            )
        })
        .collect();
    signature.sort();
    signature
}

pub fn apply(a: ApplyArgs, records: Vec<Value>, out: &mut dyn Write) -> Result<()> {
    if records.is_empty() {
        bail!("nothing to apply; pipe the output of `billy draft` or `billy approve` into apply");
    }
    let execute = a.confirm_cvr.is_some();
    if execute
        && IN_RUN.load(Ordering::Relaxed)
        && std::env::var("BILLY_RUN_WRITES").as_deref() != Ok("1")
    {
        bail!(
            "writes through `billy run` are switched off; a person must start it with \
             BILLY_RUN_WRITES=1, or run `billy apply` in a terminal"
        );
    }
    let client = Client::from_env()?;
    let mut books = Books::load(&client)?;
    if let Some(cvr) = &a.confirm_cvr {
        if cvr.replace(' ', "") != books.cvr {
            bail!("--confirm-cvr does not match the company this token belongs to (see `billy org`); nothing was changed");
        }
    }

    // Check every record before changing anything.
    let mut planned = Vec::new();
    let mut seen = HashSet::new();
    for (i, r) in records.iter().enumerate() {
        let n = i + 1;
        let (mut plan, key) = match r.get("op").and_then(Value::as_str) {
            Some("create-draft") => books
                .plan_create(&client, r)
                .with_context(|| format!("record {n} (create-draft)"))?,
            Some("approve-draft") => {
                let plan = books
                    .plan_approve(&client, r)
                    .with_context(|| format!("record {n} (approve-draft)"))?;
                let key = format!("approve|{}", record::text(&plan.summary["id"]));
                (plan, key)
            }
            Some(other) => bail!("record {n}: unknown op `{other}`"),
            None => bail!(
                "record {n} is not a change; apply reads the output of `billy draft` or `billy approve`"
            ),
        };
        if !seen.insert(key) && !matches!(plan.action, Action::Skip(_)) {
            plan.action = Action::Skip("same change as an earlier record in this batch".into());
        }
        planned.push(plan);
    }

    if !execute {
        for plan in &planned {
            let status = match plan.action {
                Action::Create(_) => "would-create",
                Action::Approve(_) => "would-approve",
                Action::Skip(_) => "skip",
            };
            record::emit(out, &[plan.result(status, None)])?;
        }
        eprintln!(
            "billy apply: dry run, nothing was changed. Add --confirm-cvr <company CVR> to apply."
        );
        return Ok(());
    }

    let mut audit = Audit::open(&books.cvr)?;
    let mut applied = 0;
    for plan in &planned {
        let outcome = match &plan.action {
            Action::Skip(_) => {
                record::emit(out, &[plan.result("skipped", None)])?;
                continue;
            }
            Action::Create(body) => client
                .post("daybookTransactions", body)
                .and_then(|resp| created_id(&resp))
                .map(|id| ("created", id)),
            Action::Approve(id) => client
                .put(
                    &format!("daybookTransactions/{id}"),
                    &json!({ "daybookTransaction": { "state": "approved" } }),
                )
                .map(|_| ("approved", id.clone())),
        };
        match outcome {
            Ok((status, id)) => {
                audit.log(plan, status, Some(&id), None)?;
                applied += 1;
                record::emit(out, &[plan.result(status, Some(&id))])?;
                out.flush()?;
            }
            Err(e) => {
                audit.log(plan, "failed", None, Some(&format!("{e:#}")))?;
                bail!(
                    "{e:#}\nstopped here; {applied} earlier change(s) in this batch were applied (see {})",
                    audit.path.display()
                );
            }
        }
    }
    eprintln!(
        "billy apply: {applied} change(s) applied, logged in {}",
        audit.path.display()
    );
    Ok(())
}

fn created_id(resp: &Value) -> Result<String> {
    resp.pointer("/daybookTransactions/0/id")
        .or_else(|| resp.pointer("/daybookTransaction/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "Billy accepted the draft but returned no id; check `billy daybook --state draft`"
            )
        })
}

/// Append-only log of every change apply makes, one JSON line each.
struct Audit {
    file: File,
    path: PathBuf,
    cvr: String,
}

impl Audit {
    fn open(cvr: &str) -> Result<Self> {
        let path = env::audit_path();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!("opening audit log {}; nothing was changed", path.display())
            })?;
        Ok(Self {
            file,
            path,
            cvr: cvr.to_string(),
        })
    }

    fn log(
        &mut self,
        plan: &Planned,
        status: &str,
        id: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let request = match &plan.action {
            Action::Create(body) => body.clone(),
            Action::Approve(id) => json!({ "approve": id }),
            Action::Skip(_) => Value::Null,
        };
        let entry = json!({
            "time": utc_now(),
            "cvr": self.cvr,
            "status": status,
            "id": id,
            "change": plan.summary,
            "request": request,
            "error": error,
        });
        writeln!(self.file, "{}", serde_json::to_string(&entry)?)?;
        self.file.flush()?;
        Ok(())
    }
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let t = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft_of(lines: &[&str]) -> Result<Vec<Value>> {
        draft(DraftArgs {
            date: "2025-07-03".into(),
            text: "Husleje juli".into(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            voucher: None,
            daybook: None,
            api_type: DEFAULT_API_TYPE.into(),
        })
    }

    #[test]
    fn draft_describes_balanced_lines() {
        let out = draft_of(&["5820:12500.00", "1310:-12500.00:Bank"]).unwrap();
        assert_eq!(
            out[0]["lines"].to_string(),
            r#"[{"accountNo":"5820","side":"debit","amount":12500.00,"text":null},{"accountNo":"1310","side":"credit","amount":12500.00,"text":"Bank"}]"#
        );
        assert_eq!(out[0]["op"], "create-draft");
    }

    #[test]
    fn draft_rejects_bad_lines() {
        let err = |lines: &[&str]| format!("{:#}", draft_of(lines).unwrap_err());
        assert!(err(&["5820:100", "1310:-99.99"]).contains("do not balance"));
        assert!(err(&["5820:100"]).contains("at least two lines"));
        assert!(err(&["5820:100.001", "1310:-100.001"]).contains("2 decimals"));
        assert!(err(&["5820:0", "1310:0"]).contains("amount 0"));
        assert!(err(&["5820=100", "1310:-100"]).contains("ACCOUNT:AMOUNT"));
    }

    #[test]
    fn approve_needs_ids_and_refuses_voided() {
        assert!(approve(vec![json!({"state": "draft"})]).is_err());
        assert!(approve(vec![json!({"id": "x", "state": "voided"})]).is_err());
        let out = approve(vec![json!({"id": "x", "state": "draft", "voucherNo": "7"})]).unwrap();
        assert_eq!(out[0]["op"], "approve-draft");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_383), (2025, 10, 22));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
