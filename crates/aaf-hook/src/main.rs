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

fn dir_var(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn is_in_temp_dir(path: &Path) -> bool {
    let norm = aaf_hook::paths::normalize(path);
    let check = |p: &Path| {
        p == Path::new("/tmp")
            || p.starts_with("/tmp")
            || p == Path::new("/private/tmp")
            || p.starts_with("/private/tmp")
    };
    if check(&norm) {
        return true;
    }
    if let Some(tmpdir) = dir_var("TMPDIR") {
        let tmp_norm = aaf_hook::paths::normalize(&tmpdir);
        let tmp_real = aaf_hook::paths::real(&tmp_norm);
        if norm == tmp_norm
            || norm.starts_with(&tmp_norm)
            || norm == tmp_real
            || norm.starts_with(&tmp_real)
        {
            return true;
        }
    }
    false
}

/// Discovers trusted absolute paths for the sibling `aaf` CLI installed next to
/// this `aaf-hook` binary. Never trusts paths inside `/tmp` or `$TMPDIR`.
fn discover_trusted_aaf_bins() -> Vec<PathBuf> {
    let Ok(exe) = env::current_exe() else {
        return Vec::new();
    };
    let bin_name = if cfg!(windows) { "aaf.exe" } else { "aaf" };
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if path.is_absolute() && !is_in_temp_dir(&path) && !out.contains(&path) {
            out.push(path);
        }
    };

    let raw_sibling = exe.parent().map(|dir| dir.join(bin_name));
    let canon_exe_sibling = fs::canonicalize(&exe)
        .ok()
        .and_then(|c| c.parent().map(|dir| dir.join(bin_name)));
    let canon_sibling = raw_sibling
        .as_deref()
        .and_then(|p| fs::canonicalize(p).ok())
        .or_else(|| {
            canon_exe_sibling
                .as_deref()
                .and_then(|p| fs::canonicalize(p).ok())
        });

    if let Some(canon) = &canon_sibling {
        if is_in_temp_dir(canon) {
            return Vec::new();
        }
        if let Some(raw) = raw_sibling {
            push_unique(aaf_hook::paths::normalize(&raw));
            push_unique(raw);
        }
        if let Some(exe_sib) = canon_exe_sibling {
            push_unique(exe_sib);
        }
        push_unique(canon.clone());
        if let Some(path_var) = env::var_os("PATH") {
            for dir in env::split_paths(&path_var) {
                let cand = dir.join(bin_name);
                if cand.is_absolute()
                    && !is_in_temp_dir(&cand)
                    && fs::canonicalize(&cand).ok().as_deref() == Some(canon.as_path())
                {
                    push_unique(aaf_hook::paths::normalize(&cand));
                    push_unique(cand);
                }
            }
        }
    } else if let Some(raw) = raw_sibling {
        push_unique(aaf_hook::paths::normalize(&raw));
        push_unique(raw);
    }
    out
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
        let found = aaf_fp::repo_root(cwd).ok();
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
        in_factory_run: env::var_os("AAF_RUN_ID").is_some_and(|v| !v.is_empty()),
        trusted_aaf_bins: discover_trusted_aaf_bins(),
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
