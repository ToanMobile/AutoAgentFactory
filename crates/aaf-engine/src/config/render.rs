//! The commented `.autofactory/config.toml` that `aaf init` writes.

use std::fmt::Write as _;

use aaf_domain::config::{RepoConfig, StoreMode};

use super::detect::Detected;

/// A TOML string literal with every special character escaped.
fn quoted(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

/// Text for a `#` comment: control characters would end the comment line and
/// let the rest be read as configuration.
fn comment_safe(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Renders tier 2 for a freshly initialised repo: the detected profile and
/// commands, every other value at its default and written out so the file
/// documents what can be changed. Parses back to an equal [`RepoConfig`].
pub fn render_repo_config(detected: &Detected, name: &str) -> String {
    let defaults = RepoConfig::default();
    let gate = &defaults.gate;
    let unblocker = &defaults.unblocker;
    let run = &defaults.run;
    let cross_audit = &defaults.cross_audit;
    let proof = &defaults.proof;
    let github = &defaults.github;
    let pipeline = &defaults.pipeline;
    let columns = &github.columns;
    let cmds = &detected.commands;

    let evidence = if detected.evidence.is_empty() {
        "no known marker file found".to_string()
    } else {
        comment_safe(&detected.evidence.join(", "))
    };
    let store = match gate.store_compliance {
        StoreMode::Off => "off",
        StoreMode::Auto => "auto",
        StoreMode::Always => "always",
    };

    let mut out = String::new();
    // Writing to a String cannot fail, so the fmt::Result carries no information.
    let _ = write!(
        out,
        "\
# AutoAgentFactory repo configuration (tier 2). Commit this file.
# Personal overrides go in .autofactory/config.local.toml (not committed); they,
# AAF_* environment variables and command-line flags can only tighten [gate]
# and cannot set [commands].
# Stack detection: {evidence}

[project]
name = {name}
# auto, android, ios, flutter, react-native, expo, web, backend, rust or unity.
profile = {profile}

# Shell commands the gate runs with `sh -c` at the repo root. Only commands
# that are certain for the detected stack are filled in; check them. While the
# test command of a gate mode is empty, that mode reports Unverified, never Clear.
[commands]
build = {build}
# Fast tests for `aaf gate --scoped`.
test_scoped = {test_scoped}
# Whole suite for `aaf gate --full` and `--release`.
test_full = {test_full}
lint = {lint}
# Heavy project-wide lint run only in `--full` and `--release` modes.
lint_full = {lint_full}
# Reproduces a reported failure. Manual only (`aaf repro`, `aaf board add
# --repro-cmd`); the gate and the factory never run it.
repro = {repro}

[gate]
# auto = origin/HEAD, origin/main, origin/master, main, master (first that exists).
base_ref = {base_ref}
# Existing files matching these globs cannot be modified, deleted or renamed
# without a signed `aaf approve`. Newly added test files are exempt; new
# build, runner, CI, and agent config files are locked.
head_lock_globs = [
",
        name = quoted(name),
        profile = quoted(&detected.profile),
        build = quoted(&cmds.build),
        test_scoped = quoted(&cmds.test_scoped),
        test_full = quoted(&cmds.test_full),
        lint = quoted(&cmds.lint),
        lint_full = quoted(&cmds.lint_full),
        repro = quoted(&cmds.repro),
        base_ref = quoted(&gate.base_ref),
    );
    for glob in &gate.head_lock_globs {
        let _ = writeln!(out, "    {},", quoted(glob));
    }
    let _ = write!(
        out,
        "\
]
# off, auto (scan when the profile ships to a store) or always.
store_compliance = {store}
min_android_target_sdk = {min_sdk}
# Per gate command; the process group is killed when it expires.
command_timeout_secs = {command_timeout}
# Skip lint/test_scoped in --scoped mode when only safe docs changed.
docs_fast_path = {docs_fast_path}
docs_fast_path_exclude_globs = []
# Red->green & vacuity oracle: off, warn, or block (recommended: block).
red_green_oracle = {red_green_oracle}

# Limits on automatic unblocking of a stuck task.
[unblocker]
max_unblock_cycles = {cycles}
max_subtask_depth = {depth}
max_token_budget = {tokens}
max_wall_clock_secs = {wall_clock}
global_freeze_consecutive_fails = {freeze}
# [unblocker.cost_guard]   # 0 = off; the machine-wide cap lives in ~/.config/autofactory/config.toml; a repo may only tighten it
# daily_token_cap = 0
# spike_factor = 0

[run]
# claude, codex, antigravity, opencode or custom.
executor = {executor}
# Command template for non-Claude executors; {{prompt}} is substituted.
executor_cmd = []
# false = open a pull request with the gate receipt; true = merge when the gate is Clear.
# Unattended preset (opt-in): auto_merge = true, and an independent auditor in
# [cross_audit] below; head-locked changes still need a signed audit.
auto_merge = {auto_merge}
max_parallel = {max_parallel}
run_timeout_secs = {run_timeout}
# Claude executor only: `claude --model` and `claude --effort`. Empty = the
# CLI's own default. The local file may override these two.
model = \"\"
effort = \"\"
# The agent gets an allow-list of aaf's environment (PATH, HOME, locale, the
# toolchain homes), not all of it. Names listed here are passed as well.
env_pass = []
# Queue brake when auto_merge = false: 0 disables the cap, positive values
# bound Review + Running + Gating tasks before starting more work.
max_open_review = {max_open_review}

# Optional separate provider/model for adversarial cross-audit of head-locked files.
[cross_audit]
# Empty = reuse [run].executor (a change to a file in head_lock_globs other than
# a test then needs a person's `aaf approve`). For unattended runs set another
# provider you are logged in to (claude, codex, gemini, opencode, custom), or
# auto = the first of gemini, codex, claude, opencode that is installed with a
# config dir, else [run].executor. auto does not check the login: an auditor
# that cannot authenticate keeps the task waiting.
executor = {ca_executor}
# Command template when executor is custom (or override binary/args).
executor_cmd = {ca_executor_cmd}
# Optional model and reasoning effort overrides for the auditor run.
model = {ca_model}
effort = {ca_effort}
# true = fail closed instead of falling back to same_account when no different provider is configured.
require_different_provider = {ca_require_diff}
# Scope of cross-audit: head_lock (default), large (>= 3 files or > 200 prod LOC), or always.
audit_scope = {ca_audit_scope}
# true = require 7 Golden Standards (G1..G7) + mechanical score thresholds to pass.
score_gate = {ca_score_gate}
# Minimum required min(mechanical_score, llm_avg) in 0..=100 when score_gate = true.
min_average_score = {ca_min_avg}
# Minimum required per-criterion score (G1..G7) in 0..=100 when score_gate = true.
min_criterion_score = {ca_min_crit}

# HMAC-signed screenshot proof verification (used by `aaf proof` and `aaf gate`).
[proof]
required = {proof_required}
min_bytes = {proof_min_bytes}
min_width = {proof_min_width}
min_height = {proof_min_height}
denylist_file = {proof_denylist}
allowed_tools = [
",
        store = quoted(store),
        min_sdk = gate.min_android_target_sdk,
        command_timeout = gate.command_timeout_secs,
        docs_fast_path = gate.docs_fast_path,
        red_green_oracle = quoted(gate.red_green_oracle.as_str()),
        cycles = unblocker.max_unblock_cycles,
        depth = unblocker.max_subtask_depth,
        tokens = unblocker.max_token_budget,
        wall_clock = unblocker.max_wall_clock_secs,
        freeze = unblocker.global_freeze_consecutive_fails,
        executor = quoted(&run.executor),
        auto_merge = run.auto_merge,
        max_parallel = run.max_parallel,
        run_timeout = run.run_timeout_secs,
        max_open_review = run.max_open_review,
        ca_executor = quoted(&cross_audit.executor),
        ca_executor_cmd = quoted(&cross_audit.executor_cmd),
        ca_model = quoted(&cross_audit.model),
        ca_effort = quoted(&cross_audit.effort),
        ca_require_diff = cross_audit.require_different_provider,
        ca_audit_scope = quoted(cross_audit.audit_scope.as_str()),
        ca_score_gate = cross_audit.score_gate,
        ca_min_avg = cross_audit.min_average_score,
        ca_min_crit = cross_audit.min_criterion_score,
        proof_required = proof.required,
        proof_min_bytes = proof.min_bytes,
        proof_min_width = proof.min_width,
        proof_min_height = proof.min_height,
        proof_denylist = quoted(&proof.denylist_file),
    );
    for tool in &proof.allowed_tools {
        let _ = writeln!(out, "    {},", quoted(tool));
    }
    let _ = write!(
        out,
        "\
]

# GitHub issues import and Projects v2 / status-label board sync.
[github]
enabled = {gh_enabled}
repo = {gh_repo}
project_owner = {gh_owner}
# Set project_number to sync a GitHub Projects v2 Status field instead of issue labels.
# project_number = 1
label_filter = {gh_label_filter}

[github.columns]
backlog = {col_backlog}
ready = {col_ready}
running = {col_running}
gating = {col_gating}
review = {col_review}
blocked = {col_blocked}
done = {col_done}
cancelled = {col_cancelled}

# Optional multi-stage pipeline (read-only plan stage and automatic audit rework).
[pipeline]
plan_stage = {pipe_plan_stage}
require_plan_approval = {pipe_require_approval}
max_audit_reworks = {pipe_max_audit_reworks}
# Automatic rework rounds from PR CI failures or owner review threads (default 2, max 3, 0 = off).
max_pr_rework_rounds = {pipe_max_pr_rework_rounds}
",
        gh_enabled = github.enabled,
        gh_repo = quoted(&github.repo),
        gh_owner = quoted(&github.project_owner),
        gh_label_filter = quoted(&github.label_filter),
        col_backlog = quoted(&columns.backlog),
        col_ready = quoted(&columns.ready),
        col_running = quoted(&columns.running),
        col_gating = quoted(&columns.gating),
        col_review = quoted(&columns.review),
        col_blocked = quoted(&columns.blocked),
        col_done = quoted(&columns.done),
        col_cancelled = quoted(&columns.cancelled),
        pipe_plan_stage = pipeline.plan_stage,
        pipe_require_approval = pipeline.require_plan_approval,
        pipe_max_audit_reworks = pipeline.max_audit_reworks,
        pipe_max_pr_rework_rounds = pipeline.max_pr_rework_rounds,
    );
    out
}
