#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Branch coverage for the SEC-015 `secret_guard` option-parsing fix: inline
//! `base64` input forms and inline expression/file flags are blocked on secret
//! files, while the same flags on ordinary files stay allowed.

mod common;

use common::{Fixture, run_corpus};

const SECRET: Option<&str> = Some("secret_guard");

fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write(".env", "KEY=supersecret\n");
    fx.write("README.md", "KEY=public\n");
    fx.write("src/main.rs", "const KEY: &str = \"x\";\n");
    fx
}

#[test]
fn sec015_blocks_inline_input_and_file_flags_on_secret_files() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("base64 --input=.env", SECRET),
            ("base64 --input .env", SECRET),
            ("base64 -i.env", SECRET),
            ("grep --file=.env README.md", SECRET),
            ("grep -f.env README.md", SECRET),
            ("rg --file=.env src/main.rs", SECRET),
            ("sed --file=.env README.md", SECRET),
            ("awk -f.env README.md", SECRET),
            ("grep -e KEY .env", SECRET),
            ("grep -i KEY .env", SECRET),
            ("cut -f1 .env", SECRET),
            ("cut -f 1 .env", SECRET),
        ],
    );
}

#[test]
fn sec015_allows_same_flags_on_ordinary_files() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("base64 -i README.md", None),
            ("base64 --input=README.md", None),
            ("base64 -iREADME.md", None),
            ("grep --regexp=KEY src/main.rs", None),
            ("grep -eKEY README.md", None),
            ("grep -e KEY src/lib.rs", None),
            ("grep -i .env README.md", None),
            ("rg --regexp=KEY src/main.rs", None),
            ("sed --expression=p README.md", None),
            ("sed -i.bak s/a/b/ README.md", None),
            ("grep --file=README.md src/main.rs", None),
            ("grep -fREADME.md src/main.rs", None),
            ("grep --files-with-matches KEY src/main.rs", None),
            ("cut -f1 README.md", None),
            ("cut -f 1 README.md", None),
        ],
    );
}

#[test]
fn sec015_r2_blocks_clusters_abbreviations_and_sed_in_place() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            // 1. `-e` is only an expression flag for pattern/script commands.
            ("cat -e .env", SECRET),
            ("xxd -e .env", SECRET),
            // 2. Short clusters ending in / containing `e` or `f`.
            ("grep -ieKEY .env", SECRET),
            ("egrep -ie KEY .env", SECRET),
            ("grep -if .env README.md", SECRET),
            ("grep -if.env README.md", SECRET),
            ("rg -if.env README.md", SECRET),
            ("sed -nep .env", SECRET),
            ("sed -nfscript.sed .env", SECRET),
            // 3. `base64` input clusters and getopt_long abbreviations.
            ("base64 -di.env", SECRET),
            ("base64 -Di.env", SECRET),
            ("base64 -di .env", SECRET),
            ("base64 --in=.env", SECRET),
            ("base64 --i=.env", SECRET),
            ("base64 --inp=.env", SECRET),
            ("grep --reg=KEY .env", SECRET),
            ("sed --expr=p .env", SECRET),
            // 4. `sed -i` never swallows the next token. `secret_guard` refuses
            // reading `.env` before `delete_guard` sees the in-place write.
            ("sed -i -ep .env", SECRET),
            ("sed -i --expression=p .env", SECRET),
            ("sed -i 'w /tmp/leak' .env", SECRET),
            ("sed -i -f.env README.md", SECRET),
            ("sed -i 'p' .env", SECRET),
            ("sed -i '' -e p .env", SECRET),
            ("sed -l p .env", SECRET),
            // 5. gawk/awk program-text and program-file flags.
            ("gawk --source='{print}' .env", SECRET),
            ("gawk --exec=s.awk .env", SECRET),
            ("gawk --exec=.env", SECRET),
            ("gawk -E .env", SECRET),
            ("gawk -i .env '{print}' README.md", SECRET),
            ("awk -f prog.awk -e .env", SECRET),
            ("awk -f prog.awk --source .env", SECRET),
            // BSD awk ignores `-xv`; BSD getopt stops at the first operand.
            ("awk -xv '{print}' .env", SECRET),
            ("gawk -dv '{print}' .env", SECRET),
            ("sed p -e .env", SECRET),
            ("head README.md -n .env", SECRET),
            // BSD `--context` takes an optional value: KEY is the pattern.
            ("grep --context KEY .env", SECRET),
        ],
    );
}

#[test]
fn sec015_r2_allows_same_forms_on_ordinary_files() {
    let fx = fixture();
    run_corpus(
        &fx,
        &[
            ("cat -e README.md", None),
            ("xxd -e README.md", None),
            ("grep -ieKEY README.md", None),
            ("grep -ie .env README.md", None),
            ("rg -ief.env README.md", None),
            ("grep -if README.md src/main.rs", None),
            ("grep -C 2 KEY README.md", None),
            ("grep --reg=.env README.md", None),
            ("sed -nep README.md", None),
            ("sed -i '' 's/a/b/' README.md", None),
            ("sed -i.bak 's/a/b/' README.md", None),
            ("sed -i .bak 's/a/b/' README.md", None),
            ("sed -i 's/a/b/' README.md", None),
            ("base64 -di README.md", None),
            ("base64 --in=README.md", None),
            ("gawk --source='{print}' README.md", None),
            ("gawk -i inplace '{print}' README.md", None),
            ("awk -F= '{print $1}' README.md", None),
        ],
    );
}
