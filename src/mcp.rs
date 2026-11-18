use crate::checkout::Checkout;
use crate::store::{MAX_WAIT, Scope, Store};
use crate::{MAX_ITEMS, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const MAX_PENDING_WAITS: usize = 16;

pub fn config(checkout: &Checkout, client: &str) -> Result<Value> {
    let exe = std::env::current_exe()?;
    let root = checkout
        .root
        .to_str()
        .ok_or("Checkout path must be UTF-8")?;
    let command = exe.to_str().ok_or("Executable path must be UTF-8")?;
    let args = ["serve", root];
    let local = || json!({"type":"local", "command":[command, "serve", root]});
    match client {
        "mcp" => Ok(json!({"mcpServers":{"agent_coordinator":{
            "type":"stdio", "command":command, "args":args
        }}})),
        "opencode" => Ok(json!({"mcp":{"servers":{"agent_coordinator":local()}}})),
        "opencode-v1" => Ok(json!({"mcp":{"agent_coordinator":{
            "type":"local", "command":[command, "serve", root], "enabled":true, "timeout":30000
        }}})),
        "deepseek" => Ok(json!({"transport":"stdio", "command":command, "args":args})),
        _ => Err("Config format must be codex, mcp, opencode, opencode-v1, or deepseek.".into()),
    }
}

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
                            let reply = match Store::open(&checkout.database) {
                                Ok(mut store) => dispatch(&mut store, checkout, request),
                                Err(error) => Some(tool_reply(request["id"].clone(), Err(error))),
                            };
                            if let Some(reply) = reply {
                                let _ = write_reply(&reply);
                            }
                            pending.fetch_sub(1, Ordering::Relaxed);
                        });
                        None
                    }
                }
                Ok(request) => dispatch(&mut store, checkout, request),
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
                "instructions": "Acquire all affected checkout-relative paths together before writing. On contention wait up to 15 seconds per call, and re-read files once acquired. Release after commit or handoff. Scope git coordinates index and commit operations; scope checkout coordinates operations such as switching branches or merging. Commit only your own paths. With the Agent Coordinator Codex hooks enabled, omit owner. Otherwise call new_session once per chat and reuse its owner value on every acquire and release, including after reconnecting. Never share a chat owner with another chat."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call(
                store,
                checkout,
                params["name"].as_str().unwrap_or_default(),
                args,
            );
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

fn call(store: &mut Store, checkout: &Checkout, name: &str, args: Value) -> Result<Value> {
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
        "new_session" if args.as_object().is_some_and(|args| args.is_empty()) => {
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

fn tools() -> Value {
    let owner = json!({"type":"string", "description":"The owner returned by new_session for this chat. Omit only when the Agent Coordinator Codex hooks are enabled."});
    json!([
        {"name":"new_session", "description":"Generate an owner ID for this chat. Use once per chat and reuse the ID for acquire and release, even after a reconnect.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true}},
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
