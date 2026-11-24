#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round-2 security & wrapper fixes in `aaf-hook`.

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_hook::host::{HookHost, decide_for_host};
use aaf_hook::{Decision, HookEnv};
use common::{Fixture, rule, run_corpus, run_corpus_in};
use serde_json::{Value, json};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");
const SECRET: Option<&str> = Some("secret_guard");

fn judge_host(fixture: &Fixture, payload: &Value, host: HookHost) -> (Decision, HookHost) {
    let root = fixture.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |_: &Path| fixture.git_config.borrow().clone();
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fixture.data.clone()),
        config_dir: Some(fixture.config.clone()),
        home: Some(fixture.home.clone()),
        cwd: None,
        in_factory_run: fixture.in_factory_run,
        trusted_aaf_bins: fixture.trusted_aaf_bins.clone(),
    };
    decide_for_host(payload.to_string().as_bytes(), &env, host)
}

fn nest_deep(inner: &str) -> String {
    let mut cmd = inner.to_string();
    for _ in 0..12 {
        cmd = format!("echo $({cmd})");
    }
    cmd
}

#[test]
fn sec019_interp_guard_versioned_attached_and_clustered_flags() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".env.example", "SECRET=\n");
    run_corpus(
        &fx,
        &[
            // Versioned interpreter names
            ("python3.12 -c \"import os; os.system('rm -rf /')\"", INTERP),
            ("python2.7 -c \"print(open('.env').read())\"", INTERP),
            ("perl5.34 -e \"system('rm -rf ~')\"", INTERP),
            (
                "ruby3.3 -e \"require 'fileutils'; FileUtils.rm_rf('/etc')\"",
                INTERP,
            ),
            ("php8.3 -r \"system('rm -rf /');\"", INTERP),
            (
                "nodejs -e \"require('fs').readFileSync('.env', 'utf8')\"",
                INTERP,
            ),
            (
                "node20 -e \"require('child_process').execSync('git reset --hard')\"",
                INTERP,
            ),
            // Attached inline flags
            ("perl -E'system(\"rm -rf /\")'", INTERP),
            ("php -r\"system('rm -rf /');\"", INTERP),
            ("node -p\"require('fs').readFileSync('.env')\"", INTERP),
            (
                "node --print=\"require('fs').readFileSync('.env')\"",
                INTERP,
            ),
            ("python3.12 -c\"import os; os.system('rm -rf /')\"", INTERP),
            // Short-flag clusters
            ("python3 -uc \"import os; os.system('rm -rf /')\"", INTERP),
            ("python3 -Bc \"print(open('.env').read())\"", INTERP),
            ("node -pe \"require('fs').readFileSync('.env')\"", INTERP),
            ("node -p -e \"require('fs').readFileSync('.env')\"", INTERP),
            ("perl -we 'system(\"rm -rf ~\")'", INTERP),
            ("perl -nle 'system(\"rm -rf ~\")'", INTERP),
            ("ruby -we \"FileUtils.rm_rf('/etc')\"", INTERP),
            ("php -nr \"system('rm -rf /');\"", INTERP),
            // Valued flags before inline code still inspect inline code
            (
                "python3 -W ignore -c \"print(open('.env').read())\"",
                INTERP,
            ),
            ("ruby -rfileutils -e \"FileUtils.rm_rf('/etc')\"", INTERP),
            ("perl -MFile::Path -e \"rmtree('/etc')\"", INTERP),
            // Benign invocations with versioned names, valued flags, or clusters stay allowed
            ("python3.12 -uc \"print(1 + 2)\"", ALLOW),
            ("python3 -Wdefault -c \"print('ok')\"", ALLOW),
            (
                "python3 -W ignore -c \"print(open('.env.example').read())\"",
                ALLOW,
            ),
            ("node -pe \"1 + 2\"", ALLOW),
            ("node -r fs -e \"console.log('ok')\"", ALLOW),
            ("perl -MFile::Path -e \"print 'ok'\"", ALLOW),
            ("ruby -rfileutils -e \"puts 'ok'\"", ALLOW),
            ("php -ddisplay_errors=1 -nr \"echo 'ok';\"", ALLOW),
        ],
    );
}

#[test]
fn sec017_host_cwd_override_and_codex_tool_detection() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hello\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();

    // `workdir`, `directory`, and `dir_path` override cwd before evaluating guards.
    let (dec, host) = judge_host(
        &fx,
        &json!({
            "tool_name": "exec_command",
            "tool_input": { "cmd": "rm -rf .", "workdir": "~" },
            "cwd": fx.root,
        }),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Codex);
    assert_eq!(rule(&dec), DELETE);

    let (dec, host) = judge_host(
        &fx,
        &json!({
            "hook_event_name": "BeforeTool",
            "tool_name": "run_shell_command",
            "tool_input": { "command": "rm -rf .", "directory": "~" },
            "cwd": fx.root,
        }),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Gemini);
    assert_eq!(rule(&dec), DELETE);

    let (dec, _) = judge_host(
        &fx,
        &json!({
            "tool_name": "exec_command",
            "tool_input": { "cmd": "touch pre-commit", "dir_path": ".git/hooks" },
            "cwd": fx.root,
        }),
        HookHost::Auto,
    );
    assert_eq!(rule(&dec), GIT);

    // Payload cwd is `/tmp`, but `workdir` jumps into the repo root -> `.env` is protected.
    let (dec, _) = judge_host(
        &fx,
        &json!({
            "tool_name": "exec_command",
            "tool_input": { "cmd": "rm .env", "workdir": fx.root },
            "cwd": "/tmp",
        }),
        HookHost::Auto,
    );
    assert_eq!(rule(&dec), DELETE);

    // Non-string cwd override fails closed as `hook_input`.
    for bad in [
        json!({ "cmd": "ls", "workdir": 123 }),
        json!({ "cmd": "ls", "directory": true }),
        json!({ "cmd": "ls", "dir_path": ["."] }),
        json!({ "cmd": "ls", "workdir": null }),
    ] {
        let (dec, _) = judge_host(
            &fx,
            &json!({ "tool_name": "exec_command", "tool_input": bad, "cwd": fx.root }),
            HookHost::Auto,
        );
        assert_eq!(rule(&dec), Some("hook_input"));
    }

    // Empty string cwd override leaves base cwd intact.
    let (dec, _) = judge_host(
        &fx,
        &json!({
            "tool_name": "exec_command",
            "tool_input": { "cmd": "git status", "workdir": "" },
            "cwd": fx.root,
        }),
        HookHost::Auto,
    );
    assert_eq!(dec, Decision::Allow);

    // `detect_host` auto-detects Codex file tools (`delete_file`, `edit_file`, `create_file`, `view_file`).
    for (tool, path, expected_rule) in [
        ("delete_file", ".env", DELETE),
        ("edit_file", ".git/config", GIT),
        ("create_file", ".claude/settings.json", DELETE),
        ("view_file", ".env", SECRET),
        ("view_file", "README.md", ALLOW),
    ] {
        let (dec, host) = judge_host(
            &fx,
            &json!({
                "tool_name": tool,
                "tool_input": { "path": path },
                "cwd": fx.root,
            }),
            HookHost::Auto,
        );
        assert_eq!(host, HookHost::Codex, "tool={tool}");
        assert_eq!(rule(&dec), expected_rule, "tool={tool}, path={path}");
    }

    // Gemini `BeforeTool` with `edit_file` continues to resolve to Gemini.
    let (dec, host) = judge_host(
        &fx,
        &json!({
            "hook_event_name": "BeforeTool",
            "tool_name": "edit_file",
            "tool_input": { "file_path": ".git/config" },
            "cwd": fx.root,
        }),
        HookHost::Auto,
    );
    assert_eq!(host, HookHost::Gemini);
    assert_eq!(rule(&dec), GIT);
}

#[test]
fn sec020_delete_guard_parent_globs_symlinks_and_nested() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("sub/ok.txt", "ok\n");
    fx.write("build/out.txt", "ok\n");
    #[cfg(unix)]
    symlink(fx.root.join(".env"), fx.root.join("sub/env_link")).unwrap();

    run_corpus(
        &fx,
        &[
            #[cfg(unix)]
            ("rm */env_link", DELETE),
            ("rm .claud?/settings.json", DELETE),
            ("rm -rf */.claude", DELETE),
            ("rm -rf **/.claude", DELETE),
            ("rm */.env.staging", DELETE),
            ("rm */.env.prod*", DELETE),
            ("rm -rf build/*", ALLOW),
            ("rm */ok.txt", ALLOW),
        ],
    );

    // Fallback without files on disk: nested pairs, guarded dirs, and `.env.*` variants.
    let empty = Fixture::new();
    run_corpus(
        &empty,
        &[
            ("rm .claud?/settings.json", DELETE),
            ("rm .autofact*/config.toml", DELETE),
            ("rm -rf */.claude", DELETE),
            ("rm -rf */.autofact*", DELETE),
            ("rm */.env.production", DELETE),
            ("rm */.env.prod*", DELETE),
            ("rm src/settings.json", ALLOW),
        ],
    );
}

#[test]
fn secret_guard_blocks_posix_read_write_redirect() {
    let fx = Fixture::new();
    fx.write("keystore.properties", "storePassword=abc\n");
    fx.write("certs/private.pem", "-----BEGIN KEY-----\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            ("cat <> keystore.properties", SECRET),
            ("exec 3<> certs/private.pem", SECRET),
            ("cat <> /tmp/outside/.env", SECRET),
            ("cat <> README.md", ALLOW),
        ],
    );
}

#[test]
fn macos_wrapper_specs_xargs_and_script() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            ("xargs -S 255 rm .env", DELETE),
            ("xargs -L 1 rm .env", DELETE),
            ("xargs --max-lines=1 rm .env", DELETE),
            ("xargs --max-lines rm -rf /", DELETE),
            ("script -F /dev/null git push --force", GIT),
            ("script -F /dev/null rm .env", DELETE),
            ("script -Fc 'git push --force' /dev/null", GIT),
            ("script -F /dev/null git status", ALLOW),
        ],
    );
}

#[test]
fn cd_home_and_wrapped_chdir_tracking() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("sub/ok.txt", "ok\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();

    run_corpus(
        &fx,
        &[
            ("cd && rm -rf .", DELETE),
            ("cd -P && rm -rf .", DELETE),
            ("cd -- && rm -rf .", DELETE),
            ("cd - && ls", ALLOW),
            ("env -C ~ rm -rf .", DELETE),
            ("env -C~ rm -rf .", DELETE),
            ("env --chdir ~ rm -rf .", DELETE),
            ("env --chdir=~ rm -rf .", DELETE),
            ("env -C sub -C .. rm .env", DELETE),
            ("sudo -D ~ rm -rf .", DELETE),
            ("sudo --chdir=~ rm -rf .", DELETE),
            ("sudo -u root -D .git/hooks touch pre-commit", GIT),
            ("env -C /tmp ls", ALLOW),
        ],
    );

    // Invoked from `/tmp`, `env -C {root}` and `sudo -D {root}` enter the repo.
    run_corpus_in(
        &fx,
        Path::new("/tmp"),
        &[
            ("env -C {root} rm .env", DELETE),
            ("sudo -D {root} rm .env", DELETE),
        ],
    );
}

#[test]
fn fallback_covers_four_newer_guards_past_max_depth() {
    let fx = Fixture::new();
    let cases = [
        (nest_deep("rm -rf /"), OPAQUE),
        (nest_deep("rm -rf ~"), OPAQUE),
        (nest_deep("rm -rf ../sibling"), OPAQUE),
        (nest_deep("cat .env"), OPAQUE),
        (nest_deep("head app/release.keystore"), OPAQUE),
        (nest_deep("dd if=.env"), OPAQUE),
        (nest_deep("gh pr merge 42"), OPAQUE),
        (nest_deep("gh api repos/o/r/pulls/42/merge"), OPAQUE),
        (
            nest_deep("gh api graphql -f query=mergePullRequest"),
            OPAQUE,
        ),
        (
            nest_deep("python3.12 -c \"import os; os.system('id')\""),
            OPAQUE,
        ),
        (
            nest_deep("node -e \"require('fs').readFileSync('x')\""),
            OPAQUE,
        ),
        (
            nest_deep("python3 -c \"import shutil; shutil.rmtree('x')\""),
            OPAQUE,
        ),
        // Benign deeply nested commands still pass.
        (nest_deep("ls"), ALLOW),
        (nest_deep("cat README.md"), ALLOW),
        (nest_deep("gh pr view 42"), ALLOW),
        (nest_deep("python3 -c \"print(1)\""), ALLOW),
    ];
    let refs: Vec<(&str, Option<&str>)> = cases.iter().map(|(c, e)| (c.as_str(), *e)).collect();
    run_corpus(&fx, &refs);
}

#[test]
fn git_files_sed_in_place_variants() {
    let fx = Fixture::new();
    fx.write(".git/config", "[core]\n");
    run_corpus(
        &fx,
        &[
            ("sed -I '' 's/a/b/' .git/config", GIT),
            ("sed -ni '' 's/a/b/' .git/config", GIT),
            ("sed -EI '' 's/a/b/' .git/config", GIT),
            ("sed -rni.bak 's/a/b/' .git/config", GIT),
            // Read-only `sed` with `i` inside `-e`/`-f` script must stay allowed.
            ("sed -e s/i/x/ .git/config", ALLOW),
            ("sed -es/i/x/ .git/config", ALLOW),
            ("sed -ne's/i/x/' .git/config", ALLOW),
            ("sed -e -i .git/config", ALLOW),
            ("sed -f script_i.sed .git/config", ALLOW),
        ],
    );
}

#[test]
fn pr_merge_guard_endpoint_normalization_and_h_flag() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("gh api repos/o/r/pulls/563/merge/", PR_MERGE),
            ("gh api repos/o/r/pulls/563/merge?foo=1", PR_MERGE),
            ("gh api repos/o/r/pulls/563/merge#frag", PR_MERGE),
            (
                "gh api https://api.github.com/repos/o/r/pulls/563/merge?x=1",
                PR_MERGE,
            ),
            (
                "gh api /graphql -f query='mutation { mergePullRequest(input:{}) { clientMutationId } }'",
                PR_MERGE,
            ),
            (
                "gh api /graphql/?x=1 -f query='mutation { mergePullRequest(input:{}) { clientMutationId } }'",
                PR_MERGE,
            ),
            (
                "gh api https://api.github.com/graphql -f query='mutation { mergePullRequest(input:{}) { clientMutationId } }'",
                PR_MERGE,
            ),
            ("gh -h pr merge 563", PR_MERGE),
            (
                "gh api -H 'Accept: application/vnd.github+json' /repos/o/r/pulls/563/merge",
                PR_MERGE,
            ),
            // Benign read-only `gh` calls stay allowed.
            (
                "gh api https://api.github.com/repos/o/r/pulls/563?state=open",
                ALLOW,
            ),
            (
                "gh api /graphql -f query='query { viewer { login } }'",
                ALLOW,
            ),
            ("gh -h", ALLOW),
        ],
    );
}

#[test]
fn recursive_rm_after_cwd_escape_outside_repo_is_denied() {
    let fx = Fixture::new();
    let outside = fx.home.join("outside_escape_test");
    std::fs::create_dir_all(&outside).unwrap();
    let outside_str = outside.to_string_lossy();
    let cmd_env = format!("env -C {outside_str} rm -rf subdir");
    let cmd_cd = format!("cd {outside_str} && rm -rf subdir");
    run_corpus(
        &fx,
        &[
            (cmd_env.as_str(), DELETE),
            (cmd_cd.as_str(), DELETE),
            ("rm -rf subdir", ALLOW),
        ],
    );
}

#[test]
fn multi_c_flags_and_git_persist_edge_cases() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".env", "SECRET=1\n");
    fx.write("ok.txt", "ok\n");
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    #[cfg(unix)]
    {
        fs::write(fx.data.join("aaf.db"), "db").unwrap();
        symlink(&fx.data, fx.root.join("data_link")).unwrap();
        fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
        symlink(fx.root.join(".git/hooks"), fx.root.join("hooks_link")).unwrap();
    }

    // On feature/x: multi -C, update-ref abbrev, force_create clusters & abbrev, 1-arg branch -m allowed.
    run_corpus(
        &fx,
        &[
            ("git -C .claude -C . rm settings.json", DELETE),
            ("git -C sub -C .. -C .claude rm settings.json", DELETE),
            ("git -C sub -C .. rm .env", DELETE),
            ("git -C .claude -C .. rm ok.txt", ALLOW),
            ("git -C /tmp -C sub rm .env", ALLOW),
            ("git update-ref --std", GIT),
            ("git update-ref --st", GIT),
            ("git update-ref --s", GIT),
            ("git checkout -qBmain", GIT),
            ("git switch -qCmain", GIT),
            ("git checkout -qBrelease/1.0", GIT),
            ("git switch --force-c=main", GIT),
            ("git switch --force-create=release/2.0", GIT),
            ("git switch --force-c main", GIT),
            ("git checkout -qBfeature/ok", ALLOW),
            ("git switch -cCmain", ALLOW),
            ("git branch -m feature/renamed", ALLOW),
            // Non-existent URLs/tokens stay allowed while real symlinks still block.
            ("curl https://example.com/a/b/c/d", ALLOW),
            #[cfg(unix)]
            ("cat data_link/aaf.db", Some("data_dir_guard")),
            #[cfg(unix)]
            ("touch hooks_link/pre-commit", GIT),
        ],
    );

    // On main: 1-arg `branch -m`/`-M`/`--move` renames the current protected branch and must be denied.
    fx.checkout("main");
    run_corpus(
        &fx,
        &[
            ("git branch -m old-main", GIT),
            ("git branch -M old-main", GIT),
            ("git branch --move old-main", GIT),
            ("git -C sub -C . branch -m old-main", GIT),
            ("git -C sub -C . push --force-with-lease", GIT),
            // 2-arg rename of an unrelated branch does not touch `main`.
            ("git branch -m feature/a feature/b", ALLOW),
        ],
    );
}
