//! Zero-loophole docs-only fast path for `aaf gate --scoped`.
//!
//! When a scoped gate sees only safe documentation edits (no code, build,
//! config, asset, test, fixture, or head-locked paths), `lint` and
//! `test_scoped` may be skipped while hidden-file checks, static checks
//! (secret scan, etc.), head-lock, store-compliance, and proof checks still
//! run in full.

use std::path::Path;
use std::time::Duration;

use aaf_domain::config::AGENT_CONFIG_GLOBS;
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

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
    "templates",
];

/// Markers (case-insensitive ERE) on a tracked non-doc line that compile a file
/// into a build or run it as a test. Each hit is read with 4 trailing lines,
/// since rustfmt and TOML arrays wrap the path onto the next lines.
/// Assembled with `concat!` so this file never self-matches `git grep`.
const EMBED_PATTERN: &str = concat!(
    "include_",
    "str!|include_",
    "bytes!|include_",
    "dir!|go:",
    "embed|doc",
    "test|md",
    "book[[:space:]]+test"
);

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

fn dir_referenced(block: &str, dir: &str) -> bool {
    [
        format!("\"{dir}\""),
        format!("\"{dir}/"),
        format!("/{dir}\""),
        format!("/{dir}/"),
        format!("'{dir}'"),
        format!("'{dir}/"),
        format!("/{dir}'"),
    ]
    .iter()
    .any(|needle| block.contains(needle.as_str()))
}

/// Parses pattern tokens on a Go embed directive line after the marker.
/// Returns `None` to fail closed on malformed quotes or double-quoted
/// backslash escapes.
fn parse_go_embed_patterns(rest: &str) -> Option<Vec<&str>> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut patterns = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let token = match bytes[i] {
            b'"' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        return None;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let tok = &rest[start..i];
                i += 1;
                if i < bytes.len() && !matches!(bytes[i], b' ' | b'\t') {
                    return None;
                }
                tok
            }
            b'`' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'`' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let tok = &rest[start..i];
                i += 1;
                if i < bytes.len() && !matches!(bytes[i], b' ' | b'\t') {
                    return None;
                }
                tok
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t') {
                    i += 1;
                }
                &rest[start..i]
            }
        };
        let pat = token.strip_prefix("all:").unwrap_or(token);
        let pat = pat.trim_end_matches('/');
        if !pat.is_empty() {
            patterns.push(pat);
        }
    }
    Some(patterns)
}

/// True when `matcher` matches any path suffix of `doc` (e.g. `docs/sub/g.md`,
/// `sub/g.md`, `g.md`) or any ancestor directory suffix (`docs/sub`, `sub`,
/// `docs`).
fn doc_suffixes_match(matcher: &GlobMatcher, doc: &str) -> bool {
    let trimmed = doc.trim_matches('/');
    if trimmed.is_empty() {
        return false;
    }
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut seg_start = 0;
    for (idx, b) in trimmed.bytes().enumerate() {
        if b == b'/' {
            if seg_start < idx {
                segs.push((seg_start, idx));
            }
            seg_start = idx + 1;
        }
    }
    if seg_start < trimmed.len() {
        segs.push((seg_start, trimmed.len()));
    }
    for (end_idx, &(_, end)) in segs.iter().enumerate() {
        for &(start, _) in &segs[..=end_idx] {
            if matcher.is_match(&trimmed[start..end]) {
                return true;
            }
        }
    }
    false
}

/// True when any Go embed directive line in `block` has a pattern that matches
/// `doc` (or any of its suffixes / ancestor dir suffixes), or fails to parse.
fn go_embed_matches_doc(block: &str, doc: &str) -> bool {
    let marker = concat!("go:", "embed");
    for raw_line in block.lines() {
        let line = raw_line.trim_end_matches('\r');
        for (pos, _) in line.match_indices(marker) {
            let rest = &line[pos + marker.len()..];
            if !rest.starts_with([' ', '\t']) {
                continue;
            }
            let Some(patterns) = parse_go_embed_patterns(rest) else {
                return true;
            };
            for pat in patterns {
                if pat == "." {
                    return true;
                }
                let Ok(glob) = GlobBuilder::new(pat).literal_separator(true).build() else {
                    return true;
                };
                if doc_suffixes_match(&glob.compile_matcher(), doc) {
                    return true;
                }
            }
        }
    }
    false
}

/// True when a lowercased grep hunk (marker + context) may pull the lowercased
/// `doc` into a build or a test run. Paths the hunk cannot spell out
/// (`concat!`, macro `$var`, `"."`, the crate root) count as embedding.
fn block_embeds(block: &str, doc: &str) -> bool {
    let has = |needle: &str| block.contains(needle);
    let has_include_dir = has(concat!("include_", "dir!"));
    let has_go_embed = has(concat!("go:", "embed"));
    let mut parts = doc.rsplit('/');
    let file = parts.next().unwrap_or(doc);
    let dirs: Vec<&str> = parts.filter(|dir| !dir.is_empty()).collect();
    if has(file)
        || (has_include_dir && dirs.iter().any(|dir| dir_referenced(block, dir)))
        || (has_go_embed && go_embed_matches_doc(block, doc))
    {
        return true;
    }
    let dynamic = has("concat!(") || has("$");
    let ext = file.rsplit_once('.').map(|(_, ext)| format!(".{ext}"));
    ((has(concat!("include_", "str!")) || has(concat!("include_", "bytes!"))) && dynamic)
        || (has_include_dir
            && (dynamic
                || has("\".\"")
                || has("\"./\"")
                || has("\"..\"")
                || has("\"../\"")
                || has("/..\"")
                || has("/../\"")
                || has("cargo_manifest_dir")))
        || (has(concat!("doc", "test")) && ext.as_deref().is_some_and(has))
        || (has("sphinx-build") && has(concat!("doc", "test")))
        || has("mdbook")
        || has(concat!("doc_comment::doc", "test!"))
}

/// [`is_docs_only`], plus a scan of the tracked non-doc files of `repo_root`:
/// a changed doc named on an embed or doc-test line is not docs-only. A scan
/// git cannot answer is taken as "embedded" (fail-closed: tests still run).
pub fn is_docs_only_in_repo(
    repo_root: &Path,
    changes: &[Change],
    head_lock_globs: &[String],
    exclude_globs: &[String],
    timeout: Duration,
) -> Result<bool, GateError> {
    if !is_docs_only(changes, head_lock_globs, exclude_globs)? {
        return Ok(false);
    }
    // `-n` makes the hunk separator `--` unambiguous (content lines carry `N:`
    // or `N-`); `-h` keeps the source file's own path out of the hunk text.
    let mut args = vec!["grep", "-h", "-n", "-I", "-i", "--no-color", "-A", "4"];
    args.extend(["-E", "-e", EMBED_PATTERN, "--"]);
    let excludes: Vec<String> = ALLOWED_EXTS
        .iter()
        .map(|ext| format!(":(exclude,icase)*.{ext}"))
        .collect();
    args.extend(excludes.iter().map(String::as_str));
    let hits = match aaf_fp::git_or_none(repo_root, &args, timeout) {
        Ok(Some(out)) => out,
        Ok(None) => return Ok(true),
        Err(_) => return Ok(false),
    };
    let docs: Vec<String> = changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.old_path.as_deref()))
        .map(str::to_ascii_lowercase)
        .collect();
    let hits = hits.to_ascii_lowercase();
    let mut blocks = hits.split("\n--\n");
    Ok(!blocks.any(|block| docs.iter().any(|doc| block_embeds(block, doc))))
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

    /// Temp repo committing `files`. Fixtures spell most macro names from
    /// pieces or placeholders, so few lines of this source read as embeds.
    fn repo_with(files: &[(&str, String)]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for (path, body) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "i",
            ],
        ] {
            aaf_fp::git(dir.path(), args, &[], Duration::from_secs(30)).unwrap();
        }
        dir
    }

    fn in_repo(dir: &Path, path: &str) -> bool {
        let globs = GateConfig::default().head_lock_globs;
        let change = ch(ChangeKind::Modified, path, None);
        is_docs_only_in_repo(dir, &[change], &globs, &[], Duration::from_secs(30)).unwrap()
    }

    #[test]
    fn docs_in_repo_blocks_embedded_docs_and_keeps_plain_docs_fast() {
        let (inc, dt) = (concat!("include", "_str!"), concat!("doc", "test"));
        let repo = repo_with(&[
            ("README.md", "# r\n".into()),
            ("docs/guide.md", "# g\n".into()),
            ("book/ch1.rst", "c\n".into()),
            ("src/lib.rs", format!("#![doc = {inc}(\"../README.md\")]\n")),
            (
                "src/data.rs",
                format!("const D: &str = {inc}(\"notes.txt\");\n"),
            ),
            ("pytest.ini", format!("addopts = --{dt}-glob=*.rst\n")),
        ]);
        assert!(!in_repo(repo.path(), "README.md"), "embedded via macro");
        assert!(!in_repo(repo.path(), "book/ch1.rst"), "test glob target");
        assert!(
            in_repo(repo.path(), "docs/guide.md"),
            "plain doc stays fast"
        );
        assert!(!in_repo(repo.path(), "templates/skills/x/SKILL.md"));
        let not_git = tempfile::TempDir::new().unwrap();
        assert!(!in_repo(not_git.path(), "docs/guide.md"), "fail-closed");
    }

    /// Rows: (tracked non-doc body, changed doc, may take fast path), one repo
    /// each. `@` and `%` stand for the include-str/include-dir macros.
    #[test]
    fn docs_in_repo_blocks_wrapped_macro_cased_and_whole_dir_embeds() {
        let rows = [
            ("#![doc = @(\n    \"../docs/g.md\"\n)]", "docs/g.md", false),
            (
                concat!("doc_comment::doc", "test!(\n    \"../g.md\"\n);"),
                "g.md",
                false,
            ),
            (
                "($n:literal) => { @(concat!(\"fx/\", $n)) };",
                "docs/a.md",
                false,
            ),
            ("($p:expr) => { @($p) };", "docs/a.md", false),
            ("($p:expr) => { %($p) };", "docs/a.md", false),
            (
                concat!("doc", "test_glob = [\n    \"*.rst\",\n]"),
                "book/a.rst",
                false,
            ),
            (
                concat!("sphinx-build -b doc", "test src out"),
                "docs/a.md",
                false,
            ),
            (concat!("md", "book  test"), "docs/a.md", false),
            ("#![doc = @(\"../readme.md\")]", "README.md", false),
            (concat!("doc", "test_glob = *.rst"), "docs/GUIDE.RST", false),
            ("static D: Dir = %(\".\");", "README.md", false),
            (
                "static D: Dir = %(\"$CARGO_MANIFEST_DIR\");",
                "docs/g.md",
                false,
            ),
            ("static D: Dir = %(\"docs\");", "docs/g.md", false),
            ("static D: Dir = %(\"./\");", "README.md", false),
            (
                concat!("include_", "bytes!(concat!(\"fx/\", \"b\"))"),
                "docs/a.md",
                false,
            ),
            (
                concat!("doc_comment::doc", "test!(\n    \"../x.txt\"\n);"),
                "g.md",
                false,
            ),
            (
                "#![doc = @(\"../README.md\")]\n\n\n\n\nfn f() {}",
                "docs/g.md",
                true,
            ),
            ("static D: Dir = %(\"web\");", "docs/g.md", true),
            ("#![doc = @(\"../docs/other.md\")]", "docs/g.md", true),
            ("static D: Dir = %(\"mydocs\");", "docs/g.md", true),
        ];
        let wrong: Vec<_> = rows
            .iter()
            .filter(|(body, doc, fast)| {
                let body = body
                    .replace('@', concat!("include_", "str!"))
                    .replace('%', concat!("include_", "dir!"));
                in_repo(repo_with(&[("t.cfg", body)]).path(), doc) != *fast
            })
            .collect();
        assert!(wrong.is_empty(), "wrong verdict: {wrong:#?}");
    }

    #[test]
    fn docs_in_repo_blocks_go_embed_tab_backtick_and_quoted_wildcards() {
        let marker = concat!("//go:", "embed");
        let rows = [
            (format!("{marker}\tdocs\n"), "docs/g.md", false),
            (format!("{marker} `docs`\n"), "docs/g.md", false),
            (format!("{marker} `docs/sub`\n"), "docs/sub/g.md", false),
            (format!("{marker}\t*\n"), "docs/g.md", false),
            (format!("{marker} \"*\"\n"), "docs/g.md", false),
            (format!("{marker} `*`\n"), "docs/g.md", false),
            (format!("{marker}\tall:*\n"), "docs/g.md", false),
            (format!("{marker} \"all:*\"\n"), "docs/g.md", false),
            (format!("{marker} `all:*`\n"), "docs/g.md", false),
            (format!("{marker}\tweb\n"), "docs/g.md", true),
            (format!("{marker} `web`\n"), "docs/g.md", true),
        ];
        for (body, doc, fast) in rows {
            let repo = repo_with(&[("embed.go", format!("package p\n{body}var f string\n"))]);
            assert_eq!(in_repo(repo.path(), doc), fast, "body={body:?}, doc={doc}");
        }
    }
}
