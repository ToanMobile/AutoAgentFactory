#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Edge-case tests for G2 cross-audit evidence verification.

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult, SandboxPolicy};
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::prompt::MAX_DIFF_BYTES;
use aaf_engine::cross_audit::{
    AuditInput, AuditVerdict, build_checklist, build_prompt, evidence_in_diff,
    has_file_line_reference, parse_diff_hunks, parse_diff_hunks_with_truncation, parse_verdict,
    run_audit,
};

struct FixedRunner(String);

impl AgentRunner for FixedRunner {
    fn run(&self, _request: &AgentRequest) -> PortResult<AgentOutcome> {
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: self.0.clone(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Harden auth".to_string(),
        acceptance: vec!["Rejects invalid tokens".to_string()],
    }
}

fn req() -> AgentRequest {
    AgentRequest {
        run_id: "g2-edge".to_string(),
        prompt: String::new(),
        cwd: "/work".to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy {
            enabled: false,
            read_only: false,
            writable_paths: vec![],
            read_denied_paths: vec![],
        },
    }
}

fn reply_with(mobile: bool, hits: &[(&str, &str)]) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(&spec(), mobile)
        .iter()
        .map(|item| {
            if let Some((_, ev)) = hits.iter().find(|(id, _)| *id == item.id) {
                serde_json::json!({ "id": item.id, "answer": "yes", "evidence": ev })
            } else {
                serde_json::json!({ "id": item.id, "answer": "no", "evidence": "" })
            }
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

#[test]
fn prompt_forbids_guessing_and_requires_diff_anchored_citations() {
    let input = AuditInput {
        spec: spec(),
        diff: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n fn a() {}\n+fn b() {}\n".to_string(),
        mobile: false,
    };
    let prompt = build_prompt(&input);
    assert!(!prompt.contains("When unsure"), "prompt still biases yes");
    assert!(prompt.contains("file:line"));
    assert!(prompt.contains("AC-N"));
}

#[test]
fn deleted_lines_and_deleted_files_validate_for_assert_and_gate_ci() {
    let diff = "\
diff --git a/tests/auth.rs b/tests/auth.rs
--- a/tests/auth.rs
+++ b/tests/auth.rs
@@ -10,5 +10,3 @@
 fn check() {
-    assert!(is_valid(tok));
-    assert_eq!(code, 200);
     ok();
 }
diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
--- a/.github/workflows/ci.yml
+++ /dev/null
@@ -1,4 +0,0 @@
-name: CI
-on: push
-jobs: {}
-
";
    let input = AuditInput {
        spec: spec(),
        diff: diff.to_string(),
        mobile: false,
    };
    let reply = reply_with(
        false,
        &[
            ("ASSERT", "removed assertion at `tests/auth.rs:11-12`"),
            ("GATE-CI", "deleted `.github/workflows/ci.yml:2`"),
        ],
    );
    let report = run_audit(&input, &FixedRunner(reply), &req()).unwrap();
    let AuditVerdict::Fail { findings, .. } = report.verdict else {
        panic!("expected Fail, got {:?}", report.verdict);
    };
    assert_eq!(findings.len(), 2);
}

#[test]
fn all_citation_formats_and_path_normalisations_work() {
    let diff = "\
diff --git a/src/auth.rs b/src/auth.rs
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,6 @@
 fn a() {}
+let x = 1;
+let y = 2;
 fn b() {}
\\ No newline at end of file
diff --git a/a/nested.rs b/a/nested.rs
--- a/a/nested.rs
+++ b/a/nested.rs
@@ -1,1 +1,2 @@
 fn n() {}
+fn m() {}
";
    let hunks = parse_diff_hunks(diff);
    for valid in [
        "src/auth.rs:11",
        "src/auth.rs:10-15",
        "src/auth.rs:10..15",
        "src/auth.rs:L11",
        "src/auth.rs:L10-L15",
        "src/auth.rs#L11",
        "src/auth.rs:11:5",
        "see `src/auth.rs:12`: hardcoded",
        "(./src/auth.rs:12)",
        "b/src/auth.rs:12",
        "a/src/auth.rs:12",
        "src\\auth.rs:12",
        "a/nested.rs:2",
    ] {
        assert!(evidence_in_diff(valid, &hunks), "should validate: {valid}");
    }
    for invalid in [
        "",
        "no file line here",
        "src/auth.rs:9",
        "src/auth.rs:10-20",
        "src/auth.rs:1-999999",
        "src/auth.rs:99",
        "src/auth.rs:0",
        "src/auth.rs:0:5",
        "https://example.com:443",
        "other/auth.rs:11",
    ] {
        assert!(
            !evidence_in_diff(invalid, &hunks),
            "should reject: {invalid}"
        );
    }
    assert!(!has_file_line_reference("foo.rs:0:5"));
}

#[test]
fn mixed_valid_and_invalid_blocking_findings_keep_only_valid() {
    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 fn f() {
+    let key = \"secret\";
 }
";
    let checklist = build_checklist(&spec(), false);
    // Syntax layer: one empty evidence + one real file:line -> keeps real only.
    let reply_syntax = reply_with(false, &[("STUB", ""), ("SECRET", "src/lib.rs:2")]);
    let AuditVerdict::Fail { findings, .. } = parse_verdict(&reply_syntax, &checklist) else {
        panic!("expected Fail with valid finding kept");
    };
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "SECRET");

    // Diff layer: one hallucinated file:line + one real diff-anchored -> keeps real only.
    let reply_diff = reply_with(
        false,
        &[("STUB", "ghost.rs:99"), ("SECRET", "src/lib.rs:2")],
    );
    let input = AuditInput {
        spec: spec(),
        diff: diff.to_string(),
        mobile: false,
    };
    let report = run_audit(&input, &FixedRunner(reply_diff), &req()).unwrap();
    let AuditVerdict::Fail { findings, .. } = report.verdict else {
        panic!(
            "expected Fail with real SECRET kept, got {:?}",
            report.verdict
        );
    };
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "SECRET");
}

#[test]
fn advisory_warnings_with_invalid_evidence_are_dropped_without_inconclusive() {
    let diff = "\
diff --git a/src/ui.rs b/src/ui.rs
--- a/src/ui.rs
+++ b/src/ui.rs
@@ -1,1 +1,2 @@
 fn view() {}
+fn login() {}
";
    let input = AuditInput {
        spec: spec(),
        diff: diff.to_string(),
        mobile: true,
    };
    let reply = reply_with(
        true,
        &[
            ("STORE-SIWA", ""),
            ("STORE-EXT-PAY", "nonexistent.rs:50"),
            ("STORE-PLACEHOLDER", "src/ui.rs:2"),
        ],
    );
    let report = run_audit(&input, &FixedRunner(reply), &req()).unwrap();
    let AuditVerdict::Pass { warnings } = report.verdict else {
        panic!(
            "advisory invalid evidence must not turn Pass inconclusive: {:?}",
            report.verdict
        );
    };
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].id, "STORE-PLACEHOLDER");
}

#[test]
fn binary_and_mode_only_hunkless_files_accept_positive_lines() {
    let diff = "\
diff --git a/assets/icon.png b/assets/icon.png
new file mode 100644
Binary files /dev/null and b/assets/icon.png differ
diff --git a/scripts/run.sh b/scripts/run.sh
old mode 100644
new mode 100755
";
    let hunks = parse_diff_hunks(diff);
    assert!(evidence_in_diff("assets/icon.png:1", &hunks));
    assert!(evidence_in_diff("scripts/run.sh:1", &hunks));
    assert!(!evidence_in_diff("scripts/run.sh:0", &hunks));
}

#[test]
fn anti_forgery_hunk_counting_blocks_forged_headers_in_diff_body() {
    let diff = "\
diff --git a/docs/note.md b/docs/note.md
--- a/docs/note.md
+++ b/docs/note.md
@@ -1,3 +1,5 @@
 intro
--- a/forged_del.rs
+++ b/forged.rs
+@@ -1,50 +1,50 @@
+fake line
 outro
";
    let hunks = parse_diff_hunks(diff);
    assert!(evidence_in_diff("docs/note.md:3", &hunks));
    assert!(!evidence_in_diff("forged.rs:10", &hunks));
    assert!(!evidence_in_diff("forged_del.rs:1", &hunks));
}

#[test]
fn truncated_diff_rejects_citations_past_visible_cut() {
    let head = "diff --git a/src/head.rs b/src/head.rs\n\
                --- a/src/head.rs\n\
                +++ b/src/head.rs\n\
                @@ -1,1 +1,2 @@\n\
                 fn h() {}\n\
                +let pad = \"";
    let tail = "\";\n\
                diff --git a/src/tail.rs b/src/tail.rs\n\
                --- a/src/tail.rs\n\
                +++ b/src/tail.rs\n\
                @@ -1,1 +1,2 @@\n\
                 fn t() {}\n\
                +let secret = 1;\n";
    let diff = format!("{head}{}{tail}", "x".repeat(MAX_DIFF_BYTES));
    let input = AuditInput {
        spec: spec(),
        diff,
        mobile: false,
    };
    let tail_reply = reply_with(false, &[("SECRET", "src/tail.rs:2")]);
    let report = run_audit(&input, &FixedRunner(tail_reply), &req()).unwrap();
    assert!(matches!(report.verdict, AuditVerdict::Inconclusive { .. }));
}

#[test]
fn zero_count_hunks_do_not_accept_untouched_anchor_line() {
    let diff = "\
diff --git a/src/add.rs b/src/add.rs
--- a/src/add.rs
+++ b/src/add.rs
@@ -5,0 +6,2 @@
+let a = 1;
+let b = 2;
";
    let hunks = parse_diff_hunks(diff);
    assert!(!evidence_in_diff("src/add.rs:5", &hunks));
    assert!(evidence_in_diff("src/add.rs:6", &hunks));
    assert!(evidence_in_diff("src/add.rs:7", &hunks));
}

#[test]
fn git_c_quoted_non_ascii_and_spaced_paths_validate() {
    let diff = "\
diff --git \"a/src/caf\\303\\251.rs\" \"b/src/caf\\303\\251.rs\"
--- \"a/src/caf\\303\\251.rs\"
+++ \"b/src/caf\\303\\251.rs\"
@@ -1,1 +1,2 @@
 fn a() {}
+fn b() {}
diff --git a/src/my file.rs b/src/my file.rs
--- a/src/my file.rs
+++ b/src/my file.rs
@@ -10,2 +10,3 @@
 fn x() {}
+fn y() {}
 fn z() {}
";
    let hunks = parse_diff_hunks(diff);
    assert!(evidence_in_diff("src/café.rs:2", &hunks));
    assert!(!evidence_in_diff("src/caf/303/251.rs:2", &hunks));
    assert!(evidence_in_diff("bug at \"src/my file.rs:11\"", &hunks));
    assert!(evidence_in_diff("see `src/my file.rs:10-12`", &hunks));
    assert!(evidence_in_diff("see `src/my file.rs`:11", &hunks));
}

#[test]
fn hunkless_files_under_b_dir_do_not_double_strip_prefix() {
    let diff = "\
diff --git a/b/icon.png b/b/icon.png
new file mode 100644
Binary files /dev/null and b/b/icon.png differ
diff --git a/dir b/file.bin b/dir b/file.bin
new file mode 100644
Binary files /dev/null and b/dir b/file.bin differ
";
    let hunks = parse_diff_hunks(diff);
    assert!(evidence_in_diff("b/icon.png:1", &hunks));
    assert!(!evidence_in_diff("icon.png:1", &hunks));
    assert!(evidence_in_diff("`dir b/file.bin:1`", &hunks));
}

#[test]
fn truncated_mid_header_file_is_not_marked_hunkless() {
    let mid_header = "\
diff --git a/src/cut.rs b/src/cut.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/cut.rs";
    let hunks = parse_diff_hunks_with_truncation(mid_header, true);
    assert!(!evidence_in_diff("src/cut.rs:1", &hunks));
    assert!(!evidence_in_diff("src/cut.rs:999", &hunks));

    // Also end-to-end via run_audit when MAX_DIFF_BYTES cuts inside the second file's header.
    let head_prefix = "diff --git a/src/head.rs b/src/head.rs\n\
                       --- a/src/head.rs\n\
                       +++ b/src/head.rs\n\
                       @@ -1,1 +1,2 @@\n\
                        fn h() {}\n\
                       +let p = \"";
    let cut_header = "\";\ndiff --git a/src/cut.rs b/src/cut.rs\nnew file mode 100644\n";
    let pad_len = MAX_DIFF_BYTES - head_prefix.len() - cut_header.len() - 5;
    let diff = format!(
        "{head_prefix}{}{cut_header}--- /dev/null\n+++ b/src/cut.rs\n@@ -0,0 +1,2 @@\n+fn c() {{}}\n",
        "x".repeat(pad_len)
    );
    let input = AuditInput {
        spec: spec(),
        diff,
        mobile: false,
    };
    let reply = reply_with(false, &[("SECRET", "src/cut.rs:999")]);
    let report = run_audit(&input, &FixedRunner(reply), &req()).unwrap();
    assert!(matches!(report.verdict, AuditVerdict::Inconclusive { .. }));
}
