#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;

use common::{Fixture, run_corpus, run_file_corpus};

const ADB: Option<&str> = Some("adb_guard");
const DELETE: Option<&str> = Some("delete_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

/// Repo denylist `DENIED1`, global allowlist `OK1`, `OK2`.
fn with_lists() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(".adb-denylist", "# lab phone\nDENIED1\n");
    fs::write(fixture.config.join("adb-allowlist"), "OK1\nOK2 # spare\n").unwrap();
    fixture
}

#[test]
fn adb_guard_bypass_corpus() {
    let rows: &[(&str, Option<&str>)] = &[
        // serial rules
        ("adb -s DENIED1 shell ls", ADB),
        ("adb -sDENIED1 shell ls", ADB),
        ("adb -s OTHER install app.apk", ADB),
        ("adb install app.apk", ADB),
        ("adb shell ls", ADB),
        ("adb -d shell ls", ADB),
        ("ANDROID_SERIAL=DENIED1 adb shell ls", ADB),
        ("ANDROID_SERIAL=OTHER adb logcat -d", ADB),
        ("adb -s $SERIAL shell ls", ADB),
        ("adb -s \"$(adb devices | tail -1)\" shell ls", ADB),
        ("bash -c 'adb -s DENIED1 shell ls'", ADB),
        (
            "echo x && /opt/sdk/platform-tools/adb -s DENIED1 logcat",
            ADB,
        ),
        ("env ANDROID_SERIAL=DENIED1 adb shell ls", ADB),
        ("fastboot -s DENIED1 getvar all", ADB),
        ("fastboot getvar all", ADB),
        ("$ADB shell ls", ADB),
        // destructive commands, even on an allowed device
        ("adb -s OK1 reboot bootloader", ADB),
        ("adb -s OK1 reboot recovery", ADB),
        ("adb -s OK1 reboot-bootloader", ADB),
        ("adb -s OK1 shell rm -rf /", ADB),
        ("adb -s OK1 shell \"rm -rf /sdcard\"", ADB),
        ("adb -s OK1 shell 'su -c \"rm -fr /data/*\"'", ADB),
        ("adb -s OK1 shell 'wipe data'", ADB),
        ("adb -s OK1 shell recovery --wipe_data", ADB),
        ("adb -s OK1 shell dd if=/dev/zero of=/dev/block/sda", ADB),
        ("adb -s OK1 shell 'ls; mkfs.ext4 /dev/block/x'", ADB),
        ("adb -s OK1 shell reboot -p", ADB),
        ("fastboot -s OK1 flash boot boot.img", ADB),
        ("fastboot -s OK1 erase userdata", ADB),
        ("fastboot -s OK1 oem unlock", ADB),
        ("fastboot -s OK1 flashing unlock", ADB),
        ("fastboot -s OK1 -w", ADB),
        (
            "\"$ANDROID_HOME\"/platform-tools/adb -s OK1 reboot recovery",
            ADB,
        ),
        ("${ADB:-adb} reboot bootloader", ADB),
        ("adb reboot bootloader 'oops", OPAQUE),
        // allowed
        ("adb -s OK1 shell ls", ALLOW),
        ("adb -s OK2 install -r app.apk", ALLOW),
        ("ANDROID_SERIAL=OK2 adb install app.apk", ALLOW),
        ("adb devices", ALLOW),
        ("adb devices -l", ALLOW),
        ("adb version", ALLOW),
        ("adb kill-server", ALLOW),
        ("adb -s OK1 reboot", ALLOW),
        (
            "adb -s OK1 shell rm -rf /sdcard/Android/data/com.x/cache",
            ALLOW,
        ),
        ("adb -s OK1 shell pm clear com.x", ALLOW),
        ("adb -s OK1 logcat -d", ALLOW),
        ("fastboot -s OK1 getvar all", ALLOW),
        ("fastboot devices", ALLOW),
        ("fastboot -s OK1 flashing get_unlock_ability", ALLOW),
        ("echo adb reboot bootloader", ALLOW),
        ("grep -n 'adb shell' README.md", ALLOW),
    ];
    assert!(rows.iter().filter(|(_, e)| e.is_some()).count() >= 12);
    run_corpus(&with_lists(), rows);
}

#[test]
fn adb_without_lists_only_blocks_destructive_commands() {
    run_corpus(
        &Fixture::new(),
        &[
            ("adb shell ls", ALLOW),
            ("adb install app.apk", ALLOW),
            ("adb -s ANY logcat -d", ALLOW),
            ("adb reboot bootloader", ADB),
            ("fastboot flash boot boot.img", ADB),
            ("adb shell rm -rf /", ADB),
        ],
    );
}

#[test]
fn adb_denylist_alone_requires_a_serial_but_accepts_others() {
    let fixture = Fixture::new();
    fs::write(fixture.config.join("adb-denylist"), "DENIED9\n").unwrap();
    run_corpus(
        &fixture,
        &[
            ("adb -s DENIED9 shell ls", ADB),
            ("adb shell ls", ADB),
            ("adb -s OTHER shell ls", ALLOW),
        ],
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_adb_list_denies() {
    let fixture = Fixture::new();
    // A directory where the list should be: reading it fails, but not with
    // "not found", so the guard cannot assume the list is empty.
    fs::create_dir(fixture.config.join("adb-denylist")).unwrap();
    run_corpus(&fixture, &[("adb -s X shell ls", ADB)]);
}

fn with_protected_files() -> Fixture {
    let fixture = Fixture::new();
    for file in [
        ".env",
        ".env.production",
        "app/release.keystore",
        "app/upload.jks",
        "app/google-services.json",
        "ios/GoogleService-Info.plist",
        "ios/dist.p12",
        "ios/app.mobileprovision",
        ".autofactory/config.toml",
        ".claude/settings.json",
        ".claude/settings.local.json",
        "build/output.txt",
        "build/app.log",
        "notes.txt",
    ] {
        fixture.write(file, "x");
    }
    fixture
}

#[test]
fn delete_guard_bypass_corpus() {
    let rows: &[(&str, Option<&str>)] = &[
        ("rm .env", DELETE),
        ("rm -f ./app/release.keystore", DELETE),
        ("rm -- .env.production", DELETE),
        ("rm notes.txt app/upload.jks", DELETE),
        ("rm {root}/.autofactory/config.toml", DELETE),
        ("rm .claude/settings.local.json", DELETE),
        ("rm -rf .claude", DELETE),
        ("rm -rf .", DELETE),
        ("rm -rf {root}", DELETE),
        ("unlink app/google-services.json", DELETE),
        ("git rm --cached ios/GoogleService-Info.plist", DELETE),
        ("git -C ios rm dist.p12", DELETE),
        ("mv .env .env.bak", DELETE),
        ("mv notes.txt .env", DELETE),
        ("mv .autofactory /tmp/gone", DELETE),
        ("truncate -s 0 .env.production", DELETE),
        ("shred -u ios/app.mobileprovision", DELETE),
        ("echo x > .env", DELETE),
        ("echo x >.env", DELETE),
        ("cat notes.txt >> .claude/settings.json", DELETE),
        (": 2>&1 > ios/dist.p12", DELETE),
        ("echo hi | tee .env", DELETE),
        ("git clean -fdx", DELETE),
        ("git clean -X -f", DELETE),
        ("git -C app clean -xf", DELETE),
        // globs are expanded against the directory
        ("rm .en*", DELETE),
        ("rm app/*.jks", DELETE),
        ("rm -r .cl?ude", DELETE),
        ("rm */release.keystore", DELETE),
        // quoting, chaining, nesting
        ("rm '.env'", DELETE),
        ("rm \".e\"nv", DELETE),
        ("ls && rm .env", DELETE),
        ("sh -c 'rm .env'", DELETE),
        ("echo $(rm .env)", DELETE),
        ("cd app && rm release.keystore", DELETE),
        ("rm \"$PWD/.env\"", DELETE),
        ("/bin/rm .env", DELETE),
        ("sudo rm .env", DELETE),
        ("xargs rm .env", DELETE),
        ("find . -name .env -exec rm .env \\;", DELETE),
        ("rm .ENV", DELETE),
        ("rm .env 'oops", OPAQUE),
        // allowed
        ("rm build/output.txt", ALLOW),
        ("rm -rf build", ALLOW),
        ("rm -rf build/*", ALLOW),
        ("rm build/*.log", ALLOW),
        ("rm *.missing", ALLOW),
        ("rm /tmp/elsewhere/.env", ALLOW),
        ("cd /tmp && rm .env", ALLOW),
        ("rm env.sample", ALLOW),
        ("git rm notes.txt", ALLOW),
        ("git clean -fd", ALLOW),
        ("git clean -n -x", ALLOW),
        ("git clean --dry-run -fdx", ALLOW),
        ("cat .env", ALLOW),
        ("cp .env /tmp/copy", ALLOW),
        ("mv notes.txt renamed.txt", ALLOW),
        ("echo x > notes.txt", ALLOW),
        ("echo x 2> build/err.txt", ALLOW),
        ("echo \"rm .env\"", ALLOW),
        ("grep -rn 'rm .env' docs", ALLOW),
    ];
    assert!(rows.iter().filter(|(_, e)| e.is_some()).count() >= 12);
    run_corpus(&with_protected_files(), rows);
}

#[test]
fn file_tools_cannot_write_protected_files() {
    run_file_corpus(
        &with_protected_files(),
        &[
            ("Write", ".env", DELETE),
            ("Write", "{root}/.env.local", DELETE),
            ("Edit", ".claude/settings.json", DELETE),
            ("Edit", "{root}/app/../.autofactory/config.toml", DELETE),
            ("MultiEdit", "app/upload.jks", DELETE),
            ("Write", "ios/GoogleService-Info.plist", DELETE),
            ("Write", "app/new.keystore", DELETE),
            ("Write", "src/main.rs", ALLOW),
            ("Edit", "notes.txt", ALLOW),
            ("Write", "/tmp/outside/.env", ALLOW),
            ("Write", ".claude/commands/x.md", ALLOW),
            ("Read", ".env", ALLOW),
        ],
    );
}
