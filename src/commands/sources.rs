//! Sources fetch records from Billy. Each one only reads.

use std::collections::HashMap;

use anyhow::{bail, Result};
use clap::{Args, ValueEnum};
use rust_decimal::Decimal;
use serde_json::{Map, Value};

use crate::client::{Client, Query};
use crate::{period, record};

const ORG_FIELDS: &[&str] = &[
    "id",
    "name",
    "registrationNo",
    "baseCurrencyId",
    "fiscalYearEndMonth",
];
const ACCOUNT_FIELDS: &[&str] = &[
    "id",
    "accountNo",
    "name",
    "groupId",
    "natureId",
    "isArchived",
];
const POSTING_FIELDS: &[&str] = &[
    "id",
    "entryDate",
    "accountNo",
    "text",
    "side",
    "amount",
    "signedAmount",
    "accountId",
    "transactionId",
    "isVoided",
];
const INVOICE_FIELDS: &[&str] = &[
    "id",
    "invoiceNo",
    "entryDate",
    "dueDate",
    "state",
    "contactId",
    "lineDescription",
    "amount",
    "tax",
    "grossAmount",
    "balance",
    "isPaid",
    "currencyId",
];
const BILL_FIELDS: &[&str] = &[
    "id",
    "voucherNo",
    "entryDate",
    "dueDate",
    "state",
    "contactId",
    "contactName",
    "suppliersInvoiceNo",
    "lineDescription",
    "amount",
    "tax",
    "grossAmount",
    "balance",
    "isPaid",
    "currencyId",
];
const CONTACT_FIELDS: &[&str] = &[
    "id",
    "contactNo",
    "name",
    "registrationNo",
    "isCustomer",
    "isSupplier",
    "cityText",
    "isArchived",
];
const DAYBOOKS_FIELDS: &[&str] = &["id", "name", "isArchived"];
const DAYBOOK_FIELDS: &[&str] = &[
    "id",
    "entryDate",
    "voucherNo",
    "description",
    "state",
    "apiType",
    "daybookId",
];

#[derive(Args, Debug)]
pub struct OutArgs {
    /// Emit Billy's full record instead of the default fields
    #[arg(long)]
    all: bool,
}

#[derive(Args, Debug)]
pub struct DateArgs {
    /// Entry-date period: 2025, 2025-Q3, 2025-H1, 2025-03, 2025-03-14 or 2025-01-01..2025-06-30
    #[arg(long, conflicts_with_all = ["from", "to"])]
    period: Option<String>,
    /// First entry date, inclusive (YYYY-MM-DD)
    #[arg(long)]
    from: Option<String>,
    /// Last entry date, inclusive (YYYY-MM-DD)
    #[arg(long)]
    to: Option<String>,
}

impl DateArgs {
    fn push_query(&self, q: &mut Query) -> Result<()> {
        let (from, to) = match &self.period {
            Some(p) => {
                let range = period::parse(p)?;
                (Some(range.from), Some(range.to))
            }
            None => (
                self.from.as_deref().map(period::date).transpose()?,
                self.to.as_deref().map(period::date).transpose()?,
            ),
        };
        if let (Some(f), Some(t)) = (&from, &to) {
            if f > t {
                bail!("--from {f} is after --to {t}");
            }
        }
        q.extend(from.map(|f| ("minEntryDate", f)));
        q.extend(to.map(|t| ("maxEntryDate", t)));
        Ok(())
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum State {
    Draft,
    Approved,
    Voided,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Draft => "draft",
            State::Approved => "approved",
            State::Voided => "voided",
        }
    }
}

#[derive(Args, Debug)]
pub struct OrgArgs {
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Args, Debug)]
pub struct AccountsArgs {
    /// Include archived accounts
    #[arg(long)]
    archived: bool,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Args, Debug)]
pub struct PostingsArgs {
    /// Account number (e.g. 1310) or Billy account id
    #[arg(long)]
    account: Option<String>,
    /// Free-text search in posting text and originator name
    #[arg(long)]
    q: Option<String>,
    /// Leave out voided postings
    #[arg(long)]
    exclude_voided: bool,
    #[command(flatten)]
    dates: DateArgs,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Args, Debug)]
pub struct DocArgs {
    #[arg(long, value_enum)]
    state: Option<State>,
    /// Only documents with an open balance
    #[arg(long, conflicts_with = "paid")]
    unpaid: bool,
    /// Only fully paid documents
    #[arg(long)]
    paid: bool,
    /// Billy contact id
    #[arg(long)]
    contact: Option<String>,
    /// Free-text search (contact name, description, number, amount)
    #[arg(long)]
    q: Option<String>,
    #[command(flatten)]
    dates: DateArgs,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Args, Debug)]
pub struct ContactsArgs {
    /// Only customers
    #[arg(long)]
    customers: bool,
    /// Only suppliers
    #[arg(long)]
    suppliers: bool,
    /// Include archived contacts
    #[arg(long)]
    archived: bool,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Args, Debug)]
pub struct DaybookArgs {
    #[arg(long, value_enum)]
    state: Option<State>,
    /// Only transactions created with this apiType marker
    #[arg(long)]
    api_type: Option<String>,
    /// Billy daybook id
    #[arg(long)]
    daybook: Option<String>,
    /// Free-text search in description and voucher number
    #[arg(long)]
    q: Option<String>,
    #[command(flatten)]
    dates: DateArgs,
    #[command(flatten)]
    out: OutArgs,
}

pub fn org(a: OrgArgs) -> Result<Vec<Value>> {
    let mut payload = Client::from_env()?.get("organization", &Query::new())?;
    let org = payload.get_mut("organization").map(Value::take);
    let Some(org @ Value::Object(_)) = org else {
        bail!("response for /organization has no organization");
    };
    Ok(vec![project(org, ORG_FIELDS, a.out.all)])
}

pub fn accounts(a: AccountsArgs) -> Result<Vec<Value>> {
    let mut records: Vec<Value> = Client::from_env()?
        .list_all("accounts", &Query::new())?
        .into_iter()
        .filter(|r| a.archived || !is_true(r, "isArchived"))
        .map(|r| project(r, ACCOUNT_FIELDS, a.out.all))
        .collect();
    records.sort_by(|x, y| record::compare(x.get("accountNo"), y.get("accountNo")));
    Ok(records)
}

pub fn postings(a: PostingsArgs) -> Result<Vec<Value>> {
    let client = Client::from_env()?;
    let accounts = client.list_all("accounts", &Query::new())?;
    let account_no: HashMap<String, Value> = accounts
        .iter()
        .filter_map(|acc| Some((record::text(acc.get("id")?), acc.get("accountNo")?.clone())))
        .collect();

    let mut q = Vec::new();
    if let Some(account) = &a.account {
        q.push(("accountId", resolve_account(account, &accounts)?));
    }
    if let Some(text) = &a.q {
        q.push(("q", text.clone()));
    }
    if a.exclude_voided {
        q.push(("isVoided", "false".to_string()));
    }
    a.dates.push_query(&mut q)?;

    let mut records = client.list_all("postings", &q)?;
    for r in &mut records {
        let no = r
            .get("accountId")
            .and_then(|id| account_no.get(&record::text(id)))
            .cloned();
        let signed = signed_amount(r);
        if let Value::Object(map) = r {
            map.insert("accountNo".into(), no.unwrap_or(Value::Null));
            map.insert(
                "signedAmount".into(),
                signed.map(record::number).unwrap_or(Value::Null),
            );
        }
    }
    Ok(records
        .into_iter()
        .map(|r| project(r, POSTING_FIELDS, a.out.all))
        .collect())
}

pub fn invoices(a: DocArgs) -> Result<Vec<Value>> {
    documents("invoices", INVOICE_FIELDS, a)
}

pub fn bills(a: DocArgs) -> Result<Vec<Value>> {
    documents("bills", BILL_FIELDS, a)
}

fn documents(resource: &str, fields: &[&str], a: DocArgs) -> Result<Vec<Value>> {
    let mut q = Vec::new();
    if let Some(state) = a.state {
        q.push(("state", state.as_str().to_string()));
    }
    if a.unpaid || a.paid {
        q.push(("isPaid", a.paid.to_string()));
    }
    if let Some(contact) = &a.contact {
        q.push(("contactId", contact.clone()));
    }
    if let Some(text) = &a.q {
        q.push(("q", text.clone()));
    }
    a.dates.push_query(&mut q)?;
    let records = Client::from_env()?.list_all(resource, &q)?;
    Ok(records
        .into_iter()
        .map(|r| project(r, fields, a.out.all))
        .collect())
}

pub fn contacts(a: ContactsArgs) -> Result<Vec<Value>> {
    let records = Client::from_env()?.list_all("contacts", &Query::new())?;
    Ok(records
        .into_iter()
        .filter(|r| a.archived || !is_true(r, "isArchived"))
        .filter(|r| !a.customers || is_true(r, "isCustomer"))
        .filter(|r| !a.suppliers || is_true(r, "isSupplier"))
        .map(|r| project(r, CONTACT_FIELDS, a.out.all))
        .collect())
}

pub fn daybook(a: DaybookArgs) -> Result<Vec<Value>> {
    let mut q = Vec::new();
    if let Some(state) = a.state {
        q.push(("state", state.as_str().to_string()));
    }
    if let Some(api_type) = &a.api_type {
        q.push(("apiType", api_type.clone()));
    }
    if let Some(daybook) = &a.daybook {
        q.push(("daybookId", daybook.clone()));
    }
    if let Some(text) = &a.q {
        q.push(("q", text.clone()));
    }
    a.dates.push_query(&mut q)?;
    let records = Client::from_env()?.list_all("daybookTransactions", &q)?;
    Ok(records
        .into_iter()
        .map(|r| project(r, DAYBOOK_FIELDS, a.out.all))
        .collect())
}

/// Account numbers are digits; Billy ids never are.
fn resolve_account(account: &str, accounts: &[Value]) -> Result<String> {
    if !account.chars().all(|c| c.is_ascii_digit()) {
        return Ok(account.to_string());
    }
    accounts
        .iter()
        .find(|acc| acc.get("accountNo").map(record::text).as_deref() == Some(account))
        .and_then(|acc| acc.get("id").map(record::text))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no account with number {account}; list them with `billy accounts --archived`"
            )
        })
}

/// Debit positive, credit negative, so postings on an account sum to its balance.
fn signed_amount(posting: &Value) -> Option<Decimal> {
    let amount = record::decimal(posting.get("amount")?)?;
    match posting.get("side")?.as_str()? {
        "debit" => Some(amount),
        "credit" => Some(-amount),
        _ => None,
    }
}

fn is_true(record: &Value, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn project(record: Value, fields: &[&str], all: bool) -> Value {
    if all {
        return record;
    }
    let Value::Object(mut map) = record else {
        return record;
    };
    let mut out = Map::new();
    for &field in fields {
        if let Some(value) = map.remove(field) {
            out.insert(field.to_string(), value);
        }
    }
    Value::Object(out)
}

pub fn daybooks(a: OrgArgs) -> Result<Vec<Value>> {
    let records = Client::from_env()?.list_all("daybooks", &Query::new())?;
    Ok(records
        .into_iter()
        .map(|r| project(r, DAYBOOKS_FIELDS, a.out.all))
        .collect())
}
