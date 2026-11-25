//! Atomic SQLite `Review -> Done` transition for `aaf/<task_id>` PR cleanup.

use std::fs;
use std::path::Path;
use std::time::Duration;

use aaf_domain::event::EventKind;
use aaf_domain::task::TaskState;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use super::pr_cleanup::main_work_tree;
use crate::db::SCHEMA_VERSION;

const DB_BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

pub(super) enum DbDecision {
    Proceed { marked_done: bool },
    Stop(String),
}

fn same_repo(task_repo_root: &str, main_repo: &Path) -> bool {
    let Ok(main_can) = fs::canonicalize(main_repo) else {
        return false;
    };
    let task_main = main_work_tree(Path::new(task_repo_root));
    fs::canonicalize(&task_main).is_ok_and(|task_can| task_can == main_can)
}

/// Atomically transitions a `Review` task matching `main_repo` to `Done` inside
/// a single `BEGIN IMMEDIATE` transaction, recording `TaskStateChanged`,
/// `MergeResult`, and `closed_fingerprints`. Never creates a new database file.
pub(super) fn transition_review_task_done(
    db_path: &Path,
    main_repo: &Path,
    task_id: &str,
    pr_number: u64,
    expect_head: &str,
    now: i64,
) -> Result<DbDecision, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(db_path, flags).map_err(|e| e.to_string())?;
    conn.busy_timeout(DB_BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if version <= 0 || version > SCHEMA_VERSION {
        return Err(format!("unsupported database schema version {version}"));
    }
    let row: Option<(String, String, Option<String>)> = tx
        .query_row(
            "SELECT state, repo_root, fingerprint FROM tasks WHERE id = ?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((state_str, task_repo_root, fingerprint)) = row else {
        return Ok(DbDecision::Proceed { marked_done: false });
    };
    if !same_repo(&task_repo_root, main_repo) {
        return Ok(DbDecision::Proceed { marked_done: false });
    }
    match TaskState::parse(&state_str) {
        Some(TaskState::Review) => {
            let sql = if version >= 5 {
                "UPDATE tasks SET state = 'done', blocked_reason = NULL, retry_after = NULL, \
                 auto_recover_count = 0, updated_at = ?1 WHERE id = ?2 AND state = 'review'"
            } else {
                "UPDATE tasks SET state = 'done', blocked_reason = NULL, updated_at = ?1 \
                 WHERE id = ?2 AND state = 'review'"
            };
            if tx
                .execute(sql, params![now, task_id])
                .map_err(|e| e.to_string())?
                == 0
            {
                return Ok(DbDecision::Stop(format!(
                    "task `{task_id}` state changed concurrently; skipped local cleanup"
                )));
            }
            let state_ev = json!({
                "from": TaskState::Review.as_str(),
                "to": TaskState::Done.as_str(),
                "why": format!("merged PR #{pr_number}"),
                "by": "aaf_merge",
                "pr_number": pr_number,
                "head_sha": expect_head,
            });
            let merge_ev = json!({
                "merged": true,
                "kind": "pr_merge",
                "pr_number": pr_number,
                "head_sha": expect_head,
            });
            for (kind, payload) in [
                (EventKind::TaskStateChanged, state_ev),
                (EventKind::MergeResult, merge_ev),
            ] {
                tx.execute(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, NULL, ?2, ?3, ?4)",
                    params![task_id, kind.as_str(), payload.to_string(), now],
                )
                .map_err(|e| e.to_string())?;
            }
            if let Some(fp) = fingerprint.filter(|s| !s.is_empty()) {
                tx.execute(
                    "INSERT INTO closed_fingerprints (fingerprint, repo_root, task_id, closed_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![fp, task_repo_root, task_id, now],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(DbDecision::Proceed { marked_done: true })
        }
        Some(TaskState::Done) => Ok(DbDecision::Proceed { marked_done: false }),
        Some(other) => Ok(DbDecision::Stop(format!(
            "task `{task_id}` is in state `{}`; skipped local cleanup",
            other.as_str()
        ))),
        None => Ok(DbDecision::Stop(format!(
            "task `{task_id}` has unknown state `{state_str}`; skipped local cleanup"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::git::pr_cleanup::is_protected_branch;
    use crate::git::pr_merge_support::TempRefGuard;

    #[test]
    fn protected_branches_are_case_insensitive_and_cover_base_and_prefixes() {
        for name in [
            "main",
            "MAIN",
            "Master",
            "develop",
            "DEVELOPMENT",
            "trunk",
            "HEAD",
            "release/1.2",
            "RELEASE/v2",
            "public-api",
            "PUBLIC-docs",
            "custom-base",
            "CUSTOM-BASE",
            "nhánh-tiếng-việt",
        ] {
            assert!(is_protected_branch(name, "custom-base"), "{name}");
        }
        for name in ["aaf/t1", "feat/demo", "fix/cleanup-c3"] {
            assert!(!is_protected_branch(name, "main"), "{name}");
        }
    }

    #[test]
    fn temp_ref_guard_includes_pid_and_unique_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = format!("refs/aaf-merge/pr-7-{}-", std::process::id());
        let (g1, g2) = (
            TempRefGuard::new(tmp.path(), 7),
            TempRefGuard::new(tmp.path(), 7),
        );
        assert!(g1.as_str().starts_with(&prefix) && g2.as_str().starts_with(&prefix));
        assert_ne!(g1.as_str(), g2.as_str());
    }
}
