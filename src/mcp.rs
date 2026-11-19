use crate::checkout::Checkout;
use crate::store::{MAX_WAIT, Scope, Store};
use crate::{MAX_ITEMS, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const MAX_PENDING_WAITS: usize = 16;

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

pub fn serve(fixed: Option<&Checkout>) -> Result<()> {
    let pending = AtomicUsize::new(0);
    thread::scope(|scope| -> Result<()> {
        for line in io::stdin().lock().lines() {
            let reply = match serde_json::from_str::<Value>(&line?) {
                Ok(request) if waits_for_lock(&request) => {
                    if pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_WAITS {
                        pending.fetch_sub(1, Ordering::Relaxed);
                        Some(tool_reply(
                            request["id"].clone(),
                            Err("Too many lock waits. Try again shortly.".into()),
                        ))
                    } else {
                        // Other chats using the same connection must be able to release while this waits.
                        let pending = &pending;
                        scope.spawn(move || {
                            if let Some(reply) = dispatch(fixed, request) {
                                let _ = write_reply(&reply);
                            }
                            pending.fetch_sub(1, Ordering::Relaxed);
                        });
                        None
                    }
                }
                Ok(request) => dispatch(fixed, request),
                Err(_) => Some(error(Value::Null, -32700, "Parse error")),
            };
            if let Some(reply) = reply {
                write_reply(&reply)?;
            }
        }
        Ok(())
    })
}

fn waits_for_lock(request: &Value) -> bool {
    request.get("id").is_some()
        && request["jsonrpc"] == "2.0"
        && request["method"] == "tools/call"
        && request["params"]["name"] == "acquire"
        && request["params"]["arguments"]["wait_seconds"]
            .as_u64()
            .is_some_and(|seconds| (1..=MAX_WAIT).contains(&seconds))
}

fn write_reply(reply: &Value) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, reply)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn dispatch(fixed: Option<&Checkout>, request: Value) -> Option<Value> {
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
                "instructions": "Acquire all affected checkout-relative paths together before writing. On contention wait up to 15 seconds per call, and re-read files once acquired. Release after commit or handoff. Scope git coordinates index and commit operations; scope checkout coordinates operations such as switching branches or merging. Commit only your own paths. Leave the owner and checkout fields to the Codex hooks."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools(fixed.is_none())}),
        "tools/call" => {
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call(fixed, params["name"].as_str().unwrap_or_default(), args);
            return Some(tool_reply(id, result));
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0", "id":id, "result":result}))
}

fn tool_reply(id: Value, result: Result<Value>) -> Value {
    let payload = match result {
        Ok(value) => {
            json!({"content": [{"type":"text", "text":value.to_string()}], "structuredContent":value})
        }
        Err(error) => {
            json!({"isError": true, "content": [{"type":"text", "text":error.to_string()}]})
        }
    };
    json!({"jsonrpc":"2.0", "id":id, "result":payload})
}

fn call(fixed: Option<&Checkout>, name: &str, mut args: Value) -> Result<Value> {
    let routed;
    let checkout = if let Some(checkout) = fixed {
        checkout
    } else {
        let root = args
            .as_object_mut()
            .and_then(|args| args.remove("checkout"));
        let root = root
            .as_ref()
            .and_then(Value::as_str)
            .map(Path::new)
            .filter(|p| p.is_absolute())
            .ok_or("Open a local Git checkout in Codex and review the Agent Coordinator hooks.")?;
        routed = Checkout::find(root)?;
        &routed
    };
    let mut store = Store::open(&checkout.database)?;
    match name {
        "acquire" => {
            let args: Acquire = serde_json::from_value(args)?;
            store.acquire(
                checkout,
                args.owner.as_deref().unwrap_or_default(),
                args.scope,
                args.paths.as_deref(),
                args.wait_seconds,
            )
        }
        "status" if args.as_object().is_some_and(|args| args.is_empty()) => {
            store.status(&checkout.root)
        }
        "status" => Err("status does not take arguments".into()),
        "new_session"
            if fixed.is_some() && args.as_object().is_some_and(|args| args.is_empty()) =>
        {
            store.new_session()
        }
        "new_session" => Err("new_session does not take arguments".into()),
        "release" => {
            let args: Release = serde_json::from_value(args)?;
            store.release(args.owner.as_deref().unwrap_or_default(), &args.lock_ids)
        }
        _ => Err("Unknown tool".into()),
    }
}

fn tools(plugin: bool) -> Vec<Value> {
    let owner = json!({"type":"string", "description":"Supplied by the Codex hook."});
    let mut tools = vec![
        json!(
        {"name":"acquire", "description":"Atomically claim a file group, Git index, or the whole checkout. On conflict no new locks are acquired. Re-read files after acquisition.",
            "inputSchema":{"type":"object", "additionalProperties":false, "properties":{
                "owner":owner, "scope":{"type":"string","enum":["files","git","checkout"],"default":"files"},
                "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_ITEMS},
                "wait_seconds":{"type":"integer","minimum":0,"maximum":MAX_WAIT,"default":0}
            }}}),
        json!({"name":"status", "description":"View held locks, session IDs, and pending recovery (ended_at).",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true}}),
        json!({"name":"release", "description":"Release your exact lock IDs after commit or a safe handoff. Missing IDs are reported as already absent.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["lock_ids"],"properties":{
            "owner":owner,"lock_ids":{"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":MAX_ITEMS}
        }}}),
    ];
    if plugin {
        for tool in &mut tools {
            tool["inputSchema"]["properties"]["checkout"] =
                json!({"type":"string", "description":"Supplied by the Codex hook."});
        }
    } else {
        tools.push(
            json!({"name":"new_session", "description":"Generate an owner ID for this chat.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true}}),
        );
    }
    tools
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}
