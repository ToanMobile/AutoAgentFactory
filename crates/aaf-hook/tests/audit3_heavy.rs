#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-3 audit, the queue: an agent must not hand itself a slot
//! (`AAF_HEAVY_SLOT`), a forged `aaf` is not the queue, and Gradle's
//! reporting tasks are builds.

mod common;

use common::{Fixture, run_corpus};

const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const ALLOW: Option<&str> = None;

#[test]
fn setting_the_slot_variable_is_refused_whatever_the_value() {
    run_corpus(
        &Fixture::new(),
        &[
            (
                "AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew assembleDebug",
                HEAVY,
            ),
            (
                "AAF_HEAVY_SLOT=0:0123456789abcdef aaf slot -- ./gradlew assembleDebug",
                HEAVY,
            ),
            ("AAF_HEAVY_SLOT= aaf slot -- ./gradlew b", HEAVY),
            ("AAF_HEAVY_SLOT+=0 aaf slot -- ./gradlew b", HEAVY),
            ("AAF_HEAVY_SLOT=\"$TOKEN\" aaf slot -- ./gradlew b", HEAVY),
            // Not only around a build: the variable is never the agent's.
            ("AAF_HEAVY_SLOT=0 aaf slot -- cargo test", HEAVY),
            ("AAF_HEAVY_SLOT=0 aaf gate", HEAVY),
            ("AAF_HEAVY_SLOT=0", HEAVY),
            ("export AAF_HEAVY_SLOT=0", HEAVY),
            ("export AAF_HEAVY_SLOT=0; aaf slot -- ./gradlew b", HEAVY),
            ("aaf slot -- ./gradlew b; export AAF_HEAVY_SLOT=0", HEAVY),
            ("export AAF_HEAVY_SLOT", HEAVY),
            ("export -n AAF_HEAVY_SLOT", HEAVY),
            ("declare -x AAF_HEAVY_SLOT=0", HEAVY),
            ("declare -x AAF_HEAVY_SLOT", HEAVY),
            ("typeset -x AAF_HEAVY_SLOT=0", HEAVY),
            ("readonly AAF_HEAVY_SLOT=0", HEAVY),
            ("local AAF_HEAVY_SLOT=0", HEAVY),
            ("env AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b", HEAVY),
            ("env -i AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b", HEAVY),
            ("env AAF_HEAVY_SLOT=0", HEAVY),
            ("env -S 'AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b'", HEAVY),
            ("env '-SAAF_HEAVY_SLOT=0 aaf slot -- true'", HEAVY),
            (
                "env --split-string='AAF_HEAVY_SLOT=0 aaf slot -- true'",
                HEAVY,
            ),
            ("sudo AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b", HEAVY),
            ("nice env AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b", HEAVY),
            ("sh -c 'AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b'", HEAVY),
            ("bash -lc \"export AAF_HEAVY_SLOT=0; aaf gate\"", HEAVY),
            ("echo $(AAF_HEAVY_SLOT=0 aaf slot -- ./gradlew b)", HEAVY),
            ("echo `AAF_HEAVY_SLOT=0 aaf slot -- true`", HEAVY),
            ("eval 'AAF_HEAVY_SLOT=0 aaf slot -- true'", HEAVY),
            (
                "aaf slot -- sh -c 'export AAF_HEAVY_SLOT=1; ./gradlew b'",
                HEAVY,
            ),
            ("aaf slot -- env AAF_HEAVY_SLOT=1 ./gradlew b", HEAVY),
            (
                "echo 0 | xargs -I{} env AAF_HEAVY_SLOT={} aaf slot -- true",
                HEAVY,
            ),
            // Other ways a shell or a tool gives the name a value.
            ("read AAF_HEAVY_SLOT < slot.txt", HEAVY),
            ("printf -v AAF_HEAVY_SLOT 0", HEAVY),
            ("for AAF_HEAVY_SLOT in 0; do aaf gate; done", HEAVY),
            ("make AAF_HEAVY_SLOT=0 build", HEAVY),
        ],
    );
}

/// The name is not on the line, so the guard cannot see which variable is
/// set. Next to a queued command that is refused; alone it is ordinary shell.
#[test]
fn a_variable_named_at_run_time_is_refused_next_to_a_slot() {
    run_corpus(
        &Fixture::new(),
        &[
            ("export \"$V\"=0; aaf slot -- ./gradlew b", HEAVY),
            ("export $V && aaf slot -- ./gradlew b", HEAVY),
            ("declare -x \"$N=0\"; aaf slot -- gradle build", HEAVY),
            ("export $(cat vars); aaf slot -- ./gradlew b", HEAVY),
            ("aaf slot -- sh -c 'export $V; ./gradlew b'", HEAVY),
            // A command word built at run time in front of a heavy tool.
            ("env \"$V\" aaf slot -- ./gradlew b", HEAVY),
            ("$AAF slot -- ./gradlew b", HEAVY),
            ("\"$BIN\"/aaf slot -- ./gradlew b", HEAVY),
            ("$RUNNER ./gradlew build", HEAVY),
            ("$RUNNER xcodebuild -scheme App build", HEAVY),
            // allowed
            ("export \"$V\"=0", ALLOW),
            ("export $(cat .env.example | xargs); npm test", ALLOW),
            ("export $(cat .env | xargs); npm test", Some("secret_guard")),
            (
                "export JAVA_HOME=$(mise where java); aaf slot -- ./gradlew b",
                ALLOW,
            ),
            // Round 4: a search path set before `aaf` is found may lead to
            // another program of that name; set for the payload it is fine.
            (
                "export PATH=\"$HOME/bin:$PATH\" && aaf slot -- ./gradlew build",
                HEAVY,
            ),
            (
                "aaf slot -- env PATH=\"$HOME/bin:$PATH\" ./gradlew build",
                ALLOW,
            ),
            ("declare -x FOO=1; aaf slot -- ./gradlew b", ALLOW),
            ("$EDITOR notes.txt", ALLOW),
            ("$AAF gate", ALLOW),
        ],
    );
}

#[test]
fn reading_or_naming_the_slot_variable_stays_allowed() {
    run_corpus(
        &Fixture::new(),
        &[
            ("echo $AAF_HEAVY_SLOT", ALLOW),
            ("echo \"slot=$AAF_HEAVY_SLOT\"", ALLOW),
            ("printenv AAF_HEAVY_SLOT", ALLOW),
            ("env | grep AAF_HEAVY_SLOT", ALLOW),
            ("grep -rn AAF_HEAVY_SLOT crates", ALLOW),
            ("test -n \"$AAF_HEAVY_SLOT\"", ALLOW),
            ("unset AAF_HEAVY_SLOT", ALLOW),
            ("env -u AAF_HEAVY_SLOT aaf slot -- ./gradlew b", ALLOW),
            ("git commit -m 'slot: AAF_HEAVY_SLOT=<slot>:<token>'", ALLOW),
            // Another variable.
            ("AAF_HEAVY_SLOTS=2 aaf gate", ALLOW),
            ("export MY_AAF_HEAVY_SLOT=0", ALLOW),
            ("FOO=1 aaf slot -- ./gradlew b", ALLOW),
        ],
    );
}

#[test]
fn only_the_bare_name_aaf_is_the_queue() {
    let fixture = Fixture::new();
    fixture.write(".env", "x");
    run_corpus(
        &fixture,
        &[
            ("/tmp/evil/aaf slot -- ./gradlew assembleDebug", HEAVY),
            ("./aaf slot -- ./gradlew assembleDebug", HEAVY),
            ("../aaf slot -- gradle build", HEAVY),
            ("~/bin/aaf slot -- xcodebuild -scheme App build", HEAVY),
            ("/usr/local/bin/aaf slot -- ./gradlew build", HEAVY),
            ("./target/debug/aaf slot -- Unity -batchmode -quit", HEAVY),
            ("env /tmp/evil/aaf slot -- ./gradlew b", HEAVY),
            ("sh -c './aaf slot -- ./gradlew b'", HEAVY),
            ("/tmp/evil/aaf slot -- sh -c './gradlew b'", HEAVY),
            (
                "/tmp/evil/aaf --json slot --timeout 5 -- ./gradlew b",
                HEAVY,
            ),
            // The other guards still see what a forged wrapper runs.
            (
                "/tmp/evil/aaf slot -- aaf approve",
                Some("human_action_guard"),
            ),
            (
                "./aaf slot -- git push --force origin main",
                Some("git_guard"),
            ),
            ("./aaf slot -- rm .env", Some("delete_guard")),
            // allowed
            ("aaf slot -- ./gradlew assembleDebug", ALLOW),
            ("command aaf slot -- ./gradlew assembleDebug", ALLOW),
            ("env FOO=1 aaf slot -- ./gradlew assembleDebug", ALLOW),
            ("/usr/local/bin/aaf slot -- cargo test", ALLOW),
            ("./target/debug/aaf doctor", ALLOW),
            ("./target/debug/aaf gate", ALLOW),
        ],
    );
}

#[test]
fn gradle_reporting_tasks_are_builds() {
    run_corpus(
        &Fixture::new(),
        &[
            ("./gradlew tasks", HEAVY),
            ("./gradlew projects", HEAVY),
            ("./gradlew properties", HEAVY),
            ("./gradlew help", HEAVY),
            ("gradle tasks", HEAVY),
            ("sh gradlew projects", HEAVY),
            ("./gradlew help --task build", HEAVY),
            ("./gradlew --version tasks", HEAVY),
            // allowed
            ("./gradlew --version", ALLOW),
            ("./gradlew -v", ALLOW),
            ("./gradlew --status", ALLOW),
            ("./gradlew --stop", ALLOW),
            ("./gradlew --help", ALLOW),
            ("./gradlew -h", ALLOW),
            ("./gradlew '-?'", ALLOW),
            ("aaf slot -- ./gradlew tasks", ALLOW),
        ],
    );
}

/// A wrapper option the splitter misreads makes its value the command and
/// hides the build behind it.
#[test]
fn clustered_and_glued_wrapper_options_do_not_hide_a_heavy_build() {
    run_corpus(
        &Fixture::new(),
        &[
            ("env -iu X ./gradlew build", HEAVY),
            ("sudo -Eu owner ./gradlew build", HEAVY),
            ("exec -la name ./gradlew build", HEAVY),
            ("env -iS './gradlew build'", HEAVY),
            ("env '-S./gradlew build'", HEAVY),
            ("env --split-string='./gradlew build'", HEAVY),
            ("sudo X=1 ./gradlew build", HEAVY),
            ("env -uX ./gradlew build", HEAVY),
            ("nice -n15 ./gradlew build", HEAVY),
            ("timeout -k 5 600 ./gradlew build", HEAVY),
            // allowed
            ("env -iu X ./gradlew --version", ALLOW),
            ("sudo -Eu owner ls gradlew", ALLOW),
            ("env -iS 'ls gradlew'", ALLOW),
            ("sudo X=1 ls", ALLOW),
        ],
    );
}
