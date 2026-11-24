#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round-5 security fixes in `aaf-hook`.

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_hook::HookEnv;
use aaf_hook::host::{HookHost, decide_for_host};
use common::{Fixture, rule, run_corpus, run_file_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const DATA: Option<&str> = Some("data_dir_guard");
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn env_split_string_escapes_and_comments() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("env -S 'rm -rf #' /", DELETE),
            ("env -S '# comment\nrm -rf /'", DELETE),
            (r#"env -S 'rm\_-rf\_/'"#, DELETE),
            (r#"env -S 'rm -rf\cignored' /"#, DELETE),
            (r#"env -S 'sh -c "rm\_-rf\_/"'"#, DELETE),
            (r#"env -S "sh -c 'echo \'ok\'; rm -rf /'""#, DELETE),
            (r#"env -S "rm -rf 'foo\\\\' /""#, DELETE),
            (r#"env -S 'rm -rf "\c"' /"#, OPAQUE),
            (r#"env -S 'rm -rf \q' /"#, OPAQUE),
            // Benign
            ("env -S 'echo hello #' world", ALLOW),
            (r#"env -S 'echo \$HOME'"#, ALLOW),
            (r#"env -S 'echo a\_b'"#, ALLOW),
        ],
    );
}

#[test]
#[cfg(unix)]
fn expand_real_dotdot_wrappers_cd_git_file_tools_data_and_secret() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("sub_env/.env", "SECRET=2\n");
    fs::create_dir_all(fx.root.join("sub_env/deep")).unwrap();
    fs::create_dir_all(fx.root.join(".claude/sub")).unwrap();
    fs::create_dir_all(fx.root.join(".git/objects")).unwrap();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join("build")).unwrap();
    fs::create_dir_all(fx.data.join("sub")).unwrap();
    fs::write(fx.data.join("aaf.db"), "db").unwrap();

    let outside = fx.home.join("outside_r5");
    fs::create_dir_all(outside.join("sub")).unwrap();
    symlink(outside.join("sub"), fx.root.join("escape")).unwrap();
    symlink(fx.root.join(".claude/sub"), fx.root.join("claudelink")).unwrap();
    symlink(fx.root.join(".git/objects"), fx.root.join("gitlink")).unwrap();
    symlink(fx.root.join("sub_env/deep"), fx.root.join("secretlink")).unwrap();
    symlink(fx.data.join("sub"), fx.root.join("datalink")).unwrap();

    run_corpus(
        &fx,
        &[
            ("env -C escape/.. rm -rf important", DELETE),
            ("sudo -D escape/.. rm -rf important", DELETE),
            ("cd -P escape/.. && rm -rf important", DELETE),
            ("cd -P escape && cd .. && rm -rf important", DELETE),
            ("git -C claudelink/.. rm settings.json", DELETE),
            ("cat secretlink/../.env", SECRET),
            ("cat datalink/../aaf.db", DATA),
            // Default logical `cd` (`-L`) stays in repo so `build` is allowed.
            ("cd escape/.. && rm -rf build", ALLOW),
        ],
    );

    run_file_corpus(
        &fx,
        &[
            ("Read", "secretlink/../.env", SECRET),
            ("Write", "claudelink/../settings.json", DELETE),
            ("Edit", "gitlink/../hooks/pre-commit", GIT),
            ("Read", "datalink/../aaf.db", DATA),
        ],
    );

    // Multi-host `override_dir` and `apply_patch` symlink-before-`..`.
    let root = fx.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let empty_cfg = |_: &Path| Some(String::new());
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &empty_cfg,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
        in_factory_run: true,
        trusted_aaf_bins: Vec::new(),
    };
    let codex_workdir = json!({
        "tool_name": "shell_command",
        "tool_input": { "command": "rm -rf important", "workdir": "escape/.." },
        "cwd": fx.root,
    });
    let (dec, _) = decide_for_host(codex_workdir.to_string().as_bytes(), &env, HookHost::Codex);
    assert_eq!(rule(&dec), DELETE);

    let codex_patch = json!({
        "tool_name": "apply_patch",
        "tool_input": { "input": "*** Begin Patch\n*** Update File: claudelink/../settings.json\n@@\n*** End Patch\n" },
        "cwd": fx.root,
    });
    let (dec, _) = decide_for_host(codex_patch.to_string().as_bytes(), &env, HookHost::Codex);
    assert_eq!(rule(&dec), DELETE);

    let codex_patch_data = json!({
        "tool_name": "apply_patch",
        "tool_input": { "input": "*** Begin Patch\n*** Update File: datalink/../aaf.db\n@@\n*** End Patch\n" },
        "cwd": fx.root,
    });
    let (dec, _) = decide_for_host(
        codex_patch_data.to_string().as_bytes(),
        &env,
        HookHost::Codex,
    );
    assert_eq!(rule(&dec), DATA);
}

#[test]
fn heredoc_same_line_trailing_commands_and_scopes() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    run_corpus(
        &fx,
        &[
            ("cd ~ && sh <<'EOF'; cd /tmp\nrm -rf Documents\nEOF", DELETE),
            (
                "cd ~ && cat <<'EOF' | sh; cd /tmp\nrm -rf Documents\nEOF",
                DELETE,
            ),
            (
                "(cd ~ && sh <<'EOF'); cd /tmp\nrm -rf Documents\nEOF",
                DELETE,
            ),
            ("env -C ~ sh <<'EOF'; cd /tmp\nrm -rf .\nEOF", DELETE),
            ("sh <<'EOF'; rm .env\ncd /tmp\nEOF", DELETE),
            (
                "sh <<'A' && sh <<'B'; cd /tmp\ncd /tmp\nA\nrm .env\nB",
                DELETE,
            ),
            // Benign
            ("sh <<'EOF'; cd /tmp\necho ok\nEOF", ALLOW),
        ],
    );
}

#[test]
fn guarded_dirs_case_folding_symlinks_copier_globs_and_short_opts() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".autofactory/config.toml", "x = 1\n");
    fx.write("src/app.rs", "fn main() {}\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("build")).unwrap();
    #[cfg(unix)]
    symlink(fx.root.join(".claude"), fx.root.join("claude_dir_link")).unwrap();

    run_corpus(
        &fx,
        &[
            ("rm -rf .CLAUDE", DELETE),
            ("rm -rf .Autofactory", DELETE),
            ("rm -rf .CLAUDE 'oops", OPAQUE),
            #[cfg(unix)]
            ("rm -rf claude_dir_link/", DELETE),
            ("cp -r /tmp/evil/ .CLAUDE/", DELETE),
            #[cfg(unix)]
            ("cp -r /tmp/evil/ claude_dir_link/", DELETE),
            ("cp /tmp/evil/* .claude/", DELETE),
            ("cp /tmp/evil/* .CLAUDE/", DELETE),
            ("cp /tmp/nonexistent/* .", DELETE),
            // `-t` scan stops before value-taking `-S`/`-m`/`-o`/`-g`.
            ("cp -S.txt /tmp/evil .claude/settings.json", DELETE),
            ("install -oroot /tmp/evil .claude/settings.json", DELETE),
            ("install -m644 /tmp/evil .claude/settings.json", DELETE),
            ("install -Dm 644 /tmp/evil .claude/settings.json", DELETE),
            // Benign
            ("cp -r src/* build/", ALLOW),
            ("cp -S.txt README.md build/README.md", ALLOW),
        ],
    );
}

#[test]
fn printf_reuse_escapes_star_char_and_base64_pipe() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("printf '%s ' rm -rf / | sh", DELETE),
            ("printf '%c' r m ' ' - r f ' ' / | sh", DELETE),
            ("printf '%*s' 0 'rm -rf /' | sh", DELETE),
            ("printf '%.*s' 99 'rm -rf /' | sh", DELETE),
            (r#"printf '\x72\x6d -rf /' | sh"#, DELETE),
            (r#"printf '\162\155 -rf /' | sh"#, DELETE),
            (r#"printf '%b' 'rm -rf \057' | sh"#, DELETE),
            (r#"echo -e '\x72\x6d -rf /' | sh"#, DELETE),
            (r#"echo -e '\0162\0155 -rf /' | sh"#, DELETE),
            ("echo cm0gLXJmIC8K | base64 -d | sh", DELETE),
            ("printf '%s' 'cm0gLXJmIC8K' | base64 -D | bash", DELETE),
            ("base64 --decode <<< 'cm0gLXJmIC8K' | sh", DELETE),
            ("base64 -d <<'EOF' | sh\ncm0gLXJmIC8K\nEOF", DELETE),
            // Benign
            ("echo ZWNobyBvaw== | base64 -d | sh", ALLOW),
            ("printf '%s ' echo hello world | sh", ALLOW),
        ],
    );
}

#[test]
fn sed_bare_exec_and_two_positional_inplace() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fx.write("README.md", "hi\n");
    fx.write("how.env", "not a secret\n");
    run_corpus(
        &fx,
        &[
            ("echo 'rm -rf /' | sed 'e'", DELETE),
            ("sed '1e' README.md", DELETE),
            ("sed '1{e}' README.md", DELETE),
            ("sed -e 's/a/b/; e' README.md", DELETE),
            ("sed -i '' 's/a/b/' .claude/settings.json", DELETE),
            // Benign (2-positional `sed -i` must not treat filename as script)
            ("sed -i 's/a/b/' how.env", ALLOW),
            ("sed -i 's/a/b/' e", ALLOW),
        ],
    );
}

#[test]
fn interp_guard_php_case_ws_perl_ruby_space_and_python_unlink() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("README.md", "hi\n");
    fx.write("tmp.txt", "scratch\n");
    run_corpus(
        &fx,
        &[
            ("php -r \"SYSTEM('rm -rf /');\"", INTERP),
            ("php -r \"PASSTHRU('rm -rf /');\"", INTERP),
            ("php -r \"FILE_GET_CONTENTS('.env');\"", INTERP),
            ("php -r \"READFILE('.env');\"", INTERP),
            ("php -r \"FILE_GET_CONTENTS ('.env');\"", INTERP),
            ("python3 -c \"import os; os.system ('rm -rf /')\"", INTERP),
            ("python3 -c \"open ('.env').read()\"", INTERP),
            ("perl -e \"system 'rm -rf /'\"", INTERP),
            ("perl -e \"system 'rm', '-rf', '/'\"", INTERP),
            ("ruby -e \"system 'rm -rf /'\"", INTERP),
            ("ruby -e \"system 'rm', '-rf', '/'\"", INTERP),
            ("ruby -e \"exec 'git push --force'\"", INTERP),
            ("ruby -e \"File.read '.env'\"", INTERP),
            ("ruby -e \"IO.read '.env'\"", INTERP),
            ("python3 -c \"import os; os.unlink('.env')\"", INTERP),
            ("python3 -c \"import os; os.remove('.env')\"", INTERP),
            (
                "python3 -c \"import os; os.remove('.claude/settings.json')\"",
                INTERP,
            ),
            ("python3 -c \"import os; os.rmdir('.claude')\"", INTERP),
            (
                "python3 -c \"import os; os.rename('.env', '/tmp/x')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.replace('/tmp/evil', '.claude/settings.json')\"",
                INTERP,
            ),
            // Benign
            ("ruby -e \"system 'echo ok'\"", ALLOW),
            ("ruby -e \"File.read 'README.md'\"", ALLOW),
            ("python3 -c \"import os; os.unlink('tmp.txt')\"", ALLOW),
        ],
    );
}

#[test]
fn awk_system_tar_exec_and_git_c_program_keys() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("src")).unwrap();
    run_corpus(
        &fx,
        &[
            // awk system()
            ("awk 'BEGIN { system(\"rm -rf /\") }'", DELETE),
            ("gawk 'BEGIN { system(\"git push --force\") }'", GIT),
            ("mawk 'BEGIN { system (\"cat .env\") }'", SECRET),
            ("nawk 'BEGIN { system(\"rm -rf /\") }'", DELETE),
            ("awk 'BEGIN { cmd=\"rm -rf /\"; system(cmd) }'", INTERP),
            ("awk '{print $1}' README.md", ALLOW),
            ("awk 'BEGIN { system(\"echo ok\") }'", ALLOW),
            ("awk '{print \"system(rm -rf /)\"}' README.md", ALLOW),
            // tar exec options
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-action=exec='rm -rf /' README.md",
                DELETE,
            ),
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-action exec='git push --force' README.md",
                GIT,
            ),
            ("tar -xf a.tar --to-command='cat .env'", SECRET),
            (
                "tar --use-compress-program='rm -rf /' -cf out.tar src",
                DELETE,
            ),
            ("tar -I 'rm -rf /' -cf out.tar src", DELETE),
            ("tar -czf out.tar.gz src", ALLOW),
            ("tar -I 'gzip -9' -cf out.tar.gz src", ALLOW),
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-action=dot README.md",
                ALLOW,
            ),
            // git -c program keys
            ("git -c core.pager='rm -rf /' log", GIT),
            ("git -c core.sshCommand='rm -rf /' fetch", GIT),
            ("git -c core.fsmonitor='rm -rf /' status", GIT),
            ("git -c core.editor='rm -rf /' commit", GIT),
            ("git -c sequence.editor='cat .env' rebase -i HEAD~1", GIT),
            ("git -c credential.helper='!rm -rf /' push", GIT),
            ("git -c core.pager=cat log", ALLOW),
            ("git -c core.pager=false log", ALLOW),
            ("git -c core.editor=vim commit", ALLOW),
        ],
    );
}
