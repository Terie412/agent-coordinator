use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXE: &str = env!("CARGO_BIN_EXE_agent-coordinator");
static NEXT: AtomicU32 = AtomicU32::new(0);

fn repo() -> PathBuf {
    let id = format!(
        "checkout {}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-repos")
        .join(id);
    fs::create_dir_all(&root).unwrap();
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    root
}

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn start(root: &Path) -> Self {
        Self::connect(&[root])
    }

    fn connect(roots: &[&Path]) -> Self {
        let mut child = Command::new(EXE)
            .arg("serve")
            .args(roots)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            input,
            output,
            next_id: 1,
        };
        client.send("initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"fixture", "version":"1"}}));
        assert_eq!(client.receive()["protocolVersion"], "2025-11-25");
        writeln!(
            client.input,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .unwrap();
        client
    }

    fn send(&mut self, method: &str, params: Value) {
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0", "id": self.next_id, "method":method, "params":params})
        )
        .unwrap();
        self.next_id += 1;
    }

    fn receive_reply(&mut self) -> Value {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["error"].is_null(), "{value}");
        value
    }

    fn receive(&mut self) -> Value {
        let value = self.receive_reply();
        assert_eq!(value["id"], self.next_id - 1);
        value["result"].clone()
    }

    fn send_tool(&mut self, name: &str, arguments: Value) {
        self.send("tools/call", json!({"name":name, "arguments":arguments}));
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.send_tool(name, arguments);
        let response = self.receive();
        assert_ne!(response["isError"], true, "{response}");
        response["structuredContent"].clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn independent_processes_wait_and_acquire_as_a_group() {
    let root = repo();
    let mut first = Client::start(&root);
    let mut second = Client::start(&root);
    first.send("tools/list", json!({}));
    assert_eq!(first.receive()["tools"].as_array().unwrap().len(), 4);

    first.send_tool(
        "acquire",
        json!({"owner":"first", "paths":["shared.txt", "first.txt"]}),
    );
    second.send_tool(
        "acquire",
        json!({"owner":"second", "paths":["./shared.txt", "second.txt"]}),
    );
    let a = first.receive()["structuredContent"].clone();
    let b = second.receive()["structuredContent"].clone();
    assert_ne!(a["acquired"], b["acquired"]);
    let (holder, waiter, held, hold_id, wait_id, untouched) = if a["acquired"] == true {
        (&mut first, &mut second, a, "first", "second", "second.txt")
    } else {
        (&mut second, &mut first, b, "second", "first", "first.txt")
    };
    let state = waiter.tool("status", json!({}));
    assert_eq!(state["locks"].as_array().unwrap().len(), 2);
    assert!(
        state["locks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|lock| lock["owner"] == hold_id)
    );
    assert_eq!(
        waiter.tool("acquire", json!({"owner":wait_id, "paths":[untouched]}))["acquired"],
        true
    );

    let id = held["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lock| lock["path"] == "shared.txt")
        .unwrap()["lock_id"]
        .clone();
    waiter.send_tool(
        "acquire",
        json!({"owner":wait_id, "paths":["shared.txt"], "wait_seconds":3}),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        let status = holder.tool("status", json!({}));
        if status["requests"][0].is_object() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Waiting request was never visible"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status["requests"][0]["owner"], wait_id);
    assert_eq!(status["requests"][0]["paths"], json!(["shared.txt"]));
    assert_eq!(status["requests"][0]["blocking_locks"], json!([id]));
    assert_eq!(
        holder.tool("release", json!({"owner":hold_id, "lock_ids":[id]}))["released"],
        json!([id])
    );
    assert_eq!(waiter.receive()["structuredContent"]["acquired"], true);
    assert_eq!(holder.tool("status", json!({}))["requests"], json!([]));
    assert_eq!(
        holder.tool("release", json!({"owner":hold_id, "lock_ids":[id]}))["already_absent"],
        json!([id])
    );
}

#[test]
fn terminal_and_mcp_chats_share_locks_even_over_a_shared_connection() {
    let root = repo();
    let run = |command: &str, args: &[&str]| {
        let output = Command::new(EXE)
            .arg(command)
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"));
        (output.status.code().unwrap(), payload)
    };
    let (code, terminal) = run("new-session", &[]);
    assert_eq!(code, 0);
    let owner = terminal["owner"].as_str().unwrap();
    let mut mcp = Client::start(&root);
    let first = mcp.tool("new_session", json!({}))["owner"].clone();
    let second = mcp.tool("new_session", json!({}))["owner"].clone();
    assert_ne!(first, second);
    assert_ne!(first, terminal["owner"]);
    mcp.send_tool("acquire", json!({"paths":["shared path.txt"]}));
    assert_eq!(mcp.receive()["isError"], true);

    let (code, grant) = run("acquire", &[owner, "--", "shared path.txt", "-other.txt"]);
    assert_eq!(code, 0);
    let id = grant["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lock| lock["path"] == "shared path.txt")
        .unwrap()["lock_id"]
        .clone();
    let (code, conflict) = run("acquire", &[first.as_str().unwrap(), "shared path.txt"]);
    assert_eq!(code, 2);
    assert_eq!(conflict["conflicts"][0]["owner"], owner);
    let held = mcp.tool("status", json!({}));
    assert_eq!(held["locks"].as_array().unwrap().len(), 2);

    let wait_id = mcp.next_id;
    mcp.send_tool(
        "acquire",
        json!({"owner":second, "paths":["shared path.txt"], "wait_seconds":3}),
    );
    mcp.send_tool("release", json!({"owner":owner, "lock_ids":[id]}));
    let replies = [mcp.receive_reply(), mcp.receive_reply()];
    let acquired = replies.iter().find(|reply| reply["id"] == wait_id).unwrap();
    assert_eq!(acquired["result"]["structuredContent"]["acquired"], true);

    let id = acquired["result"]["structuredContent"]["locks"][0]["lock_id"].to_string();
    drop(mcp);
    let (code, released) = run("release", &[second.as_str().unwrap(), &id]);
    assert_eq!(code, 0);
    assert_eq!(released["released"].as_array().unwrap().len(), 1);
}

fn hook(root: &Path, event: &str, owner: &str, tool: &str, input: Value) -> Value {
    run_hook(root, &[root], event, owner, tool, input)
}

fn run_hook(
    root: &Path,
    scope: &[&Path],
    event: &str,
    owner: &str,
    tool: &str,
    input: Value,
) -> Value {
    let mut child = Command::new(EXE)
        .arg("hook")
        .args(scope)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"cwd":root,
        "hook_event_name":event, "session_id":owner, "tool_name":tool, "tool_input":input})
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["hookSpecificOutput"].clone()
    }
}

#[test]
fn plugin_uses_codex_context_to_route_locks_to_the_correct_checkout() {
    let first = repo();
    let second = repo();
    let subfolder = first.join("src");
    fs::create_dir(&subfolder).unwrap();
    let mut mcp = Client::connect(&[]);
    mcp.send("tools/list", json!({}));
    assert_eq!(mcp.receive()["tools"].as_array().unwrap().len(), 3);
    mcp.send_tool("status", json!({}));
    assert_eq!(mcp.receive()["isError"], true);

    let route = |cwd: &Path, tool: &str, args: Value| {
        let name = format!("mcp__agent_coordinator__{tool}");
        run_hook(cwd, &[], "PreToolUse", "session", &name, args)["updatedInput"].clone()
    };
    let args = route(
        &subfolder,
        "acquire",
        json!({"owner":"wrong", "checkout":second, "paths":["src/shared.txt"]}),
    );
    assert_eq!(args["owner"], "session");
    let granted = mcp.tool("acquire", args);
    assert_eq!(granted["acquired"], true);
    assert_eq!(
        mcp.tool("status", route(&first, "status", json!({})))["locks"],
        granted["locks"]
    );
    assert_eq!(
        mcp.tool("status", route(&second, "status", json!({})))["locks"],
        json!([])
    );

    let patch =
        json!({"command":"*** Begin Patch\n*** Add File: shared.txt\n+hello\n*** End Patch"});
    assert!(
        run_hook(
            &subfolder,
            &[],
            "PreToolUse",
            "session",
            "apply_patch",
            patch.clone()
        )
        .is_null()
    );
    assert_eq!(
        run_hook(&second, &[], "PreToolUse", "session", "apply_patch", patch)["permissionDecision"],
        "deny"
    );
    let ids = json!({"lock_ids":[granted["locks"][0]["lock_id"]]});
    let released = mcp.tool("release", route(&subfolder, "release", ids));
    assert_eq!(released["released"].as_array().unwrap().len(), 1);
}

#[test]
fn hook_checks_rename_paths_and_identity_and_preserves_ended_locks() {
    let root = repo();
    fs::write(root.join("old.txt"), "old\n").unwrap();
    let mut client = Client::start(&root);
    let command =
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: 新文件.txt\n*** End Patch";
    let check = |owner| {
        hook(
            &root,
            "PreToolUse",
            owner,
            "apply_patch",
            json!({"command":command}),
        )
    };
    assert_eq!(check("session-a")["permissionDecision"], "deny");

    let request = hook(
        &root,
        "PreToolUse",
        "session-a",
        "mcp__agent_coordinator__acquire",
        json!({"owner":"other", "paths":["old.txt"]}),
    );
    assert_eq!(request["updatedInput"]["owner"], "session-a");
    let grant = client.tool("acquire", request["updatedInput"].clone());
    assert_eq!(check("session-a")["permissionDecision"], "deny");
    client.tool(
        "acquire",
        json!({"owner":"session-a", "paths":["新文件.txt"]}),
    );
    assert!(check("session-a").is_null());
    assert_eq!(check("session-b")["permissionDecision"], "deny");

    drop(client);
    hook(&root, "SessionEnd", "session-a", "", json!({}));
    let mut client = Client::start(&root);
    let denied = client.tool("acquire", json!({"owner":"session-b", "paths":["old.txt"]}));
    assert!(denied["conflicts"][0]["ended_at"].is_u64());
    assert_eq!(check("session-a")["permissionDecision"], "deny");
    let resumed = client.tool("acquire", request["updatedInput"].clone());
    assert_ne!(resumed["locks"][0]["lock_id"], grant["locks"][0]["lock_id"]);
    let stale = grant["locks"][0]["lock_id"].as_i64().unwrap().to_string();
    let result = Command::new(EXE)
        .arg("recover")
        .arg(&root)
        .args(["session-a", &stale])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        client.tool("status", json!({}))["locks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn read_only_shell_command_skips_git_metadata_lookup() {
    let root = repo();
    let nested = root.join("broken-checkout");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join(".git"), "invalid git metadata").unwrap();

    let git = |command| {
        run_hook(
            &nested,
            &[],
            "PreToolUse",
            "session",
            "Bash",
            json!({"command":command}),
        )
    };
    assert!(git("git status --short").is_null());
    assert_eq!(git("git switch main")["permissionDecision"], "deny");
}

#[test]
fn git_lock_serializes_index_operations_and_checkout_lock_excludes_other_owners() {
    let root = repo();
    let mut one = Client::start(&root);
    let mut two = Client::start(&root);
    let git = |owner, command| {
        hook(
            &root,
            "PreToolUse",
            owner,
            "Bash",
            json!({"command":command}),
        )
    };
    assert!(git("one", "git status --short").is_null());
    assert_eq!(
        git("one", "git commit --only -- a")["permissionDecision"],
        "deny"
    );
    assert_eq!(
        one.tool("acquire", json!({"owner":"one", "scope":"git"}))["acquired"],
        true
    );
    assert!(git("one", "git commit --only -- a").is_null());
    assert_eq!(
        two.tool("acquire", json!({"owner":"two", "scope":"git"}))["acquired"],
        false
    );
    assert_eq!(
        two.tool("acquire", json!({"owner":"two", "paths":["a"]}))["acquired"],
        true
    );
    assert_eq!(
        one.tool("acquire", json!({"owner":"one", "scope":"checkout"}))["acquired"],
        false
    );
    assert_eq!(git("one", "git switch main")["permissionDecision"], "deny");
    if cfg!(windows) {
        assert_eq!(
            one.tool("acquire", json!({"owner":"one", "paths":["A"]}))["acquired"],
            false
        );
    }
    let id = two.tool("status", json!({}))["locks"][1]["lock_id"].clone();
    let wrong = one.tool("status", json!({}))["locks"][0]["lock_id"].clone();
    two.send_tool("release", json!({"owner":"two", "lock_ids":[id, wrong]}));
    assert_eq!(two.receive()["isError"], true);
    assert_eq!(
        two.tool("release", json!({"owner":"two", "lock_ids":[id]}))["released"],
        json!([id])
    );
    assert_eq!(
        one.tool("acquire", json!({"owner":"one", "scope":"checkout"}))["acquired"],
        true
    );
    assert!(git("one", "git switch main").is_null());
    assert_eq!(
        two.tool("acquire", json!({"owner":"two", "paths":["a"]}))["acquired"],
        false
    );
}
