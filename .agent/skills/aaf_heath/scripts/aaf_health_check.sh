#!/usr/bin/env bash
# Single-source-of-truth 4-pillar monitor (/aaf_heath): consumes `/api/report?since=24`
# (`score`, `ops_diagnostics`, `token_diagnostics`, `runtime_health`) directly from
# Studio/Cockpit UI and adds OS-only trend checks (5-tick RSS/FD trend, macOS
# memory_pressure/swap/loadavg, orphan processes) plus SQLite fallback if UI is down.
set -uo pipefail

AAF_DATA_DIR="${AAF_DATA_DIR:-$HOME/.local/share/autofactory}"
AAF_DB="$AAF_DATA_DIR/aaf.db"
UI_BASE_URL="${AAF_UI_BASE_URL:-http://127.0.0.1:4317}"
UI_HEALTH_URL="$UI_BASE_URL/api/health"
UI_TOKEN_FILE="${AAF_UI_TOKEN_FILE:-$AAF_DATA_DIR/ui-service.token}"
UI_LOG="${AAF_UI_LOG:-$HOME/Library/Logs/aaf-ui.log}"
WORKTREES_DIR="${AAF_WORKTREE_DIR:-${AAF_WORKTREES_DIR:-$HOME/.cache/autofactory/worktrees}}"
METRICS_DIR="${AAF_HEALTH_CACHE_DIR:-$HOME/.cache/aaf-health}"
METRICS_FILE="$METRICS_DIR/metrics.jsonl"

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aaf-health.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT
STUDIO_REPORT_TMP="$TMP_DIR/studio-report.json"
UI_STATUS_TMP="$TMP_DIR/ui-status.json"
STATUS_TMP="$TMP_DIR/aaf-status.txt"
PROC_TMP="$TMP_DIR/procs.txt"
mkdir -p "$METRICS_DIR"

safe_int() {
  local raw="${1:-}"
  raw="$(printf '%s' "$raw" | tr -dc '0-9' | head -c 18)"
  printf '%s' "${raw:-0}"
}

echo "=== TIME & VERSION ==="
date "+%Y-%m-%d %H:%M:%S %z"
aaf --version 2>&1 || echo "ERROR: aaf binary not found in PATH"

echo "=== AAF STATUS (--all) ==="
aaf status --all > "$STATUS_TMP" 2>&1 || true
cat "$STATUS_TMP"

echo "=== COCKPIT UI & AAF HEALTH SYNC ==="
UI_OK=0
STUDIO_SYNC_OK=0
STUDIO_SYNC_ERR=""
if UI_RESP="$(curl -fsS --max-time 3 "$UI_HEALTH_URL" 2>&1)"; then
  echo "health: $UI_RESP"
  UI_OK=1
  if [[ -r "$UI_TOKEN_FILE" ]]; then
    UI_TOK="$(tr -d '\r\n' < "$UI_TOKEN_FILE")"
    for attempt in 1 2; do
      HTTP_CODE="$(curl -sS --max-time 25 -o "$STUDIO_REPORT_TMP" -w "%{http_code}" -H "Authorization: Bearer $UI_TOK" "$UI_BASE_URL/api/report?since=24" 2>"$TMP_DIR/curl.err")"
      CURL_RC=$?
      if [[ "$CURL_RC" -eq 0 && "$HTTP_CODE" == "200" && -s "$STUDIO_REPORT_TMP" ]]; then
        STUDIO_SYNC_OK=1
        STUDIO_SYNC_ERR=""
        echo "health_sync: OK ($UI_BASE_URL/api/report?since=24, attempt=${attempt})"
        break
      fi
      CURL_MSG="$(tr '\n' ' ' < "$TMP_DIR/curl.err" | head -c 120)"
      STUDIO_SYNC_ERR="rc=${CURL_RC} http=${HTTP_CODE:-000}${CURL_MSG:+ err=${CURL_MSG}}"
      [[ "$attempt" -eq 1 ]] && sleep 1
    done
    if [[ "$STUDIO_SYNC_OK" -ne 1 ]]; then
      echo "health_sync: FAILED ($STUDIO_SYNC_ERR) — fallback to SQLite/FS probes"
    fi
    curl -fsS --max-time 5 -H "Authorization: Bearer $UI_TOK" "$UI_BASE_URL/api/status" > "$UI_STATUS_TMP" 2>/dev/null || true
    unset UI_TOK
  else
    STUDIO_SYNC_ERR="no_token_file ($UI_TOKEN_FILE)"
    echo "health_sync: NO_TOKEN_FILE ($UI_TOKEN_FILE) — fallback to SQLite/FS probes"
  fi
else
  echo "UNHEALTHY: $UI_RESP"
fi

echo "=== SYSTEM MEMORY & SWAP ==="
SWAP_LINE="$(sysctl -n vm.swapusage 2>/dev/null || true)"
MEM_FREE_PCT="$(memory_pressure 2>/dev/null | awk -F': ' '/System-wide memory free percentage/{gsub(/%/,"",$2); print $2}' | tail -n 1 || true)"
MEM_FREE_PCT="$(safe_int "${MEM_FREE_PCT:-100}")"
LOAD_AVG="$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | xargs || true)"
echo "swap: ${SWAP_LINE:-unavailable} | memory_free_pct: ${MEM_FREE_PCT}% | loadavg: ${LOAD_AVG:-n/a}"

echo "=== PROCESS MEMORY, FDs & ORPHAN WORKERS ==="
ps -axo pid,ppid,pgid,rss,etime,comm,args > "$PROC_TMP" 2>/dev/null || true
UI_PID="$(awk '$7 ~ /(^|\/)aaf$/ && $8 == "ui" && $0 ~ /--service/ {print $1; exit}' "$PROC_TMP" 2>/dev/null || true)"
UI_RSS_KB=0
UI_FDS=0
if [[ -n "$UI_PID" ]]; then
  raw_rss="$(ps -o rss= -p "$UI_PID" 2>/dev/null || true)"
  UI_RSS_KB="$(safe_int "$raw_rss")"
  raw_fds="$(lsof -p "$UI_PID" 2>/dev/null | tail -n +2 | wc -l || true)"
  UI_FDS="$(safe_int "$raw_fds")"
fi
AAF_RUN_COUNT="$(awk '$7 ~ /(^|\/)aaf$/ && $8 == "run" {c++} END {print c+0}' "$PROC_TMP" 2>/dev/null || true)"
AAF_RUN_COUNT="$(safe_int "$AAF_RUN_COUNT")"
echo "ui_pid=${UI_PID:-none} ui_rss=$((UI_RSS_KB / 1024))MB ui_fds=${UI_FDS} active_aaf_run_procs=${AAF_RUN_COUNT}"

echo "=== DEEP ANALYSIS (AAF HEALTH SINGLE-SOURCE-OF-TRUTH + OS TRENDS) ==="
python3 - "$AAF_DB" "$AAF_DATA_DIR" "$STUDIO_REPORT_TMP" "$STUDIO_SYNC_OK" "$STUDIO_SYNC_ERR" "$METRICS_FILE" "$UI_OK" "${UI_PID:-0}" "$UI_RSS_KB" "$UI_FDS" "$AAF_RUN_COUNT" "$SWAP_LINE" "$MEM_FREE_PCT" "$STATUS_TMP" "$PROC_TMP" "$WORKTREES_DIR" "$UI_LOG" "$UI_STATUS_TMP" <<'PY'
import fcntl, json, os, re, sqlite3, sys, tempfile, time
from datetime import datetime

(
    db_path, data_dir, studio_file, studio_ok_s, studio_sync_err, metrics_file, ui_ok_s,
    ui_pid_s, ui_rss_kb_s, ui_fds_s, aaf_run_cnt_s, swap_line, mem_free_s,
    status_file, proc_file, default_wt_dir, ui_log_path, ui_status_file
) = sys.argv[1:19]

studio_ok = int(studio_ok_s)
ui_ok = int(ui_ok_s)
ui_pid = int(ui_pid_s or 0)
ui_rss_kb = int(ui_rss_kb_s or 0)
ui_fds = int(ui_fds_s or 0)
aaf_run_cnt = int(aaf_run_cnt_s or 0)
mem_free_pct = float(mem_free_s or 100)

status_out = open(status_file, encoding="utf-8", errors="replace").read() if os.path.exists(status_file) else ""
proc_lines = open(proc_file, encoding="utf-8", errors="replace").read().splitlines() if os.path.exists(proc_file) else []

sup_enabled = "unknown"
sup_path = os.path.join(data_dir, "ui-supervisor.json")
if os.path.exists(sup_path):
    try:
        sup_cfg = json.load(open(sup_path, encoding="utf-8"))
        sup_enabled = "true" if sup_cfg.get("enabled") is True else "false"
        print(f"ui-supervisor: enabled={sup_enabled} allowed_repos={len(sup_cfg.get('allowed_repos') or [])}")
    except Exception as e:
        print(f"ui-supervisor: parse_error={e}")

swap_used_mb = 0.0
m_swap = re.search(r"used\s*=\s*([0-9.]+)([MG])", swap_line or "")
if m_swap:
    val = float(m_swap.group(1))
    swap_used_mb = val * 1024.0 if m_swap.group(2) == "G" else val

now = int(time.time())
anomalies, warnings, infos = [], [], []
worktrees_dir = default_wt_dir
spike_limit_1h = 350000

def fmt_ts(ts):
    if not ts:
        return "-"
    try:
        dt = datetime.fromtimestamp(int(ts)).strftime("%m-%d %H:%M:%S")
        age = max(0, now - int(ts))
        age_s = f"{age}s" if age < 120 else (f"{age//60}m" if age < 7200 else f"{age//3600}h")
        return f"{dt} ({age_s} ago)"
    except Exception:
        return str(ts)

def fmt_dur(secs):
    if secs is None:
        return "n/a"
    s = int(round(secs))
    if s < 60:
        return f"{s}s"
    if s < 3600:
        return f"{s//60}m{s%60:02d}s"
    return f"{s//3600}h{(s%3600)//60:02d}m"

def file_kb(p):
    try:
        return os.path.getsize(p) // 1024
    except OSError:
        return 0

def dir_kb(p):
    tot = 0
    try:
        for ent in os.scandir(p):
            if ent.is_file(follow_symlinks=False):
                tot += ent.stat(follow_symlinks=False).st_size
    except OSError:
        pass
    return tot // 1024

def route_alert(prefix, al):
    sev = str(al.get("severity", "medium")).lower()
    cat = al.get("category")
    code = al.get("code", "unknown")
    label = f"{prefix}:{cat}:{code}" if cat else f"{prefix}:{code}"
    title = al.get("title", "")
    detail = str(al.get("detail") or "").strip()
    if len(detail) > 180:
        detail = detail[:177] + "..."
    active_recent = al.get("active_recent", True)
    tids = ",".join(al.get("task_ids") or [])
    suffix = f" [tasks: {tids}]" if tids else ""
    det_suffix = f" — {detail}" if detail else ""
    tag = "ACTIVE" if active_recent else "HIST24H"
    print(f"  [{prefix}:{sev}:{tag}] {code}: {title}{suffix}{det_suffix}")
    if not active_recent:
        infos.append(f"{prefix}_hist24h:{code}: {title}{suffix}")
    elif sev in ("critical", "high"):
        anomalies.append(f"{label} ({sev}): {title}{suffix}{det_suffix}")
    else:
        warnings.append(f"{label} ({sev}): {title}{suffix}{det_suffix}")

db_kb = file_kb(db_path)
wal_kb = file_kb(db_path + "-wal")
streams_kb = dir_kb(os.path.join(data_dir, "streams"))
receipts_kb = dir_kb(os.path.join(data_dir, "receipts"))

if ui_ok == 1 and studio_ok != 1:
    anomalies.append(f"health_sync_failed: /api/report?since=24 unavailable ({studio_sync_err or 'unknown'}) — running SQLite/FS fallback")

# A. Single Source of Truth from AAF Health `/api/report?since=24`
if studio_ok == 1 and os.path.exists(studio_file):
    try:
        rep = json.load(open(studio_file, encoding="utf-8"))
        if rep.get("since_hours") != 24:
            warnings.append(f"studio_window_mismatch: expected since_hours=24, got {rep.get('since_hours')}")
        sc = rep.get("score") or {}
        r_obj = rep.get("report") or {}
        aut = r_obj.get("autonomy") or {}
        lt = r_obj.get("lead_time") or {}
        td = rep.get("token_diagnostics") or {}
        s = td.get("summary") or {}
        br = td.get("burn_rate") or {}
        if int(br.get("spike_limit_1h") or 0) > 0:
            spike_limit_1h = int(br["spike_limit_1h"])
        ops = rep.get("ops_diagnostics") or {}
        oq = ops.get("quality") or {}
        osp = ops.get("speed") or {}
        oef = ops.get("efficiency") or {}
        rh = rep.get("runtime_health") or {}
        if isinstance(rh.get("worktrees"), dict) and rh["worktrees"].get("dir"):
            worktrees_dir = rh["worktrees"]["dir"]

        # 1) Quality (from Health `score` + `ops_diagnostics.quality`)
        print("-- Quality & Factory Score (Health 24h) --")
        fp_rate = aut.get("gate_first_pass_rate_pct")
        fp_str = f"{fp_rate:.1f}% ({aut.get('gate_first_pass_clear',0)}/{aut.get('gate_tasks_evaluated',0)})" if fp_rate is not None else "n/a"
        ca_rate = aut.get("cross_audit_fail_rate_pct")
        ca_str = f"{ca_rate:.1f}% fail ({aut.get('cross_audit_failed',0)}/{aut.get('cross_audit_total',0)})" if ca_rate is not None else "n/a"
        auto_rate = aut.get("autonomy_rate_pct")
        auto_str = f"{auto_rate:.1f}% ({aut.get('done_without_human',0)}/{aut.get('done_tasks',0)})" if auto_rate is not None else "n/a"
        print(f"  factory_score={sc.get('score','n/a')}/100 (grade={sc.get('grade','n/a')}) | first_pass_gate={fp_str} | cross_audit={ca_str} | autonomy={auto_str}")
        if oq:
            print(
                f"  scores: mech_avg={oq.get('mech_avg','n/a')} (n={oq.get('mech_samples',0)}) | "
                f"audit_eff_avg={oq.get('audit_effective_avg','n/a')} | llm_avg={oq.get('llm_avg','n/a')} (n={oq.get('audit_samples',0)}) | "
                f"clear_mech_zero={oq.get('clear_with_mech_zero_count',0)} | vacuous_warns={oq.get('vacuous_test_warns',0)} | "
                f"churn={len(oq.get('high_rework_churn_tasks') or [])}"
            )
        for bn in (sc.get("bottlenecks") or [])[:2]:
            print(f"  bottleneck[{bn.get('severity','info')}]: {bn.get('title')} — {bn.get('detail')}")

        # 2) Speed, Slots, Accounts & Live Bottlenecks
        print("-- Speed, Slots, Accounts & Throughput (Health 24h) --")
        if osp:
            print(
                f"  throughput: done_1h={osp.get('done_1h',0)} done_24h={osp.get('done_window',0)} | "
                f"coding p50={fmt_dur(osp.get('coding_dur_p50_secs'))} p90={fmt_dur(osp.get('coding_dur_p90_secs'))} (n={osp.get('coding_samples',0)}) | "
                f"audit p50={fmt_dur(osp.get('audit_dur_p50_secs'))} | "
                f"gate p50={fmt_dur(osp.get('gate_dur_p50_secs'))} avg={fmt_dur(osp.get('gate_avg_secs'))} (n={osp.get('gate_samples',0)})"
            )
            stuck_list = osp.get("stuck_gating_tasks") or []
            spin_list = osp.get("gating_spin_tasks") or []
            stalled_list = osp.get("stalled_workers") or []
            long_list = osp.get("long_running_workers") or []
            loop_list = osp.get("looping_workers") or []
            orphan_list = osp.get("dead_orphan_slots") or []
            dag_list = osp.get("dag_bottlenecks") or []
            stale_rev = osp.get("stale_review_tasks") or []
            ap = osp.get("account_pool") or {}
            print(
                f"  slots: active={osp.get('active_slots',0)} draining={osp.get('draining',False)} "
                f"waiters={osp.get('waiters',0)} oldest_wait={fmt_dur(osp.get('oldest_wait_secs',0))} "
                f"dead_orphans={len(orphan_list)} stuck_gating={len(stuck_list)} gating_spin={len(spin_list)} "
                f"stalled={len(stalled_list)} loops={len(loop_list)} long_runs={len(long_list)} "
                f"dag_blocks={len(dag_list)} stale_rev={len(stale_rev)} | lead_time p50={fmt_dur(lt.get('p50_secs'))}"
            )
            if ap.get("total_accounts", 0) > 0:
                provs = ", ".join(
                    f"{p.get('provider')}:{p.get('available',0)}/{p.get('total',0)}"
                    f"(leased={p.get('leased',0)},cool={p.get('cooling',0)})"
                    for p in (ap.get("by_provider") or [])
                )
                print(
                    f"  accounts: avail={ap.get('total_available',0)}/{ap.get('total_accounts',0)} "
                    f"leased={ap.get('total_leased',0)} cooling={ap.get('total_cooling',0)} "
                    f"waiting_for_account={ap.get('waiting_for_account',0)}"
                    + (f" [{provs}]" if provs else "")
                )
            if stuck_list:
                print("  stuck_gating: " + ", ".join(
                    f"{x.get('task_id')}({fmt_dur(x.get('age_secs'))}, cause={x.get('cause','?')})"
                    for x in stuck_list[:6]
                ))
            if spin_list:
                print("  gating_spin: " + ", ".join(
                    f"{x.get('task_id')}(gates_30m={x.get('gate_count_30m',0)})" for x in spin_list[:4]
                ))
            if loop_list:
                print("  looping_workers: " + ", ".join(
                    f"{x.get('task_id')}/{str(x.get('run_id',''))[:10]}(x{x.get('repeat_count',0)} {str(x.get('signature',''))[:60]})"
                    for x in loop_list[:4]
                ))
            if stalled_list:
                print("  stalled_workers: " + ", ".join(
                    f"{x.get('task_id')}/{str(x.get('run_id',''))[:10]}(idle={fmt_dur(x.get('idle_secs'))}, last={x.get('last_tool') or x.get('last_type') or 'none'}{':' + str(x.get('last_target'))[:40] if x.get('last_target') else ''})"
                    for x in stalled_list[:6]
                ))
            if long_list:
                print("  long_running_workers: " + ", ".join(
                    f"{x.get('task_id')}/{str(x.get('run_id',''))[:10]}(age={fmt_dur(x.get('age_secs'))}, last={x.get('last_tool') or x.get('last_type') or 'none'})"
                    for x in long_list[:4]
                ))
            if dag_list:
                print("  dag_bottlenecks: " + ", ".join(
                    f"{x.get('blocker_id')}({x.get('blocker_state','?')}) blocking {x.get('blocked_count',0)} ready"
                    for x in dag_list[:5]
                ))
            if orphan_list:
                print("  dead_orphan_slots: " + ", ".join(str(x) for x in orphan_list[:6]))

        # 3) Token & Efficiency (from Health `token_diagnostics` + `ops_diagnostics.efficiency`)
        print("-- Token & Efficiency (Health 24h) --")
        tpd = oef.get("tokens_per_done")
        io_r = oef.get("in_out_ratio")
        print(
            f"  total={s.get('total_tokens',0):,} ({s.get('tokens_in',0):,} in / {s.get('tokens_out',0):,} out, in:out={f'{io_r}:1' if io_r is not None else 'n/a'}) | "
            f"tok/done={f'{tpd:,}' if tpd is not None else 'n/a'} | "
            f"wasted={s.get('wasted_tokens',0):,} ({s.get('wasted_pct',0)}%, {s.get('wasted_runs',0)} runs) | "
            f"audit={s.get('audit_tokens',0):,} ({s.get('audit_pct',0)}%, avg {s.get('avg_audit_tokens',0):,}/audit) | avg_coding={s.get('avg_coding_tokens',0):,}/run"
        )
        print(
            f"  burn_rate: 5m={br.get('tokens_last_5m',0):,} | 1h={br.get('tokens_last_1h',0):,} | 24h={br.get('tokens_last_24h',0):,} | "
            f"daily_cap={br.get('daily_token_cap',0):,} | dup_audits={s.get('duplicate_audit_groups',0)} | "
            f"unmeas_long={s.get('unmeasured_long_runs',0)} ({s.get('unmeasured_long_secs',0)}s) | zero_tok={s.get('zero_token_runs',0)}/{s.get('total_runs',0)}"
        )
        top_tasks = oef.get("top_token_tasks") or []
        if top_tasks:
            print("  top_token_tasks: " + ", ".join(
                f"{t['id']}({t.get('state')}, {t.get('tokens_total',0):,} tok, {t.get('runs_total',0)} runs, wasted={t.get('wasted_tokens',0):,})"
                for t in top_tasks
            ))
        if br.get("config_error"):
            anomalies.append(f"studio:config_error: {br['config_error']}")

        # 4) Unified alerts from all 3 Health alert streams
        for al in (ops.get("alerts") or []):
            route_alert("studio_ops", al)
        for al in (td.get("alerts") or []):
            route_alert("studio", al)
        for rha in (rh.get("alerts") or []):
            route_alert("runtime", rha)
    except Exception as e:
        anomalies.append(f"studio_report_parse_error: {e}")

# B. Direct SQLite Board Snapshot (live tasks/runs/cooldowns + fallback probes when Health sync fails)
tot_in = tot_out = runs_total = 0
wt_cnt = 0
if os.path.isdir(worktrees_dir):
    for rh_dir in os.listdir(worktrees_dir):
        rp = os.path.join(worktrees_dir, rh_dir)
        if os.path.isdir(rp):
            wt_cnt += sum(1 for d in os.listdir(rp) if os.path.isdir(os.path.join(rp, d)))

print(f"-- Footprint: aaf.db={db_kb}KB wal={wal_kb}KB streams={streams_kb}KB receipts={receipts_kb}KB worktrees={wt_cnt} dirs ({worktrees_dir}) --")

if os.path.exists(db_path):
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    c = conn.cursor()

    r_tot = c.execute("SELECT count(*) n, COALESCE(sum(tokens_in),0) tin, COALESCE(sum(tokens_out),0) tout FROM runs").fetchone()
    runs_total, tot_in, tot_out = r_tot["n"], r_tot["tin"], r_tot["tout"]
    running_runs_cnt = c.execute("SELECT count(*) n FROM runs WHERE status = 'running'").fetchone()["n"]
    if running_runs_cnt > 0 and aaf_run_cnt == 0:
        anomalies.append(f"orphan_running_runs_no_aaf_run: {running_runs_cnt} run(s) marked 'running' in DB with 0 active 'aaf run' process")

    print("-- Active / Attention tasks (blocked, review, gating, running) --")
    rows = list(c.execute("""
        SELECT id, repo_root, state, title, updated_at, blocked_reason, unblock_cycles
        FROM tasks
        WHERE archived_at IS NULL AND state IN ('blocked','review','gating','running')
        ORDER BY CASE state WHEN 'blocked' THEN 0 WHEN 'review' THEN 1 WHEN 'gating' THEN 2 ELSE 3 END, updated_at DESC
        LIMIT 30
    """))
    if not rows:
        print("  (none)")
    factory_blocked = []
    fb_stuck_high, fb_stuck_warn = [], []
    for r in rows:
        repo = os.path.basename(r["repo_root"] or "")
        br_raw = r["blocked_reason"] or ""
        br_kind = ""
        br_why = ""
        if br_raw:
            try:
                parsed_br = json.loads(br_raw)
                if isinstance(parsed_br, dict):
                    br_kind = parsed_br.get("kind", "")
                    br_why = str(parsed_br.get("why", ""))
                else:
                    br_kind = str(parsed_br)
            except Exception:
                br_kind = br_raw[:40]
        if r["state"] == "blocked" and (
            br_kind not in ("needs_human", "dependency")
            or (br_kind == "needs_human" and br_why.startswith("factory error:"))
        ):
            factory_blocked.append(f"{r['id']}({br_kind or 'blocked'})")
        if studio_ok != 1 and r["state"] == "gating" and r["updated_at"]:
            age = max(0, now - int(r["updated_at"]))
            if age >= 2700:
                fb_stuck_high.append(f"{r['id']}({fmt_dur(age)})")
            elif age >= 1200:
                fb_stuck_warn.append(f"{r['id']}({fmt_dur(age)})")
        br = f" | blocked={br_kind}" if br_kind else ""
        cyc = f" | cyc={r['unblock_cycles']}" if r["unblock_cycles"] else ""
        print(f"  [{r['state']}] {r['id']} ({repo}) upd={fmt_ts(r['updated_at'])}{cyc}{br} :: {(r['title'] or '')[:60]}")
    if factory_blocked:
        anomalies.append(f"factory_blocked_tasks: {', '.join(factory_blocked[:6])}")
    if studio_ok != 1:
        if fb_stuck_high:
            anomalies.append(f"fallback:stuck_gating_tasks (high): {', '.join(fb_stuck_high[:6])}")
        elif fb_stuck_warn:
            warnings.append(f"fallback:stuck_gating_tasks (medium): {', '.join(fb_stuck_warn[:6])}")
        fb_stalled, fb_long = [], []
        for rr in c.execute("SELECT id, task_id, started_at FROM runs WHERE status = 'running'"):
            age = max(0, now - int(rr["started_at"] or now))
            if age >= 1500:
                fb_long.append((age >= 2400, f"{rr['task_id']}/{rr['id'][:10]}({fmt_dur(age)})"))
            sp = os.path.join(data_dir, "streams", f"{rr['id']}.jsonl")
            idle = max(0, now - int(os.path.getmtime(sp))) if os.path.exists(sp) else age
            if idle >= 300:
                fb_stalled.append((idle >= 600, f"{rr['task_id']}/{rr['id'][:10]}(idle={fmt_dur(idle)})"))
        if any(h for h, _ in fb_stalled):
            anomalies.append("fallback:stalled_worker (high): " + ", ".join(x for _, x in fb_stalled[:6]))
        elif fb_stalled:
            warnings.append("fallback:stalled_worker (medium): " + ", ".join(x for _, x in fb_stalled[:6]))
        if any(h for h, _ in fb_long):
            anomalies.append("fallback:long_running_worker (high): " + ", ".join(x for _, x in fb_long[:6]))
        elif fb_long:
            warnings.append("fallback:long_running_worker (medium): " + ", ".join(x for _, x in fb_long[:6]))
        q_dir = os.path.join(data_dir, "throttle", "queue")
        if os.path.isdir(q_dir):
            waits = [max(0, now - int(os.path.getmtime(os.path.join(q_dir, f)))) for f in os.listdir(q_dir) if f.endswith(".wait")]
            if waits and max(waits) >= 300:
                anomalies.append(f"fallback:heavy_slot_queue_stalled: {len(waits)} waiter(s), oldest={fmt_dur(max(waits))}")

    print("-- Recently completed tasks (done in last 24h) --")
    done_rows = list(c.execute("""
        SELECT id, repo_root, title, updated_at, archived_at FROM tasks
        WHERE state = 'done' AND updated_at >= ?
        ORDER BY updated_at DESC LIMIT 10
    """, (now - 86400,)))
    if not done_rows:
        print("  (none)")
    for r in done_rows:
        tag = "done·archived" if r["archived_at"] else "done"
        print(f"  [{tag}] {r['id']} ({os.path.basename(r['repo_root'] or '')}) @ {fmt_ts(r['updated_at'])} :: {(r['title'] or '')[:60]}")

    r_1h = c.execute("SELECT COALESCE(sum(tokens_in + tokens_out), 0) tot_1h FROM runs WHERE COALESCE(ended_at, started_at) >= ?", (now - 3600,)).fetchone()
    if r_1h["tot_1h"] > spike_limit_1h:
        anomalies.append(f"token_burn_spike_1h: {r_1h['tot_1h']:,} total tokens in last 1h (>{spike_limit_1h:,})")

    cooling = list(c.execute("SELECT name, provider, cooldown_until FROM accounts WHERE cooldown_until IS NOT NULL AND cooldown_until > ?", (now,)))
    if cooling:
        print("-- Accounts in Cooldown --")
        for a in cooling:
            print(f"  {a['name']} ({a['provider']}) cooldown remaining {fmt_dur(int(a['cooldown_until']) - now)}")
        warnings.append(f"accounts_in_cooldown: {len(cooling)} ({','.join(a['name'] for a in cooling)})")

    print("-- Latest 6 runs --")
    for r in c.execute("SELECT id, task_id, executor, status, exit_code, tokens_in, tokens_out, started_at, ended_at FROM runs ORDER BY started_at DESC LIMIT 6"):
        dur = f"{max(0, int(r['ended_at']) - int(r['started_at']))}s" if (r["started_at"] and r["ended_at"]) else (f"{max(0, now - int(r['started_at']))}s+" if (r["started_at"] and r["status"] == "running") else "-")
        print(f"  {r['id'][:10]} task={r['task_id']} exec={r['executor']} st={r['status']} rc={r['exit_code']} dur={dur} tok={r['tokens_in']}/{r['tokens_out']} start={fmt_ts(r['started_at'])}")

    print("-- Latest 8 events --")
    for r in c.execute("SELECT id, task_id, kind, created_at, substr(payload_json, 1, 150) as p FROM events ORDER BY id DESC LIMIT 8"):
        print(f"  #{r['id']} [{r['kind']}] task={r['task_id']} @ {fmt_ts(r['created_at'])} :: {r['p']}")

# C. OS-level checks outside Health API (supervisor backoff, disk space, Gradle/Kotlin daemons, orphan workers, 5-tick RSS/FD trend, macOS pressure, recent ui.log panics)
if ui_status_file and os.path.isfile(ui_status_file):
    try:
        ui_st = json.load(open(ui_status_file, encoding="utf-8"))
        sup_bo = (ui_st.get("supervisor") or {}).get("backoff") or {}
        active_bo = {k: int(v) for k, v in sup_bo.items() if isinstance(v, (int, float)) and v > 0}
        if active_bo:
            bo_str = ", ".join(f"{k}={v}s" for k, v in sorted(active_bo.items()))
            warnings.append(f"supervisor_repo_backoff: {bo_str}")
    except Exception:
        pass

disk_pools = {}
for label, path in [("data_dir", data_dir), ("worktrees", worktrees_dir)]:
    check_p = path if os.path.exists(path) else os.path.dirname(path)
    if not os.path.exists(check_p):
        continue
    try:
        st_vfs = os.statvfs(check_p)
        free_gb = (st_vfs.f_bavail * st_vfs.f_frsize) / (1024 ** 3)
        tot_gb = (st_vfs.f_blocks * st_vfs.f_frsize) / (1024 ** 3)
        used_pct = ((tot_gb - free_gb) * 100.0 / tot_gb) if tot_gb > 0 else 0.0
        # Deduplicate shared APFS containers (different f_fsid, identical total & free pool)
        pool_key = (round(tot_gb, 0), round(free_gb, 1))
        if pool_key in disk_pools:
            disk_pools[pool_key]["labels"].append(label)
        else:
            disk_pools[pool_key] = {"labels": [label], "free_gb": free_gb, "used_pct": used_pct, "path": check_p}
    except OSError:
        pass
disk_parts = []
for info in disk_pools.values():
    lbl = "+".join(info["labels"])
    free_gb, used_pct = info["free_gb"], info["used_pct"]
    disk_parts.append(f"{lbl}={free_gb:.1f}GB free ({used_pct:.0f}% used)")
    if free_gb < 5.0 or used_pct >= 95.0:
        anomalies.append(f"low_disk_space ({lbl}): only {free_gb:.1f} GiB free ({used_pct:.0f}% used) at {info['path']}")
    elif free_gb < 15.0 or used_pct >= 88.0:
        warnings.append(f"low_disk_space ({lbl}): {free_gb:.1f} GiB free ({used_pct:.0f}% used) at {info['path']}")

gradle_cnt = 0
gradle_rss_kb = 0
for ln in proc_lines[1:]:
    if ("GradleDaemon" in ln or "KotlinCompileDaemon" in ln) and "aaf_health_check" not in ln:
        parts = ln.split()
        if len(parts) >= 4:
            gradle_cnt += 1
            try:
                gradle_rss_kb += int(parts[3])
            except ValueError:
                pass
gradle_gb = gradle_rss_kb / (1024.0 * 1024.0)
print(f"-- OS Disk & Build Daemons: {' | '.join(disk_parts) if disk_parts else 'disk=n/a'} | gradle_kotlin_daemons={gradle_cnt} ({gradle_gb:.2f} GiB RSS) --")
if gradle_gb > 10.0 or (aaf_run_cnt == 0 and gradle_cnt > 0 and gradle_gb > 3.0):
    anomalies.append(f"orphan_gradle_daemons: {gradle_cnt} Gradle/Kotlin daemon(s) holding {gradle_gb:.2f} GiB RSS (aaf_run={aaf_run_cnt})")
elif gradle_cnt >= 4 or gradle_gb > 6.0:
    warnings.append(f"gradle_daemons_high: {gradle_cnt} Gradle/Kotlin daemon(s) using {gradle_gb:.2f} GiB RSS")

wt_needle = os.path.basename(worktrees_dir.rstrip("/")) or ".aaf-worktrees"
orphan_workers = [ln.strip()[:120] for ln in proc_lines[1:] if (worktrees_dir in ln or wt_needle in ln) and "aaf_health_check" not in ln and aaf_run_cnt == 0]
if orphan_workers:
    anomalies.append(f"orphan_worker_process: {len(orphan_workers)} proc(s) alive under {wt_needle} with 0 aaf run")

if ui_log_path and os.path.isfile(ui_log_path):
    try:
        if (now - int(os.path.getmtime(ui_log_path))) <= 2100:
            with open(ui_log_path, "rb") as uf:
                uf.seek(0, os.SEEK_END)
                size = uf.tell()
                uf.seek(max(0, size - 16384))
                tail_txt = uf.read().decode("utf-8", errors="replace")
            fatal_hits = [ln.strip()[:140] for ln in tail_txt.splitlines()[-80:] if re.search(r"\b(panicked at|fatal error|SIGSEGV|SIGABRT)\b", ln, re.I)]
            if fatal_hits:
                anomalies.append(f"ui_log_panic_or_fatal: {fatal_hits[-1]}")
    except OSError:
        pass

lock_path = metrics_file + ".lock"
history = []
try:
    with open(lock_path, "a+", encoding="utf-8") as lf:
        fcntl.flock(lf.fileno(), fcntl.LOCK_EX)
        if os.path.exists(metrics_file):
            try:
                history = [json.loads(ln) for ln in open(metrics_file, encoding="utf-8") if ln.strip()]
            except Exception:
                history = []
        history.append({
            "ts": now, "ui_pid": ui_pid, "ui_rss_kb": ui_rss_kb, "ui_fds": ui_fds,
            "aaf_run_cnt": aaf_run_cnt, "tot_in": tot_in, "tot_out": tot_out,
            "runs_total": runs_total, "db_kb": db_kb, "wal_kb": wal_kb,
            "streams_kb": streams_kb, "wt_cnt": wt_cnt,
        })
        history = history[-288:]
        fd, tmp_m = tempfile.mkstemp(dir=os.path.dirname(metrics_file), prefix=".metrics.")
        with os.fdopen(fd, "w", encoding="utf-8") as tf:
            for h in history:
                tf.write(json.dumps(h) + "\n")
        os.replace(tmp_m, metrics_file)
except Exception as e:
    warnings.append(f"metrics_write_failed: {e}")

print("-- Time-Series Delta & Memory/FD Leak Trends --")
if len(history) >= 2:
    prev = history[-2]
    dt_s = max(1, now - prev["ts"])
    d_tok = (tot_in + tot_out) - (prev.get("tot_in", tot_in) + prev.get("tot_out", tot_out))
    d_rss = (ui_rss_kb - prev.get("ui_rss_kb", ui_rss_kb)) / 1024.0 if ui_pid == prev.get("ui_pid") else 0.0
    d_fds = (ui_fds - prev.get("ui_fds", ui_fds)) if ui_pid == prev.get("ui_pid") else 0
    print(f"  since_prev ({dt_s}s): delta_tok=+{d_tok:,} ui_rss_delta={d_rss:+.1f}MB ui_fds_delta={d_fds:+d} samples={len(history)}")
    if dt_s <= 3600 and d_tok > 900000:
        anomalies.append(f"token_spike_since_prev_tick: +{d_tok:,} total tokens in {dt_s}s")
else:
    print("  baseline recorded (sample #1)")

same_pid = [h for h in history if h.get("ui_pid") == ui_pid and ui_pid > 0]
if len(same_pid) >= 5:
    last5 = same_pid[-5:]
    rss_vals = [h["ui_rss_kb"] for h in last5]
    fd_vals = [h["ui_fds"] for h in last5]
    rss_growth_mb = (rss_vals[-1] - rss_vals[0]) / 1024.0
    fd_growth = fd_vals[-1] - fd_vals[0]
    if all(rss_vals[i] < rss_vals[i+1] for i in range(4)) and rss_growth_mb >= 20.0:
        anomalies.append(f"ui_memory_leak_trend: pid={ui_pid} RSS rose 5 ticks straight (+{rss_growth_mb:.1f}MB to {rss_vals[-1]/1024:.1f}MB)")
    if all(fd_vals[i] < fd_vals[i+1] for i in range(4)) and fd_growth >= 32:
        anomalies.append(f"ui_fd_leak_trend: pid={ui_pid} FDs rose 5 ticks straight (+{fd_growth} to {fd_vals[-1]})")

if studio_ok != 1:
    if ui_rss_kb > 262144:
        anomalies.append(f"ui_memory_high: pid={ui_pid} RSS={ui_rss_kb/1024:.1f}MB (>256MB)")
    if ui_fds > 256:
        anomalies.append(f"ui_fds_high: pid={ui_pid} open_fds={ui_fds} (>256)")
    if wal_kb > 65536:
        anomalies.append(f"sqlite_wal_bloat: aaf.db-wal={wal_kb/1024:.1f}MB (>64MB)")

if mem_free_pct < 15.0:
    anomalies.append(f"system_memory_pressure: free={mem_free_pct:.0f}% (<15%)")
if swap_used_mb > 6144.0:
    warnings.append(f"high_swap_usage: {swap_used_mb:.0f}MB used (>6GB)")

if ui_ok != 1:
    anomalies.append("cockpit_ui_unreachable")
if "frozen=true" in status_out:
    anomalies.append("factory_frozen")
if sup_enabled == "false":
    infos.append("ui_supervisor_paused (enabled=false)")

print("=== ANOMALY & LEAK SUMMARY ===")
for a in anomalies:
    print(f"ANOMALY: {a}")
for w in warnings:
    print(f"WARN: {w}")
for i in infos:
    print(f"INFO: {i}")
if not anomalies and not warnings:
    print("OK: zero active anomalies or leaks detected")
PY

echo "=== SUPERVISOR LOG TAILS (recently updated) ==="
if [[ -d "$AAF_DATA_DIR/supervisor-logs" ]]; then
  while IFS= read -r logf; do
    [[ -f "$logf" ]] || continue
    echo "--- $(basename "$logf") ---"
    tail -n 5 "$logf"
  done < <(ls -t "$AAF_DATA_DIR/supervisor-logs"/*.log 2>/dev/null | head -n 2)
fi

echo "=== AAF-UI LOG (recent errors / last 3 lines) ==="
if [[ -f "$UI_LOG" ]]; then
  ERR_LINES="$(tail -n 200 "$UI_LOG" | grep -i -E "error|panic|failed|fatal|warn" | tail -n 6 || true)"
  if [[ -n "$ERR_LINES" ]]; then
    echo "[errors/warns]"
    echo "$ERR_LINES"
  fi
  echo "[tail]"
  tail -n 3 "$UI_LOG"
fi
