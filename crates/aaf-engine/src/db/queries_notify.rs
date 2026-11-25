//! Notification-specific read-only SQLite queries for `aaf notify`.

use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use rusqlite::params;

use super::{Db, sql_err};

impl Db {
    /// Returns `true` when there is a `WebhookSent` event for `task_id` with
    /// `id > after_id` whose JSON payload has `"ok": true`.
    pub fn webhook_sent_ok_after(&self, task_id: &str, after_id: i64) -> PortResult<bool> {
        let task_id = task_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT payload_json FROM events \
                     WHERE task_id = ?1 AND kind = ?2 AND id > ?3 \
                     ORDER BY id ASC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt
                .query(params![task_id, EventKind::WebhookSent.as_str(), after_id])
                .map_err(sql_err)?;
            while let Some(row) = rows.next().map_err(sql_err)? {
                let payload_json: String = row.get(0).map_err(sql_err)?;
                if is_webhook_sent_ok(&payload_json) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }
}

fn is_webhook_sent_ok(payload_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .is_some_and(|v| v["ok"].as_bool() == Some(true))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aaf_domain::event::FactoryEvent;
    use aaf_domain::ports::{EventSink, TaskStore};
    use aaf_domain::task::{ImmutableSpec, Priority, Task};

    fn make_task(id: &str, repo: &str) -> Task {
        Task::new_root(
            id,
            repo,
            format!("title {id}"),
            ImmutableSpec {
                description: "desc".into(),
                acceptance: vec!["ok".into()],
            },
            Priority::P2,
            100,
        )
    }

    #[test]
    fn webhook_sent_ok_after_checks_task_id_ordering_and_ok_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&make_task("t1", "/repo-a")).unwrap();
        db.insert_task(&make_task("t2", "/repo-a")).unwrap();

        // WebhookSent BEFORE the blocked event does not count.
        db.emit(&FactoryEvent::new(EventKind::WebhookSent, r#"{"ok":true}"#, 10).for_task("t1"))
            .unwrap();
        let blocked_id = db
            .emit(
                &FactoryEvent::new(EventKind::TaskStateChanged, r#"{"to":"blocked"}"#, 20)
                    .for_task("t1"),
            )
            .unwrap();
        assert!(!db.webhook_sent_ok_after("t1", blocked_id).unwrap());

        // Different task's ok:true does not count.
        db.emit(&FactoryEvent::new(EventKind::WebhookSent, r#"{"ok":true}"#, 25).for_task("t2"))
            .unwrap();
        assert!(!db.webhook_sent_ok_after("t1", blocked_id).unwrap());

        // Same task's ok:false and malformed JSON do not count.
        db.emit(&FactoryEvent::new(EventKind::WebhookSent, r#"{"ok":false}"#, 30).for_task("t1"))
            .unwrap();
        db.emit(&FactoryEvent::new(EventKind::WebhookSent, "not json", 31).for_task("t1"))
            .unwrap();
        assert!(!db.webhook_sent_ok_after("t1", blocked_id).unwrap());

        // Same task's ok:true after blocked_id counts.
        db.emit(&FactoryEvent::new(EventKind::WebhookSent, r#"{"ok":true}"#, 40).for_task("t1"))
            .unwrap();
        assert!(db.webhook_sent_ok_after("t1", blocked_id).unwrap());
    }

    #[test]
    fn events_after_and_max_event_id_include_global_freeze_across_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&make_task("ta", "/repo-a")).unwrap();
        db.insert_task(&make_task("tb", "/repo-b")).unwrap();

        let ev_a = db
            .emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 10).for_task("ta"))
            .unwrap();
        let _ev_b = db
            .emit(&FactoryEvent::new(EventKind::TaskStateChanged, "{}", 20).for_task("tb"))
            .unwrap();
        let ev_frozen = db
            .emit(&FactoryEvent::new(EventKind::FactoryFrozen, "{}", 30).for_task("tb"))
            .unwrap();
        let ev_unfrozen = db
            .emit(&FactoryEvent::new(EventKind::FactoryUnfrozen, "{}", 40))
            .unwrap();

        assert_eq!(db.max_event_id(Some("/repo-a")).unwrap(), ev_unfrozen);
        let seen = db.events_after(Some("/repo-a"), 0, 100).unwrap();
        let ids: Vec<i64> = seen.iter().filter_map(|e| e.id).collect();
        assert_eq!(ids, vec![ev_a, ev_frozen, ev_unfrozen]);
    }

    #[test]
    fn events_after_and_max_event_id_include_untasked_cost_events_across_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&make_task("ta", "/repo-a")).unwrap();
        db.insert_task(&make_task("tb", "/repo-b")).unwrap();

        let _ev_b = db
            .emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 10).for_task("tb"))
            .unwrap();
        let ev_paused = db
            .emit(&FactoryEvent::new(
                EventKind::CostPaused,
                r#"{"tokens_24h":1200,"cap":1000,"resume_at":90000}"#,
                20,
            ))
            .unwrap();
        let ev_resumed = db
            .emit(&FactoryEvent::new(
                EventKind::CostResumed,
                r#"{"tokens_24h":0,"cap":1000}"#,
                30,
            ))
            .unwrap();

        assert_eq!(db.max_event_id(Some("/repo-a")).unwrap(), ev_resumed);
        let seen = db.events_after(Some("/repo-a"), 0, 100).unwrap();
        let ids: Vec<i64> = seen.iter().filter_map(|e| e.id).collect();
        assert_eq!(ids, vec![ev_paused, ev_resumed]);
    }
}
