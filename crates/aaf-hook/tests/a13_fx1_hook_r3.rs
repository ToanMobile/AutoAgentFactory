#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit-13 round-3 hook lexer findings (R3-H6-3,4,6..10,
//! E3-F2, E3-F6).

mod common;

use std::fs;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const GIT: Option<&str> = Some("git_guard");
const HEAVY: Option<&str> = Some("heavy_cmd_guard");
const OPAQUE: Option<&str> = Some("unparsable_command");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");
const SECRET: Option<&str> = Some("secret_guard");

/// R3-H6-6: an unquoted `{a,b}` / `{x..y}` word is expanded before the
/// guards look at it; a lone `{}`, `@{u}`, `${VAR}` and a group `{ …; }`
/// are left alone.
#[test]
fn r3_h6_6_brace_expansion_is_modelled() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("gh pr {m..m}erge 1", PR_MERGE),
            ("{g..g}h pr merge 1", PR_MERGE),
            ("git push --{f..f}orce origin feature/x", GIT),
            ("git branch -f {m..m}ain HEAD", GIT),
            ("rm -rf {..,x}", DELETE),
            ("rm -rf .{g..g}it", GIT),
            ("mv .git{,.bak}", GIT),
            ("cat .{e..e}nv", SECRET),
            ("echo hi > .git/{c..c}onfig", GIT),
            (
                "curl -X PUT https://api.github.com/repos/o/r/pulls/1/{merge,x}",
                PR_MERGE,
            ),
            ("rm -rf x{a,{b,/..}}", DELETE),
            ("rm -rf {1..70,..}", DELETE),
            ("rm -rf {1..70}{,..}", OPAQUE),
            // Still allowed.
            ("find . -name '*.o' -exec rm {} \\;", ALLOW),
            ("git log @{u}", ALLOW),
            ("echo ${HOME}", ALLOW),
            ("{ cargo build; cargo test; }", ALLOW),
            ("echo {a,b}", ALLOW),
            ("mkdir -p src/{a,b}", ALLOW),
            ("echo '{..,x}' \"{..,y}\" \\{..,z}", ALLOW),
            ("rm -rf '{..,x}'", ALLOW),
            ("for i in {1..100}; do echo $i; done", ALLOW),
            ("touch file{1..3}.txt", ALLOW),
            ("git stash pop stash@{0}", ALLOW),
            ("git reset --soft HEAD@{1}", ALLOW),
            ("cp Cargo.toml{,.bak}", ALLOW),
            ("echo ${HOME}/{a,b}", ALLOW),
        ],
    );
}

/// R3-H6-7: `time`, `!` and `coproc` keep the reserved words after them, and
/// zsh's `nocorrect`, `-` and `repeat N` do not hide the command they run.
#[test]
fn r3_h6_7_keywords_and_precommand_modifiers() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("time { gh pr merge 1; }", PR_MERGE),
            ("time -p { git push --force origin feature/x; }", GIT),
            ("nocorrect gh pr merge 1", PR_MERGE),
            ("- git push --force origin feature/x", GIT),
            ("coproc gh pr merge 1", PR_MERGE),
            ("coproc NAME { gh pr merge 1; }", PR_MERGE),
            ("repeat 1 rm -rf ~", DELETE),
            ("repeat 2 { gh pr merge 1; }", PR_MERGE),
            ("nocorrect cd / && rm -rf etc", DELETE),
            ("time { cd /; } && rm -rf etc", DELETE),
            ("! time { gh pr merge 1; }", PR_MERGE),
            // Still allowed.
            ("time cargo test", ALLOW),
            ("time { cargo build; }", ALLOW),
            ("! grep -q x f", ALLOW),
            ("nocorrect ls", ALLOW),
        ],
    );
}

/// R3-H6-8: `~+` is the working directory; `~-`, `~N` and `~user` are not
/// known to the hook.
#[test]
fn r3_h6_8_tilde_prefixes() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("rm -rf ~alex", DELETE),
            ("rm -rf ~+", DELETE),
            ("rm -rf ~0", DELETE),
            ("rm -rf ~-", DELETE),
            ("rm -rf ~root/x", DELETE),
            // Still allowed.
            ("ls ~/x", ALLOW),
            ("ls ~alex", ALLOW),
            ("rm -rf ~+/build", ALLOW),
        ],
    );
}

/// R3-H6-9: a shell (or `.`/`source`) given `/dev/stdin`, `/dev/fd/0` or
/// `-` as its script reads the script from standard input.
#[test]
fn r3_h6_9_shell_script_from_dev_stdin() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("bash /dev/stdin <<< \"gh pr merge 1\"", PR_MERGE),
            ("zsh /dev/stdin <<< \"rm -rf ~\"", DELETE),
            ("echo \"rm -rf ~\" | bash /dev/stdin", DELETE),
            (". /dev/stdin <<< \"rm -rf ~\"", DELETE),
            ("source /dev/fd/0 <<< \"gh pr merge 1\"", PR_MERGE),
            ("bash /dev/fd/0 <<< \"gh pr merge 1\"", PR_MERGE),
            ("bash -- - <<< \"gh pr merge 1\"", PR_MERGE),
            // Still allowed.
            ("bash ./script.sh <<< \"gh pr merge 1\"", ALLOW),
            ("echo hi | bash /dev/stdin", ALLOW),
            (". ./env.sh", ALLOW),
        ],
    );
}

/// R3-H6-10 / E3-F6: under `CDPATH` a `cd` to a bare name may land in any
/// directory of the search path.
#[test]
fn r3_h6_10_cdpath_moves_are_uncertain() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root.join("sub")).unwrap();
    fs::create_dir_all(fx.root.join("etc")).unwrap();
    run_corpus(
        &fx,
        &[
            ("CDPATH=/ cd etc && rm -rf ssh", DELETE),
            ("export CDPATH=/; cd etc && rm -rf ssh", DELETE),
            ("CDPATH=.. cd init1 && rm -rf ./*", DELETE),
            ("CDPATH=/tmp:/ pushd etc && rm -rf ssh", DELETE),
            ("CDPATH=$X cd etc && rm -rf ssh", OPAQUE),
            ("read CDPATH; cd etc && rm -rf ssh", OPAQUE),
            // Still allowed.
            ("cd sub && cargo test", ALLOW),
            ("CDPATH=/ cd ./etc && rm -rf ssh", ALLOW),
            ("CDPATH=/ cd etc && ls", ALLOW),
            ("cd etc && rm -rf ssh", ALLOW),
        ],
    );
}

/// R3-H6-3 / R3-H6-4: an unknown launcher option may take the next word as
/// its value; both readings are judged. `mise -C<dir>` glued moves too.
#[test]
fn r3_h6_3_launcher_unknown_option_values() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("pnpm --loglevel error exec gh pr merge 1", PR_MERGE),
            (
                "pnpm --reporter silent exec git push --force origin feature/x",
                GIT,
            ),
            ("npm --loglevel silent exec -c \"gh pr merge 1\"", PR_MERGE),
            (
                "npm exec --registry https://registry.npmjs.org -c \"rm -rf ~\"",
                DELETE,
            ),
            ("mise --log-level debug exec -c \"rm -rf ~\"", DELETE),
            ("mise -P dev exec -- rm -rf ~", DELETE),
            ("pnpm --loglevel error -C .. exec rm -rf repo", DELETE),
            ("mise -C.. exec -- rm -rf repo", DELETE),
            ("mise -C/ x -- rm -rf etc", DELETE),
            // Still allowed.
            ("pnpm --loglevel error install", ALLOW),
            ("npm --loglevel silent run build", ALLOW),
            ("pnpm --reporter silent exec eslint .", ALLOW),
            ("mise -C. exec -- cargo test", ALLOW),
            ("npx --yes prettier --check .", ALLOW),
            ("pnpm --filter web exec eslint .", ALLOW),
        ],
    );
}

/// E3-F2: Python, conda, nix, direnv and bun launchers are stripped like the
/// others, so every guard judges the program they run.
#[test]
fn e3_f2_more_launchers() {
    let fx = Fixture::new();
    run_corpus(
        &fx,
        &[
            ("uv run rm -rf ../init1", DELETE),
            ("uv run --with requests -- rm -rf ../x", DELETE),
            ("uv tool run rm -rf ../x", DELETE),
            ("uvx --from foo rm -rf ../x", DELETE),
            ("poetry run rm -rf ../x", DELETE),
            ("pipenv run rm -rf ../x", DELETE),
            ("pdm run rm -rf ../x", DELETE),
            ("pipx run rm -rf ../x", DELETE),
            ("conda run -n base rm -rf ../x", DELETE),
            ("mamba run -p /opt/env rm -rf ../x", DELETE),
            ("nix develop -c rm -rf ../x", DELETE),
            ("nix shell nixpkgs#hello --command rm -rf ../x", DELETE),
            ("nix run nixpkgs#bash -- -c 'gh pr merge 1'", PR_MERGE),
            ("nix-shell --run 'rm -rf ../x'", DELETE),
            ("nix-shell -p hello --command 'gh pr merge 1'", PR_MERGE),
            ("direnv exec . rm -rf ../x", DELETE),
            ("bun x rm -rf ../x", DELETE),
            ("uv run xcodebuild -scheme A build", HEAVY),
            // Still allowed.
            ("uv run pytest", ALLOW),
            ("poetry run pytest", ALLOW),
            ("nix develop -c cargo test", ALLOW),
            ("nix-shell --run 'cargo test'", ALLOW),
            ("uv sync", ALLOW),
            ("nix build", ALLOW),
            ("direnv allow", ALLOW),
            ("conda run -n base python -V", ALLOW),
            ("bun x prettier --check .", ALLOW),
        ],
    );
}
