#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round-4 security fixes in `aaf-hook`.

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_hook::{HookEnv, decide};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn env_split_string_safe_tokenizer_and_wrapper_preservation() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            // Shell metacharacters inside `env -S` are literal argv words, not syntax.
            ("env -S 'rm -rf #' /", DELETE),
            ("env -S 'rm -rf ;' /", DELETE),
            ("env -S 'rm -rf |' /", DELETE),
            ("env -S 'rm -rf &' /", DELETE),
            // Unclosed quote inside `env -S` becomes opaque and fails closed on dangerous targets.
            ("env -S \"rm -rf '/unclosed\"", OPAQUE),
            ("env -S \"rm -rf 'unclosed\" /", OPAQUE),
            // Prior wrappers (`sudo -D`, `env -C`, `xargs`) are preserved across `-S`.
            ("sudo -D ~ env -S 'rm -rf' .", DELETE),
            ("env -C ~ env -S 'rm -rf' .", DELETE),
            ("find . | xargs env -S 'rm'", DELETE),
            // Benign
            ("env -S 'echo hello #' world", ALLOW),
        ],
    );
}

#[test]
fn wrapped_cwd_propagates_to_subshells_and_script_c_is_scope_isolated() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            ("env -C ~ sh -c 'rm -rf .'", DELETE),
            ("sudo -D ~ bash -c 'rm -rf Documents'", DELETE),
            ("env -C ~ sh <<< 'rm -rf .'", DELETE),
            ("echo 'rm -rf .' | env -C ~ sh", DELETE),
            ("echo 'rm -rf .' | sudo -D ~ sh", DELETE),
            ("env -C ~ script -c 'rm -rf .' /dev/null", DELETE),
            ("script -c 'cd /tmp && ls' /dev/null; rm .env", DELETE),
            ("env -C /tmp sh -c 'ls'; rm .env", DELETE),
            // Benign
            ("env -C /tmp sh -c 'rm -rf /tmp/ok_dir'", ALLOW),
            ("script -c 'echo ok' /dev/null", ALLOW),
        ],
    );
}

#[test]
fn heredocs_evaluate_inline_with_scope_and_wrapper_isolation() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            // `cd ~` before heredoc applies inside heredoc even if `cd /tmp` follows.
            ("cd ~\nsh <<'EOF'\nrm -rf .\nEOF\ncd /tmp", DELETE),
            // `cd /tmp` inside heredoc must not leak to commands after it.
            ("sh <<'EOF'\ncd /tmp\nEOF\nrm .env", DELETE),
            // `cd /tmp` inside one heredoc must not leak into a second heredoc.
            ("sh <<'A' && sh <<'B'\ncd /tmp\nA\nrm .env\nB", DELETE),
            // Owner / consumer `env -C` wrappers apply to heredoc body.
            ("env -C ~ sh <<'EOF'\nrm -rf .\nEOF", DELETE),
            ("cat <<'EOF' | env -C ~ sh\nrm -rf .\nEOF", DELETE),
            // Lone `cat <<'EOF'` inside a data consumer (`git commit -m`) stays allowed.
            ("git commit -m \"$(cat <<'EOF'\nrm -rf /\nEOF\n)\"", ALLOW),
        ],
    );
}

#[test]
fn whole_repo_deletion_after_cd_and_symlink_before_dotdot() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".claude/sub")).unwrap();
    fs::create_dir_all(fx.root.join(".git/objects")).unwrap();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    fs::create_dir_all(fx.root.join("build")).unwrap();

    // Realistic `repo_root`: returns `None` once cwd leaves `fx.root`.
    let root = fx.root.clone();
    let realistic_lookup = move |dir: &Path| dir.starts_with(&root).then(|| root.clone());
    let empty_cfg = |_: &Path| Some(String::new());
    let env = HookEnv {
        repo_root: &realistic_lookup,
        git_config: &empty_cfg,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
    };
    let check_realistic = |cmd: &str| {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": { "command": fx.fill(cmd) },
            "cwd": fx.root,
        });
        rule(&decide(payload.to_string().as_bytes(), &env))
    };
    assert_eq!(check_realistic("cd .. && rm -rf repo"), DELETE);
    assert_eq!(check_realistic("cd /tmp && rm -rf {root}"), DELETE);
    assert_eq!(check_realistic("cd /tmp && mv {root} /tmp/gone"), DELETE);
    assert_eq!(check_realistic("env -C /tmp rm -rf {root}"), DELETE);
    assert_eq!(check_realistic("rm -rf build"), ALLOW);

    #[cfg(unix)]
    {
        let outside = fx.home.join("outside_sym");
        fs::create_dir_all(outside.join("sub")).unwrap();
        symlink(outside.join("sub"), fx.root.join("escape")).unwrap();
        symlink(fx.root.join(".git/objects"), fx.root.join("gitlink")).unwrap();
        symlink(fx.root.join(".claude/sub"), fx.root.join("claudelink")).unwrap();

        run_corpus(
            &fx,
            &[
                ("rm -rf escape/../important", DELETE),
                ("touch gitlink/../hooks/pre-commit", GIT),
                ("echo x > gitlink/../hooks/pre-commit", GIT),
                ("rm claudelink/../settings.json", DELETE),
                ("rm -rf sub/../build", ALLOW),
            ],
        );
    }
}

#[test]
fn git_alias_tracks_cd_pushd_scopes_and_git_c() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("sub/.env", "SECRET=2\n");
    run_corpus(
        &fx,
        &[
            ("git -c 'alias.boom=!cd ~ && rm -rf Documents' boom", GIT),
            ("git -c 'alias.boom=!pushd ~ && rm -rf Documents' boom", GIT),
            ("git -c 'alias.boom=!(cd /tmp && ls); rm .env' boom", GIT),
            ("git -C ~ -c 'alias.boom=!rm -rf .' boom", GIT),
            ("git -c 'alias.ok=!cd /tmp && echo hi' ok", ALLOW),
        ],
    );

    fx.aliases(&[("boom", "!rm -rf ."), ("rmenv", "rm .env")]);
    run_corpus(
        &fx,
        &[
            ("git -C ~ boom", GIT),
            // Non-shell alias with `-C sub` resolves `sub/.env` once (not `sub/sub/.env`).
            ("git -C sub rmenv", GIT),
        ],
    );
}

#[test]
fn interp_guard_multi_arg_exec_calls_and_php_include_require() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            (
                "node -e \"require('child_process').execFileSync('rm', ['-rf', '/'])\"",
                INTERP,
            ),
            (
                "node -e \"require('child_process').spawnSync('git', ['push', '--force'])\"",
                INTERP,
            ),
            ("perl -e \"system('rm', '-rf', '/')\"", INTERP),
            ("ruby -e \"system('rm', '-rf', '/')\"", INTERP),
            (
                "python3 -c \"import subprocess; subprocess.run(('rm', '-rf', '/'))\"",
                INTERP,
            ),
            ("php -r \"include '.env';\"", INTERP),
            ("php -r \"include('.env');\"", INTERP),
            ("php -r \"include_once('.env');\"", INTERP),
            ("php -r \"require '.env';\"", INTERP),
            ("php -r \"require_once('.env');\"", INTERP),
            // Zero false positives on Node `require('fs')`, PHP benign include, quoted spaces
            ("node -e \"require('fs'); console.log('ok')\"", ALLOW),
            ("php -r \"include 'README.md';\"", ALLOW),
            (
                "python3 -c \"import subprocess; subprocess.run(['git', 'status'])\"",
                ALLOW,
            ),
            (
                "python3 -c \"import subprocess; subprocess.run(['git', 'commit', '-m', 'force push'])\"",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn pushd_and_popd_stack_offsets() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            ("pushd ~ && popd +1 && rm -rf Documents", DELETE),
            ("pushd ~ && popd -0 && rm -rf Documents", DELETE),
            (
                "pushd ~ && pushd /tmp && pushd +1 && rm -rf Documents",
                DELETE,
            ),
            ("pushd ~ && pushd /tmp && pushd -0 && rm .env", DELETE),
            ("pushd /tmp && popd +0 && rm .env", DELETE),
            ("pushd /tmp && popd +1 && rm -rf /tmp/ok_dir", ALLOW),
        ],
    );
}

#[test]
fn recursive_copiers_into_guarded_dirs_printf_formats_and_sed_exec() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".autofactory/config.toml", "x = 1\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("src")).unwrap();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    run_corpus(
        &fx,
        &[
            // Recursive `cp`/`rsync` into protected dirs
            ("cp -r /tmp/evil/.claude .", DELETE),
            ("cp -a /tmp/evil/.autofactory .", DELETE),
            ("rsync -a /tmp/evil/.claude .", DELETE),
            ("rsync -a /tmp/evil/ .claude/", DELETE),
            ("cp -r /tmp/evil/. .claude", DELETE),
            ("cp -R /tmp/evil/ .autofactory/", DELETE),
            ("cp -rt .claude /tmp/evil/.", DELETE),
            ("cp -r /tmp/evil/docs build/", ALLOW),
            ("rsync -a src/ build/", ALLOW),
            // `printf` generalized `%b` / `%q` / width-precision / `%%`
            ("printf 'rm -rf %b' '/' | sh", DELETE),
            ("printf 'rm -rf %q' '/' | sh", DELETE),
            ("printf 'rm -rf %.5s' '/' | sh", DELETE),
            ("printf 'echo 100%%; %b' 'rm -rf /' | sh", DELETE),
            ("printf '%d; %s' 1 'rm -rf /' | sh", DELETE),
            ("printf '%b' 'echo ok' | sh", ALLOW),
            // GNU `sed` `e <cmd>` and `s/.../<cmd>/e`
            ("sed 'e rm -rf /' README.md", DELETE),
            ("sed '1e git push --force' README.md", GIT),
            ("sed -e '1p; e cat .env' README.md", SECRET),
            ("sed 's/.*/rm -rf \\//e' README.md", DELETE),
            ("sed 's/a/b/' README.md", ALLOW),
            ("sed 'w /tmp/extra.txt' README.md", ALLOW),
        ],
    );
}
