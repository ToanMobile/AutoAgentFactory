//! Declarative proof flows (`[[proof.flows]]`) loaded strictly from `base_sha`,
//! confined against `$AAF_DATA_DIR`, with minimal env, Tier-1 secret allowlist,
//! JSON assertions, and unified ProofRecord v2 persistence.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::config::{ProofConfig, ProofFlow, validate_flows};
use aaf_domain::ports::Clock;
use aaf_domain::proof::{AssertionResult, FlowResult, PROOF_SCHEMA_VERSION_V2, ProofRecord};
use aaf_domain::proof_flow_config::validate_flow_script_path;
use serde::Deserialize;

use super::capture::{
    choose_temp_dir, load_base_proof_capture_cmd, load_base_repo_config, run_capture_with_retry,
    validate_base_sha, write_and_mirror_proof,
};
use super::{HOST_PROOF_FLOWS_DEVICE, HOST_PROOF_FLOWS_TOOL, ProofError, RecordedProof};
use crate::executors::env::validate_machine_env_pass;
use crate::executors::stream_log::scrub_secrets;
use crate::gate::GateError;

fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(max_chars).collect()
}

const FLOW_LOG_TAIL_BYTES: usize = 2048;
const MAX_ASSERTIONS_BYTES: u64 = 256 * 1024;
const MAX_ASSERTION_FIELD_CHARS: usize = 200;
const TEMP_DIR_MAX_ATTEMPTS: usize = 32;
const BASE_FLOW_ENV: [&str; 7] = ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "USER", "TERM"];

/// Validated PNG bytes together with its `(width, height)` dimensions.
pub type CapturedImage = (Vec<u8>, u32, u32);

static FLOW_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct AssertionsFile {
    assertions: Vec<RawAssertion>,
}

#[derive(Deserialize)]
struct RawAssertion {
    name: String,
    passed: bool,
    #[serde(default)]
    detail: String,
}

/// Loads and validates `[[proof.flows]]` strictly from `base_sha:.autofactory/config.toml`.
pub fn load_base_flows(repo_root: &Path, base_sha: &str) -> Result<Vec<ProofFlow>, ProofError> {
    let Some(repo_cfg) = load_base_repo_config(repo_root, base_sha)? else {
        return Ok(Vec::new());
    };
    validate_flows(&repo_cfg.proof.flows).map_err(ProofError::Invalid)?;
    Ok(repo_cfg.proof.flows)
}

pub(crate) fn load_gate_base_flows(
    repo_root: &Path,
    base_sha: Option<&str>,
) -> Result<Vec<ProofFlow>, GateError> {
    let Some(sha) = base_sha else {
        return Ok(Vec::new());
    };
    load_base_flows(repo_root, sha).map_err(|e| GateError::Config(e.to_string()))
}

/// Checks whether an existing signed proof satisfies `cfg` (and, when
/// `base_flows` is non-empty, is a v2 record for the exact `base_sha` with every
/// base flow recorded as passing).
pub(crate) fn verify_gate_pre_proof(
    repo_root: &Path,
    data_dir: &Path,
    cfg: &ProofConfig,
    fp_before: &str,
    base_sha: Option<&str>,
    base_flows: &[ProofFlow],
) -> Option<String> {
    let record = match super::verify_proof(repo_root, data_dir, cfg, Some(fp_before)) {
        Ok(rec) => rec,
        Err(err) => return Some(err.to_string()),
    };
    if !base_flows.is_empty() {
        let sha_ok = base_sha.is_some() && record.base_sha.as_deref() == base_sha;
        let all_passed = base_flows.iter().all(|bf| {
            record
                .flows
                .iter()
                .any(|rf| rf.name == bf.name && rf.passed)
        });
        if record.schema_version != PROOF_SCHEMA_VERSION_V2 || !sha_ok || !all_passed {
            return Some("existing proof does not cover all required base proof flows".to_string());
        }
    }
    None
}

struct PrivateTempDir {
    path: PathBuf,
}

impl PrivateTempDir {
    fn create_outside(data_dir: &Path, repo_root: &Path) -> Result<Self, ProofError> {
        let canon_data = super::canonicalize_archived_root(data_dir);
        let canon_repo = super::canonicalize_archived_root(repo_root);
        let base_dir = choose_temp_dir(&canon_data, &canon_repo)?;
        let pid = std::process::id();
        for _ in 0..TEMP_DIR_MAX_ATTEMPTS {
            let seq = FLOW_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate = base_dir.join(format!("aaf-proof-flow-{pid}-{nanos:x}-{seq}"));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&candidate) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))?;
                    }
                    let canon = fs::canonicalize(&candidate)?;
                    if canon.starts_with(&canon_data) || canon.starts_with(&canon_repo) {
                        let _ = fs::remove_dir_all(&canon);
                        return Err(ProofError::Invalid(
                            "proof flow temp dir resolved inside AAF_DATA_DIR or repository"
                                .to_string(),
                        ));
                    }
                    return Ok(Self { path: canon });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(ProofError::Invalid(
            "failed to allocate private temp directory for proof flow".to_string(),
        ))
    }

    fn create_file(&self, name: &str, mode: u32, content: &[u8]) -> Result<PathBuf, ProofError> {
        let target = self.path.join(name);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut file = opts.open(&target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        if !content.is_empty() {
            file.write_all(content)?;
            file.flush()?;
        }
        drop(file);
        Ok(target)
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn load_base_script_bytes(
    repo_root: &Path,
    base_sha: &str,
    script: &str,
) -> Result<Vec<u8>, ProofError> {
    let sha = validate_base_sha(base_sha)?;
    validate_flow_script_path(script).map_err(ProofError::Invalid)?;
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", sha, "--", script],
        &[],
        aaf_fp::GIT_TIMEOUT,
    )?;
    let lines: Vec<&str> = listed.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != 1 {
        return Err(ProofError::Invalid(format!(
            "proof flow script {script:?} not found as a single file at {sha}"
        )));
    }
    let mode = lines[0].split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(ProofError::Invalid(format!(
            "proof flow script {script:?} at {sha} must be a regular file blob (got mode {mode:?})"
        )));
    }
    let spec = format!("{sha}:{script}");
    crate::gate::proc::git_bytes(repo_root, &["show", &spec], aaf_fp::GIT_TIMEOUT)
        .map_err(|err| ProofError::Invalid(format!("failed to read {spec}: {err}")))
}

fn resolve_secret_allowlist(explicit: Option<&[String]>) -> Result<Vec<String>, ProofError> {
    let list = match explicit {
        Some(list) => list.to_vec(),
        None => {
            let Some(path) = crate::config::global_config_path(&|k| std::env::var(k).ok()) else {
                return Ok(Vec::new());
            };
            let Some(dir) = path.parent() else {
                return Ok(Vec::new());
            };
            let global = crate::config::load_global(dir).map_err(|err| {
                ProofError::Invalid(format!(
                    "failed to load global config for proof secret allowlist: {err}"
                ))
            })?;
            global.proof.secret_env_allowlist
        }
    };
    validate_machine_env_pass(&list).map_err(|err| ProofError::Invalid(err.to_string()))?;
    Ok(list)
}

fn parse_assertions(
    assertions_path: &Path,
    temp_dir: &Path,
    flow_name: &str,
    secret_values: &[String],
) -> Result<Vec<AssertionResult>, ProofError> {
    let Some((_, file, len)) =
        super::open_canonical_regular(assertions_path, Some(temp_dir), "proof assertions")?
    else {
        return Err(ProofError::Invalid(format!(
            "proof flow {flow_name} failed: no assertions reported"
        )));
    };
    if len == 0 {
        return Err(ProofError::Invalid(format!(
            "proof flow {flow_name} failed: no assertions reported"
        )));
    }
    if len > MAX_ASSERTIONS_BYTES {
        return Err(ProofError::Invalid(format!(
            "proof flow {flow_name} failed: assertions file exceeds {MAX_ASSERTIONS_BYTES} bytes"
        )));
    }
    let mut buf = Vec::new();
    file.take(MAX_ASSERTIONS_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| {
            ProofError::Invalid(format!(
                "proof flow {flow_name} failed: cannot read assertions: {err}"
            ))
        })?;
    let parsed: AssertionsFile = serde_json::from_slice(&buf).map_err(|_| {
        ProofError::Invalid(format!(
            "proof flow {flow_name} failed: no assertions reported (invalid JSON)"
        ))
    })?;
    if parsed.assertions.is_empty() {
        return Err(ProofError::Invalid(format!(
            "proof flow {flow_name} failed: no assertions reported"
        )));
    }
    let cleaned: Vec<AssertionResult> = parsed
        .assertions
        .into_iter()
        .map(|a| {
            let name = one_line(
                &scrub_secrets(&a.name, secret_values),
                MAX_ASSERTION_FIELD_CHARS,
            );
            let detail = one_line(
                &scrub_secrets(&a.detail, secret_values),
                MAX_ASSERTION_FIELD_CHARS,
            );
            AssertionResult {
                name: if name.is_empty() {
                    "unnamed".to_string()
                } else {
                    name
                },
                passed: a.passed,
                detail,
            }
        })
        .collect();
    if let Some(failed) = cleaned.iter().find(|a| !a.passed) {
        let summary = if failed.detail.is_empty() {
            failed.name.clone()
        } else {
            format!("{} ({})", failed.name, failed.detail)
        };
        return Err(ProofError::Invalid(format!(
            "proof flow {flow_name} failed: {summary}"
        )));
    }
    Ok(cleaned)
}

fn read_optional_flow_image(
    out_png: &Path,
    temp_dir: &Path,
    cfg: &ProofConfig,
) -> Result<Option<CapturedImage>, ProofError> {
    let Some((_, _, len)) =
        super::open_canonical_regular(out_png, Some(temp_dir), "proof flow image")?
    else {
        return Ok(None);
    };
    if len == 0 {
        return Ok(None);
    }
    let (_, bytes) = super::read_regular_image(out_png, cfg.min_bytes)?;
    if super::png::is_likely_blank_png(&bytes) {
        return Err(ProofError::Invalid(super::png::BLANK_PNG_ERROR.to_string()));
    }
    let (width, height) =
        super::png::validate_png(&bytes, cfg.min_width, cfg.min_height, cfg.min_bytes)?;
    Ok(Some((bytes, width, height)))
}

/// Runs a single base proof flow with an explicit timeout and environment lookup.
/// Exposed so tests can enforce short timeouts without waiting for the 10s clamp.
#[allow(clippy::too_many_arguments)]
pub fn run_single_flow_with_timeout(
    repo_root: &Path,
    data_dir: &Path,
    base_sha: &str,
    cfg: &ProofConfig,
    flow: &ProofFlow,
    allowlist: &[String],
    timeout: Duration,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(FlowResult, Option<CapturedImage>), ProofError> {
    validate_machine_env_pass(allowlist).map_err(|e| ProofError::Invalid(e.to_string()))?;
    validate_machine_env_pass(&flow.secrets).map_err(|e| ProofError::Invalid(e.to_string()))?;

    let mut secret_pairs: Vec<(&str, String)> = Vec::with_capacity(flow.secrets.len());
    let mut secret_values: Vec<String> = Vec::with_capacity(flow.secrets.len());
    for name in &flow.secrets {
        if !allowlist.iter().any(|allowed| allowed == name) {
            return Err(ProofError::Invalid(format!(
                "proof flow {} failed: secret {name} not allowlisted in the global config",
                flow.name
            )));
        }
        let val = env_lookup(name).unwrap_or_default();
        if val.is_empty() {
            return Err(ProofError::Invalid(format!(
                "proof flow {} failed: secret {name} is missing or empty in environment",
                flow.name
            )));
        }
        secret_values.push(val.clone());
        secret_pairs.push((name.as_str(), val));
    }

    let canon_root = fs::canonicalize(repo_root)?;
    let script_bytes = load_base_script_bytes(&canon_root, base_sha, &flow.script)?;
    let temp_dir = PrivateTempDir::create_outside(data_dir, &canon_root)?;
    let script_path = temp_dir.create_file("flow.sh", 0o700, &script_bytes)?;
    let assertions_path = temp_dir.create_file("assertions.json", 0o600, &[])?;
    let out_png_path = temp_dir.create_file("out.png", 0o600, &[])?;

    let (argv, _mode) = crate::gate::confine::shell_argv(
        "exec /bin/sh \"$AAF_PROOF_SCRIPT\"",
        &canon_root,
        Some(data_dir),
    )?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(&canon_root);
    cmd.env_clear();
    for var in BASE_FLOW_ENV {
        if let Some(val) = env_lookup(var) {
            cmd.env(var, val);
        }
    }
    for (name, val) in &secret_pairs {
        cmd.env(name, val);
    }
    // Set AAF_PROOF_* after secrets so allowlisted secrets can never override them.
    cmd.env("AAF_PROOF_SCRIPT", &script_path);
    cmd.env("AAF_PROOF_OUT", &out_png_path);
    cmd.env("AAF_PROOF_ASSERTIONS", &assertions_path);
    cmd.env("AAF_PROOF_FLOW", &flow.name);

    let done = crate::gate::proc::capture(cmd, timeout, Some(FLOW_LOG_TAIL_BYTES), true)?;
    let duration_ms = u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX);
    if done.timed_out {
        return Err(ProofError::Invalid(format!(
            "proof flow {} failed: timed out after {}s",
            flow.name,
            timeout.as_secs()
        )));
    }
    if done.exit_code != Some(0) {
        let raw_tail = String::from_utf8_lossy(&done.output);
        let scrubbed = scrub_secrets(raw_tail.trim(), &secret_values);
        let detail = if scrubbed.is_empty() {
            String::new()
        } else {
            format!(": {scrubbed}")
        };
        return Err(ProofError::Invalid(format!(
            "proof flow {} failed (exit {:?}){detail}",
            flow.name, done.exit_code
        )));
    }

    let assertions =
        parse_assertions(&assertions_path, &temp_dir.path, &flow.name, &secret_values)?;
    let image = read_optional_flow_image(&out_png_path, &temp_dir.path, cfg)?;
    let result = FlowResult {
        name: flow.name.clone(),
        passed: true,
        needs_human: flow.needs_human,
        assertions,
        duration_ms,
    };
    Ok((result, image))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_flows_and_record_proof(
    repo_root: &Path,
    data_dir: &Path,
    base_sha: &str,
    cfg: &ProofConfig,
    flows: &[ProofFlow],
    allowlist_override: Option<&[String]>,
    tree_fp: &str,
    clock: &dyn Clock,
) -> Result<RecordedProof, ProofError> {
    run_flows_and_record_proof_with_env(
        repo_root,
        data_dir,
        base_sha,
        cfg,
        flows,
        allowlist_override,
        tree_fp,
        clock,
        &|k| std::env::var(k).ok(),
    )
}

/// Runs all `flows` from `base_sha` using `env_lookup`, falls back to base
/// `capture_cmd` if no flow produced an image, and persists a single signed v2
/// [`ProofRecord`] (mirrored to the common repository root).
#[allow(clippy::too_many_arguments)]
pub fn run_flows_and_record_proof_with_env(
    repo_root: &Path,
    data_dir: &Path,
    base_sha: &str,
    cfg: &ProofConfig,
    flows: &[ProofFlow],
    allowlist_override: Option<&[String]>,
    tree_fp: &str,
    clock: &dyn Clock,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<RecordedProof, ProofError> {
    let fp = tree_fp.trim();
    if fp.is_empty() {
        return Err(ProofError::Invalid("tree_fp must not be empty".to_string()));
    }
    if flows.is_empty() {
        return Err(ProofError::Invalid(
            "no proof flows configured at base_sha".to_string(),
        ));
    }
    let sha = validate_base_sha(base_sha)?;
    let canon_root = fs::canonicalize(repo_root)?;
    super::validate_host_capture_device(&canon_root, cfg, HOST_PROOF_FLOWS_DEVICE)?;
    let allowlist = resolve_secret_allowlist(allowlist_override)?;

    let mut results = Vec::with_capacity(flows.len());
    let mut image: Option<CapturedImage> = None;
    for flow in flows {
        let timeout = Duration::from_secs(flow.effective_timeout_secs());
        let (res, flow_img) = run_single_flow_with_timeout(
            &canon_root,
            data_dir,
            sha,
            cfg,
            flow,
            &allowlist,
            timeout,
            env_lookup,
        )?;
        results.push(res);
        if flow_img.is_some() {
            image = flow_img;
        }
    }

    let (bytes, width, height) = match image {
        Some(img) => img,
        None => {
            let capture_cmd = load_base_proof_capture_cmd(&canon_root, sha)?.ok_or_else(|| {
                ProofError::Invalid(
                    "proof flows produced no screenshot and no [proof].capture_cmd is configured at base_sha"
                        .to_string(),
                )
            })?;
            run_capture_with_retry(&canon_root, data_dir, &capture_cmd, cfg)?
        }
    };

    let recorded_at = clock.now();
    let sha_owned = sha.to_string();
    write_and_mirror_proof(
        &canon_root,
        data_dir,
        fp,
        &bytes,
        |root, img_path, img_sha, byte_len| ProofRecord {
            schema_version: PROOF_SCHEMA_VERSION_V2,
            repo_root: root.to_string_lossy().into_owned(),
            tree_fp: fp.to_string(),
            image_path: img_path.to_string_lossy().into_owned(),
            image_sha256: img_sha.to_string(),
            width,
            height,
            byte_len,
            source_tool: HOST_PROOF_FLOWS_TOOL.to_string(),
            device_serial: HOST_PROOF_FLOWS_DEVICE.to_string(),
            task_id: None,
            recorded_at,
            base_sha: Some(sha_owned.clone()),
            flows: results.clone(),
        },
    )
}
