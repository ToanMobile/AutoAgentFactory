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

#[test]
fn g9_allowlisted_literal_messages_and_heredocs() {
    run_corpus(
        &Fixture::new(),
        &[
            ("gh pr create --body 'mentions AAF_DATA_DIR'", ALLOW),
            (
                "gh pr create -t 'fix AAF_DATA_DIR' -b 'see .local/share/autofactory'",
                ALLOW,
            ),
            ("gh issue comment 1 --body='mentions AAF_DATA_DIR'", ALLOW),
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
                "gh pr create --body-file - <<'EOF'\ndocs AAF_DATA_DIR\nEOF\n",
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
