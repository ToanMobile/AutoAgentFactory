//! Local settings checks, program resolution, and worktree/base hook
//! verification for [`super`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{
    HOOK_BINARY, LOCAL_SETTINGS_REL, SETTINGS_REL, covering_hook, forbidden_env,
    hooks_switched_off, parse, sole_program,
};

/// Why the local settings in `text` take the registration away again, or
/// `Ok`. A file that cannot be read as settings fails closed.
pub fn check_local(text: &str) -> Result<(), String> {
    let root = parse(text)?;
    if let Some(problem) = hooks_switched_off(&root) {
        return Err(format!("it sets {problem}"));
    }
    if let Some(hooks) = root.get("hooks") {
        match hooks.as_object() {
            Some(map) if !map.keys().any(|k| k.eq_ignore_ascii_case("PreToolUse")) => {}
            Some(_) => {
                return Err(
                    "it sets \"hooks.PreToolUse\", which overrides the hook registration"
                        .to_string(),
                );
            }
            None => {
                return Err(
                    "it sets \"hooks\" to a non-object value, which overrides the hook registration"
                        .to_string(),
                );
            }
        }
    }
    if let Some(problem) = forbidden_env(&root) {
        return Err(format!("it sets {problem}"));
    }
    Ok(())
}

/// Removes an empty `"hooks": {}` key from a JSON object settings file,
/// returning the updated pretty-printed JSON with a trailing newline.
/// Returns `None` when no cleanup is needed or the text is not a JSON object.
pub fn strip_empty_hooks_object(text: &str) -> Option<String> {
    let mut root: Value = serde_json::from_str(text).ok()?;
    let map = root.as_object_mut()?;
    let is_empty_hooks = map
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty);
    if !is_empty_hooks {
        return None;
    }
    map.remove("hooks");
    let mut out = serde_json::to_string_pretty(&root).ok()?;
    out.push('\n');
    Some(out)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The file the registered `program` runs for an agent whose PATH is
/// `path_var`, or why it would not run. A hook that cannot be started ends
/// with exit 127, and Claude Code treats only exit 2 as a block: the agent
/// would go on unguarded. `outside` is the agent's worktree: a program in
/// there is the agent's to replace.
pub fn resolve_program(
    program: &str,
    path_var: &OsStr,
    outside: Option<&Path>,
) -> Result<PathBuf, String> {
    let candidate = Path::new(program);
    let found = if candidate.is_absolute() {
        if !is_executable(candidate) {
            return Err(format!("{program} is missing or not executable"));
        }
        candidate.to_path_buf()
    } else if program.contains('/') {
        return Err(format!(
            "{program} is a relative path: it is looked up in the directory the agent works in"
        ));
    } else {
        std::env::split_paths(path_var)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate))
            .ok_or_else(|| {
                format!(
                    "{program} is not found on the PATH the agent gets ({})",
                    path_var.to_string_lossy()
                )
            })?
    };
    if let Some(root) = outside {
        let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if real(&found).starts_with(real(root)) {
            return Err(format!(
                "{} is inside the agent's worktree, where the agent can replace it",
                found.display()
            ));
        }
    }
    Ok(found)
}

/// What to do about a problem, as the end of its message.
enum Fix {
    Register,
    Path,
    Review,
}

impl Fix {
    fn text(&self) -> String {
        match self {
            Fix::Register => format!(
                "run `aaf init` in the repository and commit {SETTINGS_REL} on the base branch \
                 (git add {SETTINGS_REL} && git commit)"
            ),
            Fix::Path => format!(
                "start `aaf run` with the directory of {HOOK_BINARY} on PATH, or register its \
                 absolute path in {SETTINGS_REL} and commit that on the base branch"
            ),
            Fix::Review => format!(
                "the hook registration changes only on the base branch, by a person: look at \
                 what the task did to {SETTINGS_REL} and {LOCAL_SETTINGS_REL} in its worktree, \
                 then discard it or commit it on the base branch yourself"
            ),
        }
    }
}

/// The registration in `text` and the program it runs, checked together.
fn check_text(text: &str, path_var: &OsStr, outside: Option<&Path>) -> Result<(), (String, Fix)> {
    let hook = covering_hook(text).map_err(|problem| (problem, Fix::Register))?;
    let program = sole_program(&hook.command).map_err(|problem| (problem, Fix::Register))?;
    resolve_program(&program, path_var, outside)
        .map(|_| ())
        .map_err(|problem| (problem, Fix::Path))
}

fn check_dir(dir: &Path, path_var: &OsStr, base: Option<&str>) -> Result<(), (String, Fix)> {
    let text = match std::fs::read_to_string(dir.join(SETTINGS_REL)) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(("the file does not exist".to_string(), Fix::Register));
        }
        Err(err) => {
            return Err((format!("the file cannot be read ({err})"), Fix::Register));
        }
    };
    let valid = check_text(&text, path_var, Some(dir));
    let git = |args: &[&str]| aaf_fp::git(dir, args, &[], crate::gate::GIT_TIMEOUT);
    let fork = if let Some(base) = base {
        // Where the task's branch left the base: an older registration is
        // still the base's own, a different one is the task's doing, and
        // that is said before anything else that is wrong with it.
        let fork = match git(&["merge-base", "HEAD", base]) {
            Ok(fork) => fork,
            Err(err) => {
                let mut problem =
                    format!("the file could not be compared with the base branch ({err})");
                if let Err((inner, _)) = &valid {
                    problem.push_str(&format!(" ({inner})"));
                }
                return Err((problem, Fix::Review));
            }
        };
        let committed = git(&["show", &format!("{fork}:{SETTINGS_REL}")]);
        let changed = match committed {
            Ok(committed) if committed == text.trim() => None,
            Ok(_) => Some("the file is not what the base branch has: the task changed it".into()),
            Err(err) => Some(format!(
                "the file could not be compared with the base branch ({err})"
            )),
        };
        if let Some(mut problem) = changed {
            if let Err((inner, _)) = &valid {
                problem.push_str(&format!(" ({inner})"));
            }
            return Err((problem, Fix::Review));
        }
        Some(fork)
    } else {
        None
    };
    valid?;
    let local_path = dir.join(LOCAL_SETTINGS_REL);
    let worktree_local = match std::fs::read_to_string(&local_path) {
        Ok(local) => Some(local),
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && local_path.symlink_metadata().is_err() =>
        {
            None
        }
        Err(err) => {
            let problem = format!("{LOCAL_SETTINGS_REL} cannot be read ({err})");
            return Err((problem, Fix::Review));
        }
    };
    if let Some(fork) = fork {
        // `settings.local.json` in a task worktree must match `merge-base`:
        // an agent must never drop, delete, or rewrite it in its worktree.
        let listed =
            git(&["ls-tree", "--name-only", &fork, "--", LOCAL_SETTINGS_REL]).map_err(|err| {
                (
                    format!(
                        "{LOCAL_SETTINGS_REL} could not be compared with the base branch ({err})"
                    ),
                    Fix::Review,
                )
            })?;
        if listed.is_empty() {
            if let Some(local) = worktree_local {
                let mut problem =
                    format!("{LOCAL_SETTINGS_REL} is not on the base branch: the task created it");
                if let Err(inner) = check_local(&local) {
                    problem.push_str(&format!(" ({inner})"));
                }
                return Err((problem, Fix::Review));
            }
            return Ok(());
        }
        let committed_local =
            git(&["show", &format!("{fork}:{LOCAL_SETTINGS_REL}")]).map_err(|err| {
                (
                    format!(
                        "{LOCAL_SETTINGS_REL} could not be compared with the base branch ({err})"
                    ),
                    Fix::Review,
                )
            })?;
        match worktree_local {
            None => {
                return Err((
                    format!(
                        "{LOCAL_SETTINGS_REL} is not what the base branch has: the task deleted it"
                    ),
                    Fix::Review,
                ));
            }
            Some(local) if committed_local != local.trim() => {
                let mut problem = format!(
                    "{LOCAL_SETTINGS_REL} is not what the base branch has: the task changed it"
                );
                if let Err(inner) = check_local(&local) {
                    problem.push_str(&format!(" ({inner})"));
                }
                return Err((problem, Fix::Review));
            }
            Some(local) => {
                check_local(&local)
                    .map_err(|problem| (format!("{LOCAL_SETTINGS_REL}: {problem}"), Fix::Review))?;
            }
        }
    } else if let Some(local) = worktree_local {
        check_local(&local)
            .map_err(|problem| (format!("{LOCAL_SETTINGS_REL}: {problem}"), Fix::Review))?;
    }
    Ok(())
}

/// Why an agent started in `worktree` with `path_var` as its PATH would run
/// without the hook, or `Ok`.
///
/// A task worktree is cut from a commit, so what counts is the settings
/// file as it is in that worktree, not the one in the main checkout. With
/// `base` (the tip of the base branch) the file must also be the one the
/// base branch had where the task's branch left it: a task never changes
/// its own guard. The message ends with what to do about it.
pub fn check_worktree(worktree: &Path, path_var: &OsStr, base: Option<&str>) -> Result<(), String> {
    check_dir(worktree, path_var, base).map_err(|(problem, fix)| {
        format!(
            "the agent would run without {HOOK_BINARY}: {} in the task worktree: {problem} — \
             fix: {}, then `aaf board unblock` the task",
            worktree.join(SETTINGS_REL).display(),
            fix.text()
        )
    })
}

/// Why the agents of an `aaf run` on `repo_root` would run without the
/// hook, judged by the settings file committed at `base` (what every task
/// worktree is cut from), or `Ok`. Asked once before any task is taken: a
/// registration that is wrong on the base is wrong for every task alike.
pub fn check_base(repo_root: &Path, base: &str, path_var: &OsStr) -> Result<(), String> {
    let spec = format!("{base}:{SETTINGS_REL}");
    let result = match aaf_fp::git(repo_root, &["show", &spec], &[], crate::gate::GIT_TIMEOUT) {
        Ok(text) => check_text(&text, path_var, None),
        Err(_) => Err(("the file is not committed there".to_string(), Fix::Register)),
    };
    result.map_err(|(problem, fix)| {
        format!(
            "the agents of this run would start without {HOOK_BINARY}: {SETTINGS_REL} on the \
             base branch: {problem} — fix: {}, then start `aaf run` again; no task was touched",
            fix.text()
        )
    })?;
    let unreadable = |err: aaf_fp::FpError| {
        format!(
            "the agents of this run would start without {HOOK_BINARY}: {LOCAL_SETTINGS_REL} \
             on the base branch could not be read ({err}); no task was touched"
        )
    };
    // `ls-tree` reads trees only and says "absent" with an empty success;
    // `cat-file -e` fails alike for an absent file, a missing object and a
    // git that did not answer, and only the first may skip the check.
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", "--name-only", base, "--", LOCAL_SETTINGS_REL],
        &[],
        crate::gate::GIT_TIMEOUT,
    )
    .map_err(unreadable)?;
    if !listed.is_empty() {
        let local_spec = format!("{base}:{LOCAL_SETTINGS_REL}");
        let local_text = aaf_fp::git(
            repo_root,
            &["show", &local_spec],
            &[],
            crate::gate::GIT_TIMEOUT,
        )
        .map_err(unreadable)?;
        if let Err(problem) = check_local(&local_text) {
            return Err(format!(
                "the agents of this run would start without {HOOK_BINARY}: {LOCAL_SETTINGS_REL} \
                 on the base branch: {problem} — fix: {}, then start `aaf run` again; no task \
                 was touched",
                Fix::Review.text()
            ));
        }
    }
    Ok(())
}

fn checked_executor_hook(
    repo_root: &Path,
    executor: &str,
) -> Result<Option<super::Registered>, String> {
    match executor.trim().to_ascii_lowercase().as_str() {
        "claude" => {
            let path = repo_root.join(SETTINGS_REL);
            let text = std::fs::read_to_string(&path).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    format!("{SETTINGS_REL} does not exist")
                } else {
                    format!("{SETTINGS_REL} cannot be read ({err})")
                }
            })?;
            let reg = covering_hook(&text)?;
            let local_path = repo_root.join(LOCAL_SETTINGS_REL);
            match std::fs::read_to_string(&local_path) {
                Ok(local) => check_local(&local)
                    .map_err(|problem| format!("{LOCAL_SETTINGS_REL}: {problem}"))?,
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound
                        && local_path.symlink_metadata().is_err() => {}
                Err(err) => {
                    return Err(format!("{LOCAL_SETTINGS_REL} cannot be read ({err})"));
                }
            }
            Ok(Some(reg))
        }
        "codex" => super::multi_host::check_codex(repo_root).map(Some),
        "gemini" => super::multi_host::check_gemini(repo_root)
            .or_else(|primary_err| {
                super::multi_host::check_antigravity(repo_root).map_err(|_| primary_err)
            })
            .map(Some),
        "antigravity" => super::multi_host::check_antigravity(repo_root)
            .or_else(|primary_err| {
                super::multi_host::check_gemini(repo_root).map_err(|_| primary_err)
            })
            .map(Some),
        "opencode" => super::multi_host::check_opencode(repo_root).map(Some),
        _ => Ok(None),
    }
}

/// Validates the matching host hook settings file in `repo_root` for `executor`.
///
/// Checks `.claude/settings.json` (`claude`), `.codex/hooks.json` (`codex`),
/// `.gemini/settings.json` (`gemini`/`antigravity`), or `.opencode/hooks.json`
/// (`opencode`). Unknown / `custom` executors return `Ok(())`.
pub fn validate_for_executor(repo_root: &Path, executor: &str) -> Result<(), String> {
    checked_executor_hook(repo_root, executor).map(|_| ())
}

/// Verifies the matching host hook registration in `worktree` for `executor`,
/// delegating `claude` to [`check_worktree`] and validating host-specific hook
/// configs + binary resolution for `codex`, `gemini`, `antigravity`, and `opencode`.
pub fn check_worktree_for_executor(
    worktree: &Path,
    path_var: &OsStr,
    base: Option<&str>,
    executor: &str,
) -> Result<(), String> {
    let normalized = executor.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "claude" {
        return check_worktree(worktree, path_var, base);
    }
    let Some(reg) = checked_executor_hook(worktree, &normalized).map_err(|problem| {
        format!(
            "the agent would run without {HOOK_BINARY}: hook settings for {normalized} in {}: \
             {problem} — fix: run `aaf init --hook-host {normalized}` and commit the settings \
             file on the base branch, then `aaf board unblock` the task",
            worktree.display()
        )
    })?
    else {
        return Ok(());
    };
    let program = super::command_program(&reg.command).ok_or_else(|| {
        format!("the agent would run without {HOOK_BINARY}: empty hook command for {normalized}")
    })?;
    resolve_program(&program, path_var, Some(worktree))
        .map(|_| ())
        .map_err(|problem| {
            format!(
                "the agent would run without {HOOK_BINARY}: {problem} — fix: start `aaf run` \
                 with {HOOK_BINARY} on PATH, then `aaf board unblock` the task"
            )
        })
}

type HostTextCheck = fn(&str) -> Result<super::Registered, String>;

fn host_base_spec(
    normalized: &str,
) -> Option<(&'static str, HostTextCheck, Option<HostTextCheck>)> {
    use super::multi_host::{
        CODEX_HOOKS_REL, GEMINI_SETTINGS_REL, OPENCODE_HOOKS_REL, covering_antigravity_hook,
        covering_codex_hook, covering_gemini_hook, covering_opencode_hook,
    };
    match normalized {
        "codex" => Some((CODEX_HOOKS_REL, covering_codex_hook, None)),
        "gemini" => Some((
            GEMINI_SETTINGS_REL,
            covering_gemini_hook,
            Some(covering_antigravity_hook),
        )),
        "antigravity" => Some((
            GEMINI_SETTINGS_REL,
            covering_antigravity_hook,
            Some(covering_gemini_hook),
        )),
        "opencode" => Some((OPENCODE_HOOKS_REL, covering_opencode_hook, None)),
        _ => None,
    }
}

/// Verifies the matching host hook registration committed at `base` for
/// `executor`, delegating `""` / `"claude"` to [`check_base`] and checking
/// host-specific settings + `aaf-hook` binary resolution for `codex`, `gemini`,
/// `antigravity`, and `opencode`. Unknown / `custom` executors return `Ok(())`.
pub fn check_base_for_executor(
    repo_root: &Path,
    base: &str,
    path_var: &OsStr,
    executor: &str,
) -> Result<(), String> {
    let normalized = executor.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "claude" {
        return check_base(repo_root, base, path_var);
    }
    let Some((settings_rel, primary, fallback)) = host_base_spec(&normalized) else {
        return Ok(());
    };
    let register_err = |problem: &str| {
        format!(
            "the agents of this run would start without {HOOK_BINARY}: {settings_rel} for \
             {normalized} on the base branch: {problem} — fix: run `aaf init --hook-host \
             {normalized}` and commit the settings file on the base branch, then start `aaf run` \
             again; no task was touched"
        )
    };
    let spec = format!("{base}:{settings_rel}");
    let text = aaf_fp::git(repo_root, &["show", &spec], &[], crate::gate::GIT_TIMEOUT)
        .map_err(|_| register_err("the file is not committed there"))?;
    let reg = primary(&text)
        .or_else(|primary_err| match fallback {
            Some(fb) => fb(&text).map_err(|_| primary_err),
            None => Err(primary_err),
        })
        .map_err(|problem| register_err(&problem))?;
    let program =
        super::command_program(&reg.command).ok_or_else(|| register_err("empty hook command"))?;
    resolve_program(&program, path_var, None)
        .map(|_| ())
        .map_err(|problem| {
            format!(
                "the agents of this run would start without {HOOK_BINARY}: {settings_rel} for \
                 {normalized} on the base branch: {problem} — fix: start `aaf run` with \
                 {HOOK_BINARY} on PATH; no task was touched"
            )
        })
}
