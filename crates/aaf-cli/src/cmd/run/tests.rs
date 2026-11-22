use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::AccountSelection;
use aaf_engine::accounts::BindingSource;
use aaf_engine::factory::BindingView;
use aaf_engine::throttle::{Busy, Progress};

use super::progress::line;
use super::{report, sandbox_choice, waiting_line};
use crate::output::Exit;

#[test]
fn a_run_stopped_by_a_login_or_network_failure_says_so_and_exits_2() {
    let why = "t1: the agent CLI failed: its login or credentials were refused: \
               Not logged in. The run stopped";
    let summary = aaf_engine::factory::RunSummary {
        agent_runs: 1,
        requeued: vec!["t1".to_string()],
        halted: Some(why.to_string()),
        ..Default::default()
    };
    let out = report(&summary, &[], &binding(None)).unwrap();
    assert_eq!(out.status, "halted");
    assert_eq!(out.exit, Exit::Policy);
    assert!(
        out.text.ends_with(&format!("\nstopped: {why}")),
        "{}",
        out.text
    );
    assert_eq!(out.fields["halted"], serde_json::json!(why));

    // The neighbour: the same run without the stop is an ordinary result.
    let summary = aaf_engine::factory::RunSummary {
        halted: None,
        ..summary
    };
    let out = report(&summary, &[], &binding(None)).unwrap();
    assert_eq!(out.status, "ok");
    assert_eq!(out.exit, Exit::Ok);
}

fn binding(cooldown_until: Option<i64>) -> BindingView {
    BindingView {
        selection: AccountSelection::Pinned,
        account: Some(AccountProfile {
            id: "claude_code-0123456789ab".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/home/u/.claude-work".to_string(),
            name: "work".to_string(),
        }),
        source: Some(BindingSource::History),
        cooldown_until,
        settings_account: None,
        settings_account_known: false,
    }
}

#[test]
fn a_run_that_waits_for_the_repositorys_account_says_which_and_until_when() {
    assert_eq!(
        waiting_line(1, &binding(Some(1_900_000_000))).as_deref(),
        Some("waiting for account work (selection=pinned): in cooldown until 1900000000")
    );
    // Nothing waits, or the wait is not a cooldown: no line.
    assert_eq!(waiting_line(0, &binding(Some(1_900_000_000))), None);
    assert_eq!(waiting_line(1, &binding(None)), None);
}

#[test]
fn macos_sandboxes_unless_told_not_to() {
    assert_eq!(sandbox_choice(false, true), Ok(true));
    assert_eq!(sandbox_choice(true, true), Ok(false));
}

#[test]
fn without_seatbelt_running_unconfined_must_be_asked_for() {
    assert!(sandbox_choice(false, false).is_err());
    assert_eq!(sandbox_choice(true, false), Ok(false));
}

#[test]
fn a_wait_of_the_gate_is_one_line_that_names_the_tree_and_the_reason() {
    let tree = Path::new("/data/worktrees/repo-1/batch-0123456789ab");
    let slot = Progress::WaitingForSlot {
        label: "test_scoped",
        reason: Busy::SlotsTaken,
        waited_secs: 12,
    };
    assert_eq!(
        line(tree, &slot),
        "gate batch-0123456789ab: test_scoped: waiting for a heavy slot \
         (every heavy slot is taken), 12s"
    );
    let load = Progress::WaitingForLoad {
        label: "build",
        load_percent: 400,
        max_load_percent: 150,
        waited_secs: 30,
        gives_up_after_secs: 1800,
    };
    assert_eq!(
        line(Path::new("/wt/t1"), &load),
        "gate t1: build: waiting for CPU load 400% to drop to 150%, 30s \
         (runs anyway after 1800s)"
    );
}

#[test]
fn a_kill_that_gave_up_names_the_group_that_may_still_run() {
    let unkilled = Progress::Unkilled {
        label: "gradlew",
        pgid: 4242,
    };
    assert_eq!(
        crate::cmd::gate::progress_line(&unkilled),
        "gradlew: process group 4242 still had members after repeated SIGKILL; \
         they may still be running (`ps -o pid,command -g 4242`)"
    );
}
