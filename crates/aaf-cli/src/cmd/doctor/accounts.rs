//! The `accounts` check of `aaf doctor`: what discovery finds, the selection
//! mode (`accounts.selection`) and the account this repository is bound to.

use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::AccountSelection;
use aaf_domain::ports::PortResult;
use aaf_engine::accounts::selection::{self, Binding};
use aaf_engine::accounts::{DiscoveryEnv, RunHistory, binary_on_path_in, discover};
use aaf_engine::config::EffectiveConfig;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::provider_for;
use serde_json::{Value, json};

use crate::cmd::board::store::DB_FILE;
use crate::ctx::Ctx;

/// A machine where `aaf` never ran has no store, and the doctor creates
/// nothing: no history then.
struct NoHistory;

impl RunHistory for NoHistory {
    fn last_account_of_repo(&self, _: &str, _: ProviderKind) -> PortResult<Option<String>> {
        Ok(None)
    }

    fn last_account_of_task(&self, _: &str, _: ProviderKind) -> PortResult<Option<String>> {
        Ok(None)
    }
}

/// The binding `aaf run` would find for `repo`, from the settings file and
/// the run history. `None` also when the store cannot be read: this check
/// is advisory.
fn binding(
    ctx: &Ctx,
    cfg: &EffectiveConfig,
    repo: &Path,
    found: &[AccountProfile],
) -> Option<Binding> {
    let provider = provider_for(&cfg.repo.run.executor)?;
    let accounts: Vec<AccountProfile> = found
        .iter()
        .filter(|a| a.provider == provider)
        .cloned()
        .collect();
    // As `tasks.repo_root` holds it.
    let repo = fs::canonicalize(repo).ok()?;
    let db_path = ctx.data_dir.join(DB_FILE);
    let db = db_path.is_file().then(|| Db::open(&db_path).ok()).flatten();
    let history: &dyn RunHistory = match &db {
        Some(db) => db,
        None => &NoHistory,
    };
    selection::resolve(&repo, provider, &accounts, history)
        .ok()
        .flatten()
}

/// The check's result and the `accounts` object of the JSON output.
pub(super) fn report(
    ctx: &Ctx,
    cfg: &EffectiveConfig,
    repo: Option<&Path>,
) -> (Result<String, String>, Value) {
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let found = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    let mode = cfg.global.accounts.selection;
    // `least_used` binds nothing; outside a repository there is nothing to
    // bind.
    let bound = repo
        .filter(|_| mode != AccountSelection::LeastUsed)
        .and_then(|repo| binding(ctx, cfg, repo, &found));
    let fields = json!({
        "selection": mode,
        "binding": bound,
        "settings_account": repo.and_then(selection::settings_account),
    });
    if found.is_empty() {
        let none = "0 accounts discovered; check [accounts] discovery_globs";
        return (Err(none.to_string()), fields);
    }
    let names: Vec<&str> = found.iter().map(|account| account.name.as_str()).collect();
    let mut detail = format!(
        "{} discovered: {}; selection={}",
        found.len(),
        names.join(", "),
        mode.as_str()
    );
    match (&bound, repo) {
        (Some(bound), _) => detail.push_str(&format!(
            "; this repository is bound to {} ({})",
            bound.account.name,
            bound.source.as_str()
        )),
        (None, Some(_)) if mode == AccountSelection::Pinned => {
            detail.push_str("; no binding yet, the first run binds this repository");
        }
        _ => {}
    }
    (Ok(detail), fields)
}
