//! Factory halt event recording with per-run and 6-hour per-task deduplication.

use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use serde_json::json;

use super::Scheduler;
use crate::factory::unblock;

const HALT_DEDUP_SECS: i64 = 6 * 3600;
const HALT_HISTORY: u32 = 50;

fn normalize_halt_why(task_id: &str, why: &str) -> String {
    let prefix = format!("{task_id}: ");
    let stripped = why.strip_prefix(&prefix).unwrap_or(why);
    let mut out = String::with_capacity(stripped.len());
    let mut in_digits = false;
    for ch in stripped.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn halt_digest(task_id: &str, why: &str) -> String {
    let normalized = normalize_halt_why(task_id, why);
    let mut hex = aaf_fp::sha256_hex(normalized.as_bytes());
    hex.truncate(16);
    hex
}

impl Scheduler<'_> {
    /// Records the halt reason on `self.summary` and emits at most one
    /// `FactoryHalted` event per run (and at most once per 6 hours per task and
    /// normalized reason digest). Event persistence errors never block the halt.
    pub(in crate::factory) fn record_halt(&mut self, task_id: &str, why: String) {
        let already = self.summary.halted.is_some();
        self.summary.halted.get_or_insert(why.clone());
        if already {
            return;
        }
        if let Err(err) = self.emit_halt_event(task_id, &why) {
            let msg = format!("could not record factory halt event: {err}");
            eprintln!("aaf: {msg}");
            self.summary.errors.push(msg);
        }
    }

    fn emit_halt_event(&self, task_id: &str, why: &str) -> PortResult<()> {
        let digest = halt_digest(task_id, why);
        let cutoff = self.deps.now().saturating_sub(HALT_DEDUP_SECS);
        let recent = self.deps.store.events_for_task_by_kinds(
            task_id,
            &[EventKind::FactoryHalted],
            HALT_HISTORY,
        )?;
        let duplicate = recent.iter().any(|ev| {
            ev.created_at >= cutoff
                && serde_json::from_str::<serde_json::Value>(&ev.payload_json)
                    .ok()
                    .is_some_and(|p| p["digest"].as_str() == Some(digest.as_str()))
        });
        if duplicate {
            return Ok(());
        }
        self.deps.emit(
            EventKind::FactoryHalted,
            task_id,
            json!({
                "why": unblock::one_line(why, 500),
                "digest": digest,
            }),
        )
    }
}
