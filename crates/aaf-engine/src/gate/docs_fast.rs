//! Zero-loophole docs-only fast path for `aaf gate --scoped`.
//!
//! When a scoped gate sees only safe documentation edits (no code, build,
//! config, asset, test, fixture, or head-locked paths), `lint` and
//! `test_scoped` may be skipped while hidden-file checks, static checks
//! (secret scan, etc.), head-lock, store-compliance, and proof checks still
//! run in full.

use aaf_domain::config::AGENT_CONFIG_GLOBS;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::GateError;
use super::changes::Change;

/// Prefix of the non-blocking receipt reason added when docs fast path ran.
pub const DOCS_FAST_PATH_NOTE: &str = "docs_fast_path: ";

/// Allowed file extensions (lowercase, without leading dot) for docs-only skips.
/// `.txt` is intentionally excluded because many build/proguard/data/fixture
/// files use `.txt`.
const ALLOWED_EXTS: &[&str] = &["md", "markdown", "rst", "adoc"];

/// Directory segments whose contents may be loaded at runtime, tested against,
/// or consumed by build/resource compilers — never eligible for docs fast path.
const DENIED_DIRS: &[&str] = &[
    "res",
    "assets",
    "resources",
    "raw",
    "testdata",
    "fixtures",
    "golden",
    "snapshots",
    "schemas",
    "androidtest",
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
];

/// Basenames (lowercase) that end in `.txt` or similar but affect builds,
/// dependency resolution, or site security policies.
const DENIED_FILENAMES: &[&str] = &["cmakelists.txt", "robots.txt", "security.txt", "humans.txt"];

fn build_globset(globs: &[String], label: &str) -> Result<GlobSet, GateError> {
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        let compiled = GlobBuilder::new(g)
            .literal_separator(true)
            .build()
            .map_err(|err| GateError::Config(format!("{label}: {err}")))?;
        builder.add(compiled);
    }
    builder
        .build()
        .map_err(|err| GateError::Config(format!("{label}: {err}")))
}

fn is_safe_doc_path(
    path: &str,
    head_lock: &GlobSet,
    agent_cfg: &GlobSet,
    excludes: &GlobSet,
) -> bool {
    if path.is_empty()
        || head_lock.is_match(path)
        || agent_cfg.is_match(path)
        || excludes.is_match(path)
    {
        return false;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some((&file, dirs)) = parts.split_last() else {
        return false;
    };
    for dir in dirs {
        let lower = dir.to_ascii_lowercase();
        if DENIED_DIRS.contains(&lower.as_str()) {
            return false;
        }
    }
    let lower_file = file.to_ascii_lowercase();
    if DENIED_FILENAMES.contains(&lower_file.as_str())
        || lower_file.starts_with("requirements")
        || lower_file.starts_with("constraints")
    {
        return false;
    }
    let Some((_, ext)) = lower_file.rsplit_once('.') else {
        return false;
    };
    ALLOWED_EXTS.contains(&ext)
}

/// Returns `true` iff `changes` is non-empty and every changed path (both
/// current `path` and `old_path` on renames) is a safe documentation file.
pub fn is_docs_only(
    changes: &[Change],
    head_lock_globs: &[String],
    exclude_globs: &[String],
) -> Result<bool, GateError> {
    if changes.is_empty() {
        return Ok(false);
    }
    let head_lock = build_globset(head_lock_globs, "gate.head_lock_globs")?;
    let agent_globs: Vec<String> = AGENT_CONFIG_GLOBS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let agent_cfg = build_globset(&agent_globs, "agent_config_globs")?;
    let excludes = build_globset(exclude_globs, "gate.docs_fast_path_exclude_globs")?;

    for change in changes {
        if !is_safe_doc_path(&change.path, &head_lock, &agent_cfg, &excludes) {
            return Ok(false);
        }
        if let Some(old) = &change.old_path
            && !is_safe_doc_path(old, &head_lock, &agent_cfg, &excludes)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use aaf_domain::config::GateConfig;
    use aaf_domain::gate::ChangeKind;

    use super::*;

    fn ch(kind: ChangeKind, path: &str, old: Option<&str>) -> Change {
        Change {
            kind,
            path: path.to_string(),
            old_path: old.map(str::to_string),
        }
    }

    #[test]
    fn docs_only_accepts_plain_markdown_and_rejects_empty_and_code() {
        let globs = GateConfig::default().head_lock_globs;
        assert!(!is_docs_only(&[], &globs, &[]).unwrap());
        assert!(
            is_docs_only(
                &[
                    ch(ChangeKind::Modified, "plan/Audit,Review.md", None),
                    ch(ChangeKind::Added, "docs/notes.rst", None),
                ],
                &globs,
                &[],
            )
            .unwrap()
        );
        assert!(
            !is_docs_only(
                &[
                    ch(ChangeKind::Modified, "README.md", None),
                    ch(ChangeKind::Modified, "src/main.rs", None),
                ],
                &globs,
                &[],
            )
            .unwrap()
        );
    }

    #[test]
    fn docs_only_blocks_agent_config_assets_tests_requirements_and_code_renames() {
        let globs = GateConfig::default().head_lock_globs;
        for blocked in [
            ch(ChangeKind::Modified, "AGENTS.md", None),
            ch(ChangeKind::Modified, "CLAUDE.md", None),
            ch(ChangeKind::Added, ".claude/skills/x/SKILL.md", None),
            ch(ChangeKind::Added, "Assets/Design/note.md", None),
            ch(ChangeKind::Modified, "app/src/main/res/raw/help.txt", None),
            ch(ChangeKind::Modified, "tests/fixtures/expected.txt", None),
            ch(ChangeKind::Modified, "requirements.txt", None),
            ch(ChangeKind::Modified, "CMakeLists.txt", None),
            ch(ChangeKind::Renamed, "docs/new.md", Some("src/Old.kt")),
        ] {
            assert!(
                !is_docs_only(std::slice::from_ref(&blocked), &globs, &[]).unwrap(),
                "must NOT take docs fast path for {blocked:?}"
            );
        }
        assert!(
            !is_docs_only(
                &[ch(ChangeKind::Modified, "docs/critical.md", None)],
                &globs,
                &["docs/critical.md".to_string()],
            )
            .unwrap()
        );
    }
}
