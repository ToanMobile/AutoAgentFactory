#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::config::{GlobalConfig, GlobalProofConfig, ProofConfig, ProofFlow, RepoConfig};
use aaf_domain::proof_flow_config::{
    DEFAULT_FLOW_TIMEOUT_SECS, MAX_FLOW_TIMEOUT_SECS, MIN_FLOW_TIMEOUT_SECS, validate_flows,
};
use aaf_engine::config::{ConfigInputs, ConfigOverlay, load};
use aaf_engine::gate::config_hash;

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn parses_proof_flows_and_defaults_timeout() {
    let raw = r#"
[proof]
required = true

[[proof.flows]]
name = "smoke_login"
script = "scripts/proof/smoke.sh"

[[proof.flows]]
name = "checkout-ui"
script = "scripts/proof/checkout.sh"
timeout_secs = 45
needs_human = true
secrets = ["API_TOKEN"]
"#;
    let cfg: RepoConfig = toml::from_str(raw).unwrap();
    assert!(cfg.proof.required);
    assert_eq!(cfg.proof.flows.len(), 2);

    let first = &cfg.proof.flows[0];
    assert_eq!(first.name, "smoke_login");
    assert_eq!(first.script, "scripts/proof/smoke.sh");
    assert_eq!(first.timeout_secs, DEFAULT_FLOW_TIMEOUT_SECS);
    assert_eq!(first.timeout_secs, 180);
    assert!(!first.needs_human);
    assert!(first.secrets.is_empty());

    let second = &cfg.proof.flows[1];
    assert_eq!(second.name, "checkout-ui");
    assert_eq!(second.timeout_secs, 45);
    assert!(second.needs_human);
    assert_eq!(second.secrets, vec!["API_TOKEN"]);

    assert!(validate_flows(&cfg.proof.flows).is_ok());
}

#[test]
fn clamps_flow_timeout_bounds() {
    let mut flow = ProofFlow::default();
    assert_eq!(flow.timeout_secs, DEFAULT_FLOW_TIMEOUT_SECS);
    assert_eq!(flow.effective_timeout_secs(), 180);

    flow.timeout_secs = 0;
    assert_eq!(flow.effective_timeout_secs(), MIN_FLOW_TIMEOUT_SECS);
    assert_eq!(flow.effective_timeout_secs(), 10);

    flow.timeout_secs = 5;
    assert_eq!(flow.effective_timeout_secs(), 10);

    flow.timeout_secs = 900;
    assert_eq!(flow.effective_timeout_secs(), MAX_FLOW_TIMEOUT_SECS);

    flow.timeout_secs = 9999;
    assert_eq!(flow.effective_timeout_secs(), 900);
}

#[test]
fn validate_flows_rejects_bad_names_paths_and_secrets() {
    let valid = |name: &str, script: &str, secrets: &[&str]| ProofFlow {
        name: name.to_string(),
        script: script.to_string(),
        secrets: secrets.iter().map(|s| (*s).to_string()).collect(),
        ..ProofFlow::default()
    };

    assert!(validate_flows(&[valid("ok_flow-1", "scripts/ok.sh", &["TOK_A"])]).is_ok());

    // Invalid names
    assert!(validate_flows(&[valid("", "scripts/ok.sh", &[])]).is_err());
    assert!(validate_flows(&[valid("BadCase", "scripts/ok.sh", &[])]).is_err());
    assert!(validate_flows(&[valid("has space", "scripts/ok.sh", &[])]).is_err());
    let long_name = "a".repeat(41);
    assert!(validate_flows(&[valid(&long_name, "scripts/ok.sh", &[])]).is_err());

    // Duplicate names
    assert!(
        validate_flows(&[
            valid("dup", "scripts/a.sh", &[]),
            valid("dup", "scripts/b.sh", &[]),
        ])
        .is_err()
    );

    // Invalid script paths
    assert!(validate_flows(&[valid("f1", "", &[])]).is_err());
    assert!(validate_flows(&[valid("f1", "/abs/script.sh", &[])]).is_err());
    assert!(validate_flows(&[valid("f1", "../escape.sh", &[])]).is_err());
    assert!(validate_flows(&[valid("f1", "scripts/../escape.sh", &[])]).is_err());
    assert!(validate_flows(&[valid("f1", "scripts\\win.sh", &[])]).is_err());

    // Empty or duplicate secrets
    assert!(validate_flows(&[valid("f1", "scripts/ok.sh", &[""])]).is_err());
    assert!(validate_flows(&[valid("f1", "scripts/ok.sh", &["TOK", "TOK"])]).is_err());
}

#[test]
fn tier1_proof_secret_allowlist_loads_and_tier3_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let global_path = tmp.path().join("global.toml");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".autofactory")).unwrap();

    std::fs::write(
        &global_path,
        "[proof]\nsecret_env_allowlist = [\"API_TOKEN\", \"DEMO_SECRET\"]\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".autofactory/config.toml"),
        "[proof]\nrequired = true\n\n[[proof.flows]]\nname = \"smoke\"\nscript = \"scripts/smoke.sh\"\nsecrets = [\"API_TOKEN\"]\n",
    )
    .unwrap();
    // Tier 3 attempts to override both secret_env_allowlist and flows -> must be ignored.
    std::fs::write(
        repo.join(".autofactory/config.local.toml"),
        "[proof]\nsecret_env_allowlist = [\"EVIL_SECRET\"]\n\n[[proof.flows]]\nname = \"evil\"\nscript = \"scripts/evil.sh\"\n",
    )
    .unwrap();

    let eff = load(&ConfigInputs {
        global_config: Some(global_path),
        repo_root: repo,
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();

    assert_eq!(
        eff.global.proof,
        GlobalProofConfig {
            secret_env_allowlist: vec!["API_TOKEN".to_string(), "DEMO_SECRET".to_string()],
        }
    );
    assert_eq!(eff.repo.proof.flows.len(), 1);
    assert_eq!(eff.repo.proof.flows[0].name, "smoke");
    assert!(
        eff.warnings.iter().any(|w| w.contains("[proof] ignored")),
        "expected tier-3 [proof] ignored warning, got {:?}",
        eff.warnings
    );

    // Unknown field in tier-1 [proof] fails closed.
    let bad_global = toml::from_str::<GlobalConfig>("[proof]\nunknown_key = true\n");
    assert!(bad_global.is_err());
}

#[test]
fn config_hash_unchanged_when_flows_empty() {
    let default_cfg = RepoConfig::default();
    let json = serde_json::to_string(&default_cfg.proof).unwrap();
    assert!(
        !json.contains("\"flows\""),
        "empty flows must be omitted from serialization to preserve config_hash: {json}"
    );
    let mut with_flow = RepoConfig::default();
    with_flow.proof.flows.push(ProofFlow {
        name: "smoke".to_string(),
        script: "scripts/smoke.sh".to_string(),
        ..ProofFlow::default()
    });
    assert_ne!(config_hash(&default_cfg), config_hash(&with_flow));
    assert_eq!(
        config_hash(&default_cfg),
        config_hash(&RepoConfig::default())
    );
}

#[test]
fn proof_config_default_has_empty_flows() {
    assert!(ProofConfig::default().flows.is_empty());
}
