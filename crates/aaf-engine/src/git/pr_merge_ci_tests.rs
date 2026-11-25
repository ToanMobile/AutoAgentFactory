//! `check_ci`: an unattended merge needs at least one registered check.

use std::path::Path;
use std::sync::Mutex;

use super::GitError;
use super::gh_client::{GhClient, MergeStrategy, PrCheck, PrInfo};
use super::pr_merge_support::check_ci;

struct Checks(Mutex<Vec<Vec<PrCheck>>>);

impl GhClient for Checks {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        unreachable!()
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        let mut seq = self.0.lock().unwrap();
        Ok(if seq.len() > 1 {
            seq.remove(0)
        } else {
            seq[0].clone()
        })
    }
    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        unreachable!()
    }
}

fn check(name: &str, bucket: &str) -> PrCheck {
    PrCheck {
        name: name.into(),
        state: bucket.to_uppercase(),
        bucket: bucket.into(),
    }
}

#[test]
fn no_registered_check_is_pending_when_checks_are_required() {
    let gh = Checks(Mutex::new(vec![vec![]]));
    let (blocked, _) = check_ci(&gh, Path::new("/x"), 1, 0, true).unwrap();
    assert_eq!(blocked.unwrap().0, "ci_pending");
}

#[test]
fn no_registered_check_passes_only_when_not_required() {
    let gh = Checks(Mutex::new(vec![vec![]]));
    let (blocked, _) = check_ci(&gh, Path::new("/x"), 1, 0, false).unwrap();
    assert!(blocked.is_none());
}

#[test]
fn checks_appearing_then_passing_within_the_wait_clear_the_merge() {
    let gh = Checks(Mutex::new(vec![
        vec![],
        vec![check("ci/build", "pending")],
        vec![check("ci/build", "pass")],
    ]));
    let (blocked, waited) = check_ci(&gh, Path::new("/x"), 1, 30, true).unwrap();
    assert!(blocked.is_none(), "{blocked:?}");
    assert!(waited);
}

#[test]
fn a_failed_check_blocks_even_with_others_passing() {
    let gh = Checks(Mutex::new(vec![vec![
        check("a", "pass"),
        check("b", "fail"),
    ]]));
    let (blocked, _) = check_ci(&gh, Path::new("/x"), 1, 0, true).unwrap();
    assert_eq!(blocked.unwrap().0, "ci_failed");
}
