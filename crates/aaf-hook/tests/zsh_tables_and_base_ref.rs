#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for zsh associative-table rebinding and dynamic `[gate].base_ref`.

use std::fs;
use std::path::Path;

use aaf_hook::{Decision, HookEnv, decide};
use serde_json::json;
use tempfile::TempDir;

fn env_for<'a>(repo: &'a Path, factory: bool) -> HookEnv<'a> {
    let root = repo.to_path_buf();
    let finder: &'a dyn Fn(&Path) -> Option<std::path::PathBuf> =
        Box::leak(Box::new(move |dir: &Path| {
            dir.starts_with(&root).then(|| root.clone())
        }));
    let no_cfg: &'a dyn Fn(&Path) -> Option<String> = Box::leak(Box::new(|_: &Path| None));
    HookEnv {
        repo_root: finder,
        git_config: no_cfg,
        data_dir: None,
        config_dir: None,
        home: None,
        cwd: Some(repo.to_path_buf()),
        in_factory_run: factory,
        trusted_aaf_bins: Vec::new(),
    }
}

fn bash_in(repo: &Path, command: &str, factory: bool) -> Decision {
    let payload = json!({
        "tool_name": "Bash",
        "cwd": repo.to_string_lossy(),
        "tool_input": { "command": command }
    })
    .to_string();
    decide(payload.as_bytes(), &env_for(repo, factory))
}

fn write_in(repo: &Path, file: &Path, factory: bool) -> Decision {
    let payload = json!({
        "tool_name": "Write",
        "cwd": repo.to_string_lossy(),
        "tool_input": { "file_path": file.to_string_lossy(), "content": "x" }
    })
    .to_string();
    decide(payload.as_bytes(), &env_for(repo, factory))
}

#[test]
fn zsh_command_tables_rebind_aaf_and_invalidate_slot() {
    let tmp = TempDir::new().unwrap();
    for cmd in [
        "functions[aaf]=true; aaf slot -- ./gradlew build",
        "functions['aaf']=true; aaf slot -- ./gradlew build",
        "aliases[aaf]='echo'; aaf slot -- ./gradlew build",
        "galiases[aaf]='echo'; aaf slot -- ./gradlew build",
        "saliases[aaf]='echo'; aaf slot -- ./gradlew build",
        "commands[aaf]=/bin/true; aaf slot -- ./gradlew build",
        "typeset functions[aaf]=true; aaf slot -- ./gradlew build",
        "functions[$K]=true; aaf slot -- ./gradlew build",
        "functions=(aaf true); aaf slot -- ./gradlew build",
    ] {
        let decision = bash_in(tmp.path(), cmd, true);
        assert!(
            matches!(&decision, Decision::Deny(d) if d.rule == "heavy_cmd_guard"),
            "expected heavy_cmd_guard denial for `{cmd}`, got {decision:?}"
        );
    }
    // Unrelated table keys do not distrust `aaf slot`.
    let benign = bash_in(
        tmp.path(),
        "functions[other]=true; aaf slot -- ./gradlew build",
        true,
    );
    assert_eq!(benign, Decision::Allow);
}

#[test]
fn configured_gate_base_ref_is_protected_across_guards() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/trunk\n").unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[gate]\nbase_ref = \"origin/trunk\"\n",
    )
    .unwrap();

    // Interactive write and commit on configured base branch `trunk` are blocked.
    let write_decision = write_in(repo, &repo.join("src/lib.rs"), false);
    assert!(
        matches!(&write_decision, Decision::Deny(d) if d.rule == "main_branch_guard"),
        "expected main_branch_guard for Write on trunk, got {write_decision:?}"
    );
    let commit_decision = bash_in(repo, "git commit -m 'direct'", false);
    assert!(
        matches!(&commit_decision, Decision::Deny(d) if d.rule == "main_branch_guard"),
        "expected main_branch_guard for commit on trunk, got {commit_decision:?}"
    );

    // Destructive git operations on `trunk` are blocked in factory runs too.
    for cmd in [
        "git branch -D trunk",
        "git push --delete origin trunk",
        "git push --force-with-lease origin trunk",
        "git update-ref -d refs/heads/trunk",
        "git checkout -B trunk HEAD~1",
        "git branch -f trunk HEAD~1",
    ] {
        let decision = bash_in(repo, cmd, true);
        assert!(
            matches!(&decision, Decision::Deny(d) if d.rule == "git_guard"),
            "expected git_guard denial for `{cmd}`, got {decision:?}"
        );
    }

    // Ordinary feature branch stays allowed.
    let feature_ok = bash_in(repo, "git branch -d feat/demo", true);
    assert_eq!(feature_ok, Decision::Allow);
}

#[test]
fn delete_guard_protects_autofactory_memory_md() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    let mem = repo.join(".autofactory/memory.md");
    fs::write(&mem, "- Run cargo fmt\n").unwrap();

    let w = write_in(repo, &mem, true);
    assert!(
        matches!(&w, Decision::Deny(d) if d.rule == "delete_guard"),
        "expected delete_guard on Write .autofactory/memory.md, got {w:?}"
    );
    let rm = bash_in(repo, "rm .autofactory/memory.md", true);
    assert!(
        matches!(&rm, Decision::Deny(d) if d.rule == "delete_guard"),
        "expected delete_guard on rm .autofactory/memory.md, got {rm:?}"
    );
    // Reading `.autofactory/memory.md` remains allowed.
    let read_payload = json!({
        "tool_name": "Read",
        "cwd": repo.to_string_lossy(),
        "tool_input": { "file_path": mem.to_string_lossy() }
    })
    .to_string();
    assert_eq!(
        decide(read_payload.as_bytes(), &env_for(repo, true)),
        Decision::Allow
    );
}
