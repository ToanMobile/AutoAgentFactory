//! `aaf board adopt <N> [--author-executor <name>]`: hands a pull request that
//! somebody else opened (a chat session, usually) to the factory's adopt lane.
//!
//! Order matters (checklist C8): every input check first, nothing created; then
//! the ref `aaf/<id>` and its worktree, so the head is pinned before any task
//! can point at it; then one transaction for the task and its events; then the
//! comment on the PR. A failure after the pin takes the pin away again.

use aaf_domain::event::{EventKind, FactoryEvent, PrAdoptedPayload};
use aaf_domain::ports::Clock;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::AdoptInsert;
use aaf_engine::detector::github_issues::frame_untrusted;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::git::base_ref::resolve_base;
use aaf_engine::git::{GhClient, SystemGh, main_work_tree};
use aaf_engine::pr_ops::adopt_intake::{self, Intake, Refusal};
use aaf_engine::pr_ops::adopt_pin::{pin_head, unpin_head};
use serde_json::json;

use super::store::{err, open_db, refuse_inside_agent_run};
use super::{MAX_TITLE_CHARS, new_task_id, port};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const MAX_BODY_CHARS: usize = 6_000;
const MAX_EXECUTOR_CHARS: usize = 64;
const ACCEPTANCE: &str = "the head passes the gate, two reviewers of different providers pass it, \
                          and it merges at the pinned head";

/// The help of `aaf board adopt --help`.
pub(super) const LONG_ABOUT: &str = "Hand an open pull request to the factory.\n\n\
The PR must be open, not a draft, from a branch of this repository (not a fork, \
not `aaf/*`, not a protected branch), aimed at the repository's base, and written \
by a login in [safety].owner_github_allowlist. The head commit is pinned to \
`aaf/<task id>` and the task is created as `adopting`.\n\n\
The adopt lane runs only while `aaf run --auto` is running; `--once` and the \
default drain skip adopting tasks. It gates the PR on the merged tree, asks two \
reviewers of different providers, and merges at the pinned head.\n\n\
To keep a PR out of the factory, leave it a draft. If the lane cancels the \
adoption (or you push another commit), fix the PR, push, and adopt it again.\n\n\
Changes to `.autofactory/**`, CI workflows and actions, agent hook settings and \
files named in [commands] always wait for a person: the lane cancels with \
`needs_human_head_lock` instead of approving them.";

#[derive(Debug, clap::Args)]
pub struct AdoptArgs {
    /// Number of the open pull request to adopt.
    pub number: u64,
    /// What wrote the PR (recorded in the adoption event only; it does not
    /// choose the reviewers).
    #[arg(long = "author-executor", value_name = "NAME")]
    pub author_executor: Option<String>,
}

fn refused(refusal: &Refusal, number: u64) -> CmdOutput {
    CmdOutput::policy("refused", refusal.message.clone())
        .field("reason", json!(refusal.code))
        .field("pr_number", json!(number))
}

/// One line, no control characters, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn executor_name(raw: Option<&str>) -> Result<Option<String>, CmdError> {
    let Some(raw) = raw else { return Ok(None) };
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_EXECUTOR_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(CmdError::new(format!(
            "--author-executor must be one word of at most {MAX_EXECUTOR_CHARS} characters"
        )));
    }
    Ok(Some(name.to_string()))
}

/// The task for the PR: spec = its title and body, framed as untrusted data.
fn adopting_task(id: &str, root: &str, intake: &Intake, now: i64) -> Task {
    let pr = &intake.pr;
    let pr_title = one_line(&pr.title, MAX_TITLE_CHARS);
    let title: String = format!("adopt PR #{}: {pr_title}", pr.number)
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    let body: String = pr.body.chars().take(MAX_BODY_CHARS).collect();
    let spec = ImmutableSpec {
        description: format!(
            "Adopted pull request #{} ({}), pinned at {}.\n\n{}",
            pr.number,
            pr.url,
            intake.head_sha,
            frame_untrusted(&format!("{pr_title}\n\n{body}"))
        ),
        acceptance: vec![ACCEPTANCE.to_string()],
    };
    let mut task = Task::new_root(id, root, title, spec, Priority::P1, now);
    task.state = TaskState::Adopting;
    task.fingerprint = Some(adopt_intake::fingerprint(pr));
    task
}

fn events_for(
    task: &Task,
    intake: &Intake,
    executor: Option<String>,
    now: i64,
) -> Result<Vec<FactoryEvent>, CmdError> {
    let pr = &intake.pr;
    let created = json!({
        "title": task.title,
        "blocked_by": Vec::<String>::new(),
        "scope_paths": Vec::<String>::new(),
        "source": "board adopt",
        "pr_number": pr.number,
    });
    let adopted = PrAdoptedPayload {
        pr_number: pr.number,
        url: pr.url.clone(),
        head_sha: intake.head_sha.clone(),
        head_ref: pr.head_ref_name.clone(),
        base_ref: pr.base_ref_name.clone(),
        author_login: pr
            .author
            .as_ref()
            .map(|a| a.login.trim().to_string())
            .unwrap_or_default(),
        declared_executor: executor,
    };
    let adopted = serde_json::to_string(&adopted).map_err(|e| err("pr_adopted", e))?;
    Ok(vec![
        FactoryEvent::new(EventKind::TaskCreated, created.to_string(), now).for_task(&task.id),
        FactoryEvent::new(EventKind::PrAdopted, adopted, now).for_task(&task.id),
    ])
}

fn comment_body(task_id: &str) -> String {
    format!(
        "\u{1F916} aaf-bot: factory đang xử lý PR này (task {task_id}). \
         Push thêm commit thì lượt này huỷ; adopt lại sau khi push."
    )
}

pub fn run(args: &AdoptArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board adopt") {
        return Ok(refused);
    }
    let executor = executor_name(args.author_executor.as_deref())?;
    // The main checkout, whatever worktree the command was started in: it is
    // what `tasks.repo_root` holds and what `aaf run` lists tasks by.
    aaf_fp::repo_root(&ctx.cwd).map_err(|e| err("not inside a git repository", e))?;
    let root = main_work_tree(&ctx.cwd);
    let root = std::fs::canonicalize(&root).map_err(|e| err(&root.display().to_string(), e))?;
    let cfg = ctx.load_config(&root)?;
    let base = resolve_base(&root, &cfg.repo.gate.base_ref).map_err(port)?;
    let allowlist = &cfg.global.safety.owner_github_allowlist;

    let gh = SystemGh;
    let intake = match adopt_intake::intake(&gh, &root, args.number, &base, allowlist)
        .map_err(|e| err(&format!("PR #{}", args.number), e))?
    {
        Ok(intake) => intake,
        Err(refusal) => return Ok(refused(&refusal, args.number)),
    };

    let db = open_db(ctx)?;
    let now = SystemClock.now();
    let repo = root.to_string_lossy().into_owned();
    let id = new_task_id(&db, &repo, &format!("adopt #{}", args.number), now)?;
    let task = adopting_task(&id, &repo, &intake, now);
    let events = events_for(&task, &intake, executor, now)?;
    let tree = pin_head(&root, &ctx.worktrees_dir, &id, &intake.head_sha)
        .map_err(|e| err("could not pin the PR head", e))?;
    match db.insert_adopting(&task, &events) {
        Ok(AdoptInsert::Inserted) => {}
        Ok(AdoptInsert::AlreadyAdopting { task_id }) => {
            unpin_head(&root, &tree, &intake.head_sha);
            let text = format!(
                "PR #{} is already being adopted by task {task_id}; cancel that task first \
                 (`aaf board cancel {task_id}`) to adopt it again",
                args.number
            );
            return Ok(CmdOutput::policy("refused", text)
                .field("reason", json!("already_adopting"))
                .field("task_id", json!(task_id))
                .field("pr_number", json!(args.number)));
        }
        Err(error) => {
            unpin_head(&root, &tree, &intake.head_sha);
            return Err(port(error));
        }
    }

    let warning = gh
        .comment_pr(&root, args.number, &comment_body(&id))
        .err()
        .map(|e| format!("the PR was adopted but the comment was not posted: {e}"));
    let mut lines = vec![
        format!("adopting PR #{} as task {id}", args.number),
        format!("  head pinned: {} on aaf/{id}", intake.head_sha),
        format!("  worktree: {}", tree.path.display()),
        "  the adopt lane runs only while `aaf run --auto` is running".to_string(),
        format!("  see: aaf board show {id}"),
    ];
    if let Some(warning) = &warning {
        lines.push(format!("warning: {warning}"));
    }
    Ok(CmdOutput::ok("adopting", lines.join("\n"))
        .field("task_id", json!(id))
        .field("pr_number", json!(args.number))
        .field("url", json!(intake.pr.url))
        .field("head_sha", json!(intake.head_sha))
        .field("branch", json!(tree.branch))
        .field("worktree", json!(tree.path.display().to_string()))
        .field("commented", json!(warning.is_none()))
        .field("warning", json!(warning)))
}
