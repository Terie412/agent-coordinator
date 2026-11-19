use crate::checkout::Checkout;
use crate::{MAX_ITEMS, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;
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

impl FromStr for Scope {
    type Err = &'static str;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "files" => Ok(Self::Files),
            "git" => Ok(Self::Git),
            "checkout" => Ok(Self::Checkout),
            _ => Err("Scope must be files, git, or checkout."),
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
        );
        CREATE TABLE IF NOT EXISTS wait_requests (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scope TEXT NOT NULL,
            paths TEXT NOT NULL,
            owner TEXT NOT NULL,
            requested_at INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL
        )",
        )?;
        Ok(Self(connection))
    }

    pub fn new_session(&self) -> Result<Value> {
        let owner: String = self.0.query_row(
            "SELECT 'session-' || lower(hex(randomblob(16)))",
            [],
            |row| row.get(0),
        )?;
        Ok(json!({"owner": owner}))
    }

    pub fn acquire(
        &mut self,
        checkout: &Checkout,
        owner: &str,
        scope: Scope,
        paths: Option<&[String]>,
        wait: u64,
    ) -> Result<Value> {
        validate_owner(owner)?;
        if wait > MAX_WAIT {
            return Err(format!("wait_seconds must be at most {MAX_WAIT}").into());
        }
        let paths = match (scope, paths) {
            (Scope::Files, Some(paths)) => checkout.files(paths, &checkout.root)?,
            (Scope::Files, None) => return Err("paths is required for scope files".into()),
            (_, Some(_)) => return Err("Only scope files accepts paths".into()),
            (_, None) => BTreeMap::from([(String::new(), String::new())]),
        };
        let deadline = Instant::now() + Duration::from_secs(wait);
        let mut request_id = None;
        let result = loop {
            let result = match self.try_acquire(owner, scope, &paths, request_id) {
                Ok(result) => result,
                Err(error) => break Err(error),
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if result["acquired"] == true {
                return Ok(result);
            }
            if remaining.is_zero() {
                break Ok(result);
            }
            if request_id.is_none() {
                request_id = Some(self.register_wait(owner, scope, &paths, remaining)?);
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(200)),
            );
        };
        if let Some(id) = request_id {
            // The request expires even if cleanup fails or the process is terminated.
            let _ = self
                .0
                .execute("DELETE FROM wait_requests WHERE id = ?", [id]);
        }
        result
    }

    fn register_wait(
        &self,
        owner: &str,
        scope: Scope,
        paths: &BTreeMap<String, String>,
        remaining: Duration,
    ) -> Result<i64> {
        let now = timestamp_ms()?;
        self.0
            .execute("DELETE FROM wait_requests WHERE expires_at_ms <= ?", [now])?;
        let paths = serde_json::to_string(paths)?;
        let expires_at = now + i64::try_from(remaining.as_millis())?;
        Ok(self.0.query_row(
            "INSERT INTO wait_requests (scope, paths, owner, requested_at, expires_at_ms)
             VALUES (?, ?, ?, ?, ?) RETURNING id",
            params![scope.name(), paths, owner, now / 1000, expires_at],
            |row| row.get(0),
        )?)
    }

    fn try_acquire(
        &mut self,
        owner: &str,
        scope: Scope,
        paths: &BTreeMap<String, String>,
        request_id: Option<i64>,
    ) -> Result<Value> {
        // BEGIN IMMEDIATE serializes the conflict check with the entire group insert.
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locks = all_locks(&transaction)?;
        let conflicts: Vec<_> = locks
            .iter()
            .filter(|lock| lock.blocks(owner, scope, paths))
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
        if let Some(id) = request_id {
            transaction.execute("DELETE FROM wait_requests WHERE id = ?", [id])?;
        }
        transaction.commit()?;
        Ok(json!({"acquired": true, "locks": acquired}))
    }

    pub fn inspect(checkout: &Checkout) -> Result<Value> {
        if !checkout.database.try_exists()? {
            return Ok(json!({"checkout": checkout.root, "locks": [], "requests": []}));
        }
        let connection =
            Connection::open_with_flags(&checkout.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_millis(1500))?;
        Self(connection).status(&checkout.root)
    }

    fn status(&self, root: &Path) -> Result<Value> {
        let snapshot = self.0.unchecked_transaction()?;
        let exists = |name| {
            snapshot.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?)",
                [name],
                |row| row.get::<_, bool>(0),
            )
        };
        if !exists("locks")? {
            return Ok(json!({"checkout": root, "locks": [], "requests": []}));
        }
        let locks = all_locks(&snapshot)?;
        if !exists("wait_requests")? {
            return Ok(json!({"checkout": root, "locks": locks, "requests": []}));
        }
        let mut statement = snapshot.prepare(
            "SELECT id, scope, paths, owner, requested_at FROM wait_requests
             WHERE expires_at_ms > ? ORDER BY id",
        )?;
        let mut rows = statement.query([timestamp_ms()?])?;
        let mut requests = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let scope: Scope = row.get::<_, String>(1)?.parse()?;
            let paths: BTreeMap<String, String> = serde_json::from_str(&row.get::<_, String>(2)?)?;
            let owner: String = row.get(3)?;
            let requested_at: i64 = row.get(4)?;
            let blocking_locks: Vec<_> = locks
                .iter()
                .filter(|lock| lock.blocks(&owner, scope, &paths))
                .map(|lock| lock.lock_id)
                .collect();
            requests.push(json!({
                "request_id": id,
                "scope": scope,
                "paths": paths.values().filter(|path| !path.is_empty()).collect::<Vec<_>>(),
                "owner": owner,
                "requested_at": requested_at,
                "blocking_locks": blocking_locks,
            }));
        }
        Ok(json!({"checkout": root, "locks": locks, "requests": requests}))
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

impl Lock {
    fn blocks(&self, owner: &str, scope: Scope, paths: &BTreeMap<String, String>) -> bool {
        self.owner != owner
            && (scope == Scope::Checkout
                || self.scope == Scope::Checkout.name()
                || scope == Scope::Git && self.scope == Scope::Git.name()
                || scope == Scope::Files
                    && self.scope == Scope::Files.name()
                    && paths.keys().any(|key| paths_overlap(key, &self.key)))
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
    Ok(timestamp_ms()? / 1000)
}

fn timestamp_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub fn validate_owner(owner: &str) -> Result<&str> {
    if owner.trim().is_empty() || owner.len() > 128 || owner.chars().any(char::is_control) {
        return Err(
            "Supply this chat's owner from new_session, or enable the Agent Coordinator hooks in Codex."
                .into(),
        );
    }
    Ok(owner)
}
