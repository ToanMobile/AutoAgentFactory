#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 3 tests: safe bounded stream logs and Seatbelt hardening for
//! `data_dir/{streams,reports}` and `<git_dir>/refs/aaf`.

use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
#[cfg(unix)]
use aaf_engine::executors::claude::ClaudeCodeRunner;
use aaf_engine::executors::generic::GenericRunner;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
use aaf_engine::executors::stream_log::{
    MAX_STREAM_LOG_BYTES, MAX_TARGET_BYTES, StreamLog, read_tail_lines, scrub_secrets,
    truncate_utf8,
};
use serde_json::Value;

#[test]
fn open_rejects_invalid_run_id_and_none_data_dir_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["../evil", "a/b", "", "id with space", ".hidden"] {
        let err = StreamLog::open(Some(dir.path()), bad, Vec::new())
            .expect_err("invalid run_id must fail");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{bad:?}");
    }
    assert!(!dir.path().join("streams").exists());

    let mut noop = StreamLog::open(None, "run-noop", Vec::new()).unwrap();
    noop.write_event("start", Some("generic"), Some("hello"));
    assert_eq!(noop.bytes_written(), 0);
    assert!(!noop.is_truncated());
}

#[cfg(unix)]
#[test]
fn open_enforces_private_modes_and_refuses_symlinks_and_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = StreamLog::open(Some(dir.path()), "run_perm_1", Vec::new()).unwrap();
    log.write_event("start", Some("claude"), Some("sonnet"));
    drop(log);

    let streams_dir = dir.path().join("streams");
    let file_path = streams_dir.join("run_perm_1.jsonl");
    assert_eq!(
        fs::symlink_metadata(&streams_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::symlink_metadata(&file_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // Pre-existing regular file must be refused by create_new.
    let err = StreamLog::open(Some(dir.path()), "run_perm_1", Vec::new())
        .expect_err("pre-existing file must fail");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);

    // Pre-existing symlink at file path must be refused.
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "keep").unwrap();
    symlink(&outside, streams_dir.join("run_link.jsonl")).unwrap();
    let err = StreamLog::open(Some(dir.path()), "run_link", Vec::new())
        .expect_err("symlink target must fail");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");

    // Pre-existing symlink at streams dir must be refused.
    let dir2 = tempfile::tempdir().unwrap();
    let real_target = dir2.path().join("real");
    fs::create_dir(&real_target).unwrap();
    symlink(&real_target, dir2.path().join("streams")).unwrap();
    let err = StreamLog::open(Some(dir2.path()), "run_dir_link", Vec::new())
        .expect_err("symlinked streams dir must fail");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn scrub_secrets_redacts_all_patterns_and_env_pass_values() {
    // Fragments avoid tripping static secret scanners on test fixtures.
    let pem = [
        "-----BEGIN RSA ",
        "PRIVATE ",
        "KEY-----\nMIIEFAKEKEYBODY12345\n-----END RSA ",
        "PRIVATE ",
        "KEY-----",
    ]
    .concat();
    let bearer_val = ["eyJfake", ".payload.", "sig987654"].concat();
    let url_pass = ["pa55", "w0rd_99"].concat();
    let sk_val = ["sk-", "ant-", "api03-abcdef0123456789ABCDEF"].concat();
    let ghp_val = ["ghp_", "0123456789abcdef0123456789abcdef0123"].concat();
    let gho_val = ["gho_", "fedcba9876543210fedcba9876543210fedc"].concat();
    let akia_val = ["AK", "IAIOSFODNN7EXAMPLE99"].concat();
    let env_val = ["custom_runtime_", "cred_887766"].concat();

    let payload = format!(
        "pem={pem} auth=Bearer {bearer_val} url=https://alice:{url_pass}@example.invalid/r \
         k1={sk_val} k2={ghp_val} k3={gho_val} k4={akia_val} env={env_val} keep=dev"
    );
    // Short (<4 char) env values like "dev" must be ignored so ordinary words remain.
    let extras = vec![String::new(), "dev".to_string(), env_val.clone()];
    let scrubbed = scrub_secrets(&payload, &extras);

    for leaked in [
        "MIIEFAKEKEYBODY12345",
        bearer_val.as_str(),
        url_pass.as_str(),
        sk_val.as_str(),
        ghp_val.as_str(),
        gho_val.as_str(),
        akia_val.as_str(),
        env_val.as_str(),
    ] {
        assert!(
            !scrubbed.contains(leaked),
            "leaked {leaked:?} in {scrubbed}"
        );
    }
    assert!(scrubbed.contains("Bearer [REDACTED]"), "{scrubbed}");
    assert!(
        scrubbed.contains("https://[REDACTED]@example.invalid/r"),
        "{scrubbed}"
    );
    assert!(scrubbed.contains("keep=dev"), "{scrubbed}");
}

#[test]
fn target_truncates_safely_to_512_utf8_bytes() {
    // 200 x 3-byte char ('€') = 600 bytes; 512 / 3 = 170 chars = 510 bytes.
    let wide = "€".repeat(200);
    let cut = truncate_utf8(&wide, MAX_TARGET_BYTES);
    assert_eq!(cut.len(), 510);
    assert!(cut.len() <= MAX_TARGET_BYTES);

    let dir = tempfile::tempdir().unwrap();
    let mut log = StreamLog::open(Some(dir.path()), "run_utf8", Vec::new()).unwrap();
    log.write_event("assistant", None, Some(&wide));
    drop(log);

    let lines = read_tail_lines(dir.path(), "run_utf8", 5).unwrap();
    assert_eq!(lines.len(), 1);
    let parsed: Value = serde_json::from_str(&lines[0]).unwrap();
    let target = parsed["target"].as_str().unwrap();
    assert!(target.len() <= MAX_TARGET_BYTES);
    assert_eq!(target, cut);
}

#[test]
fn hard_2mib_cap_writes_single_truncated_marker_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = StreamLog::open(Some(dir.path()), "run_cap", Vec::new()).unwrap();
    let chunk = "x".repeat(MAX_TARGET_BYTES);
    // Each JSONL line is ~560 bytes -> ~3800 lines crosses 2 MiB cleanly.
    for _ in 0..4_500 {
        log.write_event("assistant", None, Some(&chunk));
    }
    assert!(log.is_truncated());
    let capped_bytes = log.bytes_written();
    assert!(capped_bytes <= MAX_STREAM_LOG_BYTES);

    // Subsequent writes must be silently ignored.
    for _ in 0..50 {
        log.write_event("assistant", None, Some("after-cap"));
    }
    assert_eq!(log.bytes_written(), capped_bytes);
    drop(log);

    let file_path = dir.path().join("streams/run_cap.jsonl");
    let raw = fs::read_to_string(&file_path).unwrap();
    assert_eq!(raw.len(), capped_bytes);
    assert!(raw.len() <= MAX_STREAM_LOG_BYTES);
    assert!(!raw.contains("after-cap"));

    let trunc_count = raw
        .lines()
        .filter(|line| *line == "{\"type\":\"truncated\"}")
        .count();
    assert_eq!(trunc_count, 1);
    assert_eq!(raw.lines().last(), Some("{\"type\":\"truncated\"}"));
}

#[test]
fn read_tail_lines_returns_last_n_and_rejects_bad_id_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = StreamLog::open(Some(dir.path()), "run_tail", Vec::new()).unwrap();
    for idx in 0..10 {
        log.write_event("assistant", None, Some(&format!("msg-{idx}")));
    }
    drop(log);

    let tail = read_tail_lines(dir.path(), "run_tail", 3).unwrap();
    assert_eq!(tail.len(), 3);
    assert!(tail[0].contains("msg-7"));
    assert!(tail[1].contains("msg-8"));
    assert!(tail[2].contains("msg-9"));
    assert!(
        read_tail_lines(dir.path(), "run_tail", 0)
            .unwrap()
            .is_empty()
    );

    let err = read_tail_lines(dir.path(), "../escape", 5).expect_err("invalid run_id rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);

    #[cfg(unix)]
    {
        let target = dir.path().join("streams/run_tail.jsonl");
        let link = dir.path().join("streams/run_symlink.jsonl");
        symlink(&target, &link).unwrap();
        let err = read_tail_lines(dir.path(), "run_symlink", 5).expect_err("symlink rejected");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }
}

#[test]
fn generic_runner_writes_sanitized_summary_lines() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let work_dir = dir.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();

    let passed_val = ["envpass_", "secret_", "998877"].concat();
    let env = ExecEnv {
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        parent_env: vec![("CUSTOM_CFG_ITEM".to_string(), passed_val.clone())],
        env_pass: vec!["CUSTOM_CFG_ITEM".to_string()],
        ..ExecEnv::default()
    };
    let script = format!(
        "echo done-ok-$CUSTOM_CFG_ITEM-{}",
        ["ghp_", "0123456789abcdef0123456789abcdef0123"].concat()
    );
    let template = ["/bin/sh", "-c", &script, "sh", "{prompt}"].map(String::from);
    let runner = GenericRunner::new(template.to_vec(), env).unwrap();

    let outcome = runner
        .run(&AgentRequest {
            run_id: "run_generic_1".to_string(),
            prompt: "hello".to_string(),
            cwd: work_dir.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 30,
            sandbox: SandboxPolicy::default(),
        })
        .unwrap();
    assert_eq!(outcome.exit_code, Some(0));

    let lines = read_tail_lines(&data_dir, "run_generic_1", 10).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let start: Value = serde_json::from_str(&lines[0]).unwrap();
    let finish: Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(start["type"], "start");
    assert_eq!(start["tool"], "sh");
    assert_eq!(finish["type"], "finish");
    assert_eq!(finish["tool"], "sh");
    let target = finish["target"].as_str().unwrap();
    assert!(target.contains("exit=0"), "{target}");
    assert!(target.contains("timed_out=false"), "{target}");
    assert!(target.contains("done-ok-"), "{target}");
    assert!(!target.contains(&passed_val), "{target}");
    assert!(!target.contains("ghp_"), "{target}");
}

#[cfg(unix)]
#[test]
fn claude_runner_writes_sanitized_projections_not_raw_stream_json() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let work_dir = dir.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();

    let sk_val = ["sk-", "ant-", "api03-testval1234567890abcdef"].concat();
    let stream_body = [
        r#"{"type":"system","subtype":"init","tools":["SENSITIVE_INTERNAL_TOOL"]}"#.to_string(),
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "m1",
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": format!("curl -H 'Authorization: Bearer {sk_val}' https://example.invalid")}},
                    {"type": "text", "text": "checked repo"}
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })
        .to_string(),
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"RAW_FILE_DUMP_NEVER_LOG"}]}}"#.to_string(),
        r#"{"type":"result","subtype":"success","is_error":false,"result":"all good","usage":{"input_tokens":12,"output_tokens":7}}"#.to_string(),
    ]
    .join("\n");

    let fake_bin = dir.path().join("fake-claude");
    fs::write(
        &fake_bin,
        format!("#!/bin/sh\ncat <<'EOF'\n{stream_body}\nEOF\n"),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let env = ExecEnv {
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        ..ExecEnv::default()
    };
    let runner = ClaudeCodeRunner::new(fake_bin.to_string_lossy(), env);
    let outcome = runner
        .run(&AgentRequest {
            run_id: "run_claude_proj".to_string(),
            prompt: "check".to_string(),
            cwd: work_dir.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 30,
            sandbox: SandboxPolicy::default(),
        })
        .unwrap();
    assert!(outcome.succeeded(), "{outcome:?}");

    let lines = read_tail_lines(&data_dir, "run_claude_proj", 20).unwrap();
    let joined = lines.join("\n");
    assert!(!joined.contains("SENSITIVE_INTERNAL_TOOL"), "{joined}");
    assert!(!joined.contains("RAW_FILE_DUMP_NEVER_LOG"), "{joined}");
    assert!(!joined.contains(&sk_val), "{joined}");

    let types: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        types,
        vec!["start", "tool_use", "assistant", "result", "finish"]
    );
}

#[test]
fn seatbelt_profile_denies_writes_under_git_refs_aaf() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().canonicalize().unwrap().join("repo/.git");
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let expected = format!(
        "(deny file-write* (subpath \"{}/refs/aaf\"))",
        git_dir.display()
    );
    assert!(profile.contains(&expected), "{profile}");
}

#[cfg(target_os = "macos")]
mod seatbelt_macos {
    use std::fs;
    use std::path::Path;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::generic::GenericRunner;

    const GIT_FLAGS: [&str; 8] = [
        "-c",
        "user.name=aaf-test",
        "-c",
        "user.email=aaf-test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
    ];

    fn git(repo: &Path, args: &[&str]) -> String {
        let mut full = GIT_FLAGS.to_vec();
        full.extend_from_slice(args);
        aaf_fp::git(repo, &full, &[], aaf_fp::GIT_TIMEOUT).unwrap()
    }

    #[test]
    fn sandbox_blocks_read_and_write_on_data_dir_streams_and_reports() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let work = root.join("work");
        let data_dir = root.join("data");
        let streams = data_dir.join("streams");
        let reports = data_dir.join("reports");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&streams).unwrap();
        fs::create_dir_all(&reports).unwrap();

        let stream_file = streams.join("run-1.jsonl");
        let report_file = reports.join("out.html");
        fs::write(&stream_file, "{\"line\":\"STREAM-DATA\"}\n").unwrap();
        fs::write(&report_file, "<html>REPORT-DATA</html>").unwrap();

        let s = stream_file.display();
        let r = report_file.display();
        let script = format!(
            "cat '{s}' || echo stream-read-denied; \
             echo forged >> '{s}' || echo stream-write-denied; \
             cat '{r}' || echo report-read-denied; \
             echo forged > '{r}' || echo report-write-denied; \
             echo ok > '{work}/allowed.txt' && echo work-write-ok",
            work = work.display(),
        );
        let template = ["/bin/sh", "-c", &script, "sh", "{prompt}"].map(String::from);
        let env = ExecEnv {
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            ..ExecEnv::default()
        };
        let outcome = GenericRunner::new(template.to_vec(), env)
            .unwrap()
            .run(&AgentRequest {
                run_id: "run-sandbox-data".to_string(),
                prompt: "unused".to_string(),
                cwd: work.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    read_only: false,
                    // Even if data_dir's parent is listed writable, data_dir stays denied.
                    writable_paths: vec![root.to_string_lossy().into_owned()],
                    read_denied_paths: Vec::new(),
                },
            })
            .unwrap();
        assert!(!outcome.timed_out, "{}", outcome.log_tail);
        for marker in [
            "stream-read-denied",
            "stream-write-denied",
            "report-read-denied",
            "report-write-denied",
            "work-write-ok",
        ] {
            assert!(
                outcome.log_tail.contains(marker),
                "missing {marker}: {}",
                outcome.log_tail
            );
        }
        assert!(!outcome.log_tail.contains("STREAM-DATA"));
        assert!(!outcome.log_tail.contains("REPORT-DATA"));
        assert_eq!(
            fs::read_to_string(&stream_file).unwrap(),
            "{\"line\":\"STREAM-DATA\"}\n"
        );
        assert_eq!(
            fs::read_to_string(&report_file).unwrap(),
            "<html>REPORT-DATA</html>"
        );
    }

    #[test]
    fn sandbox_allows_normal_worktree_and_branches_but_blocks_refs_aaf() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main_repo = root.join("main");
        let worktree = root.join("wt");
        fs::create_dir(&main_repo).unwrap();
        git(&main_repo, &["init", "-q"]);
        fs::write(main_repo.join("a.txt"), "initial\n").unwrap();
        git(&main_repo, &["add", "a.txt"]);
        git(&main_repo, &["commit", "-q", "-m", "init"]);
        git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task-branch",
                worktree.to_str().unwrap(),
            ],
        );

        let git_dir = main_repo.join(".git");
        let head_sha = git(&main_repo, &["rev-parse", "HEAD"]).trim().to_string();
        let snap_ref = git_dir.join("refs/aaf/snapshots/t1/snap1");
        fs::create_dir_all(snap_ref.parent().unwrap()).unwrap();
        fs::write(&snap_ref, format!("{head_sha}\n")).unwrap();

        let flags = GIT_FLAGS.join(" ");
        let g = git_dir.display();
        let snap = snap_ref.display();
        let snap2 = git_dir.join("refs/aaf/snapshots/t1/snap2");
        let script = format!(
            "echo work > note.txt && git add note.txt && git {flags} commit -q -m 'work' && echo commit-ok; \
             git checkout -q -b feature-normal && echo branch-ok; \
             echo deadbeef > '{snap2}' || echo create-snap-denied; \
             echo deadbeef > '{snap}' || echo modify-snap-denied; \
             rm -f '{snap}' || echo delete-snap-denied; \
             git update-ref refs/aaf/snapshots/t1/snap3 HEAD || echo update-ref-denied; \
             mv '{g}/refs' '{g}/refs-moved' || echo rename-refs-denied",
            snap2 = snap2.display(),
        );
        let template = ["/bin/sh", "-c", &script, "sh", "{prompt}"].map(String::from);
        let outcome = GenericRunner::new(template.to_vec(), ExecEnv::default())
            .unwrap()
            .run(&AgentRequest {
                run_id: "run-sandbox-refs-aaf".to_string(),
                prompt: "unused".to_string(),
                cwd: worktree.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 60,
                sandbox: SandboxPolicy {
                    enabled: true,
                    ..SandboxPolicy::default()
                },
            })
            .unwrap();
        assert!(!outcome.timed_out, "{}", outcome.log_tail);
        for marker in [
            "commit-ok",
            "branch-ok",
            "create-snap-denied",
            "modify-snap-denied",
            "delete-snap-denied",
            "update-ref-denied",
            "rename-refs-denied",
        ] {
            assert!(
                outcome.log_tail.contains(marker),
                "missing {marker}: {}",
                outcome.log_tail
            );
        }
        assert!(worktree.join("note.txt").exists());
        assert!(git_dir.join("refs/heads/feature-normal").exists());
        assert!(!snap2.exists());
        assert_eq!(
            fs::read_to_string(&snap_ref).unwrap(),
            format!("{head_sha}\n")
        );
        assert!(!git_dir.join("refs-moved").exists());
    }
}
