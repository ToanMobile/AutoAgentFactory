#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use aaf_hook::HookEnv;
use aaf_hook::host::{HookHost, decide_for_host};
use common::{Fixture, rule, run_corpus};
use serde_json::json;

const SECRET: Option<&str> = Some("secret_guard");

fn fixture_with_nested_secrets() -> Fixture {
    let fx = Fixture::new();
    fx.write("app/.env", "KEY=super-secret\n");
    fx.write("app/.env.local", "LOCAL_KEY=super-secret\n");
    fx
}

#[test]
fn bash_parent_dir_glob_blocks_nested_secret_files() {
    let fx = fixture_with_nested_secrets();
    run_corpus(
        &fx,
        &[
            ("cat */.env", SECRET),
            ("head app*/.env.local", SECRET),
            ("grep KEY */.env", SECRET),
        ],
    );
}

#[test]
fn gemini_read_many_files_parent_dir_glob_blocks_nested_secret_files() {
    let fx = fixture_with_nested_secrets();
    let root = fx.root.clone();
    let lookup = move |_: &Path| Some(root.clone());
    let config = |_: &Path| fx.git_config.borrow().clone();
    let env = HookEnv {
        repo_root: &lookup,
        git_config: &config,
        data_dir: Some(fx.data.clone()),
        config_dir: Some(fx.config.clone()),
        home: Some(fx.home.clone()),
        cwd: None,
        in_factory_run: true,
        trusted_aaf_bins: Vec::new(),
    };
    let payload = json!({
        "hook_event_name": "BeforeTool",
        "tool_name": "read_many_files",
        "tool_input": { "paths": ["*/.env*"] },
        "cwd": fx.root,
    });
    let (dec, host) = decide_for_host(payload.to_string().as_bytes(), &env, HookHost::Gemini);
    assert_eq!(host, HookHost::Gemini);
    assert_eq!(rule(&dec), SECRET);
}
