//! Cryptographic release verification: minisign signature verification for
//! `SHA256SUMS.txt` and constant-time SHA-256 verification for release archives.

use std::fmt::Write as _;

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use super::UpdateError;

pub const SHA256SUMS_FILENAME: &str = "SHA256SUMS.txt";
pub const SHA256SUMS_SIG_FILENAME: &str = "SHA256SUMS.txt.minisig";

/// Trusted minisign public keys (`RW...` base64 strings) authorized to sign
/// release `SHA256SUMS.txt` manifests.
///
/// The slice supports smooth key rotation: append a new public key alongside
/// the active key before rotating the signing key in CI, then remove the old
/// key once all supported releases are signed by the new key.
///
/// # Maintainer key bootstrap & rotation
///
/// 1. Generate a signing keypair offline (use `-W` for an unencrypted CI key,
///    or set both `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD`):
///    ```text
///    minisign -G -p aaf-release.pub -s aaf-release.key
///    ```
/// 2. Copy the second line (`RW...`) of `aaf-release.pub` into this slice.
/// 3. In GitHub repository settings, create a protected Environment named
///    `release` (restricted to release tags) and store the contents of
///    `aaf-release.key` in secret `MINISIGN_SECRET_KEY` (plus optional
///    `MINISIGN_PASSWORD` if encrypted).
///
/// When this slice is empty, verification fails closed and refuses all remote
/// upgrades — never falling back to unsigned releases (even with `--repo`).
pub const TRUSTED_RELEASE_PUBKEYS: &[&str] = &[];

/// Verify that `sig_text` is a valid, non-legacy (pre-hashed) minisign
/// signature over `sums_bytes` produced by at least one key in
/// `trusted_pubkeys`.
///
/// Fails closed if `trusted_pubkeys` is empty, `sig_text` is blank/malformed,
/// the signature uses legacy mode, or no trusted key verifies the signature.
pub fn verify_sums_signature(
    sums_bytes: &[u8],
    sig_text: &str,
    trusted_pubkeys: &[&str],
) -> Result<(), UpdateError> {
    if trusted_pubkeys.is_empty() {
        return Err(UpdateError::VerificationFailed(
            "no trusted release public keys configured; refusing unverified release".into(),
        ));
    }
    if sig_text.trim().is_empty() {
        return Err(UpdateError::VerificationFailed(format!(
            "empty {SHA256SUMS_SIG_FILENAME} signature"
        )));
    }
    let sig = Signature::decode(sig_text).map_err(|err| {
        UpdateError::VerificationFailed(format!("invalid {SHA256SUMS_SIG_FILENAME} format: {err}"))
    })?;

    let mut last_err = String::from("no trusted public key matched");
    for raw_pk in trusted_pubkeys {
        let pk = match PublicKey::from_base64(raw_pk.trim()) {
            Ok(pk) => pk,
            Err(err) => {
                last_err = format!("malformed trusted public key: {err}");
                continue;
            }
        };
        match pk.verify(sums_bytes, &sig, false) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err.to_string(),
        }
    }

    Err(UpdateError::VerificationFailed(format!(
        "{SHA256SUMS_FILENAME} signature rejected by all {} trusted key(s): {last_err}",
        trusted_pubkeys.len()
    )))
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

pub(super) fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn encode_lower_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
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
