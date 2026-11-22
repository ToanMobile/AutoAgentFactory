//! `aaf board add|list|show|unblock|unfreeze|rework|split|dep|graph|approve-plan`
//! (docs/PLAN.md C.10): the task board in the SQLite store, used through the port traits.

mod approve_plan;
mod dag;
mod github;
mod rework;
pub(crate) mod split;
pub(crate) mod store;
mod trace;
mod undo_steer;

use std::collections::HashSet;

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{BudgetStore, Clock, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::schedule_order::normalize_scope;
use aaf_engine::factory::{is_frozen, unfreeze};
use serde_json::{Value, json};

use self::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const SHOWN_EVENTS: u32 = 20;
const MAX_TITLE_CHARS: usize = 200;

#[derive(Debug, clap::Args)]
pub struct BoardArgs {
    #[command(subcommand)]
    pub command: BoardCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum BoardCommand {
    /// Add a Ready task for the repository of the current directory.
    Add(AddArgs),
    /// List tasks, most urgent first.
    List(ListArgs),
    /// Show one task with its dependencies, runs and latest events.
    Show { id: String },
    /// Put a Blocked task back to Ready with a fresh budget (human action).
    Unblock { id: String },
    /// Clear the factory freeze and the consecutive-failure counter.
    Unfreeze,
    /// Send a task in `review` back for rework (to `gating`, or `ready` with --rerun).
    Rework(rework::ReworkArgs),
    /// Show chronological runs, events, tokens, wall-clock and estimated cost for a task.
    Trace(trace::TraceArgs),
    /// Import open GitHub issues from allowlisted logins with 72h dedup.
    ImportIssues(github::ImportIssuesArgs),
    /// Map board tasks to GitHub Projects v2 status columns (with issue label fallback).
    SyncGithub(github::SyncGithubArgs),
    /// Proactively decompose a Ready or Backlog root task into disjoint parallel subtasks.
    Split(split::SplitArgs),
    /// Add or remove task dependency edges (`add` / `rm`).
    Dep(dag::DepArgs),
    /// Show the task dependency DAG, critical path and cascade-blocked tasks.
    Graph(dag::GraphArgs),
    /// Approve a task's generated plan and move it back to Ready.
    ApprovePlan { id: String },
    /// Roll back a task's worktree to its latest pre-turn snapshot (human action).
    Undo { id: String },
    /// Append a human steering note to a task's context without altering its spec.
    Steer { id: String, note: String },
}

#[derive(Debug, clap::Args)]
pub struct AddArgs {
    #[arg(long)]
    pub title: String,
    /// What has to be done. Never edited after the task exists.
    #[arg(long)]
    pub spec: String,
    /// One acceptance criterion; repeat the flag for more.
    #[arg(long = "accept", required = true)]
    pub accept: Vec<String>,
    /// P0 (most urgent) to P3.
    #[arg(long, default_value = "P2")]
    pub priority: String,
    /// Id of a task that must be Done first; repeat the flag for more.
    #[arg(long = "blocked-by")]
    pub blocked_by: Vec<String>,
    /// Relative path prefix owned by this task; repeat the flag for more.
    #[arg(long = "scope")]
    pub scope: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// backlog, ready, running, gating, review, blocked, done or cancelled.
    #[arg(long)]
    pub state: Option<String>,
    /// Every repository, not only the current one.
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: &BoardArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        BoardCommand::Add(add_args) => add(add_args, ctx),
        BoardCommand::List(list_args) => list(list_args, ctx),
        BoardCommand::Show { id } => show(id, ctx),
        BoardCommand::Unblock { id } => unblock(id, ctx),
        BoardCommand::Unfreeze => unfreeze_factory(ctx),
        BoardCommand::Rework(rework_args) => rework::run(rework_args, ctx),
        BoardCommand::Trace(trace_args) => trace::run(trace_args, ctx),
        BoardCommand::ImportIssues(import_args) => github::run_import_issues(import_args, ctx),
        BoardCommand::SyncGithub(sync_args) => github::run_sync_github(sync_args, ctx),
        BoardCommand::Split(split_args) => split::run_split(split_args, ctx),
        BoardCommand::Dep(dep_args) => dag::run_dep(dep_args, ctx),
        BoardCommand::Graph(graph_args) => dag::graph(graph_args, ctx),
        BoardCommand::ApprovePlan { id } => approve_plan::run(ctx, id),
        BoardCommand::Undo { id } => undo_steer::undo(ctx, id),
        BoardCommand::Steer { id, note } => undo_steer::steer(ctx, id, note),
    }
}

fn port(error: PortError) -> CmdError {
    CmdError::new(error.to_string())
}

fn task_json(task: &Task) -> Result<Value, CmdError> {
    serde_json::to_value(task).map_err(|e| err("task", e))
}

fn task_line(task: &Task) -> String {
    format!(
        "{}  {}  {:<8}  {}",
        task.id,
        task.priority.as_str(),
        task.state.as_str(),
        task.title
    )
}

fn clean(label: &str, text: &str) -> Result<String, CmdError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CmdError::new(format!("{label} is empty")));
    }
    Ok(text.to_string())
}

/// `t-` plus 8 hex characters; unique in the store.
fn new_task_id(
    store: &dyn TaskStore,
    repo: &str,
    title: &str,
    now: i64,
) -> Result<String, CmdError> {
    for attempt in 0_u32..100 {
        let seed = format!("{repo}\n{title}\n{now}\n{}\n{attempt}", std::process::id());
        let id = format!("t-{}", &aaf_fp::sha256_hex(seed.as_bytes())[..8]);
        match store.get_task(&id) {
            Err(PortError::NotFound(_)) => return Ok(id),
            Ok(_) => {}
            Err(other) => return Err(port(other)),
        }
    }
    Err(CmdError::new("could not find a free task id"))
}

fn add(args: &AddArgs, ctx: &Ctx) -> CmdResult {
    let title = clean("--title", &args.title)?;
    if title.chars().count() > MAX_TITLE_CHARS || title.chars().any(char::is_control) {
        return Err(CmdError::new(format!(
            "--title must be one line of at most {MAX_TITLE_CHARS} characters"
        )));
    }
    let spec = ImmutableSpec {
        description: clean("--spec", &args.spec)?,
        acceptance: args
            .accept
            .iter()
            .map(|a| clean("--accept", a))
            .collect::<Result<_, _>>()?,
    };
    let priority = Priority::parse(&args.priority)
        .ok_or_else(|| CmdError::new(format!("--priority {}: use P0..P3", args.priority)))?;
    let mut scope_paths = Vec::with_capacity(args.scope.len());
    let mut seen_scopes: HashSet<String> = HashSet::with_capacity(args.scope.len());
    for raw in &args.scope {
        let norm =
            normalize_scope(raw).map_err(|msg| CmdError::new(format!("--scope {raw}: {msg}")))?;
        if seen_scopes.insert(norm.to_ascii_lowercase()) {
            scope_paths.push(norm);
        }
    }
    let repo = repo_root(ctx)?.to_string_lossy().into_owned();
    let db = open_db(ctx)?;
    // Checked before anything is written, so a bad id leaves no task behind.
    for blocker in &args.blocked_by {
        let other = db.get_task(blocker).map_err(port)?;
        if other.repo_root != repo {
            return Err(CmdError::new(format!(
                "--blocked-by {blocker}: that task belongs to {}",
                other.repo_root
            )));
        }
    }

    let now = SystemClock.now();
    let id = new_task_id(&db, &repo, &title, now)?;
    let mut task = Task::new_root(id, repo, title, spec, priority, now);
    // `board add` is the human saying "do this": no separate triage step.
    task.state = TaskState::Ready;
    task.scope_paths = scope_paths;
    db.insert_task(&task).map_err(port)?;
    for blocker in &args.blocked_by {
        db.add_dep(&task.id, blocker).map_err(port)?;
    }
    let payload = json!({
        "title": task.title,
        "blocked_by": args.blocked_by,
        "scope_paths": task.scope_paths,
    });
    let event = FactoryEvent::new(EventKind::TaskCreated, payload.to_string(), now);
    db.emit(&event.for_task(&task.id)).map_err(port)?;

    Ok(CmdOutput::ok("ok", format!("added {}", task_line(&task)))
        .field("task", task_json(&task)?)
        .field("blocked_by", json!(args.blocked_by))
        .field("scope_paths", json!(task.scope_paths)))
}

fn list(args: &ListArgs, ctx: &Ctx) -> CmdResult {
    let state = match &args.state {
        None => None,
        Some(name) => Some(
            TaskState::parse(&name.to_ascii_lowercase())
                .ok_or_else(|| CmdError::new(format!("--state {name}: unknown state")))?,
        ),
    };
    let repo = if args.all {
        None
    } else {
        Some(repo_root(ctx)?.to_string_lossy().into_owned())
    };
    let db = open_db(ctx)?;
    let tasks = db.list_tasks(repo.as_deref(), state).map_err(port)?;
    let frozen = is_frozen(&db).map_err(port)?;

    let mut text: Vec<String> = tasks.iter().map(task_line).collect();
    if text.is_empty() {
        text.push("no tasks".to_string());
    }
    if frozen {
        text.push("factory is FROZEN: aaf board unfreeze".to_string());
    }
    let rows = tasks.iter().map(task_json).collect::<Result<Vec<_>, _>>()?;
    Ok(CmdOutput::ok("ok", text.join("\n"))
        .field("count", json!(rows.len()))
        .field("tasks", Value::Array(rows))
        .field("frozen", json!(frozen)))
}

fn event_json(event: &FactoryEvent) -> Value {
    // Payloads are JSON written by aaf; one that is not is shown as text.
    let payload = serde_json::from_str::<Value>(&event.payload_json)
        .unwrap_or_else(|_| Value::String(event.payload_json.clone()));
    json!({
        "id": event.id,
        "kind": event.kind,
        "run_id": event.run_id,
        "payload": payload,
        "created_at": event.created_at,
    })
}

fn show(id: &str, ctx: &Ctx) -> CmdResult {
    let db = open_db(ctx)?;
    let task = db.get_task(id).map_err(port)?;
    let mut deps = Vec::new();
    for dep in db.deps_of(id).map_err(port)? {
        // A dependency deleted by hand still shows, as unknown.
        let state = db.get_task(&dep).map_or("unknown", |t| t.state.as_str());
        deps.push(json!({ "id": dep, "state": state }));
    }
    let runs = db.runs_for_task(id).map_err(port)?;
    let events = db.events_for_task(id, SHOWN_EVENTS).map_err(port)?;
    let budget = match db.get_budget(&task.root_id) {
        Ok(budget) => Some(budget),
        Err(PortError::NotFound(_)) => None,
        Err(other) => return Err(port(other)),
    };

    let mut text = vec![task_line(&task), task.spec.description.clone()];
    text.extend(
        task.spec
            .acceptance
            .iter()
            .map(|a| format!("  accept: {a}")),
    );
    if !task.scope_paths.is_empty() {
        text.push(format!("  scope: {}", task.scope_paths.join(", ")));
    }
    if let Some(reason) = &task.blocked_reason {
        text.push(format!("  blocked: {}", json!(reason)));
    }
    text.extend(
        deps.iter()
            .map(|d| format!("  blocked by: {} ({})", d["id"], d["state"])),
    );
    text.extend(task.context_notes.iter().map(|n| format!("  note: {n}")));
    for run in &runs {
        text.push(format!(
            "  run {} {} {} tokens {}+{}",
            run.id,
            run.executor,
            run.status.as_str(),
            run.tokens_in,
            run.tokens_out
        ));
    }
    for event in &events {
        text.push(format!(
            "  event {} {}",
            event.kind.as_str(),
            event.payload_json
        ));
    }

    let runs = serde_json::to_value(&runs).map_err(|e| err("runs", e))?;
    let budget = serde_json::to_value(&budget).map_err(|e| err("budget", e))?;
    Ok(CmdOutput::ok("ok", text.join("\n"))
        .field("task", task_json(&task)?)
        .field("deps", Value::Array(deps))
        .field("runs", runs)
        .field("budget", budget)
        .field(
            "events",
            Value::Array(events.iter().map(event_json).collect()),
        ))
}

fn unblock(id: &str, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board unblock") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let task = db.get_task(id).map_err(port)?;
    if task.state != TaskState::Blocked {
        let text = format!("{id} is {}, not blocked", task.state.as_str());
        return Ok(CmdOutput::policy("refused", text).field("state", json!(task.state)));
    }
    let now = SystemClock.now();
    // A root that ran out of budget would be blocked again at once: the
    // human's go-ahead comes with a fresh one (same limits).
    let budget_reset = match db.get_budget(&task.root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..budget
            })
            .map_err(port)?;
            true
        }
        Ok(_) | Err(PortError::NotFound(_)) => false,
        Err(other) => return Err(port(other)),
    };
    let plan_approved = approve_plan::maybe_approve_on_unblock(&db, &task, now)?;
    db.set_state(id, TaskState::Ready, None, now)
        .map_err(port)?;
    let payload = json!({
        "to": "ready",
        "why": "aaf board unblock",
        "by": "human",
        "was": task.blocked_reason,
        "budget_reset": budget_reset,
        "plan_approved": plan_approved,
    });
    let event = FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now);
    db.emit(&event.for_task(id)).map_err(port)?;
    Ok(CmdOutput::ok("ok", format!("{id} is ready"))
        .field("task_id", json!(id))
        .field("state", json!(TaskState::Ready))
        .field("budget_reset", json!(budget_reset))
        .field("plan_approved", json!(plan_approved)))
}

fn unfreeze_factory(ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board unfreeze") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let was_frozen = is_frozen(&db).map_err(port)?;
    unfreeze(&db, &SystemClock).map_err(port)?;
    Ok(CmdOutput::ok("ok", "factory is not frozen")
        .field("frozen", json!(false))
        .field("was_frozen", json!(was_frozen)))
}
