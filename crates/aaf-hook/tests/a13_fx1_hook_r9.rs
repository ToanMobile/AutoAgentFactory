#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-9 hook findings: a heredoc fed to a
//! shell that stands on the line after its body is split once, not once
//! per copy (R9-H6-1), and `coproc` before nested groups keeps the group
//! stack balanced (R9 Lane 6, below bar).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aaf_hook::shell;
use common::{Fixture, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;

/// R9-H6-1: `cat <<'A' |`, the body, then the shell alone on its line
/// splits the body exactly as `cat <<'A' | <shell>` on one line does.
#[test]
fn r9_h6_1_next_line_shell_splits_the_body_once() {
    let mut wrong = Vec::new();
    for consumer in ["sh", "bash -s", "env -S sh", "env -i sh", ". /dev/stdin"] {
        let next_line = format!("cat <<'A' |\necho hi\nA\n{consumer}");
        let one_line = format!("cat <<'A' | {consumer}\necho hi\nA");
        let (got, want) = (shell::parse(&next_line), shell::parse(&one_line));
        if got.cmds.len() != want.cmds.len() {
            wrong.push(format!(
                "{next_line:?}: {} commands, one line {}",
                got.cmds.len(),
                want.cmds.len()
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// R9-H6-1: next-line heredocs nested six deep, 20 KB innermost, are split
/// quickly and the command at the bottom is still found.
#[test]
fn r9_h6_1_nested_next_line_heredocs_stay_linear() {
    let line = "echo building module && ls src | grep rs\n";
    let mut script = line.repeat(500) + "git push --force origin main\n";
    for depth in 0..6 {
        script = format!("cat <<'E{depth}' |\n{script}E{depth}\nsh\n");
    }
    let started = Instant::now();
    let parsed = shell::parse(&script);
    let took = started.elapsed();
    assert!(
        parsed
            .cmds
            .iter()
            .any(|cmd| cmd.words.join(" ") == "git push --force origin main"),
        "innermost command not found"
    );
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

/// R8-H6-1 stays fixed: the interpreter after the body still sees it, and
/// a shell after the body still runs it.
#[test]
fn r9_h6_1_feeder_heredoc_still_reaches_the_consumer() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    let deny = [
        "cat <<'EOF' |\nopen('.claude/settings.json', 'w').write('')\nEOF\npython3",
        "cat <<'EOF' |\nopen('.claude/settings.json', 'w').write('')\nEOF\nenv -S python3",
        "cat <<'EOF' |\ngh pr merge 1\nEOF\nsh",
        "cat <<'EOF' |\ngh pr merge 1\nEOF\nbash -s",
        "cat <<'EOF' |\ngh pr merge 1\nEOF\nenv -S sh",
        // What the body runs keeps the consumer's cleared environment.
        "cat <<'EOF' |\naaf status\nEOF\nenv -i sh",
        "cat <<'EOF' | env -i sh\naaf status\nEOF",
        "cat <<'EOF' |\naaf status\nEOF\nenv -S 'env -i sh'",
    ];
    let mut wrong = Vec::new();
    for command in deny {
        if common::rule(&fx.bash(command)).is_none() {
            wrong.push(format!("{command:?} allowed"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    run_corpus(
        &fx,
        &[
            ("cat <<'X' |\necho hi\nX\nsh", ALLOW),
            ("cat <<'X' |\nprint('hi')\nX\npython3", ALLOW),
        ],
    );
}

/// A temp dir holding `data`, `home` and a git checkout `work`.
fn checkout() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "home", "work/src"] {
        fs::create_dir_all(base.join(sub)).unwrap();
    }
    let status = Command::new("git")
        .args(["init", "-q", "-b", "feature/x"])
        .current_dir(base.join("work"))
        .env("HOME", base.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    (dir, base)
}

/// Exit code of the built hook for `command`, run from `work` with
/// `CDPATH` set to `base`.
fn exit_code(base: &Path, command: &str) -> Option<i32> {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": base.join("work"),
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_DATA_DIR", base.join("data"))
        .env("HOME", base.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("CDPATH", base)
        .current_dir(base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    child.wait().unwrap().code()
}

/// `coproc` takes a NAME only when it is a word, never `{`: the groups
/// nested in what it starts keep the group stack balanced, and a `CDPATH`
/// cleared in a piped group still does not count.
#[test]
fn r9_coproc_group_is_not_its_own_name() {
    let (_dir, base) = checkout();
    let rows = [
        (
            "{ coproc { { :; }; }; unset CDPATH; } | cat; cd data && rm -rf *",
            2,
        ),
        (
            "{ coproc { :; }; unset CDPATH; } | cat; cd data && rm -rf *",
            2,
        ),
        (
            "{ coproc NAME { { :; }; }; unset CDPATH; } | cat; cd data && rm -rf *",
            2,
        ),
        // Still cleared.
        ("coproc { { :; }; }; unset CDPATH; cd data && rm -rf *", 0),
        (
            "{ coproc { { :; }; }; unset CDPATH; }; cd data && rm -rf *",
            0,
        ),
    ];
    let mut wrong = Vec::new();
    for (command, expected) in rows {
        let got = exit_code(&base, command);
        if got != Some(expected) {
            wrong.push(format!("`{command}`: {got:?}, want {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
