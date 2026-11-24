//! Static search/filter argument exemptions for `data_guard` (`NEEDLES`-only).

use crate::shell::Cmd;

/// Marks static pattern arguments of grep-like commands (`rg`, `grep`, `egrep`,
/// `fgrep`, `git grep`) for `NEEDLES`-only exemption. File options (`-f`/`--file`)
/// and path operands are NEVER marked.
fn mark_grep_patterns(
    cmd: &Cmd,
    start: usize,
    short_valued: &str,
    long_valued: &[&str],
    double_dash_ends_patterns: bool,
    exempt: &mut [bool],
) {
    let is_dyn = |i: usize| cmd.arg_dynamic.get(i).copied().unwrap_or(true);
    let len = cmd.words.len();
    let mut has_explicit_pattern = false;
    let mut pattern_found = false;
    let mut after_dd = false;
    let mut i = start;
    while i < len {
        let word = cmd.words[i].as_str();
        if !after_dd && word == "--" {
            if double_dash_ends_patterns {
                break;
            }
            after_dd = true;
            i += 1;
            continue;
        }
        if !after_dd && let Some(long) = word.strip_prefix("--") {
            if long.starts_with("regexp=") {
                has_explicit_pattern = true;
                if !is_dyn(i) {
                    exempt[i] = true;
                }
                i += 1;
            } else if long == "regexp" {
                has_explicit_pattern = true;
                if !is_dyn(i) && i + 1 < len && !is_dyn(i + 1) {
                    exempt[i + 1] = true;
                }
                i += 2;
            } else if long.starts_with("file=") {
                has_explicit_pattern = true;
                i += 1;
            } else if long == "file" {
                has_explicit_pattern = true;
                i += 2;
            } else if !long.contains('=') && long_valued.contains(&word) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if !after_dd && word.starts_with('-') && word.len() > 1 {
            let mut chars = word[1..].chars();
            let mut step = 1;
            while let Some(c) = chars.next() {
                let inline = !chars.as_str().is_empty();
                if c == 'e' {
                    has_explicit_pattern = true;
                    if inline {
                        if !is_dyn(i) {
                            exempt[i] = true;
                        }
                    } else {
                        if !is_dyn(i) && i + 1 < len && !is_dyn(i + 1) {
                            exempt[i + 1] = true;
                        }
                        step = 2;
                    }
                    break;
                } else if c == 'f' {
                    has_explicit_pattern = true;
                    if !inline {
                        step = 2;
                    }
                    break;
                } else if short_valued.contains(c) {
                    if !inline {
                        step = 2;
                    }
                    break;
                }
            }
            i += step;
            continue;
        }
        if !has_explicit_pattern && !pattern_found {
            pattern_found = true;
            if !is_dyn(i) {
                exempt[i] = true;
            }
        }
        i += 1;
    }
}

/// Mask parallel to `cmd.words` marking static (non-dynamic) search/filter
/// arguments on read-only search/test commands (`rg`, `grep`, `egrep`, `fgrep`,
/// `git grep`, `git log`, `cargo test`) that are exempt from `NEEDLES` only.
/// Path operands, `-f`/`--file` operands, redirects, env expansions, and any
/// token matching `ctx.data_dirs` or resolving inside it remain blocked.
pub(super) fn search_needle_exempt_mask(cmd: &Cmd) -> Option<Vec<bool>> {
    if cmd.dynamic_name || cmd.xargs {
        return None;
    }
    let is_dyn = |i: usize| cmd.arg_dynamic.get(i).copied().unwrap_or(true);
    let len = cmd.words.len();
    match cmd.name() {
        "rg" => {
            let mut exempt = vec![false; len];
            mark_grep_patterns(
                cmd,
                1,
                "ABCEMTdgjmrt",
                &[
                    "--after-context",
                    "--before-context",
                    "--context",
                    "--glob",
                    "--iglob",
                    "--type",
                    "--type-not",
                    "--max-count",
                    "--max-depth",
                    "--max-columns",
                    "--encoding",
                    "--threads",
                    "--color",
                    "--sort",
                    "--sortr",
                ],
                false,
                &mut exempt,
            );
            Some(exempt)
        }
        "grep" | "egrep" | "fgrep" => {
            let mut exempt = vec![false; len];
            mark_grep_patterns(
                cmd,
                1,
                "mABCdD",
                &[
                    "--max-count",
                    "--after-context",
                    "--before-context",
                    "--include",
                    "--exclude",
                    "--exclude-dir",
                    "--directories",
                    "--devices",
                    "--label",
                ],
                false,
                &mut exempt,
            );
            Some(exempt)
        }
        "git" => {
            let call = crate::git_guard::split(cmd.args());
            let start = 2 + call.globals.len();
            match call.sub {
                "grep" => {
                    let mut exempt = vec![false; len];
                    mark_grep_patterns(
                        cmd,
                        start,
                        "ABCm",
                        &[
                            "--after-context",
                            "--before-context",
                            "--context",
                            "--max-count",
                            "--max-depth",
                            "--threads",
                        ],
                        true,
                        &mut exempt,
                    );
                    Some(exempt)
                }
                "log" => {
                    let mut exempt = vec![false; len];
                    let mut i = start;
                    while i < len {
                        let word = cmd.words[i].as_str();
                        if word == "--" {
                            break;
                        }
                        if matches!(word, "-S" | "-G" | "--grep" | "--author" | "--committer") {
                            if !is_dyn(i) && i + 1 < len && !is_dyn(i + 1) {
                                exempt[i + 1] = true;
                            }
                            i += 2;
                        } else if (word.starts_with("-S") && word.len() > 2)
                            || (word.starts_with("-G") && word.len() > 2)
                            || word.starts_with("--grep=")
                            || word.starts_with("--author=")
                            || word.starts_with("--committer=")
                        {
                            if !is_dyn(i) {
                                exempt[i] = true;
                            }
                            i += 1;
                        } else {
                            i += 1;
                        }
                    }
                    Some(exempt)
                }
                _ => None,
            }
        }
        "cargo" => {
            let mut i = 1;
            let test_start = loop {
                let word = cmd.words.get(i)?.as_str();
                if word.starts_with('+') {
                    i += 1;
                } else if matches!(word, "-C" | "--config" | "--color" | "-Z") {
                    i += 2;
                } else if word.starts_with('-') {
                    i += 1;
                } else if matches!(word, "test" | "t") {
                    break i + 1;
                } else {
                    return None;
                }
            };
            let mut exempt = vec![false; len];
            let mut after_dd = false;
            let mut at = test_start;
            let is_safe_filter =
                |idx: usize, val: &str| !is_dyn(idx) && !val.contains('/') && !val.contains('\\');
            while at < len {
                let word = cmd.words[at].as_str();
                if !after_dd && word == "--" {
                    after_dd = true;
                    at += 1;
                    continue;
                }
                if !after_dd {
                    if matches!(
                        word,
                        "-p" | "--package"
                            | "--bin"
                            | "--example"
                            | "--test"
                            | "--bench"
                            | "--manifest-path"
                            | "--target"
                            | "--target-dir"
                            | "--profile"
                            | "-F"
                            | "--features"
                            | "-j"
                            | "--jobs"
                            | "--color"
                            | "--message-format"
                            | "--config"
                            | "-Z"
                    ) {
                        at += 2;
                    } else if word.starts_with('-') {
                        at += 1;
                    } else {
                        if is_safe_filter(at, word) {
                            exempt[at] = true;
                        }
                        at += 1;
                    }
                } else if word == "--skip" {
                    if !is_dyn(at)
                        && at + 1 < len
                        && is_safe_filter(at + 1, cmd.words[at + 1].as_str())
                    {
                        exempt[at + 1] = true;
                    }
                    at += 2;
                } else if let Some(val) = word.strip_prefix("--skip=") {
                    if is_safe_filter(at, val) {
                        exempt[at] = true;
                    }
                    at += 1;
                } else if matches!(
                    word,
                    "--test-threads"
                        | "--color"
                        | "--format"
                        | "--logfile"
                        | "--report-time"
                        | "--shuffle-seed"
                ) {
                    at += 2;
                } else if word.starts_with('-') {
                    at += 1;
                } else {
                    if is_safe_filter(at, word) {
                        exempt[at] = true;
                    }
                    at += 1;
                }
            }
            Some(exempt)
        }
        _ => None,
    }
}
