//! The token a slot holder writes beside its lock file, where no agent
//! reads it, and who it lets through without waiting.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;

use super::super::paths::token_path;
use super::super::slots::{
    Admission, Busy, holds_token, is_held, read_reservation, slot_path, try_acquire,
};
use super::super::token::{SlotToken, inherited_slot, slot_env_value};

const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };
const FORGED: &str = "deadbeefdeadbeefdeadbeefdeadbeef";

fn plenty() -> Option<u64> {
    Some(u64::MAX)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn token(text: &str) -> SlotToken {
    SlotToken::parse(text).unwrap()
}

#[test]
fn token_is_128_random_bits_in_hex_and_nothing_else_parses_as_one() {
    let (a, b) = (
        SlotToken::generate().unwrap(),
        SlotToken::generate().unwrap(),
    );
    assert_eq!(a.as_str().len(), 32);
    assert!(SlotToken::parse(a.as_str()).is_some_and(|parsed| parsed.matches(&a)));
    assert!(!a.matches(&b), "two draws gave the same token");
    assert!(a.matches(&a.clone()));
    // One digit off.
    let mut near = a.as_str().to_string();
    let last = if near.ends_with('0') { "1" } else { "0" };
    near.replace_range(31.., last);
    assert!(!a.matches(&token(&near)));

    for junk in [
        "",
        "0",
        "deadbeef",
        &FORGED[1..],
        "deadbeefdeadbeefdeadbeefdeadbeef0",
        "DEADBEEFDEADBEEFDEADBEEFDEADBEEF",
        "deadbeefdeadbeefdeadbeefdeadbeeg",
        " deadbeefdeadbeefdeadbeefdeadbee",
        "deadbeefdeadbeefdeadbeefdeadbee\n",
    ] {
        assert!(SlotToken::parse(junk).is_none(), "{junk:?}");
    }
    // The token never shows in debug output.
    assert!(!format!("{a:?}").contains(a.as_str()));
}

#[test]
fn holder_writes_its_token_into_a_closed_file_only_the_owner_reads() {
    let data = TempDir::new().unwrap();
    let path = slot_path(data.path(), 0);
    let closed = token_path(data.path(), 0);
    let guard = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();

    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&closed), 0o600);
    // Not in the directory a sandboxed agent may read.
    assert!(closed.starts_with(data.path().join("locks")));
    let text = fs::read_to_string(&closed).unwrap();
    assert_eq!(text, format!("token={}\n", guard.token().as_str()));
    // The lock file says how much the command reserves, and nothing that
    // lets a reader into the slot.
    let open = fs::read_to_string(&path).unwrap();
    assert!(open.starts_with("reserved="), "{open:?}");
    assert!(!open.contains(guard.token().as_str()) && !open.contains("token"));
    assert!(holds_token(data.path(), 0, guard.token()));
    assert!(!holds_token(data.path(), 0, &token(FORGED)));
    assert!(!holds_token(data.path(), 1, guard.token()));
    assert!(!format!("{guard:?}").contains(guard.token().as_str()));

    // Released: the token is gone from the file and opens nothing.
    let old = guard.token().clone();
    drop(guard);
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    assert_eq!(fs::read_to_string(&closed).unwrap(), "");
    assert!(!holds_token(data.path(), 0, &old));
}

#[test]
fn token_of_a_finished_run_does_not_open_the_next_holders_slot() {
    let data = TempDir::new().unwrap();
    let first = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let old = first.token().clone();
    drop(first);

    let second = try_acquire(data.path(), 1, Admission::Unmeasured, &plenty)
        .unwrap()
        .unwrap();
    assert!(!old.matches(second.token()));
    assert!(!holds_token(data.path(), 0, &old));
    assert_eq!(inherited_slot(data.path(), &slot_env_value(0, &old)), None);
    assert_eq!(
        inherited_slot(data.path(), &slot_env_value(0, second.token())),
        Some(0)
    );
}

#[test]
fn lock_file_left_by_a_killed_holder_is_emptied_and_closed_to_others_when_taken() {
    let data = TempDir::new().unwrap();
    let path = slot_path(data.path(), 0);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A killed holder cleans up nothing; an earlier version also made the
    // file readable by everybody.
    let closed = token_path(data.path(), 0);
    fs::create_dir_all(closed.parent().unwrap()).unwrap();
    fs::write(&closed, format!("token={FORGED}\n")).unwrap();
    fs::write(&path, "reserved=unknown\npgid=4242\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    // Not held: what the file says counts for nothing.
    assert!(!holds_token(data.path(), 0, &token(FORGED)));

    let guard = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(mode(&path), 0o600);
    assert!(!holds_token(data.path(), 0, &token(FORGED)));
    assert!(holds_token(data.path(), 0, guard.token()));
    // Nothing of the old content is left behind the new one.
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("4242"), "{text:?}");
    let text = fs::read_to_string(&closed).unwrap();
    assert!(!text.contains(FORGED), "{text:?}");
}

#[test]
fn slot_only_looked_at_keeps_no_token_of_an_earlier_holder() {
    let data = TempDir::new().unwrap();
    let stale = slot_path(data.path(), 1);
    fs::create_dir_all(stale.parent().unwrap()).unwrap();
    fs::write(&stale, format!("token={FORGED}\nreserved=1\n")).unwrap();

    // Takes slot 0; slot 1 is locked for a moment to see that it is free.
    let _guard = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert!(!is_held(data.path(), 1));
    assert_eq!(fs::read_to_string(&stale).unwrap(), "");
}

#[test]
fn rewriting_the_reservation_never_moves_or_drops_the_token() {
    let data = TempDir::new().unwrap();
    let path = slot_path(data.path(), 0);
    let mut guard = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let closed = token_path(data.path(), 0);
    let before = (fs::read(&path).unwrap(), fs::read(&closed).unwrap());

    guard.record_pgid(4242);
    let after = (fs::read(&path).unwrap(), fs::read(&closed).unwrap());
    // Same length, same token file: a reader in the middle of the rewrite
    // finds the token whichever version it sees.
    assert_eq!(before.0.len(), after.0.len());
    let line = format!("token={}\n", guard.token().as_str());
    assert!(before.1 == line.as_bytes() && after.1 == line.as_bytes());
    assert!(holds_token(data.path(), 0, guard.token()));
    assert_eq!(
        read_reservation(data.path(), 0).map(|r| r.pgid),
        Some(Some(4242))
    );
}

#[test]
fn every_slot_of_a_command_that_runs_alone_carries_its_token() {
    let data = TempDir::new().unwrap();
    let guard = try_acquire(data.path(), 3, Admission::Unmeasured, &plenty)
        .unwrap()
        .unwrap();
    for index in 0..3 {
        assert!(holds_token(data.path(), index, guard.token()), "{index}");
    }
}

#[test]
fn variable_opens_a_slot_only_with_the_token_of_its_present_holder() {
    let data = TempDir::new().unwrap();
    let guard = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let real = guard.token().as_str().to_string();
    assert_eq!(guard.index(), 0);

    assert_eq!(inherited_slot(data.path(), &format!("0:{real}")), Some(0));
    for value in [
        String::new(),
        "0".to_string(),
        "0:".to_string(),
        ":".to_string(),
        format!(":{real}"),
        format!("0:{FORGED}"),
        format!("0:{}", &real[..31]),
        format!("0:{real}0"),
        format!("0:{}", real.to_uppercase()),
        format!(" 0:{real}"),
        format!("0:{real} "),
        format!("+0:{real}"),
        format!("-0:{real}"),
        format!("0x0:{real}"),
        format!("0:{real}:{real}"),
        format!("0;{real}"),
        "x".to_string(),
    ] {
        assert_eq!(inherited_slot(data.path(), &value), None, "{value:?}");
    }

    // Released: the right value of a moment ago opens nothing.
    drop(guard);
    assert_eq!(inherited_slot(data.path(), &format!("0:{real}")), None);
}

#[test]
fn busy_slot_zero_is_no_way_in_for_a_variable_that_names_another_slot() {
    let data = TempDir::new().unwrap();
    let zero = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let one = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!((zero.index(), one.index()), (0, 1));

    // Slot 1 is free again; slot 0 is held by somebody else.
    let gone = one.token().clone();
    drop(one);
    for value in [
        "1".to_string(),
        format!("1:{FORGED}"),
        slot_env_value(1, &gone),
        "7".to_string(),
        format!("7:{FORGED}"),
    ] {
        assert_eq!(inherited_slot(data.path(), &value), None, "{value:?}");
    }
    // The holder of slot 0 itself is found under its own number only by
    // its own token.
    assert_eq!(
        inherited_slot(data.path(), &slot_env_value(0, zero.token())),
        Some(0)
    );
}

#[test]
fn command_moved_to_slot_zero_is_still_found_by_what_its_children_carry() {
    let data = TempDir::new().unwrap();
    let zero = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let mut one = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    // What the command in slot 1 was started with.
    let carried = slot_env_value(1, one.token());
    assert_eq!(inherited_slot(data.path(), &carried), Some(1));

    // Slot 0 is taken: no move.
    assert!(!one.promote(data.path(), &|_| None).unwrap());
    drop(zero);
    assert!(one.promote(data.path(), &|_| None).unwrap());
    assert_eq!(one.index(), 0);
    assert!(!is_held(data.path(), 1));
    assert_eq!(inherited_slot(data.path(), &carried), Some(0));

    // Slot 1 goes to the next command; the token that left it stays out.
    let next = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(next.index(), 1);
    assert!(!holds_token(data.path(), 1, one.token()));
    assert_eq!(inherited_slot(data.path(), &carried), Some(0));
}

#[test]
fn lock_held_by_somebody_who_wrote_no_token_opens_for_nobody() {
    for content in ["", "reserved=unknown\n", "token=\n", "token=deadbeef\n"] {
        let data = TempDir::new().unwrap();
        let path = slot_path(data.path(), 0);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        let held = File::open(&path).unwrap();
        held.lock().unwrap();

        for value in ["0", "0:", "0:deadbeef", &format!("0:{FORGED}")] {
            assert_eq!(
                inherited_slot(data.path(), value),
                None,
                "{content:?} {value:?}"
            );
        }
        // And the slot stays taken for a command that asks properly.
        let busy = try_acquire(data.path(), 1, SMALL, &plenty).unwrap();
        assert_eq!(busy.unwrap_err(), Busy::SlotsTaken, "{content:?}");
        held.unlock().unwrap();
    }
}
