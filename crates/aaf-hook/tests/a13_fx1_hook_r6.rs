#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-6 hook findings: table- and
//! helper-driven writes below a protected directory (R6-H6-1), the
//! `'/.git' in root` skip idiom (R6-H6-2), git state judged only where a
//! script writes (R6-H6-3), glob `ignore` lists (R6-H6-4), more ways of
//! building a path below a run-time part (R6-H6-5), the cost of resolving
//! a variable reassigned from itself (R6-H6-6, O11-13), the joined
//! `.git/info/exclude`, and a `CDPATH` clear that may not run (R6-1).

mod common;

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Fixture, run_corpus};
use serde_json::json;

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fs::create_dir_all(fx.root.join(".git/info")).unwrap();
    fs::create_dir_all(fx.root.join("src")).unwrap();
    fx
}

fn py(body: &str) -> String {
    format!("python3 - <<'EOF'\n{body}EOF")
}

fn rows<'a>(
    rows: &'a [String],
    expected: Option<&'static str>,
) -> Vec<(&'a str, Option<&'static str>)> {
    rows.iter().map(|row| (row.as_str(), expected)).collect()
}

/// A loop that selects the git directory (`d == '.git'`) and writes below
/// it with a run-time path.
fn select(write: &str) -> String {
    py(&format!(
        "import os\nfor d in os.listdir('.'):\n    if d == '.git':\n        {write}\n"
    ))
}

/// R6-H6-1: the directory and the file of `join(d, f)` both come from a
/// table, a helper's arguments or nested loops.
#[test]
fn r6_h6_1_table_driven_writes_below_protected_dir_are_denied() {
    let fx = fixture();
    let deny = [
        py(
            "import os\nFILES = [('.git', 'hooks/pre-commit', '#!/bin/sh\\ncargo fmt --check\\n')]\nfor d, f, body in FILES:\n    p = os.path.join(d, f)\n    open(p, 'w').write(body)\n    os.chmod(p, 0o755)\n",
        ),
        py("import os\nfor d, f in [('.git', 'config')]:\n    open(os.path.join(d, f), 'a').write('[alias]')\n"),
        py(
            "import os\nfor d in ['.git']:\n    for f in ['hooks/pre-commit']:\n        open(os.path.join(d, f), 'w').write('x')\n",
        ),
        py(
            "import os\nfor d, f in {'.git': 'HEAD'}.items():\n    open(os.path.join(d, f), 'w').write('ref: refs/heads/main')\n",
        ),
        "node -e 'const fs=require(\"fs\"),path=require(\"path\");for(const [d,f] of [[\".git\",\"hooks/pre-commit\"]])fs.writeFileSync(path.join(d,f),\"x\")'".to_string(),
        py("import os\ndef w(d, f):\n    open(os.path.join(d, f), 'w').write('x')\nw('.git', 'hooks/pre-commit')\n"),
        py("import os\ndef w(d, f):\n    open(d + '/' + f, 'w').write('x')\nw('.claude', 'settings.json')\n"),
        py("import os\nfor d, f in [('.claude', 'settings.json')]:\n    open(os.path.join(d, f), 'w').write('{}')\n"),
    ];
    run_corpus(&fx, &rows(&deny, INTERP));
    // A skip set beside other names is not a table of paths.
    let allow = [
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    dirs[:] = [x for x in dirs if x not in ('.git', 'hooks')]\n    for f in files:\n        open(os.path.join(root, f), 'a').close()\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    if root in ('.git', 'config'):\n        continue\n    for f in files:\n        open(os.path.join(root, f), 'a').close()\n",
        ),
    ];
    run_corpus(&fx, &rows(&allow, ALLOW));
}

/// R6-H6-2: `'/.git' in root`, `'/.git/' in p` and `p.includes('/.git/')`
/// skip the git directory; a run-time root joined to `/.git/config` is
/// still written there.
#[test]
fn r6_h6_2_slash_git_skip_idiom_is_allowed() {
    let fx = fixture();
    let allow = [
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    if '/.git' in root: continue\n    for f in files:\n        if f.endswith('.py'):\n            p = os.path.join(root, f)\n            s = open(p).read()\n            open(p, 'w').write(s.replace('foo', 'bar'))\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('.'):\n    if '/.git/' in root + '/': continue\n    for f in files:\n        p = os.path.join(root, f)\n        open(p, 'a').close()\n",
        ),
        py(
            "import glob\nfor p in glob.glob('**/*.py', recursive=True):\n    if '/.git/' in p or p.startswith('.git/'): continue\n    s = open(p).read(); open(p, 'w').write(s.replace('a', 'b'))\n",
        ),
        "node -e \"const fs=require('fs'),path=require('path');function walk(d){for(const e of fs.readdirSync(d,{withFileTypes:true})){const p=path.join(d,e.name);if(p.includes('/.git/')||e.name==='node_modules')continue;if(e.isDirectory())walk(p);else if(p.endsWith('.js'))fs.writeFileSync(p,fs.readFileSync(p,'utf8').replace(/var /g,'let '))}}walk('.')\"".to_string(),
    ];
    run_corpus(&fx, &rows(&allow, ALLOW));
    let deny = [py(
        "import os\nfor root, dirs, files in os.walk('.'):\n    open(root + '/.git' + '/config', 'a').write('[alias]')\n",
    )];
    run_corpus(&fx, &rows(&deny, INTERP));
}

/// R6-H6-3: git's state read, or tested for, by a script that writes the
/// files it is given; a path below a file of git's (`config/app.json`), or
/// a dot-file kept in a directory of a walk that skips `.git`.
#[test]
fn r6_h6_3_git_state_only_judged_where_written() {
    let fx = fixture();
    let allow = [
        "python3 -c \"import sys\nhead = open('.git/HEAD').read().strip()\nopen(sys.argv[1], 'w').write(head)\" build/version.txt".to_string(),
        "python3 -c \"import sys, os\nout = sys.argv[1]\nref = open('.git/HEAD').read().split()[-1]\nsha = open(os.path.join('.git', ref)).read().strip()\nopen(out, 'w').write(sha)\" build/version.txt".to_string(),
        "node -e \"const fs=require('fs');const head=fs.readFileSync('.git/HEAD','utf8').trim();fs.writeFileSync(process.argv[1], JSON.stringify({head}))\" dist/version.json".to_string(),
        "python3 -c \"import sys, os\nassert os.path.exists('.git/HEAD'), 'not a git checkout'\nfor f in sys.argv[1:]:\n    open(f, 'a').write('\\n')\" src/a.py".to_string(),
        py(
            "import os\nfor d in os.listdir('.'):\n    if d in ('.git', '.claude') or not os.path.isdir(d): continue\n    open(os.path.join(d, 'config', 'app.json'), 'w').write('{}')\n",
        ),
        py(
            "import os\nfor root, dirs, files in os.walk('services'):\n    dirs[:] = [x for x in dirs if x != '.git']\n    if 'Dockerfile' in files:\n        os.makedirs(os.path.join(root, 'logs'), exist_ok=True)\n        open(os.path.join(root, 'logs', '.keep'), 'w').close()\n",
        ),
    ];
    run_corpus(&fx, &rows(&allow, ALLOW));
    let deny = [
        select("open(os.path.join(d, 'hooks', name), 'w').write('x')"),
        select("open(os.path.join(d, 'logs', 'HEAD'), 'w').write('x')"),
        select("open(os.path.join(d, 'refs', 'heads', 'main'), 'w').write('0')"),
        "python3 -c \"open('.git/HEAD','w').write('ref: refs/heads/x')\"".to_string(),
    ];
    run_corpus(&fx, &rows(&deny, INTERP));
}

/// R6-H6-4: what a glob leaves out (`ignore`, `exclude`) is not written.
#[test]
fn r6_h6_4_glob_ignore_lists_are_not_written() {
    let fx = fixture();
    let allow = [
        "node -e \"const fs=require('fs');const fg=require('fast-glob');for(const f of fg.sync(['**/*.ts'],{ignore:['**/node_modules/**','**/.git/**']}))fs.writeFileSync(f,fs.readFileSync(f,'utf8').replace(/foo/g,'bar'))\"".to_string(),
        "node -e \"const fs=require('fs');const {globSync}=require('glob');for(const f of globSync('src/**/*.ts',{ignore:'node_modules/**'}))fs.writeFileSync(f,'x')\"".to_string(),
        "node -e \"const fs=require('fs');const {globSync}=require('glob');for(const f of globSync('**/*.ts',{ignore:['dist/**']}))fs.writeFileSync(f,'x')\"".to_string(),
        py(
            "import glob\nfor f in glob.glob('src/**/*.py', recursive=True, exclude=['dist/**']):\n    open(f, 'a').close()\n",
        ),
    ];
    run_corpus(&fx, &rows(&allow, ALLOW));
    let deny = [py(
        "import glob\nfor f in glob.glob('**/*.keystore', recursive=True):\n    open(f, 'w').write('x')\n",
    )];
    let got = common::rule(&fx.bash(&deny[0]));
    assert!(got.is_some(), "keystore glob write allowed");
}

/// R6-H6-5: a known variable's value, `%s` formatting and a separator's
/// `join` of a list build a path below the run-time part too.
#[test]
fn r6_h6_5_more_builders_below_run_time_part_are_denied() {
    let fx = fixture();
    let deny = [
        py(
            "import os\nCFG = 'config'\nfor d in os.listdir('.'):\n    if d == '.git':\n        open(os.path.join(d, CFG), 'a').write('x')\n",
        ),
        py(
            "import os\nHOOK = 'hooks/pre-commit'\nfor d in os.listdir('.'):\n    if d == '.git':\n        open(os.path.join(d, HOOK), 'w').write('x')\n",
        ),
        select("c = 'config'\n        open(os.path.join(d, c), 'a').write('x')"),
        select("open('%s/hooks/pre-commit' % d, 'w').write('x')"),
        select("open('%s/config' % d, 'a').write('x')"),
        select("open('/'.join([d, 'config']), 'a').write('x')"),
        select("open(os.sep.join((d, 'config')), 'a').write('x')"),
        "node -e 'const fs=require(\"fs\");for(const d of fs.readdirSync(\".\"))if(d===\".git\")fs.appendFileSync([d,\"config\"].join(\"/\"),\"x\")'".to_string(),
    ];
    run_corpus(&fx, &rows(&deny, INTERP));
}

/// R6-H6-6 / O11-13: a variable built from itself, a counter and a path
/// built step by step are resolved in about linear time.
#[test]
fn r6_h6_6_self_reassignment_is_bounded() {
    let fx = fixture();
    let timed = |command: &str, expected: Option<&str>| {
        let started = Instant::now();
        assert_eq!(common::rule(&fx.bash(command)), expected, "{command:.80}");
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "took {took:?}: {command:.80}"
        );
    };
    let mut body = String::from("import os, sys\np = ''\n");
    for _ in 0..40 {
        body.push_str("p = p + 'a'\n");
    }
    body.push_str("open(os.path.join(sys.argv[1], p), 'w').write('x')\nx = '.git'\n");
    timed(&py(&body), ALLOW);
    // C3: a counter.
    let mut body = String::from("i = 0\n");
    for _ in 0..120 {
        body.push_str("i = i + 1\nopen(f'out/{i}.txt', 'w').write('x')\n");
    }
    timed(&py(&body), ALLOW);
    // A2: a path built below a run-time root.
    let mut body = String::from("import os, sys\np = sys.argv[1]\n");
    for n in 0..60 {
        body.push_str(&format!("p = os.path.join(p, 'a{n}')\n"));
    }
    body.push_str("open(p, 'w').write('x')\n");
    timed(&py(&body), ALLOW);
    // C: many paths of one name.
    let mut body = String::new();
    for n in 0..2000 {
        body.push_str(&format!("p = 'src/f{n}.py'\nopen(p, 'w').write('x')\n"));
    }
    timed(&py(&body), ALLOW);
}

/// The joined `.git/info/exclude` is judged as the protected git-state path it
/// is, matching direct literal writes (`SECURITY.md §9`).
#[test]
fn joined_info_exclude_is_denied() {
    let fx = fixture();
    let deny = [
        "python3 -c \"open('.git/info/exclude','a').write('build/\\n')\"".to_string(),
        "python3 -c \"import os\nopen(os.path.join('.git','info','exclude'),'a').write('build/\\n')\"".to_string(),
        "python3 -c \"from pathlib import Path\n(Path('.git')/'info'/'exclude').write_text('build/\\n')\"".to_string(),
        "python3 -c \"import os\nopen(os.path.join('.git','info','attributes'),'a').write('x')\""
            .to_string(),
        "python3 -c \"import shutil\nshutil.rmtree('.git')\"".to_string(),
    ];
    run_corpus(&fx, &rows(&deny, INTERP));
}

/// R6-1: a `CDPATH` clear that may not run in this shell (in a pipeline,
/// after `&&`/`||`, in the body of `if`/`case`/`while`, in the background)
/// leaves the inherited value in place.
#[test]
fn r6_1_conditional_cdpath_clear_keeps_inherited_value() {
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
    let run = |command: &str| {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": base.join("work"),
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_aaf-hook"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AAF_DATA_DIR", base.join("data"))
            .env("HOME", base.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("CDPATH", &base)
            .current_dir(&base)
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
    };
    let mut wrong = Vec::new();
    for (command, expected) in [
        ("CDPATH= | true; cd data && rm -rf *", 2),
        ("false && CDPATH=; cd data && rm -rf *", 2),
        ("if false; then CDPATH=; fi; cd data && rm -rf *", 2),
        ("true || CDPATH=; cd data && rm -rf *", 2),
        ("CDPATH= & cd data && rm -rf *", 2),
        ("case x in y) CDPATH=;; esac; cd data && rm -rf *", 2),
        ("while false; do CDPATH=; done; cd data && rm -rf *", 2),
        ("false && unset CDPATH; cd data && rm -rf *", 2),
        ("for x in; do unset CDPATH; done; cd data && rm -rf *", 2),
        // Still cleared.
        ("unset CDPATH; cd data && rm -rf *", 0),
        ("CDPATH=; cd data && rm -rf *", 0),
        ("CDPATH= && cd data && rm -rf *", 0),
        ("if true; then :; fi; unset CDPATH; cd data && rm -rf *", 0),
    ] {
        let got = run(command);
        if got != Some(expected) {
            wrong.push(format!("`{command}`: {got:?}, want {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
