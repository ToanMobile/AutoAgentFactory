#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_engine::cross_audit::diff_hunks::parse_diff_hunks_with_truncation;
use aaf_engine::cross_audit::prompt::{
    is_secondary_locale_file, summarize_diff_for_audit, summarize_diff_locales,
};

fn android_section(dir: &str, added_lines: &[&str]) -> String {
    let path = format!("core/common/src/main/res/{dir}/strings.xml");
    let mut body = String::new();
    for line in added_lines {
        body.push('+');
        body.push_str(line);
        body.push('\n');
    }
    format!(
        "diff --git a/{path} b/{path}\n\
         --- a/{path}\n\
         +++ b/{path}\n\
         @@ -10,1 +10,{} @@\n \
          <resources>\n\
         {body}",
        1 + added_lines.len()
    )
}

#[test]
fn locale_classifier_distinguishes_secondary_from_base_and_config_qualifiers() {
    assert!(is_secondary_locale_file(
        "core/common/src/main/res/values-vi/strings.xml"
    ));
    assert!(is_secondary_locale_file(
        "app/src/main/res/values-zh-rCN/strings.xml"
    ));
    assert!(is_secondary_locale_file(
        "app/src/main/res/values-b+sr+Latn/strings.xml"
    ));
    assert!(is_secondary_locale_file("ios/vi.lproj/Localizable.strings"));
    assert!(is_secondary_locale_file("lib/l10n/app_ja.arb"));

    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values-en/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values-en-rUS/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values-night/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values-v31/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "app/src/main/res/values-car/strings.xml"
    ));
    assert!(!is_secondary_locale_file(
        "core/common/src/main/res/values-vi/colors.xml"
    ));
    assert!(!is_secondary_locale_file(
        "ios/en.lproj/Localizable.strings"
    ));
    assert!(!is_secondary_locale_file(
        "ios/Base.lproj/Localizable.strings"
    ));
    assert!(!is_secondary_locale_file("lib/l10n/app_en.arb"));
}

#[test]
fn small_pr_with_under_four_secondary_locales_stays_verbatim() {
    let diff = format!(
        "{}{}{}",
        android_section(
            "values",
            &[r#"<string name="skipped">Skipped %1$d images</string>"#]
        ),
        android_section(
            "values-vi",
            &[r#"<string name="skipped">Đã bỏ %1$d ảnh</string>"#]
        ),
        android_section(
            "values-ja",
            &[r#"<string name="skipped">%1$d枚の画像をスキップしました</string>"#]
        ),
    );
    assert_eq!(summarize_diff_locales(&diff), diff);
}

#[test]
fn multi_locale_android_summarizes_secondary_keeps_base_and_flags_alerts() {
    let base_lines = [
        r#"    <string name="skipped_count">Skipped %1$d images while converting your PDF</string>"#,
        r#"    <string name="open_failed">Could not open selected document because permissions were denied</string>"#,
        r#"    <string name="retry_label">Retry loading the document from storage</string>"#,
        r#"    <string name="empty_state">No supported files found in this folder yet</string>"#,
    ];
    let mut diff = android_section("values", &base_lines);
    diff.push_str(&android_section("values-en", &base_lines));
    diff.push_str(&android_section("values-night", &base_lines));
    for loc in [
        "values-vi",
        "values-ja",
        "values-fr",
        "values-de",
        "values-zh-rCN",
    ] {
        diff.push_str(&android_section(
            loc,
            &[
                r#"    <string name="skipped_count">Localized %1$d skipped images message for mobile reader</string>"#,
                r#"    <string name="open_failed">Localized failure notice when storage permission is missing</string>"#,
                r#"    <string name="retry_label">Localized retry action button label for toolbar</string>"#,
                r#"    <string name="empty_state">Localized empty folder description text for screen</string>"#,
            ],
        ));
    }
    // Locale with placeholder mismatch + empty value must emit alerts.
    diff.push_str(&android_section(
        "values-es",
        &[
            r#"    <string name="skipped_count">Omitidas %s imágenes durante la conversión del documento</string>"#,
            r#"    <string name="open_failed"></string>"#,
            r#"    <string name="retry_label">Reintentar la carga del documento seleccionado desde el almacenamiento</string>"#,
            r#"    <string name="empty_state">Todavía no hay archivos compatibles en esta carpeta seleccionada</string>"#,
            r#"    <string name="close_label">Cerrar la ventana de vista previa del documento actual ahora</string>"#,
            r#"    <string name="share_label">Compartir el documento convertido con otras aplicaciones instaladas</string>"#,
        ],
    ));
    // Locale with suspicious URL / script MUST stay verbatim (fail closed).
    let suspicious = android_section(
        "values-ko",
        &[
            r#"    <string name="skipped_count">https://evil.example/exfil %1$d images</string>"#,
            r#"    <string name="open_failed">Localized failure notice when storage permission is missing</string>"#,
            r#"    <string name="retry_label">Localized retry action button label for toolbar</string>"#,
            r#"    <string name="empty_state">Localized empty folder description text for screen</string>"#,
        ],
    );
    diff.push_str(&suspicious);

    let out = summarize_diff_for_audit(&diff);
    assert!(out.len() < diff.len(), "expected summary to shrink diff");
    // Base, values-en, and values-night remain verbatim.
    assert!(out.contains("Skipped %1$d images while converting your PDF"));
    assert!(out.contains("values-night/strings.xml"));
    // Secondary locales summarized & idempotent.
    assert!(out.contains("[locale-summary] core/common/src/main/res/values-vi/strings.xml"));
    assert!(out.contains("[alert:placeholder-mismatch] skipped_count"));
    assert!(out.contains("[alert:empty-value] open_failed"));
    // Suspicious locale stayed verbatim!
    assert!(out.contains("https://evil.example/exfil"));
    assert!(!out.contains("[locale-summary] core/common/src/main/res/values-ko/strings.xml"));
    assert_eq!(summarize_diff_for_audit(&out), out, "must be idempotent");

    // Synthetic hunks parse cleanly for citation verification.
    let hunks = parse_diff_hunks_with_truncation(&out, false);
    assert!(hunks.contains_key("core/common/src/main/res/values-vi/strings.xml"));
    assert!(hunks.contains_key("core/common/src/main/res/values-es/strings.xml"));
}

#[test]
fn ios_and_flutter_secondary_locales_summarize_cleanly() {
    let mut ios = String::from(
        "diff --git a/ios/en.lproj/Localizable.strings b/ios/en.lproj/Localizable.strings\n\
         --- a/ios/en.lproj/Localizable.strings\n\
         +++ b/ios/en.lproj/Localizable.strings\n\
         @@ -1,1 +1,2 @@\n \
          \"title\" = \"Reader\";\n\
         +\"skip\" = \"Skipped %@ items\";\n",
    );
    for loc in ["vi", "ja", "fr", "de"] {
        ios.push_str(&format!(
            "diff --git a/ios/{loc}.lproj/Localizable.strings b/ios/{loc}.lproj/Localizable.strings\n\
             --- a/ios/{loc}.lproj/Localizable.strings\n\
             +++ b/ios/{loc}.lproj/Localizable.strings\n\
             @@ -1,1 +1,6 @@\n \
              \"title\" = \"Reader\";\n\
             +\"skip\" = \"Localized skipped %@ items message for mobile reader\";\n\
             +\"retry\" = \"Localized retry opening document action button\";\n\
             +\"done\" = \"Localized finished processing all selected pages\";\n\
             +\"close\" = \"Localized close preview sheet button description\";\n\
             +\"share\" = \"Localized share exported document with another app\";\n"
        ));
    }
    let out = summarize_diff_locales(&ios);
    assert!(out.contains("Skipped %@ items"));
    assert!(out.contains("[locale-summary] ios/vi.lproj/Localizable.strings"));
    assert!(out.len() < ios.len());

    let mut arb = String::new();
    for loc in ["vi", "ja", "fr", "de"] {
        arb.push_str(&format!(
            "diff --git a/lib/l10n/app_{loc}.arb b/lib/l10n/app_{loc}.arb\n\
             --- a/lib/l10n/app_{loc}.arb\n\
             +++ b/lib/l10n/app_{loc}.arb\n\
             @@ -1,2 +1,5 @@\n \
              {{\n\
             +  \"skip\": \"Localized skipped {{count}} items message for mobile reader\",\n\
             +  \"retry\": \"Localized retry opening document action button\",\n\
             +  \"done\": \"Localized finished processing all selected pages\"\n \
              }}\n"
        ));
    }
    let out_arb = summarize_diff_locales(&arb);
    assert!(out_arb.contains("[locale-summary] lib/l10n/app_vi.arb"));
    assert!(out_arb.len() < arb.len());
}

#[test]
fn dual_review_prompt_and_parser_honor_explicit_max_diff_bytes() {
    use aaf_domain::task::{ImmutableSpec, Priority, Task};
    use aaf_engine::cross_audit::dual_verdict::{
        DualReviewerOutcome, build_adjudicate_prompt_with_limit, build_dual_checklist,
        build_dual_review_prompt_with_limit, parse_and_validate_dual_review_with_limit,
    };
    use aaf_engine::cross_audit::prompt::TRUNCATION_MARKER;
    use serde_json::json;

    let task = Task::new_root(
        "t-big",
        "/repo",
        "Large diff test",
        ImmutableSpec {
            description: "Handle large diff".into(),
            acceptance: vec!["passes".into()],
        },
        Priority::P1,
        1_000,
    );
    let checklist = build_dual_checklist(&task.spec, false, false);
    let answers: Vec<_> = checklist
        .iter()
        .map(|it| json!({ "id": it.id, "answer": "no", "evidence": "" }))
        .collect();
    let scores: Vec<_> = ["G1", "G2", "G3", "G4", "G5", "G6", "G7"]
        .into_iter()
        .map(|id| json!({ "id": id, "score": 97, "deduction_reason": "minor" }))
        .collect();
    let pass_raw = json!({ "answers": answers, "issues": [], "scores": scores }).to_string();
    let large_diff = format!(
        "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,1 +1,2 @@\n pub fn f() {{}}\n+// {}\n",
        "x".repeat(420_000)
    );

    // Above 400KB default with a 500KB explicit limit: must NOT truncate and must PASS.
    let prompt_500k =
        build_dual_review_prompt_with_limit(&task, &large_diff, None, &checklist, true, 500_000);
    assert!(!prompt_500k.contains(TRUNCATION_MARKER));
    let adj_500k = build_adjudicate_prompt_with_limit(&task, &large_diff, None, &[], 500_000);
    assert!(!adj_500k.contains(TRUNCATION_MARKER));
    let out_500k = parse_and_validate_dual_review_with_limit(
        &pass_raw,
        &checklist,
        &large_diff,
        None,
        true,
        95,
        90,
        100,
        500_000,
    );
    assert!(
        matches!(out_500k, DualReviewerOutcome::Pass { .. }),
        "expected Pass under 500KB limit, got {out_500k:?}"
    );

    // Tighter 50KB limit must truncate and return Inconclusive.
    let out_50k = parse_and_validate_dual_review_with_limit(
        &pass_raw,
        &checklist,
        &large_diff,
        None,
        true,
        95,
        90,
        100,
        50_000,
    );
    assert!(matches!(
        out_50k,
        DualReviewerOutcome::Inconclusive { ref reason } if reason.contains("diff was truncated")
    ));
}

#[test]
fn smuggling_encoded_urls_secrets_and_renames_fail_closed_verbatim() {
    let benign_lines = [
        r#"    <string name="a">Localized alpha label text for document viewer toolbar</string>"#,
        r#"    <string name="b">Localized beta label text for document viewer toolbar</string>"#,
        r#"    <string name="c">Localized gamma label text for document viewer toolbar</string>"#,
        r#"    <string name="d">Localized delta label text for document viewer toolbar</string>"#,
        r#"    <string name="e">Localized epsilon label text for document viewer toolbar</string>"#,
    ];
    let mut diff = String::new();
    for loc in ["values-vi", "values-ja", "values-fr", "values-de"] {
        diff.push_str(&android_section(loc, &benign_lines));
    }
    // 1. Two <string> entries on one line (smuggling hidden key).
    let two_entries = android_section(
        "values-it",
        &[
            r#"    <string name="ok">x</string><string name="api_base_url">bad</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    // 2. Comment prefix smuggling.
    let comment_smuggle = android_section(
        "values-pt",
        &[
            r#"    <!-- c --><string name="hidden_key">sneaky</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    // 3. Encoded URL entity + Google API key + intent:// + protocol-relative URL.
    let encoded_url = android_section(
        "values-nl",
        &[
            r#"    <string name="a">Visit &#104;ttps://evil.example now for updates</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    let google_key = android_section(
        "values-pl",
        &[
            r#"    <string name="a">AIzaSyD-fake-mobile-api-key-value-1234567</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    let intent_url = android_section(
        "values-sv",
        &[
            r#"    <string name="a">intent://evil.example/#Intent;end</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    let proto_rel = android_section(
        "values-tr",
        &[
            r#"    <string name="a">//evil.example/payload</string>"#,
            benign_lines[1],
            benign_lines[2],
            benign_lines[3],
            benign_lines[4],
        ],
    );
    // 4. Rename across modules must stay verbatim.
    let renamed = "diff --git a/app/src/main/res/values-ru/strings.xml b/evil/src/main/res/values-ru/strings.xml\n\
                   rename from app/src/main/res/values-ru/strings.xml\n\
                   rename to evil/src/main/res/values-ru/strings.xml\n\
                   --- a/app/src/main/res/values-ru/strings.xml\n\
                   +++ b/evil/src/main/res/values-ru/strings.xml\n\
                   @@ -1,1 +1,6 @@\n \
                    <resources>\n\
                   +    <string name=\"a\">Localized alpha label text for document viewer toolbar</string>\n\
                   +    <string name=\"b\">Localized beta label text for document viewer toolbar</string>\n\
                   +    <string name=\"c\">Localized gamma label text for document viewer toolbar</string>\n\
                   +    <string name=\"d\">Localized delta label text for document viewer toolbar</string>\n\
                   +    <string name=\"e\">Localized epsilon label text for document viewer toolbar</string>\n";
    for s in [
        &two_entries,
        &comment_smuggle,
        &encoded_url,
        &google_key,
        &intent_url,
        &proto_rel,
    ] {
        diff.push_str(s);
    }
    diff.push_str(renamed);

    let out = summarize_diff_locales(&diff);
    assert!(out.contains("[locale-summary] core/common/src/main/res/values-vi/strings.xml"));
    for must_stay_verbatim in [
        r#"<string name="api_base_url">bad</string>"#,
        r#"<!-- c --><string name="hidden_key">sneaky</string>"#,
        "&#104;ttps://evil.example",
        "AIzaSyD-fake-mobile-api-key-value-1234567",
        "intent://evil.example/#Intent;end",
        "//evil.example/payload",
        "rename to evil/src/main/res/values-ru/strings.xml",
    ] {
        assert!(
            out.contains(must_stay_verbatim),
            "expected fail-closed verbatim retention for: {must_stay_verbatim}"
        );
    }

    // iOS & Flutter multi-entry / comment smuggling also fail closed verbatim.
    let mut ios_arb = String::new();
    for loc in ["vi", "ja", "fr", "de"] {
        ios_arb.push_str(&format!(
            "diff --git a/ios/{loc}.lproj/Localizable.strings b/ios/{loc}.lproj/Localizable.strings\n\
             --- a/ios/{loc}.lproj/Localizable.strings\n\
             +++ b/ios/{loc}.lproj/Localizable.strings\n\
             @@ -1,1 +1,6 @@\n \
              \"title\" = \"Reader\";\n\
             +\"k1\" = \"Localized first string label for toolbar action\";\n\
             +\"k2\" = \"Localized second string label for toolbar action\";\n\
             +\"k3\" = \"Localized third string label for toolbar action\";\n\
             +\"k4\" = \"Localized fourth string label for toolbar action\";\n\
             +\"k5\" = \"Localized fifth string label for toolbar action\";\n"
        ));
    }
    ios_arb.push_str(
        "diff --git a/ios/es.lproj/Localizable.strings b/ios/es.lproj/Localizable.strings\n\
         --- a/ios/es.lproj/Localizable.strings\n\
         +++ b/ios/es.lproj/Localizable.strings\n\
         @@ -1,1 +1,3 @@\n \
          \"title\" = \"Reader\";\n\
         +/* a */ \"smuggled_ios\" = \"evil\"; /* b */\n\
         +\"ok\" = \"x\"; \"second_ios\" = \"evil\";\n\
         diff --git a/lib/l10n/app_es.arb b/lib/l10n/app_es.arb\n\
         --- a/lib/l10n/app_es.arb\n\
         +++ b/lib/l10n/app_es.arb\n\
         @@ -1,2 +1,4 @@\n \
          {\n\
         +  \"@@locale\": \"es\", \"smuggled_arb\": \"evil\",\n\
         +  \"ok\": \"x\", \"second_arb\": \"evil\"\n \
          }\n",
    );
    let out2 = summarize_diff_locales(&ios_arb);
    assert!(out2.contains("smuggled_ios"));
    assert!(out2.contains("second_ios"));
    assert!(out2.contains("smuggled_arb"));
    assert!(out2.contains("second_arb"));
}

#[test]
fn base_lproj_order_sensitive_placeholders_and_module_isolation() {
    let mut diff = String::from(
        "diff --git a/ios/Base.lproj/Localizable.strings b/ios/Base.lproj/Localizable.strings\n\
         --- a/ios/Base.lproj/Localizable.strings\n\
         +++ b/ios/Base.lproj/Localizable.strings\n\
         @@ -1,1 +1,3 @@\n \
          \"title\" = \"Reader\";\n\
         +\"fmt_order\" = \"User %s has %d files\";\n\
         +\"long_cnt\" = \"Total %lld bytes\";\n\
         diff --git a/app/src/main/res/values/strings.xml b/app/src/main/res/values/strings.xml\n\
         --- a/app/src/main/res/values/strings.xml\n\
         +++ b/app/src/main/res/values/strings.xml\n\
         @@ -1,1 +1,2 @@\n \
          <resources>\n\
         +    <string name=\"fmt_order\">No placeholders in Android app module</string>\n",
    );
    for loc in ["vi", "ja", "fr", "de"] {
        diff.push_str(&format!(
            "diff --git a/ios/{loc}.lproj/Localizable.strings b/ios/{loc}.lproj/Localizable.strings\n\
             --- a/ios/{loc}.lproj/Localizable.strings\n\
             +++ b/ios/{loc}.lproj/Localizable.strings\n\
             @@ -1,1 +1,8 @@\n \
              \"title\" = \"Reader\";\n\
             +\"fmt_order\" = \"Swapped %d files for user %s in mobile reader\";\n\
             +\"long_cnt\" = \"Missing length specifier %d bytes in mobile reader\";\n\
             +\"k3\" = \"Localized third string label for toolbar action\";\n\
             +\"k4\" = \"Localized fourth string label for toolbar action\";\n\
             +\"k5\" = \"Localized fifth string label for toolbar action\";\n\
             +\"k6\" = \"Localized sixth string label for toolbar action\";\n\
             +\"k7\" = \"Localized seventh string label for toolbar action\";\n"
        ));
    }
    let out = summarize_diff_locales(&diff);
    assert!(
        out.contains("[alert:placeholder-mismatch] fmt_order"),
        "swapped %s %d -> %d %s must alert and not be clobbered by Android module: {out}"
    );
    assert!(
        out.contains("[alert:placeholder-mismatch] long_cnt"),
        "%lld -> %d mismatch from Base.lproj must alert: {out}"
    );
}
