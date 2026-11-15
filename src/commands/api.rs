//! Generic access to every Billy resource, so nothing the API can do is out of
//! reach. `list`, `get` and `resources` read; `create`, `update`, `delete` and
//! `call` only describe changes, which `apply` checks and executes.

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Args;
use serde_json::{json, Map, Value};

use crate::client::{Client, Query};

/// What one Billy resource supports.
pub struct Resource {
    /// Plural name as used in URLs and list responses, e.g. `invoiceLines`.
    pub name: &'static str,
    pub get: bool,
    pub list: bool,
    pub create: bool,
    pub update: bool,
    pub delete: bool,
    /// Whether records belong to the organization (have `organizationId`).
    pub org_scoped: bool,
}

const fn r(name: &'static str, ops: &str, org_scoped: bool) -> Resource {
    let b = ops.as_bytes();
    Resource {
        name,
        get: b[0] == b'g',
        list: b[1] == b'l',
        create: b[2] == b'c',
        update: b[3] == b'u',
        delete: b[4] == b'd',
        org_scoped,
    }
}

/// Every resource in Billy's v2 API documentation, with its operations
/// (g=get one, l=list, c=create, u=update, d=delete).
pub const RESOURCES: &[Resource] = &[
    r("accountGroups", "glcud", true),
    r("accountNatures", "glcu-", false),
    r("accounts", "glcud", true),
    r("attachments", "glcud", true),
    r("balanceModifiers", "glcu-", false),
    r("bankLineMatches", "glcud", false),
    r("bankLines", "glcud", false),
    r("bankLineSubjectAssociations", "glcud", false),
    r("bankPayments", "glcud", true),
    r("billLines", "glcud", false),
    r("bills", "glcud", true),
    r("cities", "glcu-", false),
    r("contactBalancePayments", "glcu-", true),
    r("contactBalancePostings", "glcu-", false),
    r("contactPersons", "glcud", false),
    r("contacts", "glcud", true),
    r("countryGroups", "glcu-", false),
    r("countries", "glcu-", false),
    r("currencies", "glcu-", false),
    r("daybookBalanceAccounts", "glcud", false),
    r("daybooks", "glcud", true),
    r("daybookTransactionLines", "glcud", false),
    r("daybookTransactions", "glcud", true),
    r("files", "glc--", false),
    r("invoiceDeliveries", "--c--", true),
    r("invoiceLateFees", "glcu-", false),
    r("invoiceLines", "glcud", false),
    r("invoiceLogs", "-l---", true),
    r("invoiceReminderAssociations", "glcud", false),
    r("invoiceReminders", "glc--", true),
    r("invoices", "glcud", true),
    r("locales", "glcu-", false),
    r("organizations", "glcu-", false),
    r("postings", "glcu-", true),
    r("productPrices", "glcud", false),
    r("products", "glcud", true),
    r("salesTaxAccounts", "glcud", true),
    r("salesTaxMetaFields", "glcud", true),
    r("salesTaxPayments", "glcu-", false),
    r("salesTaxReturns", "gl-u-", true),
    r("salesTaxRules", "glcud", false),
    r("salesTaxRulesets", "glcud", true),
    r("states", "glcu-", false),
    r("taxRateDeductionComponents", "glcud", false),
    r("taxRates", "glcud", true),
    r("transactions", "glcud", true),
    r("users", "gl-u-", false),
    r("zipcodes", "glcu-", false),
];

/// Single-record endpoints that need no id.
const SINGLETONS: &[&str] = &["organization", "user"];

impl Resource {
    /// The singular name Billy uses as the root key of one record.
    pub fn singular(&self) -> String {
        singular(self.name)
    }

    fn ops(&self) -> String {
        [
            (self.get, "get"),
            (self.list, "list"),
            (self.create, "create"),
            (self.update, "update"),
            (self.delete, "delete"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, op)| *op)
        .collect::<Vec<_>>()
        .join(", ")
    }

    pub fn require(&self, op: &str) -> Result<()> {
        let ok = match op {
            "get" => self.get,
            "list" => self.list,
            "create" => self.create,
            "update" => self.update,
            "delete" => self.delete,
            _ => false,
        };
        if !ok {
            bail!(
                "Billy does not support {op} on {}; it supports: {}",
                self.name,
                self.ops()
            );
        }
        Ok(())
    }
}

pub fn singular(plural: &str) -> String {
    if let Some(stem) = plural.strip_suffix("ies") {
        format!("{stem}y")
    } else if ["ches", "shes", "sses", "xes"]
        .iter()
        .any(|e| plural.ends_with(e))
    {
        plural[..plural.len() - 2].to_string()
    } else {
        plural.strip_suffix('s').unwrap_or(plural).to_string()
    }
}

/// Finds a resource by its plural or singular name, ignoring case and dashes,
/// so `invoice-lines`, `invoiceLine` and `invoicelines` all work.
pub fn resource(name: &str) -> Result<&'static Resource> {
    let norm = |s: &str| s.replace(['-', '_'], "").to_ascii_lowercase();
    let wanted = norm(name);
    RESOURCES
        .iter()
        .find(|r| norm(r.name) == wanted || norm(&r.singular()) == wanted)
        .ok_or_else(|| anyhow!("unknown resource `{name}`; `billy resources` lists all of them"))
}

/// Parses FIELD arguments: `key=text` sets a string, `key:=json` sets raw JSON
/// (numbers, true/false, null, arrays, objects). Dotted keys nest:
/// `address.city=Aarhus`.
pub fn parse_fields(args: &[String], base: Option<&str>) -> Result<Map<String, Value>> {
    let mut map = match base {
        Some(raw) => match serde_json::from_str(raw).context("--json is not valid JSON")? {
            Value::Object(m) => m,
            _ => bail!("--json must be a JSON object"),
        },
        None => Map::new(),
    };
    for arg in args {
        let (key, value) = parse_field(arg)?;
        let mut parts = key.split('.').peekable();
        let mut cur = &mut map;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                cur.insert(part.to_string(), value);
                break;
            }
            let next = cur
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !next.is_object() {
                *next = Value::Object(Map::new());
            }
            cur = next.as_object_mut().expect("made an object above");
        }
    }
    Ok(map)
}

fn parse_field(arg: &str) -> Result<(String, Value)> {
    let eq = arg
        .find('=')
        .ok_or_else(|| anyhow!("`{arg}` must be key=text or key:=json"))?;
    let (key, raw) = (&arg[..eq], &arg[eq + 1..]);
    let (key, value) = match key.strip_suffix(':') {
        Some(k) => (
            k,
            serde_json::from_str(raw)
                .with_context(|| format!("`{arg}`: the value after := must be JSON"))?,
        ),
        None => (key, Value::String(raw.to_string())),
    };
    if key.is_empty() || key.split('.').any(str::is_empty) {
        bail!("`{arg}` has an empty key");
    }
    Ok((key.to_string(), value))
}

fn is_field(arg: &str) -> bool {
    arg.contains('=')
}

// ---------------------------------------------------------------- reading

#[derive(Args, Debug)]
pub struct ResourcesArgs {}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Resource name, e.g. invoices, bankLines, products (see `billy resources`)
    resource: String,
    /// Query parameters as key=value, passed to Billy as-is: filters (contactId=…,
    /// state=approved, q=…), include=invoice.lines:embed, sortProperty=…,
    /// sortDirection=DESC. Billy ignores names it does not know.
    #[arg(value_name = "KEY=VALUE")]
    params: Vec<String>,
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Resource name, or `organization` / `user` for the token's own
    resource: String,
    /// Record ids, then optional KEY=VALUE query parameters (e.g.
    /// include=invoice.lines:embed). Without ids, the ids of piped records are used.
    #[arg(value_name = "ID|KEY=VALUE")]
    args: Vec<String>,
}

pub fn resources(_: ResourcesArgs) -> Vec<Value> {
    RESOURCES
        .iter()
        .map(|r| {
            json!({
                "resource": r.name,
                "singular": r.singular(),
                "get": r.get,
                "list": r.list,
                "create": r.create,
                "update": r.update,
                "delete": r.delete,
            })
        })
        .collect()
}

fn query(params: &[String]) -> Result<Vec<(String, String)>> {
    params
        .iter()
        .map(|p| {
            p.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("query parameter `{p}` must be key=value"))
        })
        .collect()
}

pub fn list(a: ListArgs) -> Result<Vec<Value>> {
    let res = resource(&a.resource)?;
    res.require("list")?;
    let q = query(&a.params)?;
    let client = Client::from_env()?;
    // An explicit page is fetched as asked; otherwise every page is.
    if q.iter().any(|(k, _)| k == "page") {
        let mut payload = client.get(res.name, &q)?;
        return match payload.get_mut(res.name).map(Value::take) {
            Some(Value::Array(records)) => Ok(records),
            _ => bail!("response for /{} has no '{}' list", res.name, res.name),
        };
    }
    client.list_all(res.name, &q)
}

pub fn get(a: GetArgs, piped: impl FnOnce() -> Result<Vec<Value>>) -> Result<Vec<Value>> {
    let (params, ids): (Vec<String>, Vec<String>) = a.args.into_iter().partition(|s| is_field(s));
    let q = query(&params)?;
    let client = Client::from_env()?;
    let wanted = a.resource.to_ascii_lowercase();
    if let Some(single) = SINGLETONS.iter().find(|s| **s == wanted) {
        if !ids.is_empty() {
            bail!("`get {single}` takes no id; it returns the token's own {single}");
        }
        let mut payload = client.get(single, &q)?;
        return Ok(vec![payload
            .get_mut(*single)
            .map(Value::take)
            .unwrap_or_default()]);
    }
    let res = resource(&a.resource)?;
    res.require("get")?;
    let ids = if ids.is_empty() {
        ids_of(piped()?, "get")?
    } else {
        ids
    };
    let key = res.singular();
    ids.iter()
        .map(|id| {
            let mut payload = client.get(&format!("{}/{id}", res.name), &q)?;
            payload
                .get_mut(&key)
                .map(Value::take)
                .filter(Value::is_object)
                .ok_or_else(|| anyhow!("Billy returned no {key} for id {id}"))
        })
        .collect()
}

fn ids_of(records: Vec<Value>, command: &str) -> Result<Vec<String>> {
    if records.is_empty() {
        bail!("`{command}` needs record ids: give them as arguments or pipe in records with an id");
    }
    records
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r.get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("piped record {} has no id", i + 1))
        })
        .collect()
}

/// `call GET …` reads straight away; other methods become a change for `apply`.
pub fn call_get(path: &str, body: &Map<String, Value>) -> Result<Vec<Value>> {
    let q: Query = Query::new();
    if !body.is_empty() {
        bail!("`call GET` takes no body; put query parameters in the path, e.g. invoiceLogs?invoiceId=…");
    }
    let payload = Client::from_env()?.get(path, &q)?;
    Ok(vec![payload])
}

// ---------------------------------------------------------------- describing changes

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Resource name, e.g. contacts, bills, invoices, bankPayments
    resource: String,
    /// Fields as key=text or key:=json; dotted keys nest (lines:='[{…}]', address.city=Aarhus)
    #[arg(value_name = "FIELD")]
    fields: Vec<String>,
    /// The whole record as a JSON object; FIELD arguments are applied on top
    #[arg(long, value_name = "OBJECT")]
    json: Option<String>,
    /// Skip the create if a record with the same values of these fields exists (e.g. --unique registrationNo)
    #[arg(long, value_name = "FIELDS", value_delimiter = ',')]
    unique: Vec<String>,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Resource name, e.g. contacts, invoices
    resource: String,
    /// An optional record id, then fields as key=text or key:=json. Without an id,
    /// every piped record is updated (e.g. state=approved to approve drafts).
    #[arg(value_name = "ID|FIELD")]
    args: Vec<String>,
    /// The changes as a JSON object; FIELD arguments are applied on top
    #[arg(long, value_name = "OBJECT")]
    json: Option<String>,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Resource name
    resource: String,
    /// Record ids; without them, the ids of piped records are used
    #[arg(value_name = "ID")]
    ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CallArgs {
    /// HTTP method: GET reads now; POST, PUT, PATCH and DELETE are applied by `apply`
    method: String,
    /// Path under /v2, e.g. invoices/<id>/emails or invoiceLogs?invoiceId=<id>
    path: String,
    /// Body fields as key=text or key:=json (e.g. email.emailSubject=…)
    #[arg(value_name = "FIELD")]
    fields: Vec<String>,
    /// The whole body as a JSON object; FIELD arguments are applied on top
    #[arg(long, value_name = "OBJECT")]
    json: Option<String>,
}

pub fn create(a: CreateArgs) -> Result<Vec<Value>> {
    let res = resource(&a.resource)?;
    res.require("create")?;
    let record = parse_fields(&a.fields, a.json.as_deref())?;
    if record.is_empty() {
        bail!(
            "give the new record's fields, e.g. `create contacts name='Acme ApS' isCustomer:=true`"
        );
    }
    for f in &a.unique {
        if !record.contains_key(f) {
            bail!("--unique {f}: the record has no field {f}");
        }
    }
    let mut op = json!({ "op": "create", "resource": res.name, "record": record });
    if !a.unique.is_empty() {
        op["unique"] = json!(a.unique);
    }
    Ok(vec![op])
}

pub fn update(a: UpdateArgs, piped: impl FnOnce() -> Result<Vec<Value>>) -> Result<Vec<Value>> {
    let res = resource(&a.resource)?;
    res.require("update")?;
    let (fields, ids): (Vec<String>, Vec<String>) = a.args.into_iter().partition(|s| is_field(s));
    if ids.len() > 1 {
        bail!("give at most one id; to update many records, pipe them in");
    }
    let changes = parse_fields(&fields, a.json.as_deref())?;
    if changes.is_empty() {
        bail!("give the fields to change, e.g. `update contacts <id> email=a@b.dk`");
    }
    if changes.contains_key("id") {
        bail!("the id cannot be changed");
    }
    let ids = if ids.is_empty() {
        ids_of(piped()?, "update")?
    } else {
        ids
    };
    Ok(ids
        .into_iter()
        .map(|id| json!({ "op": "update", "resource": res.name, "id": id, "changes": changes }))
        .collect())
}

pub fn delete(a: DeleteArgs, piped: impl FnOnce() -> Result<Vec<Value>>) -> Result<Vec<Value>> {
    let res = resource(&a.resource)?;
    res.require("delete")?;
    let ids = if a.ids.is_empty() {
        ids_of(piped()?, "delete")?
    } else {
        a.ids
    };
    Ok(ids
        .into_iter()
        .map(|id| json!({ "op": "delete", "resource": res.name, "id": id }))
        .collect())
}

pub fn call(a: CallArgs) -> Result<Vec<Value>> {
    let method = a.method.to_ascii_uppercase();
    let path = clean_path(&a.path)?;
    let body = parse_fields(&a.fields, a.json.as_deref())?;
    match method.as_str() {
        "GET" => call_get(&path, &body),
        "POST" | "PUT" | "PATCH" | "DELETE" => {
            let mut op = json!({ "op": "call", "method": method, "path": path });
            if !body.is_empty() {
                op["body"] = Value::Object(body);
            }
            Ok(vec![op])
        }
        other => bail!("unsupported method {other}; use GET, POST, PUT, PATCH or DELETE"),
    }
}

/// A path relative to /v2, without a leading slash.
pub fn clean_path(path: &str) -> Result<String> {
    let p = path.trim();
    let p = p
        .strip_prefix("https://api.billysbilling.com")
        .unwrap_or(p)
        .trim_start_matches('/');
    let p = p.strip_prefix("v2/").unwrap_or(p);
    if p.is_empty() || p.contains("..") || p.contains("://") {
        bail!("`{path}` is not a Billy API path; write it like invoices/<id>/emails");
    }
    Ok(p.to_string())
}

/// Whether a change sends something to people outside the company (emails,
/// e-invoices, reminders), which cannot be taken back.
pub fn sends(method: &str, path: &str) -> bool {
    if method == "GET" {
        return false;
    }
    let base = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = base.split('/').collect();
    matches!(
        segments.first(),
        Some(&"invoiceDeliveries" | &"invoiceReminders")
    ) || segments.last() == Some(&"emails")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolves_resource_names_loosely() {
        assert_eq!(resource("invoice-lines").unwrap().name, "invoiceLines");
        assert_eq!(resource("Currency").unwrap().name, "currencies");
        assert_eq!(resource("bankLineMatch").unwrap().name, "bankLineMatches");
        assert!(resource("widgets").is_err());
        assert_eq!(singular("cities"), "city");
        assert_eq!(singular("invoiceDeliveries"), "invoiceDelivery");
        assert_eq!(singular("bankLineMatches"), "bankLineMatch");
        assert_eq!(singular("salesTaxRulesets"), "salesTaxRuleset");
    }

    #[test]
    fn every_resource_name_round_trips() {
        for r in RESOURCES {
            assert_eq!(resource(r.name).unwrap().name, r.name);
            assert_eq!(resource(&r.singular()).unwrap().name, r.name);
        }
    }

    #[test]
    fn parses_fields_with_types_and_nesting() {
        let m = parse_fields(
            &args(&[
                "name=Acme ApS",
                "isCustomer:=true",
                "address.city=Aarhus",
                "x:=[1,2]",
                "note=a=b",
            ]),
            Some(r#"{"name":"old","countryId":"DK"}"#),
        )
        .unwrap();
        assert_eq!(
            Value::Object(m).to_string(),
            r#"{"name":"Acme ApS","countryId":"DK","isCustomer":true,"address":{"city":"Aarhus"},"x":[1,2],"note":"a=b"}"#
        );
        assert!(parse_fields(&args(&["noequals"]), None).is_err());
        assert!(parse_fields(&args(&["n:=notjson"]), None).is_err());
        assert!(parse_fields(&args(&["=x"]), None).is_err());
    }

    #[test]
    fn refuses_unsupported_operations() {
        let e = create(CreateArgs {
            resource: "salesTaxReturns".into(),
            fields: args(&["a=b"]),
            json: None,
            unique: vec![],
        })
        .unwrap_err();
        assert!(e.to_string().contains("does not support create"));
        assert!(delete(
            DeleteArgs {
                resource: "postings".into(),
                ids: args(&["1"])
            },
            || Ok(vec![])
        )
        .is_err());
    }

    #[test]
    fn describes_changes() {
        let out = update(
            UpdateArgs {
                resource: "invoices".into(),
                args: args(&["state=approved"]),
                json: None,
            },
            || Ok(vec![json!({"id": "a"}), json!({"id": "b"})]),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["id"], "b");
        assert_eq!(out[0]["changes"]["state"], "approved");
        let out = call(CallArgs {
            method: "post".into(),
            path: "/v2/invoices/x/emails".into(),
            fields: args(&["email.emailSubject=Hi"]),
            json: None,
        })
        .unwrap();
        assert_eq!(out[0]["path"], "invoices/x/emails");
        assert_eq!(out[0]["body"]["email"]["emailSubject"], "Hi");
    }

    #[test]
    fn spots_sending_and_bad_paths() {
        assert!(sends("POST", "invoices/x/emails"));
        assert!(sends("POST", "invoiceDeliveries"));
        assert!(!sends("POST", "contacts"));
        assert!(clean_path("../etc").is_err());
        assert!(clean_path("https://evil.example/x").is_err());
    }
}
