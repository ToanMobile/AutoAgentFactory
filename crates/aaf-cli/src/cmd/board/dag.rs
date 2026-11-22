//! `aaf board dep add|rm` and `aaf board graph`: DAG dependency management,
//! critical-path computation, and cascade-blocked visualization.

use std::collections::{HashMap, HashSet};

use aaf_domain::dag;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Task, TaskState};
use aaf_engine::factory::schedule_order::blocked_upstream_tasks;
use serde_json::{Value, json};

use super::port;
use super::store::{open_db, open_db_read_only, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct DepArgs {
    #[command(subcommand)]
    pub command: DepCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum DepCommand {
    /// Add a dependency edge (`task` cannot start until `blocked_by` is Done).
    Add(DepEdgeArgs),
    /// Remove an existing dependency edge.
    Rm(DepEdgeArgs),
}

#[derive(Debug, clap::Args)]
pub struct DepEdgeArgs {
    /// Task id whose dependency edge is modified.
    pub task: String,
    /// Task id that blocks `task`.
    #[arg(long = "blocked-by")]
    pub blocked_by: String,
}

#[derive(Debug, clap::Args)]
pub struct GraphArgs {
    /// Show tasks across all repositories, not only the current one.
    #[arg(long)]
    pub all: bool,
}

pub fn run_dep(args: &DepArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        DepCommand::Add(edge) => dep_add(&edge.task, &edge.blocked_by, ctx),
        DepCommand::Rm(edge) => dep_rm(&edge.task, &edge.blocked_by, ctx),
    }
}

pub fn dep_add(task_id: &str, blocked_by: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board dep add") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let task = db.get_task(task_id).map_err(port)?;
    let blocker = db.get_task(blocked_by).map_err(port)?;
    if task.repo_root != blocker.repo_root {
        return Err(CmdError::new(format!(
            "cannot add cross-repo dependency: {task_id} belongs to {} but {blocked_by} belongs to {}",
            task.repo_root, blocker.repo_root
        )));
    }
    let edges = db.all_deps().map_err(port)?;
    if dag::would_create_cycle(&edges, task_id, blocked_by) {
        return Err(CmdError::new(format!(
            "dependency {task_id} -> {blocked_by} would create a cycle"
        )));
    }
    db.add_dep(task_id, blocked_by).map_err(port)?;
    Ok(
        CmdOutput::ok("added", format!("{task_id} is blocked by {blocked_by}"))
            .field("task_id", json!(task_id))
            .field("blocked_by", json!(blocked_by)),
    )
}

pub fn dep_rm(task_id: &str, blocked_by: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board dep rm") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let removed = db.remove_dep(task_id, blocked_by).map_err(port)?;
    let text = if removed {
        format!("removed dependency {task_id} -> {blocked_by}")
    } else {
        format!("dependency {task_id} -> {blocked_by} was not present")
    };
    Ok(CmdOutput::ok("ok", text)
        .field("task_id", json!(task_id))
        .field("blocked_by", json!(blocked_by))
        .field("removed", json!(removed)))
}

pub fn graph(args: &GraphArgs, ctx: &Ctx) -> CmdResult {
    let repo = if args.all {
        None
    } else {
        Some(repo_root(ctx)?.to_string_lossy().into_owned())
    };
    let Some(db) = open_db_read_only(ctx)? else {
        return Ok(empty_graph_output());
    };
    let tasks = db.list_tasks(repo.as_deref(), None).map_err(port)?;
    if tasks.is_empty() {
        return Ok(empty_graph_output());
    }
    let id_set: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut edges: Vec<(String, String)> = db
        .all_deps()
        .map_err(port)?
        .into_iter()
        .filter(|(t, b)| id_set.contains(t.as_str()) && id_set.contains(b.as_str()))
        .collect();
    edges.sort();
    edges.dedup();

    let node_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let topo_order = dag::topo_order(&node_ids, &edges).unwrap_or_else(|| node_ids.clone());
    let critical_path = compute_critical_path(&topo_order, &edges);
    let crit_set: HashSet<&str> = critical_path.iter().map(String::as_str).collect();

    let cascade_candidates: Vec<Task> = tasks
        .iter()
        .filter(|t| !t.state.is_terminal() && t.state != TaskState::Blocked)
        .cloned()
        .collect();
    let blocked_upstream = blocked_upstream_tasks(&cascade_candidates, &tasks, &edges);
    let blocked_map: HashMap<&str, &str> = blocked_upstream
        .iter()
        .map(|(t, b)| (t.as_str(), b.as_str()))
        .collect();

    let task_by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut blockers_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for (t, b) in &edges {
        blockers_of.entry(t.as_str()).or_default().push(b.as_str());
    }

    let mut lines: Vec<String> = Vec::with_capacity(topo_order.len() + 2);
    let mut nodes_json: Vec<Value> = Vec::with_capacity(topo_order.len());
    for id in &topo_order {
        let Some(task) = task_by_id.get(id.as_str()) else {
            continue;
        };
        let deps = blockers_of.get(id.as_str()).cloned().unwrap_or_default();
        let is_crit = crit_set.contains(id.as_str());
        let cascade_by = blocked_map.get(id.as_str()).copied();

        let mut line = format!(
            "{}  {}  {:<9} {}",
            task.id,
            task.priority.as_str(),
            task.state.as_str(),
            task.title
        );
        if !deps.is_empty() {
            line.push_str(&format!(" <- {}", deps.join(", ")));
        }
        if is_crit {
            line.push_str(" [CRIT]");
        }
        if let Some(blocker) = cascade_by {
            line.push_str(&format!(" [CASCADE-BLOCKED] (by {blocker})"));
        }
        lines.push(line);

        nodes_json.push(json!({
            "id": task.id,
            "title": task.title,
            "state": task.state,
            "priority": task.priority,
            "scope_paths": task.scope_paths,
            "blocked_by": deps,
            "critical": is_crit,
            "cascade_blocked_by": cascade_by,
        }));
    }

    if !critical_path.is_empty() {
        lines.push(format!("critical path: {}", critical_path.join(" -> ")));
    }
    if !blocked_upstream.is_empty() {
        let pairs: Vec<String> = blocked_upstream
            .iter()
            .map(|(t, b)| format!("{t} (by {b})"))
            .collect();
        lines.push(format!("cascade-blocked: {}", pairs.join(", ")));
    }

    let edges_json: Vec<Value> = edges
        .iter()
        .map(|(t, b)| json!({ "task_id": t, "blocked_by": b }))
        .collect();
    let blocked_json: Vec<Value> = blocked_upstream
        .iter()
        .map(|(t, b)| json!({ "task_id": t, "blocked_by": b }))
        .collect();

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("nodes", Value::Array(nodes_json))
        .field("edges", Value::Array(edges_json))
        .field("topo_order", json!(topo_order))
        .field("critical_path", json!(critical_path))
        .field("blocked_upstream", Value::Array(blocked_json)))
}

fn empty_graph_output() -> CmdOutput {
    CmdOutput::ok("ok", "no tasks")
        .field("nodes", json!([]))
        .field("edges", json!([]))
        .field("topo_order", json!([]))
        .field("critical_path", json!([]))
        .field("blocked_upstream", json!([]))
}

/// Longest directed chain (`blocker -> ... -> dependent`) in the DAG.
/// Returns an empty path when there are no dependency edges.
fn compute_critical_path(topo: &[String], edges: &[(String, String)]) -> Vec<String> {
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
    let mut best_len: HashMap<&str, usize> = HashMap::with_capacity(topo.len());
    let mut pred: HashMap<&str, &str> = HashMap::new();
    let mut best_end: Option<&str> = None;
    let mut global_max = 0usize;

    for node in topo {
        let mut len = 1usize;
        let mut best_prev: Option<&str> = None;
        if let Some(deps) = blockers.get(node.as_str()) {
            for &dep in deps {
                let dep_len = best_len.get(dep).copied().unwrap_or(0);
                let cand = dep_len.saturating_add(1);
                if cand > len {
                    len = cand;
                    best_prev = Some(dep);
                }
            }
        }
        best_len.insert(node.as_str(), len);
        if let Some(prev) = best_prev {
            pred.insert(node.as_str(), prev);
        }
        if len > global_max {
            global_max = len;
            best_end = Some(node.as_str());
        }
    }

    if global_max < 2 {
        return Vec::new();
    }
    let mut path = Vec::with_capacity(global_max);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = best_end;
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
