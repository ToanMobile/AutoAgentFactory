#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-2 audit of `data_dir_guard`: a path glued to a short option, an
//! operand read relative to a directory-changing option, and a path that
//! only a stripped wrapper names. Each group ends with the calls that must
//! stay allowed.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_hook::paths::normalize;
use common::{Fixture, run_corpus};

const DATA: Option<&str> = Some("data_dir_guard");
const ALLOW: Option<&str> = None;

/// The data dir is `../data` from the repository root, and `data_link` in it.
fn fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("notes.txt", "x");
    fixture.write("build/out.txt", "x");
    fs::write(fixture.home.join("notes.txt"), "x").unwrap();
    symlink(&fixture.data, fixture.root.join("data_link")).unwrap();
    symlink(&fixture.home, fixture.root.join("home_link")).unwrap();
    fixture
}

#[test]
fn a_path_glued_to_a_short_option_is_checked() {
    run_corpus(
        &fixture(),
        &[
            ("tar -C./data_link -cf - aaf.db", DATA),
            ("tar -Cdata_link -cf - aaf.db", DATA),
            ("tar -cf - -C../data aaf.db", DATA),
            ("tar -C{data} -cf - aaf.db", DATA),
            ("tar -cfdata_link/x.tar notes.txt", DATA),
            ("tar -xzfdata_link/x.tar", DATA),
            ("tar -xf x.tar -Idata_link/prog", DATA),
            ("grep -fdata_link/patterns notes.txt", DATA),
            ("grep -rnfdata_link/patterns .", DATA),
            ("make -C../data all", DATA),
            ("git -C../data log", DATA),
            ("cp -t../data notes.txt", DATA),
            // allowed
            ("tar -C./home_link -cf - notes.txt", ALLOW),
            ("tar -Chome_link -cf - notes.txt", ALLOW),
            ("tar -cf/tmp/x.tar notes.txt", ALLOW),
            ("tar -xzfbuild/x.tar", ALLOW),
            ("grep -fbuild/patterns notes.txt", ALLOW),
            ("make -Cbuild all", ALLOW),
            ("ls -la", ALLOW),
            ("rm -rf build", ALLOW),
            ("grep -rn data .", ALLOW),
        ],
    );
}

#[test]
fn an_operand_is_read_from_the_directory_the_tool_changes_to() {
    run_corpus(
        &fixture(),
        &[
            ("tar -C .. -cf - data/aaf.db", DATA),
            ("tar -C.. -cf - data/aaf.db", DATA),
            ("tar -cf - -C .. data", DATA),
            ("tar --directory=.. -cf - data/aaf.db", DATA),
            ("tar --directory .. -cf - data", DATA),
            ("tar -C .. -cf - ./data/receipts", DATA),
            // Each `-C` is relative to the one before it.
            ("tar -C .. -C data -cf - aaf.db", DATA),
            ("tar -C .. --file=data/x.tar -x", DATA),
            ("make -C .. -f data/Makefile", DATA),
            ("env -C .. cat data/aaf.db", DATA),
            ("env --chdir=.. cat data/aaf.db", DATA),
            // allowed
            ("tar -C .. -cf - repo/notes.txt", ALLOW),
            ("tar -C .. -cf - database", ALLOW),
            ("tar -C build -cf - out.txt", ALLOW),
            ("tar --directory=.. -cf - home", ALLOW),
            ("make -C build all", ALLOW),
            ("env -C .. ls repo", ALLOW),
            ("rustc -C opt-level=3 src/main.rs", ALLOW),
        ],
    );
}

#[test]
fn a_path_given_to_a_stripped_wrapper_is_checked() {
    run_corpus(
        &fixture(),
        &[
            ("env -C ../data cat aaf.db", DATA),
            ("env -C data_link cat aaf.db", DATA),
            ("env --chdir=../data cat aaf.db", DATA),
            ("env --chdir ../data cat aaf.db", DATA),
            ("sudo -D ../data cat aaf.db", DATA),
            ("sudo --chdir=../data cat aaf.db", DATA),
            ("xargs -a ../data/list cat", DATA),
            ("xargs --arg-file=../data/list cat", DATA),
            // allowed
            ("env -C build cat out.txt", ALLOW),
            ("env -C home_link cat notes.txt", ALLOW),
            ("sudo -D build cat out.txt", ALLOW),
            ("xargs -a build/out.txt cat", ALLOW),
        ],
    );
}

#[test]
fn a_leading_parent_dir_survives_normalisation() {
    let norm = |raw: &str| normalize(Path::new(raw));
    assert_eq!(norm("../data/aaf.db"), Path::new("../data/aaf.db"));
    assert_eq!(norm("./../data/./aaf.db"), Path::new("../data/aaf.db"));
    assert_eq!(norm("a/../../b"), Path::new("../b"));
    assert_eq!(norm("../../a/../b"), Path::new("../../b"));
    // Nothing is above the root, and inside a known base `..` still resolves.
    assert_eq!(norm("/../a"), Path::new("/a"));
    assert_eq!(norm("/a/b/../c"), Path::new("/a/c"));
    assert_eq!(norm("a/b/../c"), Path::new("a/c"));
}
