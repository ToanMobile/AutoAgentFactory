//! The defaults that `crates/aaf-domain/defaults.toml` sets, embedded at
//! build time: a layer under every tier-2 (and tier-1) file, so the file is
//! the one place that decides what a repository gets when it leaves one of
//! these keys out. The structs' own `Default`s stay the conservative
//! baseline that code and tests build on; [`parse_repo`] and
//! [`parse_global`] are how a config file is read with the layer.
//!
//! Every key must be present (`deny_unknown_fields`, no serde defaults),
//! which the tests below check on every build. In a debug build only,
//! `AAF_NO_BUILTIN_DEFAULTS` turns the layer off so test fixtures keep the
//! baseline; a release build ignores it.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::config::AuditScope;
use crate::oracle_config::OracleMode;

/// The embedded file.
pub const DEFAULTS_TOML: &str = include_str!("../defaults.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinDefaults {
    pub run: RunDefaults,
    pub cross_audit: CrossAuditDefaults,
    pub pipeline: PipelineDefaults,
    pub gate: GateDefaults,
    pub proof: ProofDefaults,
    pub github: GithubDefaults,
    pub telegram: TelegramDefaults,
    pub notifications: NotificationsDefaults,
    pub safety: SafetyDefaults,
    pub unblocker: UnblockerDefaults,
    pub full_gate: FullGateDefaults,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullGateDefaults {
    pub cooldown_hours: u64,
    pub max_days: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnblockerDefaults {
    pub cost_guard: CostGuardDefaults,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDefaults {
    pub auto_merge: bool,
    pub max_parallel: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossAuditDefaults {
    pub executor: String,
    pub score_gate: bool,
    pub audit_scope: AuditScope,
    pub dual_review: bool,
    pub require_different_provider: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineDefaults {
    pub plan_stage: bool,
    pub plan_debate: bool,
    pub require_plan_approval: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDefaults {
    pub red_green_oracle: OracleMode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofDefaults {
    pub required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubDefaults {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramDefaults {
    pub enabled: bool,
    pub allow_approve: bool,
    pub allow_mutating: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsDefaults {
    pub webhook_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyDefaults {
    pub owner_github_allowlist: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostGuardDefaults {
    pub daily_token_cap: u64,
    pub spike_factor: u32,
}

/// The parsed embedded defaults.
///
/// # Panics
/// Only if `defaults.toml` is invalid, which the tests of this module catch
/// before a binary is ever built from it.
#[allow(clippy::expect_used)]
pub fn builtin() -> &'static BuiltinDefaults {
    static PARSED: OnceLock<BuiltinDefaults> = OnceLock::new();
    PARSED.get_or_init(|| {
        toml::from_str(DEFAULTS_TOML).expect("crates/aaf-domain/defaults.toml is invalid")
    })
}

/// `[run]` as written: `auto_merge` and `land` may each be left out, and
/// the one left out follows the other (neither: the built-in default).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfigRaw {
    executor: String,
    executor_cmd: Vec<String>,
    auto_merge: Option<bool>,
    land: Option<crate::land_mode::LandMode>,
    max_parallel: u32,
    run_timeout_secs: u64,
    model: String,
    effort: String,
    env_pass: Vec<String>,
    max_open_review: u32,
}

impl Default for RunConfigRaw {
    fn default() -> Self {
        let d = crate::config::RunConfig::default();
        Self {
            executor: d.executor,
            executor_cmd: d.executor_cmd,
            auto_merge: None,
            land: None,
            max_parallel: d.max_parallel,
            run_timeout_secs: d.run_timeout_secs,
            model: d.model,
            effort: d.effort,
            env_pass: d.env_pass,
            max_open_review: d.max_open_review,
        }
    }
}

impl From<RunConfigRaw> for crate::config::RunConfig {
    fn from(raw: RunConfigRaw) -> Self {
        use crate::land_mode::LandMode;
        let d = crate::config::RunConfig::default();
        let (auto_merge, land) = match (raw.auto_merge, raw.land) {
            (Some(am), Some(land)) => (am, land),
            (Some(true), None) => (true, LandMode::Auto),
            (Some(false), None) => (false, LandMode::Pr),
            (None, Some(land)) => (land.auto_lands(), land),
            (None, None) => (d.auto_merge, d.land),
        };
        Self {
            executor: raw.executor,
            executor_cmd: raw.executor_cmd,
            auto_merge,
            land,
            max_parallel: raw.max_parallel,
            run_timeout_secs: raw.run_timeout_secs,
            model: raw.model,
            effort: raw.effort,
            env_pass: raw.env_pass,
            max_open_review: raw.max_open_review,
        }
    }
}

/// Tier-2 sections the layer fills.
const REPO_SECTIONS: [&str; 8] = [
    "run",
    "cross_audit",
    "pipeline",
    "gate",
    "proof",
    "github",
    "unblocker",
    "full_gate",
];
/// Tier-1 sections the layer fills.
const GLOBAL_SECTIONS: [&str; 3] = ["telegram", "notifications", "safety"];

fn layer_off() -> bool {
    cfg!(debug_assertions) && std::env::var_os("AAF_NO_BUILTIN_DEFAULTS").is_some()
}

/// Whether the embedded `defaults.toml` layer is active in this process.
pub fn is_active() -> bool {
    !layer_off()
}

#[allow(clippy::expect_used)]
fn embedded_table() -> &'static toml::Table {
    static TABLE: OnceLock<toml::Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        DEFAULTS_TOML
            .parse::<toml::Table>()
            .expect("crates/aaf-domain/defaults.toml is invalid")
    })
}

/// `over` merged onto `under`, tables key by key, anything else replaced.
fn merge(under: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (under.get_mut(&key), value) {
            (Some(toml::Value::Table(u)), toml::Value::Table(o)) => merge(u, o),
            (_, value) => {
                under.insert(key, value);
            }
        }
    }
}

fn layered(file: toml::Table, sections: &[&str], on: bool) -> toml::Table {
    if !on {
        return file;
    }
    let mut out: toml::Table = embedded_table()
        .iter()
        .filter(|(k, _)| sections.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // A file that sets `land` decides auto-merge with it: the built-in
    // `auto_merge` must not contradict it.
    let file_run = file.get("run").and_then(toml::Value::as_table);
    if file_run.is_some_and(|r| r.contains_key("land") || r.contains_key("auto_merge"))
        && let Some(run) = out.get_mut("run").and_then(toml::Value::as_table_mut)
    {
        run.remove("auto_merge");
    }
    merge(&mut out, file);
    out
}

/// `(section, key, value as TOML)` of every tier-2 scalar the layer sets,
/// in file order (`unblocker.cost_guard` is the section of its two keys).
/// Empty while the layer is off, so what is rendered is what is parsed.
pub fn repo_entries() -> Vec<(String, String, String)> {
    if layer_off() {
        return Vec::new();
    }
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<(String, String, String)>) {
        for (key, value) in table {
            match value {
                toml::Value::Table(sub) => {
                    let name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&name, sub, out);
                }
                other if !prefix.is_empty() => {
                    out.push((prefix.to_string(), key.clone(), other.to_string()));
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    let repo: toml::Table = embedded_table()
        .iter()
        .filter(|(k, _)| REPO_SECTIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    walk("", &repo, &mut out);
    out
}

/// A tier-2 file (`text` may be empty: no file) with the layer under it.
/// The file alone is validated first, so its errors keep their position.
pub fn parse_repo(text: &str) -> Result<crate::config::RepoConfig, toml::de::Error> {
    parse_repo_with(text, !layer_off())
}

fn parse_repo_with(text: &str, on: bool) -> Result<crate::config::RepoConfig, toml::de::Error> {
    toml::from_str::<crate::config::RepoConfig>(text)?;
    let table = text.parse::<toml::Table>()?;
    crate::config::RepoConfig::deserialize(layered(table, &REPO_SECTIONS, on))
}

/// A tier-1 file (`text` may be empty) with the layer under it.
pub fn parse_global(text: &str) -> Result<crate::config::GlobalConfig, toml::de::Error> {
    toml::from_str::<crate::config::GlobalConfig>(text)?;
    let table = text.parse::<toml::Table>()?;
    crate::config::GlobalConfig::deserialize(layered(table, &GLOBAL_SECTIONS, !layer_off()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_defaults_parse_with_every_key() {
        let parsed: Result<BuiltinDefaults, _> = toml::from_str(DEFAULTS_TOML);
        assert!(parsed.is_ok(), "{parsed:?}");
    }

    #[test]
    fn the_layer_fills_what_a_file_leaves_out_and_the_file_wins() {
        let d = builtin();
        let repo = parse_repo_with("", true).unwrap();
        assert_eq!(repo.run.auto_merge, d.run.auto_merge);
        assert_eq!(repo.run.land.auto_lands(), d.run.auto_merge);
        assert_eq!(repo.run.max_parallel, d.run.max_parallel);
        assert_eq!(repo.cross_audit.executor, d.cross_audit.executor);
        assert_eq!(repo.pipeline.plan_stage, d.pipeline.plan_stage);
        assert_eq!(repo.gate.red_green_oracle, d.gate.red_green_oracle);
        assert_eq!(repo.full_gate.cooldown_hours, d.full_gate.cooldown_hours);
        assert_eq!(repo.full_gate.max_days, d.full_gate.max_days);
        let global = crate::config::GlobalConfig::deserialize(layered(
            toml::Table::new(),
            &GLOBAL_SECTIONS,
            true,
        ))
        .unwrap();
        assert_eq!(global.telegram.enabled, d.telegram.enabled);

        let repo = parse_repo_with("[run]\nauto_merge = false\nmax_parallel = 3\n", true).unwrap();
        assert!(!repo.run.auto_merge && !repo.run.land.auto_lands());
        assert_eq!(repo.run.max_parallel, 3);
        let repo = parse_repo_with("[run]\nland = \"pr\"\n", true).unwrap();
        assert!(
            !repo.run.auto_merge,
            "land = pr turns the built-in auto-merge off"
        );
    }

    #[test]
    fn a_file_error_keeps_its_position() {
        let err = parse_repo_with("[run]\nnope = 1\n", true).unwrap_err();
        assert!(err.span().is_some(), "{err}");
    }
}
