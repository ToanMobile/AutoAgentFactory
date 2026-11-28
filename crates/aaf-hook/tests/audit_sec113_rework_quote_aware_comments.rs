#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Rework oracle for SEC-113 (Mẫu C findings F7, F8):
//! 1. The trailing-comment pass must not cut at a `#` or `//` that sits
//!    inside a string literal (`'http://…'`, `'#'`): that cut leaves an
//!    unbalanced quote which hides the payload on the next line again.
//! 2. Block comments (`/* don't */`) in JS and PHP, and a PHP line comment
//!    ended by `?>` (the code after `<?php` runs), must not hide a payload
//!    behind the odd quote they hold.
//!
//! Benign scripts with `//` or `#` inside strings, followed by a legitimate
//! call, stay allowed.

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const INTERP: Option<&str> = Some("interp_guard");

#[test]
fn hash_or_slashes_inside_a_string_do_not_hide_the_next_line() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"u = 'http://example.com' # don't\nimport os; os.system('git push --force')\"",
                INTERP,
            ),
            (
                "python3 -c \"s = '#' # it's\nimport os; os.system('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"const u = 'https://x.io' // don't\nrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn block_comments_and_php_close_tag_do_not_hide_the_payload() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "node -e \"/* don't */ require('child_process').execSync('git push --force')\"",
                INTERP,
            ),
            (
                "php -r '/* don'\\''t */ system(\"git push --force\");'",
                INTERP,
            ),
            (
                "php -r '// don'\\''t ?><?php system(\"git push --force\");'",
                INTERP,
            ),
        ],
    );
}

#[test]
fn the_same_payloads_without_the_comment_are_already_denied() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"u = 'http://example.com'\nimport os; os.system('git push --force')\"",
                INTERP,
            ),
            (
                "node -e \"const u = 'https://x.io'\nrequire('child_process').execSync('git push --force')\"",
                INTERP,
            ),
            ("php -r 'system(\"git push --force\");'", INTERP),
            (
                "python3 -c \"import os # don't\nos.system('git push --force')\"",
                INTERP,
            ),
        ],
    );
}

#[test]
fn strings_holding_hash_or_slashes_before_a_legitimate_call_stay_allowed() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            (
                "python3 -c \"u = 'http://example.com/a#b'\nimport subprocess; subprocess.run(['ls', '-la'])\"",
                ALLOW,
            ),
            (
                "python3 -c \"u = 'http://example.com'\nimport shutil; shutil.rmtree('build/tmp')\"",
                ALLOW,
            ),
            (
                "ruby -e \"x = 1; puts \\\"v=#{x}\\\"\nrequire 'fileutils'; FileUtils.rm_rf('tmp/x')\"",
                ALLOW,
            ),
            (
                "node -e \"const c = '#fff'; const u = 'https://a.b'\nrequire('fs').rmSync('build/tmp', {recursive: true})\"",
                ALLOW,
            ),
            (
                "python3 -c \"print('a#b') # it's\nimport subprocess; subprocess.run(['git', 'status'])\"",
                ALLOW,
            ),
            (
                "python3 -c \"u = 'http://x'\nimport os; os.system('git status')\"",
                ALLOW,
            ),
            (
                "node -e \"console.log('https://a.b/c#frag') // done\"",
                ALLOW,
            ),
            ("python3 -c \"print('#1 item') # it's fine\"", ALLOW),
        ],
    );
}
