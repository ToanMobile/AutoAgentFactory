#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `aaf board adopt` is a Tier-B human action: a chat session (no
//! `AAF_RUN_ID`) runs it, an agent inside a factory run does not. Before this
//! arm existed `board adopt` fell through to "not a guarded action", so the
//! change is a tightening. The second half pins every older arm: Tier-A is
//! refused with and without `AAF_RUN_ID`, the old Tier-B verbs follow the run
//! mode, and nothing else became guarded or free.

mod common;

use common::{Fixture, run_corpus};

const HUMAN: Option<&str> = Some("human_action_guard");
const TEXT: Option<&str> = Some("unparsable_command");
const ALLOW: Option<&str> = None;

#[test]
fn a_chat_session_may_adopt() {
    run_corpus(
        &Fixture::new().interactive(),
        &[
            ("aaf board adopt 7", ALLOW),
            ("aaf board adopt 7 --author-executor codex", ALLOW),
            ("aaf --json board adopt 7", ALLOW),
            ("aaf board adopt 7 --json", ALLOW),
            ("/usr/local/bin/aaf board adopt 7", ALLOW),
            // The text fallback is Tier-B too: free interactively.
            ("aaf board adopt 7 'oops", ALLOW),
            ("script -q /dev/null aaf board adopt 7", ALLOW),
            ("expect -c 'spawn aaf board adopt 7'", ALLOW),
        ],
    );
}

#[test]
fn an_agent_inside_a_factory_run_may_not_adopt_directly() {
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf board adopt 7", HUMAN),
            ("aaf board adopt 7 --author-executor codex", HUMAN),
            ("aaf --json board adopt 7", HUMAN),
            ("aaf board adopt 7 --json", HUMAN),
            ("/usr/local/bin/aaf board adopt 7", HUMAN),
            ("AAF_FOO=1 aaf board adopt 7", HUMAN),
            ("echo go; aaf board adopt 7", HUMAN),
            ("true && aaf board adopt 7", HUMAN),
            ("aaf board adopt $PR", HUMAN),
        ],
    );
}

#[test]
fn an_agent_inside_a_factory_run_may_not_adopt_through_a_launcher_or_cargo() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/aaf-cli\"]\n",
    );
    run_corpus(
        &fixture,
        &[
            ("script -q /dev/null aaf board adopt 7", HUMAN),
            ("expect -c 'spawn aaf board adopt 7'", HUMAN),
            ("expect -c 'spawn /usr/local/bin/aaf board adopt 7'", HUMAN),
            ("cargo run -- board adopt 7", HUMAN),
            ("cargo run board adopt 7", HUMAN),
            ("cargo run -p aaf-cli -- board adopt 7", HUMAN),
            ("cargo r board adopt 7", HUMAN),
        ],
    );
}

#[test]
fn the_text_fallback_refuses_adopt_inside_a_factory_run() {
    // Unbalanced quote: the splitter gives up and only the text list is left.
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf board adopt 7 'oops", TEXT),
            ("aaf board adopt 7 --author-executor 'oops", TEXT),
            ("aaf --json board adopt 7 \"oops", TEXT),
        ],
    );
}

#[test]
fn adopt_stays_free_when_it_is_only_a_word() {
    for fixture in [Fixture::new(), Fixture::new().interactive()] {
        run_corpus(
            &fixture,
            &[
                ("echo \"aaf board adopt 7\"", ALLOW),
                ("aaf board list adopt", ALLOW),
                ("aaf board show adopt", ALLOW),
                ("aaf status", ALLOW),
                ("aaf adopt 7", ALLOW),
                ("git log --grep adopt", ALLOW),
                ("grep -rn adopt docs", ALLOW),
                ("cargo run -p other-app -- board adopt 7", ALLOW),
                ("aaf board list 'oops", ALLOW),
            ],
        );
    }
}

#[test]
fn tier_a_is_refused_with_and_without_a_run_id() {
    for fixture in [Fixture::new(), Fixture::new().interactive()] {
        run_corpus(
            &fixture,
            &[
                ("aaf approve", HUMAN),
                ("aaf board unblock T-1", HUMAN),
                ("aaf board unfreeze", HUMAN),
                ("aaf board approve-plan T-1", HUMAN),
                ("aaf board undo T-1", HUMAN),
                ("aaf board steer T-1 'keep it small'", HUMAN),
                ("aaf cockpit", HUMAN),
                ("aaf telegram", HUMAN),
                ("aaf ui", HUMAN),
                ("aaf update", HUMAN),
                ("aaf upgrade", HUMAN),
                (
                    "aaf merge 42 --expect-head 0123456789abcdef0123456789abcdef01234567",
                    HUMAN,
                ),
                ("aaf proof record --image x.png", HUMAN),
                ("aaf sop approve x", HUMAN),
                ("aaf sop send-back x", HUMAN),
                ("aaf board $ACTION T-1", HUMAN),
                // Escalating flags on Tier-B commands.
                ("aaf run --auto", HUMAN),
                ("aaf run --auto-merge", HUMAN),
                ("aaf init --force", HUMAN),
                ("aaf gc --force", HUMAN),
                ("aaf board add --title t --accept a --repro-cmd ./x", HUMAN),
                ("aaf skills sync --force", HUMAN),
                // Tier-A under a launcher and as text.
                ("script -q /dev/null aaf board unblock T-1", HUMAN),
                ("aaf board unblock T-1 'oops", TEXT),
                ("aaf board steer T-1 'oops", TEXT),
            ],
        );
    }
}

#[test]
fn the_older_tier_b_verbs_still_follow_the_run_mode() {
    let rows: &[&str] = &[
        "aaf run",
        "aaf init",
        "aaf plan",
        "aaf gc",
        "aaf skills sync",
        "aaf sop run deploy",
        "aaf sop init",
        "aaf board add --title t --accept a",
        "aaf board ready T-1",
        "aaf board rework T-1",
        "aaf board import-issues",
        "aaf board sync-github",
        "aaf board split T-1",
        "aaf board dep T-2 --after T-1",
    ];
    let refused: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (*r, HUMAN)).collect();
    let free: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (*r, ALLOW)).collect();
    run_corpus(&Fixture::new(), &refused);
    run_corpus(&Fixture::new().interactive(), &free);
}
