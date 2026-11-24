//! Advisory readiness check for Dual-Frontier Plan Debate and Blind Dual-Review.

use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::pipeline::{
    canonical_provider, distinct_real_canonical_providers, is_known_real_provider,
    validate_reviewers,
};
use aaf_engine::executors::{find_on_path, registry::find_descriptor};

use crate::ctx::Ctx;

fn check_executor_binary(
    ctx: &Ctx,
    label: &str,
    executor: &str,
    executor_cmd: &str,
) -> Result<(), String> {
    let exec = executor.trim();
    let cmd = executor_cmd.trim();
    if exec.is_empty() && cmd.is_empty() {
        return Err(format!("{label}: executor is not configured"));
    }
    if !cmd.is_empty() {
        let Some(program) = cmd.split_whitespace().next() else {
            return Err(format!("{label}: executor_cmd is empty"));
        };
        let path = Path::new(program);
        let found = if path.components().count() > 1 {
            path.is_file()
        } else {
            find_on_path(&ctx.path_var, program).is_some()
        };
        if !found {
            return Err(format!("{label}: {program} is not on PATH"));
        }
        return Ok(());
    }
    if let Some(desc) = find_descriptor(exec) {
        if desc
            .binaries
            .iter()
            .any(|bin| find_on_path(&ctx.path_var, bin).is_some())
        {
            return Ok(());
        }
        return Err(format!(
            "{label}: {exec} binary not on PATH (tried: {})",
            desc.binaries.join(", ")
        ));
    }
    if find_on_path(&ctx.path_var, exec).is_none() {
        return Err(format!("{label}: {exec} is not on PATH"));
    }
    Ok(())
}

fn non_empty<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = primary.trim();
    if trimmed.is_empty() {
        fallback.trim()
    } else {
        trimmed
    }
}

/// Checks Plan Debate and Blind Dual-Review provider diversity and CLI availability.
pub(super) fn dual_frontier_check(ctx: &Ctx, repo: &RepoConfig) -> Result<String, String> {
    let (plan_debate, dual_review) = (repo.pipeline.plan_debate, repo.cross_audit.dual_review);
    if !plan_debate && !dual_review {
        return Ok("disabled".to_string());
    }

    let mut issues = Vec::new();
    let mut ok_parts = Vec::new();
    let require_diff = repo.cross_audit.require_different_provider;

    if plan_debate {
        let author_exec = non_empty(&repo.pipeline.plan_author.executor, &repo.run.executor);
        let challenger_exec = non_empty(
            &repo.pipeline.plan_challenger.executor,
            &repo.cross_audit.executor,
        );
        for (label, exec, cmd) in [
            (
                "plan_author",
                author_exec,
                &repo.pipeline.plan_author.executor_cmd,
            ),
            (
                "plan_challenger",
                challenger_exec,
                &repo.pipeline.plan_challenger.executor_cmd,
            ),
        ] {
            if let Err(err) = check_executor_binary(ctx, label, exec, cmd) {
                issues.push(err);
            }
        }
        if require_diff && distinct_real_canonical_providers([author_exec, challenger_exec]) < 2 {
            if !is_known_real_provider(author_exec) || !is_known_real_provider(challenger_exec) {
                issues.push(format!(
                    "plan_debate requires 2 known real providers when require_different_provider=true \
                     (got author={author_exec:?}, challenger={challenger_exec:?})"
                ));
            } else {
                issues.push(format!(
                    "plan_debate author ({author_exec}) and challenger ({challenger_exec}) share \
                     canonical provider {:?}",
                    canonical_provider(author_exec)
                ));
            }
        }
        ok_parts.push(format!(
            "plan_debate({author_exec} vs {challenger_exec}, max_rounds={})",
            repo.pipeline.max_plan_rounds
        ));
    }

    if dual_review {
        let reviewers = &repo.cross_audit.reviewers;
        if let Err(err) = validate_reviewers(reviewers) {
            issues.push(format!("cross_audit.reviewers invalid: {err}"));
        }
        if reviewers.len() < 2 {
            issues.push(format!(
                "dual_review requires >= 2 reviewers (configured {})",
                reviewers.len()
            ));
        }
        if require_diff {
            let distinct =
                distinct_real_canonical_providers(reviewers.iter().map(|r| r.executor.as_str()));
            if distinct < 2 {
                issues.push(format!(
                    "dual_review requires >= 2 distinct real canonical providers when \
                     require_different_provider=true (found {distinct})"
                ));
            }
        }
        for reviewer in reviewers {
            let label = format!("reviewer {}", reviewer.id);
            if let Err(err) =
                check_executor_binary(ctx, &label, &reviewer.executor, &reviewer.executor_cmd)
            {
                issues.push(err);
            }
        }
        ok_parts.push(format!(
            "dual_review({} reviewers, cross_adjudicate={})",
            reviewers.len(),
            repo.cross_audit.cross_adjudicate
        ));
    }

    if issues.is_empty() {
        Ok(ok_parts.join("; "))
    } else {
        Err(issues.join("; "))
    }
}
