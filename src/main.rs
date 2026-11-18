mod checkout;
mod codex;
mod mcp;
mod store;

use checkout::Checkout;
use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use store::{Scope, Store};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_ITEMS: usize = 128;

const USAGE: &str = "agent-coordinator <serve|hook|configure|new-session|status> <checkout>
agent-coordinator config <checkout> [codex|mcp|opencode|opencode-v1|deepseek]
agent-coordinator acquire <checkout> <owner> [--wait <seconds>] [--scope files|git|checkout] [--] [<file> ...]
agent-coordinator <release|recover> <checkout> <owner> <lock-id> [<lock-id> ...]

acquire returns exit code 2 when another session blocks the request.";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agent-coordinator: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("--help");
    if matches!(command, "-h" | "--help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if command == "--version" {
        println!("agent-coordinator {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let [_, directory, arguments @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let checkout = Checkout::find(Path::new(directory))?;
    match (command, arguments) {
        ("serve", []) => mcp::serve(&checkout),
        ("hook", []) => codex::hook(&checkout),
        ("config", [] | [_]) => {
            let client = arguments.first().map(utf8).transpose()?.unwrap_or("codex");
            if client == "codex" {
                print!("{}", codex::config(&checkout)?);
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&mcp::config(&checkout, client)?)?
                );
            }
            Ok(())
        }
        ("configure", []) => codex::configure(&checkout),
        ("new-session", []) => {
            println!("{}", Store::open(&checkout.database)?.new_session()?);
            Ok(())
        }
        ("status", []) => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &Store::open(&checkout.database)?.status(&checkout.root)?
                )?
            );
            Ok(())
        }
        ("acquire", [owner, options @ ..]) => {
            return acquire(&checkout, utf8(owner)?, options);
        }
        ("release" | "recover", [owner, ids @ ..]) if !ids.is_empty() => {
            let ids = ids
                .iter()
                .map(|arg| Ok(utf8(arg)?.parse::<i64>()?))
                .collect::<Result<Vec<_>>>()?;
            println!(
                "{}",
                Store::open(&checkout.database)?.release(utf8(owner)?, &ids)?
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn acquire(checkout: &Checkout, owner: &str, mut arguments: &[OsString]) -> Result<ExitCode> {
    let mut scope = Scope::Files;
    let mut wait = 0;
    while let [flag, rest @ ..] = arguments {
        match utf8(flag)? {
            "--" => {
                arguments = rest;
                break;
            }
            "--wait" | "--scope" => {
                let [value, tail @ ..] = rest else {
                    return Err(format!("Missing value for {}", flag.to_string_lossy()).into());
                };
                if flag == "--wait" {
                    wait = utf8(value)?.parse()?;
                } else {
                    scope = utf8(value)?.parse()?;
                }
                arguments = tail;
            }
            option if option.starts_with('-') => {
                return Err(format!("Unknown option: {option}").into());
            }
            _ => break,
        }
    }
    let files = arguments
        .iter()
        .map(|arg| Ok(utf8(arg)?.to_owned()))
        .collect::<Result<Vec<_>>>()?;
    let paths = (!files.is_empty()).then_some(files.as_slice());
    let result = Store::open(&checkout.database)?.acquire(checkout, owner, scope, paths, wait)?;
    println!("{result}");
    Ok(if result["acquired"] == true {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn utf8(arg: &OsString) -> Result<&str> {
    arg.to_str()
        .ok_or_else(|| "Expected a UTF-8 argument".into())
}
