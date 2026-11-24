//! `aaf-hook`: reads one PreToolUse payload on stdin, prints the decision.
//! Exit code is 0 (allow) or 2 (deny), never anything else.

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use aaf_hook::{HookEnv, Outcome};

/// Larger than any real tool call; bounds memory on a runaway writer.
const MAX_INPUT: u64 = 16 * 1024 * 1024;
/// Upper bound on git config / pointer files read by the pure-Rust fast path.
const MAX_GIT_CONFIG_BYTES: u64 = 64 * 1024;

fn dir_var(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn read_bounded(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_GIT_CONFIG_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_GIT_CONFIG_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Strips `#`/`;` comments outside double quotes. Returns `None` on an unclosed
/// quote or a trailing backslash line continuation so callers fail closed.
fn strip_ini_comment(line: &str) -> Option<&str> {
    let mut in_quote = false;
    let mut escaped = false;
    for (idx, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_quote = !in_quote;
        } else if !in_quote && matches!(c, '#' | ';') {
            return Some(&line[..idx]);
        }
    }
    (!in_quote && !escaped).then_some(line)
}

fn config_is_simple(path: &Path) -> bool {
    let Some(text) = read_bounded(path) else {
        return false;
    };
    let mut in_core: Option<bool> = None;
    for raw_line in text.lines() {
        let Some(clean) = strip_ini_comment(raw_line.trim_end_matches('\r')) else {
            return false;
        };
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let Some(inner) = rest.strip_suffix(']') else {
                return false;
            };
            let sec_end = inner
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(inner.len());
            let sec = &inner[..sec_end];
            if sec.is_empty() {
                return false;
            }
            // Reject multiple headers or extra tokens after an unquoted `]`.
            let (mut in_q, mut esc) = (false, false);
            for c in inner[sec_end..].chars() {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_q = !in_q;
                } else if !in_q && c == ']' {
                    return false;
                }
            }
            if sec.eq_ignore_ascii_case("include")
                || sec.eq_ignore_ascii_case("includeif")
                || sec.eq_ignore_ascii_case("extensions")
            {
                return false;
            }
            in_core = Some(sec.eq_ignore_ascii_case("core"));
        } else {
            let Some(is_core) = in_core else {
                return false;
            };
            let (key, val) = match line.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (line, None),
            };
            if key.is_empty() || key.contains(char::is_whitespace) {
                return false;
            }
            if is_core {
                if key.eq_ignore_ascii_case("worktree") {
                    return false;
                }
                if key.eq_ignore_ascii_case("bare") {
                    let is_false = val.is_some_and(|v| {
                        v.eq_ignore_ascii_case("false")
                            || v.eq_ignore_ascii_case("no")
                            || v.eq_ignore_ascii_case("off")
                            || v == "0"
                    });
                    if !is_false {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Pure-Rust fast path for discovering a repository's work-tree root without
/// spawning `git`. Returns `None` on any edge case so the caller falls back to
/// `aaf_fp::repo_root`.
fn fast_repo_root(cwd: &Path) -> Option<PathBuf> {
    if env::var_os("GIT_CEILING_DIRECTORIES").is_some()
        || env::var_os("GIT_DIR").is_some()
        || env::var_os("GIT_WORK_TREE").is_some()
    {
        return None;
    }
    let canon = fs::canonicalize(cwd).ok()?;
    if canon.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    for top in canon.ancestors() {
        let dot_git = top.join(".git");
        let meta = match fs::symlink_metadata(&dot_git) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        if meta.file_type().is_symlink() {
            return None;
        }
        let (git_dir, is_linked) = if meta.file_type().is_dir() {
            (dot_git, false)
        } else if meta.file_type().is_file() {
            let text = read_bounded(&dot_git)?;
            let raw = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            if raw.is_empty() {
                return None;
            }
            let target = Path::new(raw);
            let resolved = if target.is_absolute() {
                target.to_path_buf()
            } else {
                top.join(target)
            };
            (resolved, true)
        } else {
            return None;
        };
        if !fs::symlink_metadata(git_dir.join("HEAD")).is_ok_and(|m| m.file_type().is_file()) {
            return None;
        }
        match fs::symlink_metadata(git_dir.join("config.worktree")) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            _ => return None,
        }
        let commondir_path = git_dir.join("commondir");
        let has_commondir = match fs::symlink_metadata(&commondir_path) {
            Ok(m) => {
                if !m.file_type().is_file() {
                    return None;
                }
                let raw = read_bounded(&commondir_path)?;
                let rel = raw.lines().next()?.trim();
                if rel.is_empty() {
                    return None;
                }
                let common = if Path::new(rel).is_absolute() {
                    PathBuf::from(rel)
                } else {
                    git_dir.join(rel)
                };
                if !config_is_simple(&common.join("config")) {
                    return None;
                }
                true
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(_) => return None,
        };
        let local_config = git_dir.join("config");
        match fs::symlink_metadata(&local_config) {
            Ok(_) => {
                if !config_is_simple(&local_config) {
                    return None;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound && is_linked && has_commondir => {}
            _ => return None,
        }
        return Some(top.to_path_buf());
    }
    None
}

fn judge() -> Outcome {
    let cli_args: Vec<String> = env::args().skip(1).collect();
    let host = match aaf_hook::host::parse_cli_host(&cli_args) {
        Ok(host) => host,
        Err(msg) => {
            return aaf_hook::host::render_for_host(
                &aaf_hook::Decision::Deny(aaf_hook::Denial::new(
                    "hook_input",
                    msg,
                    "invoke `aaf-hook` with optional `--host <auto|claude|codex|gemini>`",
                )),
                aaf_hook::host::HookHost::Claude,
            );
        }
    };
    let mut input = Vec::new();
    let read = io::stdin()
        .lock()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input);
    if read.is_err() || input.len() as u64 > MAX_INPUT {
        // Not a payload we can trust: let `decide` refuse it as bad input.
        input.clear();
    }
    let home = dir_var("HOME");
    let repo_cache: RefCell<HashMap<PathBuf, Option<PathBuf>>> = RefCell::new(HashMap::new());
    let config_cache: RefCell<HashMap<PathBuf, Option<String>>> = RefCell::new(HashMap::new());
    let lookup = |cwd: &Path| -> Option<PathBuf> {
        if let Some(cached) = repo_cache.borrow().get(cwd) {
            return cached.clone();
        }
        let found = fast_repo_root(cwd).or_else(|| aaf_fp::repo_root(cwd).ok());
        repo_cache
            .borrow_mut()
            .insert(cwd.to_path_buf(), found.clone());
        found
    };
    let config = |cwd: &Path| -> Option<String> {
        if let Some(cached) = config_cache.borrow().get(cwd) {
            return cached.clone();
        }
        let found = aaf_fp::git(cwd, &["config", "--list", "-z"], &[], aaf_fp::GIT_TIMEOUT).ok();
        config_cache
            .borrow_mut()
            .insert(cwd.to_path_buf(), found.clone());
        found
    };
    let hook_env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: aaf_fp::data_dir().ok(),
        config_dir: dir_var("AAF_CONFIG_DIR")
            .or_else(|| home.as_ref().map(|h| h.join(".config/autofactory"))),
        home,
        cwd: env::current_dir().ok(),
    };
    aaf_hook::host::run_for_host(&input, &hook_env, host)
}

fn main() {
    // The deny reason is the only output a panic may produce.
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = aaf_hook::guarded(judge);
    // A closed pipe must not change the exit code: the code is the verdict.
    let _ = io::stdout().write_all(outcome.stdout.as_bytes());
    if !outcome.stderr.is_empty() {
        let _ = io::stderr().write_all(outcome.stderr.as_bytes());
        let _ = io::stderr().write_all(b"\n");
    }
    std::process::exit(outcome.code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fast_repo_root_normal_repo_and_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            repo.join(".git/config"),
            "[core]\n\
             \trepositoryformatversion = 0\n\
             \tfilemode = true\n\
             \tbare = false\n\
             \tlogallrefupdates = true\n\
             # comment mentioning worktree and include\n\
             [branch \"feat/worktree-include\"]\n\
             \tremote = origin\n\
             \tmerge = refs/heads/feat/worktree-include\n",
        )
        .unwrap();
        let sub = repo.join("a/b");
        fs::create_dir_all(&sub).unwrap();

        assert_eq!(fast_repo_root(&repo), Some(repo.clone()));
        assert_eq!(fast_repo_root(&sub), Some(repo.clone()));
        // Inside `.git` must fall back.
        assert_eq!(fast_repo_root(&repo.join(".git")), None);
    }

    #[test]
    fn fast_repo_root_falls_back_on_bare_include_worktree_or_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for (name, cfg) in [
            ("bare_repo", "[core]\n\tbare = true\n"),
            ("bare_implicit", "[core]\n\tbare\n"),
            ("bare_yes", "[core]\n\tbare = yes\n"),
            ("inc_repo", "[include]\n\tpath = ~/.gitconfig\n"),
            ("incif_repo", "[includeIf \"gitdir:~/x/\"]\n\tpath = x\n"),
            ("ext_repo", "[extensions]\n\tworktreeConfig = true\n"),
            ("wt_repo", "[core]\n\tworktree = /tmp/other\n"),
            ("cont_repo", "[core]\n\twork\\\n\ttree = /tmp/other\n"),
            ("inline_sec", "[core] bare = true\n"),
        ] {
            let repo = base.join(name);
            fs::create_dir_all(repo.join(".git")).unwrap();
            fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::write(repo.join(".git/config"), cfg).unwrap();
            assert_eq!(fast_repo_root(&repo), None, "expected fallback for {name}");
        }
        #[cfg(unix)]
        {
            let real_git = base.join("real_git");
            fs::create_dir_all(&real_git).unwrap();
            fs::write(real_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::write(real_git.join("config"), "[core]\n").unwrap();
            let sym_repo = base.join("sym_repo");
            fs::create_dir_all(&sym_repo).unwrap();
            std::os::unix::fs::symlink(&real_git, sym_repo.join(".git")).unwrap();
            assert_eq!(fast_repo_root(&sym_repo), None);
        }
    }

    #[test]
    fn fast_repo_root_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let main_git = base.join("main/.git");
        let wt_git = main_git.join("worktrees/wt1");
        let wt_dir = base.join("wt1");
        fs::create_dir_all(&wt_git).unwrap();
        fs::create_dir_all(&wt_dir).unwrap();
        fs::write(
            main_git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", wt_git.display()),
        )
        .unwrap();

        assert_eq!(fast_repo_root(&wt_dir), Some(wt_dir.clone()));

        // Adding `include` to commondir/config forces fallback.
        fs::write(main_git.join("config"), "[include]\n\tpath = x\n").unwrap();
        assert_eq!(fast_repo_root(&wt_dir), None);
    }
}
