//! Factory settlement tests for `needs_human` proof flows (W3 K1/L2).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::config::LandMode;
use aaf_domain::gate::Receipt;
use aaf_domain::ports::PortResult;
use aaf_domain::proof::{AssertionResult, FlowResult, PROOF_SCHEMA_VERSION_V2, ProofRecord};
use aaf_domain::task::TaskState;

use super::fakes::{FakeGit, ok_outcome};
use super::world::World;
use crate::factory::ports::{GitOps, Integration};
use crate::factory::{Hooks, Mode, run};
use crate::git::{MergeOutcome, PrMergeOutcome, PrOutcome, TaskWorktree};
use crate::proof::{HOST_PROOF_FLOWS_DEVICE, HOST_PROOF_FLOWS_TOOL, captured_proof_image_path};

fn crc32(kind: &[u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if (crc & 1) != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn valid_png() -> Vec<u8> {
    let mut idat = vec![0x78, 0x9C];
    idat.extend((2..9000usize).map(|i| ((i * 37 + 13) % 251) as u8));
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    for (kind, data) in [
        (*b"IHDR", ihdr.as_slice()),
        (*b"IDAT", idat.as_slice()),
        (*b"IEND", &[]),
    ] {
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
    }
    out
}

/// Wraps `FakeGit` with real on-disk worktree directories so `verify_proof`
/// can canonicalize `worktree.path` and verify the signed ProofRecord v2.
struct DiskWorktreeGit {
    inner: Arc<FakeGit>,
    wt_root: PathBuf,
    merge_pr_calls: AtomicUsize,
}

impl GitOps for DiskWorktreeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let dir = self.wt_root.join(task_id);
        fs::create_dir_all(&dir).unwrap();
        let canon = fs::canonicalize(&dir).unwrap();
        let tree = TaskWorktree {
            path: canon.clone(),
            branch: format!("aaf/{task_id}"),
        };
        self.inner
            .0
            .lock()
            .unwrap()
            .trees
            .insert(canon, vec![task_id.to_string()]);
        Ok(tree)
    }
    fn commit_all(&self, wt: &TaskWorktree, msg: &str) -> PortResult<()> {
        self.inner.commit_all(wt, msg)
    }
    fn has_changes(&self, wt: &TaskWorktree) -> PortResult<bool> {
        self.inner.has_changes(wt)
    }
    fn diff(&self, wt: &TaskWorktree) -> PortResult<String> {
        self.inner.diff(wt)
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        self.inner.integrate(ids)
    }
    fn discard(&self, int: &Integration) -> PortResult<()> {
        self.inner.discard(int)
    }
    fn rebase(&self, wt: &TaskWorktree) -> PortResult<Vec<String>> {
        self.inner.rebase(wt)
    }
    fn base_sha(&self) -> PortResult<String> {
        self.inner.base_sha()
    }
    fn merge(
        &self,
        wt: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        self.inner.merge(wt, regate)
    }
    fn open_pr(
        &self,
        wt: &TaskWorktree,
        title: &str,
        receipt: &Receipt,
        note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        self.inner.open_pr(wt, title, receipt, note)
    }
    fn merge_pr(&self, _wt: &TaskWorktree, _url: &str, _head: &str) -> PortResult<PrMergeOutcome> {
        self.merge_pr_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PrMergeOutcome::Blocked {
            code: "unexpected".into(),
            reason: "should not call merge_pr".into(),
            receipt: None,
        })
    }
    fn fetch_base(&self) -> PortResult<()> {
        self.inner.fetch_base()
    }
    fn is_merged(&self, id: &str) -> PortResult<bool> {
        self.inner.is_merged(id)
    }
    fn cleanup(&self, id: &str) -> PortResult<()> {
        self.inner.cleanup(id)
    }
}

fn write_needs_human_proof(data_dir: &Path, wt_dir: &Path, task_id: &str, flow_name: &str) {
    fs::create_dir_all(wt_dir).unwrap();
    let canon_wt = fs::canonicalize(wt_dir).unwrap();
    let tree_fp = format!("git-tree:{task_id}");
    let bytes = valid_png();
    let img_path = captured_proof_image_path(data_dir, &canon_wt, &tree_fp);
    fs::create_dir_all(img_path.parent().unwrap()).unwrap();
    fs::write(&img_path, &bytes).unwrap();
    let canon_img = fs::canonicalize(&img_path).unwrap();
    let record = ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION_V2,
        repo_root: canon_wt.to_string_lossy().into_owned(),
        tree_fp,
        image_path: canon_img.to_string_lossy().into_owned(),
        image_sha256: aaf_fp::sha256_hex(&bytes),
        width: 1080,
        height: 1920,
        byte_len: u64::try_from(bytes.len()).unwrap(),
        source_tool: HOST_PROOF_FLOWS_TOOL.to_string(),
        device_serial: HOST_PROOF_FLOWS_DEVICE.to_string(),
        task_id: Some(task_id.to_string()),
        recorded_at: 1000,
        base_sha: Some("deadbeef".to_string()),
        flows: vec![FlowResult {
            name: flow_name.to_string(),
            passed: true,
            needs_human: true,
            assertions: vec![AssertionResult {
                name: "visual_ok".to_string(),
                passed: true,
                detail: String::new(),
            }],
            duration_ms: 15,
        }],
    };
    aaf_fp::write_proof(data_dir, &record).unwrap();
}

#[test]
fn needs_human_flow_keeps_task_in_review_even_with_auto_land() {
    let mut world = World::new();
    world.auto_merge = true;
    world.repo.run.land = LandMode::Auto;
    world.repo.proof.required = true;
    world.add("t1");
    world.runner.script("t1", vec![ok_outcome()]);

    let wt_root = world.tmp.path().join("worktrees");
    write_needs_human_proof(
        world.tmp.path(),
        &wt_root.join("t1"),
        "t1",
        "checkout_visual",
    );

    let disk_git = Arc::new(DiskWorktreeGit {
        inner: Arc::clone(&world.git),
        wt_root,
        merge_pr_calls: AtomicUsize::new(0),
    });
    let mut deps = world.deps();
    deps.git = Arc::clone(&disk_git) as Arc<dyn GitOps>;
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(world.state("t1"), TaskState::Review);
    assert!(
        world.git.0.lock().unwrap().merged.is_empty(),
        "needs_human proof flow must block auto-merge"
    );
    let notes = world.task("t1").context_notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("proof flow checkout_visual needs human review")),
        "expected context note naming flow, got {notes:?}"
    );
}

#[test]
fn needs_human_flow_opens_pr_without_merging_in_pr_merge_mode() {
    let mut world = World::new();
    world.repo.run.land = LandMode::PrMerge;
    world.repo.proof.required = true;
    world.git.0.lock().unwrap().pr = Some(PrOutcome::Opened {
        url: "https://github.com/o/r/pull/9".to_string(),
    });
    world.add("t1");
    world.runner.script("t1", vec![ok_outcome()]);

    let wt_root = world.tmp.path().join("worktrees");
    write_needs_human_proof(world.tmp.path(), &wt_root.join("t1"), "t1", "manual_qa");

    let disk_git = Arc::new(DiskWorktreeGit {
        inner: Arc::clone(&world.git),
        wt_root,
        merge_pr_calls: AtomicUsize::new(0),
    });
    let mut deps = world.deps();
    deps.git = Arc::clone(&disk_git) as Arc<dyn GitOps>;
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(world.state("t1"), TaskState::Review);
    assert_eq!(disk_git.merge_pr_calls.load(Ordering::SeqCst), 0);
    assert_eq!(world.git.0.lock().unwrap().pr_notes.len(), 1);
    let notes = world.task("t1").context_notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("proof flow manual_qa needs human review")),
        "expected context note naming flow, got {notes:?}"
    );
}
