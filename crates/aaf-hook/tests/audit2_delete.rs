#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-2 audit of `delete_guard`: a deleting tool hidden behind a wrapper
//! after `find -exec`, behind `xargs`, or behind a wrapper option that takes
//! a value. Each group ends with the calls that must stay allowed.

mod common;

use common::{Fixture, run_corpus};

const DELETE: Option<&str> = Some("delete_guard");
const ALLOW: Option<&str> = None;

fn fixture() -> Fixture {
    let fixture = Fixture::new();
    for file in [".env", "app/release.keystore", "notes.txt", "build/a.tmp"] {
        fixture.write(file, "x");
    }
    fixture
}

#[test]
fn find_exec_sees_the_tool_behind_a_wrapper() {
    run_corpus(
        &fixture(),
        &[
            ("find . -name .env -exec env rm {} +", DELETE),
            ("find . -name .env -exec env -i PATH=/bin rm {} \\;", DELETE),
            ("find . -name .env -exec env -u FOO rm {} \\;", DELETE),
            ("find . -name .env -exec command rm {} \\;", DELETE),
            ("find . -name .env -exec nice -n 5 rm {} \\;", DELETE),
            ("find . -name .env -exec nohup rm {} \\;", DELETE),
            ("find . -name .env -exec sudo -u root rm {} +", DELETE),
            (
                "find . -name .env -exec /usr/bin/env /bin/rm -f {} +",
                DELETE,
            ),
            ("find . -name .env -exec timeout 5 rm {} \\;", DELETE),
            ("find . -name .env -ok env rm {} \\;", DELETE),
            ("find . -name '*.jks' -execdir env unlink {} \\;", DELETE),
            (
                "find . -name '*.jks' -exec env sh -c 'rm \"$1\"' _ {} \\;",
                DELETE,
            ),
            ("find . -name .env -exec env -S 'rm -f' {} \\;", DELETE),
            // A tool chosen at run time could be a deleting one.
            ("find . -name .env -exec $RM {} \\;", DELETE),
            ("find . -name .env -exec env $TOOL {} +", DELETE),
            ("find . -name .env -exec tee {} \\;", DELETE),
            // allowed
            ("find . -name '*.tmp' -exec env rm {} +", ALLOW),
            ("find . -name '*.tmp' -exec nice -n 5 rm -f {} \\;", ALLOW),
            ("find . -name .env -exec env cat {} \\;", ALLOW),
            ("find . -name .env -exec nice grep -l KEY {} +", ALLOW),
            ("find . -name .env -exec env LC_ALL=C wc -c {} +", ALLOW),
            ("find /tmp/elsewhere -name .env -exec env rm {} +", ALLOW),
        ],
    );
}

#[test]
fn xargs_deletes_only_what_a_find_filter_proves_unprotected() {
    run_corpus(
        &fixture(),
        &[
            ("find . -name .env | xargs rm", DELETE),
            ("find . -name .env -print0 | xargs -0 rm -f", DELETE),
            ("find . -type f | xargs rm", DELETE),
            ("find . -name '.env*' | xargs -n 1 unlink", DELETE),
            ("find . -name '*.jks' | xargs -I{} mv {} /tmp/", DELETE),
            ("find . -name .env | xargs truncate -s 0", DELETE),
            ("find . -name .env | xargs shred -u", DELETE),
            ("find . -name .env | xargs env rm", DELETE),
            ("find . -name .env | xargs sh -c 'rm \"$@\"' _", DELETE),
            ("find . -name .env | xargs --max-args 1 rm", DELETE),
            ("find . -name .env | xargs $RM", DELETE),
            ("find . -name .env |& xargs rm", DELETE),
            ("find . -name '*.tmp' -o -name .env | xargs rm", DELETE),
            // A filter after the first printing action filters nothing.
            ("find . -print -name '*.tmp' | xargs rm", DELETE),
            // What the feeder prints is not known: refused.
            ("find . -name .env | grep -v x | xargs rm", DELETE),
            (
                "find . -name '*.tmp' -exec echo .env \\; | xargs rm",
                DELETE,
            ),
            ("(find . -name .env) | xargs rm", DELETE),
            ("ls -a | xargs rm", DELETE),
            ("git ls-files | xargs rm", DELETE),
            (
                "git ls-files -o -i --exclude-standard | xargs rm -f",
                DELETE,
            ),
            ("cat list.txt | xargs rm", DELETE),
            ("echo .env | xargs rm", DELETE),
            ("xargs rm < list.txt", DELETE),
            ("xargs -a list.txt rm", DELETE),
            ("sh -c 'find . -name .env | xargs rm'", DELETE),
            // allowed
            ("find . -name '*.tmp' | xargs rm", ALLOW),
            (
                "find build -type f -name '*.log' -print0 | xargs -0 rm -f",
                ALLOW,
            ),
            ("find . -name '*.o' -print | xargs -n 50 rm -f", ALLOW),
            ("find /tmp/elsewhere -name .env | xargs rm", ALLOW),
            ("find . -name .env | xargs cat", ALLOW),
            ("find . -name .env | xargs grep -l KEY", ALLOW),
            ("git ls-files | xargs wc -l", ALLOW),
            ("ls | xargs echo rm", ALLOW),
            ("find . -name '*.tmp' || rm notes.txt", ALLOW),
            ("git ls-files | grep -c rm", ALLOW),
        ],
    );
}

#[test]
fn a_wrapper_option_value_is_not_the_command() {
    run_corpus(
        &fixture(),
        &[
            ("sudo --user root rm .env", DELETE),
            ("env --unset FOO rm .env", DELETE),
            ("env --chdir . rm .env", DELETE),
            ("nice --adjustment 5 rm .env", DELETE),
            ("timeout --signal KILL 5 rm .env", DELETE),
            ("timeout --kill-after 2 5 rm .env", DELETE),
            ("exec -a x rm .env", DELETE),
            ("stdbuf -oL rm .env", DELETE),
            ("stdbuf -o L rm .env", DELETE),
            ("setsid rm .env", DELETE),
            ("ionice -c 3 rm .env", DELETE),
            ("caffeinate -i rm .env", DELETE),
            ("caffeinate -t 60 rm .env", DELETE),
            // allowed
            ("sudo --user root rm notes.txt", ALLOW),
            ("timeout --signal KILL 5 rm notes.txt", ALLOW),
            ("nice --adjustment 5 cat notes.txt", ALLOW),
            ("stdbuf -oL cat notes.txt", ALLOW),
            // reading `.env` through a wrapper is now blocked by secret_guard
            ("nice --adjustment 5 cat .env", Some("secret_guard")),
            ("stdbuf -oL cat .env", Some("secret_guard")),
            ("env --unset FOO ls", ALLOW),
            ("setsid rm build/a.tmp", ALLOW),
        ],
    );
}
