#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round-3 security fixes in `aaf-hook`.

mod common;

use std::cell::RefCell;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use aaf_hook::{HookEnv, decide};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn interp_guard_php_stdin_heredocs_bash_and_rmtree_protections() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".autofactory/config.toml", "x = 1\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("build")).unwrap();
    run_corpus(
        &fx,
        &[
            // PHP read and shell-exec APIs
            ("php -r \"readfile('.env');\"", INTERP),
            ("php -r \"file('.env');\"", INTERP),
            ("php -r \"parse_ini_file('.env');\"", INTERP),
            ("php -r \"highlight_file('.env');\"", INTERP),
            ("php -r \"show_source('.env');\"", INTERP),
            ("php -r \"passthru('rm -rf /');\"", INTERP),
            ("php -r \"readfile('README.md');\"", ALLOW),
            // Interpreter stdin payloads: here-string, pipe, heredoc
            ("python3 <<< \"print(open('.env').read())\"", INTERP),
            (
                "echo \"import os; os.system('rm -rf /')\" | python3",
                INTERP,
            ),
            ("printf '%s' \"open('.env').read()\" | python3 -", INTERP),
            (
                "python3 - <<'EOF'\nimport os\nos.system('rm -rf /')\nEOF",
                INTERP,
            ),
            (
                "node <<'EOF'\nrequire('fs').readFileSync('.env', 'utf8')\nEOF",
                INTERP,
            ),
            (
                "cat <<'EOF' | python3\nprint(open('.env').read())\nEOF",
                INTERP,
            ),
            ("python3 - <<'EOF'\nprint(\"don't worry\")\nEOF", ALLOW),
            // SHELL_EXEC_APIS routed through full `bash()` (cd + opaque)
            (
                "python3 -c \"import os; os.system('cd ~ && rm -rf .')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.system('git push --force \\'unclosed')\"",
                INTERP,
            ),
            // RMTREE_APIS covers guarded dirs, git files, and data dir
            (
                "python3 -c \"import shutil; shutil.rmtree('.claude')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree('.autofactory')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree('.git')\"",
                INTERP,
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree('{data}')\"",
                Some("data_dir_guard"),
            ),
            (
                "python3 -c \"import shutil; shutil.rmtree('build')\"",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn env_split_string_trailing_args_stdin_xargs_and_printf_pipe() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            ("env -S 'rm -rf' /", DELETE),
            ("env -S 'cat' .env", SECRET),
            ("echo 'rm -rf /' | env -S sh", DELETE),
            ("env -S sh <<< 'rm -rf /'", DELETE),
            ("env -C ~ -S 'rm -rf' .", DELETE),
            ("env -S 'echo' hello", ALLOW),
            ("xargs --max-lines rm -rf /", DELETE),
            ("xargs -L 1 rm .env", DELETE),
            ("xargs --max-lines=1 rm .env", DELETE),
            ("printf '%s' 'rm -rf /' | sh", DELETE),
            ("printf -- '%s\\n' 'git push --force' | bash", GIT),
            ("printf 'rm -rf %s' '/' | sh", DELETE),
            ("printf '%s' 'echo ok' | sh", ALLOW),
        ],
    );
}

#[test]
fn subshell_cwd_isolation_cd_dash_pushd_popd_and_wrapped_redirects() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    run_corpus(
        &fx,
        &[
            // Subshells must not leak `cd /tmp` outside
            ("(cd /tmp && ls); rm .env", DELETE),
            ("(cd /tmp && ls) && rm .env", DELETE),
            ("echo $(cd /tmp && pwd); rm .env", DELETE),
            ("echo `cd /tmp && pwd`; rm .env", DELETE),
            ("sh -c 'cd /tmp && ls'; rm .env", DELETE),
            // Subshells still enforce `cd ~` inside
            ("(cd ~ && rm -rf .)", DELETE),
            ("echo $(cd ~ && rm -rf .)", DELETE),
            // `cd -` and `pushd`/`popd`
            ("cd /tmp && cd - && rm .env", DELETE),
            ("cd ~ && cd /tmp && cd - && rm -rf .", DELETE),
            ("pushd /tmp && popd && rm .env", DELETE),
            ("pushd ~ && pushd /tmp && popd && rm -rf .", DELETE),
            ("pushd /tmp && pushd && rm .env", DELETE),
            // Redirects on `env -C` / `sudo -D` resolve against original cwd
            (
                "cd .claude && env -C /tmp echo '{}' > settings.json",
                DELETE,
            ),
            ("cd .git/hooks && env -C /tmp echo x > pre-commit", GIT),
            ("env -C /tmp cat < .env", SECRET),
            ("env -C /tmp python3 <<< \"open('.env').read()\"", INTERP),
            // Benign
            ("(cd /tmp && ls); echo ok", ALLOW),
            ("cd /tmp && rm -rf /tmp/ok_dir", ALLOW),
        ],
    );
}

#[test]
fn delete_guard_symlink_parent_escape_and_write_targets() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("README.md", "hello\n");
    fs::create_dir_all(fx.root.join("docs")).unwrap();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    let outside = fx.home.join("outside_sym_target");
    fs::create_dir_all(outside.join("sub")).unwrap();
    #[cfg(unix)]
    symlink(&outside, fx.root.join("escape")).unwrap();

    run_corpus(
        &fx,
        &[
            #[cfg(unix)]
            ("rm -rf escape/sub", DELETE),
            #[cfg(unix)]
            ("rm -rf escape/", DELETE),
            #[cfg(unix)]
            ("rm -rf escape", ALLOW),
            // cp / install / rsync / dd / sed write targets
            ("cp /tmp/evil.json .claude/settings.json", DELETE),
            ("cp /tmp/evil .env", DELETE),
            ("cp -t .claude /tmp/settings.json", DELETE),
            ("cp --target-directory=.claude /tmp/settings.json", DELETE),
            ("cp --target-directory .claude /tmp/settings.json", DELETE),
            ("cp /tmp/settings.json .claude/", DELETE),
            (
                "install -m 644 /tmp/evil.json .claude/settings.json",
                DELETE,
            ),
            ("install -t .claude /tmp/settings.json", DELETE),
            ("rsync -a /tmp/evil.json .claude/settings.json", DELETE),
            ("rsync -a /tmp/evil .env", DELETE),
            ("dd if=/dev/zero of=.env bs=1 count=1", DELETE),
            ("dd if=/tmp/evil.json of=.claude/settings.json", DELETE),
            ("sed -i 's/a/b/' .env", SECRET),
            ("sed -i '' 's/a/b/' .claude/settings.json", DELETE),
            ("sed --in=.bak 's/a/b/' .claude/settings.json", DELETE),
            ("sed -n 'w .claude/settings.json' README.md", DELETE),
            // Benign
            ("cp README.md docs/README.md", ALLOW),
            ("dd if=/dev/zero of=build/disk.img bs=1 count=1", ALLOW),
            ("sed -i 's/a/b/' README.md", ALLOW),
            ("sed 's/a/b/' .claude/settings.json", ALLOW),
        ],
    );
}

#[test]
fn git_abbreviations_git_dir_head_and_multi_c_alias() {
    let fx = Fixture::new();
    let other_main = fx.home.join("other_main");
    fs::create_dir_all(other_main.join(".git")).unwrap();
    fs::write(other_main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let other_feat = fx.home.join("other_feat");
    fs::create_dir_all(other_feat.join(".git")).unwrap();
    fs::write(other_feat.join(".git/HEAD"), "ref: refs/heads/feature/y\n").unwrap();

    let gd_main = format!(
        "git --git-dir={}/.git branch -m old-main",
        other_main.display()
    );
    let gd_main_push = format!(
        "git --git-dir={}/.git push --force-with-lease",
        other_main.display()
    );
    let env_main = format!(
        "GIT_DIR={}/.git git branch -m old-main",
        other_main.display()
    );
    let env_main_push = format!(
        "GIT_DIR={}/.git git push --force-with-lease",
        other_main.display()
    );
    let gd_feat = format!(
        "git --git-dir={}/.git branch -m feature/new",
        other_feat.display()
    );

    run_corpus(
        &fx,
        &[
            ("git branch --d main", GIT),
            ("git branch --de main", GIT),
            ("git push --de origin main", GIT),
            ("git push --m origin", GIT),
            ("git push --mir origin", GIT),
            ("git push --force-with-lease --al origin", GIT),
            ("git push --force-with-lease --b origin", GIT),
            ("git push --force-with-lease --br origin", GIT),
            ("git config --unset- core.hooksPath", GIT),
            ("git config --unset-a core.hooksPath", GIT),
            ("git config --un core.hooksPath", GIT),
            ("git switch --force- main", GIT),
            ("git switch --force-=main", GIT),
            (gd_main.as_str(), GIT),
            (gd_main_push.as_str(), GIT),
            (env_main.as_str(), GIT),
            (env_main_push.as_str(), GIT),
            (gd_feat.as_str(), ALLOW),
        ],
    );

    // Multi `-C` alias lookup resolves cumulative dir `a/b`, not just `b`.
    let seen_dirs: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    let root = fx.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |dir: &Path| {
        seen_dirs.borrow_mut().push(dir.to_path_buf());
        Some("alias.fp\npush --force\0".to_string())
    };
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
    };
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": "git -C a -C b fp" },
        "cwd": fx.root,
    });
    let dec = decide(payload.to_string().as_bytes(), &env);
    assert_eq!(rule(&dec), GIT);
    assert_eq!(seen_dirs.borrow().as_slice(), &[fx.root.join("a/b")]);
}

#[test]
fn pr_merge_guard_flags_positionals_and_stdin() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("gh api -p corsair repos/o/r/pulls/42/merge", PR_MERGE),
            (
                "gh api --preview corsair repos/o/r/pulls/42/merge",
                PR_MERGE,
            ),
            ("gh api --cache 1h repos/o/r/pulls/42/merge", PR_MERGE),
            ("gh api --unknown foo repos/o/r/pulls/42/merge", PR_MERGE),
            (
                "gh api graphql --input - <<< '{\"query\":\"mutation { mergePullRequest(input:{}) { clientMutationId } }\"}'",
                PR_MERGE,
            ),
            (
                "echo '{\"query\":\"mutation { mergePullRequest(input:{}) { clientMutationId } }\"}' | gh api graphql --input -",
                PR_MERGE,
            ),
            (
                "printf '%s' '{\"query\":\"mutation { mergePullRequest(input:{}) { clientMutationId } }\"}' | gh api graphql --input -",
                PR_MERGE,
            ),
            ("gh api --cache 1h repos/o/r/pulls/42", ALLOW),
            (
                "echo '{\"query\":\"query { viewer { login } }\"}' | gh api graphql --input -",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn sed_git_files_secret_reads_and_fallback_hardening() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".env.example", "SECRET=\n");
    fx.write("keystore.properties", "pass=1\n");
    fx.write(".git/config", "[core]\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            // git_files sed `--in` abbrev and `w`/`W` write commands
            ("sed --in 's/a/b/' .git/config", GIT),
            ("sed --in=.bak 's/a/b/' .git/hooks/pre-commit", GIT),
            ("sed 'w .git/hooks/pre-commit' README.md", GIT),
            ("sed 'W .git/config' README.md", GIT),
            ("sed 's/a/b/w .git/config' README.md", GIT),
            ("sed 'w .git/refs/aaf/snap' README.md", GIT),
            ("sed 'w /tmp/out.txt' .git/config", ALLOW),
            ("sed 's/world/hello/' .git/config", ALLOW),
            // secret_guard sed `r`/`R` read commands
            ("sed 'r .env' README.md", SECRET),
            ("sed -e '1R keystore.properties' README.md", SECRET),
            ("sed -n '1p; r .env' README.md", SECRET),
            ("sed 'r .env.example' README.md", ALLOW),
            ("sed 's/root/user/' README.md", ALLOW),
            // fallback hardening
            ("rm -rf . 'oops", OPAQUE),
            ("rm -rf ./ 'oops", OPAQUE),
            ("rm -rf * 'oops", OPAQUE),
            ("echo '{}' > .claude/settings.json 'oops", OPAQUE),
            ("git branch -d main 'oops", OPAQUE),
            ("git switch -C main 'oops", OPAQUE),
            ("git branch -m old-main 'oops", OPAQUE),
            ("git push -qf origin main 'oops", OPAQUE),
            ("git branch -qD main 'oops", OPAQUE),
            ("git checkout -qB main 'oops", OPAQUE),
            ("git switch -qC main 'oops", OPAQUE),
            ("git reset --ha 'oops", OPAQUE),
            ("git branch --del main 'oops", OPAQUE),
            ("git push --mir origin 'oops", OPAQUE),
            ("git commit --no-ver 'oops", OPAQUE),
            ("git push --for origin 'oops", OPAQUE),
            ("rm .git/refs/aaf/snap 'oops", OPAQUE),
            ("echo x > refs/aaf/snap 'oops", OPAQUE),
        ],
    );
}
