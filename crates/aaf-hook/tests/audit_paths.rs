#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Bypasses an independent audit found in the delete, data-dir, store and
//! device guards, each next to the calls that must stay allowed.

mod common;

use std::fs;
use std::os::unix::fs::symlink;

use common::{Fixture, run_corpus, run_corpus_in, run_file_corpus};

const ADB: Option<&str> = Some("adb_guard");
const DATA: Option<&str> = Some("data_dir_guard");
const DELETE: Option<&str> = Some("delete_guard");
const SECRET: Option<&str> = Some("secret_guard");
const STORE: Option<&str> = Some("store_submit_guard");
const ALLOW: Option<&str> = None;

/// `.env`, a keystore and plain files, plus symlinks to each kind.
fn with_links() -> Fixture {
    let fixture = Fixture::new();
    for file in [".env", "app/release.keystore", "notes.txt", "build/a.tmp"] {
        fixture.write(file, "x");
    }
    fs::write(fixture.home.join(".env"), "x").unwrap();
    symlink(".env", fixture.root.join("env_link")).unwrap();
    symlink("app", fixture.root.join("app_link")).unwrap();
    symlink("notes.txt", fixture.root.join("notes_link")).unwrap();
    // Out of the repository in both directions.
    symlink(fixture.home.join(".env"), fixture.root.join("ext_link")).unwrap();
    symlink(fixture.root.join(".env"), fixture.home.join("in_link")).unwrap();
    fixture
}

#[test]
fn file_tools_follow_a_symlink_to_a_protected_file() {
    run_file_corpus(
        &with_links(),
        &[
            ("Write", "env_link", DELETE),
            ("Edit", "env_link", DELETE),
            ("MultiEdit", "{root}/env_link", DELETE),
            ("Write", "app_link/release.keystore", DELETE),
            ("Edit", "{home}/in_link", DELETE),
            // allowed
            ("Write", "notes_link", ALLOW),
            ("Edit", "{root}/notes_link", ALLOW),
            ("Write", "ext_link", ALLOW),
            ("Read", "notes_link", ALLOW),
            // reading a symlink to `.env` is blocked by secret_guard
            ("Read", "env_link", SECRET),
        ],
    );
}

#[test]
fn bash_follows_a_symlink_to_a_protected_file() {
    run_corpus(
        &with_links(),
        &[
            ("echo x > env_link", DELETE),
            ("echo x >> ./env_link", DELETE),
            ("truncate -s 0 env_link", DELETE),
            ("rm env_link", DELETE),
            ("shred -u env_link", DELETE),
            ("mv notes.txt env_link", DELETE),
            ("echo hi | tee env_link", DELETE),
            ("rm env_lin*", DELETE),
            ("echo x > {home}/in_link", DELETE),
            // allowed
            ("echo x > notes_link", ALLOW),
            ("rm notes_link", ALLOW),
            ("truncate -s 0 notes_link", ALLOW),
            ("echo x > ext_link", ALLOW),
            ("cat notes_link", ALLOW),
            // reading a symlink to `.env` is blocked by secret_guard
            ("cat env_link", SECRET),
        ],
    );
}

#[test]
fn find_may_delete_only_what_cannot_be_protected() {
    run_corpus(
        &with_links(),
        &[
            ("find . -name .env -delete", DELETE),
            ("find -name .env -delete", DELETE),
            ("find . -delete", DELETE),
            ("find . -type f -delete", DELETE),
            ("find {root} -name '.env*' -delete", DELETE),
            ("find . -name '*.jks' -delete", DELETE),
            ("find . -iname '*.KEYSTORE' -delete", DELETE),
            ("find . -name '*.local' -delete", DELETE),
            ("find . -name '?env*' -delete", DELETE),
            ("find app -name '*' -delete", DELETE),
            ("find . -name '*.json' -delete", DELETE),
            // Operators break "every deleted file passed the name filter".
            ("find . -name '*.tmp' -o -name .env -delete", DELETE),
            ("find . ! -name '*.tmp' -delete", DELETE),
            ("find . -not -name '*.tmp' -delete", DELETE),
            // A filter after the action filters nothing.
            ("find . -delete -name '*.tmp'", DELETE),
            // The filter is on the path, which this guard cannot prove.
            ("find . -path './build/*' -delete", DELETE),
            // -exec with a deleting tool is the same action.
            ("find . -name .env -exec rm {} \\;", DELETE),
            ("find . -name '*.keystore' -exec rm -f {} +", DELETE),
            ("find . -name '.env*' -execdir /bin/rm {} \\;", DELETE),
            // A shell given the match can delete it too.
            ("find . -name .env -exec sh -c 'rm \"$1\"' _ {} \\;", DELETE),
            ("find . -name '*.jks' -exec bash -c 'rm $0' {} \\;", DELETE),
            ("find . -type f -exec shred -u {} \\;", DELETE),
            ("sudo find . -name .env -delete", DELETE),
            ("sh -c 'find . -name .env -delete'", DELETE),
            // allowed
            ("find . -name '*.tmp' -delete", ALLOW),
            ("find build -type f -name '*.log' -delete", ALLOW),
            ("find . -type f -name '*.o' -mtime +7 -delete", ALLOW),
            ("find . -name '*.tmp' -exec rm {} \\;", ALLOW),
            ("find . -name .env", ALLOW),
            ("find . -name .env -print", ALLOW),
            ("find . -name .env -exec cat {} \\;", ALLOW),
            ("find /tmp/elsewhere -name .env -delete", ALLOW),
            ("find /tmp/elsewhere -delete", ALLOW),
        ],
    );
}

#[test]
fn a_relative_path_into_the_data_dir_is_the_data_dir() {
    let fixture = Fixture::new();
    fixture.write("notes.txt", "x");
    // The data dir is `data` next to the repository: call from their parent.
    let base = fixture.root.parent().unwrap().to_path_buf();
    fs::create_dir_all(base.join("database")).unwrap();
    run_corpus_in(
        &fixture,
        &base,
        &[
            ("cat data/aaf.db", DATA),
            ("ls data", DATA),
            ("sqlite3 data/aaf.db .dump", DATA),
            ("cat repo/../data/aaf.db", DATA),
            ("echo x > data/receipts/forged.json", DATA),
            ("dd if=data/aaf.db of=/tmp/copy", DATA),
            ("tar cf /tmp/x.tar --directory=data .", DATA),
            // allowed
            ("cat repo/notes.txt", ALLOW),
            ("ls database", ALLOW),
            ("cat data-other/file", ALLOW),
            ("ls -la repo", ALLOW),
        ],
    );
}

#[test]
fn a_symlink_into_the_data_dir_is_the_data_dir_for_bash() {
    let fixture = Fixture::new();
    fs::write(fixture.home.join("notes.txt"), "x").unwrap();
    symlink(&fixture.data, fixture.root.join("data_link")).unwrap();
    symlink(&fixture.home, fixture.root.join("home_link")).unwrap();
    run_corpus(
        &fixture,
        &[
            ("cat ./data_link/aaf.db", DATA),
            ("cat data_link/aaf.db", DATA),
            ("ls data_link", DATA),
            ("echo x > data_link/receipts/forged.json", DATA),
            ("cat {root}/data_link/receipt", DATA),
            ("cp data_link/aaf.db /tmp/copy", DATA),
            // allowed
            ("cat ./home_link/notes.txt", ALLOW),
            ("ls home_link", ALLOW),
            ("echo x > home_link/new.txt", ALLOW),
        ],
    );
}

#[test]
fn only_the_bare_aaf_command_may_name_the_data_dir() {
    run_corpus(
        &Fixture::new(),
        &[
            ("/tmp/evil/aaf cat $AAF_DATA_DIR/receipt.key", DATA),
            ("./aaf cat {data}/receipt.key", DATA),
            ("/usr/local/bin/aaf approve --key {data}/receipt.key", DATA),
            // Redirects are done by the shell, not by aaf.
            ("aaf < $AAF_DATA_DIR/receipt.key", DATA),
            ("aaf gate > {data}/receipts/forged.json", DATA),
            ("aaf doctor 2>> {data}/aaf.db", DATA),
            // A command word built at run time is not known to be aaf.
            ("$DIR/aaf cat {data}/receipt.key", DATA),
            ("$(printf /tmp/evil/)aaf cat {data}/receipt.key", DATA),
            // allowed
            ("aaf doctor --data-dir {data}", ALLOW),
            // The data-dir guard lets aaf name its own directory; approving is
            // refused as a human action since audit round 2.
            (
                "aaf approve --key {data}/receipt.key",
                Some("human_action_guard"),
            ),
            ("aaf gate --data-dir {data}", ALLOW),
            ("aaf gate --release > gate.log", ALLOW),
            ("aaf gate 2>&1", ALLOW),
            ("AAF_DATA_DIR=/tmp/x aaf gate", ALLOW),
            ("command aaf gate --release", ALLOW),
        ],
    );
}

#[test]
fn an_option_value_does_not_hide_the_store_or_device_subcommand() {
    run_corpus(
        &Fixture::new(),
        &[
            ("eas --profile prod submit", STORE),
            ("npx eas-cli --non-interactive --profile prod submit", STORE),
            ("eas --platform ios submit --latest", STORE),
            ("fastboot --slot a flash boot boot.img", ADB),
            ("fastboot --slot all erase userdata", ADB),
            ("fastboot --cmdline x oem unlock", ADB),
            ("adb --one-device X reboot bootloader", ADB),
            // allowed
            ("eas build --profile prod", ALLOW),
            ("eas --profile prod build", ALLOW),
            ("eas whoami", ALLOW),
            ("fastboot --slot a getvar all", ALLOW),
            ("fastboot flashing get_unlock_ability", ALLOW),
            ("adb --one-device X shell ls", ALLOW),
        ],
    );
}
