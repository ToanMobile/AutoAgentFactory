//! `aaf-hook`: reads one PreToolUse payload on stdin, prints the decision.
//! Exit code is 0 (allow) or 2 (deny), never anything else.

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
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
