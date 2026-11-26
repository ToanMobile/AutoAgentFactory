#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-7 hook findings: a hoisted skip
//! list is not a table of paths (R7-H6-1), a newline after `||`, `|` or
//! `&&` continues the list (R7-H6-2), a `CDPATH` clear in a brace group
//! that may not run in this shell (R7-E10-1), a long pipeline (R7-H6-3)
//! and literals that grow at each hop (R7-H6-4).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Fixture, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join("src/build")).unwrap();
    fs::create_dir_all(fx.root.join("crates/aaf-hook/src")).unwrap();
    fx
}

fn py(body: &str) -> String {
    format!("python3 - <<'EOF'\n{body}EOF")
}

/// A walk that skips the names of `skip` and touches every other file.
fn walk(skip: &str) -> String {
    py(&format!(
        "import os\n{skip}\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in SKIP]\n    for f in files:\n        open(os.path.join(root, f), 'a').close()\n"
    ))
}

/// R7-H6-1: a skip list assigned to a name, or handed to `set(`/`Set(`,
/// names what is left out, not paths below `.git`.
#[test]
fn r7_h6_1_hoisted_skip_list_is_not_a_table() {
    let fx = fixture();
    let allow = [
        walk("SKIP = {'.git', 'node_modules', 'logs'}"),
        walk("SKIP = ['.git', 'hooks']"),
        walk("SKIP = ('.git', 'logs', 'objects')"),
        walk("SKIP = set(['.git', 'hooks'])"),
        walk("SKIP = frozenset(('.git', 'logs'))"),
        py(
            "import os\nexcluded = ['.git', 'hooks']\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in excluded]\n    for f in files:\n        open(os.path.join(root, f), 'a').close()\n",
        ),
        py(
            "import os\nignored_dirs = ['.git', 'logs', 'refs']\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in ignored_dirs]\n    for f in files:\n        open(os.path.join(root, f), 'a').close()\n",
        ),
        "node -e 'const fs=require(\"fs\"),path=require(\"path\");const SKIP = new Set([\".git\",\"node_modules\",\"hooks\"]);function walk(d){for(const e of fs.readdirSync(d,{withFileTypes:true})){if(SKIP.has(e.name))continue;const p=path.join(d,e.name);if(e.isDirectory())walk(p);else fs.appendFileSync(p,\"\")}}walk(\".\")'".to_string(),
    ];
    let rows: Vec<(&str, Option<&str>)> = allow.iter().map(|row| (row.as_str(), ALLOW)).collect();
    run_corpus(&fx, &rows);
}

/// Runs the installed hook binary on `command` from a fresh git checkout
/// `base/work` with `CDPATH` set to `cdpath`; its exit code and how long
/// it took.
fn run_hook(base: &Path, command: &str, cdpath: Option<&Path>) -> (Option<i32>, Duration) {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": base.join("work"),
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aaf-hook"));
    cmd.env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AAF_DATA_DIR", base.join("data"))
        .env("HOME", base.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cdpath) = cdpath {
        cmd.env("CDPATH", cdpath);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    let code = child.wait().unwrap().code();
    (code, started.elapsed())
}

/// A temp dir holding `data`, `home` and a git checkout `work`.
fn checkout() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["data", "home", "work/src", "work/etc"] {
        fs::create_dir_all(base.join(sub)).unwrap();
    }
    let status = Command::new("git")
        .args(["init", "-q", "-b", "feature/x"])
        .current_dir(base.join("work"))
        .env("HOME", base.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    (dir, base)
}

fn expect_codes(base: &Path, cdpath: Option<&Path>, rows: &[(&str, i32)]) {
    let mut wrong = Vec::new();
    for (command, expected) in rows {
        let (got, _) = run_hook(base, command, cdpath);
        if got != Some(*expected) {
            wrong.push(format!("`{command}`: {got:?}, want {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// R7-H6-2: a newline right after `||`, `|` or `&&` continues the list.
#[test]
fn r7_h6_2_newline_after_list_operator_continues_the_list() {
    let fx = fixture();
    let deny = [
        "true ||\ncd crates/aaf-hook/src && rm -rf ../../../x",
        "true |\ncd crates/aaf-hook/src && rm -rf ../../../x",
        "true || \n cd crates/aaf-hook/src && rm -rf ../../../x",
    ];
    let mut wrong = Vec::new();
    for command in deny {
        if common::rule(&fx.bash(command)).is_none() {
            wrong.push(format!("{command:?} allowed"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    run_corpus(
        &fx,
        &[
            ("make &&\ncd src && rm -rf build", ALLOW),
            ("make ||\necho failed", ALLOW),
            ("cat <<'X' |\nbody\nX\nwc -l", ALLOW),
        ],
    );
    let (_dir, base) = checkout();
    expect_codes(
        &base,
        Some(Path::new("/")),
        &[
            ("false &&\nunset CDPATH\ncd etc && rm -rf foo", 2),
            ("unset CDPATH\ncd etc && rm -rf foo", 0),
        ],
    );
}

/// R7-E10-1: a `CDPATH` clear inside a brace group that is conditional,
/// piped or in the background may not run in this shell.
#[test]
fn r7_e10_1_cdpath_clear_in_conditional_group_keeps_inherited_value() {
    let (_dir, base) = checkout();
    fs::create_dir_all(base.join("data")).unwrap();
    expect_codes(
        &base,
        Some(&base),
        &[
            ("false && { :; CDPATH=; }; cd data && rm -rf *", 2),
            ("true || { echo; unset CDPATH; }; cd data && rm -rf *", 2),
            ("true | { :; CDPATH=; }; cd data && rm -rf *", 2),
            ("{ CDPATH=; } | cat; cd data && rm -rf *", 2),
            ("{ :; CDPATH=; } | cat; cd data && rm -rf *", 2),
            ("{ :; CDPATH=; } & cd data && rm -rf *", 2),
            (
                "false && {\n echo x\n unset CDPATH\n}\ncd data && rm -rf *",
                2,
            ),
            (
                "[ -n \"$CI\" ] && { echo ci; unset CDPATH; }; cd data && rm -rf *",
                2,
            ),
            ("false && { { :; }; CDPATH=; }; cd data && rm -rf *", 2),
            // Still cleared.
            ("unset CDPATH\ncd data && rm -rf *", 0),
            ("export CDPATH=; cd data && rm -rf *", 0),
            ("{ unset CDPATH; }; cd data && rm -rf *", 0),
            ("{ :; unset CDPATH; }; cd data && rm -rf *", 0),
            ("{ :; unset CDPATH; } > /dev/null; cd data && rm -rf *", 0),
            ("{ :; } | cat; unset CDPATH; cd data && rm -rf *", 0),
        ],
    );
}

/// R8 (Lane 6): `time` and `coproc` before a `{ … }` group. A timed group
/// piped or put in the background, and anything `coproc` starts, does not
/// run in this shell.
#[test]
fn r8_time_and_coproc_groups_keep_inherited_cdpath() {
    let (_dir, base) = checkout();
    fs::create_dir_all(base.join("data")).unwrap();
    expect_codes(
        &base,
        Some(&base),
        &[
            ("time { unset CDPATH; } | cat; cd data && rm -rf *", 2),
            ("time -p { unset CDPATH; } | cat; cd data && rm -rf *", 2),
            ("time { :; CDPATH=; } & cd data && rm -rf *", 2),
            ("coproc { unset CDPATH; }; cd data && rm -rf *", 2),
            ("coproc NAME { unset CDPATH; }; cd data && rm -rf *", 2),
            ("coproc unset CDPATH; cd data && rm -rf *", 2),
            // Still cleared.
            ("time { unset CDPATH; }; cd data && rm -rf *", 0),
            ("time -p { :; unset CDPATH; }; cd data && rm -rf *", 0),
        ],
    );
}

/// How many feeders stand behind `cmd`, one pipe stage each.
fn feeder_depth(cmd: &aaf_hook::shell::Cmd) -> usize {
    let mut depth = 0;
    let mut at = cmd;
    while let Some(feeder) = at.feeder.as_deref() {
        depth += 1;
        at = feeder;
    }
    depth
}

/// R7-H6-3: a very long pipeline is judged, quickly, without a crash.
/// The cost is held by structure, not by the clock: each stage keeps only
/// `MAX_DEPTH` stages of its feeders, so the copies made while splitting
/// stay linear in the length of the pipeline. The run of the built hook
/// only holds it to its contract (exit 0 or 2, well inside its timeout).
#[test]
fn r7_h6_3_long_pipeline_does_not_crash() {
    let script = aaf_hook::shell::parse(&format!("{}t", "t|".repeat(2000)));
    assert_eq!(script.cmds.len(), 2001);
    let deepest = script.cmds.iter().map(feeder_depth).max().unwrap();
    assert!(
        deepest <= aaf_hook::shell::MAX_DEPTH + 1,
        "a stage keeps {deepest} feeders"
    );
    let (_dir, base) = checkout();
    let command = format!("{}t", "t|".repeat(30_000));
    let (code, took) = run_hook(&base, &command, None);
    assert!(matches!(code, Some(0 | 2)), "exit {code:?}");
    assert!(took < Duration::from_secs(20), "took {took:?}");
}

/// R7-H6-4: literals that grow at each hop of a variable are bounded. The
/// bound itself is held by the unit test
/// `interp_guard::writes::parse::tests::literals_past_the_byte_bound_are_not_sure`;
/// here only the contract.
#[test]
fn r7_h6_4_growing_literals_are_bounded() {
    let (_dir, base) = checkout();
    let command = "node -e '\"\"r=ignore=`p.g/[ cg`e=chdir(+open(\"{let **\"r/ignore,wi))'";
    let (code, took) = run_hook(&base, command, None);
    assert!(matches!(code, Some(0 | 2)), "exit {code:?}");
    assert!(took < Duration::from_secs(20), "took {took:?}");
}
