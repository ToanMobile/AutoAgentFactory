#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-4 hook findings: R4-H6-1/2
//! (interpreter writes: skipped `.git`, comparison operands, joins), O11-9/10
//! (brace expansion cost and depth), R4-H6-3 (`CDPATH` order and `unset`),
//! R4-H6-4 (the external `time`) and R4-H6-5 (more launchers, script from
//! `/dev/fd/N`).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Fixture, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const INTERP: Option<&str> = Some("interp_guard");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join("src")).unwrap();
    fx
}

fn heredoc(interp: &str, body: &str) -> String {
    format!("{interp} - <<'EOF'\n{body}EOF")
}

/// R4-H6-1: a script that walks the tree and skips `.git` is not writing
/// there; `.git` as a directory a copy or move fills still is.
#[test]
fn r4_h6_1_skipping_git_dir_is_not_writing_it() {
    let fx = fixture();
    let walk = heredoc(
        "python3",
        "import os\nfor root, ds, fs in os.walk('.'):\n    ds[:] = [d for d in ds if d != '.git']\n    for f in fs:\n        if f.endswith('.py'):\n            p = os.path.join(root, f)\n            open(p, 'a').write('\\n')\n",
    );
    let walk_in = heredoc(
        "python3",
        "import os\nfor r, ds, fs in os.walk('.'):\n    if '.git' in r:\n        continue\n    for f in fs:\n        p = os.path.join(r, f)\n        t = open(p).read()\n        open(p, 'w').write(t.replace('foo', 'bar'))\n",
    );
    let ignore_set = heredoc(
        "python3",
        "import os\nIGNORE = {'.git', 'node_modules', 'build'}\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in IGNORE]\n    for name in files:\n        if name.endswith('.md'):\n            path = os.path.join(root, name)\n            with open(path, 'a') as fh:\n                fh.write('\\n')\n",
    );
    let parts = heredoc(
        "python3",
        "from pathlib import Path\nfor p in Path('.').rglob('*.py'):\n    if '.git' in p.parts:\n        continue\n    p.write_text(p.read_text().replace('foo', 'bar'))\n",
    );
    let node = "node -e \"const fs=require('fs'),path=require('path');const skip=new Set(['.git','node_modules']);function walk(d){for(const e of fs.readdirSync(d,{withFileTypes:true})){if(skip.has(e.name))continue;const p=path.join(d,e.name);if(e.isDirectory())walk(p);else if(p.endsWith('.ts'))fs.writeFileSync(p,fs.readFileSync(p,'utf8').replace(/foo/g,'bar'))}}walk('.')\"";
    run_corpus(
        &fx,
        &[
            (walk.as_str(), ALLOW),
            (walk_in.as_str(), ALLOW),
            (ignore_set.as_str(), ALLOW),
            (parts.as_str(), ALLOW),
            (node, ALLOW),
            // Still denied.
            (
                "python3 -c \"import shutil\nfor p in ['.git']: shutil.copytree('x',p,dirs_exist_ok=True)\"",
                INTERP,
            ),
            (
                "python3 -c \"import os\nfor d in ['.git']: open(os.path.join(d,'hooks/pre-commit'),'w')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os\nfor d in ['.git']: os.rename('x', d)\"",
                INTERP,
            ),
            (
                "python3 -c \"for p in ['.claude/settings.json']:\n  if p == 'x': continue\n  open(p,'w')\"",
                INTERP,
            ),
        ],
    );
}

/// R4-H6-2: a path joined only to be tested is not written; a join is
/// judged when the written path is not known.
#[test]
fn r4_h6_2_joins_unrelated_to_a_sure_write_are_not_judged() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"import os\nrepos=[d for d in os.listdir('..') if os.path.isdir(os.path.join('..',d,'.git'))]\nopen('repos.txt','w').write('\\n'.join(repos))\"",
                ALLOW,
            ),
            (
                "python3 -c \"from pathlib import Path\nis_repo=Path('.').joinpath('.git').exists()\nPath('build/info.txt').write_text(str(is_repo))\"",
                ALLOW,
            ),
            // Still denied: the written path is filled in at run time.
            (
                "python3 -c \"import os\nfor n in ['x']: open(os.path.join('.git', 'hooks', n), 'w')\"",
                INTERP,
            ),
        ],
    );
}

/// O11-9: a long unquoted word full of commas is lexed in linear time; a
/// non-ASCII character before a brace keeps the offsets right.
#[test]
fn o11_9_comma_heavy_word_is_linear() {
    let fx = Fixture::new();
    let command = format!("echo {}", "a,".repeat(40_000));
    let started = Instant::now();
    assert_eq!(common::rule(&fx.bash(&command)), ALLOW);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "took {took:?}");
    run_corpus(
        &fx,
        &[("rm -rf é{a,/..}", DELETE), ("mkdir -p é{a,b}", ALLOW)],
    );
}

/// O11-10: thousands of braces in one word must not exhaust the stack: the
/// hook still answers 0 or 2.
#[test]
fn o11_10_deep_braces_keep_the_exit_code_contract() {
    let dir = tempfile::tempdir().unwrap();
    let command = format!("echo {}", "{1..1}".repeat(32_000));
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": dir.path(),
    });
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
        .env_clear()
        .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("AAF_DATA_DIR", dir.path().join("data"))
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    let code = child.wait().unwrap().code();
    assert!(matches!(code, Some(0) | Some(2)), "exited {code:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn git(dir: &Path, home: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A real repository in `work` with `work/sub`, and a `home`, for the
/// built binary run with an inherited `CDPATH`.
struct Bench {
    _dir: tempfile::TempDir,
    base: PathBuf,
}

impl Bench {
    fn new() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for sub in ["data", "home", "work/sub"] {
            fs::create_dir_all(base.join(sub)).unwrap();
        }
        git(
            &base.join("work"),
            &base.join("home"),
            &["init", "-q", "-b", "feature/x"],
        );
        Bench { _dir: dir, base }
    }

    fn home(&self) -> String {
        self.base.join("home").to_string_lossy().into_owned()
    }

    fn exit_code(&self, cdpath: &str, command: &str) -> Option<i32> {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command.replace("{home}", &self.home())},
            "cwd": self.base.join("work"),
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .env_clear()
            .env("AAF_NO_BUILTIN_DEFAULTS", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AAF_DATA_DIR", self.base.join("data"))
            .env("HOME", self.base.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("CDPATH", cdpath.replace("{home}", &self.home()))
            .current_dir(self.base.join("work"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes());
        child.wait().unwrap().code()
    }
}

/// R4-H6-3: `cd` takes the first `CDPATH` entry where the directory exists;
/// `unset CDPATH` and an empty `CDPATH=` drop the inherited value.
#[test]
fn r4_h6_3_cdpath_first_existing_entry_wins() {
    let bench = Bench::new();
    let cd = "cd sub && rm -rf target";
    let mut wrong = Vec::new();
    let mut check = |cdpath: &str, command: &str, expected: i32| {
        let got = bench.exit_code(cdpath, command);
        if got != Some(expected) {
            wrong.push(format!(
                "CDPATH={cdpath} `{command}`: {got:?}, want {expected}"
            ));
        }
    };
    check(".:{home}", cd, 0);
    check(":{home}", cd, 0);
    check("{home}", "unset CDPATH; cd sub && rm -rf target", 0);
    check("{home}", "CDPATH=; cd sub && rm -rf target", 0);
    check("{home}", "export CDPATH=; cd sub && rm -rf target", 0);
    // `{home}/sub` does not exist yet: the missing entry is still a place.
    check("{home}", cd, 2);
    fs::create_dir_all(bench.base.join("home/sub")).unwrap();
    check("{home}:.", cd, 2);
    check("{home}", cd, 2);
    check(".:{home}", "CDPATH={home} cd sub && rm -rf target", 2);
    check(
        "",
        "unset CDPATH; CDPATH={home}; cd sub && rm -rf target",
        2,
    );
    check("{home}", "cd sub && rm -rf target; unset CDPATH", 2);
    // Each value is searched on its own: `.` of one does not stop `{home}`.
    check("", "CDPATH=.; CDPATH={home}; cd sub && rm -rf target", 2);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// R4-H6-4: the external `time` takes the values of `-o` and `-f`; the
/// program after them is the command.
#[test]
fn r4_h6_4_external_time_options() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("/usr/bin/time -o t.txt rm -rf ~", DELETE),
            ("\\time -o t.txt rm -rf ~", DELETE),
            ("command time -o t.txt rm -rf ~", DELETE),
            ("env time -o t gh pr merge 1", PR_MERGE),
            ("gtime -o t.txt rm -rf ~", DELETE),
            ("/usr/bin/time --output t.txt rm -rf ~", DELETE),
            (
                "/usr/bin/time -a -o t.txt git push --force origin feature/x",
                GIT,
            ),
            ("/usr/bin/time -f '%e' ./gradlew assembleDebug", HEAVY),
            (
                "/usr/bin/time -o build/t.txt xcodebuild -scheme App build",
                HEAVY,
            ),
            // Still allowed.
            ("time cargo test", ALLOW),
            ("/usr/bin/time -l make", ALLOW),
            ("/usr/bin/time -p cargo build", ALLOW),
            ("gtime -v ls", ALLOW),
        ],
    );
}

/// R4-H6-5: more launchers run the program in their arguments, and a shell
/// may read its script from `/proc/self/fd/0` or a here-string on `/dev/fd/N`.
#[test]
fn r4_h6_5_more_launchers_and_fd_scripts() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("pixi run gh pr merge 1", PR_MERGE),
            ("hatch run rm -rf ~", DELETE),
            ("rye run gh pr merge 1", PR_MERGE),
            ("devbox run -- rm -rf ~", DELETE),
            ("devbox run gh pr merge 1", PR_MERGE),
            ("op run -- rm -rf ~", DELETE),
            ("op run --env-file .env.tpl -- gh pr merge 1", PR_MERGE),
            ("doppler run -- gh pr merge 1", PR_MERGE),
            ("doppler run --command 'gh pr merge 1'", PR_MERGE),
            ("dotenvx run -f .env.prod -- rm -rf ~", DELETE),
            ("flock /tmp/b.lock rm -rf ~", DELETE),
            ("flock -w 10 /tmp/b.lock gh pr merge 1", PR_MERGE),
            ("flock /tmp/b.lock -c 'gh pr merge 1'", PR_MERGE),
            ("taskset -c 0-3 rm -rf ~", DELETE),
            ("taskset 0x3 gh pr merge 1", PR_MERGE),
            ("chronic rm -rf ~", DELETE),
            ("watch -n 5 gh pr merge 1", PR_MERGE),
            ("watch 'gh pr merge 1'", PR_MERGE),
            ("asdf exec gh pr merge 1", PR_MERGE),
            ("yarn workspace web exec gh pr merge 1", PR_MERGE),
            ("yarn workspace web run rm -rf ~", DELETE),
            ("yarn workspaces foreach -A exec gh pr merge 1", PR_MERGE),
            ("yarn workspaces foreach --all -j 4 run rm -rf ~", DELETE),
            ("bash /proc/self/fd/0 <<< 'gh pr merge 1'", PR_MERGE),
            ("bash /dev/fd/3 3<<< 'gh pr merge 1'", PR_MERGE),
            ("sh /dev/fd/4 4<<< 'rm -rf ~'", DELETE),
            ("flock /tmp/b.lock ./gradlew assembleDebug", HEAVY),
            ("taskset -c 0-3 ./gradlew assembleDebug", HEAVY),
            // Still allowed.
            ("pixi run test", ALLOW),
            ("hatch run test", ALLOW),
            ("flock /tmp/b.lock make", ALLOW),
            ("flock 9", ALLOW),
            ("taskset -p 0x3 1234", ALLOW),
            ("watch -n 5 git status", ALLOW),
            ("yarn workspace web test", ALLOW),
            ("yarn workspaces foreach -A run build", ALLOW),
            ("op read op://vault/item/field", ALLOW),
            ("asdf install", ALLOW),
            ("chronic make", ALLOW),
            ("echo 'gh pr merge 1' | bash /dev/fd/3 3<script.sh", ALLOW),
        ],
    );
}
