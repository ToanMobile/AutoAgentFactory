#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use sha2::{Digest, Sha256};

use super::*;

struct FakeReleaseSource {
    latest: Result<String, UpdateError>,
    texts: BTreeMap<String, String>,
    blobs: BTreeMap<String, Vec<u8>>,
}

impl FakeReleaseSource {
    fn new(latest: &str) -> Self {
        Self {
            latest: Ok(latest.to_string()),
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

impl ReleaseSource for FakeReleaseSource {
    fn latest_tag(&self, _repo: &str) -> Result<String, UpdateError> {
        self.latest.clone()
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
    verify::encode_lower_hex(Sha256::digest(bytes).as_slice())
}

const TEST_PUBKEY_1: &str = "RWQBAgMEBQYHCNAlIrEywCYMycwLQ7sVFGdhwN1WS9nRZNDEBxQG9gsE";

const SIG_GOOD: &str = "\
untrusted comment: signature from aaf test key 0102030405060708
RUQBAgMEBQYHCF4pUq8WYe/jdS9q29gPEL682HwdHdR/d/+1A+2XF+oWyj7QQvbfHB8Ih7pBDbojG1AMa91jyO2yltOjfp/02gY=
trusted comment: timestamp:1700000000\tfile:SHA256SUMS.txt\tprehashed
BzEI/by3DWICY1pCXjR07idOnAWlMW9zYyf0ZxMZyQw8mTen5Xmj7imbq/nxqw+pgF35ANOSjgcrikmI42tHBw==
";

const SIG_OTHER: &str = "\
untrusted comment: signature from aaf test key 0102030405060708
RUQBAgMEBQYHCFBBp+WAFEdnLec0S5Fu/lEgBbnr77zl/grQlzjqlMBHOTOo1VXPBv/D9zdctaVEgWbfnO8jva/mFjFEopNQmQ0=
trusted comment: timestamp:1700000000\tfile:SHA256SUMS.txt\tprehashed
XpGIjsy5BC7EUdIGLDwY6A62pt6719FlbeoQ/TWkwiaqbweOw/0nzLIH4L1fHgl6xlPaRqT6qzN893RzVTivDQ==
";

const SIG_E2E: &str = "\
untrusted comment: signature from aaf test key 0102030405060708
RUQBAgMEBQYHCD2d4puXqZM8bRToCBoejsqCkW7qBohy1RW/r5/Txp4OleC6egVORCuvOEdZMGuUycI9tg8irczwXhi65IhF0wc=
trusted comment: timestamp:1700000000\tfile:SHA256SUMS.txt\tprehashed
tlifJCwf3o+2qCwVcqjWR51dfTcpu1puQ5ZH8jzJGi0pvn1fJk5ImiWy6F2a32aCwaS872+dQN4BfiCf0rBZBQ==
";

// Deterministic tar.gz containing `aaf-v0.9.0-aarch64-apple-darwin/{aaf,aaf-hook}`.
#[cfg(unix)]
const E2E_TARBALL_HEX: &str = concat!(
    "1f8b08000000000000ffedd4cd0a8240148661d75e85d17af4988ed1e59c4c99",
    "2832b49fdb6f7213d8c240b082f7d9cc309b6fe0cc37aab5b949bc89c5a8b6a5",
    "2b72a3e7f3b1323b6deffb53a25a075389b7b6b65fbde12a92a5affdf33ccd0a",
    "5905914c4efec0b5bb68ebe3e7c8fa41cb45b2f563ee5c5895ae89fcb8a3fe35",
    "84dfbe1866a1e3fd37ae690e5332c6fb5f0cfa9fdbdcd2ff39bcf7bf1f379f00",
    "0000000000000000000000c09f78003ca73eb300280000",
);

#[cfg(unix)]
fn decode_hex(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn semver_parsing_and_ordering() {
    let v010 = parse_semver("v0.1.0").unwrap();
    let v020 = parse_semver("0.2.0").unwrap();
    let v100 = parse_semver("  v1.0.0\n").unwrap();
    let v1010 = parse_semver("v1.0.10").unwrap();

    assert_eq!(v010.to_string(), "0.1.0");
    assert!(v010 < v020);
    assert!(v020 < v100);
    assert!(v100 < v1010);

    for bad in [
        "",
        "   ",
        "v1.0",
        "1.0.0.0",
        "v1.0.0-rc1",
        "v1.0.0+build.1",
        "vv1.0.0",
        "v1.-2.0",
        "v1.a.0",
        "v1.0.0/../evil",
        "v99999999999999999999999999.0.0",
    ] {
        assert!(
            matches!(parse_semver(bad), Err(UpdateError::InvalidSemVer(_))),
            "expected InvalidSemVer for {bad:?}"
        );
    }
}

#[test]
fn target_triple_maps_all_five_targets_and_rejects_unknown() {
    assert_eq!(
        target_triple("macos", "aarch64").unwrap(),
        "aarch64-apple-darwin"
    );
    assert_eq!(
        target_triple("macos", "x86_64").unwrap(),
        "x86_64-apple-darwin"
    );
    assert_eq!(
        target_triple("linux", "x86_64").unwrap(),
        "x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        target_triple("linux", "aarch64").unwrap(),
        "aarch64-unknown-linux-gnu"
    );
    assert_eq!(
        target_triple("windows", "x86_64").unwrap(),
        "x86_64-pc-windows-msvc"
    );

    assert!(matches!(
        target_triple("freebsd", "x86_64"),
        Err(UpdateError::UnsupportedTarget(_))
    ));
    assert!(matches!(
        target_triple("windows", "aarch64"),
        Err(UpdateError::UnsupportedTarget(_))
    ));
    assert!(current_target_triple().is_ok());
}

#[test]
fn archive_filename_selects_zip_for_windows_and_tar_gz_for_unix() {
    assert_eq!(
        archive_filename("v0.5.0", "aarch64-apple-darwin"),
        "aaf-v0.5.0-aarch64-apple-darwin.tar.gz"
    );
    assert_eq!(
        archive_filename("v0.5.0", "x86_64-unknown-linux-gnu"),
        "aaf-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
    );
    assert_eq!(
        archive_filename("v0.5.0", "x86_64-pc-windows-msvc"),
        "aaf-v0.5.0-x86_64-pc-windows-msvc.zip"
    );
}

#[test]
fn release_url_and_repo_validation_fail_closed() {
    for good in [
        "https://api.github.com/repos/ToanMobile/AutoAgentFactory/releases/latest",
        "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v0.2.0/SHA256SUMS.txt",
        "https://objects.githubusercontent.com/github-production-release-asset-2e65be/abc",
        "https://release-assets.githubusercontent.com/asset/123",
    ] {
        assert!(validate_release_url(good).is_ok(), "should allow {good}");
    }

    for bad in [
        "http://github.com/ToanMobile/AutoAgentFactory/releases/download/v0.2.0/SHA256SUMS.txt",
        "https://evil.example.com/aaf.tar.gz",
        "https://user:pass@github.com/ToanMobile/AutoAgentFactory",
        "file:///etc/passwd",
        "not a url",
    ] {
        assert!(
            matches!(validate_release_url(bad), Err(UpdateError::InvalidUrl(_))),
            "should reject {bad}"
        );
    }

    assert!(validate_repo_slug("ToanMobile/AutoAgentFactory").is_ok());
    for bad_repo in ["no-slash", "a/b/c", "../evil", "owner/repo name", "/repo"] {
        assert!(matches!(
            validate_repo_slug(bad_repo),
            Err(UpdateError::InvalidRepo(_))
        ));
    }
}

#[test]
fn parse_sha256sums_handles_valid_binary_prefix_and_rejects_bad_entries() {
    let hash_a = "A".repeat(64);
    let hash_b = "0123456789abcdef".repeat(4);
    let manifest = format!(
        "# release sums\n\n{hash_a}  aaf-v0.2.0-aarch64-apple-darwin.tar.gz\n{hash_b} *aaf-v0.2.0-x86_64-pc-windows-msvc.zip\n"
    );

    assert_eq!(
        parse_sha256sums(&manifest, "aaf-v0.2.0-aarch64-apple-darwin.tar.gz").unwrap(),
        "a".repeat(64)
    );
    assert_eq!(
        parse_sha256sums(&manifest, "aaf-v0.2.0-x86_64-pc-windows-msvc.zip").unwrap(),
        hash_b
    );

    assert!(matches!(
        parse_sha256sums(&manifest, "aaf-v0.2.0-x86_64-apple-darwin.tar.gz"),
        Err(UpdateError::MissingChecksum(_))
    ));

    let short_hash = "deadbeef  aaf-v0.2.0-aarch64-apple-darwin.tar.gz\n";
    assert!(matches!(
        parse_sha256sums(short_hash, "aaf-v0.2.0-aarch64-apple-darwin.tar.gz"),
        Err(UpdateError::MalformedChecksum(_))
    ));

    let non_hex = format!(
        "{}  aaf-v0.2.0-aarch64-apple-darwin.tar.gz\n",
        "z".repeat(64)
    );
    assert!(matches!(
        parse_sha256sums(&non_hex, "aaf-v0.2.0-aarch64-apple-darwin.tar.gz"),
        Err(UpdateError::MalformedChecksum(_))
    ));

    let conflicting = format!(
        "{}  aaf-v0.2.0-aarch64-apple-darwin.tar.gz\n{}  aaf-v0.2.0-aarch64-apple-darwin.tar.gz\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    assert!(matches!(
        parse_sha256sums(&conflicting, "aaf-v0.2.0-aarch64-apple-darwin.tar.gz"),
        Err(UpdateError::MalformedChecksum(_))
    ));
}

#[test]
fn verify_sha256_passes_exact_and_fails_on_single_bit_tamper() {
    let payload = b"autoagentfactory-verified-release-archive";
    let expected = sha256_hex(payload);
    assert_eq!(
        verify_sha256(payload, &expected.to_uppercase()).unwrap(),
        expected
    );

    let mut tampered = payload.to_vec();
    tampered[0] ^= 0x01;
    assert!(matches!(
        verify_sha256(&tampered, &expected),
        Err(UpdateError::ChecksumMismatch { .. })
    ));

    assert!(matches!(
        verify_sha256(payload, "not-a-valid-sha256"),
        Err(UpdateError::MalformedChecksum(_))
    ));
}

#[test]
fn check_for_update_classifies_newer_same_and_downgrade() {
    let source = FakeReleaseSource::new("v0.3.0");

    let newer =
        check_for_update(&source, DEFAULT_REPO, "0.2.0", None, "aarch64-apple-darwin").unwrap();
    assert!(newer.is_newer);
    assert!(!newer.is_same);
    assert_eq!(newer.tag, "v0.3.0");
    assert_eq!(
        newer.archive_url,
        "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v0.3.0/aaf-v0.3.0-aarch64-apple-darwin.tar.gz"
    );
    assert_eq!(
        newer.checksums_url,
        "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v0.3.0/SHA256SUMS.txt"
    );
    assert_eq!(
        newer.checksums_sig_url,
        "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v0.3.0/SHA256SUMS.txt.minisig"
    );

    let same =
        check_for_update(&source, DEFAULT_REPO, "0.3.0", None, "aarch64-apple-darwin").unwrap();
    assert!(!same.is_newer);
    assert!(same.is_same);

    let downgrade = check_for_update(
        &source,
        DEFAULT_REPO,
        "0.4.0",
        Some("0.2.5"),
        "x86_64-pc-windows-msvc",
    )
    .unwrap();
    assert!(!downgrade.is_newer);
    assert!(!downgrade.is_same);
    assert_eq!(downgrade.tag, "v0.2.5");
    assert_eq!(
        downgrade.archive_name,
        "aaf-v0.2.5-x86_64-pc-windows-msvc.zip"
    );

    assert!(matches!(
        check_for_update(&source, DEFAULT_REPO, "0.2.0", None, "mips-unknown-none"),
        Err(UpdateError::UnsupportedTarget(_))
    ));
}

#[test]
fn download_and_verify_fails_closed_on_tampered_bytes_or_missing_checksum() {
    let good_bytes = b"authentic-tarball-bytes".to_vec();
    let good_hash = sha256_hex(&good_bytes);
    let plan = check_for_update(
        &FakeReleaseSource::new("v0.5.0"),
        DEFAULT_REPO,
        "0.1.0",
        None,
        "x86_64-unknown-linux-gnu",
    )
    .unwrap();

    let mut tampered = good_bytes.clone();
    tampered[5] ^= 0x80;
    let tampered_source = FakeReleaseSource::new("v0.5.0")
        .with_text(
            plan.checksums_url.clone(),
            format!("{good_hash}  {}\n", plan.archive_name),
        )
        .with_text(plan.checksums_sig_url.clone(), SIG_GOOD)
        .with_blob(plan.archive_url.clone(), tampered);

    assert!(matches!(
        download_and_verify_with_keys(&tampered_source, &plan, &[TEST_PUBKEY_1]),
        Err(UpdateError::ChecksumMismatch { .. })
    ));

    let missing_sum_source = FakeReleaseSource::new("v0.5.0")
        .with_text(
            plan.checksums_url.clone(),
            format!("{good_hash}  some-other-file.tar.gz\n"),
        )
        .with_text(plan.checksums_sig_url.clone(), SIG_OTHER)
        .with_blob(plan.archive_url.clone(), good_bytes.clone());

    assert!(matches!(
        download_and_verify_with_keys(&missing_sum_source, &plan, &[TEST_PUBKEY_1]),
        Err(UpdateError::MissingChecksum(_))
    ));

    let unsigned_source = FakeReleaseSource::new("v0.5.0")
        .with_text(
            plan.checksums_url.clone(),
            format!("{good_hash}  {}\n", plan.archive_name),
        )
        .with_blob(plan.archive_url.clone(), good_bytes.clone());

    assert!(matches!(
        download_and_verify_with_keys(&unsigned_source, &plan, &[TEST_PUBKEY_1]),
        Err(UpdateError::VerificationFailed(_))
    ));

    let ok_source = FakeReleaseSource::new("v0.5.0")
        .with_text(
            plan.checksums_url.clone(),
            format!("{good_hash}  {}\n", plan.archive_name),
        )
        .with_text(plan.checksums_sig_url.clone(), SIG_GOOD)
        .with_blob(plan.archive_url.clone(), good_bytes.clone());

    // Default production TRUSTED_RELEASE_PUBKEYS rejects untrusted test-key signatures.
    assert!(matches!(
        download_and_verify(&ok_source, &plan),
        Err(UpdateError::VerificationFailed(_))
    ));

    let verified = download_and_verify_with_keys(&ok_source, &plan, &[TEST_PUBKEY_1]).unwrap();
    assert_eq!(verified.sha256, good_hash);
    assert_eq!(verified.bytes, good_bytes);
}

#[cfg(unix)]
#[test]
fn end_to_end_install_verified_tarball_and_reject_symlink_archive() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let target = "aarch64-apple-darwin";
    let tag = "v0.9.0";
    let pkg_dir = format!("aaf-{tag}-{target}");

    let fake_aaf = b"#!/bin/sh\necho aaf 0.9.0\n".to_vec();
    let fake_hook = b"#!/bin/sh\necho aaf-hook 0.9.0\n".to_vec();
    let archive_name = archive_filename(tag, target);
    let archive_bytes = decode_hex(E2E_TARBALL_HEX);
    let archive_hash = sha256_hex(&archive_bytes);

    let base_source = FakeReleaseSource::new(tag);
    let plan = check_for_update(&base_source, DEFAULT_REPO, "0.1.0", None, target).unwrap();
    let source = base_source
        .with_text(
            plan.checksums_url.clone(),
            format!("{archive_hash}  {archive_name}\n"),
        )
        .with_text(plan.checksums_sig_url.clone(), SIG_E2E)
        .with_blob(plan.archive_url.clone(), archive_bytes);

    let install_dir = tmp.path().join("bin");
    fs::create_dir_all(&install_dir).unwrap();
    // Seed old binaries to verify atomic replacement cleans up backups.
    fs::write(install_dir.join("aaf"), b"old-aaf").unwrap();
    fs::write(install_dir.join("aaf-hook"), b"old-hook").unwrap();

    let receipt = upgrade_with_source_and_keys(
        &source,
        DEFAULT_REPO,
        "0.1.0",
        None,
        target,
        &install_dir,
        &[TEST_PUBKEY_1],
    )
    .unwrap();
    assert_eq!(receipt.tag, tag);
    assert_eq!(receipt.sha256, archive_hash);
    assert_eq!(receipt.installed_binaries.len(), 2);

    let installed_aaf = install_dir.join("aaf");
    let installed_hook = install_dir.join("aaf-hook");
    assert_eq!(fs::read(&installed_aaf).unwrap(), fake_aaf);
    assert_eq!(fs::read(&installed_hook).unwrap(), fake_hook);
    assert_eq!(
        fs::metadata(&installed_aaf).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&installed_hook).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // Ensure no leftover staging or backup files remain in install_dir.
    let entries: Vec<String> = fs::read_dir(&install_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2, "unexpected leftover files: {entries:?}");

    // Security boundary: archive containing a symlink binary MUST be rejected.
    let bad_stage = tmp.path().join("bad_stage");
    let bad_pkg = bad_stage.join(&pkg_dir);
    fs::create_dir_all(&bad_pkg).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bad_pkg.join("aaf")).unwrap();
    fs::write(bad_pkg.join("aaf-hook"), &fake_hook).unwrap();

    let bad_tarball = tmp.path().join("bad.tar.gz");
    let bad_status = Command::new("tar")
        .arg("-czf")
        .arg(&bad_tarball)
        .arg("-C")
        .arg(&bad_stage)
        .arg(&pkg_dir)
        .status()
        .unwrap();
    assert!(bad_status.success());

    let bad_bytes = fs::read(&bad_tarball).unwrap();
    let bad_hash = sha256_hex(&bad_bytes);
    let bad_verified = VerifiedArchive {
        plan,
        sha256: bad_hash,
        bytes: bad_bytes,
    };
    let reject_dir = tmp.path().join("reject_bin");
    assert!(matches!(
        install_verified_binaries(&bad_verified, &reject_dir),
        Err(UpdateError::UnsafeArchiveEntry(_))
    ));
}
