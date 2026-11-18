mod checkout;
mod codex;
mod mcp;
mod store;

use checkout::Checkout;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::ExitCode;
use store::Store;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_ITEMS: usize = 128;

const USAGE: &str = "agent-coordinator <serve|hook|config|configure|status> <checkout>\nagent-coordinator recover <checkout> <owner> <lock-id> [<lock-id> ...]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent-coordinator: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("--help");
    if matches!(command, "-h" | "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if command == "--version" {
        println!("agent-coordinator {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() < 2 || (command != "recover" && args.len() != 2) {
        return Err(USAGE.into());
    }
    let checkout = Checkout::find(Path::new(&args[1]))?;
    match command {
        "serve" => mcp::serve(&checkout),
        "hook" => codex::hook(&checkout),
        "config" => {
            print!("{}", codex::config(&checkout)?);
            Ok(())
        }
        "configure" => configure(&checkout),
        "status" => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &Store::open(&checkout.database)?.status(&checkout.root)?
                )?
            );
            Ok(())
        }
        "recover" if args.len() >= 4 => {
            let owner = args[2].to_str().ok_or("Owner must be UTF-8")?;
            let ids = args[3..]
                .iter()
                .map(|arg| arg.to_str().unwrap_or_default().parse::<i64>())
                .collect::<std::result::Result<Vec<_>, _>>()?;
            println!("{}", Store::open(&checkout.database)?.release(owner, &ids)?);
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn configure(checkout: &Checkout) -> Result<()> {
    let directory = checkout.root.join(".codex");
    fs::create_dir_all(&directory)?;
    if dunce::canonicalize(&directory)? != directory {
        return Err(
            "The .codex directory resolves through an alias; use config for manual setup.".into(),
        );
    }
    let path = directory.join("config.toml");
    let config = codex::config(checkout)?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => file.write_all(config.as_bytes())?,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if fs::read_to_string(&path)? != config {
                return Err(format!(
                    "{} already exists. Use config to print the configuration for merging.",
                    path.display()
                )
                .into());
            }
        }
        Err(error) => return Err(error.into()),
    }
    println!("{}", path.display());
    Ok(())
}
