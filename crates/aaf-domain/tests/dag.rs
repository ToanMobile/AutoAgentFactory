#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Dependency graph: an edge `(task, blocked_by)` orders `blocked_by` first.

use aaf_domain::dag::{topo_order, would_create_cycle};

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| (*n).to_string()).collect()
}

fn edges(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
        .collect()
}

/// Every node exactly once and every blocker before the task it blocks.
fn assert_valid_order(order: &[String], expect_nodes: &[&str], edges: &[(String, String)]) {
    let mut sorted: Vec<&str> = order.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut want = expect_nodes.to_vec();
    want.sort_unstable();
    assert_eq!(sorted, want, "node set of {order:?}");
    let pos = |name: &str| order.iter().position(|n| n == name).unwrap();
    for (task, blocked_by) in edges {
        assert!(
            pos(blocked_by) < pos(task),
            "{blocked_by} must precede {task} in {order:?}"
        );
    }
}

#[test]
fn empty_graph_has_an_empty_order() {
    assert_eq!(topo_order(&[], &[]), Some(Vec::new()));
    assert!(!would_create_cycle(&[], "a", "b"));
}

#[test]
fn diamond_orders_the_shared_blocker_first_and_the_join_last() {
    // d is blocked by b and c, which are both blocked by a.
    let e = edges(&[("b", "a"), ("c", "a"), ("d", "b"), ("d", "c")]);
    let order = topo_order(&names(&["d", "c", "b", "a"]), &e).unwrap();
    assert_valid_order(&order, &["a", "b", "c", "d"], &e);
    assert_eq!(order.first().map(String::as_str), Some("a"));
    assert_eq!(order.last().map(String::as_str), Some("d"));
}

#[test]
fn diamond_cycle_checks() {
    let e = edges(&[("b", "a"), ("c", "a"), ("d", "b"), ("d", "c")]);
    // Closing the diamond backwards is a cycle through either arm.
    assert!(would_create_cycle(&e, "a", "d"));
    assert!(would_create_cycle(&e, "a", "b"));
    // A shortcut along the existing direction, or a cross edge, is not.
    assert!(!would_create_cycle(&e, "d", "a"));
    assert!(!would_create_cycle(&e, "b", "c"));
    assert!(!would_create_cycle(&e, "c", "b"));
}

#[test]
fn long_chain_is_ordered_end_to_end_without_recursion_limits() {
    let n = 20_000;
    let nodes: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
    // n(i+1) is blocked by n(i), listed in reverse to defeat input order.
    let e: Vec<(String, String)> = (0..n - 1)
        .rev()
        .map(|i| (format!("n{}", i + 1), format!("n{i}")))
        .collect();
    let order = topo_order(&nodes, &e).unwrap();
    assert_eq!(order, nodes);

    let last = format!("n{}", n - 1);
    assert!(would_create_cycle(&e, "n0", &last));
    assert!(!would_create_cycle(&e, &last, "n0"));
}

#[test]
fn disconnected_nodes_and_components_are_all_kept() {
    let e = edges(&[("b", "a"), ("y", "x")]);
    let order = topo_order(&names(&["solo", "a", "b", "x", "y", "other"]), &e).unwrap();
    assert_valid_order(&order, &["a", "b", "other", "solo", "x", "y"], &e);
    // Unrelated components never form a cycle with each other.
    assert!(!would_create_cycle(&e, "a", "y"));
    assert!(!would_create_cycle(&e, "solo", "other"));
}

#[test]
fn nodes_without_edges_keep_their_input_order() {
    let order = topo_order(&names(&["c", "a", "b"]), &[]).unwrap();
    assert_eq!(order, names(&["c", "a", "b"]));
}

#[test]
fn nodes_named_only_in_edges_are_included() {
    let e = edges(&[("b", "a")]);
    let order = topo_order(&names(&["z"]), &e).unwrap();
    assert_valid_order(&order, &["a", "b", "z"], &e);
}

#[test]
fn duplicate_nodes_and_edges_do_not_duplicate_or_stall_the_order() {
    let e = edges(&[("b", "a"), ("b", "a"), ("c", "b"), ("b", "a")]);
    let order = topo_order(&names(&["a", "a", "b", "c", "b"]), &e).unwrap();
    assert_eq!(order, names(&["a", "b", "c"]));
    assert!(would_create_cycle(&e, "a", "c"));
    assert!(!would_create_cycle(&e, "c", "a"));
}

#[test]
fn any_cycle_yields_none() {
    let nodes = names(&["a", "b", "c", "free"]);
    assert_eq!(topo_order(&nodes, &edges(&[("a", "a")])), None);
    assert_eq!(
        topo_order(&nodes, &edges(&[("a", "b"), ("b", "c"), ("c", "a")])),
        None
    );
    // A cycle hidden behind an acyclic prefix still fails the whole order.
    assert_eq!(
        topo_order(&nodes, &edges(&[("b", "free"), ("b", "c"), ("c", "b")])),
        None
    );
}

#[test]
fn cycle_check_terminates_on_a_graph_that_already_has_a_cycle() {
    let e = edges(&[("a", "b"), ("b", "a"), ("c", "a")]);
    assert!(!would_create_cycle(&e, "d", "a"));
    assert!(would_create_cycle(&e, "a", "c"));
}

#[test]
fn an_edge_accepted_by_the_cycle_check_keeps_the_graph_orderable() {
    let nodes = names(&["a", "b", "c", "d"]);
    let mut e = edges(&[("b", "a"), ("c", "b")]);
    for (task, blocked_by) in [("d", "c"), ("a", "d"), ("d", "a"), ("c", "a"), ("a", "c")] {
        if would_create_cycle(&e, task, blocked_by) {
            let mut with = e.clone();
            with.push((task.to_string(), blocked_by.to_string()));
            assert_eq!(topo_order(&nodes, &with), None, "{task} -> {blocked_by}");
        } else {
            e.push((task.to_string(), blocked_by.to_string()));
            assert!(topo_order(&nodes, &e).is_some(), "{task} -> {blocked_by}");
        }
    }
}
