#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Plan v3 Wave 4 Slice 3 regression suite:
//! - `HOOK-07`: same-script non-reassigned `TMP=$(mktemp -d); rm -rf "$TMP"` + actionable FIX
//! - `HOOK-08`: call-scoped `RMTREE_APIS`/`FILE_READ_APIS`, regex-literal `/.../.exec(`,
//!   and Python `tempfile.mkdtemp()` / `TemporaryDirectory` rmtree exemption
//! - `HOOK-12`: canonical `/tmp` scratch `git -C <dir> reset --hard` / `HOME=<tmp>` /
//!   `GIT_CONFIG_GLOBAL=<tmp>` and non-global `git config core.pager cat`
//! - `HOOK-13`: multi-host `Bash` `cmd`/array fallback and Antigravity/Gemini tool aliases

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_hook::host::{HookHost, decide_for_host};
use aaf_hook::{Decision, HookEnv};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");

#[test]
fn hook07_mktemp_rm_rf_allowed_only_when_unreassigned_and_bare() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            // Same-script mktemp -d with bare "$TMP" / ${TMP} allowed.
            ("TMP=$(mktemp -d); rm -rf \"$TMP\"", ALLOW),
            (
                "TMP=$(mktemp -d) && touch \"$TMP/a\" && rm -rf \"$TMP\"",
                ALLOW,
            ),
            ("TMP=$(mktemp -d -t aaf); rm -rf ${TMP}", ALLOW),
            ("local TMP=$(mktemp -d); rm -rf \"$TMP\"", ALLOW),
            ("TMP=$(mktemp -d /tmp/aaf.XXXXXX); rm -rf \"$TMP\"", ALLOW),
            // Suffix, prefix, or traversal on $TMP still denied.
            ("TMP=$(mktemp -d); rm -rf \"$TMP/sub\"", DELETE),
            ("TMP=$(mktemp -d); rm -rf \"$TMP/..\"", DELETE),
            ("TMP=$(mktemp -d); rm -rf target/$TMP", DELETE),
            ("rm -rf target/$VAR", DELETE),
            ("rm -rf \"$TMP\"", DELETE),
            // Reassignment, unset, subshell-only bind, or non-mktemp subst denied.
            ("TMP=$(mktemp -d); TMP=/etc; rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d); TMP=$(echo /); rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d); export TMP=/; rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d); unset TMP; rm -rf \"$TMP\"", DELETE),
            ("(TMP=$(mktemp -d)); rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d; echo /); rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d)/..; rm -rf \"$TMP\"", DELETE),
            ("TMP=$(mktemp -d -p /etc); rm -rf \"$TMP\"", DELETE),
            // Sibling protected file in same rm still denied.
            ("TMP=$(mktemp -d); rm -rf \"$TMP\" .env", DELETE),
        ],
    );

    // Actionable non-human-handoff FIX on recursive-rm target errors.
    match fx.bash("rm -rf target/$VAR") {
        Decision::Deny(d) => {
            assert_eq!(d.rule, "delete_guard");
            assert_eq!(
                d.fix,
                "use an explicit literal path under the repository or /tmp"
            );
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn hook08_interp_guard_scoped_args_regex_exec_and_py_mkdtemp() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".gitignore", "target/\n");
    fs::create_dir_all(fx.root.join("build")).unwrap();

    run_corpus(
        &fx,
        &[
            // Regex literal `/.../.exec(` allowed; arbitrary `obj.exec(` still checked.
            ("node -e \"console.log(/foo/.exec('foobar'))\"", ALLOW),
            (
                "node -e \"const m = /^v(\\\\d+)/i.exec('v12'); console.log(m)\"",
                ALLOW,
            ),
            ("node -e \"console.log(/[a/b]+/g.exec('ab'))\"", ALLOW),
            ("node -e \"cp.exec('rm -rf /')\"", INTERP),
            ("node -e \"const r = /foo/; r.exec('rm -rf /')\"", INTERP),
            (
                "node -e \"require('child_process').exec('git push --force')\"",
                INTERP,
            ),
            // Scoped FILE_READ_APIS: unrelated '.env' literal next to open('.gitignore') allowed.
            (
                "python3 -c \"marker = '.env'; print(open('.gitignore').read())\"",
                ALLOW,
            ),
            ("python3 -c \"print(open('.env').read())\"", INTERP),
            (
                "node -e \"const msg = '.env'; require('fs').readFileSync('.gitignore', 'utf8')\"",
                ALLOW,
            ),
            (
                "node -e \"require('fs').readFileSync('.env', 'utf8')\"",
                INTERP,
            ),
            // Scoped RMTREE_APIS: unrelated '/etc' literal next to rmSync('build') allowed.
            (
                "node -e \"const info = '/etc'; require('fs').rmSync('build', {recursive: true})\"",
                ALLOW,
            ),
            (
                "node -e \"require('fs').rmSync('/etc', {recursive: true})\"",
                INTERP,
            ),
            // Python tempfile.mkdtemp() / TemporaryDirectory rmtree exemption.
            (
                "python3 -c \"import shutil, tempfile; d = tempfile.mkdtemp(); shutil.rmtree(d)\"",
                ALLOW,
            ),
            (
                "python3 -c \"import shutil, tempfile; td = tempfile.TemporaryDirectory(); shutil.rmtree(td.name)\"",
                ALLOW,
            ),
            (
                "python3 -c \"import shutil, tempfile\nwith tempfile.TemporaryDirectory() as d:\n    shutil.rmtree(d)\"",
                ALLOW,
            ),
            // Reassigned, suffixed, or dir-overridden mkdtemp still denied.
            (
                "python3 -c \"import shutil, tempfile; d = tempfile.mkdtemp(); d = '/etc'; shutil.rmtree(d)\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil, tempfile; d = tempfile.mkdtemp(); shutil.rmtree(d + '/..')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil, tempfile; d = tempfile.mkdtemp(dir='/etc'); shutil.rmtree(d)\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn hook12_git_tmp_scratch_reset_and_local_pager_cat() {
    let fx = Fixture::new();
    let scratch = tempfile::tempdir_in("/tmp").unwrap();
    fs::create_dir(scratch.path().join(".git")).unwrap();
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let cfg_file = scratch.path().join("gitconfig");
    fs::write(&cfg_file, "[user]\n\tname = test\n").unwrap();
    let cfg_path = cfg_file.to_string_lossy().into_owned();

    let mut cases: Vec<(String, Option<&str>)> = vec![
        // Real canonicalized /tmp scratch dir allowed.
        (format!("git -C {scratch_path} reset --hard"), ALLOW),
        (
            format!("HOME={scratch_path} git -C {scratch_path} reset --hard"),
            ALLOW,
        ),
        (
            format!("GIT_CONFIG_GLOBAL={cfg_path} git -C {scratch_path} status"),
            ALLOW,
        ),
        // `/tmp` itself, nonexistent dirs, or `--git-dir` pointing to repo denied.
        ("git -C /tmp reset --hard".into(), GIT),
        (
            "git -C /tmp/nonexistent-aaf-scratch-xyz reset --hard".into(),
            GIT,
        ),
        (
            format!("git -C {scratch_path} --git-dir={{root}}/.git reset --hard"),
            GIT,
        ),
        ("HOME=/tmp git status".into(), GIT),
        // Non-global / non-system `core.pager = cat` allowed; global/system/flags denied.
        ("git config core.pager cat".into(), ALLOW),
        ("git config --local core.pager cat".into(), ALLOW),
        ("git config set core.pager cat".into(), ALLOW),
        ("git config --global core.pager cat".into(), GIT),
        ("git config --system core.pager cat".into(), GIT),
        ("git config core.pager 'cat -n'".into(), GIT),
        ("git config core.pager 'cat; id'".into(), GIT),
    ];

    #[cfg(unix)]
    let _link_guard = {
        let link_dir = tempfile::tempdir_in("/tmp").unwrap();
        let link_to_repo = link_dir.path().join("link_to_repo");
        symlink(&fx.root, &link_to_repo).unwrap();
        let link_str = link_to_repo.to_string_lossy().into_owned();
        cases.push((format!("git -C {link_str} reset --hard"), GIT));
        cases.push((format!("HOME={link_str} git status"), GIT));
        link_dir
    };

    let borrowed: Vec<(&str, Option<&str>)> = cases
        .iter()
        .map(|(cmd, exp)| (cmd.as_str(), *exp))
        .collect();
    run_corpus(&fx, &borrowed);
}

#[test]
fn hook13_bash_cmd_array_fallback_and_antigravity_tool_aliases() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    let root = fx.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |_: &Path| Some(String::new());
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
        in_factory_run: true,
        trusted_aaf_bins: Vec::new(),
    };

    let eval = |host: HookHost, payload: serde_json::Value| {
        decide_for_host(payload.to_string().as_bytes(), &env, host).0
    };

    // Claude / Auto Bash with `tool_input.cmd` or array `command` works instead of `hook_input`.
    assert_eq!(
        eval(
            HookHost::Claude,
            json!({ "tool_name": "Bash", "tool_input": { "cmd": "git status" }, "cwd": fx.root })
        ),
        Decision::Allow
    );
    assert_eq!(
        rule(&eval(
            HookHost::Auto,
            json!({ "tool_name": "Bash", "tool_input": { "cmd": "git push --force" }, "cwd": fx.root })
        )),
        Some("git_guard")
    );
    assert_eq!(
        eval(
            HookHost::Codex,
            json!({ "tool_name": "Bash", "tool_input": { "command": ["git", "status"] }, "cwd": fx.root })
        ),
        Decision::Allow
    );
    assert_eq!(
        rule(&eval(
            HookHost::Codex,
            json!({ "tool_name": "Bash", "tool_input": { "command": ["cat", ".env"] }, "cwd": fx.root })
        )),
        Some("secret_guard")
    );

    // Antigravity / Gemini aliases.
    assert_eq!(
        eval(
            HookHost::Auto,
            json!({ "tool_name": "run_command", "tool_input": { "CommandLine": "git status" }, "cwd": fx.root })
        ),
        Decision::Allow
    );
    assert_eq!(
        rule(&eval(
            HookHost::Auto,
            json!({ "tool_name": "run_command", "tool_input": { "CommandLine": "git push --force" }, "cwd": fx.root })
        )),
        Some("git_guard")
    );
    for tool in [
        "write_to_file",
        "replace_file_content",
        "multi_replace_file_content",
    ] {
        assert_eq!(
            rule(&eval(
                HookHost::Auto,
                json!({ "tool_name": tool, "tool_input": { "TargetFile": ".env" }, "cwd": fx.root })
            )),
            Some("delete_guard"),
            "{tool}"
        );
        assert_eq!(
            eval(
                HookHost::Auto,
                json!({ "tool_name": tool, "tool_input": { "TargetFile": "src/ok.rs" }, "cwd": fx.root })
            ),
            Decision::Allow,
            "{tool}"
        );
    }
    assert_eq!(
        rule(&eval(
            HookHost::Gemini,
            json!({ "tool_name": "view_file", "tool_input": { "AbsolutePath": ".env" }, "cwd": fx.root })
        )),
        Some("secret_guard")
    );
}
