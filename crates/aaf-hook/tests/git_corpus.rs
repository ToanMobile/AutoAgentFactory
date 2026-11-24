#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, run_corpus};

const GIT: Option<&str> = Some("git_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

#[test]
fn git_guard_bypass_corpus() {
    let rows: &[(&str, Option<&str>)] = &[
        // force push
        ("git push --force", GIT),
        ("git push -f origin feature/x", GIT),
        ("git push origin +feature/x", GIT),
        ("git push origin +refs/heads/a:refs/heads/a", GIT),
        ("git push -uf origin feature/x", GIT),
        ("git push -fu origin feature/x", GIT),
        ("git push --forc origin feature/x", GIT),
        ("git push --mirror", GIT),
        ("git push origin feature/x --force", GIT),
        // force-with-lease only to protected branches
        ("git push --force-with-lease origin main", GIT),
        ("git push --force-with-lease origin HEAD:release/1.2", GIT),
        (
            "git push --force-with-lease=main:abc origin refs/heads/master",
            GIT,
        ),
        ("git push --force-with-lease --all", GIT),
        ("git push --force-with-lease origin $BRANCH", GIT),
        // deleting protected branches
        ("git push origin --delete main", GIT),
        ("git push -d origin release/2.0", GIT),
        ("git push origin :main", GIT),
        ("git push origin :refs/heads/master", GIT),
        ("git branch -D main", GIT),
        ("git branch -d release/1.0", GIT),
        ("git branch --delete --force master", GIT),
        ("git update-ref -d refs/heads/master", GIT),
        // discarding work, skipping hooks
        ("git reset --hard HEAD~1", GIT),
        ("git reset --har", GIT),
        ("git commit --no-verify -m x", GIT),
        ("git commit -n -m x", GIT),
        ("git commit -anm x", GIT),
        ("git commit --no-verif -m x", GIT),
        ("git push --no-verify origin feature/x", GIT),
        ("git merge --no-verify feature/x", GIT),
        ("git -c core.hooksPath=/dev/null commit -m x", GIT),
        ("git -c CORE.HOOKSPATH= commit -m x", GIT),
        ("git --config-env=core.hooksPath=EMPTY commit -m x", GIT),
        ("git config core.hooksPath /dev/null", GIT),
        ("git config --global core.hooksPath /dev/null", GIT),
        ("git config --unset core.hooksPath", GIT),
        (
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/x git commit -m x",
            GIT,
        ),
        ("export GIT_CONFIG_KEY_0=core.hooksPath", GIT),
        // quoting
        ("\"git\" push --force", GIT),
        ("g'i't push --force", GIT),
        ("\\git push --force", GIT),
        ("git 'push' \"--force\"", GIT),
        ("$'\\x67it' push --force", GIT),
        ("git push --for\\ce", GIT),
        // chaining
        ("echo hi; git push --force", GIT),
        ("true && git reset --hard", GIT),
        ("false || git push -f", GIT),
        ("echo x | git push --force", GIT),
        ("echo x\ngit reset --hard", GIT),
        ("git status & git push --force", GIT),
        ("git push \\\n  --force", GIT),
        // subshells and substitutions
        ("echo $(git push --force)", GIT),
        ("echo `git reset --hard`", GIT),
        ("echo \"result: $(git push -f)\"", GIT),
        ("(git push --force)", GIT),
        ("{ git push --force; }", GIT),
        ("if true; then git push --force; fi", GIT),
        ("for b in a b; do git push -f origin $b; done", GIT),
        ("f() { git reset --hard; }; f", GIT),
        ("diff <(git push --force) /dev/null", GIT),
        // nested interpreters
        ("bash -c 'git push --force'", GIT),
        ("sh -c \"git reset --hard\"", GIT),
        ("zsh -lc 'git push -f'", GIT),
        ("bash -c \"bash -c 'git push --force'\"", GIT),
        ("eval \"git push --force\"", GIT),
        ("eval git reset --hard", GIT),
        ("env -S 'git push --force'", GIT),
        ("bash <<EOF\ngit push --force\nEOF", GIT),
        ("bash -c \"$(cat <<'EOF'\ngit push --force\nEOF\n)\"", GIT),
        ("find . -name x -exec git reset --hard \\;", GIT),
        // prefixes and absolute paths
        ("FOO=1 git push --force", GIT),
        ("command git push --force", GIT),
        ("env GIT_TRACE=1 git push --force", GIT),
        ("sudo -u root git push --force", GIT),
        ("timeout 10 git push --force", GIT),
        ("xargs git push --force", GIT),
        ("/usr/bin/git push --force", GIT),
        ("git -C /tmp/x push --force", GIT),
        ("git -C /tmp/x -c user.name=x reset --hard", GIT),
        ("git --no-pager -c color.ui=never push -f", GIT),
        // a command word that could be git
        ("$GIT push --force", GIT),
        ("$(which git) reset --hard", GIT),
        // unparsable text with git and a dangerous word
        ("git push --force 'oops", OPAQUE),
        ("echo \"unterminated; git reset --hard", OPAQUE),
        // allowed
        ("git push -n origin main", ALLOW),
        ("git push -n --force", ALLOW),
        ("git push --dry-run -f origin main", ALLOW),
        ("git push --force-with-lease origin feature/x", ALLOW),
        ("git push --force-with-lease", ALLOW),
        ("git push --force-with-lease origin HEAD", ALLOW),
        (
            "git push --force-with-lease --force-if-includes origin feature/x",
            ALLOW,
        ),
        ("git push origin feature/x", ALLOW),
        ("git push -u origin HEAD", ALLOW),
        ("git push origin main", ALLOW),
        ("git push origin --delete feature/old", ALLOW),
        ("git push origin :feature/old", ALLOW),
        ("git push -o ci.skip origin feature/x", ALLOW),
        ("git branch -D feature/old", ALLOW),
        ("git branch main", ALLOW),
        ("git reset --soft HEAD~1", ALLOW),
        ("git reset HEAD file.txt", ALLOW),
        ("git commit -m 'no --no-verify here'", ALLOW),
        ("git commit -m -n", ALLOW),
        ("git commit -am 'explain reset --hard'", ALLOW),
        ("git commit -m 'document core.hooksPath'", ALLOW),
        ("git config core.hooksPath", ALLOW),
        ("git config --get core.hooksPath", ALLOW),
        ("git config user.name x", ALLOW),
        ("git -c user.name=x commit -m y", ALLOW),
        ("git -C /tmp/x status", ALLOW),
        ("git log --oneline -n 5", ALLOW),
        ("git merge feature/x", ALLOW),
        ("echo \"git push --force\"", ALLOW),
        ("echo 'git reset --hard'", ALLOW),
        ("grep -rn 'push --force' docs", ALLOW),
        ("# git push --force", ALLOW),
        ("ls -la # git reset --hard", ALLOW),
        (
            "git commit -m \"$(cat <<'EOF'\nfix: don't run git push --force\n\nreset --hard is blocked too\nEOF\n)\"",
            ALLOW,
        ),
        ("cat <<'EOF'\ngit push --force\nEOF", ALLOW),
        ("cargo test -p aaf-hook", ALLOW),
    ];
    assert!(rows.iter().filter(|(_, e)| e.is_some()).count() >= 25);
    run_corpus(&Fixture::new(), rows);
}

#[test]
fn force_with_lease_follows_the_checked_out_branch() {
    let fixture = Fixture::new();
    fixture.checkout("main");
    run_corpus(
        &fixture,
        &[
            ("git push --force-with-lease", GIT),
            ("git push --force-with-lease origin HEAD", GIT),
            ("git push --force-with-lease origin feature/x", ALLOW),
        ],
    );
    fixture.checkout("release/3.1");
    run_corpus(&fixture, &[("git push --force-with-lease origin", GIT)]);
    // Detached HEAD: the target is unknown, so refuse.
    std::fs::write(fixture.root.join(".git/HEAD"), "0123abcd\n").unwrap();
    run_corpus(&fixture, &[("git push --force-with-lease", GIT)]);
}

#[test]
fn heredoc_bodies_are_commands_unless_proven_data() {
    run_corpus(
        &Fixture::new(),
        &[
            // An unquoted delimiter still expands $(…) inside the body.
            ("cat <<EOF\n$(git push --force)\nEOF", GIT),
            // The body is piped into a shell.
            ("cat <<'EOF' | bash\ngit push --force\nEOF", GIT),
            // The substitution is evaluated, not passed as data.
            ("eval \"$(cat <<'EOF'\ngit push --force\nEOF\n)\"", GIT),
            ("$(cat <<'EOF'\ngit push --force\nEOF\n)", GIT),
            // Unparsable body next to git and a dangerous word.
            ("sh <<EOF\ngit push --force 'x\nEOF", OPAQUE),
            (
                "gh pr create --body \"$(cat <<'EOF'\n🤖 aaf-bot: don't git push --force\nEOF\n)\"",
                ALLOW,
            ),
        ],
    );
}

#[test]
fn nesting_past_the_limit_is_judged_as_text() {
    let fixture = Fixture::new();
    let mut danger = "git push --force".to_string();
    let mut harmless = "ls".to_string();
    for _ in 0..12 {
        danger = format!("echo $({danger})");
        harmless = format!("echo $({harmless})");
    }
    run_corpus(&fixture, &[(&danger, OPAQUE), (&harmless, ALLOW)]);
}
