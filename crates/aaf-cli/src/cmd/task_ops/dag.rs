//! DAG critical-path computation for task board views.

use std::collections::{HashMap, HashSet};

/// Longest directed chain (`blocker -> ... -> dependent`) in the DAG.
pub(crate) fn compute_critical_path(topo: &[String], edges: &[(String, String)]) -> Vec<String> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task, blocked_by) in edges {
        blockers
            .entry(task.as_str())
            .or_default()
            .push(blocked_by.as_str());
    }
    let (mut best_len, mut pred) = (
        HashMap::<&str, usize>::with_capacity(topo.len()),
        HashMap::new(),
    );
    let (mut best_end, mut global_max) = (None, 0usize);
    for node in topo {
        let (mut len, mut best_prev) = (1usize, None);
        if let Some(deps) = blockers.get(node.as_str()) {
            for &dep in deps {
                let cand = best_len.get(dep).copied().unwrap_or(0).saturating_add(1);
                if cand > len {
                    (len, best_prev) = (cand, Some(dep));
                }
            }
        }
        best_len.insert(node.as_str(), len);
        if let Some(prev) = best_prev {
            pred.insert(node.as_str(), prev);
        }
        if len > global_max {
            (global_max, best_end) = (len, Some(node.as_str()));
        }
    }
    if global_max < 2 {
        return Vec::new();
    }
    let (mut path, mut seen, mut cur) = (Vec::with_capacity(global_max), HashSet::new(), best_end);
    while let Some(node) = cur {
        if !seen.insert(node) {
            break;
        }
        path.push(node.to_string());
        cur = pred.get(node).copied();
    }
    path.reverse();
    path
}
