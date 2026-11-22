#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `heavy_cmd_guard`: Gradle, xcodebuild and Unity batch runs started bare
//! are refused; the same command inside `aaf slot -- …` (or `aaf gate`)
//! shares the machine queue and passes. Light, read-only invocations and
//! lines that only mention the tools stay allowed.

mod common;

use aaf_hook::Decision;
use common::{Fixture, run_corpus};

const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const ALLOW: Option<&str> = None;

#[test]
fn a_bare_heavy_build_is_refused() {
    run_corpus(
        &Fixture::new(),
        &[
            ("./gradlew :app:lintDebug", HEAVY),
            ("gradlew build", HEAVY),
            ("gradle build", HEAVY),
            ("/work/app/gradlew assembleDebug", HEAVY),
            ("android/gradlew test", HEAVY),
            ("./Gradlew build", HEAVY),
            // No task: the project's default tasks run.
            ("./gradlew", HEAVY),
            // Every argument must be a light one.
            ("./gradlew --version build", HEAVY),
            ("./gradlew -q tasks", HEAVY),
            ("./gradlew tasks --all", HEAVY),
            ("./gradlew dependencies", HEAVY),
            ("./gradlew $TASK", HEAVY),
            ("./gradlew \"$@\"", HEAVY),
            ("$ROOT/gradlew build", HEAVY),
            ("$(pwd)/gradlew build", HEAVY),
            ("xcodebuild -scheme App build", HEAVY),
            ("xcodebuild", HEAVY),
            ("xcodebuild test -workspace A.xcworkspace -scheme A", HEAVY),
            ("/usr/bin/xcodebuild clean", HEAVY),
            ("xcodebuild $ARGS", HEAVY),
            // A light flag does not excuse a build action next to it.
            ("xcodebuild -scheme -list build", HEAVY),
            ("xcodebuild -showBuildSettings -scheme App archive", HEAVY),
            ("Unity -batchmode -runTests", HEAVY),
            ("Unity -runTests -testPlatform EditMode", HEAVY),
            ("Unity -executeMethod Build.Run", HEAVY),
            ("Unity -BatchMode -quit", HEAVY),
            ("unity-editor -batchmode -quit -projectPath .", HEAVY),
            (
                "/Applications/Unity/Hub/Editor/6000.0.1f1/Unity.app/Contents/MacOS/Unity -batchmode -quit",
                HEAVY,
            ),
            ("Unity $ARGS", HEAVY),
        ],
    );
}

#[test]
fn wrappers_and_nesting_do_not_hide_a_heavy_build() {
    run_corpus(
        &Fixture::new(),
        &[
            ("env X=1 ./gradlew build", HEAVY),
            ("nice ./gradlew build", HEAVY),
            ("nice -n 15 ./gradlew build", HEAVY),
            ("time ./gradlew build", HEAVY),
            ("nohup ./gradlew build &", HEAVY),
            ("timeout 600 ./gradlew build", HEAVY),
            ("command xcodebuild -scheme App build", HEAVY),
            ("sh -c './gradlew build'", HEAVY),
            ("bash -lc \"xcodebuild -scheme App test\"", HEAVY),
            ("eval './gradlew build'", HEAVY),
            ("cd app && ./gradlew build", HEAVY),
            ("echo $(./gradlew build)", HEAVY),
            ("echo `gradle build`", HEAVY),
            ("echo build | xargs ./gradlew", HEAVY),
            // What xargs appends is unknown.
            ("echo x | xargs ./gradlew --version", HEAVY),
            ("cat args | xargs Unity", HEAVY),
            ("find . -name build.gradle -execdir gradle build \\;", HEAVY),
            // The script a shell is handed is the program.
            ("sh gradlew build", HEAVY),
            ("bash ./gradlew assembleDebug", HEAVY),
            ("bash -x -o pipefail android/gradlew test", HEAVY),
            ("git -c 'alias.b=!./gradlew build' b", Some("git_guard")),
            ("./gradlew build 'oops", Some("unparsable_command")),
            (
                "echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(./gradlew build)))))))))",
                Some("unparsable_command"),
            ),
            // One call in the slot does not cover the one next to it.
            ("aaf slot -- ./gradlew a; ./gradlew b", HEAVY),
            ("aaf slot -- ./gradlew a && ./gradlew b", HEAVY),
            ("./gradlew a | aaf slot -- cat", HEAVY),
            ("aaf gate && ./gradlew build", HEAVY),
            // A substitution runs before the slot is taken.
            ("aaf slot -- echo $(./gradlew -q printVersion)", HEAVY),
            // `slot` must be the subcommand, with `--` before the command.
            ("aaf doctor; ./gradlew build", HEAVY),
        ],
    );
}

#[test]
fn light_invocations_and_mentions_stay_allowed() {
    let fixture = Fixture::new();
    fixture.write("gradlew", "#!/bin/sh\n");
    fixture.write("build.gradle", "\n");
    run_corpus(
        &fixture,
        &[
            ("./gradlew --version", ALLOW),
            ("./gradlew -v", ALLOW),
            ("./gradlew --status", ALLOW),
            ("./gradlew --stop", ALLOW),
            ("./gradlew --help", ALLOW),
            ("./gradlew -h", ALLOW),
            ("./gradlew '-?'", ALLOW),
            // Round 3: these configure the project and start a daemon.
            ("./gradlew help", HEAVY),
            ("./gradlew tasks", HEAVY),
            ("./gradlew projects", HEAVY),
            ("./gradlew properties", HEAVY),
            ("gradle --version", ALLOW),
            ("sh gradlew --version", ALLOW),
            ("env JAVA_HOME=/jdk ./gradlew --stop", ALLOW),
            ("xcodebuild -version", ALLOW),
            ("xcodebuild -showsdks", ALLOW),
            ("xcodebuild -list", ALLOW),
            ("xcodebuild -list -project App.xcodeproj", ALLOW),
            ("xcodebuild -help", ALLOW),
            ("xcodebuild -showBuildSettings -scheme App", ALLOW),
            ("Unity -version", ALLOW),
            ("unity-editor -version", ALLOW),
            // Mentioning a tool is not running it.
            ("echo gradlew", ALLOW),
            ("echo ./gradlew build", ALLOW),
            ("ls gradlew", ALLOW),
            ("cat gradlew", ALLOW),
            ("git add gradlew", ALLOW),
            ("chmod +x gradlew", ALLOW),
            ("grep gradle build.gradle", ALLOW),
            ("cat gradle/wrapper/gradle-wrapper.properties", ALLOW),
            ("cd gradle", ALLOW),
            ("rm -rf .gradle build", ALLOW),
            ("which xcodebuild", ALLOW),
            ("git commit -m 'run ./gradlew build in CI'", ALLOW),
            ("ls /Applications/Unity", ALLOW),
            // Not inspectable: what the script runs is unknown.
            ("bash scripts/build.sh", ALLOW),
            ("cargo build --release", ALLOW),
        ],
    );
}

#[test]
fn a_heavy_build_inside_the_slot_is_allowed() {
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf slot -- ./gradlew :app:lintDebug", ALLOW),
            ("aaf slot -- gradle build", ALLOW),
            ("aaf slot -- xcodebuild -scheme App build", ALLOW),
            ("aaf slot -- Unity -batchmode -runTests", ALLOW),
            ("aaf slot --timeout 600 -- ./gradlew build", ALLOW),
            ("aaf --json slot -- ./gradlew build", ALLOW),
            // Round 3: only the bare name `aaf` is the queue.
            ("/usr/local/bin/aaf slot -- ./gradlew build", HEAVY),
            ("./target/debug/aaf slot -- ./gradlew build", HEAVY),
            ("aaf slot -- sh -c './gradlew a && ./gradlew b'", ALLOW),
            ("aaf slot -- env X=1 nice ./gradlew build", ALLOW),
            ("aaf slot -- bash ./gradlew build", ALLOW),
            ("aaf slot -- env -S './gradlew build'", ALLOW),
            ("env FOO=1 aaf slot -- ./gradlew build", ALLOW),
            ("sh -c 'aaf slot -- ./gradlew build'", ALLOW),
            ("cd android && aaf slot -- ./gradlew build", ALLOW),
            ("aaf slot -- ./gradlew a && aaf slot -- ./gradlew b", ALLOW),
            ("aaf gate", ALLOW),
            ("aaf gate --full", ALLOW),
            ("aaf slot -- cargo test", ALLOW),
        ],
    );
}

/// The slot lifts the heavy guard only: every other guard still judges the
/// command it runs.
#[test]
fn the_slot_does_not_hide_its_command_from_the_other_guards() {
    let fixture = Fixture::new();
    fixture.write(".env", "x");
    run_corpus(
        &fixture,
        &[
            ("aaf slot -- aaf approve", Some("human_action_guard")),
            (
                "aaf slot -- sh -c 'aaf board unfreeze'",
                Some("human_action_guard"),
            ),
            (
                "aaf slot -- git push --force origin main",
                Some("git_guard"),
            ),
            ("aaf slot -- rm .env", Some("delete_guard")),
            ("aaf slot -- cat {data}/receipt.key", Some("data_dir_guard")),
            ("aaf slot -- fastlane deliver", Some("store_submit_guard")),
            ("aaf slot -- git status", ALLOW),
            ("aaf slot -- aaf doctor", ALLOW),
        ],
    );
}

#[test]
fn the_denial_names_the_queue_and_the_way_in_open_to_an_agent() {
    let Decision::Deny(denial) = Fixture::new().bash("./gradlew :app:lintDebug") else {
        panic!("a bare gradlew run was allowed");
    };
    // Round 4: the text no longer offers `aaf gate` to the agent reading it.
    // The gate runs outside the agent's sandbox; `aaf slot` is the agent's way.
    assert_eq!(
        denial.message(),
        "heavy_cmd_guard: heavy builds must share the machine queue — fix: aaf slot -- <command> \
         (aaf gate is run by the factory or the operator, not by an agent)"
    );
    let Decision::Deny(denial) = Fixture::new().bash("AAF_HEAVY_SLOT=0 ./gradlew build") else {
        panic!("a hand-set slot variable was allowed");
    };
    assert_eq!(
        denial.fix,
        "drop the AAF_HEAVY_SLOT assignment and run aaf slot -- <command>"
    );
    // The rule itself is unchanged: the gate's own call is not a bare build.
    assert_eq!(Fixture::new().bash("aaf gate"), Decision::Allow);
}
