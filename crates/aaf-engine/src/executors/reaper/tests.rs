use std::cell::Cell;

use aaf_domain::ports::{PortError, PortResult};

use super::{PS_READS, process_table, reap_with};

/// A `ps` that gives no answer for its first `misses` calls (as one
/// starved past its timeout under load does), then prints `listing`.
/// Counts its calls.
fn ps_answering_after<'a>(
    misses: usize,
    listing: &'static str,
    calls: &'a Cell<usize>,
) -> impl Fn(&[&str]) -> PortResult<(Option<i32>, String)> + 'a {
    move |_args: &[&str]| {
        calls.set(calls.get() + 1);
        if calls.get() <= misses {
            Err(PortError::Io("/bin/ps: timed out".to_string()))
        } else {
            Ok((Some(0), listing.to_string()))
        }
    }
}

const LISTING: &str = "    1     0 /sbin/launchd\n";
const TABLE: &str = "    1     0 Mon Nov 22 08:00:00 2026\n";

/// One unanswered `ps` used to end a run's cleanup with an error before
/// anything was looked at; the next call usually answers.
#[test]
fn a_ps_that_answers_on_a_later_try_does_not_end_the_cleanup() {
    let calls = Cell::new(0);
    let ps = ps_answering_after(PS_READS - 1, LISTING, &calls);
    assert_eq!(reap_with("run-1", None, &ps).unwrap(), Vec::<u32>::new());
    assert_eq!(calls.get(), PS_READS);

    let calls = Cell::new(0);
    let ps = ps_answering_after(PS_READS - 1, TABLE, &calls);
    let table = process_table(&ps, PS_READS).unwrap();
    assert_eq!(table.get(&1).map(|row| row.ppid), Some(0));
    assert_eq!(calls.get(), PS_READS);
}

/// The bound: a `ps` that never answers is still an error, after
/// [`PS_READS`] calls and no more.
#[test]
fn a_ps_that_never_answers_is_an_error_after_the_bounded_tries() {
    let calls = Cell::new(0);
    let ps = ps_answering_after(usize::MAX, LISTING, &calls);
    let error = reap_with("run-1", None, &ps).unwrap_err().to_string();
    assert!(error.contains("timed out"), "{error}");
    assert_eq!(calls.get(), PS_READS);

    let calls = Cell::new(0);
    let ps = ps_answering_after(usize::MAX, TABLE, &calls);
    assert!(process_table(&ps, PS_READS).is_err());
    assert_eq!(calls.get(), PS_READS);
}

/// An answer comes back as is: a `ps` that answers at once is asked once.
#[test]
fn a_ps_that_answers_at_once_is_asked_once() {
    let calls = Cell::new(0);
    let ps = ps_answering_after(0, LISTING, &calls);
    assert_eq!(reap_with("run-1", None, &ps).unwrap(), Vec::<u32>::new());
    assert_eq!(calls.get(), 1);
}
