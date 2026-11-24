//! Engine-level confinement tests for `proof_capture` (`capture_cmd`) and
//! worktree hydration (`setup_cmd`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "macos")]
mod macos {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aaf_domain::config::ProofConfig;
    use aaf_domain::ports::Clock;
    use aaf_engine::git::hydration::hydrate_fresh_worktree_with_snaps;
    use aaf_engine::git::worktree;
    use aaf_engine::proof::capture_and_record_proof;
    use tempfile::TempDir;

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "dev")
            .env("GIT_AUTHOR_EMAIL", "dev@example.invalid")
            .env("GIT_COMMITTER_NAME", "dev")
            .env("GIT_COMMITTER_EMAIL", "dev@example.invalid")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path, config_toml: &str) -> String {
        fs::create_dir_all(dir.join(".autofactory")).unwrap();
        git(dir, &["init", "-b", "main"]);
        fs::write(dir.join(".autofactory/config.toml"), config_toml).unwrap();
        fs::write(dir.join("README.md"), "fixture\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", "initial"]);
        git(dir, &["rev-parse", "HEAD"])
    }

    fn crc32(kind: &[u8], data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in kind.iter().chain(data.iter()) {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }

    fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
    }

    fn valid_ui_png() -> Vec<u8> {
        let mut idat = vec![0x78, 0x9C];
        idat.extend((2..9000usize).map(|i| ((i * 37 + 13) % 251) as u8));
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&1080u32.to_be_bytes());
        ihdr.extend_from_slice(&1920u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        push_chunk(&mut out, *b"IHDR", &ihdr);
        push_chunk(&mut out, *b"IDAT", &idat);
        push_chunk(&mut out, *b"IEND", &[]);
        out
    }

    #[test]
    fn proof_capture_cmd_cannot_read_receipt_key() {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        let data = root.join("data");
        let out = root.join("out");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&out).unwrap();
        let _ = aaf_fp::load_or_create_key(&data).unwrap();

        let good_png = out.join("good.png");
        fs::write(&good_png, valid_ui_png()).unwrap();
        let leak = out.join("leak");
        let capture_cmd = format!(
            "cp '{}/receipt.key' '{}' 2>/dev/null; cp '{}' \"$AAF_PROOF_OUT\"",
            data.display(),
            leak.display(),
            good_png.display()
        );
        let sha = init_repo(
            &repo,
            &format!("[proof]\nrequired = true\ncapture_cmd = {capture_cmd:?}\n"),
        );
        let fp = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
        let cfg = ProofConfig {
            required: true,
            ..ProofConfig::default()
        };
        capture_and_record_proof(&repo, &data, &sha, &cfg, &fp, &FixedClock(1000)).unwrap();
        assert!(
            !leak.exists(),
            "proof capture_cmd leaked receipt.key into {}",
            leak.display()
        );
    }

    #[test]
    fn hydration_setup_cmd_cannot_read_receipt_key() {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        let worktrees = root.join("worktrees");
        let data = root.join("data");
        let out = root.join("out");
        fs::create_dir_all(&worktrees).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&out).unwrap();
        let _ = aaf_fp::load_or_create_key(&data).unwrap();

        let leak = out.join("leak");
        let setup_cmd = format!(
            "cp '{}/receipt.key' '{}' 2>/dev/null; exit 0",
            data.display(),
            leak.display()
        );
        let sha = init_repo(&repo, &format!("[worktree]\nsetup_cmd = {setup_cmd:?}\n"));
        let wt = worktree::create(&repo, &worktrees, "t-confine", "main").unwrap();
        hydrate_fresh_worktree_with_snaps(&repo, &wt, &sha, &data, "repo-id").unwrap();
        assert!(
            !leak.exists(),
            "hydration setup_cmd leaked receipt.key into {}",
            leak.display()
        );
    }
}
