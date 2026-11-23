//! Fail-closed SHA-256 verified self-updater for macOS, Linux, and Windows.

#[cfg(test)]
mod tests;

use std::collections::hash_map::RandomState;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_REPO: &str = "ToanMobile/AutoAgentFactory";
pub const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const SHA256SUMS_FILENAME: &str = "SHA256SUMS.txt";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid semantic version: {0}")]
    InvalidSemVer(String),
    #[error("unsupported target: {0}")]
    UnsupportedTarget(String),
    #[error("invalid repository slug: {0}")]
    InvalidRepo(String),
    #[error("refused release URL: {0}")]
    InvalidUrl(String),
    #[error("HTTP {status} from {url}")]
    HttpStatus { url: String, status: u16 },
    #[error("network request failed: {0}")]
    Transport(String),
    #[error("response from {url} exceeded {limit} bytes")]
    SizeLimitExceeded { url: String, limit: usize },
    #[error("invalid release metadata: {0}")]
    InvalidMetadata(String),
    #[error("SHA-256 entry missing for '{0}' in {SHA256SUMS_FILENAME}")]
    MissingChecksum(String),
    #[error("malformed SHA-256 checksum: {0}")]
    MalformedChecksum(String),
    #[error("SHA-256 checksum mismatch: expected {expected}, actual {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("archive extraction failed: {0}")]
    ExtractFailed(String),
    #[error("unsafe archive entry: {0}")]
    UnsafeArchiveEntry(String),
    #[error("installation I/O error: {0}")]
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatePlan {
    pub repo: String,
    pub target: String,
    pub current_version: SemVer,
    pub target_version: SemVer,
    pub tag: String,
    pub is_newer: bool,
    pub is_same: bool,
    pub archive_name: String,
    pub archive_url: String,
    pub checksums_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArchive {
    pub plan: UpdatePlan,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReceipt {
    pub tag: String,
    pub target: String,
    pub sha256: String,
    pub install_dir: PathBuf,
    pub installed_binaries: Vec<PathBuf>,
}

pub trait ReleaseSource {
    fn latest_tag(&self, repo: &str) -> Result<String, UpdateError>;
    fn fetch_text(&self, url: &str) -> Result<String, UpdateError>;
    fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, UpdateError>;
}

/// Strict `MAJOR.MINOR.PATCH` parser accepting an optional leading `v`.
pub fn parse_semver(s: &str) -> Result<SemVer, UpdateError> {
    let trimmed = s.trim();
    let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let bad = |why: &str| UpdateError::InvalidSemVer(format!("'{s}': {why}"));
    let mut parts = body.split('.');
    let (Some(a), Some(b), Some(c), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("expected major.minor.patch"));
    };
    let num = |p: &str| -> Result<u64, UpdateError> {
        if p.is_empty() || !p.bytes().all(|ch| ch.is_ascii_digit()) {
            return Err(bad("components must be ASCII digits"));
        }
        p.parse::<u64>().map_err(|_| bad("component overflow"))
    };
    Ok(SemVer {
        major: num(a)?,
        minor: num(b)?,
        patch: num(c)?,
    })
}

pub fn target_triple(os: &str, arch: &str) -> Result<&'static str, UpdateError> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(UpdateError::UnsupportedTarget(format!("{os}/{arch}"))),
    }
}

pub fn current_target_triple() -> Result<&'static str, UpdateError> {
    target_triple(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn archive_filename(tag: &str, target: &str) -> String {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("aaf-{tag}-{target}.{ext}")
}

pub fn validate_repo_slug(repo: &str) -> Result<(), UpdateError> {
    let ok = |p: &str| {
        !p.is_empty()
            && !p.contains("..")
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    match repo.split_once('/') {
        Some((o, n)) if !n.contains('/') && ok(o) && ok(n) => Ok(()),
        _ => Err(UpdateError::InvalidRepo(format!("'{repo}'"))),
    }
}

pub fn validate_release_url(url: &str) -> Result<(), UpdateError> {
    let bad = |why: &str| UpdateError::InvalidUrl(format!("'{url}': {why}"));
    let uri: ureq::http::Uri = url.trim().parse().map_err(|_| bad("not a valid URL"))?;
    if uri.scheme_str() != Some("https") {
        return Err(bad("only https URLs are permitted"));
    }
    let auth = uri.authority().ok_or_else(|| bad("missing host"))?;
    if auth.as_str().contains('@') {
        return Err(bad("userinfo forbidden"));
    }
    let host = auth.host().to_ascii_lowercase();
    let ok = [
        "github.com",
        "api.github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
        "github-releases.githubusercontent.com",
    ]
    .contains(&host.as_str());
    if !ok {
        return Err(bad("host not allowlisted"));
    }
    Ok(())
}

/// Parse `SHA256SUMS.txt` (`<64-hex>  <file>` or `<64-hex> *<file>`), failing closed.
pub fn parse_sha256sums(content: &str, filename: &str) -> Result<String, UpdateError> {
    let bad = |why: &str| UpdateError::MalformedChecksum(format!("'{filename}': {why}"));
    if filename.is_empty()
        || filename.contains(['/', '\\'])
        || filename.chars().any(char::is_whitespace)
    {
        return Err(bad("invalid target filename"));
    }
    let mut found: Option<String> = None;
    for line in content.lines().map(str::trim) {
        let Some((digest, rest)) = line.find(char::is_whitespace).map(|i| line.split_at(i)) else {
            continue;
        };
        let rest = rest.trim_start();
        if line.starts_with('#') || rest.strip_prefix('*').unwrap_or(rest) != filename {
            continue;
        }
        if !is_sha256_hex(digest) {
            return Err(bad("digest must be 64 hex chars"));
        }
        let norm = digest.to_ascii_lowercase();
        if found.as_ref().is_some_and(|prev| prev != &norm) {
            return Err(bad("conflicting duplicate entries"));
        }
        found = Some(norm);
    }
    found.ok_or_else(|| UpdateError::MissingChecksum(filename.to_string()))
}

/// Compute `Sha256::digest(bytes)` and compare against `expected_hex` in constant time.
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<String, UpdateError> {
    if !is_sha256_hex(expected_hex) {
        return Err(UpdateError::MalformedChecksum(
            "expected 64 hex chars".into(),
        ));
    }
    let expected = expected_hex.to_ascii_lowercase();
    let actual = encode_lower_hex(Sha256::digest(bytes).as_slice());
    let diff = actual
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |a, (x, y)| a | (x ^ y));
    if actual.len() != expected.len() || diff != 0 {
        return Err(UpdateError::ChecksumMismatch { expected, actual });
    }
    Ok(actual)
}

#[derive(Debug, Clone)]
pub struct HttpReleaseSource {
    pub connect_timeout: Duration,
    pub global_timeout: Duration,
}

impl Default for HttpReleaseSource {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            global_timeout: Duration::from_secs(120),
        }
    }
}

impl HttpReleaseSource {
    fn get_validated(
        &self,
        initial: &str,
        accept: &str,
        limit: usize,
    ) -> Result<Vec<u8>, UpdateError> {
        let mut url = initial.to_string();
        let cfg = ureq::Agent::config_builder()
            .timeout_connect(Some(self.connect_timeout))
            .timeout_global(Some(self.global_timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build();
        let agent = cfg.new_agent();
        for hop in 0..=5 {
            validate_release_url(&url)?;
            let resp = agent
                .get(&url)
                .header("User-Agent", "aaf-updater")
                .header("Accept", accept)
                .call()
                .map_err(|e| UpdateError::Transport(format!("{url}: {e}")))?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let loc = resp.headers().get("location").and_then(|v| v.to_str().ok());
                let Some(next) = loc.filter(|_| hop < 5) else {
                    return Err(UpdateError::Transport(format!("{url}: redirect rejected")));
                };
                validate_release_url(next)?;
                url = next.to_string();
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(UpdateError::HttpStatus { url, status });
            }
            let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
            let mut buf = Vec::new();
            resp.into_body()
                .into_reader()
                .take(cap)
                .read_to_end(&mut buf)
                .map_err(|e| UpdateError::Transport(format!("{url}: read {:?}", e.kind())))?;
            return if buf.len() > limit {
                Err(UpdateError::SizeLimitExceeded { url, limit })
            } else {
                Ok(buf)
            };
        }
        Err(UpdateError::Transport(format!("{url}: redirect limit")))
    }
}

impl ReleaseSource for HttpReleaseSource {
    fn latest_tag(&self, repo: &str) -> Result<String, UpdateError> {
        validate_repo_slug(repo)?;
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let bytes = self.get_validated(&url, "application/vnd.github+json", MAX_TEXT_BYTES)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpdateError::InvalidMetadata(format!("latest release JSON: {e}")))?;
        v.get("tag_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| UpdateError::InvalidMetadata("missing tag_name".into()))
    }

    fn fetch_text(&self, url: &str) -> Result<String, UpdateError> {
        let bytes = self.get_validated(url, "text/plain, */*", MAX_TEXT_BYTES)?;
        String::from_utf8(bytes).map_err(|_| UpdateError::InvalidMetadata("non-UTF-8".into()))
    }

    fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
        self.get_validated(
            url,
            "application/octet-stream",
            max_bytes.min(MAX_ARCHIVE_BYTES),
        )
    }
}

pub fn check_for_update(
    source: &dyn ReleaseSource,
    repo: &str,
    current_version: &str,
    requested_tag: Option<&str>,
    target: &str,
) -> Result<UpdatePlan, UpdateError> {
    validate_repo_slug(repo)?;
    if !matches!(
        target,
        "aarch64-apple-darwin"
            | "x86_64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-pc-windows-msvc"
    ) {
        return Err(UpdateError::UnsupportedTarget(target.to_string()));
    }
    let current_version = parse_semver(current_version)?;
    let raw_tag = match requested_tag {
        Some(tag) => tag.trim().to_string(),
        None => source.latest_tag(repo)?,
    };
    let target_version = parse_semver(&raw_tag)?;
    let tag = format!("v{target_version}");
    let archive_name = archive_filename(&tag, target);
    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    let archive_url = format!("{base}/{archive_name}");
    let checksums_url = format!("{base}/{SHA256SUMS_FILENAME}");
    validate_release_url(&archive_url)?;
    validate_release_url(&checksums_url)?;
    Ok(UpdatePlan {
        repo: repo.into(),
        target: target.into(),
        current_version,
        target_version,
        tag,
        is_newer: target_version > current_version,
        is_same: target_version == current_version,
        archive_name,
        archive_url,
        checksums_url,
    })
}

pub fn download_and_verify(
    source: &dyn ReleaseSource,
    plan: &UpdatePlan,
) -> Result<VerifiedArchive, UpdateError> {
    validate_release_url(&plan.checksums_url)?;
    validate_release_url(&plan.archive_url)?;
    let expected = parse_sha256sums(&source.fetch_text(&plan.checksums_url)?, &plan.archive_name)?;
    let bytes = source.fetch_bytes(&plan.archive_url, MAX_ARCHIVE_BYTES)?;
    let sha256 = verify_sha256(&bytes, &expected)?;
    Ok(VerifiedArchive {
        plan: plan.clone(),
        sha256,
        bytes,
    })
}

pub fn install_verified_binaries(
    archive: &VerifiedArchive,
    install_dir: &Path,
) -> Result<InstallReceipt, UpdateError> {
    verify_sha256(&archive.bytes, &archive.sha256)?;
    let temp = create_temp_dir()?;
    let archive_path = temp.0.join(&archive.plan.archive_name);
    let extract_dir = temp.0.join("extract");
    fs::write(&archive_path, &archive.bytes).map_err(|e| io_err("write archive", e))?;
    fs::create_dir(&extract_dir).map_err(|e| io_err("create extract dir", e))?;
    let is_zip = archive.plan.archive_name.ends_with(".zip");
    extract_archive(&archive_path, &extract_dir, is_zip)?;
    let bins = if is_zip {
        ["aaf.exe", "aaf-hook.exe"]
    } else {
        ["aaf", "aaf-hook"]
    };
    let pkg = format!("aaf-{}-{}", archive.plan.tag, archive.plan.target);
    let canon = fs::canonicalize(&extract_dir).map_err(|e| io_err("canonicalize", e))?;
    let found: Vec<_> = bins
        .into_iter()
        .map(|n| Ok((n, locate_verified_binary(&extract_dir, &canon, &pkg, n)?)))
        .collect::<Result<_, UpdateError>>()?;
    fs::create_dir_all(install_dir).map_err(|e| io_err("create install dir", e))?;
    Ok(InstallReceipt {
        tag: archive.plan.tag.clone(),
        target: archive.plan.target.clone(),
        sha256: archive.sha256.clone(),
        install_dir: install_dir.to_path_buf(),
        installed_binaries: install_pair_atomically(install_dir, &found)?,
    })
}

fn extract_archive(archive: &Path, dest: &Path, is_zip: bool) -> Result<(), UpdateError> {
    let ps = "Expand-Archive -LiteralPath $env:AAF_ARCHIVE -DestinationPath $env:AAF_DEST -Force";
    let out = if is_zip {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", ps])
            .env("AAF_ARCHIVE", archive)
            .env("AAF_DEST", dest)
            .output()
    } else {
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .output()
    }
    .map_err(|e| UpdateError::ExtractFailed(format!("spawn: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        return Err(UpdateError::ExtractFailed(if msg.is_empty() {
            format!("status {}", out.status)
        } else {
            msg.to_string()
        }));
    }
    Ok(())
}

fn locate_verified_binary(
    extract_dir: &Path,
    canon_extract: &Path,
    pkg_dir: &str,
    name: &str,
) -> Result<PathBuf, UpdateError> {
    let bad = |why: &str| UpdateError::UnsafeArchiveEntry(format!("'{name}': {why}"));
    let path = [extract_dir.join(pkg_dir).join(name), extract_dir.join(name)]
        .into_iter()
        .find(|p| fs::symlink_metadata(p).is_ok())
        .ok_or_else(|| bad("missing"))?;
    let meta = fs::symlink_metadata(&path).map_err(|e| io_err("inspect", e))?;
    if !meta.file_type().is_file() {
        return Err(bad("not a regular file"));
    }
    let canon = fs::canonicalize(&path).map_err(|e| io_err("canonicalize", e))?;
    if !canon.starts_with(canon_extract) {
        return Err(bad("escaped extract dir"));
    }
    Ok(path)
}

fn install_pair_atomically(
    dir: &Path,
    bins: &[(&str, PathBuf)],
) -> Result<Vec<PathBuf>, UpdateError> {
    let nonce = unique_nonce();
    let mut staged = Vec::with_capacity(bins.len());
    for (name, src) in bins {
        let tmp = dir.join(format!(".{name}.new.{nonce}"));
        if let Err(err) = copy_executable(src, &tmp) {
            staged.iter().for_each(|(s, _, _)| drop(fs::remove_file(s)));
            drop(fs::remove_file(&tmp));
            return Err(err);
        }
        staged.push((
            tmp,
            dir.join(name),
            dir.join(format!(".{name}.bak.{nonce}")),
        ));
    }
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::with_capacity(staged.len());
    for (i, (tmp, dst, bak)) in staged.iter().enumerate() {
        let had = fs::symlink_metadata(dst).is_ok();
        let step = if had { fs::rename(dst, bak) } else { Ok(()) }.and_then(|()| {
            fs::rename(tmp, dst).inspect_err(|_| {
                if had {
                    drop(fs::rename(bak, dst));
                }
            })
        });
        if let Err(e) = step {
            staged[i..]
                .iter()
                .for_each(|(t, _, _)| drop(fs::remove_file(t)));
            for (d, b) in done.iter().rev() {
                drop(
                    b.as_ref()
                        .map_or_else(|| fs::remove_file(d), |bak| fs::rename(bak, d)),
                );
            }
            return Err(io_err("install binary", e));
        }
        done.push((dst.clone(), had.then(|| bak.clone())));
    }
    done.iter()
        .filter_map(|(_, b)| b.as_ref())
        .for_each(|b| drop(fs::remove_file(b)));
    Ok(done.into_iter().map(|(dst, _)| dst).collect())
}

fn copy_executable(src: &Path, dst: &Path) -> Result<(), UpdateError> {
    fs::copy(src, dst).map_err(|e| io_err("copy staged", e))?;
    File::options()
        .write(true)
        .open(dst)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_err("sync", e))?;
    #[cfg(unix)]
    fs::set_permissions(dst, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| io_err("chmod", e))?;
    Ok(())
}

fn create_temp_dir() -> Result<TempDirGuard, UpdateError> {
    for _ in 0..16 {
        let path = std::env::temp_dir().join(format!("aaf-upgrade-{}", unique_nonce()));
        let mut b = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        match b.create(&path) {
            Ok(()) => return Ok(TempDirGuard(path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err("create temp dir", e)),
        }
    }
    Err(UpdateError::Io("exhausted temp dir attempts".into()))
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn io_err(ctx: &str, e: impl fmt::Display) -> UpdateError {
    UpdateError::Io(format!("{ctx}: {e}"))
}

fn unique_nonce() -> String {
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{seq}-{:016x}",
        std::process::id(),
        RandomState::new().build_hasher().finish()
    )
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
