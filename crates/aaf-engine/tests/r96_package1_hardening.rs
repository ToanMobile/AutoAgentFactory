#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Round-2 & Round-3 security hardening regression tests for Package 1 (#96):
//! - Symlink directory shadowing (`.claude`, `tests/.claude`, `.cargo`, `gradle`)
//! - Runner configs inside test dirs, custom `head_lock_globs`, soft manifest consistency
//! - CamelCase & digit->UPPER security tokens (`AuthService.kt`, `OAuth2Client.kt`, `X509Cert.rs`, `Sha256Hmac.rs`)
//! - Invisible/bidi/line-separator path chars (`U+200B`, `U+061C`, `U+2028`)
//! - Hunk-body `TRUNCATION_MARKER` and escaped trailing quotes in `diff_hunks.rs`
//! - Truncated & wrapped JSON anti-downgrade latch (`names_dual_blocker`) and compact-repair gating

use aaf_domain::config::default_head_lock_globs;
use aaf_domain::pipeline::{PlanContract, PlanFileChange, RedGreenTestPlan};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::cross_audit::dual_verdict::{
    analyze_plan_conformance, build_dual_checklist, can_use_compact_format_repair,
    deterministic_scope_violations, names_dual_blocker,
};
use aaf_engine::cross_audit::{GOLDEN_IDS, parse_diff_hunks};
use serde_json::{Value, json};

fn sample_task(id: &str) -> Task {
    Task::new_root(
        id,
        "/repo",
        "Hardening task",
        ImmutableSpec {
            description: "Implement planned feature".into(),
            acceptance: vec!["planned feature works".into()],
        },
        Priority::P1,
        100,
    )
}

fn sample_plan() -> PlanContract {
    PlanContract {
        summary: "Update parser and sandbox".into(),
        tier_assessment: "M".into(),
        tier_reasons: vec!["scoped".into()],
        files_to_change: vec![
            PlanFileChange {
                path: "src/feature/parser.rs".into(),
                responsibility: "parse input".into(),
                risk: "low".into(),
            },
            PlanFileChange {
                path: "src/sandbox/seatbelt.rs".into(),
                responsibility: "seatbelt rules".into(),
                risk: "high".into(),
            },
        ],
        invariants_and_edge_cases: vec!["no panic".into()],
        red_green_tests: vec![RedGreenTestPlan {
            ac_ref: "AC-1".into(),
            test_path: "tests/feature_test.rs".into(),
            red_condition: "fails".into(),
            green_condition: "passes".into(),
        }],
        dispositions: vec![],
        open_questions: vec![],
    }
}

fn scores_all(score: u8) -> Value {
    let reason = if score < 100 { "minor" } else { "" };
    GOLDEN_IDS
        .iter()
        .map(|id| json!({"id": id, "score": score, "deduction_reason": reason}))
        .collect()
}

fn dual_answers(
    checklist: &[aaf_engine::cross_audit::ChecklistItem],
    yes: &[(&str, &str)],
) -> Value {
    checklist
        .iter()
        .map(|it| {
            let ev = yes.iter().find(|(k, _)| *k == it.id).map(|(_, e)| *e);
            json!({
                "id": it.id,
                "answer": if ev.is_some() { "yes" } else { "no" },
                "evidence": ev.unwrap_or(""),
            })
        })
        .collect()
}

#[test]
fn round2_and_round3_hardening_covers_symlinks_camelcase_truncation_and_blockers() {
    let plan = sample_plan();
    let mut globs = default_head_lock_globs();
    globs.push("deploy/**".into());

    // 1. Sibling helper in planned security dir + repo-wide "." scope -> 0 SCOPE-HARD.
    let helper_diff = "\
diff --git a/src/sandbox/seatbelt.rs b/src/sandbox/seatbelt.rs
--- /dev/null
+++ b/src/sandbox/seatbelt.rs
@@ -0,0 +1,1 @@
+pub fn rules() {}
diff --git a/src/sandbox/helper.rs b/src/sandbox/helper.rs
--- /dev/null
+++ b/src/sandbox/helper.rs
@@ -0,0 +1,1 @@
+pub fn fmt() {}
";
    let h_conf = analyze_plan_conformance(helper_diff, &plan, &[".".into()], &globs);
    assert!(
        deterministic_scope_violations(&h_conf).is_empty(),
        "sibling helper in planned dir must not false-block: {:?}",
        deterministic_scope_violations(&h_conf)
    );

    // 2. Round-2 bypass vectors MUST ALL trigger SCOPE-HARD.
    let attack_diff = "\
diff --git a/src/feature/parser.rs b/src/feature/parser.rs
--- a/src/feature/parser.rs
+++ b/src/feature/parser.rs
@@ -1,1 +1,2 @@
 pub fn parse() {}
+// [AAF_UNTRUSTED_DIFF_TRUNCATED]
diff --git a/.claude b/.claude
new file mode 120000
diff --git a/tests/.claude b/tests/.claude
new file mode 120000
diff --git a/tests/conftest.py b/tests/conftest.py
--- /dev/null
+++ b/tests/conftest.py
@@ -0,0 +1,1 @@
+pass
diff --git a/deploy/prod.sh b/deploy/prod.sh
--- /dev/null
+++ b/deploy/prod.sh
@@ -0,0 +1,1 @@
+echo hi
diff --git a/src/build/sandbox.rs b/src/build/sandbox.rs
--- /dev/null
+++ b/src/build/sandbox.rs
@@ -0,0 +1,1 @@
+pub fn s() {}
diff --git a/crates/other/src/test_leak.rs b/crates/other/src/test_leak.rs
--- /dev/null
+++ b/crates/other/src/test_leak.rs
@@ -0,0 +1,1 @@
+pub fn leak() {}
diff --git a/src/feature/AuthService.kt b/src/feature/AuthService.kt
--- /dev/null
+++ b/src/feature/AuthService.kt
@@ -0,0 +1,1 @@
+class AuthService
diff --git a/src/feature/OAuthClient.ts b/src/feature/OAuthClient.ts
--- /dev/null
+++ b/src/feature/OAuthClient.ts
@@ -0,0 +1,1 @@
+export class OAuthClient {}
diff --git a/src/feature/a\u{200B}uth.rs b/src/feature/a\u{200B}uth.rs
--- /dev/null
+++ b/src/feature/a\u{200B}uth.rs
@@ -0,0 +1,1 @@
+pub fn z() {}
diff --git a/zzz/payload.bin b/zzz/payload.bin
new file mode 100644
Binary files /dev/null and b/zzz/payload.bin differ
";
    let a_conf = analyze_plan_conformance(
        attack_diff,
        &plan,
        &["src/feature".into(), "src/sandbox".into()],
        &globs,
    );
    let viols = deterministic_scope_violations(&a_conf);
    assert_eq!(
        viols.len(),
        10,
        "all 10 Round-2 bypass vectors must trigger SCOPE-HARD: {viols:?}"
    );
    assert!(
        viols
            .iter()
            .any(|v| v.location.starts_with("zzz/payload.bin:")),
        "hunk-body truncation marker must not drop trailing hunkless file: {viols:?}"
    );

    // 3. Git C-quoted path ending in escaped quote preserves the quote.
    let quoted_diff = "diff --git \"a/src/foo\\\"\" \"b/src/foo\\\"\"\n--- /dev/null\n+++ \"b/src/foo\\\"\"\n@@ -0,0 +1,1 @@\n+x\n";
    let q_hunks = parse_diff_hunks(quoted_diff);
    assert!(
        q_hunks.contains_key("src/foo\""),
        "escaped trailing quote must be preserved: {:?}",
        q_hunks.keys().collect::<Vec<_>>()
    );

    // 4. Truncated/wrapped JSON blocker detection + advisory STORE-* isolation + empty-evidence gating.
    let checklist = build_dual_checklist(&sample_task("t-r3").spec, true, false);
    assert!(names_dual_blocker(
        r#"{"answers":[{"id":"SECRET","answer":"yes","evidence":"src/lib.rs:1"}],"issues":[{"severity":"P0""#,
        &checklist
    ));
    assert!(names_dual_blocker(
        r#"{"answers":[{"id":"SECRET","answer":"no","evidence":""}],"issues":[{"id":"I1","severity":"P0","category":"SEC","location":"src/a.rs:1","description":"b","proposed_fix":"f"}],"scores":[{"id":"G1""#,
        &checklist
    ));
    assert!(names_dual_blocker(
        r#"{"review":{"answers":[{"id":"SECRET","answer":"yes","evidence":"src/lib.rs:1"}],"issues":[]}}"#,
        &checklist
    ));
    assert!(!names_dual_blocker(
        r#"{"answers":[{"id":"STORE-PRIVACY","answer":"yes","evidence":"src/a.rs:1"}],"issues":[{"severity":"P2""#,
        &checklist
    ));
    // Adjacent STORE-* yes must NOT bleed across `},{` into a blocking `"no"` answer.
    assert!(!names_dual_blocker(
        r#"{"answers":[{"id":"STORE-PRIVACY","answer":"yes","evidence":""},{"id":"SECRET","answer":"no","evidence":""}],"issues":["#,
        &checklist
    ));
    // Long evidence between `"answer":"yes"` and `"id":"SECRET"` in truncated object MUST still latch.
    assert!(names_dual_blocker(
        r#"{"answers":[{"answer":"yes","evidence":"crates/aaf-engine/src/factory/dual_audit/turn.rs:410 very long evidence text here","id":"SECRET""#,
        &checklist
    ));
    let empty_ev_extra = json!({
        "answers": dual_answers(&checklist, &[("SECRET", "")]),
        "issues": [],
        "scores": scores_all(96),
        "extra": 1
    })
    .to_string();
    assert!(!can_use_compact_format_repair(
        &empty_ev_extra,
        &checklist,
        true
    ));
    // Multi-object reply where object 1 is clean PASS and object 2 has an uncited P1 MUST reject compact repair.
    let clean_obj = json!({
        "answers": dual_answers(&checklist, &[]),
        "issues": [],
        "scores": scores_all(98)
    })
    .to_string();
    let multi_uncited = format!(
        "{clean_obj}\n{{\"issues\":[{{\"id\":\"B1\",\"severity\":\"P1\",\"location\":\"\"}}]}}"
    );
    assert!(!can_use_compact_format_repair(
        &multi_uncited,
        &checklist,
        true
    ));

    // 5. Soft manifests in scope do NOT false-block as SCOPE-HARD.
    let soft_diff = "\
diff --git a/go.mod b/go.mod
--- /dev/null
+++ b/go.mod
@@ -0,0 +1,1 @@
+module example
diff --git a/pubspec.yaml b/pubspec.yaml
--- /dev/null
+++ b/pubspec.yaml
@@ -0,0 +1,1 @@
+name: app
diff --git a/Package.swift b/Package.swift
--- /dev/null
+++ b/Package.swift
@@ -0,0 +1,1 @@
+// swift
";
    let soft_conf = analyze_plan_conformance(soft_diff, &plan, &[".".into()], &globs);
    assert!(
        deterministic_scope_violations(&soft_conf).is_empty(),
        "soft manifests must not trigger SCOPE-HARD: {:?}",
        deterministic_scope_violations(&soft_conf)
    );

    // 6. .cargo, gradle symlinks, digit->UPPER CamelCase security names, and U+2028/U+061C trigger SCOPE-HARD.
    let r3_attack = "\
diff --git a/.cargo b/.cargo
new file mode 120000
diff --git a/gradle b/gradle
new file mode 120000
diff --git a/src/feature/OAuth2Client.kt b/src/feature/OAuth2Client.kt
--- /dev/null
+++ b/src/feature/OAuth2Client.kt
@@ -0,0 +1,1 @@
+class OAuth2Client
diff --git a/src/feature/X509Cert.rs b/src/feature/X509Cert.rs
--- /dev/null
+++ b/src/feature/X509Cert.rs
@@ -0,0 +1,1 @@
+pub struct X509Cert;
diff --git a/src/feature/Sha256Hmac.rs b/src/feature/Sha256Hmac.rs
--- /dev/null
+++ b/src/feature/Sha256Hmac.rs
@@ -0,0 +1,1 @@
+pub fn hmac() {}
diff --git a/src/feature/Auth0Provider.ts b/src/feature/Auth0Provider.ts
--- /dev/null
+++ b/src/feature/Auth0Provider.ts
@@ -0,0 +1,1 @@
+export const p = 1;
diff --git a/src/feature/bidi\u{061C}x.rs b/src/feature/bidi\u{061C}x.rs
--- /dev/null
+++ b/src/feature/bidi\u{061C}x.rs
@@ -0,0 +1,1 @@
+pub fn x() {}
diff --git a/docs/sep\u{2028}line.md b/docs/sep\u{2028}line.md
--- /dev/null
+++ b/docs/sep\u{2028}line.md
@@ -0,0 +1,1 @@
+hi
";
    let r3_conf = analyze_plan_conformance(r3_attack, &plan, &[".".into()], &globs);
    let r3_viols = deterministic_scope_violations(&r3_conf);
    assert_eq!(
        r3_viols.len(),
        8,
        "all 8 Round-3 vectors must trigger SCOPE-HARD: {r3_viols:?}"
    );
}
