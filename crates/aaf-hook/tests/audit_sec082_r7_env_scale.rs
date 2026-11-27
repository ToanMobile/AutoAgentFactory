#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-082 (rework 2 residuals):
//! - option env vars carrying operands when the command line has no operands
//!   (`BZIP2=.env bzip2`, `env BZIP2=.env bzip2`, `GZIP=.env gzip`) must be
//!   denied by `delete_guard`;
//! - sibling compressor tools (`pzstd --rm`, `zstdcat -o`) targeting protected
//!   files must be denied by `delete_guard`;
//! - repeated `-S` suffixes with many operands must scale linearly rather than
//!   quadratically;
//! - legitimate compressor invocations on non-protected files must stay
//!   allowed.

mod common;

use std::fmt::Write as _;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

#[test]
fn env_carried_and_sibling_compressor_rows_are_denied() {
    let fx = Fixture::new();
    fx.write(".env", "K=V\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write("build/a.zst", "compressed\n");
    run_corpus(
        &fx,
        &[
            ("BZIP2=.env bzip2", DELETE),
            ("env BZIP2=.env bzip2", DELETE),
            ("GZIP=.env gzip", DELETE),
            ("pzstd --rm .env", DELETE),
            ("zstdcat build/a.zst -o .env", DELETE),
            ("zstdcat -f build/a.zst -o .claude/settings.json", DELETE),
        ],
    );
}

#[test]
fn many_suffixes_do_not_blow_up() {
    let fx = Fixture::new();
    fx.write("build/f", "data\n");
    let build_cmd = |n: usize| -> String {
        let mut cmd = String::from("gzip");
        for i in 0..n {
            let _ = write!(cmd, " -S .s{i}");
        }
        for _ in 0..n {
            cmd.push_str(" build/f");
        }
        cmd
    };
    let cmds: Vec<(usize, String)> = [100, 800].into_iter().map(|n| (n, build_cmd(n))).collect();
    common::assert_scales(100, 800, 24, |n| {
        let (_, cmd) = cmds.iter().find(|(k, _)| *k == n).unwrap();
        let _ = common::rule(&fx.bash(cmd));
    });
}

#[test]
fn legit_compressor_rows_stay_allowed() {
    let fx = Fixture::new();
    fx.write("build/a", "plain\n");
    fx.write("build/a.gz", "gz\n");
    fx.write("file.gz", "gz\n");
    fx.write("build/a.zst", "zst\n");
    run_corpus(
        &fx,
        &[
            ("gzip -S .bak build/a", ALLOW),
            ("zstd -19 --long=27 build/a", ALLOW),
            ("gzip -dc file.gz", ALLOW),
            ("zcat build/a.gz", ALLOW),
            ("zstd -d build/a.zst -o build/b", ALLOW),
        ],
    );
}
