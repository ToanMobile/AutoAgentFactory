#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for GATE-131 (over-blocks added by the SEC-082 fix that
//! `main` allowed):
//! - `ln -t<dir>/` clusters: every tail of the cluster ends in `/`, and the
//!   tails alone used to exceed the target-directory cap;
//! - compressor option env vars whose value only sets a level or a thread
//!   count (`GZIP=-9`, `XZ_OPT=-T0`) were denied wholesale.
//!
//! Every deny row of the SEC-082 oracles still holds; the rows below pin the
//! env-value and cluster cases that must stay denied.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".env", "K=V\n");
    fx.write(".claude/settings.json", "{}\n");
    fx.write(".claude/settings", "x\n");
    fx.write(".claude/x.gz", "x\n");
    fx.write("build/a", "plain\n");
    fx.write("build/k", "x\n");
    fx.write("a", "x\n");
    fx
}

#[test]
fn ln_target_dir_cluster_rows_are_allowed() {
    run_corpus(
        &fixture(),
        &[
            ("ln -tbuild/ a", ALLOW),
            ("ln -s -tbuild/ ../a", ALLOW),
            ("ln -sftbuild/ ../a", ALLOW),
            ("ln -s --target-directory=build/ ../a", ALLOW),
        ],
    );
}

#[test]
fn level_only_compressor_env_rows_are_allowed() {
    run_corpus(
        &fixture(),
        &[
            ("GZIP=-9 gzip build/a", ALLOW),
            ("GZIP=--best gzip build/a", ALLOW),
            ("env GZIP=-1 gzip build/a", ALLOW),
            ("XZ_OPT=-T0 xz build/a", ALLOW),
            ("XZ_OPT='-9 -T4' xz build/a", ALLOW),
            ("BZIP2=-9 bzip2 build/a", ALLOW),
        ],
    );
}

#[test]
fn env_and_cluster_rows_stay_denied() {
    run_corpus(
        &fixture(),
        &[
            ("GZIP=.env gzip", DELETE),
            ("GZIP=-9 gzip .env", DELETE),
            ("BZIP2=-9 bzip2 .env", DELETE),
            ("GZIP=-S.json gzip -f .claude/settings", DELETE),
            ("GZIP='-9 -S .json' gzip -f .claude/settings", DELETE),
            ("GZIP=-9S.json gzip -f .claude/settings", DELETE),
            ("GZIP=-dN gunzip -f .claude/x.gz", DELETE),
            ("GZIP=-r gzip .", DELETE),
            ("GZIP=--suffix=.json gzip -f .claude/settings", DELETE),
            ("XZ_OPT=--suffix=.json xz -f .claude/settings", DELETE),
            ("XZ_OPT='-T0 -S .json' xz -f .claude/settings", DELETE),
            ("GZIP=$OPTS gzip build/a", DELETE),
            ("GZIP=\"$OPTS\" gzip build/a", DELETE),
            ("GZIP=-9 gzip -f -S .json .claude/settings", DELETE),
            ("ln -sft.claude /tmp/x/settings.json", DELETE),
            ("ln -t.claude/ /tmp/x/settings.json", DELETE),
            ("ln -sftbuild/ .env", DELETE),
            ("ln -tbuild/ -sf /tmp/x .env", DELETE),
            ("ln -sfS.bak build/a .env", DELETE),
        ],
    );
}
