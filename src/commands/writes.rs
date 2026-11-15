//! Writes happen in steps so every change can be read before it is made:
//! `draft` and `approve` only describe changes as records, and `apply` checks
//! the whole batch against Billy, then executes it only with --confirm-cvr.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Args;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use super::api;
use crate::client::{Client, Query};
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
pub struct AttachArgs {
    /// PDF or image to upload
    file: PathBuf,
    /// Daybook transaction id to attach it to; without --to it goes to Billy's inbox
    #[arg(long, value_name = "ID")]
    to: Option<String>,
    /// File name shown in Billy [default: the file's own name]
    #[arg(long)]
    name: Option<String>,
}

/// Largest file `attach` will upload.
const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// The company's CVR number; without it apply is a dry run that changes nothing
    #[arg(long, value_name = "CVR")]
    confirm_cvr: Option<String>,
    /// Allow deletes in this batch (inside `billy run` also needs BILLY_RUN_DELETES=1)
    #[arg(long)]
    allow_delete: bool,
    /// Allow sending emails, e-invoices and reminders to people outside the company
    /// (inside `billy run` also needs BILLY_RUN_SENDS=1)
    #[arg(long)]
    allow_send: bool,
    /// Send creates and updates of the same resource as one bulk save, and its
    /// deletes as one bulk delete; each bulk request succeeds or fails as a whole
    #[arg(long)]
    bulk: bool,
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

/// Describes uploading one file, to a daybook transaction or to the inbox.
/// Offline apart from reading the file's size.
pub fn attach(a: AttachArgs) -> Result<Vec<Value>> {
    let path = std::fs::canonicalize(&a.file)
        .with_context(|| format!("cannot read {}", a.file.display()))?;
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() {
        bail!("{} is not a file", path.display());
    }
    let name = match a.name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        Some(_) => bail!("--name must not be empty"),
        None => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("{} has no file name", path.display()))?,
    };
    content_type(&name)?;
    let mut map = Map::new();
    map.insert("op".into(), json!("attach-file"));
    map.insert("file".into(), json!(path.to_string_lossy()));
    map.insert("fileName".into(), json!(name));
    map.insert("size".into(), json!(meta.len()));
    map.insert("to".into(), json!(a.to.filter(|t| !t.trim().is_empty())));
    Ok(vec![Value::Object(map)])
}

fn content_type(name: &str) -> Result<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("pdf") => Ok("application/pdf"),
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        _ => bail!("`{name}`: only .pdf, .png, .jpg and .jpeg files can be attached"),
    }
}

/// Billy's file names travel in an HTTP header, so keep them to plain ASCII.
fn header_safe_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            'æ' => out.push_str("ae"),
            'Æ' => out.push_str("Ae"),
            'ø' => out.push_str("oe"),
            'Ø' => out.push_str("Oe"),
            'å' => out.push_str("aa"),
            'Å' => out.push_str("Aa"),
            'ä' | 'á' | 'à' => out.push('a'),
            'ö' | 'ó' => out.push('o'),
            'ü' | 'ú' => out.push('u'),
            'é' | 'è' => out.push('e'),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

/// A file name as Billy stores it (it turns spaces into dashes), for comparing.
fn name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
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
    Upload(Upload),
    Request(Request),
    Skip(String),
}

/// A generic change from `create`, `update`, `delete` or `call`.
struct Request {
    method: &'static str,
    path: String,
    body: Option<Value>,
    /// Past-tense status once done: created, updated, deleted or called.
    done: &'static str,
    /// The record's id when known before the request (update, delete).
    id: Option<String>,
    /// Plural resource whose first record in the response carries the new id.
    created_in: Option<String>,
    kind: Risk,
    /// How this change joins a bulk request under `apply --bulk`.
    bulk: Option<Bulk>,
}

/// One record's part in a bulk request on `resource`.
enum Bulk {
    /// A record to save: new fields, or changes plus its id.
    Save {
        resource: String,
        record: Value,
    },
    Delete {
        resource: String,
        id: String,
    },
}

impl Bulk {
    fn group(&self) -> (String, bool) {
        match self {
            Bulk::Save { resource, .. } => (resource.clone(), true),
            Bulk::Delete { resource, .. } => (resource.clone(), false),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Risk {
    Change,
    Delete,
    Send,
}

struct Upload {
    path: PathBuf,
    file_name: String,
    content_type: &'static str,
    owner: Option<String>,
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
    /// (owner id or "" for the inbox, file name, size) of attachments already in Billy.
    attachments: Option<HashSet<(String, String, u64)>>,
}

impl Books {
    fn load(client: &Client) -> Result<Self> {
        let mut payload = client.get("organization", &Query::new())?;
        let org = payload
            .get_mut("organization")
            .map(Value::take)
            .filter(Value::is_object)
            .ok_or_else(|| anyhow!("response for /organization has no organization"))?;
        let text = |key: &str| org.get(key).map(record::text).filter(|t| !t.is_empty());
        let accounts = client
            .list_all("accounts", &Query::new())?
            .into_iter()
            .filter_map(|acc| Some((record::text(acc.get("accountNo")?), acc)))
            .collect();
        Ok(Self {
            org_id: text("id").ok_or_else(|| anyhow!("organization has no id"))?,
            cvr: text("registrationNo").unwrap_or_default().replace(' ', ""),
            currency: text("baseCurrencyId").unwrap_or_else(|| "DKK".into()),
            locked_through: text("fiscalLockingDate"),
            accounts,
            daybooks: client.list_all("daybooks", &Query::new())?,
            existing: HashMap::new(),
            attachments: None,
        })
    }

    /// Attachments already in Billy, keyed for spotting an upload made twice.
    fn attachments(&mut self, client: &Client) -> Result<&HashSet<(String, String, u64)>> {
        if self.attachments.is_none() {
            let mut keys = HashSet::new();
            for att in client.list_all("attachments", &Query::new())? {
                let file_id = record::text(&att["fileId"]);
                if file_id.is_empty() {
                    continue;
                }
                let mut payload = client.get(&format!("files/{file_id}"), &Query::new())?;
                let file = payload.get_mut("file").map(Value::take).unwrap_or_default();
                keys.insert((
                    record::text(&att["ownerId"]),
                    name_key(&record::text(&file["fileName"])),
                    file["fileSize"].as_u64().unwrap_or(0),
                ));
            }
            self.attachments = Some(keys);
        }
        Ok(self.attachments.as_ref().expect("loaded above"))
    }

    fn plan_attach(&mut self, client: &Client, r: &Value) -> Result<(Planned, String)> {
        let path = r
            .get("file")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("missing file"))?;
        let meta =
            std::fs::metadata(&path).with_context(|| format!("cannot read {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a file", path.display());
        }
        if meta.len() == 0 {
            bail!("{} is empty", path.display());
        }
        if meta.len() > MAX_ATTACHMENT_BYTES {
            bail!("{} is larger than 20 MB", path.display());
        }
        let shown = r
            .get("fileName")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| anyhow!("missing fileName"))?;
        let content_type = content_type(shown)?;
        let file_name = header_safe_name(shown);
        let to = r
            .get("to")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);

        let mut summary = json!({
            "op": "attach-file",
            "fileName": &file_name,
            "size": meta.len(),
            "to": to.as_deref().unwrap_or("inbox"),
        });
        // `--to <id>` means a daybook transaction; `--to bill:<id>` or
        // `--to invoice:<id>` names the owner type explicitly.
        let owner = to.as_deref().map(|t| match t.split_once(':') {
            Some((kind, id)) => (kind.to_string(), id.to_string()),
            None => ("daybookTransaction".to_string(), t.to_string()),
        });
        if let Some((kind, id)) = &owner {
            if !matches!(kind.as_str(), "daybookTransaction" | "bill" | "invoice") {
                bail!("--to {kind}:{id}: files attach to a daybookTransaction, bill or invoice");
            }
            let mut payload = client.get(&format!("{kind}s/{id}"), &Query::new())?;
            let rec = payload
                .get_mut(kind.as_str())
                .map(Value::take)
                .filter(Value::is_object)
                .ok_or_else(|| anyhow!("Billy has no {kind} {id}"))?;
            if record::text(&rec["state"]) == "voided" {
                bail!("{kind} {id} is voided");
            }
            for f in ["voucherNo", "invoiceNo", "description", "lineDescription"] {
                if !rec[f].is_null() {
                    summary[f] = rec[f].clone();
                }
            }
        }
        let owner_id = owner.as_ref().map(|(_, id)| id.clone()).unwrap_or_default();
        let key = format!("attach|{owner_id}|{file_name}|{}", meta.len());
        let exists =
            self.attachments(client)?
                .contains(&(owner_id, name_key(&file_name), meta.len()));
        let action = if exists {
            Action::Skip("a file with this name and size is already attached there".into())
        } else {
            Action::Upload(Upload {
                path,
                file_name,
                content_type,
                owner: owner.map(|(kind, id)| format!("{kind}:{id}")),
            })
        };
        Ok((Planned { summary, action }, key))
    }

    fn upload(&self, client: &Client, u: &Upload) -> Result<String> {
        let bytes =
            std::fs::read(&u.path).with_context(|| format!("reading {}", u.path.display()))?;
        let resp = client.post_file(&u.file_name, u.content_type, &bytes)?;
        let file_id = resp
            .pointer("/files/0/id")
            .or_else(|| resp.pointer("/file/id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Billy accepted the upload but returned no file id"))?;
        let mut attachment = json!({
            "organizationId": self.org_id,
            "fileId": file_id,
        });
        if let Some(owner) = &u.owner {
            attachment["ownerReference"] = json!(owner);
        }
        let resp = client.post("attachments", &json!({ "attachment": attachment }))?;
        resp.pointer("/attachments/0/id")
            .or_else(|| resp.pointer("/attachment/id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!("file {file_id} was uploaded but Billy returned no attachment id")
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

impl Books {
    fn plan_generic_create(&mut self, client: &Client, r: &Value) -> Result<(Planned, String)> {
        let res = api::resource(&record::text(&r["resource"]))?;
        res.require("create")?;
        let mut rec = r
            .get("record")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| anyhow!("missing record"))?;
        if res.org_scoped && rec.get("organizationId").is_none() {
            rec["organizationId"] = json!(self.org_id);
        }
        let key = format!("create|{}|{}", res.name, rec);
        let unique: Vec<String> = r
            .get("unique")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(record::text)
            .collect();
        let mut summary = json!({ "op": "create", "resource": res.name, "record": rec });
        if !unique.is_empty() {
            summary["unique"] = json!(unique);
            let same = |x: &Value| {
                unique
                    .iter()
                    .all(|f| x.get(f).map(record::text) == rec.get(f).map(record::text))
            };
            let q: Vec<(String, String)> = unique
                .iter()
                .map(|f| (f.clone(), record::text(&rec[f])))
                .collect();
            // Billy ignores filters it does not know, so check the values here too.
            let found = client.list_all(res.name, &q)?;
            if let Some(existing) = found.iter().find(|x| same(x)) {
                let id = record::text(&existing["id"]);
                let action = Action::Skip(format!(
                    "a {} with the same {} exists: {id}",
                    res.singular(),
                    unique.join(", ")
                ));
                return Ok((Planned { summary, action }, key));
            }
        }
        let kind = if api::sends("POST", res.name) {
            Risk::Send
        } else {
            Risk::Change
        };
        let action = Action::Request(Request {
            method: "POST",
            path: res.name.to_string(),
            body: Some(json!({ res.singular(): rec })),
            done: "created",
            id: None,
            created_in: Some(res.name.to_string()),
            kind,
            bulk: (kind == Risk::Change).then(|| Bulk::Save {
                resource: res.name.to_string(),
                record: rec.clone(),
            }),
        });
        Ok((Planned { summary, action }, key))
    }

    /// The current record, or an error naming what is missing.
    fn fetch(client: &Client, res: &api::Resource, id: &str) -> Result<Value> {
        let mut payload = client.get(&format!("{}/{id}", res.name), &Query::new())?;
        payload
            .get_mut(res.singular().as_str())
            .map(Value::take)
            .filter(Value::is_object)
            .ok_or_else(|| anyhow!("Billy has no {} {id}", res.singular()))
    }

    fn plan_update(&self, client: &Client, r: &Value) -> Result<(Planned, String)> {
        let res = api::resource(&record::text(&r["resource"]))?;
        res.require("update")?;
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing id"))?;
        let Some(Value::Object(changes)) = r.get("changes") else {
            bail!("missing changes");
        };
        let current = Self::fetch(client, res, id)?;
        let mut diff = Map::new();
        let mut body = Map::new();
        for (k, v) in changes {
            let before = current.get(k).cloned().unwrap_or(Value::Null);
            if !same_value(&before, v) {
                diff.insert(k.clone(), json!({ "from": before, "to": v }));
                body.insert(k.clone(), v.clone());
            }
        }
        let summary = json!({
            "op": "update",
            "resource": res.name,
            "id": id,
            "label": label(&current),
            "changes": diff,
        });
        let key = format!(
            "update|{}|{id}|{}",
            res.name,
            Value::Object(changes.clone())
        );
        let action = if body.is_empty() {
            Action::Skip("already has these values".into())
        } else {
            let mut record = body.clone();
            record.insert("id".into(), json!(id));
            Action::Request(Request {
                method: "PUT",
                path: format!("{}/{id}", res.name),
                body: Some(json!({ res.singular(): body })),
                done: "updated",
                id: Some(id.to_string()),
                created_in: None,
                kind: Risk::Change,
                bulk: Some(Bulk::Save {
                    resource: res.name.to_string(),
                    record: Value::Object(record),
                }),
            })
        };
        Ok((Planned { summary, action }, key))
    }

    fn plan_delete(&self, client: &Client, r: &Value) -> Result<(Planned, String)> {
        let res = api::resource(&record::text(&r["resource"]))?;
        res.require("delete")?;
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing id"))?;
        let current = Self::fetch(client, res, id)?;
        let summary =
            json!({ "op": "delete", "resource": res.name, "id": id, "label": label(&current) });
        let action = Action::Request(Request {
            method: "DELETE",
            path: format!("{}/{id}", res.name),
            body: None,
            done: "deleted",
            id: Some(id.to_string()),
            created_in: None,
            kind: Risk::Delete,
            bulk: Some(Bulk::Delete {
                resource: res.name.to_string(),
                id: id.to_string(),
            }),
        });
        Ok((
            Planned { summary, action },
            format!("delete|{}|{id}", res.name),
        ))
    }

    fn plan_call(&self, r: &Value) -> Result<(Planned, String)> {
        let method = match record::text(&r["method"]).to_ascii_uppercase().as_str() {
            "POST" => "POST",
            "PUT" => "PUT",
            "PATCH" => "PATCH",
            "DELETE" => "DELETE",
            other => bail!("method `{other}` cannot be applied; use POST, PUT, PATCH or DELETE"),
        };
        let path = api::clean_path(&record::text(&r["path"]))?;
        let body = r.get("body").filter(|b| !b.is_null()).cloned();
        let kind = if method == "DELETE" {
            Risk::Delete
        } else if api::sends(method, &path) {
            Risk::Send
        } else {
            Risk::Change
        };
        let summary = json!({ "op": "call", "method": method, "path": &path, "body": &body });
        let key = format!("call|{method}|{path}|{}", body.clone().unwrap_or_default());
        let action = Action::Request(Request {
            method,
            path,
            body,
            done: "called",
            id: None,
            created_in: None,
            kind,
            bulk: None,
        });
        Ok((Planned { summary, action }, key))
    }
}

/// Equal as JSON, or as text for scalars ("100" and 100, "true" and true).
fn same_value(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    let scalar = |v: &Value| !v.is_object() && !v.is_array() && !v.is_null();
    scalar(a)
        && scalar(b)
        && match (record::decimal(a), record::decimal(b)) {
            (Some(x), Some(y)) => x == y,
            _ => record::text(a) == record::text(b),
        }
}

/// A few identifying fields of a record, for summaries.
fn label(rec: &Value) -> Value {
    let mut out = Map::new();
    for f in [
        "name",
        "invoiceNo",
        "voucherNo",
        "suppliersInvoiceNo",
        "accountNo",
        "entryDate",
        "description",
        "lineDescription",
        "state",
        "amount",
        "grossAmount",
    ] {
        if let Some(v) = rec.get(f).filter(|v| !v.is_null()) {
            out.insert(f.to_string(), v.clone());
        }
    }
    Value::Object(out)
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
        bail!("nothing to apply; pipe the output of draft, approve, attach, create, update, delete or call into apply");
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
            Some("attach-file") => books
                .plan_attach(&client, r)
                .with_context(|| format!("record {n} (attach-file)"))?,
            Some("create") => books
                .plan_generic_create(&client, r)
                .with_context(|| format!("record {n} (create {})", record::text(&r["resource"])))?,
            Some("update") => books
                .plan_update(&client, r)
                .with_context(|| format!("record {n} (update {})", record::text(&r["resource"])))?,
            Some("delete") => books
                .plan_delete(&client, r)
                .with_context(|| format!("record {n} (delete {})", record::text(&r["resource"])))?,
            Some("call") => books
                .plan_call(r)
                .with_context(|| format!("record {n} (call)"))?,
            Some(other) => bail!("record {n}: unknown op `{other}`"),
            None => bail!(
                "record {n} is not a change; apply reads the output of draft, approve, attach, create, update, delete or call"
            ),
        };
        if !seen.insert(key) && !matches!(plan.action, Action::Skip(_)) {
            plan.action = Action::Skip("same change as an earlier record in this batch".into());
        }
        planned.push(plan);
    }

    let risky = |risk: Risk| {
        planned
            .iter()
            .any(|p| matches!(&p.action, Action::Request(rq) if rq.kind == risk))
    };
    let (deletes, sends) = (risky(Risk::Delete), risky(Risk::Send));

    if !execute {
        for plan in &planned {
            let status = match &plan.action {
                Action::Create(_) => "would-create".to_string(),
                Action::Approve(_) => "would-approve".to_string(),
                Action::Upload(_) => "would-attach".to_string(),
                Action::Request(rq) => match rq.done {
                    "created" => "would-create",
                    "updated" => "would-update",
                    "deleted" => "would-delete",
                    _ => "would-call",
                }
                .to_string(),
                Action::Skip(_) => "skip".to_string(),
            };
            record::emit(out, &[plan.result(&status, None)])?;
        }
        if deletes && !a.allow_delete {
            crate::notes::note(
                "billy apply: this batch deletes records; applying it also needs --allow-delete.",
            );
        }
        if sends && !a.allow_send {
            crate::notes::note("billy apply: this batch sends to people outside the company; applying it also needs --allow-send.");
        }
        crate::notes::note(
            "billy apply: dry run, nothing was changed. Add --confirm-cvr <company CVR> to apply.",
        );
        return Ok(());
    }

    let in_run = IN_RUN.load(Ordering::Relaxed);
    if deletes {
        if !a.allow_delete {
            bail!(
                "this batch deletes records; add --allow-delete to apply it. Nothing was changed"
            );
        }
        if in_run && std::env::var("BILLY_RUN_DELETES").as_deref() != Ok("1") {
            bail!("deletes through `billy run` are switched off; a person must start it with BILLY_RUN_DELETES=1. Nothing was changed");
        }
    }
    if sends {
        if !a.allow_send {
            bail!("this batch sends emails or e-invoices; add --allow-send to apply it. Nothing was changed");
        }
        if in_run && std::env::var("BILLY_RUN_SENDS").as_deref() != Ok("1") {
            bail!("sending through `billy run` is switched off; a person must start it with BILLY_RUN_SENDS=1. Nothing was changed");
        }
    }

    let mut audit = Audit::open(&books.cvr)?;
    let mut applied = 0;
    for group in batches(&planned, a.bulk) {
        if group.len() > 1 {
            let plans: Vec<&Planned> = group.iter().map(|&i| &planned[i]).collect();
            match run_bulk(&client, &plans) {
                Ok(results) => {
                    for (plan, (status, id)) in plans.iter().zip(results) {
                        audit.log(plan, status, Some(&id), None)?;
                        applied += 1;
                        let mut rec = plan.result(
                            status,
                            Some(&id).filter(|i| !i.is_empty()).map(String::as_str),
                        );
                        rec["bulk"] = json!(true);
                        record::emit(out, &[rec])?;
                    }
                    out.flush()?;
                }
                Err(e) => {
                    for plan in &plans {
                        audit.log(plan, "failed", None, Some(&format!("{e:#}")))?;
                    }
                    bail!(
                        "{e:#}\nthe bulk request of {} change(s) failed as a whole; {applied} earlier change(s) in this batch were applied (see {})",
                        plans.len(),
                        audit.path.display()
                    );
                }
            }
            continue;
        }
        let plan = &planned[group[0]];
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
            Action::Upload(u) => books.upload(&client, u).map(|id| ("attached", id)),
            Action::Request(rq) => client
                .send(rq.method, &rq.path, rq.body.as_ref())
                .map(|resp| (rq.done, request_id(rq, &resp))),
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
    crate::notes::note(format!(
        "billy apply: {applied} change(s) applied, logged in {}",
        audit.path.display()
    ));
    Ok(())
}

/// The order changes are executed in: one change per step, or with `bulk`,
/// generic saves and deletes of the same resource joined into one step at the
/// place of the first of them.
fn batches(planned: &[Planned], bulk: bool) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut open: HashMap<(String, bool), usize> = HashMap::new();
    for (i, plan) in planned.iter().enumerate() {
        let group = match &plan.action {
            Action::Request(Request { bulk: Some(b), .. }) if bulk => Some(b.group()),
            _ => None,
        };
        match group {
            Some(g) => match open.get(&g) {
                Some(&at) => out[at].push(i),
                None => {
                    open.insert(g, out.len());
                    out.push(vec![i]);
                }
            },
            None => out.push(vec![i]),
        }
    }
    out
}

/// Executes changes sharing one bulk group: `PATCH /<resource>` with every
/// record, or `DELETE /<resource>?ids[]=…`.
fn run_bulk(client: &Client, plans: &[&Planned]) -> Result<Vec<(&'static str, String)>> {
    let parts: Vec<(&Request, &Bulk)> = plans
        .iter()
        .filter_map(|p| match &p.action {
            Action::Request(rq) => rq.bulk.as_ref().map(|b| (rq, b)),
            _ => None,
        })
        .collect();
    if parts.len() != plans.len() {
        bail!("internal error: a bulk group holds a change that cannot be bulked");
    }
    match parts[0].1 {
        Bulk::Save { resource, .. } => {
            let records: Vec<Value> = parts
                .iter()
                .map(|(_, b)| match b {
                    Bulk::Save { record, .. } => record.clone(),
                    Bulk::Delete { .. } => Value::Null,
                })
                .collect();
            let resp = client.send(
                "PATCH",
                resource,
                Some(&json!({ resource.as_str(): records })),
            )?;
            let returned = resp.get(resource.as_str()).and_then(Value::as_array);
            // New records' ids, in order, when Billy returns exactly one record per change.
            let positional = returned.filter(|r| r.len() == parts.len());
            Ok(parts
                .iter()
                .enumerate()
                .map(|(i, (rq, _))| {
                    let id = rq.id.clone().unwrap_or_else(|| {
                        positional
                            .and_then(|r| r[i].get("id").and_then(Value::as_str))
                            .unwrap_or_default()
                            .to_string()
                    });
                    (rq.done, id)
                })
                .collect())
        }
        Bulk::Delete { resource, .. } => {
            let ids: Vec<&str> = parts
                .iter()
                .filter_map(|(_, b)| match b {
                    Bulk::Delete { id, .. } => Some(id.as_str()),
                    Bulk::Save { .. } => None,
                })
                .collect();
            let query = ids
                .iter()
                .map(|id| format!("ids%5B%5D={}", url_encode(id)))
                .collect::<Vec<_>>()
                .join("&");
            client.send("DELETE", &format!("{resource}?{query}"), None)?;
            Ok(parts
                .iter()
                .map(|(rq, _)| (rq.done, rq.id.clone().unwrap_or_default()))
                .collect())
        }
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The id a generic request concerns: known beforehand, or the first record
/// Billy returns.
fn request_id(rq: &Request, resp: &Value) -> String {
    if let Some(id) = &rq.id {
        return id.clone();
    }
    if let Some(res) = &rq.created_in {
        if let Some(id) = resp
            .pointer(&format!("/{res}/0/id"))
            .and_then(Value::as_str)
        {
            return id.to_string();
        }
    }
    resp.as_object()
        .into_iter()
        .flat_map(|m| m.iter())
        .filter(|(k, _)| *k != "meta")
        .find_map(|(_, v)| {
            v.pointer("/0/id")
                .or_else(|| v.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string()
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
        let file = open_audit_file(&path)?;
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
            Action::Upload(u) => json!({
                "upload": u.path.to_string_lossy(),
                "fileName": u.file_name,
                "ownerReference": u.owner,
            }),
            Action::Request(rq) => json!({
                "method": rq.method,
                "path": rq.path,
                "body": rq.body,
            }),
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

fn open_audit_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);

    let file = options
        .open(path)
        .with_context(|| format!("opening audit log {}; nothing was changed", path.display()))?;

    #[cfg(unix)]
    {
        let mode = file.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "audit log {} is accessible by group or other users (mode {:04o}); \
                 restrict it with `chmod 600` before applying changes",
                path.display(),
                mode & 0o7777
            );
        }
    }

    Ok(file)
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
    fn attach_names_and_types() {
        assert_eq!(
            header_safe_name("Faktura for løbende forbrug.pdf"),
            "Faktura for loebende forbrug.pdf"
        );
        assert_eq!(content_type("a.PDF").unwrap(), "application/pdf");
        assert_eq!(content_type("a.jpeg").unwrap(), "image/jpeg");
        assert!(content_type("mail.eml").is_err());
        assert!(content_type("noext").is_err());
        assert_eq!(
            name_key("Coop faktura 1.pdf"),
            name_key("Coop-faktura-1.pdf")
        );
    }

    #[test]
    fn generic_changes_compare_and_find_ids() {
        assert!(same_value(&json!("100.00"), &json!(100)));
        assert!(same_value(&json!(true), &json!("true")));
        assert!(!same_value(&json!("draft"), &json!("approved")));
        assert!(!same_value(&Value::Null, &json!("")));
        let rq = |id: Option<&str>, created_in: Option<&str>| Request {
            method: "POST",
            path: "contacts".into(),
            body: None,
            done: "created",
            id: id.map(str::to_string),
            created_in: created_in.map(str::to_string),
            kind: Risk::Change,
            bulk: None,
        };
        let resp =
            json!({"meta": {"x": 1}, "contacts": [{"id": "c1"}], "contactPersons": [{"id": "p1"}]});
        assert_eq!(request_id(&rq(None, Some("contacts")), &resp), "c1");
        assert_eq!(request_id(&rq(Some("k"), None), &resp), "k");
        assert_eq!(
            request_id(&rq(None, None), &json!({"email": {"id": "e1"}})),
            "e1"
        );
        assert_eq!(request_id(&rq(None, None), &Value::Null), "");
    }

    #[test]
    fn bulk_groups_saves_and_deletes_per_resource() {
        let req = |b: Option<Bulk>| Planned {
            summary: json!({}),
            action: Action::Request(Request {
                method: "POST",
                path: String::new(),
                body: None,
                done: "created",
                id: None,
                created_in: None,
                kind: Risk::Change,
                bulk: b,
            }),
        };
        let save = |r: &str| {
            Some(Bulk::Save {
                resource: r.into(),
                record: json!({}),
            })
        };
        let del = |r: &str| {
            Some(Bulk::Delete {
                resource: r.into(),
                id: "x".into(),
            })
        };
        let planned = vec![
            req(save("contacts")),
            req(None),
            req(del("contacts")),
            req(save("contacts")),
            req(save("products")),
            req(del("contacts")),
        ];
        assert_eq!(
            batches(&planned, true),
            vec![vec![0, 3], vec![1], vec![2, 5], vec![4]]
        );
        assert_eq!(batches(&planned, false).len(), 6);
        assert_eq!(url_encode("a b/ø"), "a%20b%2F%C3%B8");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_383), (2025, 10, 22));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[cfg(unix)]
    #[test]
    fn creates_a_private_audit_log() {
        let path = std::env::temp_dir().join(format!(
            "billy-audit-private-{}-{}.ndjson",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let file = open_audit_file(&path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_audit_log_accessible_to_other_users() {
        let path = std::env::temp_dir().join(format!(
            "billy-audit-shared-{}-{}.ndjson",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = open_audit_file(&path).unwrap_err().to_string();
        assert!(error.contains("chmod 600"), "{error}");
        std::fs::remove_file(path).unwrap();
    }
}
