//! Dependency graph checks for `task_deps`.
//!
//! An edge `(task, blocked_by)` means `task` cannot start until `blocked_by`
//! is done.

use std::collections::{HashMap, HashSet, VecDeque};

/// Whether adding the edge `task -> blocked_by` would close a cycle.
pub fn would_create_cycle(edges: &[(String, String)], task: &str, blocked_by: &str) -> bool {
    if task == blocked_by {
        return true;
    }
    let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in edges {
        blockers.entry(from.as_str()).or_default().push(to.as_str());
    }
    // A cycle appears iff `task` is already reachable from `blocked_by`.
    let mut stack = vec![blocked_by];
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == task {
            return true;
        }
        if !seen.insert(node) {
            continue;
        }
        if let Some(next) = blockers.get(node) {
            stack.extend(next.iter().copied());
        }
    }
    false
}

/// Kahn's algorithm: blockers come before the tasks they block.
/// Returns `None` when the graph has a cycle.
pub fn topo_order(nodes: &[String], edges: &[(String, String)]) -> Option<Vec<String>> {
    let mut all: Vec<&str> = Vec::new();
    let mut known: HashSet<&str> = HashSet::new();
    let endpoints = edges.iter().flat_map(|(a, b)| [a.as_str(), b.as_str()]);
    for node in nodes.iter().map(String::as_str).chain(endpoints) {
        if known.insert(node) {
            all.push(node);
        }
    }
    let mut pending: HashMap<&str, usize> = all.iter().map(|n| (*n, 0)).collect();
    let mut blocks: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task, blocked_by) in edges {
        *pending.entry(task.as_str()).or_default() += 1;
        blocks
            .entry(blocked_by.as_str())
            .or_default()
            .push(task.as_str());
    }
    let mut queue: VecDeque<&str> = all
        .iter()
        .copied()
        .filter(|n| pending.get(n).copied() == Some(0))
        .collect();
    let mut order = Vec::with_capacity(all.len());
    while let Some(node) = queue.pop_front() {
        order.push(node.to_string());
        for task in blocks.get(node).into_iter().flatten() {
            if let Some(count) = pending.get_mut(task) {
                *count -= 1;
                if *count == 0 {
                    queue.push_back(task);
                }
            }
        }
    }
    (order.len() == all.len()).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    #[test]
    fn self_edge_is_a_cycle() {
        assert!(would_create_cycle(&[], "a", "a"));
    }

    #[test]
    fn back_edge_is_a_cycle_and_forward_edge_is_not() {
        let existing = edges(&[("a", "b"), ("b", "c")]);
        assert!(would_create_cycle(&existing, "c", "a"));
        assert!(!would_create_cycle(&existing, "a", "c"));
        assert!(!would_create_cycle(&existing, "d", "a"));
    }

    #[test]
    fn topo_order_puts_blockers_first_and_rejects_cycles() {
        let nodes = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let order = topo_order(&nodes, &edges(&[("a", "b"), ("b", "c")])).unwrap();
        assert_eq!(order, vec!["c", "b", "a"]);
        assert_eq!(topo_order(&nodes, &edges(&[("a", "b"), ("b", "a")])), None);
    }
}
