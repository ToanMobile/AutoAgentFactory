//! Cross-audit provider resolution, independence policy, and command-template
//! construction helpers for [`crate::factory::audit_backend`].

use std::path::Path;

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::pipeline::{canonical_provider, normalize_executor_id};

const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// True when two provider names refer to the same underlying provider
/// (e.g. `gemini` and `antigravity`).
pub fn same_audit_provider(a: &str, b: &str) -> bool {
    canonical_provider(a) == canonical_provider(b)
}

/// True when `executor_cmd` invokes a `claude` binary as `argv[0]`.
pub fn audit_cmd_is_claude(executor_cmd: &str) -> bool {
    let Ok(words) = split_cmd_words(executor_cmd.trim()) else {
        return false;
    };
    let Some(arg0) = words.first() else {
        return false;
    };
    Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

/// Computes `(reviewer, same_account)` for an audit approval and enforces
/// `require_different_provider` (fail-closed when the auditor shares the
/// author's provider).
pub fn resolve_audit_policy(
    run_provider: &str,
    author_account: Option<&str>,
    audit_provider: &str,
    auditor_account: Option<&str>,
    require_different_provider: bool,
) -> Result<(String, bool), String> {
    let run_p = run_provider.trim();
    let audit_p = audit_provider.trim();
    let run_canon = canonical_provider(run_p);
    let audit_canon = canonical_provider(audit_p);
    let different_provider = audit_canon != run_canon;
    if !different_provider && require_different_provider {
        return Err(format!(
            "cross_audit.require_different_provider is true, but auditor uses the same provider \
             ({audit_p}) as [run].executor ({run_p}); configure a different [cross_audit].executor"
        ));
    }
    let same_account = if audit_canon == "custom" {
        // Unverified custom commands may invoke any underlying account/CLI.
        true
    } else if different_provider {
        false
    } else {
        // Unknown on either side counts as the same account: never overclaim
        // independence within the same provider.
        match (author_account, auditor_account) {
            (Some(author), Some(auditor)) => author == auditor,
            _ => true,
        }
    };
    let reviewer = format!(
        "cross-audit:{audit_p}/{}",
        auditor_account.unwrap_or("default")
    );
    Ok((reviewer, same_account))
}

fn validate_cli_value(field: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid cross_audit.{field} {value:?}: must not start with '-' or contain whitespace or control chars"
        ));
    }
    Ok(())
}

/// Builds the `GenericRunner` argv template for non-Claude `[cross_audit]`
/// executors (`codex`, `gemini`, `antigravity`, `opencode`, `custom`) or an
/// explicit `executor_cmd` override.
pub fn build_audit_command(cfg: &CrossAuditConfig) -> Result<Vec<String>, String> {
    let custom = cfg.executor_cmd.trim();
    if !custom.is_empty() {
        let mut words = split_cmd_words(custom)?;
        if words.is_empty() {
            return Err("cross_audit.executor_cmd is empty".to_string());
        }
        let has_placeholder = words[1..]
            .iter()
            .any(|arg| arg.contains(PROMPT_PLACEHOLDER));
        if !has_placeholder {
            // `GenericRunner` passes the prompt as an argv element (no stdin):
            // replace a trailing `-` placeholder or append `{prompt}`.
            if words.len() > 1
                && let Some(last) = words.last_mut()
                && last == "-"
            {
                *last = PROMPT_PLACEHOLDER.to_string();
            } else {
                words.push(PROMPT_PLACEHOLDER.to_string());
            }
        }
        return Ok(words);
    }

    let exec = normalize_executor_id(&cfg.executor);
    let model = cfg.model.trim();
    let effort = cfg.effort.trim();
    if !model.is_empty() {
        validate_cli_value("model", model)?;
    }
    if !effort.is_empty() {
        validate_cli_value("effort", effort)?;
    }
    match exec.as_str() {
        "codex" => {
            let mut cmd = vec![
                "codex".into(),
                "exec".into(),
                "--sandbox".into(),
                "read-only".into(),
            ];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            if !effort.is_empty() {
                cmd.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
            }
            cmd.push(PROMPT_PLACEHOLDER.into());
            Ok(cmd)
        }
        "gemini" => {
            let mut cmd = vec!["gemini".into(), "--approval-mode".into(), "plan".into()];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            cmd.extend(["-p".into(), PROMPT_PLACEHOLDER.into()]);
            Ok(cmd)
        }
        "opencode" => {
            let mut cmd = vec!["opencode".into(), "run".into()];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            cmd.push(PROMPT_PLACEHOLDER.into());
            Ok(cmd)
        }
        "grok" => {
            let mut cmd = vec![
                "grok".into(),
                "--permission-mode".into(),
                "plan".into(),
                "--no-memory".into(),
                "--no-subagents".into(),
            ];
            if !model.is_empty() {
                cmd.extend(["--model".into(), model.into()]);
            }
            if !effort.is_empty() {
                cmd.extend(["--reasoning-effort".into(), effort.into()]);
            }
            cmd.push(format!("--single={PROMPT_PLACEHOLDER}"));
            Ok(cmd)
        }
        "custom" => {
            Err("cross_audit.executor = \"custom\" requires cross_audit.executor_cmd".into())
        }
        other => Err(format!(
            "unknown cross_audit.executor {other:?}: expected claude, codex, gemini, antigravity, opencode, grok or custom"
        )),
    }
}

/// Minimal quote-aware argv splitter (no shell expansion).
fn split_cmd_words(input: &str) -> Result<Vec<String>, String> {
    let (mut words, mut cur, mut in_word, mut quote) = (Vec::new(), String::new(), false, '\0');
    let mut chars = input.chars();
    let trail = || "trailing backslash in cross_audit.executor_cmd".to_string();
    while let Some(c) = chars.next() {
        match quote {
            '\'' if c == '\'' => quote = '\0',
            '\'' => cur.push(c),
            '"' if c == '"' => quote = '\0',
            '"' if c == '\\' => cur.push(chars.next().ok_or_else(trail)?),
            '"' => cur.push(c),
            _ => match c {
                '\'' | '"' => (quote, in_word) = (c, true),
                '\\' => {
                    cur.push(chars.next().ok_or_else(trail)?);
                    in_word = true;
                }
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                other => {
                    cur.push(other);
                    in_word = true;
                }
            },
        }
    }
    if quote != '\0' {
        return Err("unterminated quote in cross_audit.executor_cmd".to_string());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// `dual_review` with no `[[cross_audit.reviewers]]`: the two reviewers are
/// `[run].executor` and the resolved auditor. When they are one provider the
/// task gets the single audit instead and the returned warning says why;
/// without this every task would stop at review for want of a quorum.
pub fn fill_default_reviewers(cfg: &mut CrossAuditConfig, run_executor: &str) -> Option<String> {
    use aaf_domain::pipeline::{ReviewerConfig, is_known_real_provider};
    if !cfg.dual_review || !cfg.reviewers.is_empty() {
        return None;
    }
    let run = normalize_executor_id(run_executor);
    let auditor = normalize_executor_id(super::configured_audit_provider(&run, cfg));
    if !cfg.executor_cmd.trim().is_empty()
        || !is_known_real_provider(&run)
        || !is_known_real_provider(&auditor)
        || same_audit_provider(&run, &auditor)
    {
        cfg.dual_review = false;
        return Some(format!(
            "cross_audit.dual_review needs two providers and only {run} is set up here: \
             one audit runs instead (install another CLI or list [[cross_audit.reviewers]])"
        ));
    }
    let reviewer = |executor: &str, model: &str, effort: &str| ReviewerConfig {
        id: executor.to_string(),
        executor: executor.to_string(),
        executor_cmd: String::new(),
        model: model.trim().to_string(),
        effort: effort.trim().to_string(),
    };
    cfg.reviewers = vec![
        reviewer(&run, "", ""),
        reviewer(&auditor, &cfg.model, &cfg.effort),
    ];
    None
}

/// The two reviewers `aaf board adopt` runs on a PR: real built-in providers
/// of two different canonical providers (`gemini` and `antigravity` are one),
/// each in `installed`, whatever `cross_audit.dual_review` says. Listed
/// `[[cross_audit.reviewers]]` come first, then `[run].executor` and the
/// resolved auditor, as [`fill_default_reviewers`] would pick them.
///
/// Empty when fewer than two providers qualify: there is no second opinion to
/// ask for, and the caller must refuse rather than fall back to one reviewer.
pub fn adopt_reviewers(
    cfg: &aaf_domain::config::RepoConfig,
    installed: &[&str],
) -> Vec<aaf_domain::pipeline::ReviewerConfig> {
    use aaf_domain::pipeline::{ReviewerConfig, is_known_real_provider, is_known_real_reviewer};
    let cross = &cfg.cross_audit;
    let run = normalize_executor_id(&cfg.run.executor);
    let auditor = normalize_executor_id(super::configured_audit_provider(&run, cross));
    let default_reviewer = |executor: &str, model: &str, effort: &str| ReviewerConfig {
        id: executor.to_string(),
        executor: executor.to_string(),
        executor_cmd: String::new(),
        model: model.trim().to_string(),
        effort: effort.trim().to_string(),
    };
    let mut candidates: Vec<ReviewerConfig> = cross
        .reviewers
        .iter()
        .filter(|r| is_known_real_reviewer(r))
        .cloned()
        .collect();
    // The runner and the auditor only stand in when they are plain providers.
    if cross.executor_cmd.trim().is_empty() {
        if is_known_real_provider(&run) {
            candidates.push(default_reviewer(&run, "", ""));
        }
        if is_known_real_provider(&auditor) {
            candidates.push(default_reviewer(&auditor, &cross.model, &cross.effort));
        }
    }
    let mut picked: Vec<ReviewerConfig> = Vec::new();
    for cand in candidates {
        let provider = canonical_provider(&cand.executor);
        let present = installed.iter().any(|i| canonical_provider(i) == provider);
        if present
            && !picked
                .iter()
                .any(|p| canonical_provider(&p.executor) == provider)
        {
            picked.push(cand);
        }
    }
    picked.truncate(2);
    if picked.len() < 2 {
        picked.clear();
    }
    picked
}

/// [`fill_default_reviewers`], then the same for `plan_debate`: with no
/// `plan_challenger`, no separate auditor and no reviewer of another
/// provider there is nobody to challenge the plan, and every task would be
/// blocked at the plan stage, so the debate is turned off with a warning.
pub fn fall_back_to_one_provider(repo: &mut aaf_domain::config::RepoConfig) -> Vec<String> {
    use aaf_domain::pipeline::is_known_real_reviewer;
    let mut warnings: Vec<String> = Vec::new();
    if repo.cross_audit.dual_review
        && repo.cross_audit.reviewers.is_empty()
        && !repo.run.executor_cmd.is_empty()
    {
        repo.cross_audit.dual_review = false;
        warnings.push(
            "cross_audit.dual_review disabled because [run].executor_cmd is set without explicit [[cross_audit.reviewers]]"
                .to_string(),
        );
    } else if let Some(w) = fill_default_reviewers(&mut repo.cross_audit, &repo.run.executor) {
        warnings.push(w);
    }
    let pipe = &repo.pipeline;
    if pipe.plan_debate && pipe.plan_challenger.is_empty() {
        let author = if pipe.plan_author.executor.trim().is_empty() {
            repo.run.executor.trim()
        } else {
            pipe.plan_author.executor.trim()
        }
        .to_string();
        let cross = &repo.cross_audit;
        let has_cross_cfg =
            !cross.executor.trim().is_empty() || !cross.executor_cmd.trim().is_empty();
        let eff = if cross.executor.trim().is_empty() {
            "custom"
        } else {
            cross.executor.trim()
        };
        let invalid_empty_custom =
            eff.eq_ignore_ascii_case("custom") && cross.executor_cmd.trim().is_empty();
        let has_distinct_cross = has_cross_cfg
            && !invalid_empty_custom
            && !same_audit_provider(&author, eff)
            && !(audit_cmd_is_claude(&cross.executor_cmd)
                && same_audit_provider(&author, "claude"));
        let challenger = has_distinct_cross
            || cross
                .reviewers
                .iter()
                .any(|r| is_known_real_reviewer(r) && !same_audit_provider(&author, &r.executor));
        if !challenger {
            repo.pipeline.plan_debate = false;
            warnings.push(format!(
                "pipeline.plan_debate needs a second provider and only {author} is set up here: \
                 the plan is written without a debate (install another CLI or set \
                 [pipeline.plan_challenger])"
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod default_reviewers_tests {
    use super::*;

    fn dual(executor: &str) -> CrossAuditConfig {
        CrossAuditConfig {
            dual_review: true,
            executor: executor.to_string(),
            ..CrossAuditConfig::default()
        }
    }

    #[test]
    fn the_runner_and_the_auditor_review_when_none_are_listed() {
        let mut cfg = dual("antigravity");
        assert_eq!(fill_default_reviewers(&mut cfg, "claude"), None);
        let ids: Vec<&str> = cfg.reviewers.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["claude", "antigravity"]);
        assert!(cfg.dual_review);
    }

    #[test]
    fn one_provider_falls_back_to_one_audit_with_a_warning() {
        let mut cfg = dual("");
        assert!(fill_default_reviewers(&mut cfg, "claude").is_some());
        assert!(!cfg.dual_review && cfg.reviewers.is_empty());
    }

    #[test]
    fn a_debate_without_a_second_provider_is_turned_off() {
        let mut repo = aaf_domain::config::RepoConfig::default();
        repo.run.executor = "claude".into();
        repo.pipeline.plan_debate = true;
        repo.cross_audit.dual_review = true;
        let warnings = fall_back_to_one_provider(&mut repo);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(!repo.pipeline.plan_debate && !repo.cross_audit.dual_review);

        let mut repo = aaf_domain::config::RepoConfig::default();
        repo.run.executor = "claude".into();
        repo.pipeline.plan_debate = true;
        repo.cross_audit.executor = "antigravity".into();
        assert!(fall_back_to_one_provider(&mut repo).is_empty());
        assert!(repo.pipeline.plan_debate);

        // Switching primary to antigravity while cross_audit is pinned to
        // antigravity (or gemini) shares canonical provider "gemini": debate
        // must cleanly fall back to off instead of self-debating or failing.
        for same in ["antigravity", "gemini"] {
            let mut same_repo = aaf_domain::config::RepoConfig::default();
            same_repo.run.executor = "antigravity".into();
            same_repo.pipeline.plan_debate = true;
            same_repo.cross_audit.dual_review = true;
            same_repo.cross_audit.executor = same.into();
            let warns = fall_back_to_one_provider(&mut same_repo);
            assert_eq!(warns.len(), 2, "same={same} warns={warns:?}");
            assert!(!same_repo.pipeline.plan_debate && !same_repo.cross_audit.dual_review);
        }

        // Switching primary to antigravity with grok auditor keeps both active.
        let mut grok_repo = aaf_domain::config::RepoConfig::default();
        grok_repo.run.executor = "antigravity".into();
        grok_repo.pipeline.plan_debate = true;
        grok_repo.cross_audit.dual_review = true;
        grok_repo.cross_audit.executor = "grok".into();
        assert!(fall_back_to_one_provider(&mut grok_repo).is_empty());
        assert!(grok_repo.pipeline.plan_debate && grok_repo.cross_audit.dual_review);
    }

    #[test]
    fn listed_reviewers_are_kept() {
        let mut cfg = dual("codex");
        cfg.reviewers = vec![aaf_domain::pipeline::ReviewerConfig {
            id: "x".into(),
            executor: "gemini".into(),
            executor_cmd: String::new(),
            model: String::new(),
            effort: String::new(),
        }];
        assert_eq!(fill_default_reviewers(&mut cfg, "claude"), None);
        assert_eq!(cfg.reviewers.len(), 1);
    }
}

#[cfg(test)]
mod adopt_reviewers_tests {
    use super::*;
    use aaf_domain::config::RepoConfig;
    use aaf_domain::pipeline::ReviewerConfig;

    fn cfg(run: &str, auditor: &str) -> RepoConfig {
        let mut cfg = RepoConfig::default();
        cfg.run.executor = run.to_string();
        cfg.cross_audit.executor = auditor.to_string();
        cfg
    }

    fn providers(revs: &[ReviewerConfig]) -> Vec<String> {
        revs.iter()
            .map(|r| canonical_provider(&r.executor))
            .collect()
    }

    #[test]
    fn two_installed_providers_are_picked_even_with_dual_review_off() {
        let mut c = cfg("claude", "codex");
        c.cross_audit.dual_review = false;
        let revs = adopt_reviewers(&c, &["claude", "codex"]);
        assert_eq!(providers(&revs), ["claude", "codex"]);
    }

    #[test]
    fn the_result_does_not_depend_on_dual_review() {
        let mut on = cfg("claude", "codex");
        on.cross_audit.dual_review = true;
        let mut off = on.clone();
        off.cross_audit.dual_review = false;
        let installed = ["claude", "codex"];
        assert_eq!(
            adopt_reviewers(&on, &installed),
            adopt_reviewers(&off, &installed)
        );
    }

    #[test]
    fn gemini_and_antigravity_count_as_one_provider() {
        let c = cfg("gemini", "antigravity");
        assert!(adopt_reviewers(&c, &["gemini", "antigravity"]).is_empty());
    }

    #[test]
    fn one_installed_provider_is_not_enough() {
        let c = cfg("claude", "codex");
        assert!(adopt_reviewers(&c, &["claude"]).is_empty());
        assert!(adopt_reviewers(&c, &[]).is_empty());
    }

    #[test]
    fn a_custom_command_is_not_a_provider() {
        let mut c = cfg("claude", "custom");
        c.cross_audit.executor_cmd = "my-wrapper {prompt}".to_string();
        assert!(adopt_reviewers(&c, &["claude", "custom"]).is_empty());
    }

    #[test]
    fn listed_reviewers_come_first_and_the_installed_filter_applies() {
        let mut c = cfg("claude", "codex");
        c.cross_audit.reviewers = vec![
            ReviewerConfig {
                id: "g".into(),
                executor: "antigravity".into(),
                ..ReviewerConfig::default()
            },
            ReviewerConfig {
                id: "o".into(),
                executor: "opencode".into(),
                ..ReviewerConfig::default()
            },
        ];
        let revs = adopt_reviewers(&c, &["claude", "antigravity"]);
        // opencode is not installed, so antigravity pairs with claude.
        assert_eq!(providers(&revs), ["gemini", "claude"]);
        assert_eq!(revs[0].id, "g");
    }
}
