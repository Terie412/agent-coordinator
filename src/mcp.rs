use crate::checkout::Checkout;
use crate::store::{MAX_WAIT, Scope, Store};
use crate::{MAX_ITEMS, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acquire {
    owner: Option<String>,
    #[serde(default)]
    scope: Scope,
    paths: Option<Vec<String>>,
    #[serde(default)]
    wait_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    owner: Option<String>,
    lock_ids: Vec<i64>,
}

pub fn serve(checkout: &Checkout) -> Result<()> {
    let mut store = Store::open(&checkout.database)?;
    let mut output = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let message = serde_json::from_str::<Value>(&line?);
        let reply = match message {
            Ok(request) => dispatch(&mut store, checkout, request),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn dispatch(store: &mut Store, checkout: &Checkout, request: Value) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str();
    if request["jsonrpc"] != "2.0" || method.is_none() {
        return Some(error(id, -32600, "Invalid request"));
    }
    // Notifications (including initialized and cancelled) have no response.
    request.get("id")?;
    let params = &request["params"];
    let result = match method? {
        "initialize" => {
            let Some(version) = params["protocolVersion"].as_str() else {
                return Some(error(id, -32602, "protocolVersion is required"));
            };
            let version = if PROTOCOLS.contains(&version) {
                version
            } else {
                PROTOCOLS[3]
            };
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": env!("CARGO_PKG_NAME"), "title": "Agent Coordinator", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Read freely. Acquire all affected checkout-relative file paths together before editing, with wait_seconds up to 15 on contention. Re-read after acquisition. Release after the commit or handoff. Git index operations require scope git, and operations changing the checkout require scope checkout. Commit only this session's files. The Codex hooks supply owner."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call(
                store,
                checkout,
                params["name"].as_str().unwrap_or_default(),
                args,
            ) {
                Ok(value) => {
                    json!({"content": [{"type":"text", "text":value.to_string()}], "structuredContent":value})
                }
                Err(error) => {
                    json!({"isError": true, "content": [{"type":"text", "text":error.to_string()}]})
                }
            }
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0", "id":id, "result":result}))
}

fn call(store: &mut Store, checkout: &Checkout, name: &str, args: Value) -> Result<Value> {
    match name {
        "acquire" => {
            let args: Acquire = serde_json::from_value(args)?;
            let paths = match (args.scope, args.paths) {
                (Scope::Files, Some(paths)) => checkout.files(&paths, &checkout.root)?,
                (Scope::Files, None) => return Err("paths is required for scope files".into()),
                (_, Some(_)) => return Err("Only scope files accepts paths".into()),
                (_, None) => BTreeMap::new(),
            };
            store.acquire(
                args.owner.as_deref().unwrap_or_default(),
                args.scope,
                paths,
                args.wait_seconds,
            )
        }
        "status" if args.as_object().is_some_and(|args| args.is_empty()) => {
            store.status(&checkout.root)
        }
        "status" => Err("status does not take arguments".into()),
        "release" => {
            let args: Release = serde_json::from_value(args)?;
            store.release(args.owner.as_deref().unwrap_or_default(), &args.lock_ids)
        }
        _ => Err("Unknown tool".into()),
    }
}

fn tools() -> Value {
    let owner = json!({"type":"string", "description":"Omit in Codex. Its hook supplies the actual session ID."});
    json!([
        {"name":"acquire", "description":"Atomically claim a file group, Git index, or the whole checkout. On conflict no new locks are acquired. Re-read files after acquisition.",
            "inputSchema":{"type":"object", "additionalProperties":false, "properties":{
                "owner":owner, "scope":{"type":"string","enum":["files","git","checkout"],"default":"files"},
                "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_ITEMS},
                "wait_seconds":{"type":"integer","minimum":0,"maximum":MAX_WAIT,"default":0}
            }}},
        {"name":"status", "description":"View held locks, session IDs, and pending recovery (ended_at).",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true}},
        {"name":"release", "description":"Release your exact lock IDs after commit or a safe handoff. Missing IDs are reported as already absent.",
            "inputSchema":{"type":"object","additionalProperties":false,"required":["lock_ids"],"properties":{
                "owner":owner,"lock_ids":{"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":MAX_ITEMS}
            }}}
    ])
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}
