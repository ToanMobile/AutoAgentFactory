#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(target_os = "macos")]
//! `aaf slot` as an agent runs it: inside the Seatbelt sandbox of an agent
//! run, with the `AAF_DATA_DIR` the run is given. The hook sends every heavy
//! build of an agent through `aaf slot`, and the sandbox closes the data
//! dir, so this is the one sanctioned way to build: it has to work, and the
//! secrets of the data dir have to stay closed while it does. The real
//! `sandbox-exec`, the generated profile and the real `aaf`; the agent is a
//! shell script standing in for `claude`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::ClaudeCodeRunner;
use aaf_engine::throttle::slots::{self, Admission};

#[path = "cli_support.rs"]
mod cli_support;

const AAF: &str = env!("CARGO_BIN_EXE_aaf");
const KEY: &str = "not-a-real-key-0123456789";
static RUNS: AtomicU32 = AtomicU32::new(0);

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

/// A data dir as `aaf run` leaves one, and a home whose tier 1 gives the
/// machine one heavy slot and no wait for the load of the test machine.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let data = root.join("data");
    for dir in ["leases", "locks", "receipts", "approvals"] {
        fs::create_dir_all(data.join(dir)).unwrap();
    }
    fs::write(data.join("receipt.key"), KEY).unwrap();
    fs::write(data.join("aaf.db"), "sqlite").unwrap();
    fs::write(data.join("leases/acct.lock"), "").unwrap();
    fs::write(data.join("locks/repo.run.lock"), "").unwrap();
    fs::write(data.join("receipts/r.json"), "{}").unwrap();
    fs::write(data.join("approvals/a.json"), "{}").unwrap();
    fs::create_dir_all(root.join("home/.config/autofactory")).unwrap();
    fs::write(
        root.join("home/.config/autofactory/config.toml"),
        "[machine]\nmax_heavy_parallel = 1\nmax_load_percent = 0\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("wt")).unwrap();
    World {
        _tmp: tmp,
        root,
        data,
    }
}

/// Runs `script` as the agent of a sandboxed run and returns what it printed.
fn agent(world: &World, script: &str) -> String {
    let claude = world.root.join("claude");
    fs::write(&claude, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let env = ExecEnv {
        home: Some(world.root.join("home").to_string_lossy().into_owned()),
        // Nothing writable by default: the temp dir holds this whole tree.
        temp_dirs: Vec::new(),
        data_dir: Some(world.data.to_string_lossy().into_owned()),
        parent_env: vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            (
                "HOME".to_string(),
                world.root.join("home").to_string_lossy().into_owned(),
            ),
        ],
        ..ExecEnv::default()
    };
    ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            // Unique among the tests of this process and of others: the
            // reaper finds what a run left behind by this id.
            run_id: format!(
                "test-slot-{}-{}",
                std::process::id(),
                RUNS.fetch_add(1, Ordering::SeqCst)
            ),
            prompt: "unused".to_string(),
            cwd: world.root.join("wt").to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 120,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
        })
        .unwrap()
        .log_tail
}

fn sandbox_exists() -> bool {
    Path::new("/usr/bin/sandbox-exec").exists()
}

/// For a test whose agent needs a heavy command of `aaf slot` to start: under
/// memory pressure the command waits (and `--timeout` ends the wait with
/// 75), which is the product working, not the test failing. Same rule as
/// the other heavy tests, `AAF_REQUIRE_REAL=1` included.
fn can_run_heavy(test: &str) -> bool {
    sandbox_exists() && cli_support::memory_is_calm(test)
}

#[test]
fn a_sandboxed_agent_can_queue_a_build_through_aaf_slot() {
    if !can_run_heavy("a_sandboxed_agent_can_queue_a_build_through_aaf_slot") {
        return;
    }
    let world = world();

    let log = agent(
        &world,
        &format!("'{AAF}' slot --timeout 30 -- sh -c 'echo ran-in-slot'\necho slot-exit=$?\n"),
    );

    assert!(log.contains("ran-in-slot"), "{log}");
    assert!(log.contains("slot-exit=0"), "{log}");
}

#[test]
fn a_slot_taken_inside_the_sandbox_is_held_against_others_and_open_to_its_own_command() {
    if !can_run_heavy("a_slot_taken_inside_the_sandbox_is_held_against_others") {
        return;
    }
    let world = world();
    let inner = world.root.join("wt/inner.sh");
    // What the agent's build runs: a command below it that carries the
    // slot's token goes straight through, one without it has to queue and
    // does not get the only slot.
    fs::write(
        &inner,
        format!(
            "'{AAF}' slot --timeout 2 -- sh -c 'echo below-ran'\necho below-exit=$?\n\
             env -u AAF_HEAVY_SLOT '{AAF}' slot --timeout 2 -- sh -c 'echo beside-ran'\n\
             echo beside-exit=$?\n"
        ),
    )
    .unwrap();

    let log = agent(
        &world,
        &format!(
            "'{AAF}' slot --timeout 30 -- sh '{}'\necho slot-exit=$?\n",
            inner.display()
        ),
    );

    assert!(log.contains("below-ran"), "{log}");
    assert!(log.contains("below-exit=0"), "{log}");
    assert!(!log.contains("beside-ran"), "{log}");
    assert!(log.contains("beside-exit=75"), "{log}");
    assert!(log.contains("slot-exit=0"), "{log}");
    // Given back: the factory's gate, outside the sandbox, gets the slot,
    // and no record of the agent's run is left to be read as the gate's.
    let gate = slots::try_acquire(&world.data, 1, Admission::Unmeasured, &|| None).unwrap();
    assert!(gate.is_ok());
    assert!(slots::read_reservation(&world.data, 0).is_some_and(|r| r.reserved_bytes.is_none()));
}

#[test]
fn the_secrets_of_the_data_dir_stay_closed_to_an_agent_that_can_queue() {
    if !can_run_heavy("the_secrets_of_the_data_dir_stay_closed_to_an_agent_that_can_queue") {
        return;
    }
    let world = world();
    let closed = [
        "receipt.key",
        "aaf.db",
        "leases/acct.lock",
        "locks/repo.run.lock",
        "receipts/r.json",
        "approvals/a.json",
    ];
    let mut script = format!("'{AAF}' slot --timeout 30 -- true; echo slot-exit=$?\n");
    for rel in closed {
        let path = world.data.join(rel);
        script.push_str(&format!(
            "cat '{0}' 2>/dev/null && echo 'read:{rel}'\n\
             ( echo x >> '{0}' ) 2>/dev/null && echo 'written:{rel}'\n",
            path.display()
        ));
    }
    script.push_str(&format!(
        "ls '{0}' 2>/dev/null && echo 'listed:data'\n\
         ( echo x > '{0}/new.txt' ) 2>/dev/null && echo 'created:data'\n\
         ( echo x > '{0}/receipts/forged.json' ) 2>/dev/null && echo 'created:receipt'\n",
        world.data.display()
    ));

    let log = agent(&world, &script);

    assert!(log.contains("slot-exit=0"), "{log}");
    for marker in ["read:", "written:", "listed:", "created:", KEY] {
        assert!(!log.contains(marker), "{marker} in: {log}");
    }
    assert_eq!(
        fs::read_to_string(world.data.join("receipt.key")).unwrap(),
        KEY
    );
    assert!(!world.data.join("receipts/forged.json").exists());
}

#[test]
fn aaf_gate_inside_the_sandbox_signs_nothing() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let repo = world.root.join("wt");
    fs::create_dir_all(repo.join(".autofactory")).unwrap();
    fs::write(
        repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"echo gate-command-ran\"\n",
    )
    .unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "config",
        ],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    // The gate is the factory's, run outside the sandbox after the agent's
    // run. An agent that runs it gets no receipt: it cannot reach the key.
    let log = agent(
        &world,
        &format!("'{AAF}' gate --json 2>&1\necho gate-exit=$?\n"),
    );

    assert!(!log.contains("gate-exit=0"), "{log}");
    assert!(!log.contains(KEY), "{log}");
    let receipts: Vec<_> = fs::read_dir(world.data.join("receipts"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(receipts, ["r.json"], "{log}");
    assert_eq!(
        fs::read_to_string(world.data.join("receipt.key")).unwrap(),
        KEY
    );
    // Shown when the test is run with `--nocapture`: what the agent saw.
    println!("{log}");
}

#[test]
fn an_agent_cannot_remove_empty_or_forge_the_slot_of_a_holder_outside_the_sandbox() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    // A gate of the factory, outside the sandbox, holds the only slot.
    let held = slots::try_acquire(&world.data, 1, Admission::Unmeasured, &|| None)
        .unwrap()
        .unwrap();
    let lock = slots::slot_path(&world.data, 0);
    let before = fs::read_to_string(&lock).unwrap();
    assert!(before.contains("reserved="), "{before}");
    let forged = "0123456789abcdef0123456789abcdef";
    let throttle = lock.parent().unwrap();

    let log = agent(
        &world,
        &format!(
            "rm -f '{lock}' 2>/dev/null; test -e '{lock}' || echo lock-removed\n\
             ( echo 'token={forged}' > '{lock}' ) 2>/dev/null && echo lock-written\n\
             ( : > '{lock}' ) 2>/dev/null && echo lock-emptied\n\
             mv '{lock}' '{lock}.gone' 2>/dev/null && echo lock-moved\n\
             mv '{dir}' '{dir}.gone' 2>/dev/null && echo dir-moved\n\
             for f in '{dir}'/guest/*; do ( echo 'token={forged}' > \"$f\" ) 2>/dev/null; done\n\
             ( printf 'token={forged}\\nreserved=0\\n' > '{dir}/guest/heavy.0.rec' ) 2>/dev/null\n\
             AAF_HEAVY_SLOT=0:{forged} '{AAF}' slot --timeout 2 -- sh -c 'echo jumped-the-queue'\n\
             echo slot-exit=$?\n",
            lock = lock.display(),
            dir = throttle.display(),
        ),
    );

    for marker in [
        "lock-removed",
        "lock-written",
        "lock-emptied",
        "lock-moved",
        "dir-moved",
        "jumped-the-queue",
    ] {
        assert!(!log.contains(marker), "{marker} in: {log}");
    }
    // It waited its turn, and the turn did not come while the slot was held.
    assert!(log.contains("slot-exit=75"), "{log}");
    assert_eq!(fs::read_to_string(&lock).unwrap(), before);
    assert!(slots::is_held(&world.data, 0));
    drop(held);
}

#[test]
fn the_token_of_a_holder_outside_the_sandbox_cannot_be_read_from_inside() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let held = slots::try_acquire(&world.data, 1, Admission::Unmeasured, &|| None)
        .unwrap()
        .unwrap();
    let token = held.token().as_str().to_string();
    let lock = slots::slot_path(&world.data, 0);

    // Everything the agent can read of the queue, and the place the token
    // is kept. With the real token it would run beside the holder.
    let log = agent(
        &world,
        &format!(
            "cat '{dir}'/* '{dir}'/guest/* 2>/dev/null\n\
             cat '{closed}' 2>/dev/null && echo token-file-read\n\
             grep -r token '{data}' 2>/dev/null\n\
             echo looked\n",
            dir = lock.parent().unwrap().display(),
            closed = aaf_engine::throttle::paths::token_path(&world.data, 0).display(),
            data = world.data.display(),
        ),
    );

    // It did read the lock file: the reservation is there for it to see.
    assert!(log.contains("reserved=unknown"), "{log}");
    assert!(log.contains("looked"), "{log}");
    assert!(!log.contains("token-file-read"), "{log}");
    assert!(!log.contains(&token), "the token is in: {log}");
    drop(held);
}

#[test]
fn the_opened_directories_cannot_be_moved_out_and_back() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let held = slots::try_acquire(&world.data, 1, Admission::Unmeasured, &|| None)
        .unwrap()
        .unwrap();
    let lock = slots::slot_path(&world.data, 0);
    let before = fs::read_to_string(&lock).unwrap();
    let throttle = lock.parent().unwrap();

    // A write-deny on what is inside a directory holds only while the
    // directory itself cannot be renamed into a writable place and back.
    let log = agent(
        &world,
        &format!(
            "cd '{wt}'\n\
             mv '{dir}/guest' ./g 2>/dev/null && echo guest-moved\n\
             mv '{dir}' ./t 2>/dev/null && echo throttle-moved\n\
             mv '{data}' ./d 2>/dev/null && echo data-moved\n\
             for lock in ./t/heavy.0.lock ./d/throttle/heavy.0.lock; do\n\
             ( echo forged > $lock ) 2>/dev/null && echo lock-written\n\
             done\n\
             rmdir '{dir}/guest' 2>/dev/null && echo guest-removed\n\
             ln -s '{wt}' '{dir}/guest.link' 2>/dev/null && echo throttle-entry-made\n\
             mv ./g '{dir}/guest' 2>/dev/null; mv ./t '{dir}' 2>/dev/null\n\
             mv ./d '{data}' 2>/dev/null\n\
             echo tried\n",
            wt = world.root.join("wt").display(),
            dir = throttle.display(),
            data = world.data.display(),
        ),
    );

    assert!(log.contains("tried"), "{log}");
    for marker in [
        "guest-moved",
        "throttle-moved",
        "data-moved",
        "lock-written",
        "guest-removed",
        "throttle-entry-made",
    ] {
        assert!(!log.contains(marker), "{marker} in: {log}");
    }
    assert!(throttle.join("guest").is_dir());
    assert_eq!(fs::read_to_string(&lock).unwrap(), before);
    assert!(slots::is_held(&world.data, 0));
    drop(held);
}

#[test]
fn a_record_an_agent_makes_up_for_a_free_slot_is_believed_by_nobody() {
    if !can_run_heavy("a_record_an_agent_makes_up_for_a_free_slot_is_believed_by_nobody") {
        return;
    }
    let world = world();
    let forged = "0123456789abcdef0123456789abcdef";
    let started = "Sun Nov 22 11:00:00 2026";
    let record = slots::slot_path(&world.data, 0)
        .parent()
        .unwrap()
        .join("guest/heavy.0.rec");
    // The record of a killed holder whose command still runs, made up: the
    // group it names is the agent's own, which does run.
    let plant = format!(
        "printf 'token={forged}\\nreserved=unknown\\npgid=%s\\nlstart={started}\\n' $$ > '{}'\n",
        record.display()
    );

    let log = agent(
        &world,
        &format!(
            "{plant}AAF_HEAVY_SLOT=0:{forged} '{AAF}' slot --timeout 5 -- sh -c 'echo ran' 2>&1\n\
             echo slot-exit=$?\n{plant}echo planted=$$\n"
        ),
    );

    // The made-up token let it past nothing: it took the free slot like
    // anybody else.
    assert!(log.contains("slot-exit=0"), "{log}");
    assert!(!log.contains("already inside heavy slot"), "{log}");
    let pgid: u32 = log
        .lines()
        .find_map(|line| line.trim().strip_prefix("planted="))
        .and_then(|pgid| pgid.parse().ok())
        .unwrap_or_else(|| panic!("no pgid in: {log}"));
    assert!(fs::read_to_string(&record).unwrap().contains(forged));
    // And outside the sandbox, with that group running as the record says:
    // the slot is not kept for it, and its token opens nothing.
    let running = |asked: u32| {
        (asked == pgid).then(|| aaf_engine::throttle::GroupState {
            stopped: false,
            leader_started: Some(started.to_string()),
        })
    };
    let token = aaf_engine::throttle::SlotToken::parse(forged).unwrap();
    assert!(!slots::holds_token_in(&world.data, 0, &token, &running));
    let view = slots::MachineView {
        free_memory: &|| None,
        group_rss: &|_| None,
        group_state: &running,
    };
    let taken = slots::try_acquire_in(&world.data, 1, Admission::Unmeasured, &view).unwrap();
    assert!(taken.is_ok(), "{:?}", taken.err());
}
