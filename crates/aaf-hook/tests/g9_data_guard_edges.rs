//! Extra bypass & allowlist edge tests for G9 (`data_dir_guard` literal-text
//! relaxation and single-command quoted heredoc data sinks).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DATA: Option<&str> = Some("data_dir_guard");
const GIT: Option<&str> = Some("git_guard");
const DELETE: Option<&str> = Some("delete_guard");
const HUMAN: Option<&str> = Some("human_action_guard");
const PR_MERGE: Option<&str> = Some("pr_merge_guard");

#[test]
fn g9_allowlisted_literal_messages_and_heredocs() {
    run_corpus(
        &Fixture::new(),
        &[
            (
                "gh pr create --body '🤖 aaf-bot: mentions AAF_DATA_DIR'",
                ALLOW,
            ),
            (
                "gh pr create -t 'fix AAF_DATA_DIR' -b '🤖 aaf-bot: see .local/share/autofactory'",
                ALLOW,
            ),
            (
                "gh issue comment 1 --body='🤖 aaf-bot: mentions AAF_DATA_DIR'",
                ALLOW,
            ),
            ("git commit -m'docs: explain AAF_DATA_DIR'", ALLOW),
            ("git commit --message='docs: explain AAF_DATA_DIR'", ALLOW),
            ("git commit -am 'docs: explain AAF_DATA_DIR'", ALLOW),
            (
                "git commit -m 'title AAF_DATA_DIR' -m 'body receipt.key'",
                ALLOW,
            ),
            ("git notes add -m 'mentions AAF_DATA_DIR'", ALLOW),
            ("git tag -F - <<'EOF'\nrelease AAF_DATA_DIR\nEOF\n", ALLOW),
            (
                "gh pr create --body-file - <<'EOF'\n🤖 aaf-bot: docs AAF_DATA_DIR\nEOF\n",
                ALLOW,
            ),
            ("cat - >> note.md <<\"EOF\"\nAAF_DATA_DIR\nEOF\n", ALLOW),
        ],
    );
}

#[test]
fn g9_bypass_vectors_remain_blocked() {
    run_corpus(
        &Fixture::new(),
        &[
            ("git commit -F $AAF_DATA_DIR/receipt.key", DATA),
            ("git commit -m \"$(cat $AAF_DATA_DIR/receipt.key)\"", DATA),
            ("git commit -m \"fix\" -F $AAF_DATA_DIR/x", DATA),
            ("git commit -m\"$AAF_DATA_DIR\"", DATA),
            ("git commit --message=\"$AAF_DATA_DIR\"", DATA),
            ("git commit -F -m $AAF_DATA_DIR/receipt.key", DATA),
            ("git commit -- -m AAF_DATA_DIR", DATA),
            ("gh pr create --body \"$AAF_DATA_DIR\"", DATA),
            ("gh pr create --body-file $AAF_DATA_DIR/receipt.key", DATA),
            (
                "cat <<'EOF' > >(bash)\ncat $AAF_DATA_DIR/receipt.key\nEOF\n",
                DATA,
            ),
            ("sh <<'EOF'\ncat $AAF_DATA_DIR/receipt.key\nEOF\n", DATA),
            (
                "sh -c \"cat > /tmp/x.sh <<'EOF'\ncat $AAF_DATA_DIR/receipt.key\nEOF\n\" && bash /tmp/x.sh",
                DATA,
            ),
            ("cat > .git/hooks/pre-commit <<'EOF'\necho hi\nEOF\n", GIT),
            ("cat > .env <<'EOF'\nAAF_DATA_DIR=x\nEOF\n", DELETE),
            ("git commit -m 'docs: AAF_DATA_DIR' --no-verify", GIT),
            ("env -i cat > note.md <<'EOF'\naaf run\nEOF\n", HUMAN),
        ],
    );
}

/// Spellings of a quoted heredoc delimiter that bash reads as `EOF`.
const QUOTED_EOF: &[&str] = &[
    "'EOF'",
    "\"EOF\"",
    "\\EOF",
    "$'EOF'",
    "E'OF'",
    "E\"OF\"",
    "'E'OF",
    "\"E\"OF",
    "\\E\\O\\F",
    "E\\OF",
    "$'E'OF",
    "$\"EOF\"",
    "E$\"OF\"",
    "-'EOF'",
    " 'EOF'",
];

/// R10-H6-1: `$"EOF"` is the delimiter `EOF` to bash (a locale-translated
/// string), not `$EOF`: the body ends at the `EOF` line and what follows it
/// is a command, not data the body swallowed. Every other spelling of a
/// quoted `EOF` behaves as before.
#[test]
fn r10_h6_1_locale_quoted_delimiter_ends_at_its_text() {
    run_corpus(
        &Fixture::new(),
        &[
            (
                "cat <<$\"EOF\" > notes.txt\nbody\nEOF\nrm -rf ../cdp",
                DELETE,
            ),
            (
                "git commit -F - <<$\"EOF\"\nmsg\nEOF\ngit push --force origin main",
                GIT,
            ),
            (
                "cat <<$\"EOF\" >> notes.txt\nbody\nEOF\ngh pr merge 1 --squash",
                PR_MERGE,
            ),
            ("cat <<$\"EOF\"\nbody\nEOF\nrm -rf ../cdp", DELETE),
            (
                "git commit -m \"$(cat <<$\"EOF\"\nmsg\nEOF\n)\"\ngit push --force origin main",
                GIT,
            ),
            // Still data: the body is quoted.
            ("cat <<$\"EOF\" > notes.txt\nrm -rf ../cdp\nEOF\n", ALLOW),
            ("git commit -F - <<$\"EOF\"\nrm -rf ../cdp\nEOF\n", ALLOW),
        ],
    );
    let mut rows = Vec::new();
    for delim in QUOTED_EOF {
        rows.push((
            format!("cat <<{delim} > notes.txt\nrm -rf ../cdp\nEOF\n"),
            ALLOW,
        ));
        rows.push((format!("cat <<{delim}\nrm -rf ../cdp\nEOF\n"), ALLOW));
        rows.push((
            format!("git tag -F - <<{delim}\nrm -rf ../cdp\nEOF\n"),
            ALLOW,
        ));
        rows.push((
            format!("cat <<{delim} > notes.txt\nbody\nEOF\nrm -rf ../cdp"),
            DELETE,
        ));
        rows.push((format!("cat <<{delim}\nbody\nEOF\nrm -rf ../cdp"), DELETE));
    }
    let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(c, r)| (c.as_str(), *r)).collect();
    run_corpus(&Fixture::new(), &rows);
}

/// R10-H6-1 hardening: a heredoc whose delimiter line never comes is not
/// data. The hook and the shell may disagree on the delimiter, and then the
/// "body" is the rest of the script.
#[test]
fn r10_h6_1_unterminated_heredoc_is_never_data() {
    run_corpus(
        &Fixture::new(),
        &[
            ("cat <<'EOF' > notes.txt\nbody\nrm -rf ../cdp", DELETE),
            ("cat <<'EOF'\nbody\nrm -rf ../cdp\n", DELETE),
            (
                "git commit -F - <<'EOF'\nmsg\ngit push --force origin main",
                GIT,
            ),
            ("cat <<'END' > notes.txt\nbody\nEND\n", ALLOW),
        ],
    );
}
