//! Regression tests of the SOP engine.

use super::runner::{list_checkpoints, load_checkpoint, save_checkpoint};
use super::*;
use aaf_domain::sop::SopCheckpoint;

#[test]
fn load_repo_sops_rejects_duplicate_spec_name() {
    let repo = tempfile::tempdir().unwrap();
    let first = init_sop_file(repo.path(), "bugfix", Some("dup-sop"), false).unwrap();
    let copy = first.with_file_name("dup-sop-copy.toml");
    fs::copy(&first, &copy).unwrap();
    let err = load_repo_sops(repo.path()).unwrap_err();
    assert!(matches!(err, SopError::InvalidInput(msg) if msg.contains("duplicate SOP")));
}

#[cfg(unix)]
#[test]
fn allows_symlinked_ancestors_while_refusing_managed_symlinks() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let real_work = tmp.path().join("real_work");
    let real_repo = real_work.join("repo");
    let real_data = real_work.join("data");
    fs::create_dir_all(&real_repo).unwrap();
    fs::create_dir_all(&real_data).unwrap();
    let link_work = tmp.path().join("work_link");
    symlink(&real_work, &link_work).unwrap();

    let linked_repo = link_work.join("repo");
    let linked_data = link_work.join("data");
    let sop_path = init_sop_file(&linked_repo, "bugfix", None, false).unwrap();
    assert!(sop_path.is_file());
    assert_eq!(load_repo_sops(&linked_repo).unwrap().len(), 1);

    let spec = find_builtin("bugfix").unwrap();
    let cp = SopCheckpoint::new("run-sym-anc".into(), &spec, "/repo".into(), 10);
    save_checkpoint(&linked_data, &cp).unwrap();
    assert_eq!(
        load_checkpoint(&linked_data, "run-sym-anc").unwrap().run_id,
        "run-sym-anc"
    );
    assert_eq!(list_checkpoints(&linked_data).unwrap().len(), 1);

    // Managed `.autofactory/sops` symlink MUST still be rejected.
    let bad_repo = tmp.path().join("bad_repo");
    fs::create_dir_all(bad_repo.join(".autofactory")).unwrap();
    let outside_dir = tmp.path().join("outside_sops");
    fs::create_dir_all(&outside_dir).unwrap();
    symlink(&outside_dir, bad_repo.join(REPO_SOPS_REL)).unwrap();
    assert!(matches!(
        load_repo_sops(&bad_repo),
        Err(SopError::InvalidPath(_))
    ));
    assert!(matches!(
        init_sop_file(&bad_repo, "bugfix", None, false),
        Err(SopError::InvalidPath(_))
    ));

    // Managed `<data_dir>/sops` symlink MUST still be rejected.
    let bad_data = tmp.path().join("bad_data");
    fs::create_dir_all(&bad_data).unwrap();
    symlink(&outside_dir, bad_data.join("sops")).unwrap();
    assert!(matches!(
        save_checkpoint(&bad_data, &cp),
        Err(SopError::InvalidPath(_))
    ));
}
