//! S1 marker detection from `base_sha`, S2.1–S2.5 pre-clone checks, and S4 overlap checks.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path};
use std::time::Instant;

use aaf_domain::worktree_config::WorktreeConfig;
use serde_json::Value;

use super::clone::SEED_TIMEOUT;
use super::git_bytes::git_raw_bytes;
use crate::git::hydration::stage::truncate_stderr_first_line;
use crate::git::hydration::validate_relative_entry;
use crate::git::path_str;

pub const UNITY_EXCLUDED_ENTRIES: [&str; 7] = [
    "ScriptAssemblies",
    "Bee",
    "BuildPlayerData",
    "PlayerDataCache",
    "Il2cppBuildCache",
    "EditorUserBuildSettings.asset",
    "LastBuild.buildreport",
];

const MARKER_PATHS: [&str; 9] = [
    "ProjectSettings/ProjectVersion.txt",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Podfile",
    "Podfile.lock",
    "ios/Podfile",
    "ios/Podfile.lock",
];

const UNITY_STATE_FILES: [&str; 3] = [
    "ProjectSettings/ProjectVersion.txt",
    "Packages/manifest.json",
    "Packages/packages-lock.json",
];

const NPM_COMPARE_FIELDS: [&str; 4] = ["version", "resolved", "integrity", "link"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedKind {
    Unity,
    Npm,
    CocoaPods { lock_rel: &'static str },
}

impl SeedKind {
    pub fn label(&self) -> &'static str {
        match self {
            SeedKind::Unity => "unity",
            SeedKind::Npm => "npm",
            SeedKind::CocoaPods { .. } => "cocoapods",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedTarget {
    pub kind: SeedKind,
    pub rel_dir: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedCandidate {
    Target(SeedTarget),
    Skipped {
        rel_dir: &'static str,
        reason: String,
    },
}

pub(super) fn validate_base_sha(base_sha: &str) -> Result<&str, String> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("invalid_base_sha: {base_sha:?}"));
    }
    Ok(sha)
}

/// S2.2 (deadline-bounded): verifies `rel` has no tracked files and is ignored by a
/// git-tracked `.gitignore` inside `repo_root`, honoring S2.8 `deadline` on every git call.
pub(super) fn assert_tracked_gitignore_timed(
    repo_root: &Path,
    rel: &Path,
    deadline: Instant,
) -> Result<(), String> {
    let clean = validate_relative_entry(path_str(rel).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let rel_str = path_str(&clean).map_err(|e| e.to_string())?;

    let tracked = git_raw_bytes(repo_root, &["ls-files", "--", rel_str], deadline)?;
    if !tracked.success() {
        return Err(format!("ls_files_failed (exit {:?})", tracked.code));
    }
    if !String::from_utf8_lossy(&tracked.stdout).trim().is_empty() {
        return Err(format!(
            "hydration path {rel_str:?} contains git-tracked files"
        ));
    }

    let probe = format!("{rel_str}/");
    let out = git_raw_bytes(
        repo_root,
        &[
            "-c",
            "core.excludesfile=/dev/null",
            "check-ignore",
            "--no-index",
            "--verbose",
            "--",
            &probe,
        ],
        deadline,
    )?;
    if !out.success() {
        return Err(format!(
            "hydration path {rel_str:?} is not ignored by a tracked .gitignore"
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let source = parse_check_ignore_source(&stdout)
        .ok_or_else(|| format!("could not determine gitignore source for {rel_str:?}"))?;
    let source_path = Path::new(source);
    let valid_source = !source_path.is_absolute()
        && !source.ends_with("/info/exclude")
        && source != "info/exclude"
        && source_path.file_name() == Some(OsStr::new(".gitignore"))
        && source_path.components().all(|c| {
            matches!(
                c,
                Component::Normal(n)
                    if n.to_str().is_some_and(|s| !s.eq_ignore_ascii_case(".git"))
            )
        });
    if !valid_source {
        return Err(format!(
            "hydration path {rel_str:?} must be ignored by a repo .gitignore (matched {source:?})"
        ));
    }
    let src_tracked = git_raw_bytes(
        repo_root,
        &["ls-files", "--error-unmatch", "--", source],
        deadline,
    )?;
    if !src_tracked.success() {
        return Err(format!(
            "gitignore source {source:?} for {rel_str:?} is not tracked in git"
        ));
    }
    Ok(())
}

fn parse_check_ignore_source(stdout: &str) -> Option<&str> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let (meta, _) = line.split_once('\t')?;
    let (source, rest) = meta.split_once(':')?;
    let (linenum, _) = rest.split_once(':')?;
    let digits = !linenum.is_empty() && linenum.bytes().all(|b| b.is_ascii_digit());
    (!source.is_empty() && digits).then_some(source)
}

/// S1: Selects seed candidates solely from `git ls-tree <base_sha>` (never reading disk).
pub fn detect_seed_candidates(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Vec<SeedCandidate>, String> {
    detect_seed_candidates_timed(repo_root, base_sha, Instant::now() + SEED_TIMEOUT)
}

pub(super) fn detect_seed_candidates_timed(
    repo_root: &Path,
    base_sha: &str,
    deadline: Instant,
) -> Result<Vec<SeedCandidate>, String> {
    let sha = validate_base_sha(base_sha)?;
    let mut args = vec!["ls-tree", sha, "--"];
    args.extend_from_slice(&MARKER_PATHS);
    let out = git_raw_bytes(repo_root, &args, deadline)?;
    if !out.success() {
        return Err(format!(
            "ls_tree_failed (exit {:?}): {}",
            out.code,
            truncate_stderr_first_line(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut blobs = BTreeSet::new();
    for line in text.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mode = meta.split_whitespace().next().unwrap_or("");
        if matches!(mode, "100644" | "100755") {
            blobs.insert(path.to_string());
        }
    }
    Ok(select_candidates_from_blobs(&blobs))
}

fn select_candidates_from_blobs(blobs: &BTreeSet<String>) -> Vec<SeedCandidate> {
    let mut out = Vec::new();
    if blobs.contains("ProjectSettings/ProjectVersion.txt") {
        out.push(SeedCandidate::Target(SeedTarget {
            kind: SeedKind::Unity,
            rel_dir: "Library",
        }));
    }
    if blobs.contains("yarn.lock") || blobs.contains("pnpm-lock.yaml") {
        out.push(SeedCandidate::Skipped {
            rel_dir: "node_modules",
            reason: "unsupported_package_manager".to_string(),
        });
    } else if blobs.contains("package.json") && blobs.contains("package-lock.json") {
        out.push(SeedCandidate::Target(SeedTarget {
            kind: SeedKind::Npm,
            rel_dir: "node_modules",
        }));
    }
    if blobs.contains("Podfile") && blobs.contains("Podfile.lock") {
        out.push(SeedCandidate::Target(SeedTarget {
            kind: SeedKind::CocoaPods {
                lock_rel: "Podfile.lock",
            },
            rel_dir: "Pods",
        }));
    }
    if blobs.contains("ios/Podfile") && blobs.contains("ios/Podfile.lock") {
        out.push(SeedCandidate::Target(SeedTarget {
            kind: SeedKind::CocoaPods {
                lock_rel: "ios/Podfile.lock",
            },
            rel_dir: "ios/Pods",
        }));
    }
    out
}

/// Reads exact raw bytes of a regular file blob from `base_sha` bounded by `deadline`.
pub(super) fn git_blob_at_base_timed(
    repo_root: &Path,
    base_sha: &str,
    rel: &str,
    apply_filters: bool,
    deadline: Instant,
) -> Result<Option<Vec<u8>>, String> {
    let sha = validate_base_sha(base_sha)?;
    let listed = git_raw_bytes(repo_root, &["ls-tree", sha, "--", rel], deadline)?;
    if !listed.success() {
        return Err(format!(
            "ls_tree failed (exit {:?}): {}",
            listed.code,
            truncate_stderr_first_line(&listed.stderr)
        ));
    }
    let line_owned = String::from_utf8_lossy(&listed.stdout);
    let line = line_owned.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(format!("base_blob_not_regular_file: {rel} (mode {mode})"));
    }
    let spec = format!("{sha}:{rel}");
    let shown = if apply_filters {
        git_raw_bytes(repo_root, &["cat-file", "--filters", &spec], deadline)?
    } else {
        git_raw_bytes(repo_root, &["show", &spec], deadline)?
    };
    if !shown.success() {
        return Err(format!(
            "git blob read {spec} failed (exit {:?}): {}",
            shown.code,
            truncate_stderr_first_line(&shown.stderr)
        ));
    }
    Ok(Some(shown.stdout))
}

/// Reads a regular non-symlink file on disk, returning `Ok(None)` if `NotFound`.
pub(super) fn read_regular_file_opt(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("stat {}: {err}", path.display())),
        Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
            Err(format!("non_regular_file: {}", path.display()))
        }
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|err| format!("read {}: {err}", path.display())),
    }
}

/// S2.4 (npm): Verifies `node_modules/.package-lock.json` (`installed_lock`)
/// against `base_sha:package-lock.json` (`base_lock`).
pub fn check_npm_lock_match(base_lock: &[u8], installed_lock: &[u8]) -> Result<(), String> {
    let base: Value =
        serde_json::from_slice(base_lock).map_err(|_| "npm_base_lock_parse_error".to_string())?;
    let inst: Value = serde_json::from_slice(installed_lock)
        .map_err(|_| "npm_installed_lock_parse_error".to_string())?;

    let base_ver = base.get("lockfileVersion").and_then(Value::as_u64);
    let inst_ver = inst.get("lockfileVersion").and_then(Value::as_u64);
    if !base_ver.is_some_and(|v| v >= 2) || !inst_ver.is_some_and(|v| v >= 2) {
        return Err("npm_lockfile_version_unsupported".to_string());
    }

    let base_pkgs = base
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| "npm_base_lock_missing_packages".to_string())?;
    let inst_pkgs = inst
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| "npm_installed_lock_missing_packages".to_string())?;

    let fields_match = |a: &Value, b: &Value| {
        NPM_COMPARE_FIELDS
            .iter()
            .all(|field| a.get(*field) == b.get(*field))
    };

    for (key, inst_val) in inst_pkgs {
        if key.is_empty() {
            continue;
        }
        let Some(base_val) = base_pkgs.get(key) else {
            return Err(format!("npm_extra_installed_pkg: {key}"));
        };
        if !fields_match(base_val, inst_val) {
            return Err(format!("npm_pkg_mismatch: {key}"));
        }
    }

    for (key, base_val) in base_pkgs {
        if key.is_empty() {
            continue;
        }
        let is_optional = base_val
            .get("optional")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || base_val
                .get("devOptional")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if is_optional {
            continue;
        }
        let Some(inst_val) = inst_pkgs.get(key) else {
            return Err(format!("npm_missing_installed_pkg: {key}"));
        };
        if !fields_match(base_val, inst_val) {
            return Err(format!("npm_pkg_mismatch: {key}"));
        }
    }

    Ok(())
}

/// S2.4 (CocoaPods): Exact byte match between `Podfile.lock` and `Pods/Manifest.lock`.
pub fn check_pods_lock_match(base_lock: &[u8], manifest_lock: &[u8]) -> Result<(), String> {
    if base_lock == manifest_lock {
        Ok(())
    } else {
        Err("pods_manifest_mismatch".to_string())
    }
}

/// S2.4 (Unity): Verifies `ProjectVersion.txt`, `manifest.json`, and `packages-lock.json`
/// on `repo_root` disk match `base_sha` (applying git filters via `cat-file --filters`).
pub fn check_unity_state_match(repo_root: &Path, base_sha: &str) -> Result<(), String> {
    check_unity_state_match_timed(repo_root, base_sha, Instant::now() + SEED_TIMEOUT)
}

pub(super) fn check_unity_state_match_timed(
    repo_root: &Path,
    base_sha: &str,
    deadline: Instant,
) -> Result<(), String> {
    for rel in UNITY_STATE_FILES {
        let base_bytes = git_blob_at_base_timed(repo_root, base_sha, rel, true, deadline)?;
        let disk_bytes = read_regular_file_opt(&repo_root.join(rel))?;
        if base_bytes != disk_bytes {
            return Err(format!("unity_state_mismatch: {rel}"));
        }
    }
    Ok(())
}

fn find_exec_flag_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    (0..bytes.len().saturating_sub(2)).find(|&i| {
        bytes[i].is_ascii_whitespace()
            && bytes[i + 1] == b'-'
            && (bytes[i + 2].is_ascii_alphabetic() || bytes[i + 2] == b'-')
    })
}

fn is_unity_process_args(args: &str) -> bool {
    let before_flag = find_exec_flag_boundary(args).map_or(args, |idx| &args[..idx]);
    let exec_path = before_flag.trim().trim_matches(|c| c == '"' || c == '\'');
    Path::new(exec_path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("unity") || name.eq_ignore_ascii_case("unity-editor")
        })
}

fn find_next_flag_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    (0..bytes.len().saturating_sub(2)).find(|&i| {
        bytes[i].is_ascii_whitespace() && bytes[i + 1] == b'-' && bytes[i + 2].is_ascii_alphabetic()
    })
}

fn extract_unity_project_path(args: &str) -> Option<String> {
    let lower = args.to_ascii_lowercase();
    let needle = "-projectpath";
    let mut search_from = 0;
    while let Some(rel_pos) = lower[search_from..].find(needle) {
        let pos = search_from + rel_pos;
        let boundary_before = pos == 0 || args.as_bytes()[pos - 1].is_ascii_whitespace();
        let after_flag = pos + needle.len();
        let boundary_after =
            after_flag < args.len() && args.as_bytes()[after_flag].is_ascii_whitespace();
        if boundary_before && boundary_after {
            let rest = args[after_flag..].trim_start();
            let end = find_next_flag_boundary(rest).unwrap_or(rest.len());
            let raw = rest[..end].trim();
            let unquoted = if (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
                || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2)
            {
                &raw[1..raw.len() - 1]
            } else {
                raw
            };
            if !unquoted.is_empty() {
                return Some(unquoted.to_string());
            }
        }
        search_from = after_flag;
    }
    None
}

/// Checks whether raw Unity `args` contains `needle` verbatim (case-insensitive) at path
/// token boundaries — catches project paths containing ` -<letter>` like `/Users/a/My -Game`.
fn args_contains_path_verbatim(args_lower: &str, path: &Path) -> bool {
    let Some(raw) = path.to_str() else {
        return false;
    };
    let needle = raw.trim_end_matches('/').to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    let bytes = args_lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = args_lower[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0
            || bytes[start - 1].is_ascii_whitespace()
            || matches!(bytes[start - 1], b'"' | b'\'' | b'=');
        let after = &args_lower[end..];
        let after_ok = after.is_empty()
            || after.starts_with('/')
            || after.starts_with('"')
            || after.starts_with('\'')
            || (after.len() >= 2
                && after.as_bytes()[0].is_ascii_whitespace()
                && after.as_bytes()[1] == b'-');
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// S2.5: Fail-closed check on `ps -A -ww -o pid=,args=` output against `main_root`.
pub fn unity_editor_conflict(
    ps_output: &str,
    main_root: &Path,
    has_unity_lockfile: bool,
) -> Option<String> {
    let Ok(main_canon) = fs::canonicalize(main_root) else {
        return Some("unity_project_path_unresolved".to_string());
    };
    for line in ps_output.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let args = trimmed
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim_start());
        if args.is_empty() || !is_unity_process_args(args) {
            continue;
        }
        // Finding #6: fail-closed raw match catches paths containing ` -<letter>` (e.g. `My -Game`).
        let args_lower = args.to_ascii_lowercase();
        if args_contains_path_verbatim(&args_lower, &main_canon)
            || args_contains_path_verbatim(&args_lower, main_root)
        {
            return Some("unity_editor_open".to_string());
        }
        if let Some(proj) = extract_unity_project_path(args) {
            let proj_path = Path::new(&proj);
            if !proj_path.is_absolute() {
                return Some("unity_project_path_unresolved".to_string());
            }
            let Ok(proj_canon) = fs::canonicalize(proj_path) else {
                return Some("unity_project_path_unresolved".to_string());
            };
            if proj_canon.starts_with(&main_canon) {
                return Some("unity_editor_open".to_string());
            }
        } else if has_unity_lockfile {
            return Some("unity_lockfile_with_unknown_unity_process".to_string());
        }
    }
    None
}

/// S2.5: Fail-closed Unity Editor check (`ps` error or timeout skips Unity seed).
pub fn check_unity_editor_not_open(
    main_root: &Path,
    ps_result: Result<&str, &str>,
) -> Result<(), String> {
    let ps_out = ps_result.map_err(|err| format!("unity_ps_failed: {err}"))?;
    let has_lockfile = fs::symlink_metadata(main_root.join("Temp/UnityLockfile")).is_ok();
    if let Some(reason) = unity_editor_conflict(ps_out, main_root, has_lockfile) {
        return Err(reason);
    }
    Ok(())
}

fn normal_components(path: &Path) -> Vec<&OsStr> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect()
}

fn component_eq_ascii_ci(a: &OsStr, b: &OsStr) -> bool {
    match (a.to_str(), b.to_str()) {
        (Some(sa), Some(sb)) => sa.eq_ignore_ascii_case(sb),
        _ => a == b,
    }
}

fn paths_overlap_case_insensitive(a: &Path, b: &Path) -> bool {
    let ac = normal_components(a);
    let bc = normal_components(b);
    if ac.is_empty() || bc.is_empty() {
        return false;
    }
    let min_len = ac.len().min(bc.len());
    (0..min_len).all(|i| component_eq_ascii_ci(ac[i], bc[i]))
}

/// S4: Checks whether `rel` overlaps (case-insensitively by component, for APFS)
/// any entry in `[worktree].clone_dirs`, `copy_files`, or `gate_copy_files`.
/// Invalid entries fail closed (treated as overlapping).
pub fn overlaps_worktree_config(rel: &Path, cfg: &WorktreeConfig) -> Option<String> {
    let groups = [
        ("clone_dirs", &cfg.clone_dirs),
        ("copy_files", &cfg.copy_files),
        ("gate_copy_files", &cfg.gate_copy_files),
    ];
    for (label, entries) in groups {
        for raw in entries {
            let Ok(entry) = validate_relative_entry(raw) else {
                return Some(format!("overlaps_invalid_{label}:{raw}"));
            };
            if paths_overlap_case_insensitive(rel, &entry) {
                return Some(format!("overlaps_{label}:{raw}"));
            }
        }
    }
    None
}
