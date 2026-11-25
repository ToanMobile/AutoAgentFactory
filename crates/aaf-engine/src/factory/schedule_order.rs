//! Pure scheduling helpers: scope-path normalization/overlap, priority +
//! critical-path ordering, and upstream `Blocked` cascade detection.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path};

use aaf_domain::task::{Task, TaskState};

/// Normalizes a relative repository scope path (e.g. `src/api`).
///
/// Rejects empty paths, control/NUL characters, glob metacharacters
/// (`*`, `?`, `[`, `]`), absolute/drive-prefixed paths, and `..` traversal.
/// Converts `\` to `/` and strips leading `./` and trailing `/`.
pub fn normalize_scope(path: &str) -> Result<String, String> {
    if path.contains('\0') || path.chars().any(char::is_control) {
        return Err(format!("scope path {path:?} contains control characters"));
    }
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("scope path must not be empty".to_string());
    }
    if trimmed.chars().any(|c| matches!(c, '*' | '?' | '[' | ']')) {
        return Err(format!(
            "scope path {path:?} must not contain glob characters"
        ));
    }
    let replaced = trimmed.replace('\\', "/");
    if replaced.starts_with('/') || has_windows_drive_prefix(&replaced) {
        return Err(format!(
            "scope path {path:?} must be relative, not absolute"
        ));
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in replaced.split('/') {
        if seg == ".." {
            return Err(format!("scope path {path:?} must not contain '..'"));
        }
        if !seg.is_empty() && seg != "." {
            parts.push(seg);
        }
    }
    if parts.is_empty() {
        return Err(format!("scope path {path:?} must name a file or directory"));
    }

    let clean = parts.join("/");
    let bad_comp = |c: Component<'_>| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    };
    if Path::new(&clean).components().any(bad_comp) {
        return Err(format!(
            "scope path {path:?} must stay relative within repo"
        ));
    }
    Ok(clean)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn canon_for_overlap(path: &str) -> String {
    match normalize_scope(path) {
        Ok(norm) => norm.to_ascii_lowercase(),
        Err(_) => path
            .trim()
            .replace('\\', "/")
            .trim_matches('/')
            .to_ascii_lowercase(),
    }
}

fn single_path_overlaps(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

/// Returns `true` iff any path in `a` equals or is a directory-segment prefix
/// of any path in `b` (or vice versa), compared ASCII-case-insensitively (APFS).
///
/// Empty scope lists represent unscoped tasks and never overlap with anything,
/// so unscoped tasks do not block scoped tasks.
pub fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let norm_a: Vec<String> = a.iter().map(|p| canon_for_overlap(p)).collect();
    let norm_b: Vec<String> = b.iter().map(|p| canon_for_overlap(p)).collect();
    for pa in &norm_a {
        for pb in &norm_b {
            if single_path_overlaps(pa, pb) {
                return true;
            }
        }
    }
    false
}

/// Sorts ready tasks by `(priority, Reverse(downstream_depth), created_at, id)`.
///
/// `deps` contains `(task_id, blocked_by)` edges (`task_id` waits on `blocked_by`).
/// `downstream_depth` is the longest chain of downstream tasks waiting on a task,
/// computed safely with a cycle guard. Priority (`P0..P3`) always takes precedence.
pub fn sort_ready_by_priority_and_critical_path(tasks: &mut [Task], deps: &[(String, String)]) {
    let depths: HashMap<String, usize> = {
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for (task_id, blocked_by) in deps {
            dependents
                .entry(blocked_by.as_str())
                .or_default()
                .push(task_id.as_str());
        }
        let mut memo: HashMap<&str, usize> = HashMap::new();
        let mut visiting: HashSet<&str> = HashSet::new();
        tasks
            .iter()
            .map(|task| {
                let (depth, _) = downstream_depth(&task.id, &dependents, &mut memo, &mut visiting);
                (task.id.clone(), depth)
            })
            .collect()
    };

    tasks.sort_by(|a, b| {
        let da = depths.get(&a.id).copied().unwrap_or(0);
        let db = depths.get(&b.id).copied().unwrap_or(0);
        (a.priority, Reverse(da), a.created_at, &a.id).cmp(&(
            b.priority,
            Reverse(db),
            b.created_at,
            &b.id,
        ))
    });
}

fn downstream_depth<'a>(
    node: &'a str,
    dependents: &HashMap<&'a str, Vec<&'a str>>,
    memo: &mut HashMap<&'a str, usize>,
    visiting: &mut HashSet<&'a str>,
) -> (usize, bool) {
    if let Some(&depth) = memo.get(node) {
        return (depth, false);
    }
    if !visiting.insert(node) {
        return (0, true);
    }
    let mut best = 0usize;
    let mut hit_cycle = false;
    if let Some(children) = dependents.get(node) {
        for &child in children {
            if visiting.contains(child) {
                hit_cycle = true;
                continue;
            }
            let (child_depth, child_cycle) = downstream_depth(child, dependents, memo, visiting);
            hit_cycle |= child_cycle;
            let candidate = 1usize.saturating_add(child_depth);
            if candidate > best {
                best = candidate;
            }
        }
    }
    visiting.remove(node);
    if !hit_cycle {
        memo.insert(node, best);
    }
    (best, hit_cycle)
}

/// For each task in `ready` that has a direct or transitive upstream blocker
/// currently in [`TaskState::Blocked`] or [`TaskState::Cancelled`], returns
/// `(task.id, blocker.id)` sorted deterministically.
///
/// Dependencies whose state is already [`TaskState::Done`] are satisfied and
/// are not traversed further.
pub fn blocked_upstream_tasks(
    ready: &[Task],
    all_tasks: &[Task],
    deps: &[(String, String)],
) -> Vec<(String, String)> {
    let mut state_by_id: HashMap<&str, TaskState> =
        all_tasks.iter().map(|t| (t.id.as_str(), t.state)).collect();
    for task in ready {
        state_by_id.entry(task.id.as_str()).or_insert(task.state);
    }

    let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task_id, blocked_by) in deps {
        blockers
            .entry(task_id.as_str())
            .or_default()
            .push(blocked_by.as_str());
    }
    for list in blockers.values_mut() {
        list.sort_unstable();
        list.dedup();
    }

    let mut result: Vec<(String, String)> = Vec::new();
    for task in ready {
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut visited: HashSet<&str> = HashSet::new();
        visited.insert(task.id.as_str());
        if let Some(direct) = blockers.get(task.id.as_str()) {
            for &dep in direct {
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }
        while let Some(cur) = queue.pop_front() {
            match state_by_id.get(cur).copied() {
                Some(TaskState::Blocked | TaskState::Cancelled) => {
                    result.push((task.id.clone(), cur.to_string()));
                    break;
                }
                Some(TaskState::Done) => {}
                _ => {
                    if let Some(next_deps) = blockers.get(cur) {
                        for &dep in next_deps {
                            if visited.insert(dep) {
                                queue.push_back(dep);
                            }
                        }
                    }
                }
            }
        }
    }

    result.sort();
    result.dedup();
    result
}

/// Maximum consecutive transient retries per task in `Mode::Auto` before halting.
pub const MAX_TRANSIENT_RETRIES: u32 = 3;
/// Upper bound on transient backoff delay in seconds.
pub const MAX_TRANSIENT_BACKOFF_SECS: u64 = 60;

/// Exponential backoff (`5 * 2^(attempt - 1)`) plus `0..=5s` jitter, clamped to `1..=60s`.
pub fn transient_backoff_secs(attempt: u32) -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let base = 5u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(6));
    base.saturating_add(RandomState::new().hash_one(attempt) % 6)
        .clamp(1, MAX_TRANSIENT_BACKOFF_SECS)
}

/// Counts `Review + Running + Gating` tasks and collects non-empty scopes of active (`Running | Gating`) tasks.
pub fn open_review_and_scopes(tasks: &[Task]) -> (usize, Vec<Vec<String>>) {
    let (mut open, mut scopes) = (0usize, Vec::new());
    for task in tasks {
        match task.state {
            TaskState::Review => open += 1,
            TaskState::Running | TaskState::Gating => {
                open += 1;
                if !task.scope_paths.is_empty() {
                    scopes.push(task.scope_paths.clone());
                }
            }
            _ => {}
        }
    }
    (open, scopes)
}

/// Indexes `(task_id, blocked_by)` pairs into a `task_id -> blockers` adjacency map.
pub fn deps_by_task(all_deps: &[(String, String)]) -> HashMap<&str, Vec<&str>> {
    let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task_id, dep_id) in all_deps {
        map.entry(task_id.as_str())
            .or_default()
            .push(dep_id.as_str());
    }
    map
}

#[cfg(test)]
mod tests {
    use aaf_domain::task::{ImmutableSpec, Priority};

    use super::*;

    fn mk(id: &str, state: TaskState) -> Task {
        let mut task = Task::new_root(
            id,
            "/repo",
            id,
            ImmutableSpec {
                description: "d".to_string(),
                acceptance: vec!["a".to_string()],
            },
            Priority::P2,
            1,
        );
        task.state = state;
        task
    }

    #[test]
    fn blocked_upstream_tasks_detects_cancelled_direct_and_transitive() {
        let cancelled = mk("t-cancel", TaskState::Cancelled);
        let mid = mk("t-mid", TaskState::Ready);
        let leaf = mk("t-leaf", TaskState::Ready);
        let done = mk("t-done", TaskState::Done);
        let after_done = mk("t-after-done", TaskState::Ready);

        let all = vec![
            cancelled,
            mid.clone(),
            leaf.clone(),
            done,
            after_done.clone(),
        ];
        let ready = vec![mid, leaf, after_done];
        let deps = vec![
            ("t-mid".to_string(), "t-cancel".to_string()),
            ("t-leaf".to_string(), "t-mid".to_string()),
            ("t-done".to_string(), "t-cancel".to_string()),
            ("t-after-done".to_string(), "t-done".to_string()),
        ];

        assert_eq!(
            blocked_upstream_tasks(&ready, &all, &deps),
            vec![
                ("t-leaf".to_string(), "t-cancel".to_string()),
                ("t-mid".to_string(), "t-cancel".to_string()),
            ]
        );
    }
}
