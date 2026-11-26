#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-072 rework 2: characters inside the attached value
//! of Perl `-C`/`-D`/`-i` and Ruby `-X`/`-E`/`-x` must not be read as
//! `I`/`M`/`m`/`r`/`C` value-taking flags that swallow or hide `-e`.

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const INTERP: Option<&str> = Some("interp_guard");

const PERL_HOOK: &str = "'BEGIN { open(F, \">.git/hooks/pre-commit\") }'";
const RUBY_HOOK: &str = "\"File.write('.git/hooks/pre-commit', 'x')\"";
const RUBY_PUSH: &str = "\"exec 'git push --force'\"";

fn fixture_with_hooks() -> Fixture {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fx
}

fn denied(fx: &Fixture, prefixes: &[&str], payload: &str) {
    let cmds: Vec<String> = prefixes.iter().map(|p| format!("{p} {payload}")).collect();
    let corpus: Vec<(&str, Option<&str>)> = cmds.iter().map(|c| (c.as_str(), INTERP)).collect();
    run_corpus(fx, &corpus);
}

#[test]
fn perl_detached_valued_flags_do_not_swallow_inline_code() {
    let fx = fixture_with_hooks();
    denied(
        &fx,
        &[
            "perl -CI -e",
            "perl -CSI -e",
            "perl -Dm -e",
            "perl -DI -e",
            "perl -iI -e",
            "perl -C -e",
        ],
        PERL_HOOK,
    );
}

#[test]
fn perl_same_word_valued_flags_restart_cluster_after_space() {
    let fx = fixture_with_hooks();
    denied(
        &fx,
        &[
            "perl '-CI -e'",
            "perl '-Dm -e'",
            "perl '-i -e'",
            "perl '-i.bak -e'",
            "perl '-CSD -le'",
        ],
        PERL_HOOK,
    );
}

#[test]
fn ruby_attached_dir_and_encoding_values_do_not_hide_inline_code() {
    let fx = fixture_with_hooks();
    let prefixes = [
        "ruby -X/usr -e",
        "ruby -x/usr -e",
        "ruby -EEUc-kr -e",
        "ruby -EUTF-8:UTF-8 -e",
        "ruby -KuX/usr -e",
        "ruby -x -e",
        "ruby -X /usr -e",
        "ruby -E UTF-8 -e",
    ];
    denied(&fx, &prefixes, RUBY_HOOK);
    denied(&fx, &prefixes, RUBY_PUSH);
}

/// `reads_program_from_stdin` walks clusters with the same rules: `-CI` does
/// not swallow `script.pl`, so the heredoc is data for a script, not the
/// program, and is judged as shell text.
#[test]
fn stdin_program_scan_uses_same_valued_flag_rules() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -CI script.pl <<'EOF'\nrm -rf ~\nEOF", DELETE),
            ("perl -DI script.pl <<'EOF'\nrm -rf ~\nEOF", DELETE),
            ("ruby -x/usr/r script.rb <<'EOF'\nrm -rf ~\nEOF", DELETE),
            ("ruby -X /tmp script.rb <<'EOF'\nrm -rf ~\nEOF", DELETE),
        ],
    );
}

#[test]
fn valid_valued_flag_commands_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("perl -F: -lane 'print $F[0]' f", ALLOW),
            ("perl -ne 'print if /x/' f", ALLOW),
            ("ruby -ne 'puts $_' f", ALLOW),
            ("ruby -X /tmp -e 'puts 1'", ALLOW),
            ("perl -CSD -e 'print 1'", ALLOW),
            ("perl -CSD -le 'print 1'", ALLOW),
            ("ruby -EUTF-8 -e 'puts 1'", ALLOW),
        ],
    );
}
