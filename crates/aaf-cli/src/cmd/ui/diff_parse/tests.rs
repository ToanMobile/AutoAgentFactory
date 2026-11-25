#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn parses_multi_file_diff_statuses_line_numbers_and_head_lock() {
    let raw = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,3 +10,4 @@ fn run()
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 return a + b;
\\ No newline at end of file
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+pub fn hi() {}
+pub fn bye() {}
diff --git a/tests/old_test.rs b/tests/old_test.rs
deleted file mode 100644
--- a/tests/old_test.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-#[test]
-fn gone() {}
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 100%
rename from src/old_name.rs
rename to src/new_name.rs
diff --git a/assets/logo.png b/assets/logo.png
Binary files a/assets/logo.png and b/assets/logo.png differ
";
    let d = parse_unified_diff(raw, true, |p| p.starts_with("tests/"));
    assert_eq!(d.summary.files_changed, 5);
    assert_eq!((d.summary.insertions, d.summary.deletions), (4, 3));
    assert!(d.summary.truncated);
    assert_eq!(d.summary.head_locked_count, 1);

    let f0 = &d.files[0];
    assert_eq!(
        (f0.path.as_str(), f0.status.as_str()),
        ("src/lib.rs", "modified")
    );
    assert!(!f0.head_locked);
    assert_eq!((f0.additions, f0.deletions), (2, 1));
    assert_eq!(f0.hunks.len(), 1);
    assert_eq!((f0.hunks[0].old_start, f0.hunks[0].new_start), (10, 10));

    let lines = &f0.hunks[0].lines;
    assert_eq!(
        lines[0],
        DiffLine {
            kind: "ctx",
            old_ln: Some(10),
            new_ln: Some(10),
            text: "let a = 1;".into(),
        }
    );
    assert_eq!(
        lines[1],
        DiffLine {
            kind: "del",
            old_ln: Some(11),
            new_ln: None,
            text: "let b = 2;".into(),
        }
    );
    assert_eq!(
        lines[2],
        DiffLine {
            kind: "add",
            old_ln: None,
            new_ln: Some(11),
            text: "let b = 3;".into(),
        }
    );
    assert_eq!(
        lines[3],
        DiffLine {
            kind: "add",
            old_ln: None,
            new_ln: Some(12),
            text: "let c = 4;".into(),
        }
    );
    assert_eq!(
        lines[4],
        DiffLine {
            kind: "ctx",
            old_ln: Some(12),
            new_ln: Some(13),
            text: "return a + b;".into(),
        }
    );
    assert_eq!(
        lines[5],
        DiffLine {
            kind: "meta",
            old_ln: None,
            new_ln: None,
            text: "\\ No newline at end of file".into(),
        }
    );

    assert_eq!(
        (d.files[1].status.as_str(), d.files[1].additions),
        ("added", 2)
    );
    assert_eq!(
        (
            d.files[2].status.as_str(),
            d.files[2].deletions,
            d.files[2].head_locked
        ),
        ("deleted", 2, true)
    );
    assert_eq!(
        (d.files[3].status.as_str(), d.files[3].path.as_str()),
        ("renamed", "src/new_name.rs")
    );
    assert!(d.files[4].binary);

    let json = serde_json::to_string(&d).unwrap();
    let roundtrip: StructuredDiff = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtrip, d);
}

#[test]
fn handles_renamed_head_locked_origin_and_quoted_paths() {
    let raw = "\
diff --git \"a/tests/my test.rs\" \"b/src/moved.rs\"
similarity index 100%
rename from \"tests/my test.rs\"
rename to \"src/moved.rs\"
";
    let d = parse_unified_diff(raw, false, |p| p.starts_with("tests/"));
    assert_eq!(d.summary.files_changed, 1);
    assert_eq!(d.summary.head_locked_count, 1);
    assert_eq!(d.files[0].path, "src/moved.rs");
    assert_eq!(d.files[0].status, "renamed");
    assert!(d.files[0].head_locked);
}

#[test]
fn scrubs_api_keys_and_multiline_pem_blocks_in_diff() {
    let ant = ["sk-", "ant-secretToken123456"].concat();
    let gh = ["ghp_", "0123456789abcdefABCD"].concat();
    let pem_begin = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
    let pem_end = ["-----END ", "PRIVATE KEY-----"].concat();
    let raw = format!(
        "diff --git a/cfg.rs b/cfg.rs\n\
--- a/cfg.rs\n\
+++ b/cfg.rs\n\
@@ -1,1 +1,5 @@\n\
-const A: &str = \"{ant}\";\n\
+const B: &str = \"{gh}\";\n\
+{pem_begin}\n\
+MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcw\n\
+{pem_end}\n"
    );
    let parsed = parse_unified_diff(&raw, false, |_| false);
    let lines = &parsed.files[0].hunks[0].lines;
    assert_eq!(lines.len(), 5);
    assert!(!lines[0].text.contains(&ant) && lines[0].text.contains(REDACTED));
    assert!(!lines[1].text.contains(&gh) && lines[1].text.contains(REDACTED));
    assert!(!lines[2].text.contains("BEGIN") && lines[2].text.contains(REDACTED));
    assert_eq!(lines[3].text, REDACTED);
    assert_eq!(lines[4].text, REDACTED);

    let scrubbed_raw = scrub_diff_text(&raw);
    assert!(!scrubbed_raw.contains(&ant));
    assert!(!scrubbed_raw.contains(&gh));
    assert!(!scrubbed_raw.contains("MIIEvQIBADAN"));
}

#[test]
fn format_inline_comments_happy_path_and_sanitization() {
    let mk = |path: &str, start: u32, end: Option<u32>, body: &str| DiffComment {
        path: path.into(),
        line_start: start,
        line_end: end,
        body: body.into(),
    };
    let ok = format_inline_comments(
        "--rerun please refine",
        &[
            mk(
                "src/main.rs",
                12,
                Some(15),
                "\x1b[31mFix\x1b[0m\nmulti\r\n\x07\x00line\tok",
            ),
            mk("src/lib.rs", 42, Some(42), "  Check edge case  "),
            mk(
                "src/mod..rs",
                7,
                None,
                "\x1b]8;;https://x\x07clean\x1b]8;;\x07",
            ),
        ],
    )
    .unwrap();
    assert!(ok.starts_with("--rerun please refine\n\n[Human Diff Review Comments]\n"));
    assert!(ok.contains("- src/main.rs:12-15: Fix multi line\tok"));
    assert!(ok.contains("- src/lib.rs:42: Check edge case"));
    assert!(ok.contains("- src/mod..rs:7: clean"));
    assert!(!ok.contains('\x1b') && !ok.contains('\x07') && !ok.contains('\0'));

    let empty_base = format_inline_comments("", &[mk("src/a.rs", 3, None, "tighten")]).unwrap();
    assert_eq!(
        empty_base,
        "[Human Diff Review Comments]\n- src/a.rs:3: tighten"
    );

    let no_comments = format_inline_comments("  just retry  ", &[]).unwrap();
    assert_eq!(no_comments, "just retry");
}

#[test]
fn format_inline_comments_rejects_invalid_inputs() {
    let mk = |path: &str, start: u32, end: Option<u32>, body: &str| DiffComment {
        path: path.into(),
        line_start: start,
        line_end: end,
        body: body.into(),
    };
    assert!(format_inline_comments("", &[mk("../etc/passwd", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("src/../../etc/passwd", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("src\\..\\secret", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("/etc/passwd", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("C:/Windows/win.ini", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("a\0b.rs", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("a\nb.rs", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("   ", 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk(&"a".repeat(261), 1, None, "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("src/a.rs", 0, Some(1), "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("src/a.rs", 10, Some(5), "bad")]).is_err());
    assert!(format_inline_comments("", &[mk("src/a.rs", 1, None, "\x1b[31m \n\x00")]).is_err());

    let too_many: Vec<_> = (0..=MAX_COMMENTS)
        .map(|i| mk("src/a.rs", (i + 1) as u32, None, "ok"))
        .collect();
    assert!(format_inline_comments("", &too_many).is_err());

    let huge = mk("src/a.rs", 1, None, &"x".repeat(MAX_COMMENTS_TOTAL_BYTES));
    assert!(format_inline_comments("base", &[huge]).is_err());
}

#[test]
fn diff_comment_supports_line_alias_and_file_level_zero() {
    let aliased: DiffComment =
        serde_json::from_str(r#"{"path":"a.rs","line":5,"body":"hi"}"#).unwrap();
    assert_eq!(aliased.line_start, 5);
    assert_eq!(aliased.line_end, None);

    let file_level: DiffComment =
        serde_json::from_str(r#"{"path":"img.png","line_start":0,"body":"binary note"}"#).unwrap();
    let file_level_end0: DiffComment = serde_json::from_str(
        r#"{"path":"empty.txt","line_start":0,"line_end":0,"body":"empty note"}"#,
    )
    .unwrap();

    let formatted = format_inline_comments("", &[aliased, file_level, file_level_end0]).unwrap();
    assert!(formatted.contains("- a.rs:5: hi"));
    assert!(formatted.contains("- img.png: binary note"));
    assert!(formatted.contains("- empty.txt: empty note"));
    assert!(!formatted.contains("img.png:0") && !formatted.contains(":L0"));
}

#[test]
fn is_safe_git_ref_rejects_option_injection_and_accepts_normal_refs() {
    for bad in [
        "",
        "--output=/tmp/x",
        "-foo",
        "-",
        "a b",
        "main\tfoo",
        "main\nfoo",
        "main\0foo",
        "a..b",
        "HEAD@{0}",
        "@",
        "/leading",
        "trailing/",
        "bad.lock.",
        "a//b",
    ] {
        assert!(!is_safe_git_ref(bad), "expected reject for {bad:?}");
    }
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for ok in [
        "main",
        "aaf/task-1",
        "origin/main",
        "origin/HEAD",
        "HEAD",
        sha,
    ] {
        assert!(is_safe_git_ref(ok), "expected accept for {ok:?}");
    }
    assert!(is_hex_sha(sha));
    assert!(!is_hex_sha("short"));
    assert!(!is_hex_sha(&"z".repeat(40)));
}

#[test]
fn steer_note_over_2000_bytes_is_rejected_while_rework_allows_4096() {
    let big_body = "x".repeat(2005);
    let raw_comment = serde_json::json!({
        "path": "src/lib.rs",
        "line_start": 1,
        "body": big_body,
    });
    let err = super::super::actions::parse_and_format_comments(
        "steer",
        "",
        std::slice::from_ref(&raw_comment),
    )
    .unwrap_err();
    assert!(
        err.to_ascii_lowercase().starts_with("invalid"),
        "expected HTTP 400 invalid error, got: {err}"
    );
    assert!(err.contains("2000"));

    // Plain steer note without comments over 2000 bytes is also rejected.
    let err_plain =
        super::super::actions::parse_and_format_comments("steer", &"a".repeat(2001), &[])
            .unwrap_err();
    assert!(err_plain.to_ascii_lowercase().starts_with("invalid"));

    // Rework still allows up to 4096 bytes.
    let ok_rework =
        super::super::actions::parse_and_format_comments("rework", "", &[raw_comment]).unwrap();
    assert!(ok_rework.len() > MAX_STEER_NOTE_BYTES && ok_rework.len() <= MAX_COMMENTS_TOTAL_BYTES);
}

#[test]
fn scrub_diff_text_redacts_truncated_pem_without_end_marker() {
    let pem_begin = ["-----BEGIN ", "OPENSSH PRIVATE KEY-----"].concat();
    let raw = format!(
        "diff --git a/key.pem b/key.pem\n\
--- /dev/null\n\
+++ b/key.pem\n\
@@ -0,0 +1,3 @@\n\
+{pem_begin}\n\
+b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ\n\
+secretPayloadLineTruncatedBeforeEndMarker\n\
diff --git a/ok.rs b/ok.rs\n\
--- a/ok.rs\n\
+++ b/ok.rs\n\
@@ -1,1 +1,1 @@\n\
+let safe = 1;\n"
    );
    let scrubbed = scrub_diff_text(&raw);
    assert!(!scrubbed.contains("BEGIN"));
    assert!(!scrubbed.contains("b3BlbnNzaC1rZXktdjE"));
    assert!(!scrubbed.contains("secretPayloadLineTruncatedBeforeEndMarker"));
    assert!(scrubbed.contains(&format!("+{REDACTED}")));
    assert!(scrubbed.contains("+let safe = 1;"));
}

#[test]
fn handles_filename_containing_space_b_slash_and_deleted_file() {
    let raw = "\
diff --git a/dir/foo b/bar.rs b/dir/foo b/bar.rs
index 1111111..2222222 100644
--- a/dir/foo b/bar.rs
+++ b/dir/foo b/bar.rs
@@ -1,1 +1,1 @@
-let a = 1;
+let a = 2;
diff --git a/gone b/file.rs b/gone b/file.rs
deleted file mode 100644
--- a/gone b/file.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-bye();
";
    let parsed = parse_unified_diff(raw, false, |_| false);
    assert_eq!(parsed.files.len(), 2);
    assert_eq!(parsed.files[0].path, "dir/foo b/bar.rs");
    assert_eq!(parsed.files[0].status, "modified");
    assert_eq!(parsed.files[1].path, "gone b/file.rs");
    assert_eq!(parsed.files[1].status, "deleted");
}
