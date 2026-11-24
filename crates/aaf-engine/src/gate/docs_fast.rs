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

/// Extensions (lowercase) of code that can open a doc by name while a test
/// runs (`read_to_string(format!("{}/GUIDE.md", env!("CARGO_MANIFEST_DIR")))`).
/// Manifests, data and site configs (`Cargo.toml` `readme = "README.md"`,
/// `package.json`, `mkdocs.yml`) name docs without running anything and are
/// left out: with them nearly no README edit would stay fast.
const CODE_EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "mjs", "cjs", "ts", "mts", "cts", "tsx", "jsx", "go", "java", "kt",
    "kts", "scala", "groovy", "gradle", "swift", "m", "mm", "c", "h", "cc", "cpp", "cxx", "hpp",
    "cs", "fs", "vb", "rb", "php", "pl", "pm", "lua", "dart", "ex", "exs", "erl", "clj", "r", "jl",
    "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd",
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
        || (has("sphinx") && has(concat!("doc", "test")))
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
    let docs = lowered_docs(changes);
    let (code, runner) = (code_words(&docs, false), code_words(&docs, true));
    let code_specs: Vec<String> = CODE_EXTS
        .iter()
        .map(|ext| format!(":(icase)*.{ext}"))
        .collect();
    let code_specs: Vec<&str> = code_specs.iter().map(String::as_str).collect();
    // The scans are independent reads of the index: run them at once.
    // A scan that fails, or a worker that panics, keeps the tests running.
    let (embedded, named) = std::thread::scope(|scope| {
        let named = scope.spawn(|| named_in(repo_root, &code, &code_specs, timeout));
        let in_runner = scope.spawn(|| named_in(repo_root, &runner, RUNNER_SPECS, timeout));
        let embedded = match aaf_fp::git_or_none(repo_root, &args, timeout) {
            Ok(Some(hits)) => {
                let hits = hits.to_ascii_lowercase();
                let mut blocks = hits.split("\n--\n");
                blocks.any(|block| docs.iter().any(|doc| block_embeds(block, doc)))
            }
            Ok(None) => false,
            Err(_) => true,
        };
        // Join both before combining: a `||` would leave a panicked runner
        // scan to the scope's own join, which re-panics instead of failing
        // closed.
        let (in_code, in_runner) = (
            named.join().unwrap_or(true),
            in_runner.join().unwrap_or(true),
        );
        let named = in_code || in_runner;
        (embedded, named)
    });
    Ok(!embedded && !named)
}

/// Runners (`Makefile`, `GNUmakefile`, `*.mk`, `justfile`, `Rakefile`,
/// `*.rake`, `Makefile.toml`, `Taskfile.yml`) that run commands reading a doc
/// as surely as a script does.
const RUNNER_SPECS: &[&str] = &[
    ":(icase)*makefile",
    ":(icase)*.mk",
    ":(icase)*justfile",
    ":(icase)*rakefile",
    ":(icase)*.rake",
    ":(icase)*makefile.toml",
    ":(icase)*taskfile.yml",
    ":(icase)*taskfile.yaml",
];

/// Changed paths (old names of renames too), lowercased.
fn lowered_docs(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.old_path.as_deref()))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// True when a configured command (`lint`, `test_scoped`) spells a
/// [`code_words`] word of a changed doc or one of its parent directories
/// alone, as a whole word in any ASCII case (`! grep -rq TODO docs`,
/// `markdownlint '**/*.md'`): run, it may read the doc (audit a13 R3-L3-2).
pub fn commands_name_docs(commands: &[&str], changes: &[Change]) -> bool {
    let words = code_words(&lowered_docs(changes), true);
    let word_byte = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    commands.iter().any(|command| {
        let command = command.to_ascii_lowercase();
        let bytes = command.as_bytes();
        words.iter().any(|word| {
            command.match_indices(word.as_str()).any(|(at, _)| {
                let before = at.checked_sub(1).and_then(|i| bytes.get(i));
                !word_byte(before) && !word_byte(bytes.get(at + word.len()))
            })
        })
    })
}

/// Words (`-w -F -i`) that a tracked code file or runner spells when a test
/// may read a changed doc: its file name, its extension standing alone with
/// or without the dot (`glob("*.<ext>")`, `endswith(".<ext>")`,
/// `extension() == Some("<ext>")`, `**/*.{<ext>,x}`), and each parent
/// directory closed by a quote or a slash (`Path("<dir>")`,
/// `read_dir("../<dir>/")`, `<dir>/*.<ext>` in a runner) or, with
/// `bare_dirs` (runners, commands), alone (`markdownlint <dir>`).
///
/// Trade-off (audit a13 R2-L3-1): one code file that spells the extension
/// alone (a test fixture string `"*.<ext>"`, say) takes every doc of that
/// extension in the repo off the fast path, and one that names a directory
/// does so for every doc under it. A missed reader would let a doc edit skip
/// the very test that reads it; a slow docs edit only costs time. The bare
/// extension (audit a13 R3-L3-1) makes this so for any code that spells it.
fn code_words(docs: &[String], bare_dirs: bool) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for doc in docs {
        let mut parts = doc.rsplit('/').filter(|part| !part.is_empty());
        let Some(name) = parts.next() else {
            continue;
        };
        words.push(name.to_string());
        if let Some((_, ext)) = name.rsplit_once('.') {
            words.extend([format!(".{ext}"), ext.to_string()]);
        }
        for dir in parts {
            words.extend(["\"", "'", "/"].map(|end| format!("{dir}{end}")));
            if bare_dirs {
                words.push(dir.to_string());
            }
        }
    }
    words.sort();
    words.dedup();
    words
}

/// True when a tracked file under `specs` (code: [`CODE_EXTS`], or
/// [`RUNNER_SPECS`]) spells one of `words` ([`code_words`] of the changed
/// docs): a test may open a doc at run time, however the path is put
/// together. A scan git cannot answer counts as named.
fn named_in(repo_root: &Path, words: &[String], specs: &[&str], timeout: Duration) -> bool {
    if words.is_empty() {
        return false;
    }
    let mut args = vec!["grep", "-l", "-I", "-i", "-w", "-F", "--no-color"];
    for word in words {
        args.extend(["-e", word.as_str()]);
    }
    args.push("--");
    args.extend(specs);
    !matches!(aaf_fp::git_or_none(repo_root, &args, timeout), Ok(None))
}

#[cfg(test)]
mod tests;
