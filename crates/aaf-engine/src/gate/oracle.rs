//! Opt-in Red->Green & Vacuity Oracle (G6).
//!
//! Partitions production vs test changes (G6a), prepares a throwaway detached
//! worktree with production files reverted to `base` (G6b+G6c), runs the gate's
//! test command through the machine throttle, and maps the classified outcome
//! to a gate verdict.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aaf_domain::config::{OracleMode, RepoConfig};
use aaf_domain::gate::{ChangeKind, GateMode, GateStatus};
use aaf_domain::oracle_config::OracleCompileRedPolicy;

use super::changes::{AddedLines, Change};
use super::oracle_classify::{OracleOutcome, classify};
use super::oracle_worktree::{
    ScratchWorktree, inline_cfg_test_spans, overlay_live_changes, revert_prod_files,
};
use super::runner::HeavyContext;
use super::{GIT_TIMEOUT, GateError, static_checks};
use crate::throttle::HeavyJob;

/// Capture up to 64 KiB of combined test output for oracle classification.
pub const ORACLE_OUTPUT_CAP_BYTES: usize = 64 * 1024;

static ORACLE_ENV_SEQ: AtomicU64 = AtomicU64::new(0);

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "go.sum",
    "gradle.lockfile",
    "Package.resolved",
];

const ROOT_GENERATED_DIRS: &[&str] = &["target", "node_modules", ".gradle", "Pods"];

const RUNTIME_DATA_DIRS: &[&str] = &[
    "assets",
    "res",
    "resources",
    "raw",
    "testdata",
    "fixtures",
    "golden",
    "snapshots",
    "schemas",
];

/// Directories whose files are test data (Go `testdata`, golden files, Jest
/// `__snapshots__`, ...): an updated golden goes with the test that reads it,
/// so the oracle never reverts these and counts them as test changes.
const FIXTURE_DIRS: &[&str] = &[
    "testdata",
    "fixtures",
    "golden",
    "snapshots",
    "__snapshots__",
];

/// Result of running the opt-in red->green oracle after a `Clear` gate run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleVerdict {
    Skipped,
    Applied {
        status: Option<GateStatus>,
        reason: String,
    },
}

/// Applies an [`OracleVerdict`] to the gate's verdict before signing the receipt.
pub fn apply_verdict(
    status: GateStatus,
    mut reasons: Vec<String>,
    verdict: OracleVerdict,
) -> (GateStatus, Vec<String>) {
    match verdict {
        OracleVerdict::Skipped => (status, reasons),
        OracleVerdict::Applied {
            status: next,
            reason,
        } => {
            reasons.push(reason);
            (next.unwrap_or(status), reasons)
        }
    }
}

/// Recognizes test paths via `static_checks::is_test_path`, `test.*`/`tests.*`
/// stems, and files under a [`FIXTURE_DIRS`] directory.
pub fn is_oracle_test_path(path: &str) -> bool {
    if static_checks::is_test_path(path) {
        return true;
    }
    let mut dirs: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    dirs.pop();
    // Data files only: source code that lives in a module named like a
    // fixture dir (`src/snapshots/mod.rs`) stays production (fail closed).
    if !is_source_file(path)
        && dirs
            .iter()
            .any(|d| FIXTURE_DIRS.contains(&d.to_ascii_lowercase().as_str()))
    {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    matches!(stem, "test" | "tests")
}

/// Source-code extensions that are never treated as fixture data.
const SOURCE_EXTS: &[&str] = &[
    "rs", "go", "py", "js", "jsx", "mjs", "cjs", "ts", "tsx", "java", "kt", "kts", "swift", "c",
    "cc", "cpp", "cxx", "h", "hh", "hpp", "m", "mm", "rb", "php", "cs", "dart", "scala", "sh",
    "bash", "zsh", "ex", "exs", "erl", "hs", "lua", "pl", "pm", "r", "jl", "zig", "vue", "svelte",
];

fn is_source_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| SOURCE_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn is_ignored_non_prod(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some((&name, dirs)) = parts.split_last() else {
        return false;
    };
    if dirs
        .first()
        .is_some_and(|root| ROOT_GENERATED_DIRS.contains(root))
    {
        return true;
    }
    if name.ends_with(".lock") || LOCKFILES.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "license"
            | "license.txt"
            | "copying"
            | "changelog"
            | ".gitignore"
            | ".gitattributes"
            | ".editorconfig"
    ) {
        return true;
    }
    let is_doc_ext = matches!(
        lower.rsplit_once('.').map(|(_, ext)| ext),
        Some("md" | "markdown" | "rst" | "adoc")
    );
    is_doc_ext
        && !dirs
            .iter()
            .any(|d| RUNTIME_DATA_DIRS.contains(&d.to_ascii_lowercase().as_str()))
}

fn inline_rust_test_files(repo_root: &Path, added: &[AddedLines]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in added {
        if !file.path.ends_with(".rs") || is_oracle_test_path(&file.path) {
            continue;
        }
        let full = repo_root.join(&file.path);
        // Symlinks are not followed: their target may be outside the repo.
        if !fs::symlink_metadata(&full).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let Ok(src) = fs::read_to_string(&full) else {
            continue;
        };
        let spans = inline_cfg_test_spans(&src);
        if spans.is_empty() {
            continue;
        }
        let hits_test = file.lines.iter().any(|(line_no, _)| {
            let line = *line_no as usize;
            spans
                .iter()
                .any(|&(start, end)| line >= start && line <= end)
        });
        if hits_test {
            out.insert(file.path.clone());
        }
    }
    out
}

/// G6b+G6c: the scratch worktree with the live changes overlaid and the
/// production files reverted to `base_sha`.
fn prepare_reverted_tree(
    repo_root: &Path,
    head_sha: &str,
    base_sha: &str,
    prod_changes: &[&Change],
    inline_rust_tests: &BTreeSet<String>,
    heavy: &HeavyContext<'_>,
) -> Result<ScratchWorktree, GateError> {
    let scratch = ScratchWorktree::create(
        repo_root,
        heavy.data_dir,
        heavy.stats_repo_id,
        head_sha,
        base_sha,
    )?;
    overlay_live_changes(repo_root, scratch.path())?;
    revert_prod_files(
        repo_root,
        scratch.path(),
        base_sha,
        prod_changes,
        inline_rust_tests,
    )?;
    Ok(scratch)
}

struct TestFilesEnv(PathBuf);

impl TestFilesEnv {
    fn write(files: &BTreeSet<String>) -> Result<Self, GateError> {
        let seq = ORACLE_ENV_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aaf-oracle-tests-{}-{seq}.txt", std::process::id()));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        for entry in files {
            writeln!(file, "{entry}")?;
        }
        Ok(Self(path))
    }
}

impl Drop for TestFilesEnv {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn select_test_command(cfg: &RepoConfig, mode: GateMode) -> Option<(&'static str, &str)> {
    let scoped = cfg.commands.test_scoped.trim();
    let full = cfg.commands.test_full.trim();
    match mode {
        GateMode::Full | GateMode::Release => {
            if !full.is_empty() {
                Some(("test_full", full))
            } else if !scoped.is_empty() {
                Some(("test_scoped", scoped))
            } else {
                None
            }
        }
        GateMode::Scoped => {
            if !scoped.is_empty() {
                Some(("test_scoped", scoped))
            } else if !full.is_empty() {
                Some(("test_full", full))
            } else {
                None
            }
        }
    }
}

fn map_outcome(
    mode: OracleMode,
    compile_red_policy: OracleCompileRedPolicy,
    all_prod_added: bool,
    has_repro: bool,
    outcome: OracleOutcome,
) -> OracleVerdict {
    let applied = |status, reason| OracleVerdict::Applied { status, reason };
    match (mode, outcome) {
        (OracleMode::Off, _) => OracleVerdict::Skipped,
        (_, OracleOutcome::Proven) => applied(None, "oracle: red_green=proven".to_string()),
        (OracleMode::Block, OracleOutcome::Vacuous) => applied(
            Some(GateStatus::Blocked),
            "oracle: vacuous test — tests still pass after reverting production changes"
                .to_string(),
        ),
        (OracleMode::Warn, OracleOutcome::Vacuous) => applied(
            None,
            "warning: oracle: vacuous test — tests still pass after reverting production changes"
                .to_string(),
        ),
        (OracleMode::Block, OracleOutcome::CompileRed(msg)) => {
            let downgrade_to_warn = match compile_red_policy {
                OracleCompileRedPolicy::Warn => true,
                OracleCompileRedPolicy::Blocked => all_prod_added && !has_repro,
                OracleCompileRedPolicy::Unverified => false,
            };
            if downgrade_to_warn {
                applied(
                    None,
                    format!("warning: oracle: compile_red ({msg}); behavioral_red_unproven"),
                )
            } else if compile_red_policy == OracleCompileRedPolicy::Blocked {
                applied(
                    Some(GateStatus::Blocked),
                    format!(
                        "oracle: compile_red ({msg}): the changed tests do not build against the \
                         base version, so they cannot show the change fixes anything. Add a test \
                         that compiles against the base API (call only code that exists before \
                         your change) and fails there, then passes with your change"
                    ),
                )
            } else {
                applied(
                    Some(GateStatus::Unverified),
                    format!("oracle: inconclusive (compile_red: {msg})"),
                )
            }
        }
        (OracleMode::Warn, OracleOutcome::CompileRed(msg)) => applied(
            None,
            format!("warning: oracle: compile_red ({msg}); behavioral_red_unproven"),
        ),
        (OracleMode::Block, OracleOutcome::Inconclusive(msg)) => applied(
            Some(GateStatus::Unverified),
            format!("oracle: inconclusive ({msg})"),
        ),
        (OracleMode::Warn, OracleOutcome::Inconclusive(msg)) => {
            applied(None, format!("warning: oracle: inconclusive ({msg})"))
        }
    }
}

/// Runs G6a (test-change detection) and G6b+G6c (scratch worktree prod revert + test run).
pub fn evaluate(
    repo_root: &Path,
    base_sha: &str,
    cfg: &RepoConfig,
    mode: GateMode,
    changes: &[Change],
    added: &[AddedLines],
    heavy: &HeavyContext<'_>,
) -> Result<OracleVerdict, GateError> {
    evaluate_with_options(repo_root, base_sha, cfg, mode, changes, added, heavy, false)
}

/// [`evaluate`] with explicit compiler-wrapper neutralization for trusted verification.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_with_options(
    repo_root: &Path,
    base_sha: &str,
    cfg: &RepoConfig,
    mode: GateMode,
    changes: &[Change],
    added: &[AddedLines],
    heavy: &HeavyContext<'_>,
    strip_compiler_wrappers: bool,
) -> Result<OracleVerdict, GateError> {
    let oracle_mode = cfg.gate.red_green_oracle;
    if oracle_mode == OracleMode::Off {
        return Ok(OracleVerdict::Skipped);
    }
    let mut prod_changes = Vec::new();
    let mut test_files = BTreeSet::new();
    for ch in changes {
        let dest_is_test = is_oracle_test_path(&ch.path);
        if dest_is_test && ch.kind != ChangeKind::Deleted {
            test_files.insert(ch.path.clone());
        }
        let dest_is_prod = !dest_is_test && !is_ignored_non_prod(&ch.path);
        let old_is_prod = ch.kind == ChangeKind::Renamed
            && ch
                .old_path
                .as_deref()
                .is_some_and(|old| !is_oracle_test_path(old) && !is_ignored_non_prod(old));
        if dest_is_prod || old_is_prod {
            prod_changes.push(ch);
        }
    }
    if prod_changes.is_empty() {
        return Ok(OracleVerdict::Skipped);
    }
    let all_prod_added = prod_changes.iter().all(|ch| ch.kind == ChangeKind::Added);
    let has_repro = !cfg.commands.repro.trim().is_empty();
    let inline_rust_tests = inline_rust_test_files(repo_root, added);
    if test_files.is_empty() && inline_rust_tests.is_empty() {
        let msg = "oracle: no test changes detected for production diff";
        return Ok(match oracle_mode {
            OracleMode::Block => OracleVerdict::Applied {
                status: Some(GateStatus::Blocked),
                reason: msg.to_string(),
            },
            OracleMode::Warn => OracleVerdict::Applied {
                status: None,
                reason: format!("warning: {msg}"),
            },
            OracleMode::Off => OracleVerdict::Skipped,
        });
    }
    let Some((cmd_label, cmd_str)) = select_test_command(cfg, mode) else {
        return Ok(OracleVerdict::Skipped);
    };

    // No committed HEAD is a broken repository, not an unbuildable tree.
    let head_sha = super::base::head(repo_root, GIT_TIMEOUT)?.ok_or_else(|| {
        GateError::Git("oracle requires a committed HEAD in repository".to_string())
    })?;
    // A tree the oracle cannot build proves nothing either way: block mode
    // reports Unverified, warn mode a warning, never a gate error with no
    // receipt.
    let scratch = match prepare_reverted_tree(
        repo_root,
        &head_sha,
        base_sha,
        &prod_changes,
        &inline_rust_tests,
        heavy,
    ) {
        Ok(scratch) => scratch,
        Err(err) => {
            return Ok(map_outcome(
                oracle_mode,
                cfg.gate.oracle_compile_red_policy,
                all_prod_added,
                has_repro,
                OracleOutcome::Inconclusive(format!("could not prepare reverted tree: {err}")),
            ));
        }
    };

    let mut env_files = test_files;
    env_files.extend(inline_rust_tests);
    let test_files_env = TestFilesEnv::write(&env_files)?;
    let test_files_path = test_files_env.0.to_string_lossy().into_owned();
    let mode_str = match mode {
        GateMode::Scoped => "scoped",
        GateMode::Full => "full",
        GateMode::Release => "release",
    };
    let mut extra_env = vec![
        ("AAF_GATE_MODE", mode_str),
        ("AAF_BASE_SHA", base_sha),
        ("AAF_TEST_FILES_FILE", test_files_path.as_str()),
    ];
    let env_remove: Vec<&str> = if strip_compiler_wrappers {
        extra_env.extend_from_slice(super::runner::COMPILER_WRAPPER_DISABLE_ENV);
        super::runner::COMPILER_WRAPPER_ENV_VARS
            .iter()
            .copied()
            .filter(|var| !extra_env.iter().any(|(k, _)| k == var))
            .collect()
    } else {
        Vec::new()
    };
    let job = HeavyJob {
        repo_id: heavy.stats_repo_id,
        label: cmd_label,
        command: cmd_str,
        cwd: scratch.path(),
        timeout: Duration::from_secs(cfg.gate.command_timeout_secs),
        log_tail_bytes: ORACLE_OUTPUT_CAP_BYTES,
    };
    let run = heavy.throttle.run_heavy_with_env_and_removals(
        heavy.data_dir,
        heavy.clock,
        &job,
        &extra_env,
        &env_remove,
    )?;
    let output = String::from_utf8_lossy(&run.output);
    Ok(map_outcome(
        oracle_mode,
        cfg.gate.oracle_compile_red_policy,
        all_prod_added,
        has_repro,
        classify(run.exit_code, run.timed_out, &output),
    ))
}
