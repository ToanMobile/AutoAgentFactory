#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Wave 3: aliases that already sit in the git configuration, and the files
//! git obeys (`.git/config`, `.git/hooks`). The built binary against a real
//! repository with real `git config` entries.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn git(dir: &Path, home: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Runs the hook from `base/work`, as Claude Code does from the project.
fn exit_code(base: &Path, payload: &Value) -> Option<i32> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_DATA_DIR", base.join("data"))
        .env("AAF_CONFIG_DIR", base.join("config"))
        .env("HOME", base.join("home"))
        .current_dir(base.join("work"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The hook may answer before reading everything; a closed pipe is fine.
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    child.wait().unwrap().code()
}

fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "config", "home", "work", "elsewhere"] {
        fs::create_dir(base.join(sub)).unwrap();
    }
    let (work, home) = (base.join("work"), base.join("home"));
    git(&work, &home, &["init", "-q", "-b", "feature/x"]);
    git(&work, &home, &["config", "alias.fp", "push --force"]);
    git(&work, &home, &["config", "alias.st", "status --short"]);
    git(
        &work,
        &home,
        &["config", "alias.yolo", "!git push --force origin main"],
    );
    git(&work, &home, &["config", "alias.chain", "fp"]);
    git(&work, &home, &["config", "alias.wipe", "reset --hard"]);
    // An alias every repository of this user has.
    fs::write(home.join(".gitconfig"), "[alias]\n\tgfp = push -f\n").unwrap();
    (dir, base)
}

#[test]
fn a_stored_alias_is_judged_as_the_command_it_stands_for() {
    let (_dir, base) = repo();
    let work = base.join("work");
    let elsewhere = base.join("elsewhere");
    let from = format!("git -C {} fp origin main", work.display());

    let cases: &[(&str, &str, &Path, i32)] = &[
        (
            "local alias for push --force",
            "git fp origin main",
            &work,
            2,
        ),
        ("the same alias, any case", "git FP origin main", &work, 2),
        ("shell alias", "git yolo", &work, 2),
        ("alias of an alias", "git chain origin main", &work, 2),
        ("alias for reset --hard", "git wipe", &work, 2),
        ("alias from ~/.gitconfig", "git gfp origin main", &work, 2),
        ("named with -C from another directory", &from, &elsewhere, 2),
        (
            "inside a nested shell",
            "sh -c 'git fp origin main'",
            &work,
            2,
        ),
        ("harmless alias", "git st", &work, 0),
        ("not an alias at all", "git nosuchcommand --help", &work, 0),
        ("builtin", "git status --short", &work, 0),
        (
            "the alias is unknown in another repository",
            "git fp",
            &elsewhere,
            0,
        ),
    ];
    let mut wrong = Vec::new();
    for (label, command, cwd, expected) in cases {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": cwd,
        });
        let got = exit_code(&base, &payload);
        if got != Some(*expected) {
            wrong.push(format!(
                "{label}: `{command}` exited {got:?}, expected {expected}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn an_alias_that_cannot_be_looked_up_is_refused() {
    let (_dir, base) = repo();
    let work = base.join("work");
    // git itself refuses to read this file, so no alias can be ruled out.
    fs::write(work.join(".git/config"), "[alias\n\tbroken\n").unwrap();
    let call = |command: &str| {
        exit_code(
            &base,
            &json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": work}),
        )
    };
    assert_eq!(call("git fp origin main"), Some(2));
    // A builtin needs no lookup and is judged as before.
    assert_eq!(call("git status"), Some(0));
}

#[test]
fn the_files_git_obeys_cannot_be_written() {
    let (_dir, base) = repo();
    let work = base.join("work");
    fs::create_dir_all(work.join(".git/worktrees/t1")).unwrap();

    let file_cases: &[(&str, &str, i32)] = &[
        ("Edit", ".git/config", 2),
        ("Write", ".git/hooks/pre-commit", 2),
        ("MultiEdit", ".git/hooks/pre-push", 2),
        ("Write", ".git/config.worktree", 2),
        ("Write", ".git/worktrees/t1/config.worktree", 2),
        ("Write", ".git/modules/lib/hooks/post-checkout", 2),
        ("Read", ".git/config", 0),
        ("Read", ".git/hooks/pre-commit", 0),
        ("Write", ".git/COMMIT_EDITMSG", 0),
        ("Write", "docs/config", 0),
        ("Write", "hooks/pre-commit", 0),
    ];
    let bash_cases: &[(&str, i32)] = &[
        ("echo 'exit 0' > .git/hooks/pre-commit", 2),
        ("printf x >> .git/config", 2),
        ("cp /tmp/evil .git/hooks/pre-push", 2),
        ("mv .git/hooks/pre-commit.sample .git/hooks/pre-commit", 2),
        ("chmod +x .git/hooks/pre-commit", 2),
        ("ln -s /tmp/evil .git/hooks/post-merge", 2),
        ("rm -rf .git/hooks", 2),
        ("sed -i '' 's/a/b/' .git/config", 2),
        ("tee .git/hooks/pre-commit < /tmp/evil", 2),
        ("dd if=/tmp/evil of=.git/hooks/pre-commit", 2),
        ("cat .git/config", 0),
        ("ls -la .git/hooks", 0),
        ("sh .git/hooks/pre-commit", 0),
        ("cp .git/hooks/pre-commit.sample /tmp/sample", 0),
        ("git config user.name someone", 0),
    ];
    let mut wrong = Vec::new();
    for (tool, path, expected) in file_cases {
        let payload = json!({
            "tool_name": tool,
            "tool_input": {"file_path": work.join(path)},
            "cwd": work,
        });
        let got = exit_code(&base, &payload);
        if got != Some(*expected) {
            wrong.push(format!(
                "{tool} {path}: exited {got:?}, expected {expected}"
            ));
        }
    }
    for (command, expected) in bash_cases {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": work,
        });
        let got = exit_code(&base, &payload);
        if got != Some(*expected) {
            wrong.push(format!("`{command}`: exited {got:?}, expected {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
