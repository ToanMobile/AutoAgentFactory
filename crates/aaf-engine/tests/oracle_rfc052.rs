#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for RFC-052: minisign verification of `SHA256SUMS.txt` and 40-char SHA pinning.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::updater::{
    DEFAULT_REPO, ReleaseSource, TRUSTED_RELEASE_PUBKEYS, UpdateError, check_for_update,
    download_and_verify, download_and_verify_with_keys, upgrade_with_source,
    upgrade_with_source_and_keys, verify_sums_signature,
};
use sha2::{Digest, Sha256};

const TEST_PUBKEY_1: &str = "RWQBAgMEBQYHCNAlIrEywCYMycwLQ7sVFGdhwN1WS9nRZNDEBxQG9gsE";
const TEST_PUBKEY_2: &str = "RWQREhMUFRYXGH4NUdwV3pye/gRA+XwTNR6czp5vDTje8vt6Ytssyihl";

const ORACLE_PAYLOAD: &[u8] = b"rfc052-verified-release-archive";
const ORACLE_HASH: &str = "8927477d213611b27d4ec305ea21dff0287d44a0e73a097ed085eba3298b98a0";
const ORACLE_TARGET: &str = "x86_64-unknown-linux-gnu";
const ORACLE_TAG: &str = "v1.2.3";
const ORACLE_SUMS: &str = "8927477d213611b27d4ec305ea21dff0287d44a0e73a097ed085eba3298b98a0  aaf-v1.2.3-x86_64-unknown-linux-gnu.tar.gz\n";

const ORACLE_SIG_KEY1: &str = "\
untrusted comment: signature from aaf test key 0102030405060708
RUQBAgMEBQYHCGZ+8PdMRBJa6QrW3C+bIiCezNfJoBiF4VcY1+ww1EQ+OkUkWRr9yzhRDzk+LYd3GQWcmEeuKjUNDK9Gnhfu3wE=
trusted comment: timestamp:1700000000\tfile:SHA256SUMS.txt\tprehashed
ukK4VPtkyfzPSGpkG8fBye53iodHqEztT8RQbGsnZ6w4mCJl6YuIm+xbuf8h5LnDaBwovHqtIMo9y0dSL2KYDQ==
";

const ORACLE_SIG_KEY2: &str = "\
untrusted comment: signature from aaf test key 1112131415161718
RUQREhMUFRYXGNaXdnzrDLcNLdh5m9ro/NcffIUnVk+zsWRXS1v/fNmawc17A+mY02vl9jV+FBvLdSHpuzmsnwHBVSpAOZOdxw0=
trusted comment: timestamp:1700000000\tfile:SHA256SUMS.txt\tprehashed
mCeLYztJ/rmgZVHJiSlKfDcQq850cTUQKMZNzs5uxVyNkcMNFbOMiuN+oeJEUDP7ZGT3S723gfNYtCR371ZNDQ==
";

struct MockSource {
    latest: String,
    texts: BTreeMap<String, String>,
    blobs: BTreeMap<String, Vec<u8>>,
}

impl MockSource {
    fn new(latest: &str) -> Self {
        Self {
            latest: latest.to_string(),
            texts: BTreeMap::new(),
            blobs: BTreeMap::new(),
        }
    }

    fn with_text(mut self, url: impl Into<String>, text: impl Into<String>) -> Self {
        self.texts.insert(url.into(), text.into());
        self
    }

    fn with_blob(mut self, url: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.blobs.insert(url.into(), bytes);
        self
    }
}

impl ReleaseSource for MockSource {
    fn latest_tag(&self, _repo: &str) -> Result<String, UpdateError> {
        Ok(self.latest.clone())
    }

    fn fetch_text(&self, url: &str) -> Result<String, UpdateError> {
        self.texts
            .get(url)
            .cloned()
            .ok_or_else(|| UpdateError::HttpStatus {
                url: url.to_string(),
                status: 404,
            })
    }

    fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
        let data = self
            .blobs
            .get(url)
            .cloned()
            .ok_or_else(|| UpdateError::HttpStatus {
                url: url.to_string(),
                status: 404,
            })?;
        if data.len() > max_bytes {
            return Err(UpdateError::SizeLimitExceeded {
                url: url.to_string(),
                limit: max_bytes,
            });
        }
        Ok(data)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// (a) Valid minisign Ed25519 signature verifies cleanly and unlocks download_and_verify.
#[test]
fn valid_minisign_signature_verifies_and_downloads() {
    assert_eq!(sha256_hex(ORACLE_PAYLOAD), ORACLE_HASH);
    verify_sums_signature(ORACLE_SUMS.as_bytes(), ORACLE_SIG_KEY1, &[TEST_PUBKEY_1]).unwrap();

    let plan = check_for_update(
        &MockSource::new(ORACLE_TAG),
        DEFAULT_REPO,
        "0.1.0",
        None,
        ORACLE_TARGET,
    )
    .unwrap();
    let source = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), ORACLE_SUMS)
        .with_text(plan.checksums_sig_url.clone(), ORACLE_SIG_KEY1)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());

    let verified = download_and_verify_with_keys(&source, &plan, &[TEST_PUBKEY_1]).unwrap();
    assert_eq!(verified.sha256, ORACLE_HASH);
    assert_eq!(verified.bytes, ORACLE_PAYLOAD);
}

/// (b) Flipping even 1 byte in SHA256SUMS.txt fails closed with VerificationFailed.
#[test]
fn single_byte_tamper_in_sha256sums_fails_verification() {
    let mut tampered = ORACLE_SUMS.as_bytes().to_vec();
    tampered[0] = if tampered[0] == b'8' { b'9' } else { b'8' };

    assert!(matches!(
        verify_sums_signature(&tampered, ORACLE_SIG_KEY1, &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));

    let plan = check_for_update(
        &MockSource::new(ORACLE_TAG),
        DEFAULT_REPO,
        "0.1.0",
        None,
        ORACLE_TARGET,
    )
    .unwrap();
    let tampered_text = String::from_utf8(tampered).unwrap();
    let source = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), tampered_text)
        .with_text(plan.checksums_sig_url.clone(), ORACLE_SIG_KEY1)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());

    assert!(matches!(
        download_and_verify_with_keys(&source, &plan, &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));
}

/// (c) Missing SHA256SUMS.txt.minisig fails closed with VerificationFailed (never unsigned fallback).
#[test]
fn missing_minisig_asset_fails_closed() {
    let plan = check_for_update(
        &MockSource::new(ORACLE_TAG),
        DEFAULT_REPO,
        "0.1.0",
        None,
        ORACLE_TARGET,
    )
    .unwrap();
    let source = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), ORACLE_SUMS)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());

    assert!(matches!(
        download_and_verify_with_keys(&source, &plan, &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));
    assert!(matches!(
        download_and_verify(&source, &plan),
        Err(UpdateError::VerificationFailed(_))
    ));
}

/// (d) Wrong public key or empty trusted_pubkeys fails closed; key #2 in rotation slice succeeds.
#[test]
fn wrong_key_and_empty_keys_fail_closed_and_key_rotation_succeeds() {
    // Production trusted keys are bootstrapped with a valid Ed25519 minisign key,
    // empty slice fails closed, and untrusted test-key signatures are rejected.
    assert!(!TRUSTED_RELEASE_PUBKEYS.is_empty());
    assert!(
        TRUSTED_RELEASE_PUBKEYS
            .iter()
            .all(|k| k.starts_with("RW") && minisign_verify::PublicKey::from_base64(k).is_ok())
    );
    assert!(matches!(
        verify_sums_signature(ORACLE_SUMS.as_bytes(), ORACLE_SIG_KEY1, &[]),
        Err(UpdateError::VerificationFailed(_))
    ));
    assert!(matches!(
        verify_sums_signature(
            ORACLE_SUMS.as_bytes(),
            ORACLE_SIG_KEY1,
            TRUSTED_RELEASE_PUBKEYS
        ),
        Err(UpdateError::VerificationFailed(_))
    ));

    // Blank / whitespace-only signature.
    assert!(matches!(
        verify_sums_signature(ORACLE_SUMS.as_bytes(), "   \n", &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));

    // Wrong public key (key 1 cannot verify signature made by key 2, and vice versa).
    assert!(matches!(
        verify_sums_signature(ORACLE_SUMS.as_bytes(), ORACLE_SIG_KEY2, &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));
    assert!(matches!(
        verify_sums_signature(ORACLE_SUMS.as_bytes(), ORACLE_SIG_KEY1, &[TEST_PUBKEY_2]),
        Err(UpdateError::VerificationFailed(_))
    ));

    // Smooth key rotation: signature by key #2 verifies against &[KEY_1, KEY_2].
    verify_sums_signature(
        ORACLE_SUMS.as_bytes(),
        ORACLE_SIG_KEY2,
        &[TEST_PUBKEY_1, TEST_PUBKEY_2],
    )
    .unwrap();

    let plan = check_for_update(
        &MockSource::new(ORACLE_TAG),
        DEFAULT_REPO,
        "0.1.0",
        None,
        ORACLE_TARGET,
    )
    .unwrap();
    let source = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), ORACLE_SUMS)
        .with_text(plan.checksums_sig_url.clone(), ORACLE_SIG_KEY2)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());

    let verified =
        download_and_verify_with_keys(&source, &plan, &[TEST_PUBKEY_1, TEST_PUBKEY_2]).unwrap();
    assert_eq!(verified.sha256, ORACLE_HASH);
}

/// (e) Custom fork `--repo custom/fork` still requires valid signature — never falls back unsigned.
#[test]
fn custom_fork_repo_refuses_missing_or_invalid_minisig() {
    let tmp = tempfile::tempdir().unwrap();
    let fork_repo = "custom/fork";
    let plan = check_for_update(
        &MockSource::new(ORACLE_TAG),
        fork_repo,
        "0.1.0",
        None,
        ORACLE_TARGET,
    )
    .unwrap();
    assert!(plan.checksums_sig_url.contains("github.com/custom/fork/"));

    // Fork missing `.minisig`
    let unsigned_fork = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), ORACLE_SUMS)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());
    assert!(matches!(
        upgrade_with_source_and_keys(
            &unsigned_fork,
            fork_repo,
            "0.1.0",
            None,
            ORACLE_TARGET,
            tmp.path(),
            &[TEST_PUBKEY_1],
        ),
        Err(UpdateError::VerificationFailed(_))
    ));
    assert!(matches!(
        upgrade_with_source(
            &unsigned_fork,
            fork_repo,
            "0.1.0",
            None,
            ORACLE_TARGET,
            tmp.path(),
        ),
        Err(UpdateError::VerificationFailed(_))
    ));

    // Fork with untrusted / forged signature (signed by key 2 when only key 1 is trusted)
    let bad_sig_fork = MockSource::new(ORACLE_TAG)
        .with_text(plan.checksums_url.clone(), ORACLE_SUMS)
        .with_text(plan.checksums_sig_url.clone(), ORACLE_SIG_KEY2)
        .with_blob(plan.archive_url.clone(), ORACLE_PAYLOAD.to_vec());
    assert!(matches!(
        upgrade_with_source_and_keys(
            &bad_sig_fork,
            fork_repo,
            "0.1.0",
            None,
            ORACLE_TARGET,
            tmp.path(),
            &[TEST_PUBKEY_1],
        ),
        Err(UpdateError::VerificationFailed(_))
    ));
}

/// (f) Every `uses:` in release.yml and ci.yml is pinned to a 40-char hex commit SHA,
/// and release.yml signs & publishes `dist/SHA256SUMS.txt.minisig` under `environment: release`.
#[test]
fn workflow_actions_pinned_to_40_char_sha_and_release_publishes_minisig() {
    for rel in [".github/workflows/release.yml", ".github/workflows/ci.yml"] {
        let path = repo_root().join(rel);
        let text = fs::read_to_string(&path).unwrap();
        let mut uses_count = 0usize;
        let mut unpinned = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            let Some(rest) = trimmed
                .strip_prefix("uses:")
                .or_else(|| trimmed.strip_prefix("- uses:"))
            else {
                continue;
            };
            uses_count += 1;
            let spec = rest.split('#').next().unwrap_or(rest).trim();
            let spec = spec.trim_matches(['\'', '"']);
            let is_pinned = spec.split_once('@').is_some_and(|(action, rev)| {
                !action.is_empty()
                    && rev.len() == 40
                    && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            });
            if !is_pinned {
                unpinned.push(format!("{rel}:{}: {spec}", idx + 1));
            }
        }
        assert!(uses_count > 0, "expected at least one `uses:` in {rel}");
        assert!(
            unpinned.is_empty(),
            "RFC-052: unpinned GitHub Actions in {rel}:\n{}",
            unpinned.join("\n")
        );
    }

    let release_yml =
        fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    assert!(
        release_yml.contains("environment: release"),
        "RFC-052: publish job must require protected `environment: release`"
    );
    assert!(
        release_yml.contains("MINISIGN_SECRET_KEY"),
        "RFC-052: release.yml must sign SHA256SUMS.txt using MINISIGN_SECRET_KEY"
    );
    assert!(
        release_yml.contains("dist/SHA256SUMS.txt.minisig"),
        "RFC-052: release.yml must publish dist/SHA256SUMS.txt.minisig"
    );
}
