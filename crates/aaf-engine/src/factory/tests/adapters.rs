//! The production adapters on the real things they wrap: `RouterPool` on a
//! real `Db`, real lease files and real config dirs; `RealGate` on a real
//! repository and the real machine throttle.

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateStatus;
use aaf_domain::ports::{AccountStore, LimitHit};
use tempfile::TempDir;

use super::fakes::FakeClock;
use super::git_real::Fixture;
use crate::db::Db;
use crate::factory::adapters::RouterPool;
use crate::factory::ports::{AccountPool, GateRunner};
use crate::throttle::slots::slot_path;

/// Long enough that only a hung gate reaches it.
const HANG: Duration = Duration::from_secs(30);

struct Accounts {
    tmp: TempDir,
    db: Db,
}

impl Accounts {
    fn new() -> Accounts {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        Accounts { tmp, db }
    }

    /// Registers an account; its config dir exists only when `on_disk`.
    fn add(&self, id: &str, provider: ProviderKind, on_disk: bool) {
        let dir = self.tmp.path().join(format!("config-{id}"));
        if on_disk {
            fs::create_dir_all(&dir).unwrap();
        }
        self.db
            .upsert_account(&AccountProfile {
                id: id.to_string(),
                provider,
                config_dir: dir.to_string_lossy().into_owned(),
                name: id.to_string(),
            })
            .unwrap();
    }

    fn pool(&self, provider: Option<ProviderKind>) -> RouterPool {
        RouterPool {
            accounts: Arc::new(self.db.clone()),
            runs: Arc::new(self.db.clone()),
            clock: Arc::new(FakeClock(AtomicI64::new(1_000))),
            leases_dir: self.tmp.path().join("leases"),
            max_concurrent_per_account: 1,
            provider,
        }
    }
}

#[test]
fn the_router_pool_hands_out_only_accounts_of_the_run_s_provider_that_exist_on_disk() {
    let accounts = Accounts::new();
    accounts.add("a-codex", ProviderKind::Codex, true);
    accounts.add("b-gone", ProviderKind::ClaudeCode, false);
    accounts.add("c-claude", ProviderKind::ClaudeCode, true);
    let pool = accounts.pool(Some(ProviderKind::ClaudeCode));

    let grant = pool.acquire().unwrap().unwrap();

    // Not the Codex account (first by id) and not the one whose config dir
    // was removed.
    assert_eq!(grant.account_id(), Some("c-claude"));
    // One run per account: the lease is a real file, held until the grant
    // is dropped.
    assert!(pool.acquire().unwrap().is_none(), "leased twice");
    drop(grant);
    let again = pool.acquire().unwrap().unwrap();
    assert_eq!(again.account_id(), Some("c-claude"));

    // The sandbox closes the dirs of every provider, not only the run's.
    let mut known: Vec<String> = pool
        .profiles()
        .unwrap()
        .into_iter()
        .map(|profile| profile.id)
        .collect();
    known.sort();
    assert_eq!(known, ["a-codex", "b-gone", "c-claude"]);
}

#[test]
fn the_router_pool_runs_on_the_default_login_only_when_the_provider_has_no_account_at_all() {
    let accounts = Accounts::new();
    accounts.add("a-codex", ProviderKind::Codex, true);

    // A custom executor has no provider; Claude has no account here.
    for provider in [None, Some(ProviderKind::ClaudeCode)] {
        let grant = accounts.pool(provider).acquire().unwrap().unwrap();
        assert_eq!(grant.account_id(), None, "{provider:?}");
    }

    // An account that exists but cannot be used right now is waited for:
    // the run must not fall back to the default login.
    let codex = accounts.pool(Some(ProviderKind::Codex));
    let hit = LimitHit {
        reset_at: Some(2_000),
        raw: "usage limit reached".to_string(),
    };
    codex.report_limit("a-codex", &hit).unwrap();
    assert!(
        codex.acquire().unwrap().is_none(),
        "cooling down until 2000"
    );
    let cooled = accounts.db.list_accounts().unwrap();
    assert_eq!(cooled[0].cooldown_until, Some(2_000));
}

#[test]
fn the_real_gate_reports_the_wait_for_a_heavy_slot_with_the_tree_it_gates() {
    let fx = Fixture::new();
    let tree = fx.task("t1", "other.txt", "x\n");
    let mut gate = fx.gate("true");
    gate.machine = MachineConfig {
        max_heavy_parallel: 1,
        ..gate.machine
    };
    // Another gate run on this machine holds the only heavy slot.
    let slot = slot_path(&gate.data_dir, 0);
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    let other = File::create(&slot).unwrap();
    other.try_lock().unwrap();
    let (tx, reports) = mpsc::channel();
    let tx = Mutex::new(tx);
    gate.progress = Box::new(move |tree: &Path, event| {
        let report = (tree.to_path_buf(), format!("{event:?}"));
        // The receiver is gone only when the test already failed.
        let _ = tx.lock().unwrap().send(report);
    });

    let (first, receipt) = std::thread::scope(|scope| {
        let run = scope.spawn(|| gate.run(&tree.path));
        // The gate cannot start its command before the slot is free, so
        // its first report is in before the slot is given up.
        let first = reports.recv_timeout(HANG);
        other.unlock().unwrap();
        (first, run.join().unwrap())
    });

    let (reported_tree, event) = first.expect("no report while the gate waited for a slot");
    assert_eq!(reported_tree, tree.path);
    assert!(
        event.contains("WaitingForSlot") && event.contains("test_scoped"),
        "{event}"
    );
    assert_eq!(receipt.unwrap().status, GateStatus::Clear);
}
