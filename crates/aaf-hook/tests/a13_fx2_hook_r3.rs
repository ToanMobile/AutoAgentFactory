#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-3 hook guard findings: R3-H6-1/E3-F1
//! (interpreter writes through unresolved paths, `chdir` and path joins),
//! R3-H6-2 (adb `run-as`), R3-H6-5 (BSD `find -f` clusters), E3-F3
//! (`rimraf`/`trash`) and R3-H6-11 (`perl -e` writing `$ARGV`).

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const ADB: Option<&str> = Some("adb_guard");
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const INTERP: Option<&str> = Some("interp_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".claude/settings.json", "{}\n");
    fs::create_dir_all(fx.root.join(".git/hooks")).unwrap();
    fx
}

/// R3-H6-1: a write whose path the parser cannot resolve to literals falls
/// back to judging every literal of the script as written.
#[test]
fn r3_h6_1_unresolved_write_path_falls_back_to_every_literal() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"m='w'; open('.git/hooks/pre-commit', m).write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"for p in ['.git/hooks/pre-commit']: open(p,'w').write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"ps=['.git/hooks/pre-commit']; open(ps[0],'w').write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"d='.git/hooks'; open(f'{d}/pre-commit','w').write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"def w(p):\n  open(p,'w').write('x')\nw('.git/hooks/pre-commit')\"",
                INTERP,
            ),
            (
                "python3 -c \"from pathlib import Path; [Path(p).write_text('x') for p in ['.git/hooks/pre-commit']]\"",
                INTERP,
            ),
            (
                "node -e \"const fs=require('fs'); ['.git/hooks/pre-commit'].forEach(p=>fs.writeFileSync(p,'x'))\"",
                INTERP,
            ),
            (
                "node -e 'const fs=require(\"fs\"); const d=\".git/hooks\"; fs.writeFileSync(`${d}/pre-commit`,\"x\")'",
                INTERP,
            ),
            (
                "python3 -c \"from pathlib import Path; [p.write_text('x') for p in [Path('.claude/settings.json')]]\"",
                INTERP,
            ),
            (
                "python3 -c \"import sys; open(sys.argv[1],'w').write(open('.claude/settings.json').read())\"",
                INTERP,
            ),
            (
                "python3 -c \"def where(): return '.git/hooks/pre-commit'\nopen(where(),'w').write('x')\"",
                INTERP,
            ),
            // Reading protected files and writing elsewhere stays allowed.
            (
                "python3 -c \"t=open('.git/config').read(); open('docs/remote.txt','w').write(t)\"",
                ALLOW,
            ),
            (
                "python3 -c \"import json; d=json.load(open('app/config.json')); open('build/firebase_ids.txt','w').write(d['project_info']['project_id'])\"",
                ALLOW,
            ),
            (
                "python3 -c \"t=open('.git/config', 'r', 1).read(); print(t)\"",
                ALLOW,
            ),
            (
                "python3 -c \"for p in ['a.txt', 'b.txt']: open(p,'w').write('x')\"",
                ALLOW,
            ),
            (
                "python3 -c \"name='out'; open(f'build/{name}.txt','w').write('x')\"",
                ALLOW,
            ),
        ],
    );
}

/// E3-F1: a script that writes and changes directory, or joins paths, has
/// the directories it names judged as written.
#[test]
fn e3_f1_chdir_and_joins_are_path_context() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"import os; os.chdir('.git/hooks'); open('pre-commit','w').write('x')\"",
                INTERP,
            ),
            (
                "node -e \"const fs=require('fs'); process.chdir('.git/hooks'); fs.writeFileSync('pre-commit','x')\"",
                INTERP,
            ),
            (
                "ruby -e \"Dir.chdir('.git/hooks'); File.write('pre-commit', 'x')\"",
                INTERP,
            ),
            (
                "perl -e 'chdir \".git/hooks\"; open(F, \">pre-commit\"); print F 1'",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.chdir('.git'); open('hooks/pre-commit','w').write('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; os.chdir('.claude'); open('settings.json','w').write('{}')\"",
                INTERP,
            ),
            (
                "python3 -c \"from pathlib import Path; Path('.git/hooks').joinpath('pre-commit').write_text('x')\"",
                INTERP,
            ),
            (
                "python3 -c \"import os; d='.git/hooks'; open(os.path.join(d,'pre-commit'),'w').write('x')\"",
                INTERP,
            ),
            (
                "node -e \"const fs=require('fs'), path=require('path'); fs.writeFileSync(path.join('.git', 'hooks', 'pre-push'), 'x')\"",
                INTERP,
            ),
            // No write: naming the directory is fine.
            (
                "python3 -c \"import os; print(os.path.join('.git', 'config'))\"",
                ALLOW,
            ),
            (
                "python3 -c \"import os; os.chdir('.git'); print(open('HEAD').read())\"",
                ALLOW,
            ),
            (
                "python3 -c \"import os; os.chdir('build'); open('out.txt','w').write('x')\"",
                ALLOW,
            ),
            (
                "python3 -c \"import os; open(os.path.join('build', 'out.txt'),'w').write('x')\"",
                ALLOW,
            ),
        ],
    );
}

/// O11-8: a method called on a variable is still a write to what the
/// variable holds (the assignment lookup is now memoized).
#[test]
fn o11_8_variable_receiver_write_still_detected() {
    let fx = fixture();
    let mut script = String::from("from pathlib import Path\nclass A:\n  def x(self, n): pass\n");
    script.push_str("  def run(self):\n");
    for n in 0..1000 {
        script.push_str(&format!("    self.x({n})\n"));
    }
    script.push_str("p = Path('.git/hooks/pre-commit')\np.write_text('x')\n");
    let command = format!("python3 - <<'EOF'\n{script}EOF");
    run_corpus(&fx, &[(command.as_str(), INTERP)]);
}

/// R3-H6-2: the `run-as` exemption holds only for `run-as <pkg> rm …`
/// itself, never for an `rm` inside a substitution.
#[test]
fn r3_h6_2_run_as_exemption_needs_run_as_right_before_rm() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("adb shell 'run-as com.app ls $(rm -rf data)'", ADB),
            ("adb shell 'run-as com.app ls `rm -rf data`'", ADB),
            ("adb shell 'run-as com.app `rm -rf data`'", ADB),
            ("adb shell 'echo run-as $(rm -rf data)'", ADB),
            ("adb shell 'run-as com.app $(rm -rf data)'", ADB),
            ("adb shell run-as com.example.app rm -rf cache", ALLOW),
            ("adb shell 'run-as com.example.app rm -rf files/*'", ALLOW),
            (
                "adb -s emulator-5554 shell run-as com.example.app rm -r shared_prefs",
                ALLOW,
            ),
        ],
    );
}

/// R3-H6-5: BSD `find` reads `-f` with its value glued or after a cluster
/// of flag letters; that value is a search root.
#[test]
fn r3_h6_5_find_dash_f_clusters_name_roots() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("find -f/Users -name \"*.swift\" -delete", DELETE),
            ("find -xf / -name x -delete", DELETE),
            ("find -sf/ -name x -delete", DELETE),
            ("find -xf .. -name \"*.o\" -print0 | xargs -0 rm", DELETE),
            ("find -f .. -name x -delete", DELETE),
            ("find -f build -name \"*.o\" -delete", ALLOW),
            ("find -xf build -name \"*.o\" -delete", ALLOW),
            ("find -fstype nfs -name \"*.o\" -delete", ALLOW),
        ],
    );
}

/// E3-F3: `rimraf` and `trash` delete recursively, like `rm -rf`.
#[test]
fn e3_f3_rimraf_and_trash_are_recursive_deleters() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("rimraf ../init1", DELETE),
            ("trash ../init1", DELETE),
            ("rimraf --glob ../init1/*", DELETE),
            ("del-cli ../init1", DELETE),
            ("rimraf .claude", DELETE),
            ("trash .env", DELETE),
            ("rimraf dist", ALLOW),
            ("rimraf ./build", ALLOW),
            ("trash build/tmp.log", ALLOW),
        ],
    );
}

/// R3-H6-11: `perl -e` reads its files only while its script opens none
/// for writing.
#[test]
fn r3_h6_11_perl_e_writing_argv_is_not_a_reader() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            (
                "perl -e 'open(my $f, \">\", $ARGV[0]); print $f 1' .git/hooks/pre-commit",
                GIT,
            ),
            (
                "perl -e 'sysopen(my $f, $ARGV[0], 1); print $f 1' .git/hooks/pre-commit",
                GIT,
            ),
            (
                "perl -e 'open F, \">>$ARGV[0]\"; print F 1' .git/config",
                GIT,
            ),
            ("perl -ne 'print' .git/HEAD", ALLOW),
            ("perl -ne 'print' .git/hooks/pre-commit", ALLOW),
            (
                "perl -e 'open(my $f, \"<\", $ARGV[0]); print <$f>' .git/hooks/pre-commit",
                ALLOW,
            ),
        ],
    );
}
