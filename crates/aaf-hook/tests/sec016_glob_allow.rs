#![allow(clippy::unwrap_used, clippy::expect_used)]

//! SEC-016 companion: parent-directory globs that cannot name a secret file
//! stay allowed, next to deny rows in the same fixture so the allows are not
//! vacuous.

mod common;

use std::path::Path;

use aaf_hook::HookEnv;
use aaf_hook::host::{HookHost, decide_for_host};
use common::{Fixture, rule, run_corpus, run_file_corpus};
use serde_json::{Value, json};

const SECRET: Option<&str> = Some("secret_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write("app/.env", "KEY=super-secret\n");
    fx.write("app/.env.example", "KEY=\n");
    fx.write("app/README.md", "# app\n");
    fx.write("app/Cargo.toml", "[package]\n");
    fx.write("app/id_ed25519", "PRIVATE\n");
    fx.write("app/server.pem", "PRIVATE\n");
    fx.write("app/keystore.properties", "storePassword=x\n");
    fx.write("src/a.rs", "fn a() {}\n");
    fx.write("src/sub/b.rs", "fn b() {}\n");
    fx.write("docs/guide.md", "# guide\n");
    fx.write("app/.env.production", "KEY=prod-secret\n");
    // A real directory whose name holds glob characters (Next.js routes).
    fx.write("web/[id]/.env.qa", "KEY=qa-secret\n");
    fx.write("web/[id]/page.tsx", "export {}\n");
    fx
}

fn gemini_read_many(fx: &Fixture, paths: &[&str]) -> Option<&'static str> {
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
    let payload: Value = json!({
        "hook_event_name": "BeforeTool",
        "tool_name": "read_many_files",
        "tool_input": { "paths": paths },
        "cwd": fx.root,
    });
    let (dec, host) = decide_for_host(payload.to_string().as_bytes(), &env, HookHost::Gemini);
    assert_eq!(host, HookHost::Gemini);
    rule(&dec)
}

#[test]
fn bash_parent_dir_globs_allow_public_files_and_deny_secrets() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("cat src/*.rs", None),
            ("cat */README.md", None),
            ("cat */.env.example", None),
            ("head */Cargo.toml", None),
            ("grep foo src/*/*.rs", None),
            ("cat docs/*.md", None),
            ("cat */*.rs", None),
            ("cat */.env", SECRET),
            ("cat */.env*", SECRET),
            ("cat */id_ed25519", SECRET),
            ("cat */*.pem", SECRET),
            ("cat */keystore.*", SECRET),
            ("cat */.env.p*", SECRET),
            ("cat web/[id]/.env.q*", SECRET),
            ("cat web/[id]/*.tsx", None),
            ("cat docs/*", None),
        ],
    );
    run_file_corpus(
        &fx,
        &[
            ("Read", "*/.env*", SECRET),
            ("Read", "*/README.md", None),
            ("Read", "web/[id]/page.tsx", None),
        ],
    );
}

#[cfg(unix)]
#[test]
fn parent_dir_globs_walk_the_tree_and_follow_symlinks() {
    let fx = Fixture::new();
    fx.write("app/sub/.env", "KEY=nested\n");
    fx.write("app/main.rs", "fn main() {}\n");
    fx.write("app/.env.example", "KEY=\n");
    fx.write("keys/server.pem", "PRIVATE\n");
    fx.write("keys/id_rsa.bak", "PRIVATE\n");
    fx.write("qa/.env.qa", "KEY=qa\n");
    fx.write("cfg/.env", "KEY=cfg\n");
    fx.write("docs/guide.md", "# guide\n");
    std::os::unix::fs::symlink(".env", fx.root.join("cfg/public.txt")).unwrap();
    run_corpus(
        &fx,
        &[
            ("cat */server*.pem", SECRET),
            ("cat */server*.p?m", SECRET),
            ("cat **/s*.pe*", SECRET),
            ("cat */id_rsa.*", SECRET),
            ("cat **/id_rsa?bak", SECRET),
            ("cat */.env.q*", SECRET),
            ("cat */.env.??", SECRET),
            ("cat */.env?qa", SECRET),
            ("cat */public.txt", SECRET),
            ("cat */*.rs", None),
            ("cat */.env.example", None),
            ("cat docs/*", None),
        ],
    );
    assert_eq!(gemini_read_many(&fx, &["**"]), SECRET);
    assert_eq!(gemini_read_many(&fx, &["app/**"]), SECRET);
    assert_eq!(gemini_read_many(&fx, &["*/public.txt"]), SECRET);
    assert_eq!(
        gemini_read_many(&fx, &["docs/**", "**/*.rs", "*/*.md"]),
        None
    );
}

#[test]
fn gemini_read_many_files_parent_dir_globs_allow_sources_and_deny_secrets() {
    let fx = fixture();
    assert_eq!(gemini_read_many(&fx, &["src/**/*.rs"]), None);
    assert_eq!(gemini_read_many(&fx, &["*/README.md", "docs/*.md"]), None);
    assert_eq!(gemini_read_many(&fx, &["*/.env.example"]), None);
    assert_eq!(gemini_read_many(&fx, &["src/**/*.rs", "*/.env*"]), SECRET);
    assert_eq!(gemini_read_many(&fx, &["**/*.pem"]), SECRET);
}
