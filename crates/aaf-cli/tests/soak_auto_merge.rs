#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 5-cycle soak test for `aaf run --unsandboxed --auto-merge` with a fake
//! custom executor, verifying resource invariants after every cycle:
//! task state, clean git/worktree/branches/refs, all `*.lock` files unlocked,
//! agent and agent-spawned background processes dead, gate background process
//! observation/cleanup, bounded `<data>` disk growth, and total runtime <= 120s.
//!
//! Note: the `custom` executor does not acquire account leases
//! (`factory/adapters.rs`), so `leases/*.lock` is verified whenever present.

#[path = "factory_support.rs"]
mod support;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use support::{Fixture, git};

const CYCLES: usize = 5;
const MAX_GROWTH_PER_CYCLE_BYTES: u64 = 256 * 1024;
const MAX_DURATION: Duration = Duration::from_secs(120);

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcInfo {
    lstart: String,
    comm: String,
}

fn parse_lstart_and_comm(raw: &str) -> Option<ProcInfo> {
    let mut parts = raw.split_whitespace();
    let mut lstart_tokens = Vec::with_capacity(5);
    for _ in 0..5 {
        lstart_tokens.push(parts.next()?);
    }
    let comm = parts.collect::<Vec<_>>().join(" ");
    if comm.is_empty() {
        return None;
    }
    Some(ProcInfo {
        lstart: lstart_tokens.join(" "),
        comm,
    })
}

fn is_sleep_comm(comm: &str) -> bool {
    Path::new(comm).file_name().and_then(|name| name.to_str()) == Some("sleep")
}

fn ps_lstart(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let norm = normalize_ws(&String::from_utf8_lossy(&out.stdout));
    (!norm.is_empty()).then_some(norm)
}

fn ps_lstart_and_comm(pid: u32) -> Option<ProcInfo> {
    let out = Command::new("ps")
        .args(["-o", "lstart=,comm=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_lstart_and_comm(&String::from_utf8_lossy(&out.stdout))
}

fn pid_alive_with_lstart(pid: u32, recorded_lstart: &str) -> bool {
    let expected = normalize_ws(recorded_lstart);
    ps_lstart(pid).is_some_and(|current| normalize_ws(&current) == expected)
}

fn kill_sigkill(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-KILL", "--", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Kills `pid` only if it is still running with the exact normalized `lstart`
/// (and, when `require_sleep` is set, its `comm` basename is `sleep`).
fn safe_kill_if_matches(pid: u32, expected_lstart: &str, require_sleep: bool) {
    let expected = normalize_ws(expected_lstart);
    let matches = if require_sleep {
        ps_lstart_and_comm(pid)
            .is_some_and(|info| normalize_ws(&info.lstart) == expected && is_sleep_comm(&info.comm))
    } else {
        ps_lstart(pid).is_some_and(|current| normalize_ws(&current) == expected)
    };
    if matches {
        let _ = kill_sigkill(pid);
    }
}

#[derive(Debug, Clone)]
struct TrackedPid {
    pid: u32,
    lstart: String,
    require_sleep: bool,
    confirmed_dead: bool,
}

/// Ensures no background `sleep` spawned by the test survives even on panic,
/// while refusing to kill recycled PIDs whose `lstart` (or `comm` for gate
/// pids) does not match.
struct PidGuard {
    bg_agent_file: PathBuf,
    entries: Vec<TrackedPid>,
}

impl PidGuard {
    fn new(bg_agent_file: PathBuf) -> Self {
        Self {
            bg_agent_file,
            entries: Vec::new(),
        }
    }

    fn record_live(&mut self, pid: u32, lstart: String, require_sleep: bool) {
        self.entries.push(TrackedPid {
            pid,
            lstart: normalize_ws(&lstart),
            require_sleep,
            confirmed_dead: false,
        });
    }

    fn mark_dead(&mut self, pid: u32, lstart: &str) {
        let norm = normalize_ws(lstart);
        for entry in &mut self.entries {
            if entry.pid == pid && entry.lstart == norm {
                entry.confirmed_dead = true;
            }
        }
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        for entry in &self.entries {
            if !entry.confirmed_dead {
                safe_kill_if_matches(entry.pid, &entry.lstart, entry.require_sleep);
            }
        }
        // Catch any agent-spawned background pid written before an early panic.
        if let Ok(content) = fs::read_to_string(&self.bg_agent_file) {
            for line in content.lines() {
                if let Some((pid, lstart)) = parse_pid_lstart_line(line) {
                    let already_dead = self
                        .entries
                        .iter()
                        .any(|e| e.pid == pid && e.lstart == lstart && e.confirmed_dead);
                    if !already_dead {
                        safe_kill_if_matches(pid, &lstart, false);
                    }
                }
            }
        }
    }
}

fn parse_pid_lstart_line(line: &str) -> Option<(u32, String)> {
    let mut parts = line.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let lstart = parts.collect::<Vec<_>>().join(" ");
    if lstart.is_empty() {
        return None;
    }
    Some((pid, lstart))
}

fn parse_pid_line(line: &str, has_pgid: bool) -> Option<(u32, String)> {
    let mut parts = line.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    if has_pgid {
        let _pgid: u32 = parts.next()?.parse().ok()?;
    }
    let lstart = parts.collect::<Vec<_>>().join(" ");
    (!lstart.is_empty()).then_some((pid, lstart))
}

fn assert_recorded_pids_dead(path: &Path, label: &str, cycle: usize, has_pgid: bool) {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("cycle {cycle}: read {label}: {e}"));
    let entries: Vec<(u32, String)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            parse_pid_line(l, has_pgid)
                .unwrap_or_else(|| panic!("cycle {cycle}: malformed {label} line: {l:?}"))
        })
        .collect();
    assert!(
        entries.len() >= cycle,
        "cycle {cycle}: expected at least {cycle} {label} entries, got {}",
        entries.len()
    );
    for (pid, lstart) in &entries {
        assert!(
            !pid_alive_with_lstart(*pid, lstart),
            "cycle {cycle}: {label} pid {pid} (lstart={lstart:?}) is still alive"
        );
    }
}

fn configure(fx: &Fixture, script: &str, test: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    let config = format!(
        "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", {}, \"{{prompt}}\"]\nrun_timeout_secs = 60\n",
        json!(test),
        json!(script)
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

fn assert_single_main_worktree(repo: &Path) {
    let raw = git(repo, &["worktree", "list", "--porcelain"]);
    let worktrees: Vec<&str> = raw
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .collect();
    assert_eq!(
        worktrees.len(),
        1,
        "expected only main worktree, got:\n{raw}"
    );
    assert_eq!(
        fs::canonicalize(worktrees[0]).unwrap(),
        fs::canonicalize(repo).unwrap(),
        "sole worktree should be the main repo"
    );
}

fn assert_worktrees_dir_empty(worktrees_root: &Path, repo_id: &str) {
    let dir = worktrees_root.join(repo_id);
    if dir.exists() {
        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(
            entries.is_empty(),
            "expected {} to be empty, found {entries:?}",
            dir.display()
        );
    }
}

fn walk_files(dir: &Path, visit: &mut impl FnMut(&Path, &fs::Metadata)) {
    if !dir.exists() {
        return;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_files(&path, visit);
        } else if meta.is_file() {
            visit(&path, &meta);
        }
    }
}

fn assert_all_locks_free(data_dir: &Path, repo_id: &str) -> Vec<PathBuf> {
    let mut locks = Vec::new();
    walk_files(data_dir, &mut |path, _| {
        if path.extension().and_then(|e| e.to_str()) == Some("lock") {
            locks.push(path.to_path_buf());
        }
    });
    locks.sort();

    let run_lock = data_dir.join("locks").join(format!("{repo_id}.run.lock"));
    let heavy_lock = data_dir.join("throttle").join("heavy.0.lock");
    assert!(
        locks.contains(&run_lock),
        "expected run lock {} in {locks:?}",
        run_lock.display()
    );
    assert!(
        locks.contains(&heavy_lock),
        "expected heavy slot lock {} in {locks:?}",
        heavy_lock.display()
    );

    for path in &locks {
        let file = File::open(path)
            .unwrap_or_else(|e| panic!("failed to open lock {}: {e}", path.display()));
        file.try_lock()
            .unwrap_or_else(|e| panic!("lock still held after cycle: {} ({e})", path.display()));
        file.unlock()
            .unwrap_or_else(|e| panic!("failed to unlock {}: {e}", path.display()));
    }
    locks
}

fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0_u64;
    walk_files(dir, &mut |_, meta| {
        total = total.saturating_add(meta.len());
    });
    total
}

fn inspect_new_gate_pids(
    bg_gate_pids_file: &Path,
    seen_gate_pids: &mut usize,
    cycle: usize,
    guard: &mut PidGuard,
    gate_leaks: &mut Vec<GateLeak>,
) -> usize {
    let Ok(gate_lines) = fs::read_to_string(bg_gate_pids_file) else {
        return 0;
    };
    let all_gate_pids: Vec<u32> = gate_lines
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.trim()
                .parse::<u32>()
                .unwrap_or_else(|e| panic!("cycle {cycle}: invalid gate pid {l:?}: {e}"))
        })
        .collect();
    let added = all_gate_pids.len().saturating_sub(*seen_gate_pids);
    for &pid in &all_gate_pids[*seen_gate_pids..] {
        if let Some(info) = ps_lstart_and_comm(pid)
            && is_sleep_comm(&info.comm)
        {
            guard.record_live(pid, info.lstart.clone(), true);
            gate_leaks.push(GateLeak {
                cycle,
                pid,
                lstart: info.lstart.clone(),
                comm: info.comm.clone(),
            });
            safe_kill_if_matches(pid, &info.lstart, true);
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline && pid_alive_with_lstart(pid, &info.lstart) {
                thread::sleep(Duration::from_millis(10));
            }
            if !pid_alive_with_lstart(pid, &info.lstart) {
                guard.mark_dead(pid, &info.lstart);
            }
        }
    }
    *seen_gate_pids = all_gate_pids.len();
    added
}

#[derive(Debug)]
struct GateLeak {
    cycle: usize,
    pid: u32,
    lstart: String,
    comm: String,
}

#[derive(Debug)]
struct SoakReport {
    s1: u64,
    s5: u64,
    signed_delta: i128,
    growth_per_cycle: u64,
    elapsed: Duration,
    lock_files_last_cycle: Vec<PathBuf>,
    gate_leaks: Vec<GateLeak>,
}

fn run_soak_cycles(cycles: usize) -> SoakReport {
    let started = Instant::now();
    let fx = Fixture::new();
    let repo_id = aaf_fp::repo_id(&fx.repo);
    let pids_dir = fx.root().join("pids");
    fs::create_dir_all(&pids_dir).unwrap();
    let agent_pids_file = pids_dir.join("agent-pids");
    let bg_agent_pids_file = pids_dir.join("bg-agent-pids");
    let bg_gate_pids_file = pids_dir.join("bg-gate-pids");

    let mut guard = PidGuard::new(bg_agent_pids_file.clone());

    let agent_script = format!(
        "echo \"$$ $(ps -o pgid= -p $$ | tr -d ' ') $(ps -o lstart= -p $$ | tr -s ' ')\" >> {agent_pids}; \
         n=0; for f in cycle-*.txt; do [ -e \"$f\" ] && n=$((n + 1)); done; \
         next=$((n + 1)); echo \"cycle $next\" > \"cycle-$next.txt\"; \
         sleep 30 </dev/null >/dev/null 2>&1 & bg=$!; \
         echo \"$bg $(ps -o lstart= -p \"$bg\" | tr -s ' ')\" >> {bg_agent_pids}",
        agent_pids = sh_single_quote(&agent_pids_file.display().to_string()),
        bg_agent_pids = sh_single_quote(&bg_agent_pids_file.display().to_string()),
    );

    // Gate runs inside macOS Seatbelt where setuid `/bin/ps` is blocked;
    // only record `$!` here and inspect via `ps -o lstart=,comm=` outside.
    let gate_cmd = format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >> {bg_gate_pids}; \
         test -n \"$(ls cycle-*.txt)\"",
        bg_gate_pids = sh_single_quote(&bg_gate_pids_file.display().to_string()),
    );

    configure(&fx, &agent_script, &gate_cmd);

    let selftest = std::env::var_os("AAF_SOAK_SELFTEST").is_some_and(|v| !v.is_empty() && v != "0");

    let mut s1 = 0_u64;
    let mut s5 = 0_u64;
    let mut lock_files_last_cycle = Vec::new();
    let mut seen_gate_pids = 0_usize;
    let mut gate_leaks = Vec::new();

    for cycle in 1..=cycles {
        let id = fx.add(&format!("cycle {cycle}"));
        let out = fx.aaf(&["run", "--unsandboxed", "--auto-merge"]);

        // 6 (early capture). Inspect newly recorded gate background pids outside
        // Seatbelt right after `aaf run` returns so `PidGuard` holds `lstart`
        // and cleans up even if an earlier invariant or selftest panics.
        let added_gate_pids = inspect_new_gate_pids(
            &bg_gate_pids_file,
            &mut seen_gate_pids,
            cycle,
            &mut guard,
            &mut gate_leaks,
        );

        // 1. Task succeeded and reached `done`.
        assert_eq!(out.code, 0, "cycle {cycle}: {}", out.stderr);
        assert_eq!(
            out.json["done"],
            json!([id]),
            "cycle {cycle}: {}",
            out.stderr
        );
        assert_eq!(fx.state(&id), "done", "cycle {cycle}");

        // 2. Clean git state, no `aaf/*` branches, single worktree, no `refs/aaf/`,
        //    and `cycle-1..cycle.txt` landed on `main`.
        assert_eq!(
            git(&fx.repo, &["status", "--porcelain"]),
            "",
            "cycle {cycle}: dirty working tree"
        );
        assert_eq!(
            git(&fx.repo, &["branch", "--list", "aaf/*"]),
            "",
            "cycle {cycle}: leftover aaf/* branch"
        );
        assert_single_main_worktree(&fx.repo);
        assert_eq!(
            git(&fx.repo, &["for-each-ref", "refs/aaf/"]),
            "",
            "cycle {cycle}: leftover refs/aaf/ ref"
        );
        for k in 1..=cycle {
            let expected_file = fx.repo.join(format!("cycle-{k}.txt"));
            assert!(
                expected_file.is_file(),
                "cycle {cycle}: missing {} on main",
                expected_file.display()
            );
        }

        // 3. Worktree directory for this repo has no leftover entries.
        assert_worktrees_dir_empty(&fx.worktrees, &repo_id);

        // Optional intentional-RED self-test: hold an extra lock to prove
        // `assert_all_locks_free` catches a leaked flock.
        let _selftest_lock = if selftest && cycle == 1 {
            let extra = fx.data.join("throttle").join("extra.lock");
            fs::create_dir_all(extra.parent().unwrap()).unwrap();
            let file = File::create(&extra).unwrap();
            file.try_lock().unwrap();
            Some(file)
        } else {
            None
        };

        // 4. Every `*.lock` under `<data>` can be acquired via `try_lock`.
        lock_files_last_cycle = assert_all_locks_free(&fx.data, &repo_id);

        // 5. All agent pids and agent-spawned background pids are dead.
        assert_recorded_pids_dead(&agent_pids_file, "agent-pids", cycle, true);
        assert_recorded_pids_dead(&bg_agent_pids_file, "bg-agent-pids", cycle, false);

        // 6. Confirm `test_scoped` ran and appended a gate background pid.
        assert!(
            added_gate_pids >= 1,
            "cycle {cycle}: expected a new gate pid in bg-gate-pids (total={seen_gate_pids})"
        );

        let size = dir_size_bytes(&fx.data);
        if cycle == 1 {
            s1 = size;
        }
        if cycle == cycles {
            s5 = size;
        }
    }

    let elapsed = started.elapsed();
    let signed_delta = i128::from(s5) - i128::from(s1);
    let denom = u64::try_from(cycles.saturating_sub(1).max(1)).unwrap();
    let growth_per_cycle = s5.saturating_sub(s1) / denom;

    SoakReport {
        s1,
        s5,
        signed_delta,
        growth_per_cycle,
        elapsed,
        lock_files_last_cycle,
        gate_leaks,
    }
}

#[test]
fn five_auto_merge_cycles_leave_no_leftovers() {
    let report = run_soak_cycles(CYCLES);

    eprintln!(
        "soak_auto_merge: s1={}B s5={}B signed_delta={}B growth_per_cycle={}B \
         elapsed={:.3}s locks={} gate_leaks={}",
        report.s1,
        report.s5,
        report.signed_delta,
        report.growth_per_cycle,
        report.elapsed.as_secs_f64(),
        report.lock_files_last_cycle.len(),
        report.gate_leaks.len(),
    );
    for leak in &report.gate_leaks {
        eprintln!(
            "soak_auto_merge: gate_leak cycle={} pid={} lstart={:?} comm={:?}",
            leak.cycle, leak.pid, leak.lstart, leak.comm
        );
    }

    assert!(
        report.growth_per_cycle <= MAX_GROWTH_PER_CYCLE_BYTES,
        "data dir grew by {} B/cycle (s1={}, s5={}, signed_delta={}), limit is {} B/cycle",
        report.growth_per_cycle,
        report.s1,
        report.s5,
        report.signed_delta,
        MAX_GROWTH_PER_CYCLE_BYTES
    );
    assert!(
        report.elapsed <= MAX_DURATION,
        "soak test took {:.3}s, limit is {}s",
        report.elapsed.as_secs_f64(),
        MAX_DURATION.as_secs()
    );
    assert!(
        report.gate_leaks.is_empty(),
        "gate background processes outlived `aaf run --auto-merge`: {:?}",
        report.gate_leaks
    );
}

/// Regression check: `test_scoped` runs in its own process group
/// (`gate/proc.rs`), and normal exit must kill background jobs left in that
/// group so a detached `sleep 30` child does not outlive `aaf run`.
#[test]
fn gate_background_processes_do_not_outlive_auto_merge() {
    let report = run_soak_cycles(1);
    assert!(
        report.gate_leaks.is_empty(),
        "gate background sleep process outlived `aaf run --auto-merge`: {:?}",
        report.gate_leaks
    );
}
