//! Host-side proof capture (`[proof].capture_cmd` loaded strictly from `base_sha`)
//! and glob-scoped proof triggering (`[proof].trigger_globs`).

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::config::{ProofConfig, RepoConfig};
use aaf_domain::gate::GateStatus;
use aaf_domain::ports::Clock;
use aaf_domain::proof::{PROOF_SCHEMA_VERSION, ProofRecord};
use globset::{GlobBuilder, GlobSetBuilder};

use super::{ProofError, RecordedProof};
use crate::config::REPO_CONFIG_REL;
use crate::gate::{GIT_TIMEOUT, GateError, HiddenPolicy};

/// Source tool identifier recorded for host-side `capture_cmd` proofs.
pub const HOST_CAPTURE_TOOL: &str = "host:capture_cmd";
/// Device serial identifier recorded for host-side `capture_cmd` proofs.
pub const HOST_CAPTURE_DEVICE: &str = "host-capture";

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(180);
const CAPTURE_LOG_TAIL_BYTES: usize = 4096;
const TEMP_FILE_MAX_ATTEMPTS: usize = 32;
const DEFAULT_PROOF_RETRY_DELAY_MS: u64 = 500;
const MAX_PROOF_RETRY_DELAY_MS: u64 = 2000;
const SCRUBBED_CAPTURE_ENV: [&str; 7] = [
    "AAF_DATA_DIR",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns `true` when proof verification is required for `changed_paths`.
///
/// - `!cfg.required` => `false`
/// - `cfg.trigger_globs.is_empty()` => `true`
/// - otherwise => `true` iff any path in `changed_paths` matches any glob in
///   `cfg.trigger_globs` (invalid globs fail closed to `true`).
pub fn proof_triggered(cfg: &ProofConfig, changed_paths: &[String]) -> bool {
    if !cfg.required {
        return false;
    }
    if cfg.trigger_globs.is_empty() {
        return true;
    }
    let mut builder = GlobSetBuilder::new();
    let mut non_empty = 0usize;
    for pattern in &cfg.trigger_globs {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(glob) = GlobBuilder::new(trimmed).literal_separator(true).build() else {
            return true;
        };
        builder.add(glob);
        non_empty += 1;
    }
    if non_empty == 0 {
        return true;
    }
    let Ok(set) = builder.build() else {
        return true;
    };
    changed_paths.iter().any(|path| set.is_match(path))
}

/// Loads `[proof].capture_cmd` strictly from immutable
/// `git show <base_sha>:.autofactory/config.toml`.
///
/// Returns `Ok(None)` when `.autofactory/config.toml` does not exist at
/// `base_sha` or its `[proof].capture_cmd` is empty. Fails closed on invalid
/// `base_sha`, non-blob entries, git errors, or invalid TOML.
pub fn load_base_proof_capture_cmd(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Option<String>, ProofError> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ProofError::Invalid(format!(
            "invalid base_sha {base_sha:?}"
        )));
    }
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", sha, "--", REPO_CONFIG_REL],
        &[],
        aaf_fp::GIT_TIMEOUT,
    )?;
    let line = listed.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(ProofError::Invalid(format!(
            "{REPO_CONFIG_REL} at {sha} must be a regular file blob (got mode {mode:?})"
        )));
    }
    let spec = format!("{sha}:{REPO_CONFIG_REL}");
    let shown = aaf_fp::git(repo_root, &["show", &spec], &[], aaf_fp::GIT_TIMEOUT)?;
    let repo_cfg: RepoConfig = toml::from_str(&shown)
        .map_err(|err| ProofError::Invalid(format!("invalid TOML in {spec}: {}", err.message())))?;
    let cmd = repo_cfg.proof.capture_cmd.trim();
    Ok((!cmd.is_empty()).then(|| cmd.to_string()))
}

/// Destination path for a host-captured proof PNG under
/// `$AAF_DATA_DIR/proofs/<repo_id>/<safe_tree_fp>.png`.
pub fn captured_proof_image_path(data_dir: &Path, repo_root: &Path, tree_fp: &str) -> PathBuf {
    let safe_fp: String = tree_fp
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if safe_fp.is_empty() {
        aaf_fp::sha256_hex(tree_fp.as_bytes())
    } else {
        safe_fp
    };
    data_dir
        .join("proofs")
        .join(aaf_fp::repo_id(repo_root))
        .join(format!("{stem}.png"))
}

struct PrivateTempFile {
    path: PathBuf,
}

impl PrivateTempFile {
    fn create_outside(data_dir: &Path, repo_root: &Path) -> Result<Self, ProofError> {
        let canon_data = super::canonicalize_archived_root(data_dir);
        let canon_repo = super::canonicalize_archived_root(repo_root);
        let base_dir = choose_temp_dir(&canon_data, &canon_repo)?;
        let pid = std::process::id();
        for _ in 0..TEMP_FILE_MAX_ATTEMPTS {
            let seq = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate = base_dir.join(format!("aaf-proof-{pid}-{nanos:x}-{seq}.png"));
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&candidate) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file.set_permissions(fs::Permissions::from_mode(0o600))?;
                    }
                    drop(file);
                    let canon = fs::canonicalize(&candidate)?;
                    if canon.starts_with(&canon_data) || canon.starts_with(&canon_repo) {
                        let _ = fs::remove_file(&canon);
                        return Err(ProofError::Invalid(
                            "proof output temp file resolved inside AAF_DATA_DIR or repository"
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
            "failed to allocate unique private temp file for proof capture".to_string(),
        ))
    }
}

impl Drop for PrivateTempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn choose_temp_dir(canon_data: &Path, canon_repo: &Path) -> Result<PathBuf, ProofError> {
    let candidates = [std::env::temp_dir(), PathBuf::from("/tmp")];
    for candidate in candidates {
        let Ok(canon) = fs::canonicalize(&candidate) else {
            continue;
        };
        if canon.is_dir() && !canon.starts_with(canon_data) && !canon.starts_with(canon_repo) {
            return Ok(canon);
        }
    }
    Err(ProofError::Invalid(
        "cannot find a temp directory outside AAF_DATA_DIR and repository".to_string(),
    ))
}

fn write_private_bytes(data_dir: &Path, dest: &Path, bytes: &[u8]) -> Result<PathBuf, ProofError> {
    let mut dirs: Vec<&Path> = dest
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(data_dir))
        .collect();
    dirs.reverse();
    for dir in dirs {
        crate::db::private::create_dir(dir)?;
    }
    let parent = dest
        .parent()
        .ok_or_else(|| ProofError::Invalid("proof destination has no parent".to_string()))?;
    let seq = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".proof-img-{}-{seq}.tmp", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    let write_res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.flush()
    })();
    drop(file);
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    if let Err(err) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    crate::db::private::restrict_file_if_present(dest)?;
    Ok(fs::canonicalize(dest)?)
}

#[allow(clippy::too_many_arguments)]
fn build_record(
    repo_root: &Path,
    tree_fp: &str,
    image_path: &Path,
    image_sha256: &str,
    width: u32,
    height: u32,
    byte_len: u64,
    recorded_at: i64,
) -> ProofRecord {
    ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: tree_fp.to_string(),
        image_path: image_path.to_string_lossy().into_owned(),
        image_sha256: image_sha256.to_string(),
        width,
        height,
        byte_len,
        source_tool: HOST_CAPTURE_TOOL.to_string(),
        device_serial: HOST_CAPTURE_DEVICE.to_string(),
        task_id: None,
        recorded_at,
    }
}

fn proof_retry_delay() -> Duration {
    let ms = std::env::var("AAF_PROOF_RETRY_DELAY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PROOF_RETRY_DELAY_MS)
        .min(MAX_PROOF_RETRY_DELAY_MS);
    Duration::from_millis(ms)
}

fn is_retryable_capture_error(err: &ProofError) -> bool {
    match err {
        ProofError::Invalid(msg) => {
            !msg.contains("must be a regular non-symlink file")
                && !msg.contains("gate commands cannot be confined")
        }
        ProofError::Io(_) => true,
        ProofError::DeniedDevice(_) | ProofError::Fingerprint(_) => false,
    }
}

fn run_capture_attempt(
    canon_root: &Path,
    data_dir: &Path,
    capture_cmd: &str,
    cfg: &ProofConfig,
) -> Result<(Vec<u8>, u32, u32), ProofError> {
    let temp_out = PrivateTempFile::create_outside(data_dir, canon_root)?;
    let (argv, _mode) = crate::gate::confine::shell_argv(capture_cmd, canon_root, Some(data_dir))?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(canon_root);
    for var in SCRUBBED_CAPTURE_ENV {
        cmd.env_remove(var);
    }
    cmd.env("AAF_PROOF_OUT", &temp_out.path);

    let done =
        crate::gate::proc::capture(cmd, CAPTURE_TIMEOUT, Some(CAPTURE_LOG_TAIL_BYTES), true)?;
    if done.timed_out {
        return Err(ProofError::Invalid(format!(
            "proof capture_cmd timed out after {}s",
            CAPTURE_TIMEOUT.as_secs()
        )));
    }
    if done.exit_code != Some(0) {
        let tail = String::from_utf8_lossy(&done.output);
        let tail = tail.trim();
        let detail = if tail.is_empty() {
            String::new()
        } else {
            format!(": {tail}")
        };
        return Err(ProofError::Invalid(format!(
            "proof capture_cmd failed (exit {:?}){detail}",
            done.exit_code
        )));
    }

    let (_, bytes) = super::read_regular_image(&temp_out.path, cfg.min_bytes)?;
    if super::png::is_likely_blank_png(&bytes) {
        return Err(ProofError::Invalid(super::png::BLANK_PNG_ERROR.to_string()));
    }
    let (width, height) =
        super::png::validate_png(&bytes, cfg.min_width, cfg.min_height, cfg.min_bytes)?;
    Ok((bytes, width, height))
}

/// Runs the immutable `base_sha` `[proof].capture_cmd` on the host, validates
/// the PNG output from a private per-run temp file outside `AAF_DATA_DIR`
/// (retrying once automatically after a brief settle pause on transient capture
/// failure or blank frame), copies the validated bytes into
/// `$AAF_DATA_DIR/proofs/<repo_id>/<tree_fp>.png` (mode `0600`), and writes the
/// HMAC-signed [`ProofRecord`] (mirroring to the common repository root for the
/// exact same `tree_fp`).
pub fn capture_and_record_proof(
    repo_root: &Path,
    data_dir: &Path,
    base_sha: &str,
    cfg: &ProofConfig,
    tree_fp: &str,
    clock: &dyn Clock,
) -> Result<RecordedProof, ProofError> {
    let fp = tree_fp.trim();
    if fp.is_empty() {
        return Err(ProofError::Invalid("tree_fp must not be empty".to_string()));
    }
    let canon_root = fs::canonicalize(repo_root)?;
    super::validate_host_capture_device(&canon_root, cfg, HOST_CAPTURE_DEVICE)?;

    let capture_cmd = load_base_proof_capture_cmd(&canon_root, base_sha)?.ok_or_else(|| {
        ProofError::Invalid("no [proof].capture_cmd configured at base_sha".to_string())
    })?;

    let (bytes, width, height) = match run_capture_attempt(&canon_root, data_dir, &capture_cmd, cfg)
    {
        Ok(ok) => ok,
        Err(err) if is_retryable_capture_error(&err) => {
            let delay = proof_retry_delay();
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            run_capture_attempt(&canon_root, data_dir, &capture_cmd, cfg)?
        }
        Err(err) => return Err(err),
    };
    let image_sha256 = aaf_fp::sha256_hex(&bytes);
    let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let recorded_at = clock.now();

    let dest_png = captured_proof_image_path(data_dir, &canon_root, fp);
    let canon_dest = write_private_bytes(data_dir, &dest_png, &bytes)?;
    let record = build_record(
        &canon_root,
        fp,
        &canon_dest,
        &image_sha256,
        width,
        height,
        byte_len,
        recorded_at,
    );
    let proof_path = aaf_fp::write_proof(data_dir, &record)?;

    if let Ok(layout) = aaf_fp::probe_git_layout(&canon_root, aaf_fp::GIT_TIMEOUT) {
        let common = layout.common_repo_root();
        let canon_common = fs::canonicalize(common).unwrap_or_else(|_| common.to_path_buf());
        if canon_common != canon_root {
            let common_png = captured_proof_image_path(data_dir, &canon_common, fp);
            let canon_common_png = write_private_bytes(data_dir, &common_png, &bytes)?;
            let mirrored = build_record(
                &canon_common,
                fp,
                &canon_common_png,
                &image_sha256,
                width,
                height,
                byte_len,
                recorded_at,
            );
            aaf_fp::write_proof(data_dir, &mirrored)?;
        }
    }

    Ok(RecordedProof { record, proof_path })
}

pub(crate) struct GateProofCheck<'a> {
    pub repo_root: &'a Path,
    pub data_dir: &'a Path,
    pub layout: &'a aaf_fp::GitLayout,
    pub cfg: &'a ProofConfig,
    pub base_sha: Option<&'a str>,
    pub fp_before: &'a str,
    pub policy: HiddenPolicy,
    pub release: bool,
    pub clock: &'a dyn Clock,
}

pub(crate) fn load_gate_base_capture_cmd(
    repo_root: &Path,
    base_sha: Option<&str>,
) -> Result<Option<String>, GateError> {
    let Some(sha) = base_sha else {
        return Ok(None);
    };
    load_base_proof_capture_cmd(repo_root, sha).map_err(|e| GateError::Config(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_gate_proof(
    ctx: &GateProofCheck<'_>,
    proof_needed: bool,
    pre_proof_err: Option<&str>,
    has_base_capture: bool,
    pre_capture_status: GateStatus,
    tree_fp: &mut String,
    hidden: &mut Vec<String>,
    hidden_notes: &mut Vec<String>,
) -> Result<Option<String>, GateError> {
    if !proof_needed {
        return Ok(None);
    }
    if pre_proof_err.is_none() {
        return Ok(
            super::verify_proof(ctx.repo_root, ctx.data_dir, ctx.cfg, Some(ctx.fp_before))
                .err()
                .map(|e| e.to_string()),
        );
    }
    if !has_base_capture {
        return Ok(pre_proof_err.map(str::to_string));
    }
    if pre_capture_status != GateStatus::Clear {
        return Ok((pre_capture_status == GateStatus::Unverified)
            .then(|| pre_proof_err.map(str::to_string))
            .flatten());
    }
    let Some(base_sha) = ctx.base_sha else {
        return Ok(pre_proof_err.map(str::to_string));
    };
    if let Err(err) = capture_and_record_proof(
        ctx.repo_root,
        ctx.data_dir,
        base_sha,
        ctx.cfg,
        ctx.fp_before,
        ctx.clock,
    ) {
        return Ok(Some(err.to_string()));
    }
    if ctx.policy == HiddenPolicy::Remove || ctx.release {
        let after = crate::gate::hidden::inspect(
            ctx.repo_root,
            ctx.policy,
            crate::gate::hidden::When::AfterCommands,
            ctx.release,
        )?;
        hidden.extend(after.blocks);
        hidden_notes.extend(after.notes);
    }
    *tree_fp = aaf_fp::tree_fp_with_layout(ctx.repo_root, ctx.layout, GIT_TIMEOUT)?;
    Ok(
        super::verify_proof(ctx.repo_root, ctx.data_dir, ctx.cfg, Some(ctx.fp_before))
            .err()
            .map(|e| e.to_string()),
    )
}
