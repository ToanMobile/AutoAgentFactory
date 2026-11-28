//! `aaf board adopt`: the one transaction that makes a pull request a task.

use aaf_domain::event::FactoryEvent;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Task, TaskState};
use rusqlite::params;

use super::tasks::{encode_task, insert_task_row};
use super::{Db, sql_err};

/// How [`Db::insert_adopting`] ended when the store itself worked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptInsert {
    Inserted,
    /// Another `Adopting` task already holds this PR; nothing was written.
    AlreadyAdopting {
        task_id: String,
    },
}

impl Db {
    /// Inserts the `Adopting` task and its events in one `BEGIN IMMEDIATE`
    /// transaction, after checking that no other `Adopting` task of the same
    /// repository carries the same fingerprint (the PR). Any failure rolls the
    /// whole thing back: there is never a task without its `pr_adopted` event.
    ///
    /// Done and Cancelled tasks never count as a duplicate: a PR can be
    /// adopted again. An `Err` is the store failing (a constraint included),
    /// never a duplicate.
    pub fn insert_adopting(&self, task: &Task, events: &[FactoryEvent]) -> PortResult<AdoptInsert> {
        if task.state != TaskState::Adopting {
            return Err(PortError::Invalid(format!(
                "task {} is {}, not adopting",
                task.id,
                task.state.as_str()
            )));
        }
        let Some(fingerprint) = task.fingerprint.clone() else {
            return Err(PortError::Invalid(format!(
                "task {} has no fingerprint to deduplicate the adoption on",
                task.id
            )));
        };
        let enc = encode_task(task)?;
        let events = events.to_vec();
        self.write(move |tx| {
            let existing: Option<String> = tx
                .prepare_cached(
                    "SELECT id FROM tasks WHERE repo_root = ?1 AND fingerprint = ?2 \
                     AND state = 'adopting' LIMIT 1",
                )
                .and_then(|mut stmt| {
                    use rusqlite::OptionalExtension;
                    stmt.query_row(params![enc.task.repo_root, fingerprint], |row| row.get(0))
                        .optional()
                })
                .map_err(sql_err)?;
            if let Some(task_id) = existing {
                return Ok(AdoptInsert::AlreadyAdopting { task_id });
            }
            insert_task_row(tx, &enc, enc.task.state.as_str())?;
            for event in &events {
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        event.task_id,
                        event.run_id,
                        event.kind.as_str(),
                        event.payload_json,
                        event.created_at,
                    ])
                })
                .map_err(sql_err)?;
            }
            Ok(AdoptInsert::Inserted)
        })
    }
}
