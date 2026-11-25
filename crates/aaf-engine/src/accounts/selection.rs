//! Which account a repository is bound to (`accounts.selection`, PLAN C.8).
//!
//! Nothing here leases anything: it only answers "which account does this
//! repository belong to", from the repository's own settings file and from
//! the runs the store already has. No table records a binding, so there is
//! nothing to migrate and nothing that can disagree with the run history.

use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind, default_account_name};
use aaf_domain::ports::PortResult;
use serde::{Deserialize, Serialize};

/// The file profile switchers keep a repository's account in, as
/// `{"account": "<name>"}` next to whatever else the file holds.
pub const SETTINGS_LOCAL_REL: &str = ".claude/settings.local.json";

/// A settings file larger than this is not one a human wrote.
const SETTINGS_MAX_BYTES: u64 = 1024 * 1024;

/// What the run history says about accounts. Cross-audit runs are left out
/// by the implementations: the auditor's account is not the one a
/// repository or task "runs on".
pub use aaf_domain::engine_contracts::RunHistory;

/// Where a binding comes from, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingSource {
    /// `account` of the repository's `.claude/settings.local.json`.
    Settings,
    /// The account the repository last ran on.
    History,
    /// A repository without history: the account its first job was given
    /// in this process. Its run turns it into `History`.
    FirstUse,
}

impl BindingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            BindingSource::Settings => "settings",
            BindingSource::History => "history",
            BindingSource::FirstUse => "first_use",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub account: AccountProfile,
    pub source: BindingSource,
}

/// The `account` value of the repository's settings file. `None` when the
/// file is missing, not JSON, too large or names nothing: the binding then
/// comes from the history, it is never guessed from a broken file.
pub fn settings_account(repo_root: &Path) -> Option<String> {
    let path = repo_root.join(SETTINGS_LOCAL_REL);
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > SETTINGS_MAX_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    let name = value.get("account")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The account `wanted` names: its id, else the one account whose display
/// name (an `accounts.aliases` entry, or the default derived from the config
/// dir) or default name equals it. A name two accounts share names neither.
pub fn match_account<'a>(
    wanted: &str,
    accounts: &'a [AccountProfile],
) -> Option<&'a AccountProfile> {
    if let Some(by_id) = accounts.iter().find(|a| a.id == wanted) {
        return Some(by_id);
    }
    let mut named = accounts
        .iter()
        .filter(|a| a.name == wanted || default_account_name(&a.config_dir) == wanted);
    match (named.next(), named.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// The account `repo_root` is bound to among `accounts` (the usable accounts
/// of `provider`): the settings file's account when it names one of them,
/// else the account the repository last ran on. `None` for a repository
/// that has neither.
pub fn resolve(
    repo_root: &Path,
    provider: ProviderKind,
    accounts: &[AccountProfile],
    history: &dyn RunHistory,
) -> PortResult<Option<Binding>> {
    let named = settings_account(repo_root);
    if let Some(account) = named.as_deref().and_then(|n| match_account(n, accounts)) {
        return Ok(Some(Binding {
            account: account.clone(),
            source: BindingSource::Settings,
        }));
    }
    let last = history.last_account_of_repo(&repo_root.to_string_lossy(), provider)?;
    Ok(last
        .and_then(|id| accounts.iter().find(|a| a.id == id))
        .map(|account| Binding {
            account: account.clone(),
            source: BindingSource::History,
        }))
}
