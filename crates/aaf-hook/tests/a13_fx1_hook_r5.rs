#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-5 hook findings: the interpreter
//! write fallback redesign (R5-H6-1 false positives, R5-H6-2 bypasses, the
//! Agent-10 `== '.git'` select regression), O11-11/O11-12 (cost of the
//! assignment index and of the fallback), R5-H6-3 (`CDPATH` clearing
//! scope), R5-H6-4 (brace expansion work budget) and R5-H6-5 (more
//! launchers, the external `time -o <file>`).

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
const INTERP: Option<&str> = Some("interp_guard");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join(".git/info")).unwrap();
    fs::create_dir_all(fx.root.join("src")).unwrap();
    fx
}

fn heredoc(interp: &str, body: &str) -> String {
    format!("{interp} - <<'EOF'\n{body}EOF")
}

fn py(body: &str) -> String {
    heredoc("python3", body)
}

/// A loop that selects the git directory (`d == '.git'`) and writes below
/// it with a run-time path.
fn select(cond: &str, write: &str) -> String {
    py(&format!(
        "import os\nfor d in os.listdir('.'):\n    if {cond}:\n        {write}\n"
    ))
}

/// R5 item 1: a write below a directory picked at run time is judged
/// below every protected directory the script names; a name only looked
/// at is not written.
#[test]
fn r5_unsure_write_selecting_protected_dir_is_denied() {
    let fx = fixture();
    let rows = [
        select(
            "d == '.git'",
            "open(os.path.join(d, 'config'), 'a').write('[alias]')",
        ),
        select(
            "d == '.git'",
            "open(os.path.join(d, 'hooks', 'pre-commit'), 'w').write('x')",
        ),
        select(
            "d == '.git'",
            "open(os.path.join(d, 'info', 'attributes'), 'w').write('* x')",
        ),
        select("d == '.git'", "open(d + '/config', 'a').write('[alias]')"),
        select(
            "d == '.claude'",
            "open(os.path.join(d, 'settings.json'), 'w').write('{}')",
        ),
        py(
            "import os\nfor r, ds, fs in os.walk('.'):\n    if '.git' in r:\n        open(os.path.join(r, 'HEAD'), 'w').write('ref: refs/heads/main')\n",
        ),
        "node -e \"const fs=require('fs');for(const d of fs.readdirSync('.')){if(d==='.git')fs.appendFileSync(d+'/config','[alias]')}\"".to_string(),
        "python3 -c \"for f in '.git/hooks/pre-commit'.split(): open(f,'w').write('x')\"".to_string(),
        "python3 -c \"for f in '.git/config'.splitlines(): open(f,'a')\"".to_string(),
        "ruby -e \"for f in '.git/hooks/pre-commit'.split do File.write(f,'x') end\"".to_string(),
        "python3 -c \"for d in ['.git']: open(d + '/hooks/pre-commit','w')\"".to_string(),
        "node -e \"['.git'].forEach(d => require('fs').writeFileSync(d + '/config','x'))\"".to_string(),
        py("def w(d):\n    open(d+'/hooks/pre-commit','w')\nw('.git')\n"),
        py("from pathlib import Path\nfor d in Path('.').iterdir():\n    if d.name == '.git':\n        (d / 'config').write_text('[alias]')\n"),
        py("import os\nfor d in os.listdir('.'):\n    if d == '.git':\n        open(f'{d}/config', 'a').write('[alias]')\n"),
    ];
    let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (r.as_str(), INTERP)).collect();
    run_corpus(&fx, &rows);
}

/// R5-H6-1: codemods that skip the git directory, look at it, or pass it
/// to `ignore_patterns` write elsewhere.
#[test]
fn r5_h6_1_codemods_naming_protected_dirs_are_allowed() {
    let fx = fixture();
    let rows = [
        py(
            "from pathlib import Path\nfor p in Path('.').rglob('*.py'):\n    if any(part in ('.git', 'node_modules') for part in p.parts):\n        continue\n    p.write_text(p.read_text().replace('hooks', 'handlers'))\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    if '.git' in dirs:\n        dirs.remove('.git')\n    for f in files:\n        p = os.path.join(root, f)\n        s = open(p).read()\n        open(p, 'w').write(s.replace('config', 'settings'))\n",
        ),
        py(
            "import os\nSKIP = {'.git', 'node_modules'}\nos.makedirs('src/hooks', exist_ok=True)\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in SKIP]\n    for f in files:\n        if f.endswith('.ts'):\n            p = os.path.join(root, f)\n            open(p, 'a').write('\\n')\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d not in ('.git', 'node_modules')]\n    for f in files:\n        p = os.path.join(root, f)\n        t = open(p).read()\n        open(p, 'w').write(t.replace('refs', 'references'))\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('..'):\n    if os.path.exists(os.path.join(root, '.git')):\n        continue\n    for f in files:\n        p = os.path.join(root, f)\n        open(p, 'a').write('\\n')\n",
        ),
        py(
            "import os, shutil\nfor name in ['a', 'b']:\n    shutil.copytree('template', os.path.join('out', name), ignore=shutil.ignore_patterns('.git', '__pycache__'))\n",
        ),
        // A literal before the run-time part is not below it.
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d != '.git']\n    for f in files:\n        open(os.path.join('src/hooks', f), 'w').write('x')\n",
        ),
        // A suffix glued to a run-time name is not a path below it.
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [d for d in dirs if d != '.git']\n    for f in files:\n        open(os.path.join(root, f + '.config'), 'w').write('x')\n",
        ),
        // A file of its own in each directory a walk that skips `.git` sees.
        py(
            "import os\nfor root, dirs, files in os.walk('src'):\n    dirs[:] = [d for d in dirs if d != '.git']\n    open(os.path.join(root, '__init__.py'), 'a').close()\n",
        ),
    ];
    let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (r.as_str(), ALLOW)).collect();
    run_corpus(&fx, &rows);
}

/// O11-11: one long line holding thousands of `name = value` pairs is
/// indexed in linear time.
#[test]
fn o11_11_long_line_of_assignments_is_linear() {
    let fx = fixture();
    let attrs: String = (0..8000).map(|n| format!("a{n} = '{n}' ")).collect();
    let command = py(&format!(
        "t = \"<x {attrs}>\"\nout = 'build/x.html'\nopen(out, 'w').write(t)\n"
    ));
    let started = Instant::now();
    assert_eq!(common::rule(&fx.bash(&command)), ALLOW);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

/// O11-12: a write of unknown path in a script of thousands of literals is
/// judged in about linear time, and still denied below `.git`.
#[test]
fn o11_12_unknown_path_with_many_literals_is_linear() {
    let fx = fixture();
    let lits: String = (0..8000).map(|n| format!("x{n} = 'k{n}'\n")).collect();
    let command = py(&format!(
        "import sys\n{lits}open(sys.argv[1], 'w').write('x')\n"
    ));
    let started = Instant::now();
    assert_eq!(common::rule(&fx.bash(&command)), ALLOW);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "took {took:?}");
    let denied = py(&format!(
        "import sys\n{lits}t = '.git/hooks/pre-commit'\nopen(sys.argv[1], 'w').write(t)\n"
    ));
    assert_eq!(common::rule(&fx.bash(&denied)), INTERP);
}

/// A tree with `data` (the data dir) and `work` (a repository); `CDPATH`
/// inherited by the built hook names the tree, so `cd data` from `work`
/// lands in the data dir.
struct Tree {
    _dir: tempfile::TempDir,
    base: PathBuf,
}

impl Tree {
    fn new() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for sub in ["data", "home", "work"] {
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
        Tree { _dir: dir, base }
    }

    fn exit_code(&self, command: &str) -> Option<i32> {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": self.base.join("work"),
        });
        hook(&payload, &self.base, Some(&self.base))
    }
}

/// Runs the built hook on `payload` with a home and data dir under `base`.
fn hook(payload: &serde_json::Value, base: &Path, cdpath: Option<&Path>) -> Option<i32> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aaf-hook"));
    cmd.env_clear()
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
    let mut child = cmd.spawn().unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    child.wait().unwrap().code()
}

/// R5-H6-3: only a bare assignment, a declarer or `unset` (not `-f`) in
/// this shell clears the inherited `CDPATH` for later commands.
#[test]
fn r5_h6_3_cdpath_clearing_respects_scope() {
    let tree = Tree::new();
    let mut wrong = Vec::new();
    for (command, expected) in [
        ("cd data && rm -rf *", 2),
        ("CDPATH= ls; cd data && rm -rf *", 2),
        ("(unset CDPATH); cd data && rm -rf *", 2),
        ("f() { unset CDPATH; }; cd data && rm -rf *", 2),
        ("unset -f CDPATH; cd data && rm -rf *", 2),
        ("echo $(unset CDPATH); cd data && rm -rf *", 2),
        // Still cleared.
        ("unset CDPATH; cd data && rm -rf *", 0),
        ("CDPATH=; cd data && rm -rf *", 0),
        ("export CDPATH=; cd data && rm -rf *", 0),
        ("unset -v CDPATH; cd data && rm -rf *", 0),
    ] {
        let got = tree.exit_code(command);
        if got != Some(expected) {
            wrong.push(format!("`{command}`: {got:?}, want {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// R5-H6-4: a word crafted to make brace expansion copy a lot is given up
/// on (judged opaque) before the hook's own timeout.
#[test]
fn r5_h6_4_brace_expansion_has_a_work_budget() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let command = format!(
        "echo {}{}{}{}",
        "{".repeat(120),
        "{a,b}".repeat(6),
        "{1..1}".repeat(120),
        "x".repeat(30_000)
    );
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": base,
    });
    let started = Instant::now();
    let code = hook(&payload, &base, None);
    let took = started.elapsed();
    assert!(matches!(code, Some(0) | Some(2)), "exited {code:?}");
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

/// R5-H6-5: `pixi exec`, `hatch env run`, `hatch run env:cmd`, `op plugin
/// run --` run the program after them; `time -o <file>` writes `<file>`.
#[test]
fn r5_h6_5_more_launchers_and_time_output() {
    let fx = fixture();
    let redirect = common::rule(&fx.bash("true > .claude/settings.json"));
    assert!(redirect.is_some());
    run_corpus(
        &fx,
        &[
            ("pixi exec rm -rf ~", DELETE),
            ("pixi exec --spec gh gh pr merge 1", PR_MERGE),
            ("hatch env run -- rm -rf ~", DELETE),
            ("hatch env run -e test -- gh pr merge 1", PR_MERGE),
            ("hatch run test:rm -rf ~", DELETE),
            ("op plugin run -- gh pr merge 1", PR_MERGE),
            ("/usr/bin/time -o .claude/settings.json true", redirect),
            ("gtime -o .claude/settings.json true", redirect),
            (
                "/usr/bin/time --output .claude/settings.json true",
                redirect,
            ),
            (
                "/usr/bin/time --output=.claude/settings.json true",
                redirect,
            ),
            ("/usr/bin/time -ao .claude/settings.json true", redirect),
            // Still allowed.
            ("pixi exec python -V", ALLOW),
            ("hatch env run -- pytest", ALLOW),
            ("hatch run test:pytest", ALLOW),
            ("op plugin run -- gh pr list", ALLOW),
            ("op plugin list", ALLOW),
            ("/usr/bin/time -o build/t.txt true", ALLOW),
        ],
    );
}
