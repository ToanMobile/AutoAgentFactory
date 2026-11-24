use std::path::{Path, PathBuf};

use super::*;
use crate::HookEnv;

/// E10: each distinct written path is judged once, however many calls
/// write it and however it is spelled.
#[test]
fn judges_each_distinct_path_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let lookup = |_: &Path| -> Option<PathBuf> { None };
    let config = |_: &Path| -> Option<String> { None };
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: None,
        config_dir: None,
        home: None,
        cwd: None,
    };
    let ctx = Ctx::new(&env, root);
    let mut code = String::from("p = 'build/out.txt'\n");
    for n in 0..1500 {
        code.push_str(&format!("open(p, 'a').write('{n}')\n"));
        code.push_str("open('./build/out.txt', 'w').write('x')\n");
        code.push_str(&format!("open('build/f{}.txt', 'w').write('x')\n", n % 3));
    }
    JUDGE_RUNS.with(|runs| runs.set(0));
    assert!(check_written("python3", &code, &ctx).is_none());
    // `build/out.txt` and `build/f0.txt` … `build/f2.txt`.
    assert_eq!(JUDGE_RUNS.with(std::cell::Cell::get), 4);
}

/// O11-14: an expression written many times adds its paths once.
#[test]
fn repeated_write_expression_adds_its_paths_once() {
    let mut code = String::new();
    for n in 0..500 {
        code.push_str(&format!("p = 'src/f{n}.py'\nopen(p, 'w').write('x')\n"));
    }
    let script = Script::new(&code);
    let writes = written(&script, &calls(&script));
    assert!(writes.paths.len() <= 501, "{}", writes.paths.len());
}
