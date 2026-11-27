//! Advisory readiness check for Dual-Frontier Plan Debate and Blind Dual-Review.

use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::pipeline::{
    canonical_provider, distinct_real_reviewer_providers, is_known_real_provider,
    is_known_real_reviewer, validate_reviewers,
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
        if aaf_engine::executors::registry::resolve_binary_with_home(
            desc,
            &ctx.path_var,
            Some(&ctx.home),
        )
        .is_some()
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

fn resolve_plan_author(repo: &RepoConfig) -> (&str, &str) {
    let exec = repo.pipeline.plan_author.executor.trim();
    if exec.is_empty() {
        let run_cmd = repo
            .run
            .executor_cmd
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("");
        (repo.run.executor.trim(), run_cmd)
    } else {
        (exec, repo.pipeline.plan_author.executor_cmd.trim())
    }
}

fn resolve_plan_challenger(ctx: &Ctx, repo: &RepoConfig, author_exec: &str) -> (String, String) {
    let exec = repo.pipeline.plan_challenger.executor.trim();
    if !exec.is_empty() {
        return (
            exec.to_string(),
            repo.pipeline
                .plan_challenger
                .executor_cmd
                .trim()
                .to_string(),
        );
    }
    let audit_exec = repo.cross_audit.executor.trim();
    let audit_cmd = repo.cross_audit.executor_cmd.trim();
    if !audit_exec.is_empty() || !audit_cmd.is_empty() {
        let eff = if audit_exec.is_empty() {
            "custom".to_string()
        } else {
            aaf_engine::factory::resolve_auto_audit_executor(
                author_exec,
                audit_exec,
                &ctx.path_var,
                &ctx.home,
            )
        };
        let same_prov = aaf_engine::factory::same_audit_provider(author_exec, &eff)
            || (aaf_engine::factory::audit_cmd_is_claude(audit_cmd)
                && aaf_engine::factory::same_audit_provider(author_exec, "claude"));
        if !same_prov {
            return (eff, audit_cmd.to_string());
        }
    }
    let author_canon = canonical_provider(author_exec);
    if let Some(reviewer) = repo
        .cross_audit
        .reviewers
        .iter()
        .find(|r| is_known_real_reviewer(r) && canonical_provider(&r.executor) != author_canon)
    {
        return (
            reviewer.executor.trim().to_string(),
            reviewer.executor_cmd.trim().to_string(),
        );
    }
    (String::new(), String::new())
}

fn is_real_stage(executor: &str, executor_cmd: &str) -> bool {
    executor_cmd.trim().is_empty() && is_known_real_provider(executor)
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
        let (author_exec, author_cmd) = resolve_plan_author(repo);
        let (challenger_exec, challenger_cmd) = resolve_plan_challenger(ctx, repo, author_exec);
        let (challenger_exec, challenger_cmd) = (challenger_exec.as_str(), challenger_cmd.as_str());
        for (label, exec, cmd) in [
            ("plan_author", author_exec, author_cmd),
            ("plan_challenger", challenger_exec, challenger_cmd),
        ] {
            if let Err(err) = check_executor_binary(ctx, label, exec, cmd) {
                issues.push(err);
            }
        }
        if require_diff {
            if !is_real_stage(author_exec, author_cmd)
                || !is_real_stage(challenger_exec, challenger_cmd)
            {
                issues.push(format!(
                    "plan_debate requires 2 known real providers without custom executor_cmd when \
                     require_different_provider=true (got author={author_exec:?}, challenger={challenger_exec:?})"
                ));
            } else if canonical_provider(author_exec) == canonical_provider(challenger_exec) {
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
            let distinct = distinct_real_reviewer_providers(reviewers);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use aaf_domain::pipeline::ReviewerConfig;
    use tempfile::TempDir;

    use super::*;

    fn test_ctx(bins: &[&str]) -> (TempDir, Ctx) {
        let dir = TempDir::new().unwrap();
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        for name in bins {
            let p = bin_dir.join(name);
            fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ctx = Ctx {
            cwd: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            data_dir: dir.path().join("data"),
            config_dir: dir.path().join("cfg"),
            worktrees_dir: dir.path().join("wt"),
            path_var: OsString::from(bin_dir.as_os_str()),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        (dir, ctx)
    }

    fn rev(id: &str, executor: &str, executor_cmd: &str) -> ReviewerConfig {
        ReviewerConfig {
            id: id.to_string(),
            executor: executor.to_string(),
            executor_cmd: executor_cmd.to_string(),
            ..ReviewerConfig::default()
        }
    }

    #[test]
    fn plan_author_inherits_run_cmd_and_challenger_falls_back_to_reviewers() {
        let (_dir, ctx) = test_ctx(&["claude", "agy"]);
        let mut repo = RepoConfig::default();
        repo.pipeline.plan_debate = true;
        repo.cross_audit.require_different_provider = true;
        repo.run.executor = "claude".to_string();
        // Stale executor_cmd on empty plan_author must NOT leak when inheriting repo.run.
        repo.pipeline.plan_author.executor_cmd = "/nonexistent/stale-cmd".to_string();
        // Empty plan_challenger and empty cross_audit.executor fall back to first real
        // reviewer whose canonical provider differs from plan_author.
        repo.cross_audit.reviewers = vec![
            rev("r-claude", "claude", ""),
            rev("r-fake", "antigravity", "/bin/echo PASS"),
            rev("r-agy", "antigravity", ""),
        ];

        let ok = dual_frontier_check(&ctx, &repo).unwrap();
        assert!(ok.contains("plan_debate(claude vs antigravity"), "{ok}");

        // Non-empty repo.run.executor_cmd is inherited and excluded from real diversity.
        repo.run.executor_cmd = vec!["claude".to_string()];
        let err = dual_frontier_check(&ctx, &repo).unwrap_err();
        assert!(err.contains("without custom executor_cmd"), "{err}");
    }

    #[test]
    fn dual_review_rejects_reviewers_with_custom_executor_cmd() {
        let (_dir, ctx) = test_ctx(&["claude", "agy", "echo"]);
        let mut repo = RepoConfig::default();
        repo.cross_audit.dual_review = true;
        repo.cross_audit.require_different_provider = true;
        repo.cross_audit.reviewers = vec![
            rev("r1", "claude", "echo PASS"),
            rev("r2", "antigravity", "echo PASS"),
        ];

        let err = dual_frontier_check(&ctx, &repo).unwrap_err();
        assert!(err.contains("found 0"), "{err}");
    }
}
