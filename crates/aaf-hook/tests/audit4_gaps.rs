#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Bypasses an independent review still found after audit round 4
//! (lane/fix8-hook), with the neighbours that must stay allowed.

mod common;

use common::{Fixture, run_corpus, run_file_corpus};

const ALLOW: Option<&str> = None;
const HUMAN: Option<&str> = Some("human_action_guard");
const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const DELETE: Option<&str> = Some("delete_guard");

#[test]
fn env_split_string_payload_keeps_its_own_env_options() {
    run_corpus(
        &Fixture::new(),
        &[
            // BSD env applies -i/-u/-P found inside the -S string.
            ("env -S '-i aaf run'", HUMAN),
            ("env -S '-u AAF_RUN_ID aaf run'", HUMAN),
            ("env -S '-i aaf approve'", HUMAN),
            ("env -S'-i aaf run'", HUMAN),
            ("env --split-string='-i aaf run'", HUMAN),
            ("env -S '-P/tmp/evil aaf slot -- ./gradlew build'", HEAVY),
            ("env -S '-P /tmp/evil aaf slot -- ./gradlew build'", HEAVY),
            // Neighbours.
            ("env -S 'aaf gate'", ALLOW),
            ("env -S 'aaf slot -- ./gradlew build'", ALLOW),
            ("env -S '-u FOO aaf gate'", ALLOW),
        ],
    );
}

#[test]
fn an_earlier_clearing_wrapper_reaches_a_later_split_string() {
    run_corpus(
        &Fixture::new(),
        &[
            ("env -i env -S 'aaf run'", HUMAN),
            ("sudo env -S 'aaf run'", HUMAN),
            ("exec -c env -S 'aaf gate'", HUMAN),
            ("env - nice env -S 'aaf run'", HUMAN),
            // Neighbours.
            ("env -i sh -c 'true'", ALLOW),
            ("env -u PATHi sh -c 'aaf gate'", ALLOW),
            ("env FOO=bar sh -c 'aaf gate'", ALLOW),
            ("env FOO=bar env -S 'aaf gate'", ALLOW),
            ("env -u PATHi sh -c 'aaf run'", HUMAN),
        ],
    );
}

#[test]
fn the_script_of_sh_c_is_the_first_word_that_is_not_an_option() {
    run_corpus(
        &Fixture::new(),
        &[
            ("sh -c -- 'aaf approve'", HUMAN),
            ("bash -c -x 'aaf approve'", HUMAN),
            ("bash -c -e -u 'aaf approve'", HUMAN),
            ("bash -c -o pipefail 'aaf approve'", HUMAN),
            ("bash -o pipefail -c 'aaf approve'", HUMAN),
            ("zsh -c +o nomatch 'aaf approve'", HUMAN),
            ("bash -c - 'aaf approve'", HUMAN),
            ("env -i sh -c -- 'aaf run'", HUMAN),
            ("sh -c -- 'git push --force'", GIT),
            ("bash -c -x './gradlew build'", HEAVY),
            // Neighbours.
            ("sh -c -- 'echo hi'", ALLOW),
            ("bash -c -x 'aaf gate' arg0", ALLOW),
            ("bash -c 'echo $0' aaf", ALLOW),
            ("env -i sh -c 'true'", ALLOW),
        ],
    );
}

#[test]
fn scripts_fed_on_stdin_are_split_under_the_wrapper_that_runs_them() {
    run_corpus(
        &Fixture::new(),
        &[
            ("env -i sh <<'EOF'\naaf run\nEOF", HUMAN),
            ("env -i bash -s <<EOF\naaf run\nEOF", HUMAN),
            ("sudo sh <<'EOF'\necho hi\naaf gate\nEOF", HUMAN),
            ("env -i bash -s <<< 'aaf run'", HUMAN),
            ("bash <<< 'aaf approve'", HUMAN),
            ("sh -s -- x <<< 'aaf approve'", HUMAN),
            ("bash - <<< 'git push --force'", GIT),
            ("echo 'aaf run' | env -i sh", HUMAN),
            ("echo aaf approve | bash", HUMAN),
            ("echo -n 'aaf approve' | sh -s", HUMAN),
            ("printf 'aaf approve\\n' | sh", HUMAN),
            ("printf 'echo hi\\naaf approve\\n' | bash", HUMAN),
            ("echo './gradlew build' | bash", HEAVY),
            // Neighbours: data on stdin, or a shell that runs a file.
            ("cat <<'EOF'\nhello\nEOF", ALLOW),
            ("env -i cat <<'EOF'\naaf run\nEOF", ALLOW),
            ("cat <<< 'aaf approve'", ALLOW),
            ("echo 'aaf approve' | cat", ALLOW),
            ("echo 'aaf run' | sh build.sh", ALLOW),
            ("bash build.sh <<< 'aaf approve'", ALLOW),
            ("echo 'aaf gate' | sh", ALLOW),
            ("env -i sh <<'EOF'\ntrue\nEOF", ALLOW),
            ("bash <<< 'echo hi'", ALLOW),
        ],
    );
}

#[test]
fn git_config_option_values_are_not_read_as_flags_or_verbs() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git config --comment --list core.editor /tmp/e", GIT),
            ("git config --comment -l core.sshCommand /tmp/e", GIT),
            ("git config --comment --unset core.askPass /tmp/e", GIT),
            ("git config --comment get core.pager /tmp/e", GIT),
            ("git config set --comment --get core.gitProxy /tmp/e", GIT),
            ("git config --type --get core.editor /tmp/e", GIT),
            ("git config --default --list core.editor /tmp/e", GIT),
            ("git config -f --list core.editor /tmp/e", GIT),
            ("git config --file --get core.editor /tmp/e", GIT),
            ("git config --blob --get core.editor /tmp/e", GIT),
            ("git config set --value --get core.editor /tmp/e", GIT),
            ("git config --comm --get core.editor /tmp/e", GIT),
            // The same for core.hooksPath.
            ("git config --comment get core.hooksPath /tmp/h", GIT),
            ("git config --type get core.hooksPath /tmp/h", GIT),
            // And for a stored alias: the file is not the alias name.
            (
                "git config --file alias.x/../y alias.f '!git push --force'",
                GIT,
            ),
            // Neighbours: real reads, glued values.
            ("git config --get-regexp x", ALLOW),
            ("git config --type bool --get core.bare", ALLOW),
            ("git config --type=bool --get core.editor", ALLOW),
            ("git config --comment=x --list", ALLOW),
            ("git config -f other.cfg --get core.editor", ALLOW),
            ("git config --default vi --get core.editor", ALLOW),
            ("git config --file x get core.hooksPath", ALLOW),
            ("git config --comment note user.name me", ALLOW),
        ],
    );
}

#[test]
fn branch_names_apfs_folds_onto_a_protected_one_are_protected() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("git branch -D maﬆer", GIT),
            ("git branch -D MAſTER", GIT),
            ("git branch -D releaſe/1.0", GIT),
            ("git update-ref -d refs/heads/maﬆer", GIT),
            ("git branch -m maﬆer x", GIT),
            ("git checkout -B maſter", GIT),
            ("git push --force-with-lease origin maſter", GIT),
            ("echo x > .git/refs/heads/maﬆer", GIT),
            // Neighbours.
            ("git branch -D feature/x", ALLOW),
            ("git branch -D feature/Main", ALLOW),
        ],
    );
    run_file_corpus(
        &f,
        &[
            ("Write", ".git/refs/heads/maﬆer", GIT),
            ("Write", ".git/refs/heads/MAſTER", GIT),
            ("Write", ".git/refs/heads/releaſe/1.0", GIT),
            ("Write", ".git/refs/heads/feature/x", ALLOW),
        ],
    );
}

#[test]
fn guarded_file_names_apfs_folds_onto_are_guarded() {
    let f = Fixture::new();
    run_corpus(
        &f,
        &[
            ("echo x > .git/hoo\u{212A}s/pre-commit", GIT),
            ("echo x > .git/conﬁg", GIT),
            ("echo x > .git/refſ/heads/master", GIT),
            ("echo x > .claude/ſettings.json", DELETE),
            ("rm .claude/ſettings.local.json", DELETE),
            ("rm .autofactory/conﬁg.toml", DELETE),
            // Neighbours.
            ("echo x > notes/ſettings.json", ALLOW),
            ("cat .git/conﬁg", ALLOW),
        ],
    );
    run_file_corpus(
        &f,
        &[
            ("Write", ".git/hoo\u{212A}s/pre-push", GIT),
            ("Write", ".git/conﬁg", GIT),
            ("Edit", ".claude/ſettings.json", DELETE),
            ("Write", ".claude/notes.md", ALLOW),
        ],
    );
}
