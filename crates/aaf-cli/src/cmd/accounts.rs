//! `aaf accounts list|next|cooldown`.
//!
//! This module opens no file inside an account's config dir and prints only
//! id, name, provider, config dir path, cooldown and token counts.

use std::path::PathBuf;
use std::sync::Arc;

use aaf_domain::account::{AccountProfile, AccountStatus};
use aaf_domain::config::AccountSelection;
use aaf_domain::ports::{AccountStore, Clock};
use aaf_engine::accounts::{AccountView, DiscoveryEnv, Router, binary_on_path_in, discover};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RouterPool, provider_for};
use aaf_engine::factory::{AccountPool, Purpose, SelectingPool};
use serde_json::{Value, json};

use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

/// Same default as the router uses for a limit hit with no reset time.
const DEFAULT_COOLDOWN_SECS: i64 = 5 * 3600;

#[derive(Debug, clap::Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Show every discovered account.
    List,
    /// Show the account the next run would use.
    Next,
    /// Put an account in cooldown (5 hours unless --until), or clear it.
    Cooldown {
        /// Account id or display name.
        id: String,
        /// Unix seconds at which the cooldown ends.
        #[arg(long, conflicts_with = "clear")]
        until: Option<i64>,
        /// Remove the cooldown.
        #[arg(long)]
        clear: bool,
    },
}

/// The store plus what discovery sees right now.
struct Pool {
    db: Db,
    discovered: Vec<AccountProfile>,
    max_concurrent: u32,
    /// `run.executor` of the repository (the default outside one).
    executor: String,
    selection: AccountSelection,
    /// The repository as `tasks.repo_root` names it; the current directory
    /// outside one.
    root: PathBuf,
}

fn runtime(context: &str, err: impl std::fmt::Display) -> CmdError {
    CmdError::new(format!("{context}: {err}"))
}

fn open(ctx: &Ctx) -> Result<Pool, CmdError> {
    // Accounts are machine-wide; outside a repo only the global tier applies.
    let root = ctx.repo_root().unwrap_or_else(|_| ctx.cwd.clone());
    let cfg = ctx.load_config(&root)?;
    let lookup = |name: &str| binary_on_path_in(&ctx.path_var, name);
    let discovered = discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &lookup,
        },
    );
    aaf_engine::db::private::create_dir(&ctx.data_dir)
        .map_err(|e| runtime(&ctx.data_dir.display().to_string(), e))?;
    let db = Db::open(&ctx.data_dir.join("aaf.db")).map_err(|e| runtime("database", e))?;
    Ok(Pool {
        db,
        discovered,
        max_concurrent: cfg.global.accounts.max_concurrent_per_account,
        executor: cfg.repo.run.executor,
        selection: cfg.global.accounts.selection,
        root: std::fs::canonicalize(&root).unwrap_or(root),
    })
}

impl Pool {
    fn router<'a>(&'a self, ctx: &Ctx, clock: &'a SystemClock) -> Result<Router<'a>, CmdError> {
        let router = Router::new(
            &self.db,
            &self.db,
            clock,
            ctx.data_dir.join("leases"),
            self.max_concurrent,
        );
        router
            .sync(&self.discovered)
            .map_err(|e| runtime("accounts", e))?;
        Ok(router)
    }

    /// Rows of the accounts discovery still sees. The store keeps rows of
    /// accounts whose config dir is gone; those are not listed.
    fn views(&self, router: &Router<'_>, now: i64) -> Result<Vec<AccountView>, CmdError> {
        let mut views = router.status(now).map_err(|e| runtime("accounts", e))?;
        views.retain(|view| self.discovered.iter().any(|p| p.id == view.id));
        Ok(views)
    }
}

fn status_word(status: &AccountStatus) -> &'static str {
    match status {
        AccountStatus::Available => "available",
        AccountStatus::Cooldown { .. } => "cooldown",
        AccountStatus::Leased => "leased",
    }
}

fn cooldown_until(status: &AccountStatus) -> Option<i64> {
    match status {
        AccountStatus::Cooldown { until } => Some(*until),
        _ => None,
    }
}

fn view_json(view: &AccountView) -> Value {
    json!({
        "id": view.id,
        "name": view.name,
        "provider": view.provider,
        "config_dir": view.config_dir,
        "status": status_word(&view.status),
        "cooldown_until": cooldown_until(&view.status),
        "tokens_5h": view.tokens_5h,
        "tokens_7d": view.tokens_7d,
    })
}

fn profile_json(profile: &AccountProfile) -> Value {
    json!({
        "id": profile.id,
        "name": profile.name,
        "provider": profile.provider,
        "config_dir": profile.config_dir,
    })
}

fn list(ctx: &Ctx) -> CmdResult {
    let pool = open(ctx)?;
    let clock = SystemClock;
    let router = pool.router(ctx, &clock)?;
    let views = pool.views(&router, clock.now())?;
    let mut text = format!("{} account(s)", views.len());
    for view in &views {
        let state = match cooldown_until(&view.status) {
            Some(until) => format!("cooldown until {until}"),
            None => status_word(&view.status).to_string(),
        };
        text.push_str(&format!(
            "\n{}  {}  {}  {}  {}  tokens_5h={}  tokens_7d={}",
            view.name,
            view.provider.as_str(),
            view.id,
            view.config_dir,
            state,
            view.tokens_5h,
            view.tokens_7d
        ));
    }
    let rows: Vec<Value> = views.iter().map(view_json).collect();
    Ok(CmdOutput::ok("ok", text).field("accounts", json!(rows)))
}

fn next(ctx: &Ctx) -> CmdResult {
    let pool = open(ctx)?;
    // Syncing first makes a freshly discovered account a candidate.
    pool.router(ctx, &SystemClock)?;
    // The pool `aaf run` picks from: only accounts of the provider the
    // repository's executor runs on. Asking the unfiltered router named a
    // Gemini account for a repository whose runs go to Claude Code.
    let executor = pool.executor.clone();
    let provider = provider_for(&executor);
    let store = Arc::new(pool.db.clone());
    let selection = pool.selection;
    let run_pool = SelectingPool::new(
        RouterPool {
            accounts: store.clone(),
            runs: store.clone(),
            clock: Arc::new(SystemClock),
            leases_dir: ctx.data_dir.join("leases"),
            max_concurrent_per_account: pool.max_concurrent,
            provider,
        },
        selection,
        pool.root.clone(),
        store,
    );
    let binding = run_pool
        .binding(SystemClock.now())
        .map_err(|e| runtime("accounts", e))?;
    // The lease is only a probe here: it is released when the grant drops,
    // so the answer is "who would run now", not a reservation. No task is
    // named: the answer is the repository's.
    let grant = run_pool
        .acquire_for("", Purpose::Work)
        .map_err(|e| runtime("accounts", e))?;
    let mode = format!("selection={}", selection.as_str());
    let out = match grant.as_ref().map(|grant| &grant.account) {
        Some(Some(profile)) => CmdOutput::ok(
            "ok",
            format!(
                "{}  {}  {}\n{mode}{}",
                profile.name,
                profile.id,
                profile.config_dir,
                match binding.source {
                    Some(source) => format!(": this repository's account ({})", source.as_str()),
                    None if selection == AccountSelection::Pinned =>
                        ": no binding yet, the first run binds the repository to it".to_string(),
                    None => String::new(),
                }
            ),
        )
        .field("account", profile_json(profile)),
        // Pinned: the repository's account is not free, and no other
        // account of the provider is offered in its place.
        None if selection == AccountSelection::Pinned && binding.account.is_some() => {
            let name = binding.account.as_ref().map_or("", |a| a.name.as_str());
            let why = match binding.cooldown_until {
                Some(until) => format!("in cooldown until {until}"),
                None => "leased by another run".to_string(),
            };
            CmdOutput::policy(
                "none",
                format!(
                    "{mode}: this repository is bound to account {name}, which is {why}; \
                     a run waits for it and takes no other account"
                ),
            )
            .field("account", Value::Null)
        }
        // No account of this provider exists at all (or the executor is a
        // custom command): `aaf run` then uses the agent's own login.
        Some(None) => CmdOutput::policy(
            "none",
            format!(
                "no account was discovered for executor {executor}: \
                 a run would use the agent's default login"
            ),
        )
        .field("account", Value::Null),
        None => CmdOutput::policy(
            "none",
            format!("no account is usable for executor {executor}: all are in cooldown or leased"),
        )
        .field("account", Value::Null),
    };
    Ok(out
        .field("executor", json!(executor))
        .field("selection", json!(selection))
        .field("binding", json!(binding)))
}

fn cooldown(ctx: &Ctx, wanted: &str, until: Option<i64>, clear: bool) -> CmdResult {
    let pool = open(ctx)?;
    let clock = SystemClock;
    // Syncing first makes a freshly discovered account addressable.
    pool.router(ctx, &clock)?;
    let matches: Vec<&AccountProfile> = pool
        .discovered
        .iter()
        .filter(|p| p.id == wanted || p.name == wanted)
        .collect();
    let profile = match matches.as_slice() {
        [one] => *one,
        [] => {
            return Err(CmdError::new(format!(
                "no account has id or name {wanted:?}; see aaf accounts list"
            )));
        }
        _ => {
            return Err(CmdError::new(format!(
                "{wanted:?} names more than one account; use the id"
            )));
        }
    };
    let until = if clear {
        None
    } else {
        Some(until.unwrap_or_else(|| clock.now() + DEFAULT_COOLDOWN_SECS))
    };
    pool.db
        .set_cooldown(&profile.id, until)
        .map_err(|e| runtime("accounts", e))?;
    let text = match until {
        Some(until) => format!("{} is in cooldown until {until}", profile.name),
        None => format!("{} is out of cooldown", profile.name),
    };
    Ok(CmdOutput::ok("ok", text)
        .field("account", profile_json(profile))
        .field("cooldown_until", json!(until)))
}

pub fn run(args: &AccountsArgs, ctx: &Ctx) -> CmdResult {
    match &args.action {
        Action::List => list(ctx),
        Action::Next => next(ctx),
        Action::Cooldown { id, until, clear } => cooldown(ctx, id, *until, *clear),
    }
}
