#![allow(clippy::unwrap_used, clippy::expect_used)]

//! V3 human-action guard coverage: `approve`, `run`, `init`, `gc`, `proof record`,
//! `skills sync`, and board mutations (`unblock`, `unfreeze`, `rework`,
//! `import-issues`, `sync-github`, `split`, `dep`, `approve-plan`) directly,
//! via PTY launchers, and via `cargo run` with or without `--`.

mod common;

use common::{Fixture, run_corpus};

const HUMAN: Option<&str> = Some("human_action_guard");
const TEXT: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

#[test]
fn guards_every_v3_human_subcommand_directly() {
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf approve", HUMAN),
            ("aaf run", HUMAN),
            ("aaf --json run --once", HUMAN),
            ("aaf init", HUMAN),
            ("aaf --json init --with-skills", HUMAN),
            ("aaf gc", HUMAN),
            ("aaf --json gc --apply", HUMAN),
            (
                "aaf proof record --image screen.png --device emu-5554",
                HUMAN,
            ),
            ("aaf --json proof record --image screen.png", HUMAN),
            ("aaf skills sync", HUMAN),
            ("aaf --json skills sync --all --force", HUMAN),
            ("aaf board unblock T-1", HUMAN),
            ("aaf board unfreeze", HUMAN),
            ("aaf board rework T-1 --note 'fix edge case'", HUMAN),
            ("aaf board import-issues", HUMAN),
            ("aaf board sync-github", HUMAN),
            ("aaf board split T-1", HUMAN),
            ("aaf --json board --json split T-1", HUMAN),
            ("aaf board dep T-2 --after T-1", HUMAN),
            ("aaf --json board dep T-2 --remove T-1", HUMAN),
            ("aaf board approve-plan T-1", HUMAN),
            ("aaf --json board approve-plan T-1", HUMAN),
            // Dynamic subcommands under known aaf remain refused.
            ("aaf proof $ACTION", HUMAN),
            ("aaf skills $ACTION", HUMAN),
            ("aaf board $ACTION T-1", HUMAN),
            // Unparsable commands with guarded pairs are caught by fallback.
            ("aaf proof record 'oops", TEXT),
            ("aaf skills sync 'oops", TEXT),
            ("aaf board rework T-1 'oops", TEXT),
            ("aaf board split T-1 'oops", TEXT),
            ("aaf board dep T-2 --after T-1 'oops", TEXT),
            ("aaf board approve-plan T-1 'oops", TEXT),
            ("aaf run 'oops", TEXT),
            ("aaf init 'oops", TEXT),
            ("aaf gc 'oops", TEXT),
        ],
    );
}

#[test]
fn guards_v3_human_subcommands_via_launchers() {
    run_corpus(
        &Fixture::new(),
        &[
            ("script -q /dev/null aaf approve", HUMAN),
            ("script -q /dev/null aaf run", HUMAN),
            ("script -q /dev/null aaf --json init", HUMAN),
            ("script -q /dev/null aaf gc --apply", HUMAN),
            ("script -q /dev/null aaf --json gc", HUMAN),
            (
                "script -q /dev/null aaf proof record --image x.png --device emu",
                HUMAN,
            ),
            ("script -q /dev/null aaf skills sync --all", HUMAN),
            ("script -q /dev/null aaf board unblock T-1", HUMAN),
            ("script -q /dev/null aaf board unfreeze", HUMAN),
            ("script -q /dev/null aaf board rework T-1", HUMAN),
            ("script -q /dev/null aaf board import-issues", HUMAN),
            ("script -q /dev/null aaf board sync-github", HUMAN),
            ("script -q /dev/null aaf board split T-1", HUMAN),
            ("script -q /dev/null aaf board dep T-2 --after T-1", HUMAN),
            ("script -q /dev/null aaf board approve-plan T-1", HUMAN),
            (
                "expect -c 'spawn aaf proof record --image x.png --device emu'",
                HUMAN,
            ),
            ("expect -c 'spawn aaf skills sync'", HUMAN),
            ("expect -c 'spawn aaf board rework T-1'", HUMAN),
            ("expect -c 'spawn aaf board import-issues'", HUMAN),
            ("expect -c 'spawn aaf board sync-github'", HUMAN),
            ("expect -c 'spawn aaf board split T-1'", HUMAN),
            ("expect -c 'spawn aaf board dep T-2 --after T-1'", HUMAN),
            ("expect -c 'spawn aaf board approve-plan T-1'", HUMAN),
            ("expect -c 'spawn /usr/local/bin/aaf run'", HUMAN),
            ("expect -c 'spawn /usr/local/bin/aaf init'", HUMAN),
            ("expect -c 'spawn /usr/local/bin/aaf gc --apply'", HUMAN),
        ],
    );
}

#[test]
fn guards_v3_human_subcommands_via_cargo_run_with_and_without_dashes() {
    run_corpus(
        &Fixture::new(),
        &[
            // With `--`
            ("cargo run -- approve", HUMAN),
            ("cargo run -- run", HUMAN),
            ("cargo run -- init", HUMAN),
            ("cargo run -- gc", HUMAN),
            ("cargo run -p aaf-cli -- gc --apply", HUMAN),
            (
                "cargo run -- proof record --image x.png --device emu",
                HUMAN,
            ),
            ("cargo run -- skills sync", HUMAN),
            ("cargo run -- board unblock T-1", HUMAN),
            ("cargo run -- board unfreeze", HUMAN),
            ("cargo run -- board rework T-1", HUMAN),
            ("cargo run -- board import-issues", HUMAN),
            ("cargo run -- board sync-github", HUMAN),
            ("cargo run -- board split T-1", HUMAN),
            ("cargo run -- board dep T-2 --after T-1", HUMAN),
            ("cargo run -- board approve-plan T-1", HUMAN),
            ("cargo run -p aaf-cli -- proof record --image x.png", HUMAN),
            // Without `--`
            ("cargo run approve", HUMAN),
            ("cargo run run", HUMAN),
            ("cargo run init", HUMAN),
            ("cargo run gc", HUMAN),
            ("cargo run -p aaf-cli gc --apply", HUMAN),
            ("cargo run proof record --image x.png --device emu", HUMAN),
            ("cargo run -p aaf-cli proof record --image x.png", HUMAN),
            ("cargo run skills sync", HUMAN),
            ("cargo run -p aaf-cli skills sync --all", HUMAN),
            ("cargo run board unblock T-1", HUMAN),
            ("cargo run board unfreeze", HUMAN),
            ("cargo run board rework T-1", HUMAN),
            ("cargo run board import-issues", HUMAN),
            ("cargo run board sync-github", HUMAN),
            ("cargo run board split T-1", HUMAN),
            ("cargo run board dep T-2 --after T-1", HUMAN),
            ("cargo run board approve-plan T-1", HUMAN),
            ("cargo r gc", HUMAN),
            ("cargo r proof record", HUMAN),
        ],
    );
}

#[test]
fn allows_non_human_commands_and_avoids_short_word_false_positives() {
    run_corpus(
        &Fixture::new(),
        &[
            ("cargo run -- --help", ALLOW),
            ("cargo run -p other-app -- proof record", ALLOW),
            ("cargo run -p other-app init", ALLOW),
            ("cargo run -p other-app gc", ALLOW),
            ("cargo run -p other-app -- board dep T-2 --after T-1", ALLOW),
            ("cargo run -p other-app board approve-plan T-1", ALLOW),
            ("cargo run --bin other-tool run", ALLOW),
            ("cargo run proof verify", ALLOW),
            ("cargo run skills list", ALLOW),
            ("cargo run board list", ALLOW),
            ("git gc", ALLOW),
            ("aaf status", ALLOW),
            ("aaf gate", ALLOW),
            ("aaf doctor", ALLOW),
            ("aaf board list", ALLOW),
            ("aaf board show T-1", ALLOW),
            (
                "aaf board add --title 'rework split gc dep approve-plan' --accept 'sync ok'",
                ALLOW,
            ),
            ("aaf proof verify", ALLOW),
            ("aaf proof show", ALLOW),
            ("aaf skills list", ALLOW),
            ("aaf skills list --profile rust", ALLOW),
            ("echo \"split record sync gc dep approve-plan\"", ALLOW),
            ("echo \"run init rework\"", ALLOW),
            (
                "script -q /dev/null echo \"split record sync gc dep approve-plan\"",
                ALLOW,
            ),
            ("script -q /dev/null aaf proof verify", ALLOW),
            ("script -q /dev/null aaf skills list", ALLOW),
            ("script -q /dev/null aaf board list", ALLOW),
            (
                "expect -c 'spawn echo aaf status && echo split record sync'",
                ALLOW,
            ),
            ("aaf board list 'oops", ALLOW),
            ("aaf proof verify 'oops", ALLOW),
            ("aaf skills list 'oops", ALLOW),
        ],
    );
}
