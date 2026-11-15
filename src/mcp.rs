//! `billy mcp`: serves billy to AI assistants over the Model Context Protocol
//! (JSON-RPC, one message per line on stdio). It offers one tool, `billy`,
//! which runs a pipeline exactly as `billy run` does, so writes stay gated by
//! BILLY_RUN_WRITES.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::CommandFactory;
use serde_json::{json, Value};

use crate::commands::{Cli, Input};

/// Newest first; an unknown client version gets the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_OUTPUT_BYTES: usize = 100_000;

pub fn serve() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(out, "{}", serde_json::to_string(&reply)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The reply to one message; notifications and responses get none.
fn handle(message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    let id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": [tool()] }),
        "tools/call" => {
            if params.get("name").and_then(Value::as_str) != Some("billy") {
                return Some(error(id, -32602, "unknown tool; the only tool is `billy`"));
            }
            let Some(command) = params.pointer("/arguments/command").and_then(Value::as_str) else {
                return Some(error(id, -32602, "missing string argument `command`"));
            };
            let (text, is_error) = call(command);
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "billy", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Use the `billy` tool to work with the Billy accounting books. \
            Start with a source command and pipe it through filters, e.g. \
            `postings --period 2025 | sum signedAmount --by accountNo | table`. \
            Every API resource is reachable: `resources` lists them, `list`/`get` read \
            any of them, and `create`/`update`/`delete`/`call` describe changes that \
            `apply` dry-runs. Show the user the dry run before applying. \
            Append --help to any command for its flags.",
    })
}

fn tool() -> Value {
    let help = Cli::command().render_help().to_string();
    json!({
        "name": "billy",
        "title": "Billy accounting",
        "description": format!(
            "Runs a pipeline of billy commands against the Billy accounting API and returns \
             its output. Write the pipeline without the `billy run` prefix, stages separated \
             by |. Sources print one JSON record per line; filters transform them; `table` \
             renders the result. Every Billy API resource is reachable through `list`, `get`, \
             `create`, `update`, `delete` and `call`. Changes are only made by \
             `apply --confirm-cvr`, which is refused unless the person running this server \
             allowed writes (deletes and sending emails/e-invoices need separate permission).\n\n{help}"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "A billy pipeline, e.g. 'accounts | where name~bank | table'"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": false,
            "idempotentHint": false,
            "openWorldHint": true
        }
    })
}

/// Runs one pipeline with empty input: stdin is the protocol stream, not data.
fn call(command: &str) -> (String, bool) {
    let mut buffer = Vec::new();
    let (result, notes) = crate::notes::capture(|| {
        crate::pipeline::run(&unwrap_run(command), Input::Bytes(Vec::new()), &mut buffer)
    });
    let (text, is_error) = match result {
        Ok(()) => {
            let text = String::from_utf8_lossy(&buffer).into_owned();
            if text.is_empty() {
                ("(no output)".to_string(), false)
            } else {
                (truncate(text), false)
            }
        }
        Err(e) => (format!("{e:#}"), true),
    };
    if notes.is_empty() {
        return (text, is_error);
    }
    (
        format!("{}\n\n{}", text.trim_end(), notes.join("\n")),
        is_error,
    )
}

/// Models often write `billy run '<pipeline>'` out of habit; accept it.
fn unwrap_run(command: &str) -> String {
    if let Some(words) = shlex::split(command) {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        if let ["billy", "run", pipeline] | ["run", pipeline] = words.as_slice() {
            return pipeline.to_string();
        }
    }
    command.to_string()
}

fn truncate(text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = text[..cut].rfind('\n').map_or(cut, |i| i + 1);
    format!(
        "{}… output truncated at {cut} of {} bytes; narrow it with where, pick or head",
        &text[..cut],
        text.len()
    )
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> Value {
        handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).unwrap()
    }

    fn call_tool(command: &str) -> Value {
        request(
            "tools/call",
            json!({ "name": "billy", "arguments": { "command": command } }),
        )
    }

    #[test]
    fn negotiates_protocol_version() {
        let r = request("initialize", json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = request("initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply() {
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none()
        );
    }

    #[test]
    fn lists_one_tool_with_the_command_reference() {
        let r = request("tools/list", json!({}));
        let tool = &r["result"]["tools"][0];
        assert_eq!(tool["name"], "billy");
        assert!(tool["description"].as_str().unwrap().contains("postings"));
    }

    #[test]
    fn runs_pipelines_on_empty_input() {
        let r = call_tool("count");
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["content"][0]["text"], "{\"count\":0}\n");
        let r = call_tool("billy run 'count | table'");
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("count"));
    }

    #[test]
    fn reports_command_errors_as_tool_errors() {
        let r = call_tool("count --bye x");
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("--by"));
    }

    #[test]
    fn rejects_unknown_tools_and_methods() {
        let r = request("tools/call", json!({ "name": "other", "arguments": {} }));
        assert_eq!(r["error"]["code"], -32602);
        let r = request("resources/list", json!({}));
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn notes_reach_the_assistant() {
        let (_, notes) = crate::notes::capture(|| crate::notes::note("hello"));
        assert_eq!(notes, ["hello"]);
    }

    #[test]
    fn truncates_long_output_at_a_line() {
        let text = "x".repeat(60_000) + "\n" + &"y".repeat(60_000) + "\n";
        let out = truncate(text);
        assert!(out.starts_with(&"x".repeat(60_000)));
        assert!(out.contains("output truncated at 60001 of 120002 bytes"));
    }
}
