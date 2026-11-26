#!/usr/bin/env bash
# Safely reset historical AAF Studio (/studio, /api/report) telemetry data so
# post-fix measurements start clean without interrupting live running workers.
set -euo pipefail

DRY_RUN=0
ARCHIVE_CLOSED=1
RESET_CYCLES=0
RESTART_UI=0
DO_BACKUP=1
DATA_DIR="${AAF_DATA_DIR:-$HOME/.local/share/autofactory}"
HEALTH_CACHE_DIR="${AAF_HEALTH_CACHE_DIR:-$HOME/.cache/aaf-health}"

usage() {
  cat <<'EOF'
Usage: reset_studio.sh [OPTIONS]

Safely clear historical AAF Studio telemetry (finished runs, telemetry events,
root budget counters, old streams, health time-series cache, and optionally
archive old done/cancelled tasks) while preserving active tasks, accounts,
receipt keys, and live running workers.

Options:
  --dry-run        Preview counts without modifying DB or files
  --keep-closed    Do not archive existing done/cancelled tasks
  --reset-cycles   Reset unblock_cycles=0 on ready/gating tasks
  --restart-ui     Restart com.autoagentfactory.ui (may interrupt active runs!)
  --no-backup      Skip automatic SQLite backup before reset
  -h, --help       Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --keep-closed) ARCHIVE_CLOSED=0; shift ;;
    --reset-cycles) RESET_CYCLES=1; shift ;;
    --restart-ui) RESTART_UI=1; shift ;;
    --no-backup) DO_BACKUP=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

DB_PATH="$DATA_DIR/aaf.db"
if [[ ! -f "$DB_PATH" ]]; then
  echo "error: AAF database not found at $DB_PATH" >&2
  exit 1
fi

python3 - "$DB_PATH" "$DATA_DIR" "$HEALTH_CACHE_DIR" "$DRY_RUN" "$ARCHIVE_CLOSED" "$RESET_CYCLES" "$DO_BACKUP" <<'PY'
import datetime
import fcntl
import os
import pathlib
import sqlite3
import sys
import time

db_path = pathlib.Path(sys.argv[1])
data_dir = pathlib.Path(sys.argv[2])
health_cache_dir = pathlib.Path(sys.argv[3])
dry_run = sys.argv[4] == "1"
archive_closed = sys.argv[5] == "1"
reset_cycles = sys.argv[6] == "1"
do_backup = sys.argv[7] == "1"

# Structural events on active tasks that must NEVER be deleted.
KEEP_EVENT_KINDS = ("task_created", "detector_hit", "plan_generated", "pr_opened")

# Telemetry / history events that pollute Studio reports & alerts.
TELEMETRY_EVENT_KINDS = (
    "run_started",
    "run_finished",
    "gate_result",
    "cross_audit",
    "unblock_step",
    "budget_frozen",
    "factory_frozen",
    "factory_unfrozen",
    "factory_halted",
    "account_cooldown",
    "rework_requested",
    "task_state_changed",
    "merge_result",
    "pr_feedback",
)

now = int(time.time())
conn = sqlite3.connect(str(db_path), timeout=30.0)
conn.isolation_level = None  # explicit transactions
cur = conn.cursor()

# Snapshot original events_no_delete trigger SQL so we recreate it verbatim.
trig_row = cur.execute(
    "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'events_no_delete'"
).fetchone()
orig_trigger_sql = trig_row[0] if trig_row else None

running_run_ids = {
    row[0] for row in cur.execute("SELECT id FROM runs WHERE status = 'running'").fetchall()
}
finished_runs_cnt = cur.execute("SELECT COUNT(*) FROM runs WHERE status != 'running'").fetchone()[0]

placeholders = ",".join("?" for _ in TELEMETRY_EVENT_KINDS)
if running_run_ids:
    run_ph = ",".join("?" for _ in running_run_ids)
    del_events_where = (
        f"kind IN ({placeholders}) AND (run_id IS NULL OR run_id NOT IN ({run_ph}))"
    )
    del_events_params = list(TELEMETRY_EVENT_KINDS) + sorted(running_run_ids)
else:
    del_events_where = f"kind IN ({placeholders})"
    del_events_params = list(TELEMETRY_EVENT_KINDS)

telemetry_events_cnt = cur.execute(
    f"SELECT COUNT(*) FROM events WHERE {del_events_where}", del_events_params
).fetchone()[0]
kept_events_cnt = cur.execute("SELECT COUNT(*) FROM events").fetchone()[0] - telemetry_events_cnt

root_budgets_cnt = cur.execute("SELECT COUNT(*) FROM root_budgets").fetchone()[0]
closed_visible_cnt = cur.execute(
    "SELECT COUNT(*) FROM tasks WHERE state IN ('done', 'cancelled') AND archived_at IS NULL"
).fetchone()[0]
reset_cycles_cnt = cur.execute(
    "SELECT COUNT(*) FROM tasks WHERE state IN ('ready', 'gating') AND unblock_cycles > 0 AND archived_at IS NULL"
).fetchone()[0]

streams_dir = data_dir / "streams"
stale_streams = []
if streams_dir.is_dir():
    for entry in streams_dir.iterdir():
        if entry.is_file() and entry.suffix == ".jsonl" and entry.stem not in running_run_ids:
            stale_streams.append(entry)

metrics_file = health_cache_dir / "metrics.jsonl"
metrics_exists = metrics_file.exists() and metrics_file.stat().st_size > 0

print("=== AAF STUDIO RESET PLAN ===")
print(f"mode           : {'DRY-RUN (no changes)' if dry_run else 'APPLY'}")
print(f"db             : {db_path}")
print(f"live running   : {len(running_run_ids)} run(s) preserved")
print(f"runs to delete : {finished_runs_cnt} finished run(s)")
print(f"events         : delete {telemetry_events_cnt} telemetry event(s), keep {kept_events_cnt} structural/live event(s)")
print(f"root_budgets   : reset {root_budgets_cnt} row(s) & prune closed roots")
print(f"closed tasks   : {'archive ' + str(closed_visible_cnt) + ' done/cancelled task(s)' if archive_closed else 'keep visible (--keep-closed)'}")
print(f"unblock_cycles : {'reset ' + str(reset_cycles_cnt) + ' ready/gating task(s)' if reset_cycles else 'unchanged (pass --reset-cycles to zero)'}")
print(f"streams/*.jsonl: delete {len(stale_streams)} finished stream file(s)")
print(f"health metrics : {'clear ' + str(metrics_file) if metrics_exists else 'already empty'}")

if dry_run:
    conn.close()
    sys.exit(0)

backup_path = None
if do_backup:
    backup_dir = data_dir / "backups"
    backup_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    backup_path = backup_dir / f"aaf-before-studio-reset-{stamp}.db"
    dst = sqlite3.connect(str(backup_path))
    try:
        conn.backup(dst)
    finally:
        dst.close()
    os.chmod(backup_path, 0o600)
    # Keep only the 5 most recent Studio reset backups.
    backups = sorted(backup_dir.glob("aaf-before-studio-reset-*.db"))
    for old in backups[:-5]:
        try:
            old.unlink()
        except OSError:
            pass
    print(f"backup         : {backup_path}")

try:
    cur.execute("BEGIN IMMEDIATE")
    # Re-read live running runs inside the write lock so we never race a newly started run.
    running_run_ids = {
        row[0] for row in cur.execute("SELECT id FROM runs WHERE status = 'running'").fetchall()
    }
    if running_run_ids:
        run_ph = ",".join("?" for _ in running_run_ids)
        del_events_where = (
            f"kind IN ({placeholders}) AND (run_id IS NULL OR run_id NOT IN ({run_ph}))"
        )
        del_events_params = list(TELEMETRY_EVENT_KINDS) + sorted(running_run_ids)
    else:
        del_events_where = f"kind IN ({placeholders})"
        del_events_params = list(TELEMETRY_EVENT_KINDS)

    cur.execute("DELETE FROM runs WHERE status != 'running'")
    deleted_runs = cur.rowcount

    if orig_trigger_sql:
        cur.execute("DROP TRIGGER IF EXISTS events_no_delete")
    cur.execute(f"DELETE FROM events WHERE {del_events_where}", del_events_params)
    deleted_events = cur.rowcount
    if orig_trigger_sql:
        cur.execute(orig_trigger_sql)

    cur.execute(
        "DELETE FROM root_budgets "
        "WHERE root_id NOT IN ("
        "  SELECT DISTINCT root_id FROM tasks WHERE state NOT IN ('done', 'cancelled') AND archived_at IS NULL"
        ")"
    )
    pruned_budgets = cur.rowcount
    cur.execute(
        "UPDATE root_budgets SET tokens_used = 0, wall_secs_used = 0, cycles_used = 0, frozen = 0"
    )
    reset_budgets = cur.rowcount

    for k, v in (
        ("consecutive_root_fails", "0"),
        ("factory_frozen", "0"),
        ("factory_unfrozen_at", str(now)),
    ):
        cur.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) "
            "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (k, v),
        )

    archived_tasks = 0
    if archive_closed:
        cur.execute(
            "UPDATE tasks SET archived_at = ? "
            "WHERE state IN ('done', 'cancelled') AND archived_at IS NULL",
            (now,),
        )
        archived_tasks = cur.rowcount

    cycles_cleared = 0
    if reset_cycles:
        cur.execute(
            "UPDATE tasks SET unblock_cycles = 0 "
            "WHERE state IN ('ready', 'gating') AND unblock_cycles > 0 AND archived_at IS NULL"
        )
        cycles_cleared = cur.rowcount

    cur.execute("COMMIT")
except Exception:
    try:
        cur.execute("ROLLBACK")
    except Exception:
        pass
    raise

# Checkpoint WAL cleanly (best-effort if readers are open).
try:
    ck = cur.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
except sqlite3.Error as exc:
    ck = f"skipped ({exc})"
conn.close()

removed_streams = 0
if streams_dir.is_dir():
    for entry in streams_dir.iterdir():
        if entry.is_file() and entry.suffix == ".jsonl" and entry.stem not in running_run_ids:
            try:
                entry.unlink()
                removed_streams += 1
            except OSError:
                pass

if health_cache_dir.is_dir():
    lock_file = health_cache_dir / "metrics.jsonl.lock"
    with open(lock_file, "a+") as lf:
        fcntl.flock(lf.fileno(), fcntl.LOCK_EX)
        try:
            tmp = health_cache_dir / f".metrics.jsonl.tmp.{os.getpid()}"
            tmp.write_text("", encoding="utf-8")
            os.chmod(tmp, 0o600)
            os.replace(tmp, metrics_file)
        finally:
            fcntl.flock(lf.fileno(), fcntl.LOCK_UN)

wal_path = data_dir / "aaf.db-wal"
wal_kb = wal_path.stat().st_size // 1024 if wal_path.exists() else 0

print("=== APPLIED ===")
print(f"deleted_runs   : {deleted_runs} (live running kept: {len(running_run_ids)})")
print(f"deleted_events : {deleted_events} (events_no_delete trigger restored: {bool(orig_trigger_sql)})")
print(f"root_budgets   : reset={reset_budgets}, pruned={pruned_budgets}")
print(f"archived_tasks : {archived_tasks}")
print(f"reset_cycles   : {cycles_cleared}")
print(f"removed_streams: {removed_streams}")
print(f"wal_checkpoint : {ck} (wal={wal_kb}KB)")
PY

if [[ "$DRY_RUN" -eq 1 ]]; then
  exit 0
fi

if [[ "$RESTART_UI" -eq 1 && -f "$HOME/Library/LaunchAgents/com.autoagentfactory.ui.plist" ]]; then
  echo "==> Restarting com.autoagentfactory.ui (--restart-ui)..."
  /bin/launchctl kickstart -k "gui/$(id -u)/com.autoagentfactory.ui" || true
  sleep 1
fi

echo ""
echo "=== POST-RESET STATUS ==="
aaf status --all

TOKEN_FILE="$DATA_DIR/ui-service.token"
if [[ -f "$TOKEN_FILE" ]] && curl -fsS --max-time 2 "http://127.0.0.1:4317/api/health" >/dev/null 2>&1; then
  TOKEN="$(tr -d '\r\n' < "$TOKEN_FILE")"
  echo ""
  echo "=== LIVE /studio CHECK (/api/report?since=24) ==="
  if STUDIO_JSON="$(curl -fsS --max-time 5 -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:4317/api/report?since=24")"; then
    STUDIO_JSON="$STUDIO_JSON" python3 - <<'PY'
import json, os
d = json.loads(os.environ["STUDIO_JSON"])
td = d.get("token_diagnostics", {})
s = td.get("summary", {})
b = td.get("burn_rate", {})
alerts = td.get("alerts", [])
print(
    f"total_tokens={s.get('total_tokens', 0)} total_runs={s.get('total_runs', 0)} "
    f"wasted={s.get('wasted_tokens', 0)} ({s.get('wasted_runs', 0)} runs) "
    f"unmeas_long={s.get('unmeasured_long_runs', 0)}"
)
print(
    f"burn_rate: 5m={b.get('tokens_last_5m', 0)} 1h={b.get('tokens_last_1h', 0)} "
    f"24h={b.get('tokens_last_24h', 0)}"
)
print(f"alerts ({len(alerts)}): {[a.get('code') for a in alerts]}")
PY
  else
    echo "(Studio report cache refreshes within ~15s)"
  fi
fi
