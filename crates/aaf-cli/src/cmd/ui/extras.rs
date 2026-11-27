//! Extended Web UI backend operations: task creation, worktree diff,
//! plan details, and account pool cooldown controls.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{AccountStore, Clock, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::accounts::{DiscoveryEnv, Router, binary_on_path_in, discover};
use aaf_engine::db::Db;
use aaf_engine::factory::schedule_order::normalize_scope;
use aaf_engine::git::worktree::worktree_path;
use serde_json::{Value, json};

use crate::cmd::board::store::open_db;
use crate::cmd::task_ops::{
    PLAN_APPROVAL_PREFIX, PLAN_DEBATE_STALLED_PREFIX, latest_plan_info, plan_from_task_notes,
};
use crate::ctx::{Ctx, SystemClock};

const MAX_TITLE_CHARS: usize = 200;
pub(super) const MAX_DIFF_BYTES: usize = 200 * 1024;
const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_COOLDOWN_MINS: i64 = 300;
const INVALID_REPO_MSG: &str =
    "invalid repo_root: must be an absolute path to a valid git repository";

/// Collect known repositories (name + canonical root) from DB tasks and current scope.
pub(super) fn known_repos(db: Option<&Db>, scoped_repo: Option<&Path>) -> Vec<Value> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(root) = scoped_repo
        && root.is_dir()
    {
        let p = root.to_string_lossy().into_owned();
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&p)
            .to_string();
        map.insert(p, name);
    }
    if let Some(db) = db
        && let Ok(rows) = db.repo_task_summary(None)
    {
        for (repo_root, _, _) in rows {
            let p = Path::new(&repo_root);
            if p.is_dir() {
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&repo_root)
                    .to_string();
                map.entry(repo_root).or_insert(name);
            }
        }
    }
    let mut out: Vec<Value> = map
        .into_iter()
        .map(|(root, name)| json!({ "name": name, "repo_root": root }))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

pub(super) fn task_needs_plan_approval(task: &Task) -> bool {
    task.state == TaskState::Blocked
        && matches!(
            &task.blocked_reason,
            Some(BlockedReason::NeedsHuman { why })
                if why.starts_with(PLAN_APPROVAL_PREFIX)
                    || why.starts_with(PLAN_DEBATE_STALLED_PREFIX)
        )
}

fn extract_plan_contract_hash(note_body: &str) -> Option<&str> {
    const MARKER: &str = "[plan_contract_hash=";
    let start = note_body.rfind(MARKER)? + MARKER.len();
    let rest = &note_body[start..];
    let end = rest.find(']')?;
    let hash = rest[..end].trim();
    (!hash.is_empty()).then_some(hash)
}

fn hydrate_plan_contract_by_hash(
    db: &Db,
    task_id: &str,
    expected_hash: &str,
) -> Option<aaf_domain::pipeline::PlanContract> {
    let events = db
        .events_for_task_by_kinds(
            task_id,
            &[EventKind::PlanConverged, EventKind::PlanDebateTurn],
            32,
        )
        .ok()?;
    for ev in events {
        let Ok(payload) = serde_json::from_str::<Value>(&ev.payload_json) else {
            continue;
        };
        let hash = payload["plan_contract_hash"].as_str().map(str::trim);
        if hash != Some(expected_hash) {
            continue;
        }
        if let Some(val) = payload.get("plan_contract").filter(|v| !v.is_null())
            && let Ok(contract) = serde_json::from_value(val.clone())
        {
            return Some(contract);
        }
    }
    None
}

pub(super) fn plan_detail_json(db: &Db, task: &Task) -> Value {
    let needs_approval = task_needs_plan_approval(task);
    let plan = match latest_plan_info(db, &task.id) {
        Ok(Some(plan)) => Some(plan),
        _ => plan_from_task_notes(task).map(|mut p| {
            if p.plan_contract.is_none()
                && let Some(hash) = extract_plan_contract_hash(&p.note_body).map(str::to_string)
            {
                if let Some(contract) = hydrate_plan_contract_by_hash(db, &task.id, &hash) {
                    p.plan_contract = Some(contract);
                }
                p.plan_contract_hash = Some(hash);
            }
            p
        }),
    };
    match plan {
        Some(plan) => json!({
            "available": true,
            "needs_approval": needs_approval,
            "summary": plan.summary,
            "note_body": plan.note_body,
            "hash": plan.plan_contract_hash,
            "contract": plan.plan_contract,
        }),
        None => json!({
            "available": false,
            "needs_approval": needs_approval,
        }),
    }
}

/// Create a Ready or Backlog task from `POST /api/tasks`.
pub(super) fn do_create_task(
    ctx: &Ctx,
    scoped_repo: Option<&Path>,
    body: &Value,
) -> Result<Value, String> {
    let title = body["title"].as_str().unwrap_or("").trim();
    if title.is_empty()
        || title.chars().count() > MAX_TITLE_CHARS
        || title.chars().any(char::is_control)
    {
        return Err(format!(
            "invalid title: must be one line of 1..={MAX_TITLE_CHARS} characters"
        ));
    }
    let spec_desc = body["spec"].as_str().unwrap_or("").trim();
    if spec_desc.is_empty() {
        return Err("invalid spec: description is required".into());
    }
    let mut acceptance: Vec<String> = Vec::new();
    if let Some(arr) = body["accept"].as_array() {
        for item in arr {
            if let Some(s) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                acceptance.push(s.to_string());
            }
        }
    } else if let Some(raw) = body["accept"].as_str() {
        for line in raw.lines() {
            let s = line.trim().trim_start_matches(['-', '*', '•']).trim();
            if !s.is_empty() {
                acceptance.push(s.to_string());
            }
        }
    }
    if acceptance.is_empty() {
        return Err("invalid accept: at least one acceptance criterion is required".into());
    }
    let prio_raw = body["priority"].as_str().unwrap_or("P2").trim();
    let priority = Priority::parse(prio_raw)
        .ok_or_else(|| format!("invalid priority '{prio_raw}': expected P0..P3"))?;
    let state_raw = body["state"].as_str().unwrap_or("ready").trim();
    let initial_state = match state_raw.to_ascii_lowercase().as_str() {
        "ready" => TaskState::Ready,
        "backlog" => TaskState::Backlog,
        _ => return Err("invalid state: expected 'ready' or 'backlog'".into()),
    };

    let mut scope_paths = Vec::new();
    let mut seen_scopes = HashSet::new();
    if let Some(arr) = body["scope"].as_array() {
        for item in arr {
            if let Some(raw) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                let norm =
                    normalize_scope(raw).map_err(|m| format!("invalid scope '{raw}': {m}"))?;
                if seen_scopes.insert(norm.to_ascii_lowercase()) {
                    scope_paths.push(norm);
                }
            }
        }
    } else if let Some(raw_list) = body["scope"].as_str() {
        for part in raw_list.split([',', '\n']) {
            let raw = part.trim();
            if !raw.is_empty() {
                let norm =
                    normalize_scope(raw).map_err(|m| format!("invalid scope '{raw}': {m}"))?;
                if seen_scopes.insert(norm.to_ascii_lowercase()) {
                    scope_paths.push(norm);
                }
            }
        }
    }

    let raw_repo = body["repo_root"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target_path = match (raw_repo, scoped_repo) {
        (Some(r), _) => PathBuf::from(r),
        (None, Some(s)) => s.to_path_buf(),
        (None, None) => return Err(INVALID_REPO_MSG.into()),
    };
    if !target_path.is_absolute() || !target_path.is_dir() {
        return Err(INVALID_REPO_MSG.into());
    }
    let git_root = aaf_fp::repo_root(&target_path).map_err(|_| INVALID_REPO_MSG.to_string())?;
    let canonical = fs::canonicalize(&git_root).map_err(|_| INVALID_REPO_MSG.to_string())?;
    if let Some(scoped) = scoped_repo {
        let canon_scoped = fs::canonicalize(scoped).unwrap_or_else(|_| scoped.to_path_buf());
        if canonical != canon_scoped {
            return Err("refused: repo_root is outside the scoped repository".into());
        }
    }
    let forbidden = [ctx.worktrees_dir.clone(), ctx.data_dir.clone()];
    if canonical_under_allowed_roots(&canonical, &forbidden).is_some() {
        return Err("refused: repo_root cannot be inside AAF worktrees or data directory".into());
    }
    let repo_str = canonical.to_string_lossy().into_owned();
    let repo_name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let db = open_db(ctx).map_err(|e| e.message)?;
    let now = SystemClock.now();
    let mut chosen_id = None;
    for attempt in 0_u32..100 {
        let seed = format!(
            "{repo_str}\n{title}\n{now}\n{}\n{attempt}",
            std::process::id()
        );
        let cand = format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8]);
        match db.get_task(&cand) {
            Err(PortError::NotFound(_)) => {
                chosen_id = Some(cand);
                break;
            }
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let id = chosen_id.ok_or_else(|| "could not allocate unique task id".to_string())?;
    let mut task = Task::new_root(
        id.clone(),
        repo_str.clone(),
        title.to_string(),
        ImmutableSpec {
            description: spec_desc.to_string(),
            acceptance,
        },
        priority,
        now,
    );
    task.state = initial_state;
    task.scope_paths = scope_paths.clone();
    db.insert_task(&task).map_err(|e| e.to_string())?;
    let payload = json!({
        "title": task.title,
        "state": initial_state.as_str(),
        "scope_paths": scope_paths,
        "by": "web-ui",
    });
    db.emit(&FactoryEvent::new(EventKind::TaskCreated, payload.to_string(), now).for_task(&id))
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "task_id": id,
        "title": task.title,
        "state": initial_state.as_str(),
        "priority": priority.as_str(),
        "repo_root": repo_str,
        "repo_name": repo_name,
    }))
}

pub(super) fn allowed_task_worktree_roots(ctx: &Ctx, task: &Task) -> Vec<PathBuf> {
    let mut roots = vec![ctx.worktrees_dir.clone(), PathBuf::from(&task.repo_root)];
    if let Ok(Some((wt, _))) = aaf_engine::git::worktree::existing_registered(
        Path::new(&task.repo_root),
        &ctx.worktrees_dir,
        &task.id,
    ) {
        roots.push(wt.path);
    }
    roots
}

pub(super) fn resolve_task_worktree(db: &Db, ctx: &Ctx, task: &Task) -> Option<(PathBuf, String)> {
    if let Ok(Some((wt, _))) = aaf_engine::git::worktree::existing_registered(
        Path::new(&task.repo_root),
        &ctx.worktrees_dir,
        &task.id,
    ) {
        return Some((wt.path, wt.branch));
    }
    let runs = db.runs_for_task(&task.id).unwrap_or_default();
    if let Some(latest) = runs.last() {
        let wt = PathBuf::from(&latest.worktree_path);
        if wt.is_dir() {
            return Some((wt, latest.branch.clone()));
        }
    }
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return Some((wt, format!("aaf/{}", task.id)));
    }
    None
}

pub(super) fn canonical_under_allowed_roots(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let canon = fs::canonicalize(path).ok()?;
    let ok = roots.iter().any(|r| {
        fs::canonicalize(r)
            .map(|cr| canon == cr || canon.starts_with(&cr))
            .unwrap_or(false)
    });
    ok.then_some(canon)
}

pub(super) fn run_git_bounded(wt: &Path, args: &[&str], max_bytes: usize) -> (String, bool, bool) {
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return (String::new(), false, false);
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return (String::new(), false, false);
    };
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take((max_bytes + 1) as u64).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + GIT_DIFF_TIMEOUT;
    let mut exited_ok = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(st)) => {
                exited_ok = st.success();
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut bytes = reader.join().unwrap_or_default();
    let truncated = bytes.len() > max_bytes;
    if !exited_ok && !truncated {
        return (String::new(), false, false);
    }
    if truncated {
        bytes.truncate(max_bytes);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (text, truncated, exited_ok)
}

/// Read git diff (bounded, no-textconv, timeout-protected, secret-scrubbed, structured)
/// for a task's worktree, falling back to `base..branch` in `repo_root` when cleaned.
pub(super) fn read_task_diff(db: &Db, ctx: &Ctx, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(|e| e.to_string())?;
    let runs = db.runs_for_task(&task.id).unwrap_or_default();
    let cfg = ctx.load_config(Path::new(&task.repo_root)).ok();
    let globs = cfg
        .as_ref()
        .map(|c| c.repo.gate.head_lock_globs.clone())
        .unwrap_or_else(aaf_domain::head_lock_globs::default_head_lock_globs);
    let is_locked = aaf_engine::gate::head_lock_path_matcher(&globs);
    let base_cfg = cfg.as_ref().map(|c| c.repo.gate.base_ref.as_str());
    let (diff, stat, truncated, branch, wt_disp) =
        if let Some((wt, branch)) = resolve_task_worktree(db, ctx, &task) {
            let allowed = allowed_task_worktree_roots(ctx, &task);
            let Some(canon_wt) = canonical_under_allowed_roots(&wt, &allowed) else {
                return Err("refused: worktree path outside allowed roots".into());
            };
            let base_ref = super::diff_parse::resolve_diff_base(&canon_wt, base_cfg, "HEAD");
            let (diff, truncated, _) = run_git_bounded(
                &canon_wt,
                &["diff", "--no-ext-diff", "--no-textconv", &base_ref, "--"],
                MAX_DIFF_BYTES,
            );
            let (stat, _, _) = run_git_bounded(
                &canon_wt,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--stat",
                    &base_ref,
                    "--",
                ],
                32 * 1024,
            );
            (
                diff,
                stat,
                truncated,
                branch,
                canon_wt.display().to_string(),
            )
        } else if let Some((diff, stat, truncated, branch)) =
            super::diff_parse::fallback_branch_diff(Path::new(&task.repo_root), &runs, base_cfg)
        {
            (diff, stat, truncated, branch, String::new())
        } else {
            return Ok(json!({
                "available": false, "branch": "", "worktree_path": "",
                "stat": "", "diff": "", "truncated": false,
                "note": "No active worktree found for this task yet."
            }));
        };
    let structured = super::diff_parse::parse_unified_diff(&diff, truncated, is_locked);
    let scrubbed_diff = super::diff_parse::scrub_diff_text(&diff);
    Ok(json!({
        "available": true,
        "branch": branch,
        "worktree_path": wt_disp,
        "stat": stat.trim(),
        "diff": scrubbed_diff,
        "truncated": truncated,
        "summary": structured.summary,
        "files": structured.files,
    }))
}

/// Put an account into cooldown (`clear == false`) or clear its cooldown (`clear == true`).
pub(super) fn do_account_cooldown(
    db: &Db,
    ctx: &Ctx,
    config_base: &Path,
    wanted: &str,
    minutes_raw: &str,
    clear: bool,
) -> Result<Value, String> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return Err("invalid account: id or name is required".into());
    }
    let cfg = ctx.load_config(config_base).map_err(|e| e.message)?;
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let discovered = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    let clock = SystemClock;
    let router = Router::new(
        db,
        db,
        &clock,
        ctx.data_dir.join("leases"),
        cfg.global.accounts.max_concurrent_per_account,
    );
    router.sync(&discovered).map_err(|e| e.to_string())?;
    let profile = discovered
        .iter()
        .find(|p| p.id == wanted || p.name == wanted)
        .cloned()
        .or_else(|| {
            db.list_accounts().ok().and_then(|states| {
                states.into_iter().map(|s| s.profile).find(|p| {
                    Path::new(&p.config_dir).is_dir() && (p.id == wanted || p.name == wanted)
                })
            })
        })
        .ok_or_else(|| format!("not found: account '{wanted}'"))?;
    let until = if clear {
        None
    } else {
        let trimmed = minutes_raw.trim();
        let mins = if trimmed.is_empty() {
            DEFAULT_COOLDOWN_MINS
        } else {
            trimmed
                .parse::<i64>()
                .ok()
                .filter(|&m| (1..=1440).contains(&m))
                .ok_or_else(|| "invalid minutes: expected integer in 1..=1440".to_string())?
        };
        Some(clock.now() + mins * 60)
    };
    db.set_cooldown(&profile.id, until)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "action": if clear { "account_clear" } else { "account_cooldown" },
        "account_id": profile.id,
        "account_name": profile.name,
        "cooldown_until": until,
    }))
}

const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
static SETTINGS_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn read_regular_bounded(path: &Path, max_bytes: u64) -> Result<String, std::io::Error> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    if meta.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds size limit",
        ));
    }
    let f = fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(max_bytes + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds size limit",
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf-8"))
}

#[path = "extras_account.rs"]
mod extras_account;
pub(super) use extras_account::{read_repo_local_account, write_repo_local_account_native};
