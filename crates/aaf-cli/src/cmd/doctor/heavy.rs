//! Advisory checks about the build tools that can freeze the machine:
//! daemons that live outside the heavy queue, and git hooks that start a
//! build without going through it. Read-only, like the rest of the doctor.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::config::CommandsConfig;
use aaf_engine::throttle::{BuildDaemon, HeavyTool, heavy_tool, unqueued_heavy_tool};

/// Scripts larger than this are not read.
const MAX_SCRIPT_BYTES: u64 = 256 * 1024;
/// Scripts a hook names that are looked at, at most.
const MAX_SCRIPTS: usize = 20;
/// The hooks git runs around a commit and a push.
const HOOKS: &[&str] = &["pre-commit", "pre-push"];
const MB: u64 = 1024 * 1024;

/// Gradle and Kotlin daemons that are running now.
pub fn live_daemons(daemons: &[BuildDaemon]) -> Result<String, String> {
    if daemons.is_empty() {
        return Ok("no Gradle or Kotlin daemon is running".to_string());
    }
    let list = daemons
        .iter()
        .map(|daemon| {
            format!(
                "{} pid={} rss={} MB",
                daemon.kind,
                daemon.pid,
                daemon.rss_bytes / MB
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "{} running outside the heavy queue: {list}; their memory is not counted and they \
         are never paused. Stop them with `gradle --stop` and build with --no-daemon",
        daemons.len()
    ))
}

fn properties_switch_the_daemon_off(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with(['#', '!']) {
            return false;
        }
        line.split_once(['=', ':']).is_some_and(|(key, value)| {
            key.trim() == "org.gradle.daemon" && value.trim() == "false"
        })
    })
}

/// The repo's gate commands that run Gradle with its daemon left on. A lint
/// that starts Gradle is one of them: the gate queues it like a build.
pub fn daemon_config(
    repo_root: Option<&Path>,
    commands: &CommandsConfig,
) -> Result<String, String> {
    let gradle: Vec<(&str, &str)> = [
        ("lint", commands.lint.as_str()),
        ("build", commands.build.as_str()),
        ("test_scoped", commands.test_scoped.as_str()),
        ("test_full", commands.test_full.as_str()),
    ]
    .into_iter()
    .filter(|(_, command)| heavy_tool(command) == Some(HeavyTool::Gradle))
    .collect();
    if gradle.is_empty() {
        return Ok("no gate command runs Gradle".to_string());
    }
    let without_flag: Vec<&str> = gradle
        .iter()
        .filter(|(_, command)| !command.split_whitespace().any(|word| word == "--no-daemon"))
        .map(|(name, _)| *name)
        .collect();
    if without_flag.is_empty() {
        return Ok("every Gradle command passes --no-daemon".to_string());
    }
    let properties = repo_root
        .and_then(|root| fs::read_to_string(root.join("gradle.properties")).ok())
        .unwrap_or_default();
    if properties_switch_the_daemon_off(&properties) {
        return Ok("gradle.properties sets org.gradle.daemon=false".to_string());
    }
    let names = without_flag
        .iter()
        .map(|name| format!("commands.{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "{names} run Gradle without --no-daemon and gradle.properties does not set \
         org.gradle.daemon=false. Under aaf they get GRADLE_OPTS=-Dorg.gradle.daemon=false \
         and run alone; started any other way they leave a daemon behind. Add --no-daemon"
    ))
}

/// One line of a hook, or of a script it names, that starts a build tool
/// outside the queue.
#[derive(Debug, PartialEq, Eq)]
struct Finding {
    file: PathBuf,
    line: usize,
    tool: HeavyTool,
    text: String,
}

/// The file as text; `None` when it is missing, too large or not UTF-8.
fn read_script(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SCRIPT_BYTES {
        return None;
    }
    String::from_utf8(fs::read(path).ok()?).ok()
}

/// The build tool the logical command made of `lines` starts outside the
/// queue. `lines` is one line, or several joined by a trailing backslash.
fn unqueued_in(lines: &[&str]) -> Option<HeavyTool> {
    // The shell reads a backslash at the end of a line as white space, and
    // so does the classifier: `aaf slot -- \` covers the line after it.
    let joined = lines.join("\n");
    if let Some(tool) = unqueued_heavy_tool(&joined) {
        return Some(tool);
    }
    if heavy_tool(&joined).is_some() {
        // A build tool, and all of it handed to the queue.
        return None;
    }
    // Nothing in the joined command is queued, so a continuation excuses
    // nothing: a line that reads as a build on its own is still reported,
    // as it was before lines were joined.
    lines.iter().find_map(|line| unqueued_heavy_tool(line))
}

fn scan(file: &Path, text: &str, findings: &mut Vec<Finding>) {
    let mut lines = text.lines().enumerate();
    while let Some((index, first)) = lines.next() {
        let mut command = vec![first];
        while command.last().is_some_and(|line| line.ends_with('\\')) {
            match lines.next() {
                Some((_, line)) => command.push(line),
                None => break,
            }
        }
        if let Some(tool) = unqueued_in(&command) {
            let text = command
                .iter()
                .map(|line| line.trim().trim_end_matches('\\').trim_end())
                .collect::<Vec<_>>()
                .join(" ");
            findings.push(Finding {
                file: file.to_path_buf(),
                // Where the command starts.
                line: index + 1,
                tool,
                text: text.chars().take(120).collect(),
            });
        }
    }
}

/// Files inside the repo (or the hooks directory) that `text` names as a
/// word: the scripts a hook hands its work to. A word that starts with a
/// variable is tried by what follows its first `/`.
fn named_scripts(text: &str, repo_root: &Path, hooks_dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let words = text
        .split(|c: char| c.is_whitespace() || "'\"();&|<>=`".contains(c))
        .filter(|word| !word.is_empty() && !word.starts_with('-'));
    for word in words {
        let tail = if word.starts_with('$') {
            word.split_once('/').map_or("", |(_, rest)| rest)
        } else {
            word
        };
        let relative = tail.trim_start_matches("./").trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        let candidates = [
            PathBuf::from(word),
            repo_root.join(relative),
            hooks_dir.join(relative),
        ];
        for candidate in candidates {
            if !candidate.is_absolute() {
                continue;
            }
            let Ok(real) = fs::canonicalize(&candidate) else {
                continue;
            };
            let inside = real.starts_with(repo_root) || real.starts_with(hooks_dir);
            // The build tool's own launcher is what the hook runs, not a
            // script of the hook: its lines are not the hook's.
            let launcher = real
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| heavy_tool(name).is_some());
            if inside && real.is_file() && !launcher && !found.contains(&real) {
                found.push(real);
            }
        }
        if found.len() >= MAX_SCRIPTS {
            break;
        }
    }
    found.truncate(MAX_SCRIPTS);
    found
}

/// Looks at `pre-commit` and `pre-push` in `hooks_dir` and at the scripts
/// they name, one level deep. Both paths are canonical.
pub fn git_hooks(repo_root: &Path, hooks_dir: &Path) -> Result<String, String> {
    let mut findings = Vec::new();
    let mut present = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for hook in HOOKS {
        let path = hooks_dir.join(hook);
        let Some(text) = read_script(&path) else {
            continue;
        };
        present.push(*hook);
        scan(&path, &text, &mut findings);
        let real_hook = fs::canonicalize(&path).unwrap_or(path);
        for script in named_scripts(&text, repo_root, hooks_dir) {
            if script == real_hook || seen.contains(&script) {
                continue;
            }
            if let Some(text) = read_script(&script) {
                scan(&script, &text, &mut findings);
            }
            seen.push(script);
        }
    }
    if present.is_empty() {
        return Ok(format!(
            "no pre-commit or pre-push hook in {}",
            hooks_dir.display()
        ));
    }
    if findings.is_empty() {
        return Ok(format!(
            "{}: no build tool is started outside the heavy queue",
            present.join(", ")
        ));
    }
    let detail = findings
        .iter()
        .map(|finding| {
            let file = finding
                .file
                .strip_prefix(repo_root)
                .unwrap_or(&finding.file);
            format!(
                "{}:{} starts {} outside the heavy queue; change it to: aaf slot -- {}",
                file.display(),
                finding.line,
                finding.tool.name(),
                finding.text
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(detail)
}

/// [`git_hooks`] for the hooks directory git itself would use
/// (`core.hooksPath`, else the `hooks` directory of the repository).
pub fn git_hooks_check(repo_root: Option<&Path>) -> Result<String, String> {
    let Some(repo_root) = repo_root else {
        return Ok("not checked: not inside a git work tree".to_string());
    };
    let args = ["rev-parse", "--path-format=absolute", "--git-path", "hooks"];
    let hooks_dir = aaf_fp::git(repo_root, &args, &[], aaf_engine::gate::GIT_TIMEOUT)
        .map_err(|err| format!("cannot find the hooks directory: {err}"))?;
    let hooks_dir = PathBuf::from(hooks_dir);
    // Canonical, so that "inside the repo" compares like with like; a hooks
    // directory that does not exist simply holds no hook.
    let repo_root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let hooks_dir = fs::canonicalize(&hooks_dir).unwrap_or(hooks_dir);
    git_hooks(&repo_root, &hooks_dir)
}
