//! Budgets, runs, accounts, events, fingerprints and the `meta` key/value
//! table on SQLite.

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind, default_account_name};
use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    AccountStore, BudgetStore, ClosedFingerprint, EventSink, FingerprintStore, KvStore, PortError,
    PortResult, RunStore,
};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::TaskState;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::queries::run_from_row;
use super::{Db, corrupt, sql_err, to_i64, to_u64};

const I64_MAX: u64 = i64::MAX as u64;

fn budget_not_found(root_id: &str) -> PortError {
    PortError::NotFound(format!("budget {root_id}"))
}

fn load_budget(conn: &Connection, root_id: &str) -> PortResult<RootBudget> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT tokens_used, token_limit, wall_secs_used, wall_secs_limit, cycles_used, \
             cycles_limit, frozen FROM root_budgets WHERE root_id = ?1",
        )
        .map_err(sql_err)?;
    let mut rows = stmt.query(params![root_id]).map_err(sql_err)?;
    let row = rows
        .next()
        .map_err(sql_err)?
        .ok_or_else(|| budget_not_found(root_id))?;
    let int = |idx: usize, what: &str| -> PortResult<u64> {
        to_u64(row.get(idx).map_err(sql_err)?, what)
    };
    Ok(RootBudget {
        root_id: root_id.to_string(),
        tokens_used: int(0, "root_budgets.tokens_used")?,
        token_limit: int(1, "root_budgets.token_limit")?,
        wall_secs_used: int(2, "root_budgets.wall_secs_used")?,
        wall_secs_limit: int(3, "root_budgets.wall_secs_limit")?,
        cycles_used: row.get(4).map_err(sql_err)?,
        cycles_limit: row.get(5).map_err(sql_err)?,
        frozen: row.get(6).map_err(sql_err)?,
    })
}

fn store_budget(conn: &Connection, budget: &RootBudget) -> PortResult<()> {
    let tokens_used = to_i64(budget.tokens_used, "tokens_used")?;
    let token_limit = to_i64(budget.token_limit, "token_limit")?;
    let wall_secs_used = to_i64(budget.wall_secs_used, "wall_secs_used")?;
    let wall_secs_limit = to_i64(budget.wall_secs_limit, "wall_secs_limit")?;
    conn.prepare_cached(
        "INSERT INTO root_budgets (root_id, tokens_used, token_limit, wall_secs_used, \
         wall_secs_limit, cycles_used, cycles_limit, frozen) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(root_id) DO UPDATE SET tokens_used = excluded.tokens_used, \
         token_limit = excluded.token_limit, wall_secs_used = excluded.wall_secs_used, \
         wall_secs_limit = excluded.wall_secs_limit, cycles_used = excluded.cycles_used, \
         cycles_limit = excluded.cycles_limit, frozen = excluded.frozen",
    )
    .and_then(|mut stmt| {
        stmt.execute(params![
            budget.root_id,
            tokens_used,
            token_limit,
            wall_secs_used,
            wall_secs_limit,
            budget.cycles_used,
            budget.cycles_limit,
            budget.frozen,
        ])
    })
    .map_err(sql_err)?;
    Ok(())
}

impl BudgetStore for Db {
    fn put_budget(&self, budget: &RootBudget) -> PortResult<()> {
        let budget = budget.clone();
        self.write(move |tx| store_budget(tx, &budget))
    }

    fn get_budget(&self, root_id: &str) -> PortResult<RootBudget> {
        let root_id = root_id.to_string();
        self.read(move |conn| load_budget(conn, &root_id))
    }

    fn add_usage(
        &self,
        root_id: &str,
        tokens: u64,
        wall_secs: u64,
        cycles: u32,
    ) -> PortResult<RootBudget> {
        let root_id = root_id.to_string();
        // Read-modify-write inside one immediate transaction: concurrent
        // callers (threads or processes) cannot lose each other's usage.
        self.write(move |tx| {
            let mut budget = load_budget(tx, &root_id)?;
            budget.add_usage(tokens, wall_secs, cycles);
            // Usage saturates at what SQLite can hold; that is far past any
            // limit, so the budget still reads as exceeded.
            budget.tokens_used = budget.tokens_used.min(I64_MAX);
            budget.wall_secs_used = budget.wall_secs_used.min(I64_MAX);
            store_budget(tx, &budget)?;
            Ok(budget)
        })
    }

    fn set_frozen(&self, root_id: &str, frozen: bool) -> PortResult<()> {
        let root_id = root_id.to_string();
        self.write(move |tx| {
            let changed = tx
                .prepare_cached("UPDATE root_budgets SET frozen = ?1 WHERE root_id = ?2")
                .and_then(|mut stmt| stmt.execute(params![frozen, root_id]))
                .map_err(sql_err)?;
            if changed == 0 {
                return Err(budget_not_found(&root_id));
            }
            Ok(())
        })
    }
}

impl RunStore for Db {
    fn insert_run(&self, run: &RunRecord) -> PortResult<()> {
        let run = run.clone();
        let tokens_in = to_i64(run.tokens_in, "tokens_in")?;
        let tokens_out = to_i64(run.tokens_out, "tokens_out")?;
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO runs (id, task_id, account_id, executor, worktree_path, branch, \
                 status, exit_code, tokens_in, tokens_out, started_at, ended_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    run.id,
                    run.task_id,
                    run.account_id,
                    run.executor,
                    run.worktree_path,
                    run.branch,
                    run.status.as_str(),
                    run.exit_code,
                    tokens_in,
                    tokens_out,
                    run.started_at,
                    run.ended_at,
                ])
            })
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn finish_run(&self, id: &str, finish: &RunFinish) -> PortResult<()> {
        let id = id.to_string();
        let finish = finish.clone();
        let tokens_in = to_i64(finish.tokens_in, "tokens_in")?;
        let tokens_out = to_i64(finish.tokens_out, "tokens_out")?;
        self.write(move |tx| {
            let changed = tx
                .prepare_cached(
                    "UPDATE runs SET status = ?1, exit_code = ?2, tokens_in = ?3, \
                     tokens_out = ?4, ended_at = ?5 WHERE id = ?6",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        finish.status.as_str(),
                        finish.exit_code,
                        tokens_in,
                        tokens_out,
                        finish.ended_at,
                        id,
                    ])
                })
                .map_err(sql_err)?;
            if changed == 0 {
                return Err(PortError::NotFound(format!("run {id}")));
            }
            Ok(())
        })
    }

    fn runs_for_task(&self, task_id: &str) -> PortResult<Vec<RunRecord>> {
        let task_id = task_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, task_id, account_id, executor, worktree_path, branch, status, \
                     exit_code, tokens_in, tokens_out, started_at, ended_at \
                     FROM runs WHERE task_id = ?1 ORDER BY started_at, rowid",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![task_id]).map_err(sql_err)?;
            let mut runs = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                runs.push(run_from_row(row)?);
            }
            Ok(runs)
        })
    }

    fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64> {
        let account_id = account_id.to_string();
        self.read(move |conn| {
            let total: i64 = conn
                .prepare_cached(
                    "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM runs \
                     WHERE account_id = ?1 AND started_at >= ?2",
                )
                .and_then(|mut stmt| stmt.query_row(params![account_id, since], |row| row.get(0)))
                .map_err(sql_err)?;
            to_u64(total, "sum of runs tokens")
        })
    }
}

impl AccountStore for Db {
    fn upsert_account(&self, profile: &AccountProfile) -> PortResult<()> {
        let profile = profile.clone();
        self.write(move |tx| {
            // `cooldown_until` is absent from the update list on purpose: a
            // rediscovered account keeps its cooldown.
            tx.prepare_cached(
                "INSERT INTO accounts (id, provider, config_dir, name) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(id) DO UPDATE SET provider = excluded.provider, \
                 config_dir = excluded.config_dir, name = excluded.name",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    profile.id,
                    profile.provider.as_str(),
                    profile.config_dir,
                    profile.name
                ])
            })
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn list_accounts(&self) -> PortResult<Vec<AccountState>> {
        let has_name = self.schema_version() >= 3;
        self.read(move |conn| {
            let sql = if has_name {
                "SELECT id, provider, config_dir, cooldown_until, name FROM accounts ORDER BY id"
            } else {
                "SELECT id, provider, config_dir, cooldown_until FROM accounts ORDER BY id"
            };
            let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
            let mut rows = stmt.query([]).map_err(sql_err)?;
            let mut accounts = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let provider: String = row.get(1).map_err(sql_err)?;
                let config_dir: String = row.get(2).map_err(sql_err)?;
                // Rows from before schema v3 have no name until rediscovery.
                let name = if has_name {
                    match row.get::<_, String>(4).map_err(sql_err)? {
                        name if name.is_empty() => default_account_name(&config_dir),
                        name => name,
                    }
                } else {
                    default_account_name(&config_dir)
                };
                accounts.push(AccountState {
                    profile: AccountProfile {
                        id: row.get(0).map_err(sql_err)?,
                        provider: ProviderKind::parse(&provider)
                            .ok_or_else(|| corrupt("accounts.provider", &provider))?,
                        config_dir,
                        name,
                    },
                    cooldown_until: row.get(3).map_err(sql_err)?,
                });
            }
            Ok(accounts)
        })
    }

    fn set_cooldown(&self, account_id: &str, until: Option<i64>) -> PortResult<()> {
        let account_id = account_id.to_string();
        self.write(move |tx| {
            let changed = tx
                .prepare_cached("UPDATE accounts SET cooldown_until = ?1 WHERE id = ?2")
                .and_then(|mut stmt| stmt.execute(params![until, account_id]))
                .map_err(sql_err)?;
            if changed == 0 {
                return Err(PortError::NotFound(format!("account {account_id}")));
            }
            Ok(())
        })
    }
}

// `events` is append-only: this impl has no update or delete statement, and
// the schema's triggers reject both for any other writer.
impl EventSink for Db {
    fn emit(&self, event: &FactoryEvent) -> PortResult<i64> {
        let event = event.clone();
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .and_then(|mut stmt| {
                stmt.insert(params![
                    event.task_id,
                    event.run_id,
                    event.kind.as_str(),
                    event.payload_json,
                    event.created_at,
                ])
            })
            .map_err(sql_err)
        })
    }

    fn events_for_task(&self, task_id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>> {
        let task_id = task_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, task_id, run_id, kind, payload_json, created_at FROM events \
                     WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![task_id, limit]).map_err(sql_err)?;
            let mut events = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let kind: String = row.get(3).map_err(sql_err)?;
                events.push(FactoryEvent {
                    id: Some(row.get(0).map_err(sql_err)?),
                    task_id: row.get(1).map_err(sql_err)?,
                    run_id: row.get(2).map_err(sql_err)?,
                    kind: EventKind::parse(&kind).ok_or_else(|| corrupt("events.kind", &kind))?,
                    payload_json: row.get(4).map_err(sql_err)?,
                    created_at: row.get(5).map_err(sql_err)?,
                });
            }
            Ok(events)
        })
    }
}

impl FingerprintStore for Db {
    fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()> {
        let closed = closed.clone();
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO closed_fingerprints (fingerprint, repo_root, task_id, closed_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    closed.fingerprint,
                    closed.repo_root,
                    closed.task_id,
                    closed.closed_at,
                ])
            })
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn find_closed_since(
        &self,
        fingerprint: &str,
        repo_root: &str,
        since: i64,
    ) -> PortResult<Option<ClosedFingerprint>> {
        let fingerprint = fingerprint.to_string();
        let repo_root = repo_root.to_string();
        self.read(move |conn| {
            conn.prepare_cached(
                "SELECT fingerprint, repo_root, task_id, closed_at FROM closed_fingerprints \
                 WHERE fingerprint = ?1 AND repo_root = ?2 AND closed_at >= ?3 \
                 ORDER BY closed_at DESC, id DESC LIMIT 1",
            )
            .and_then(|mut stmt| {
                stmt.query_row(params![fingerprint, repo_root, since], |row| {
                    Ok(ClosedFingerprint {
                        fingerprint: row.get(0)?,
                        repo_root: row.get(1)?,
                        task_id: row.get(2)?,
                        closed_at: row.get(3)?,
                    })
                })
                .optional()
            })
            .map_err(sql_err)
        })
    }

    fn find_open_task(&self, fingerprint: &str, repo_root: &str) -> PortResult<Option<String>> {
        let fingerprint = fingerprint.to_string();
        let repo_root = repo_root.to_string();
        self.read(move |conn| {
            conn.prepare_cached(
                "SELECT id FROM tasks WHERE fingerprint = ?1 AND repo_root = ?2 \
                 AND state NOT IN (?3, ?4) ORDER BY created_at, rowid LIMIT 1",
            )
            .and_then(|mut stmt| {
                stmt.query_row(
                    params![
                        fingerprint,
                        repo_root,
                        TaskState::Done.as_str(),
                        TaskState::Cancelled.as_str(),
                    ],
                    |row| row.get(0),
                )
                .optional()
            })
            .map_err(sql_err)
        })
    }
}

impl KvStore for Db {
    fn get_meta(&self, key: &str) -> PortResult<Option<String>> {
        let key = key.to_string();
        self.read(move |conn| {
            conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")
                .and_then(|mut stmt| stmt.query_row(params![key], |row| row.get(0)).optional())
                .map_err(sql_err)
        })
    }

    fn set_meta(&self, key: &str, value: &str) -> PortResult<()> {
        let key = key.to_string();
        let value = value.to_string();
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .and_then(|mut stmt| stmt.execute(params![key, value]))
            .map_err(sql_err)?;
            Ok(())
        })
    }
}

impl Db {
    /// Returns the IDs of all runs currently in `running` state.
    pub fn running_run_ids(&self) -> PortResult<std::collections::HashSet<String>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached("SELECT id FROM runs WHERE status = 'running'")
                .map_err(sql_err)?;
            let mut rows = stmt.query([]).map_err(sql_err)?;
            let mut ids = std::collections::HashSet::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                ids.insert(row.get(0).map_err(sql_err)?);
            }
            Ok(ids)
        })
    }

    /// Aggregates `(status, run_count, tokens_in, tokens_out)` grouped by `runs.status`,
    /// optionally scoped to tasks under `repo_root`.
    pub fn run_totals_by_repo(
        &self,
        repo_root: Option<&str>,
    ) -> PortResult<Vec<(RunStatus, usize, u64, u64)>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let map_row = |row: &Row<'_>| -> PortResult<(RunStatus, usize, u64, u64)> {
                let status_str: String = row.get(0).map_err(sql_err)?;
                let status = RunStatus::parse(&status_str)
                    .ok_or_else(|| corrupt("runs.status", &status_str))?;
                let count_i64: i64 = row.get(1).map_err(sql_err)?;
                let count = usize::try_from(count_i64)
                    .map_err(|_| corrupt("runs.count", &count_i64.to_string()))?;
                let tokens_in = to_u64(row.get(2).map_err(sql_err)?, "runs.tokens_in")?;
                let tokens_out = to_u64(row.get(3).map_err(sql_err)?, "runs.tokens_out")?;
                Ok((status, count, tokens_in, tokens_out))
            };
            let mut totals = Vec::new();
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT r.status, COUNT(*), COALESCE(SUM(r.tokens_in), 0), \
                         COALESCE(SUM(r.tokens_out), 0) \
                         FROM runs r INNER JOIN tasks t ON t.id = r.task_id \
                         WHERE t.repo_root = ?1 GROUP BY r.status",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    totals.push(map_row(row)?);
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT status, COUNT(*), COALESCE(SUM(tokens_in), 0), \
                         COALESCE(SUM(tokens_out), 0) FROM runs GROUP BY status",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query([]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    totals.push(map_row(row)?);
                }
            }
            Ok(totals)
        })
    }

    /// Returns up to `limit` most recent runs (`ORDER BY started_at DESC, rowid DESC`),
    /// optionally scoped to tasks under `repo_root`, in a single SQL query.
    pub fn recent_runs(&self, repo_root: Option<&str>, limit: usize) -> PortResult<Vec<RunRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let repo_root = repo_root.map(str::to_string);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(move |conn| {
            let mut runs = Vec::new();
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT r.id, r.task_id, r.account_id, r.executor, r.worktree_path, \
                         r.branch, r.status, r.exit_code, r.tokens_in, r.tokens_out, \
                         r.started_at, r.ended_at FROM runs r \
                         INNER JOIN tasks t ON t.id = r.task_id WHERE t.repo_root = ?1 \
                         ORDER BY r.started_at DESC, r.rowid DESC LIMIT ?2",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root, limit_i64]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    runs.push(run_from_row(row)?);
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT id, task_id, account_id, executor, worktree_path, branch, status, \
                         exit_code, tokens_in, tokens_out, started_at, ended_at FROM runs \
                         ORDER BY started_at DESC, rowid DESC LIMIT ?1",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![limit_i64]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    runs.push(run_from_row(row)?);
                }
            }
            Ok(runs)
        })
    }
}
