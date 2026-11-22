#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-2 audit: an agent must not run the human's side of the factory
//! (`aaf approve`, `aaf board unblock`, `aaf board unfreeze`) for itself.

mod common;

use common::{Fixture, run_corpus};

const HUMAN: Option<&str> = Some("human_action_guard");
const ALLOW: Option<&str> = None;

#[test]
fn an_agent_cannot_run_the_human_actions() {
    run_corpus(
        &Fixture::new(),
        &[
            ("aaf approve", HUMAN),
            ("aaf approve --task T-1", HUMAN),
            ("aaf --json approve", HUMAN),
            ("aaf board unblock T-3", HUMAN),
            ("aaf board unfreeze", HUMAN),
            ("aaf --json board unfreeze", HUMAN),
            ("aaf board --json unblock T-3", HUMAN),
            // Any path to an `aaf`.
            ("./target/debug/aaf approve", HUMAN),
            ("/usr/local/bin/aaf board unfreeze", HUMAN),
            ("~/.cargo/bin/aaf board unblock T-3", HUMAN),
            // Through wrappers and nested shells.
            ("env aaf approve", HUMAN),
            ("env FOO=1 aaf approve", HUMAN),
            ("command aaf board unblock T-3", HUMAN),
            ("nohup aaf approve &", HUMAN),
            ("sudo -u owner aaf approve", HUMAN),
            ("sh -c 'aaf approve'", HUMAN),
            ("bash -lc \"aaf board unfreeze\"", HUMAN),
            ("eval 'aaf approve'", HUMAN),
            ("echo $(aaf approve)", HUMAN),
            ("echo `aaf board unfreeze`", HUMAN),
            ("aaf gate && aaf approve", HUMAN),
            ("aaf gate; aaf board unblock T-3", HUMAN),
            ("echo T-3 | xargs aaf board unblock", HUMAN),
            ("find . -name x -exec aaf approve \\;", HUMAN),
            ("cargo run -p aaf-cli -- approve", HUMAN),
            ("cargo run -q --bin aaf -- board unfreeze", HUMAN),
            // A command word built at run time could be aaf.
            ("$AAF approve", HUMAN),
            ("$(which aaf) board unfreeze", HUMAN),
            ("\"$BIN\"/aaf approve", HUMAN),
            // A subcommand built at run time could be a human one.
            ("aaf $SUB", HUMAN),
            ("aaf \"$@\"", HUMAN),
            ("aaf $(echo approve)", HUMAN),
            ("aaf board $ACTION T-3", HUMAN),
            ("aaf appro?e", HUMAN),
            ("git -c 'alias.x=!aaf approve' x", Some("git_guard")),
            ("aaf approve 'oops", Some("unparsable_command")),
            ("aaf board unfreeze \"oops", Some("unparsable_command")),
            // allowed
            ("aaf gate", ALLOW),
            ("aaf gate --release", ALLOW),
            ("aaf --json gate", ALLOW),
            ("aaf doctor", ALLOW),
            ("aaf accounts list", ALLOW),
            ("aaf board list", ALLOW),
            ("aaf board list --status blocked", ALLOW),
            ("aaf board show T-3", ALLOW),
            ("aaf --json board show T-3", ALLOW),
            (
                "aaf board add --title 'approve the plan' --accept 'unblock works'",
                ALLOW,
            ),
            ("aaf board add --title unblock --accept unfreeze", ALLOW),
            ("aaf board show unblock", ALLOW),
            ("aaf run", HUMAN),
            ("aaf init", HUMAN),
            ("aaf --help", ALLOW),
            ("./target/debug/aaf gate", ALLOW),
            ("$AAF gate", ALLOW),
            ("cargo run -p aaf-cli -- gate", ALLOW),
            ("cargo run -p aaf-cli -- board list", ALLOW),
            ("cargo test -p aaf-cli approve", ALLOW),
            ("echo aaf approve", ALLOW),
            ("grep -rn 'aaf approve' docs", ALLOW),
            ("git commit -m 'aaf approve flow'", ALLOW),
            ("cat approve.md", ALLOW),
            ("./approve.sh", ALLOW),
            ("aaf doctor 'oops", ALLOW),
        ],
    );
}
