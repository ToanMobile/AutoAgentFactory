#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round-6 security fixes in `aaf-hook`.

mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const HUMAN: Option<&str> = Some("human_action_guard");
const INTERP: Option<&str> = Some("interp_guard");
const SECRET: Option<&str> = Some("secret_guard");

#[test]
fn sed_bare_exec_with_trailing_cmds_and_subst_exec_flag() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            ("sed '1{e}p' README.md", DELETE),
            ("sed '1{e}; p' README.md", DELETE),
            ("sed '1e; p' README.md", DELETE),
            ("echo 'rm -rf /' | sed 's/^//e'", DELETE),
            ("echo 'rm -rf /' | sed 's/.*/&/e'", DELETE),
            ("sed 's/x/y/e' README.md", DELETE),
            // Explicit dangerous payloads keep their specific guard rule.
            ("sed 's/.*/git push --force/e' README.md", GIT),
            ("sed 's/.*/cat .env/e' README.md", SECRET),
            ("sed '1{e}; s/.*/git push --force/e' README.md", GIT),
            // Benign
            ("sed '1{p}q' README.md", ALLOW),
            ("sed 's/a/b/g' README.md", ALLOW),
            ("sed -i 's/a/b/' e", ALLOW),
        ],
    );
}

#[test]
fn awk_regex_literals_and_octal_hex_escapes() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            (r#"awk '/"/ { system("rm -rf /") }' README.md"#, DELETE),
            (r#"awk '$0 ~ /"/ { system("rm -rf /") }' README.md"#, DELETE),
            (
                r#"awk 'BEGIN { if ($0 !~ /"/) system("git push --force") }'"#,
                GIT,
            ),
            (r#"awk '/["\/]/ { system("cat .env") }' README.md"#, SECRET),
            (r#"awk 'BEGIN { print /"/; system("rm -rf /") }'"#, DELETE),
            (r#"awk 'BEGIN { system("\162\155 -rf /") }'"#, DELETE),
            (r#"awk 'BEGIN { system("\x72\x6d -rf /") }'"#, DELETE),
            (
                r#"gawk 'BEGIN { system("\x63\x61\x74 \x2e\x65\x6e\x76") }'"#,
                SECRET,
            ),
            // Benign
            (r#"awk '/"/ { print $1 }' README.md"#, ALLOW),
            ("awk 'BEGIN { a = 10 / 2; b = a / 5; print a, b }'", ALLOW),
            (r#"awk 'BEGIN { system("\x65\x63\x68\x6f ok") }'"#, ALLOW),
            (r#"awk '{ print "system(\"rm -rf /\")" }' README.md"#, ALLOW),
        ],
    );
}

#[test]
fn printf_precision_unicode_and_base64_flags_and_cat_heredoc() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("printf '%.2s %s' 'rmXXX' '-rf /' | sh", DELETE),
            ("printf '%.0srm -rf /' 'echo ' | sh", DELETE),
            ("printf '%.srm -rf /' 'echo ' | sh", DELETE),
            ("printf '%.*s %s' 2 'rmXXX' '-rf /' | sh", DELETE),
            ("printf '%.*srm -rf /' 0 'echo ' | sh", DELETE),
            (r#"printf '%.2b %s' '\x72\x6dXXX' '-rf /' | sh"#, DELETE),
            (r#"printf '\u0072\u006d -rf /' | sh"#, DELETE),
            (r#"printf '\U00000072\U0000006d -rf /' | sh"#, DELETE),
            (r#"printf '%b' '\u0072\u006d -rf /' | sh"#, DELETE),
            ("echo cm0gLXJmIC8K | base64 -id | sh", DELETE),
            ("echo cm0gLXJmIC8K | base64 --dec | sh", DELETE),
            ("echo cm0gLXJmIC8K | base64 --d | sh", DELETE),
            ("cat <<'EOF' | base64 -d | sh\ncm0gLXJmIC8K\nEOF", DELETE),
            ("cat <<'EOF' | base64 -id | bash\ncm0gLXJmIC8K\nEOF", DELETE),
            ("cat <<'EOF' | base64 --dec | sh\ncm0gLXJmIC8K\nEOF", DELETE),
            // Benign
            ("printf '%.4s %s' 'echoXXX' 'hello' | sh", ALLOW),
            (r#"printf '\u0065\u0063\u0068\u006f ok' | sh"#, ALLOW),
            ("echo ZWNobyBvaw== | base64 -id | sh", ALLOW),
            ("echo ZWNobyBvaw== | base64 --dec | sh", ALLOW),
            ("cat <<'EOF' | base64 -d | sh\nZWNobyBvaw==\nEOF", ALLOW),
        ],
    );
}

#[test]
#[cfg(unix)]
fn delete_guard_glob_with_symlink_before_dotdot() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".claude/notes.txt", "scratch\n");
    fs::create_dir_all(fx.root.join(".claude/sub")).unwrap();
    symlink(fx.root.join(".claude/sub"), fx.root.join("claudelink")).unwrap();

    run_corpus(
        &fx,
        &[
            ("rm claudelink/../settings.*", DELETE),
            ("rm -f claudelink/../*.json", DELETE),
            // Benign: only matches non-protected file in `.claude/`.
            ("rm claudelink/../notes.*", ALLOW),
        ],
    );
}

#[test]
fn tar_old_style_keyletters_and_abbreviated_long_options() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("src")).unwrap();

    run_corpus(
        &fx,
        &[
            ("tar cI 'rm -rf /' src", DELETE),
            ("tar cfI out.tar 'rm -rf /' src", DELETE),
            ("tar cIf 'git push --force' out.tar src", GIT),
            ("tar cf out.tar --use-compress-pro='rm -rf /' src", DELETE),
            ("tar --us 'rm -rf /' -cf out.tar src", DELETE),
            ("tar -xf a.tar --to-c='cat .env'", SECRET),
            ("tar -xf a.tar --to-comm 'rm -rf /'", DELETE),
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-act=exec='rm -rf /' README.md",
                DELETE,
            ),
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-a exec='git push --force' README.md",
                GIT,
            ),
            ("tar --rsh-c='rm -rf /' -cf host:out.tar src", DELETE),
            ("tar --rs 'cat .env' -cf host:out.tar src", SECRET),
            // Benign
            ("tar czf out.tar.gz src", ALLOW),
            ("tar cfI out.tar 'gzip -9' src", ALLOW),
            (
                "tar -cf /dev/null --checkpoint=1 --checkpoint-act=dot README.md",
                ALLOW,
            ),
            ("tar -xf a.tar --to-stdout", ALLOW),
        ],
    );
}

#[test]
fn git_config_env_key_and_parameters_program_keys() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("git --config-env=core.pager=EVIL log", GIT),
            ("git --config-env core.sshCommand=EVIL fetch", GIT),
            ("P='rm -rf /' git --config-env=core.pager=P log", GIT),
            ("P=\"$EVIL\" git --config-env=core.pager=P log", GIT),
            ("P=$(cat cmd) git --config-env=core.pager=P log", GIT),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0='rm -rf /' git log",
                GIT,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.fsmonitor git status",
                GIT,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.sshCommand GIT_CONFIG_VALUE_0=\"$CMD\" git fetch",
                GIT,
            ),
            ("export GIT_CONFIG_KEY_0=core.pager", GIT),
            (
                "GIT_CONFIG_PARAMETERS=\"'core.pager=rm -rf /'\" git log",
                GIT,
            ),
            (
                "GIT_CONFIG_PARAMETERS=\"'user.name=x' 'core.sshCommand=rm -rf /'\" git fetch",
                GIT,
            ),
            (
                "GIT_CONFIG_PARAMETERS=\"'filter.x.clean=rm -rf /'\" git checkout",
                GIT,
            ),
            (
                "export GIT_CONFIG_PARAMETERS=\"'core.fsmonitor=evil'\"",
                GIT,
            ),
            // Benign
            ("git --config-env=http.extraheader=AUTH fetch", ALLOW),
            ("P=cat git --config-env=core.pager=P log", ALLOW),
            ("P=false git --config-env=core.pager=P log", ALLOW),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=cat git log",
                ALLOW,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=false git log",
                ALLOW,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=user.name GIT_CONFIG_VALUE_0=alice git commit -m m",
                ALLOW,
            ),
            (
                "GIT_CONFIG_PARAMETERS=\"'user.name=alice' 'color.ui=false'\" git status",
                ALLOW,
            ),
            (
                "echo GIT_CONFIG_PARAMETERS=\"'core.pager=rm -rf /'\"",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn interp_guard_perl_ruby_space_call_file_reads() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    run_corpus(
        &fx,
        &[
            (
                "perl -e \"open my \\$f, '<', '.env'; print <\\$f>\"",
                INTERP,
            ),
            ("ruby -e \"puts open '.env'\"", INTERP),
            ("ruby -e \"puts File.open '.env', &:read\"", INTERP),
            ("ruby -e \"puts File.readlines '.env'\"", INTERP),
            ("ruby -e \"puts File.binread '.env'\"", INTERP),
            ("ruby -e \"puts IO.readlines '.env'\"", INTERP),
            ("ruby -e \"puts IO.binread '.env'\"", INTERP),
            ("ruby -e \"puts File.readlines('.env')\"", INTERP),
            ("ruby -e \"puts IO.binread('.env')\"", INTERP),
            // Benign
            (
                "perl -e \"open my \\$f, '<', 'README.md'; print <\\$f>\"",
                ALLOW,
            ),
            ("ruby -e \"puts File.open 'README.md', &:read\"", ALLOW),
            ("ruby -e \"puts File.readlines 'README.md'\"", ALLOW),
            ("ruby -e \"puts IO.binread 'README.md'\"", ALLOW),
        ],
    );
}

#[test]
fn tar_incremental_g_boolean_and_awk_ctrl_paren_regex() {
    let fx = Fixture::new();
    fx.write(".env", "SECRET=1\n");
    fx.write("README.md", "hi\n");
    fs::create_dir_all(fx.root.join("src")).unwrap();

    run_corpus(
        &fx,
        &[
            // GNU `tar -G` (--incremental) takes no argument.
            ("tar -G -I 'rm -rf /' -cf a.tar src", DELETE),
            ("tar -GI 'rm -rf /' -cf a.tar src", DELETE),
            ("tar cGI 'rm -rf /' src", DELETE),
            ("tar cGfI out.tar 'git push --force' src", GIT),
            // `awk` `if`/`while`/`for` `(...)` followed by `/.../` regex literal.
            (r#"awk 'BEGIN { if (1) /"/; system("rm -rf /") }'"#, DELETE),
            (
                r#"awk 'BEGIN { while (i++ < 1) /"/; system("rm -rf /") }'"#,
                DELETE,
            ),
            (
                r#"awk 'BEGIN { for (i = 0; i < 1; i++) /"/; system("git push --force") }'"#,
                GIT,
            ),
            (
                r#"awk 'BEGIN { if ((10 + 2) / 3) /"/; system("cat .env") }'"#,
                SECRET,
            ),
            // Benign
            ("tar -G -cf a.tar src", ALLOW),
            ("tar cGf a.tar src", ALLOW),
            (
                r#"awk 'BEGIN { a = (10 + 2) / 3; print "system(\"rm -rf /\")" }'"#,
                ALLOW,
            ),
            (r#"awk 'BEGIN { if (1) print "ok" }'"#, ALLOW),
        ],
    );
}

#[test]
fn command_substitution_retains_dynamic_dollar_marker() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("P=\"x$(cat cmd)\" git --config-env=core.pager=P log", GIT),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=\"x$(cat cmd)\" git log",
                GIT,
            ),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=\"core.$(cat k)\" GIT_CONFIG_VALUE_0=x git log",
                GIT,
            ),
            ("git -c \"core.$(cat k)=rm -rf /\" log", GIT),
            ("git push --force-with-lease origin \"ma$(echo in)\"", GIT),
            ("aaf \"app$(echo rove)\"", HUMAN),
            // Benign
            (
                "git commit -m \"$(cat <<'EOF'\nfix: don't run git push --force\nEOF\n)\"",
                ALLOW,
            ),
            ("git push origin \"$(git branch --show-current)\"", ALLOW),
            ("echo \"hi $(date)\"", ALLOW),
            (
                "export JAVA_HOME=$(mise where java); aaf slot -- ./gradlew b",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn multihop_heredoc_pipeline_and_base64_ignore_garbage() {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");

    run_corpus(
        &fx,
        &[
            // Multi-hop heredoc pipeline (`cat <<'EOF' | base64 -d | python3` / `env -C` / `env -i`).
            (
                "cat <<'EOF' | base64 -d | python3\naW1wb3J0IG9zOyBvcy5zeXN0ZW0oInJtIC1yZiAvIikK\nEOF",
                INTERP,
            ),
            (
                "cat <<'EOF' | base64 -d | env -C .claude sh\ncm0gc2V0dGluZ3MuanNvbgo=\nEOF",
                DELETE,
            ),
            (
                "cat <<'EOF' | base64 -d | env -i sh\nYWFmIGRvY3Rvcgo=\nEOF",
                HUMAN,
            ),
            ("cat <<'EOF' | sh | python3\nrm -rf /\nEOF", DELETE),
            // GNU `base64 -id` / `--ignore-garbage` strips non-base64 garbage bytes.
            ("echo 'cm0g!@#LXJmIC8K' | base64 -id | sh", DELETE),
            (
                "echo 'cm0g!LXJm!IC8K==!!!' | base64 --ignore-garbage -d | sh",
                DELETE,
            ),
            ("echo 'cm0g@LXJm#IC8K' | base64 --ig --dec | bash", DELETE),
            (
                "cat <<'EOF' | base64 -id | sh\ncm0g!@#LXJmIC8K\nEOF",
                DELETE,
            ),
            (
                "cat <<'EOF' | base64 -id | python3\naW1wb3J0!@#IG9zOyBvcy5zeXN0ZW0oInJtIC1yZiAvIikK\nEOF",
                INTERP,
            ),
            // Benign
            (
                "cat <<'EOF' | base64 -d | python3\ncHJpbnQoImhlbGxvIikK\nEOF",
                ALLOW,
            ),
            (
                "cat <<'EOF' | base64 -d | env -C .claude sh\nZWNobyBvawo=\nEOF",
                ALLOW,
            ),
            ("echo 'ZWNoby!@#Bvawo=' | base64 -id | sh", ALLOW),
            (
                "echo 'ZWNoby!Bvawo=' | base64 --ignore-garbage -d | sh",
                ALLOW,
            ),
            (
                "cat <<'EOF' | base64 -id | python3\ncHJpbnQo!ImhlbGxvIikK\nEOF",
                ALLOW,
            ),
        ],
    );
}

#[test]
#[cfg(unix)]
fn delete_guard_glob_dotdot_nonexistent_and_symlink_walk() {
    let fx = Fixture::new();
    fx.write("evil", "payload\n");
    fx.write("README.md", "hi\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".claude/notes.txt", "scratch\n");
    fs::create_dir_all(fx.root.join("foo1")).unwrap();
    fs::create_dir_all(fx.root.join(".claude/sub")).unwrap();
    fs::create_dir_all(fx.root.join("sub/safe1")).unwrap();
    symlink(fx.root.join(".claude/sub"), fx.root.join("claudelink")).unwrap();
    symlink(fx.root.join(".claude/sub"), fx.root.join("sub/claudelink")).unwrap();

    run_corpus(
        &fx,
        &[
            // `.env` does not exist yet: expanded path `<root>/.env` has no glob.
            ("cp evil foo*/../.env", DELETE),
            ("echo x > foo*/../.env", DELETE),
            // Glob matches symlink `claudelink -> .claude/sub` before `..`.
            ("rm claude*/../settings.json", DELETE),
            ("rm claude*/../settings.*", DELETE),
            ("rm -rf sub/claude*/..", DELETE),
            // Benign
            ("cp evil foo*/../README.md", ALLOW),
            ("rm claude*/../notes.txt", ALLOW),
            ("rm -rf sub/safe*/..", ALLOW),
        ],
    );
}

#[test]
#[cfg(unix)]
fn round8_awk_postfix_multi_base64_heredoc_and_copier_glob_dst() {
    let fx = Fixture::new();
    fx.write("settings.json", "{}\n");
    fx.write("settings.local.json", "{}\n");
    fx.write("notes.txt", "ok\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("src/a.txt", "ok\n");
    fs::create_dir_all(fx.root.join(".claude/sub")).unwrap();
    fs::create_dir_all(fx.root.join("build1")).unwrap();
    symlink(fx.root.join(".claude/sub"), fx.root.join("claudelink")).unwrap();

    run_corpus(
        &fx,
        &[
            // awk postfix `++`/`--` before `/` is division, not regex.
            ("awk 'BEGIN { a++ / 2; system(\"rm -rf /\") }'", DELETE),
            ("awk 'BEGIN { a-- / 2; system(\"git push --force\") }'", GIT),
            // Multi-stage `base64 -d | base64 -d | sh` heredoc (`Y20wZ0xYSm1JQzhL` -> `cm0gLXJmIC8K` -> `rm -rf /`).
            (
                "cat <<'EOF' | base64 -d | base64 -d | sh\nY20wZ0xYSm1JQzhL\nEOF",
                DELETE,
            ),
            // Copier with glob destination directory targeting guarded dir.
            ("cp settings.json .claude*", DELETE),
            ("cp settings.json .claude*/", DELETE),
            ("cp -t .claude* settings.json", DELETE),
            ("install settings.json .claude*", DELETE),
            ("rsync settings.json .claude*/", DELETE),
            ("cp settings.local.json claude*/..", DELETE),
            ("cp -r src/* .claude*", DELETE),
            ("cp -r src/ .claude*", DELETE),
            // Benign
            ("awk 'BEGIN { a++ / 2; print a }'", ALLOW),
            (
                "cat <<'EOF' | base64 -d | base64 -d | sh\nWldOb2J5QnZhd289\nEOF",
                ALLOW,
            ),
            ("cp notes.txt .claude*", ALLOW),
            ("cp settings.json build*", ALLOW),
        ],
    );
}
