//! The heavy-command classifier, as a table.

use super::super::heavy::{HeavyTool, heavy_tool, heavy_tool_argv, unqueued_heavy_tool};

const GRADLE: Option<HeavyTool> = Some(HeavyTool::Gradle);
const XCODE: Option<HeavyTool> = Some(HeavyTool::Xcodebuild);
const UNITY: Option<HeavyTool> = Some(HeavyTool::Unity);

#[test]
fn build_tools_are_found_where_a_command_starts() {
    let table: &[(&str, Option<HeavyTool>)] = &[
        ("gradle build", GRADLE),
        ("gradlew test", GRADLE),
        ("./gradlew :app:lintDebug --no-daemon", GRADLE),
        ("/opt/gradle/bin/gradle assemble", GRADLE),
        ("\"$ROOT/gradlew\" test", GRADLE),
        ("sh -c './gradlew test'", GRADLE),
        ("bash gradlew test", GRADLE),
        ("cd android && ./gradlew assembleDebug", GRADLE),
        ("sh -c 'cd app && ./gradlew test'", GRADLE),
        ("(cd app; ./gradlew test)", GRADLE),
        ("CI=1 JAVA_HOME=/jdk ./gradlew test", GRADLE),
        ("env CI=1 nice -n 10 ./gradlew test", GRADLE),
        ("taskpolicy -b nice -n 10 sh -c './gradlew test'", GRADLE),
        ("timeout 600 ./gradlew test", GRADLE),
        ("if ! ./gradlew lint; then exit 1; fi", GRADLE),
        ("true\n./gradlew test\n", GRADLE),
        ("echo start | xargs ./gradlew", GRADLE),
        ("exec ./gradlew \\\n  test", GRADLE),
        ("gradlew.bat test", GRADLE),
        ("aaf slot -- ./gradlew test", GRADLE),
        ("xcodebuild -scheme App test", XCODE),
        ("xcrun xcodebuild -version", XCODE),
        ("set -o pipefail && xcodebuild test | xcpretty", XCODE),
        (
            "/Applications/Unity/Hub/Editor/6000.0.1f1/Unity.app/Contents/MacOS/Unity -quit",
            UNITY,
        ),
        ("unity-editor -runTests", UNITY),
        ("Unity.exe -quit", UNITY),
        (
            "'/Applications/Unity Hub/Editor/x/Unity' -batchmode -quit",
            UNITY,
        ),
        ("./ci/run_editor.sh -batchmode -nographics", UNITY),
        ("./ci/editor.sh -runTests -testPlatform EditMode", UNITY),
        ("./ci/editor.sh -executeMethod Build.All", UNITY),
        ("./ci/editor.sh -BATCHMODE", UNITY),
        // Whatever the case of the name, and also when only asked for
        // information: the hook lets these through, here they run alone.
        ("GRADLEW test", GRADLE),
        ("./gradlew --version", GRADLE),
        ("gradle --stop", GRADLE),
        ("XcodeBuild -version", XCODE),
        ("xcodebuild -showsdks", XCODE),
        ("UNITY -quit", UNITY),
        ("unity -version", UNITY),
        // Accepted over-classification: a separator inside quotes starts a
        // command, and so does `--`.
        ("echo \"then; gradle\"", GRADLE),
        ("git checkout -- gradlew", GRADLE),
        // Not a build tool: an argument, a directory, a comment, a
        // different program.
        ("echo gradle", None),
        ("echo ./gradlew test", None),
        ("ls gradle/", None),
        ("cat gradle/libs.versions.toml", None),
        ("./gradle/tools/format.sh", None),
        ("chmod +x gradlew", None),
        ("git add gradlew gradle/wrapper", None),
        ("# ./gradlew test", None),
        ("true # then xcodebuild", None),
        ("GRADLE_OPTS=-Xmx2g make test", None),
        ("cargo test --workspace", None),
        ("npm test -- --watch=false", None),
        ("open -a Unity", None),
        ("sh ./scripts/build.sh", None),
        ("", None),
    ];
    for (command, expected) in table {
        assert_eq!(heavy_tool(command), *expected, "{command:?}");
    }
}

#[test]
fn command_handed_to_the_queue_is_not_reported_as_unqueued() {
    let table: &[(&str, Option<HeavyTool>)] = &[
        ("./gradlew lint", GRADLE),
        ("aaf slot -- ./gradlew lint", None),
        (
            "aaf slot --label lint --timeout 600 -- ./gradlew lint",
            None,
        ),
        ("exec /usr/local/bin/aaf slot -- xcodebuild test", None),
        ("aaf gate --full", None),
        // Only the queued command is excused, not the one after it.
        ("aaf slot -- ./gradlew lint && ./gradlew test", GRADLE),
        ("aaf slot -- true; Unity -batchmode", UNITY),
        // Another `aaf` command queues nothing.
        ("aaf doctor && ./gradlew lint", GRADLE),
        ("echo aaf slot; ./gradlew lint", GRADLE),
    ];
    for (command, expected) in table {
        assert_eq!(unqueued_heavy_tool(command), *expected, "{command:?}");
    }
}

#[test]
fn argument_vector_is_classified_like_the_line_it_spells() {
    assert_eq!(heavy_tool_argv(&["./gradlew", "test"]), GRADLE);
    assert_eq!(
        heavy_tool_argv(&["sh", "-c", "cd app && ./gradlew test"]),
        GRADLE
    );
    assert_eq!(heavy_tool_argv(&["xcodebuild"]), XCODE);
    assert_eq!(heavy_tool_argv(&["echo", "gradle"]), None);
    assert_eq!(heavy_tool_argv(&["sh", "-c", "sleep 1"]), None);
    assert_eq!(heavy_tool_argv::<&str>(&[]), None);
}

#[test]
fn argument_vector_is_read_one_element_at_a_time() {
    // A space inside an element is part of the path, not a word break.
    assert_eq!(heavy_tool_argv(&["/tmp/my app/gradlew", "test"]), GRADLE);
    assert_eq!(
        heavy_tool_argv(&["/Applications/Unity Hub/Unity", "-batchmode"]),
        UNITY
    );
    assert_eq!(
        heavy_tool_argv(&["nice", "-n", "10", "/tmp/my app/gradlew", "test"]),
        GRADLE
    );
    assert_eq!(
        heavy_tool_argv(&["env", "CI=1", "/Volumes/My Disk/xcodebuild"]),
        XCODE
    );
    assert_eq!(heavy_tool_argv(&["/tmp/my app/run.sh", "-runTests"]), UNITY);
    assert_eq!(heavy_tool_argv(&["aaf", "slot", "--", "./gradlew"]), GRADLE);
    assert_eq!(heavy_tool_argv(&["bash", "gradlew", "test"]), GRADLE);
    assert_eq!(
        heavy_tool_argv(&["bash", "-lc", "cd app; ./gradlew test"]),
        GRADLE
    );
    // No shell reads these elements: a separator inside one starts nothing,
    // and a path that only passes through a directory named like a tool is
    // some other program.
    assert_eq!(heavy_tool_argv(&["echo", "a; gradle build"]), None);
    assert_eq!(heavy_tool_argv(&["/opt/gradle build/run.sh"]), None);
    assert_eq!(heavy_tool_argv(&["/tmp/my app/tool", "gradlew"]), None);
    // Only the script after `-c` is shell text; its arguments are not.
    assert_eq!(
        heavy_tool_argv(&["sh", "-c", "echo \"$1\"", "sh", "gradle; x"]),
        None
    );
}

#[test]
fn value_of_a_wrapper_option_is_not_taken_for_the_command() {
    let table: &[(&str, Option<HeavyTool>)] = &[
        ("env -u FOO gradle build", GRADLE),
        ("env --unset FOO -C /src gradle build", GRADLE),
        ("sudo -u builder ./gradlew test", GRADLE),
        ("sudo -g staff -u builder ./gradlew test", GRADLE),
        ("doas -u builder ./gradlew test", GRADLE),
        ("nice -n 10 ./gradlew test", GRADLE),
        ("nice --adjustment 10 ./gradlew test", GRADLE),
        ("timeout -s KILL -k 5 600 ./gradlew test", GRADLE),
        ("echo x | xargs -I X -n 1 -P 2 ./gradlew X", GRADLE),
        ("arch -arch arm64 xcodebuild test", XCODE),
        ("xcrun --sdk iphoneos xcodebuild test", XCODE),
        ("stdbuf -o L ./gradlew test", GRADLE),
        ("ionice -c 3 ./gradlew test", GRADLE),
        ("setsid ./gradlew test", GRADLE),
        ("caffeinate -t 600 ./gradlew test", GRADLE),
        ("taskpolicy -c utility ./gradlew test", GRADLE),
        ("bash -o pipefail ./gradlew test", GRADLE),
        // The value is named like a tool; the command is not one.
        ("env -u gradle true", None),
        ("sudo -u gradle true", None),
        ("timeout -s gradle 5 true", None),
        ("xargs -I gradle echo gradle", None),
        ("arch -arch gradle true", None),
    ];
    for (command, expected) in table {
        assert_eq!(heavy_tool(command), *expected, "{command:?}");
        if !command.contains('|') {
            let argv: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(heavy_tool_argv(&argv), *expected, "argv {command:?}");
        }
    }
}
