use crate::{MAX_ITEMS, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_WAIT: u64 = 15;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Files,
    Git,
    Checkout,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Git => "git",
            Self::Checkout => "checkout",
        }
    }
}

#[derive(Serialize)]
struct Lock {
    lock_id: i64,
    scope: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    path: String,
    owner: String,
    acquired_at: i64,
    ended_at: Option<i64>,
    #[serde(skip)]
    key: String,
}

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(1500))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS locks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scope TEXT NOT NULL,
            key TEXT NOT NULL,
            path TEXT NOT NULL,
            owner TEXT NOT NULL,
            acquired_at INTEGER NOT NULL,
            ended_at INTEGER,
            UNIQUE (scope, key)
        )",
        )?;
        Ok(Self(connection))
    }

    pub fn acquire(
        &mut self,
        owner: &str,
        scope: Scope,
        mut paths: BTreeMap<String, String>,
        wait: u64,
    ) -> Result<Value> {
        validate_owner(owner)?;
        if wait > MAX_WAIT {
            return Err(format!("wait_seconds must be at most {MAX_WAIT}").into());
        }
        if scope != Scope::Files {
            paths.insert(String::new(), String::new());
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let result = self.try_acquire(owner, scope, &paths)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if result["acquired"] == true || remaining.is_zero() {
                return Ok(result);
            }
            thread::sleep(remaining.min(Duration::from_millis(200)));
        }
    }

    fn try_acquire(
        &mut self,
        owner: &str,
        scope: Scope,
        paths: &BTreeMap<String, String>,
    ) -> Result<Value> {
        // BEGIN IMMEDIATE serializes the conflict check with the entire group insert.
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locks = all_locks(&transaction)?;
        let conflicts: Vec<_> = locks
            .iter()
            .filter(|lock| {
                lock.owner != owner
                    && (scope == Scope::Checkout
                        || lock.scope == Scope::Checkout.name()
                        || scope == Scope::Git && lock.scope == Scope::Git.name()
                        || scope == Scope::Files
                            && lock.scope == Scope::Files.name()
                            && paths.keys().any(|key| paths_overlap(key, &lock.key)))
            })
            .collect();
        if !conflicts.is_empty() {
            return Ok(json!({"acquired": false, "conflicts": conflicts}));
        }

        let now = timestamp()?;
        let mut acquired = Vec::with_capacity(paths.len());
        for (key, path) in paths {
            if let Some(lock) = locks
                .iter()
                .find(|lock| lock.scope == scope.name() && lock.key == *key)
            {
                if lock.ended_at.is_none() {
                    acquired.push(serde_json::to_value(lock)?);
                    continue;
                }
                // Mint a new ID on resume so a stale recovery command cannot release the revived lock.
                transaction.execute("DELETE FROM locks WHERE id = ?", [lock.lock_id])?;
            }
            transaction.execute(
                "INSERT INTO locks (scope, key, path, owner, acquired_at) VALUES (?, ?, ?, ?, ?)",
                params![scope.name(), key, path, owner, now],
            )?;
            acquired.push(serde_json::to_value(Lock {
                lock_id: transaction.last_insert_rowid(),
                scope: scope.name().into(),
                key: key.clone(),
                path: path.clone(),
                owner: owner.into(),
                acquired_at: now,
                ended_at: None,
            })?);
        }
        transaction.commit()?;
        Ok(json!({"acquired": true, "locks": acquired}))
    }

    pub fn status(&self, root: &Path) -> Result<Value> {
        Ok(json!({"checkout": root, "locks": all_locks(&self.0)?}))
    }

    pub fn missing(
        &self,
        owner: &str,
        scope: Scope,
        files: &BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        let locks = all_locks(&self.0)?;
        let held = |role: Scope, key: &str| {
            locks.iter().any(|lock| {
                lock.owner == owner
                    && lock.scope == role.name()
                    && lock.key == key
                    && lock.ended_at.is_none()
            })
        };
        if scope != Scope::Files {
            let owns_checkout = held(Scope::Checkout, "");
            return Ok(if owns_checkout || held(scope, "") {
                vec![]
            } else {
                vec![scope.name().into()]
            });
        }
        Ok(files
            .iter()
            .filter(|(key, _)| !held(Scope::Files, key))
            .map(|(_, name)| name.clone())
            .collect())
    }

    pub fn release(&mut self, owner: &str, ids: &[i64]) -> Result<Value> {
        validate_owner(owner)?;
        if ids.is_empty() || ids.len() > MAX_ITEMS || ids.iter().any(|id| *id <= 0) {
            return Err(format!("Specify 1 to {MAX_ITEMS} positive lock IDs").into());
        }
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut already_absent: BTreeSet<_> = ids.iter().copied().collect();
        let mut released = Vec::new();
        for lock in all_locks(&transaction)? {
            if already_absent.remove(&lock.lock_id) {
                if lock.owner != owner {
                    return Err(
                        "A requested lock belongs to another session. Released nothing.".into(),
                    );
                }
                transaction.execute("DELETE FROM locks WHERE id = ?", [lock.lock_id])?;
                released.push(lock.lock_id);
            }
        }
        transaction.commit()?;
        Ok(json!({"released": released, "already_absent": already_absent}))
    }

    pub fn mark_ended(&self, owner: &str) -> Result<()> {
        self.0.execute(
            "UPDATE locks SET ended_at = ? WHERE owner = ? AND ended_at IS NULL",
            params![timestamp()?, validate_owner(owner)?],
        )?;
        Ok(())
    }
}

fn all_locks(connection: &Connection) -> Result<Vec<Lock>> {
    let mut statement = connection.prepare(
        "SELECT id, scope, key, path, owner, acquired_at, ended_at FROM locks ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(Lock {
            lock_id: row.get(0)?,
            scope: row.get(1)?,
            key: row.get(2)?,
            path: row.get(3)?,
            owner: row.get(4)?,
            acquired_at: row.get(5)?,
            ended_at: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn paths_overlap(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

fn timestamp() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}

pub fn validate_owner(owner: &str) -> Result<&str> {
    if owner.trim().is_empty() || owner.len() > 128 || owner.chars().any(char::is_control) {
        return Err(
            "A session ID is required. In Codex, review and enable the Agent Coordinator hooks."
                .into(),
        );
    }
    Ok(owner)
}
