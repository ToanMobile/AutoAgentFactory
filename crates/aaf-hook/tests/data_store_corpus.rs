#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use aaf_hook::shell::{Redirect, parse};
use common::{Fixture, run_corpus, run_file_corpus};

const DATA: Option<&str> = Some("data_dir_guard");
const STORE: Option<&str> = Some("store_submit_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

#[test]
fn data_dir_guard_bypass_corpus() {
    let rows: &[(&str, Option<&str>)] = &[
        ("cat {data}/receipt.key", DATA),
        ("ls {data}", DATA),
        ("ls -la {data}/", DATA),
        ("sqlite3 {data}/aaf.db .dump", DATA),
        ("cat {data}/../data/aaf.db", DATA),
        ("cat {root}/../data/receipts/x/latest.json", DATA),
        ("cat ~/.local/share/autofactory/aaf.db", DATA),
        ("cat $HOME/.local/share/autofactory/aaf.db", DATA),
        ("echo $AAF_DATA_DIR", DATA),
        ("cat \"${AAF_DATA_DIR}/aaf.db\"", DATA),
        ("export AAF_DATA_DIR=/tmp/evil", DATA),
        ("AAF_DATA_DIR=/tmp/evil fastlane test", DATA),
        ("cp receipt.key /tmp/k", DATA),
        ("cat rec''eipt.key", DATA),
        ("cat receipt\\.key", DATA),
        ("bash -c 'cat receipt.key'", DATA),
        ("aaf doctor; cat receipt.key", DATA),
        ("aaf doctor && ls {data}", DATA),
        ("echo x > {data}/receipts/forged.json", DATA),
        ("echo $(cat {data}/aaf.db)", DATA),
        ("cd {data}", DATA),
        ("ln -s {data} here", DATA),
        ("cat receipt.key 'oops", OPAQUE),
        // A path to some `aaf` is not known to be the CLI (audit case 8:
        // `/tmp/evil/aaf` read the key). This row used to be allowed.
        ("/usr/local/bin/aaf approve --key {data}/receipt.key", DATA),
        // allowed
        ("aaf gate --release", ALLOW),
        ("aaf doctor --data-dir {data}", ALLOW),
        ("AAF_DATA_DIR=/tmp/x aaf gate", ALLOW),
        // aaf may name its own directory; approving is refused as a human
        // action since audit round 2.
        (
            "aaf approve --key {data}/receipt.key",
            Some("human_action_guard"),
        ),
        ("aaf gate --data-dir {data}", ALLOW),
        ("ls ~/.local/share", ALLOW),
        ("ls {data}-other/file", ALLOW),
        ("echo autofactory", ALLOW),
        ("cat docs/receipt.md", ALLOW),
        ("cargo test -p aaf-fp", ALLOW),
    ];
    assert!(rows.iter().filter(|(_, e)| e.is_some()).count() >= 12);
    run_corpus(&Fixture::new(), rows);
}

#[test]
fn file_tools_cannot_touch_the_data_dir() {
    run_file_corpus(
        &Fixture::new(),
        &[
            ("Read", "{data}/aaf.db", DATA),
            ("Read", "{data}/receipt.key", DATA),
            ("Write", "{data}/receipts/abc/latest.json", DATA),
            ("Edit", "{data}/approvals/abc/x.json", DATA),
            ("MultiEdit", "{data}/aaf.db", DATA),
            ("Read", "{root}/../data/aaf.db", DATA),
            ("Read", "~/.local/share/autofactory/aaf.db", DATA),
            ("Write", "anywhere/receipt.key", DATA),
            ("Read", "{root}/src/lib.rs", ALLOW),
            ("Read", "{data}-other/file", ALLOW),
            ("Write", "{home}/notes.txt", ALLOW),
        ],
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_into_the_data_dir_is_still_the_data_dir() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink(&fixture.data, fixture.root.join("innocent")).unwrap();
    run_file_corpus(
        &fixture,
        &[
            ("Read", "innocent/aaf.db", DATA),
            ("Write", "{root}/innocent/receipts/new.json", DATA),
        ],
    );
}

/// The fixture has no key and no receipt, so every command recognised as a
/// store submission is refused: this corpus tests the matching.
#[test]
fn store_submit_guard_matching_corpus() {
    let rows: &[(&str, Option<&str>)] = &[
        ("fastlane deliver", STORE),
        ("fastlane supply --aab app.aab", STORE),
        ("fastlane pilot upload", STORE),
        ("fastlane run upload_to_app_store", STORE),
        ("bundle exec fastlane upload_to_testflight", STORE),
        ("fastlane ios deliver --force", STORE),
        ("eas submit -p ios", STORE),
        ("npx eas-cli submit --platform android", STORE),
        ("eas build --auto-submit", STORE),
        ("xcrun altool --upload-app -f app.ipa", STORE),
        ("xcrun altool --upload-package app.ipa", STORE),
        ("./gradlew publishReleaseBundle", STORE),
        ("./gradlew :app:publishBundle", STORE),
        ("./gradlew promoteArtifact --track production", STORE),
        ("cd android && ./gradlew publishBundle", STORE),
        ("bash -c \"fastlane deliver\"", STORE),
        ("FASTLANE_SKIP_UPDATE_CHECK=1 fastlane deliver", STORE),
        ("echo ok; \"fastlane\" 'deliver'", STORE),
        ("echo $(eas submit)", STORE),
        ("fastlane deliver 'oops", OPAQUE),
        // allowed
        ("fastlane test", ALLOW),
        ("fastlane ios build", ALLOW),
        ("fastlane --help", ALLOW),
        ("eas build -p ios", ALLOW),
        ("eas whoami", ALLOW),
        ("xcrun altool --validate-app -f app.ipa", ALLOW),
        ("xcrun simctl list", ALLOW),
        // Not submissions, but bare Gradle runs are refused by the heavy
        // command guard since audit round 2; in a slot they pass.
        ("./gradlew assembleRelease", Some("heavy_cmd_guard")),
        ("./gradlew test", Some("heavy_cmd_guard")),
        ("gradle build", Some("heavy_cmd_guard")),
        ("aaf slot -- ./gradlew assembleRelease", ALLOW),
        ("aaf slot -- ./gradlew test", ALLOW),
        ("aaf slot -- gradle build", ALLOW),
        ("aaf slot -- ./gradlew publishReleaseBundle", STORE),
        ("echo fastlane deliver", ALLOW),
        ("cat fastlane/Fastfile", ALLOW),
    ];
    assert!(rows.iter().filter(|(_, e)| e.is_some()).count() >= 12);
    let fixture = Fixture::new();
    run_corpus(&fixture, rows);
    let denial = match fixture.bash("fastlane deliver") {
        aaf_hook::Decision::Deny(denial) => denial,
        other => panic!("expected a denial, got {other:?}"),
    };
    // Inside a factory worker run, `aaf gate --release` cannot be run by the
    // agent, so the fix guides the worker to build/verify locally instead.
    assert_eq!(denial.fix, aaf_hook::store_guard::FIX_IN_FACTORY_RUN);

    // Interactive operator sessions keep the `aaf gate --release` fix.
    let interactive_denial = match Fixture::new().interactive().bash("fastlane deliver") {
        aaf_hook::Decision::Deny(denial) => denial,
        other => panic!("expected a denial, got {other:?}"),
    };
    assert_eq!(interactive_denial.fix, "aaf gate --release");
}

/// The reader of the data guard's fix is an agent. Inside a factory run
/// `aaf gate` cannot work (the factory runs it outside the sandbox) and
/// `aaf doctor` cannot read the data directory, so neither is offered any
/// more: the text was changed on purpose to name what an agent can run.
#[test]
fn the_data_guard_fix_offers_only_what_an_agent_can_run() {
    let denial = match Fixture::new().bash("cat receipt.key") {
        aaf_hook::Decision::Deny(denial) => denial,
        other => panic!("expected a denial, got {other:?}"),
    };
    assert_eq!(denial.rule, "data_dir_guard");
    assert_eq!(
        denial.fix,
        "leave it alone: the factory or the operator runs aaf gate after the agent ends; an \
         agent runs its tests directly and heavy builds through aaf slot -- <command>"
    );
}

fn argv(src: &str) -> Vec<Vec<String>> {
    parse(src).cmds.into_iter().map(|c| c.words).collect()
}

#[test]
fn tokenizer_removes_quotes_and_splits_on_operators() {
    assert_eq!(
        argv("a 'b c' \"d e\" f\\ g; h && i || j | k\nl"),
        vec![
            vec!["a", "b c", "d e", "f g"],
            vec!["h"],
            vec!["i"],
            vec!["j"],
            vec!["k"],
            vec!["l"],
        ]
    );
}

#[test]
fn tokenizer_strips_wrappers_and_keeps_assignments() {
    let script = parse("A=1 B='two words' env -u X C=3 command /usr/bin/git -C /p status");
    let cmd = &script.cmds[0];
    assert_eq!(cmd.name(), "git");
    assert_eq!(cmd.args(), ["-C", "/p", "status"]);
    assert_eq!(
        cmd.assigns,
        [
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "two words".to_string()),
            ("C".to_string(), "3".to_string()),
        ]
    );
    assert!(!cmd.dynamic_name);
}

#[test]
fn tokenizer_recurses_into_substitutions_and_interpreters() {
    let names = |src: &str| -> Vec<String> {
        parse(src)
            .cmds
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    };
    assert_eq!(
        names("echo $(one) `two` \"$(three)\""),
        ["one", "two", "three", "echo"]
    );
    assert_eq!(names("bash -c 'four; five'"), ["four", "five", "bash"]);
    assert_eq!(names("eval six"), ["six", "eval"]);
    assert!(parse("$(pick) arg").cmds.iter().any(|c| c.dynamic_name));
}

#[test]
fn tokenizer_reads_redirects_and_file_descriptors() {
    let script = parse("cmd arg >out.txt 2>&1 <in.txt &>>all.log");
    let cmd = &script.cmds[0];
    assert_eq!(cmd.words, ["cmd", "arg"]);
    let redirect = |op: &str, target: &str| Redirect {
        op: op.to_string(),
        target: target.to_string(),
    };
    assert_eq!(
        cmd.redirects,
        [
            redirect(">", "out.txt"),
            redirect(">&", "1"),
            redirect("<", "in.txt"),
            redirect("&>>", "all.log"),
        ]
    );
}

#[test]
fn tokenizer_reports_what_it_cannot_split() {
    assert!(parse("echo 'open").opaque.len() == 1);
    assert!(parse("echo \"open").opaque.len() == 1);
    assert!(parse("echo $(open").opaque.len() == 1);
    assert!(parse("echo `open").opaque.len() == 1);
    assert!(parse("echo done").opaque.is_empty());
}
