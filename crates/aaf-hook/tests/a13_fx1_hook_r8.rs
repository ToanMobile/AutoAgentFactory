#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-8 hook findings: a heredoc on a
//! `cat <<'EOF' |` line whose interpreter stands after the body (R8-H6-1),
//! and a Python program that changes directory too often (O13-R8-3).

mod common;

use std::fs;

use aaf_hook::Decision;
use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join("src/build")).unwrap();
    fx
}

/// `cat <<'EOF' |`, the body, then the interpreter alone on its line.
fn fed(body: &str, interp: &str) -> String {
    format!("cat <<'EOF' |\n{body}EOF\n{interp}")
}

/// R8-H6-1: the body reaches the interpreter after the delimiter line
/// just as it does with `cat <<'EOF' | python3` on one line.
#[test]
fn r8_h6_1_heredoc_fed_to_interpreter_after_body_is_inspected() {
    let fx = fixture();
    let deny = [
        (
            "import json\np = '.claude/settings.json'\njson.dump({}, open(p, 'w'))\n",
            "python3",
        ),
        (
            "p = '.git/config'\nopen(p, 'a').write('[core]\\n')\n",
            "python3",
        ),
        (
            "require('fs').writeFileSync('.claude/settings.json', '{}')\n",
            "node",
        ),
        ("import shutil\nshutil.rmtree('../x')\n", "python3"),
        ("open('.git/config', 'w').write('')\n", "python3 -"),
    ];
    let mut wrong = Vec::new();
    for (body, interp) in deny {
        let command = fed(body, interp);
        if common::rule(&fx.bash(&command)).is_none() {
            wrong.push(format!("{command:?} allowed"));
        }
        // The same program on one line is denied too.
        let one_line = format!("cat <<'EOF' | {interp}\n{body}EOF");
        if common::rule(&fx.bash(&one_line)).is_none() {
            wrong.push(format!("one-line {one_line:?} allowed"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    run_corpus(
        &fx,
        &[
            ("cat <<'X' |\nbody\nX\nwc -l", ALLOW),
            (&fed("print('hello')\n", "python3"), ALLOW),
            (
                &fed("import sys\nprint(sum(1 for _ in sys.argv))\n", "python3"),
                ALLOW,
            ),
            (
                &fed("open('src/build/out.txt', 'w').write('x')\n", "python3"),
                ALLOW,
            ),
            (&fed("console.log('hi')\n", "node"), ALLOW),
        ],
    );
}

/// O13-R8-3: a program that changes directory more often than the
/// analysis follows is denied, and says why.
#[test]
fn o13_r8_3_chdir_budget_denies_with_reason() {
    let fx = fixture();
    let mut body = String::from("import os\n");
    for n in 0..400 {
        body.push_str(&format!("os.chdir('d{n}')\n"));
    }
    for n in 0..400 {
        body.push_str(&format!("open('f{n}.txt', 'w')\n"));
    }
    let command = format!("python3 - <<'EOF'\n{body}EOF");
    match fx.bash(&command) {
        Decision::Deny(denial) => assert!(
            denial.reason.contains("too often"),
            "reason: {}",
            denial.reason
        ),
        Decision::Allow => panic!("allowed"),
    }
}
