#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use aaf_domain::account::{AccountProfile, AccountState, AccountStatus, ProviderKind};
use aaf_domain::ports::{AccountStore, Clock, LimitHit, PortError, PortResult, RunStore};
use aaf_domain::run::{RunFinish, RunRecord};
use aaf_engine::accounts::{AccountsError, Lease, Router};

const T0: i64 = 1_800_000_000;
const HOUR: i64 = 3600;

#[derive(Default)]
struct FakeAccounts(Mutex<BTreeMap<String, AccountState>>);

impl AccountStore for FakeAccounts {
    fn upsert_account(&self, profile: &AccountProfile) -> PortResult<()> {
        let mut map = self.0.lock().unwrap();
        let cooldown_until = map.get(&profile.id).and_then(|s| s.cooldown_until);
        map.insert(
            profile.id.clone(),
            AccountState {
                profile: profile.clone(),
                cooldown_until,
            },
        );
        Ok(())
    }

    fn list_accounts(&self) -> PortResult<Vec<AccountState>> {
        Ok(self.0.lock().unwrap().values().cloned().collect())
    }

    fn set_cooldown(&self, account_id: &str, until: Option<i64>) -> PortResult<()> {
        match self.0.lock().unwrap().get_mut(account_id) {
            Some(state) => {
                state.cooldown_until = until;
                Ok(())
            }
            None => Err(PortError::NotFound(account_id.to_string())),
        }
    }
}

/// Runs as `(account_id, started_at, tokens)`.
#[derive(Default)]
struct FakeRuns(Mutex<Vec<(String, i64, u64)>>);

impl FakeRuns {
    fn add(&self, account_id: &str, started_at: i64, tokens: u64) {
        self.0
            .lock()
            .unwrap()
            .push((account_id.to_string(), started_at, tokens));
    }
}

impl RunStore for FakeRuns {
    fn insert_run(&self, _run: &RunRecord) -> PortResult<()> {
        Err(PortError::Unsupported("insert_run".to_string()))
    }
    fn finish_run(&self, _id: &str, _finish: &RunFinish) -> PortResult<()> {
        Err(PortError::Unsupported("finish_run".to_string()))
    }
    fn runs_for_task(&self, _task_id: &str) -> PortResult<Vec<RunRecord>> {
        Err(PortError::Unsupported("runs_for_task".to_string()))
    }
    fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64> {
        let runs = self.0.lock().unwrap();
        Ok(runs
            .iter()
            .filter(|(id, started, _)| id == account_id && *started >= since)
            .map(|(_, _, tokens)| tokens)
            .sum())
    }
}

struct FakeClock(AtomicI64);

impl FakeClock {
    fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct World {
    accounts: FakeAccounts,
    runs: FakeRuns,
    clock: FakeClock,
    tmp: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        World {
            accounts: FakeAccounts::default(),
            runs: FakeRuns::default(),
            clock: FakeClock(AtomicI64::new(T0)),
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn router(&self, max_concurrent: u32) -> Router<'_> {
        let router = Router::new(
            &self.accounts,
            &self.runs,
            &self.clock,
            self.tmp.path().join("leases"),
            max_concurrent,
        );
        let profiles = ["a", "b", "c"].map(|name| self.profile(name));
        for profile in &profiles {
            std::fs::create_dir_all(&profile.config_dir).unwrap();
        }
        router.sync(&profiles).unwrap();
        router
    }

    /// The config dir is real: the router only hands out accounts whose
    /// directory still exists.
    fn profile(&self, name: &str) -> AccountProfile {
        let config_dir = self.tmp.path().join(format!("home/.claude-{name}"));
        AccountProfile {
            id: format!("claude_code-{name}"),
            provider: ProviderKind::ClaudeCode,
            config_dir: config_dir.to_string_lossy().into_owned(),
            name: format!("Account {name}"),
        }
    }
}

fn limit(reset_at: Option<i64>) -> LimitHit {
    LimitHit {
        reset_at,
        raw: "usage limit reached".to_string(),
    }
}

fn next_id(router: &Router) -> Option<String> {
    router.next().unwrap().map(|(profile, _lease)| profile.id)
}

#[test]
fn prefers_the_account_with_fewest_tokens_in_the_last_five_hours() {
    let world = World::new();
    let router = world.router(1);
    world.runs.add("claude_code-a", T0 - HOUR, 500);
    world.runs.add("claude_code-b", T0 - HOUR, 100);
    world.runs.add("claude_code-c", T0 - HOUR, 300);
    // Exactly five hours ago still counts; one second older does not.
    world.runs.add("claude_code-b", T0 - 5 * HOUR, 250);
    world.runs.add("claude_code-c", T0 - 5 * HOUR - 1, 9_000);

    // a = 500, b = 350, c = 300.
    let (first, _first_lease) = router.next().unwrap().unwrap();
    assert_eq!(first, world.profile("c"));
    // c is leased now, so the next pick is the runner-up, then the last.
    let (second, _second_lease) = router.next().unwrap().unwrap();
    assert_eq!(second.id, "claude_code-b");
    let (third, _third_lease) = router.next().unwrap().unwrap();
    assert_eq!(third.id, "claude_code-a");
}

#[test]
fn ties_break_by_id_and_all_leased_returns_none() {
    let world = World::new();
    let router = world.router(1);

    let held: Vec<_> = (0..3).map(|_| router.next().unwrap().unwrap()).collect();
    let ids: Vec<_> = held.iter().map(|(p, _)| p.id.as_str()).collect();
    assert_eq!(ids, ["claude_code-a", "claude_code-b", "claude_code-c"]);
    assert!(router.next().unwrap().is_none());

    drop(held);
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));
}

#[test]
fn fails_over_after_a_limit_hit_and_returns_exactly_at_reset() {
    let world = World::new();
    let router = world.router(1);
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));

    let reset = T0 + 2 * HOUR;
    router
        .report_limit("claude_code-a", &limit(Some(reset)))
        .unwrap();
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-b"));

    world.clock.set(reset - 1);
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-b"));
    world.clock.set(reset);
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));
}

#[test]
fn limit_without_reset_time_cools_down_for_five_hours() {
    let world = World::new();
    let router = world.router(1);
    for id in ["claude_code-a", "claude_code-b", "claude_code-c"] {
        router.report_limit(id, &limit(None)).unwrap();
    }
    let until = T0 + 5 * HOUR;
    assert!(
        world
            .accounts
            .list_accounts()
            .unwrap()
            .iter()
            .all(|s| s.cooldown_until == Some(until))
    );

    world.clock.set(until - 1);
    assert_eq!(next_id(&router), None);
    world.clock.set(until);
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));
}

#[test]
fn sync_keeps_cooldown_and_unknown_account_is_an_error() {
    let world = World::new();
    let router = world.router(1);
    router.report_limit("claude_code-a", &limit(None)).unwrap();
    router.sync(&[world.profile("a")]).unwrap();
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-b"));

    assert!(matches!(
        router.report_limit("claude_code-zzz", &limit(None)),
        Err(AccountsError::Store(PortError::NotFound(_)))
    ));
}

#[test]
fn an_account_whose_config_dir_is_gone_is_never_handed_out() {
    let world = World::new();
    let router = world.router(1);
    // The store still holds the row of a, which would win the tie by id.
    let stale = world.profile("a").config_dir;
    std::fs::remove_dir(&stale).unwrap();
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-b"));

    for name in ["b", "c"] {
        std::fs::remove_dir(world.profile(name).config_dir).unwrap();
    }
    assert_eq!(next_id(&router), None);

    // The row is skipped, not deleted: the account returns with its dir.
    std::fs::create_dir(&stale).unwrap();
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));
}

#[test]
fn max_concurrent_allows_that_many_leases_per_account() {
    let world = World::new();
    let router = world.router(2);
    world.runs.add("claude_code-b", T0, 10);
    world.runs.add("claude_code-c", T0, 20);

    let held: Vec<_> = (0..6).map(|_| router.next().unwrap().unwrap()).collect();
    let ids: Vec<_> = held.iter().map(|(p, _)| &p.id[12..]).collect();
    assert_eq!(ids, ["a", "a", "b", "b", "c", "c"]);
    assert!(router.next().unwrap().is_none());
}

#[test]
fn status_reports_state_and_usage_and_serialises_flat() {
    let world = World::new();
    let router = world.router(1);
    world.runs.add("claude_code-a", T0 - HOUR, 40);
    world.runs.add("claude_code-a", T0 - 6 * HOUR, 60);
    world.runs.add("claude_code-a", T0 - 8 * 24 * HOUR, 1_000);
    router
        .report_limit("claude_code-b", &limit(Some(T0 + 60)))
        .unwrap();
    let _held = Lease::try_acquire(&world.tmp.path().join("leases"), "claude_code-c")
        .unwrap()
        .unwrap();

    let views = router.status(T0).unwrap();
    assert_eq!(views.len(), 3);
    assert_eq!(views[0].status, AccountStatus::Available);
    assert_eq!((views[0].tokens_5h, views[0].tokens_7d), (40, 100));
    assert_eq!(views[1].status, AccountStatus::Cooldown { until: T0 + 60 });
    assert_eq!(views[2].status, AccountStatus::Leased);

    // Probing must not leave a lease behind, and cooldown ends at `until`.
    assert_eq!(next_id(&router).as_deref(), Some("claude_code-a"));
    assert_eq!(
        router.status(T0 + 60).unwrap()[1].status,
        AccountStatus::Available
    );

    let json = serde_json::to_value(&views).unwrap();
    assert_eq!(
        json[1],
        serde_json::json!({
            "id": "claude_code-b",
            "provider": "claude_code",
            "config_dir": world.profile("b").config_dir,
            "name": "Account b",
            "status": "cooldown",
            "until": T0 + 60,
            "tokens_5h": 0,
            "tokens_7d": 0,
        })
    );
    assert_eq!(json[2]["status"], "leased");
    let back: Vec<aaf_engine::accounts::AccountView> = serde_json::from_value(json).unwrap();
    assert_eq!(back, views);
}
