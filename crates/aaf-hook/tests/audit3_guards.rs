#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-3 audit, the other guards: `cargo run -- approve` is `aaf approve`,
//! and an option glued to its value is read like the spaced form.

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const HUMAN: Option<&str> = Some("human_action_guard");
const ADB: Option<&str> = Some("adb_guard");
const STORE: Option<&str> = Some("store_submit_guard");
const ALLOW: Option<&str> = None;

#[test]
fn cargo_run_reaches_the_human_actions_without_naming_the_crate() {
    run_corpus(
        &Fixture::new(),
        &[
            ("cargo run -- approve --reviewer bob", HUMAN),
            ("cargo run -- board unblock T-3", HUMAN),
            ("cargo run -- board unfreeze", HUMAN),
            ("cargo run -q -- approve", HUMAN),
            ("cargo r -- approve", HUMAN),
            ("cargo +nightly run -- approve", HUMAN),
            ("cargo run --release -- --json approve", HUMAN),
            ("cd crates/aaf-cli && cargo run -- approve", HUMAN),
            (
                "cargo run --manifest-path crates/aaf-cli/Cargo.toml -- approve",
                HUMAN,
            ),
            // The aaf CLI named in every spelling of the selector.
            ("cargo run -p aaf-cli -- approve", HUMAN),
            ("cargo run -paaf-cli -- approve", HUMAN),
            ("cargo run --package=aaf-cli -- board unfreeze", HUMAN),
            ("cargo run --bin=aaf -- approve", HUMAN),
            // A selector filled in at run time does not clearly name another.
            ("cargo run -p \"$CRATE\" -- approve", HUMAN),
            ("cargo run --bin $BIN -- board unblock T-3", HUMAN),
            // One selector for another crate does not excuse one for aaf.
            ("cargo run -p other-crate --bin aaf -- approve", HUMAN),
            // cargo passes trailing words to the program without `--` too.
            ("cargo run approve", HUMAN),
            ("cargo run -q board unfreeze", HUMAN),
            ("cargo run -p aaf-cli approve", HUMAN),
            // With aaf named, a subcommand built at run time is refused.
            ("cargo run -p aaf-cli -- $SUB", HUMAN),
            // allowed
            ("cargo run -p other-crate -- approve", ALLOW),
            ("cargo run --package other-crate -- approve", ALLOW),
            ("cargo run --package=other-crate -- board unfreeze", ALLOW),
            ("cargo run -pother-crate -- approve", ALLOW),
            ("cargo run --bin tool -- approve", ALLOW),
            ("cargo run --bin=tool -- board unblock T-3", ALLOW),
            ("cargo run -p other-crate approve", ALLOW),
            ("cargo run -- gate", ALLOW),
            ("cargo run -- board list", ALLOW),
            ("cargo run -- board show unblock", ALLOW),
            // Some other project's program: only the literal actions count.
            ("cargo run -- \"$@\"", ALLOW),
            ("cargo run -- $ARGS", ALLOW),
            ("cargo run -- *.txt", ALLOW),
            ("cargo run", ALLOW),
            ("cargo run --release", ALLOW),
            ("cargo test approve", ALLOW),
            ("cargo build -p aaf-cli", ALLOW),
        ],
    );
}

/// Repo denylist `DENIED1`, global allowlist `OK1`.
fn with_lists() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(".adb-denylist", "DENIED1\n");
    fs::write(fixture.config.join("adb-allowlist"), "OK1\n").unwrap();
    fixture
}

#[test]
fn a_glued_serial_is_judged_like_a_spaced_one() {
    run_corpus(
        &with_lists(),
        &[
            ("adb -sFAKE123 uninstall com.app", ADB),
            ("adb -s FAKE123 uninstall com.app", ADB),
            ("adb -sDENIED1 uninstall com.app", ADB),
            ("adb -s$SERIAL shell ls", ADB),
            ("adb -sOK1 reboot bootloader", ADB),
            ("adb -sOK1 shell rm -rf /sdcard", ADB),
            ("fastboot -sFAKE123 getvar all", ADB),
            ("fastboot -sDENIED1 getvar all", ADB),
            ("fastboot -sOK1 flash boot boot.img", ADB),
            ("fastboot -sOK1 -w", ADB),
            // fastboot reads its short options as a cluster.
            ("fastboot -sOK1 -vw", ADB),
            ("fastboot -s OK1 -wv", ADB),
            ("fastboot -sOK1 getvar all -vw", ADB),
            ("fastboot -vsOK1 -w", ADB),
            ("fastboot -vs OK1 erase userdata", ADB),
            ("fastboot -vsDENIED1 getvar all", ADB),
            ("fastboot -vsFAKE123 getvar all", ADB),
            // allowed
            ("adb -sOK1 uninstall com.app", ALLOW),
            ("adb -s OK1 uninstall com.app", ALLOW),
            ("fastboot -sOK1 getvar all", ALLOW),
            ("fastboot -vsOK1 getvar all", ALLOW),
            ("fastboot -vs OK1 getvar all", ALLOW),
            ("fastboot -sOK1 -v getvar all", ALLOW),
        ],
    );
}

#[test]
fn glued_fastboot_options_without_lists() {
    run_corpus(
        &Fixture::new(),
        &[
            ("fastboot -vw", ADB),
            ("fastboot -wv", ADB),
            ("fastboot getvar all -vw", ADB),
            ("fastboot --slot=all erase userdata", ADB),
            ("fastboot --slot=a flash boot boot.img", ADB),
            ("fastboot -S100M flash system system.img", ADB),
            ("fastboot -S 100M flash system system.img", ADB),
            ("fastboot --cmdline=x oem unlock", ADB),
            // allowed
            ("adb -sFAKE123 uninstall com.app", ALLOW),
            ("fastboot --slot=a getvar all", ALLOW),
            ("fastboot -v getvar all", ALLOW),
            // `w123` is the serial here, not the wipe flag.
            ("fastboot -sw123 getvar all", ALLOW),
            ("fastboot -s w123 getvar all", ALLOW),
        ],
    );
}

/// No receipt in the fixture: a recognised submission is refused.
#[test]
fn glued_options_do_not_hide_a_store_submission() {
    run_corpus(
        &Fixture::new(),
        &[
            ("eas build --auto-submit", STORE),
            ("eas build --auto-submit-with-profile=prod", STORE),
            ("eas --profile=prod submit", STORE),
            ("eas submit --platform=ios", STORE),
            ("xcrun altool --upload-app=app.ipa", STORE),
            ("xcrun altool --upload-package=app.ipa -t ios", STORE),
            ("npx -peas-cli eas submit", STORE),
            ("npx --package=eas-cli eas submit", STORE),
            ("bundle exec --gemfile=Gemfile fastlane deliver", STORE),
            ("fastlane --env=prod deliver", STORE),
            ("fastlane run upload_to_testflight", STORE),
            ("./gradlew -Pv=1 :app:publishReleaseBundle", STORE),
            ("./gradlew --project-dir=android publishBundle", STORE),
            // allowed
            ("eas build --profile=prod", ALLOW),
            ("fastlane --env=prod test", ALLOW),
        ],
    );
}
