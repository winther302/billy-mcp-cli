use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const API_BASE: &str = "https://api.billysbilling.com/v2";
/// Billy's maximum page size. `BILLY_PAGE_SIZE` lowers it, to exercise paging.
const MAX_PAGE_SIZE: usize = 1000;

pub type Query = Vec<(&'static str, String)>;

pub struct Client {
    token: String,
    base: String,
    agent: ureq::Agent,
    page_size: usize,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(45))
            .build();
        let page_size = std::env::var("BILLY_PAGE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
            .unwrap_or(MAX_PAGE_SIZE);
        Ok(Self {
            token: crate::env::token()?,
            // BILLY_API_BASE points billy at a test server; set it only for testing.
            base: std::env::var("BILLY_API_BASE")
                .ok()
                .filter(|b| !b.trim().is_empty())
                .map(|b| b.trim_end_matches('/').to_string())
                .unwrap_or_else(|| API_BASE.to_string()),
            agent,
            page_size,
        })
    }

    pub fn get<K: AsRef<str>>(&self, path: &str, query: &[(K, String)]) -> Result<Value> {
        let mut req = self.request("GET", path);
        for (key, value) in query {
            req = req.query(key.as_ref(), value);
        }
        respond("GET", path, req.call())
    }

    /// Any request, for generic changes: `body` is sent as JSON when given.
    pub fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let req = self.request(method, path);
        let result = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        respond(method, path, result)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        respond("POST", path, self.request("POST", path).send_json(body))
    }

    /// Uploads raw file bytes (Billy's `POST /files`).
    pub fn post_file(&self, file_name: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let req = self
            .request("POST", "files")
            .set("Content-Type", content_type)
            .set("X-Filename", file_name);
        respond("POST", "files", req.send_bytes(bytes))
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        respond("PUT", path, self.request("PUT", path).send_json(body))
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}/{path}", self.base))
            .set("X-Access-Token", &self.token)
            .set("Accept", "application/json")
            .set(
                "User-Agent",
                concat!("billy-mcp-cli/", env!("CARGO_PKG_VERSION")),
            )
    }

    /// Every record of a list resource, following Billy's paging. Fails rather
    /// than return a partial list.
    pub fn list_all<K: AsRef<str>>(
        &self,
        resource: &str,
        query: &[(K, String)],
    ) -> Result<Vec<Value>> {
        let mut records = Vec::new();
        let mut total = None;
        for page in 1.. {
            let mut q: Vec<(&str, String)> = query
                .iter()
                .map(|(k, v)| (k.as_ref(), v.clone()))
                .filter(|(k, _)| *k != "pageSize")
                .collect();
            q.push(("page", page.to_string()));
            q.push(("pageSize", self.page_size.to_string()));
            let mut payload = self.get(resource, &q)?;
            let Some(Value::Array(chunk)) = payload.get_mut(resource).map(Value::take) else {
                bail!("response for /{resource} has no '{resource}' list");
            };
            let fetched = chunk.len();
            records.extend(chunk);
            let paging = |key: &str| {
                payload
                    .pointer(&format!("/meta/paging/{key}"))
                    .and_then(Value::as_u64)
            };
            total = paging("total").or(total);
            let more = match paging("pageCount") {
                Some(page_count) => page < page_count,
                None => fetched == self.page_size,
            };
            if !more || fetched == 0 {
                break;
            }
        }
        if let Some(total) = total {
            if records.len() as u64 != total {
                bail!(
                    "/{resource} returned {} records but Billy reports {total}; the data may have changed while paging, try again",
                    records.len()
                );
            }
        }
        Ok(records)
    }
}

fn respond(method: &str, path: &str, result: Result<ureq::Response, ureq::Error>) -> Result<Value> {
    match result {
        Ok(resp) => {
            // into_string caps at 10 MB; a page of 1000 records with embeds can be larger.
            let mut body = String::new();
            std::io::Read::read_to_string(&mut resp.into_reader(), &mut body)
                .with_context(|| format!("reading the response to {method} /{path}"))?;
            if body.trim().is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&body)
                .with_context(|| format!("invalid JSON from {method} /{path}"))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            bail!(
                "Billy API {code} for {method} /{path}: {}",
                error_message(&body)
            )
        }
        Err(e) => bail!("could not reach Billy for {method} /{path}: {e}"),
    }
}

/// Billy's error text if the body carries one, else the (shortened) body.
fn error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        ["errorMessage", "message", "error"]
            .iter()
            .find_map(|k| v.get(k).and_then(Value::as_str))
    });
    match message {
        Some(m) => m.to_string(),
        None => body.chars().take(500).collect(),
    }
}
