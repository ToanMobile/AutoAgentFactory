#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_engine::detector::fingerprint::{MAX_FRAMES_PER_RUN, fingerprint, normalize};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("fixtures/detect/", $name))
    };
}

/// (language, occurrence A, occurrence B of the same bug, same trace with a
/// different exception type, same trace with a different frame symbol).
const CASES: [(&str, &str, &str, &str, &str); 5] = [
    (
        "jvm",
        fixture!("jvm_a.txt"),
        fixture!("jvm_b.txt"),
        fixture!("jvm_other_type.txt"),
        fixture!("jvm_other_frame.txt"),
    ),
    (
        "swift",
        fixture!("swift_a.txt"),
        fixture!("swift_b.txt"),
        fixture!("swift_other_type.txt"),
        fixture!("swift_other_frame.txt"),
    ),
    (
        "python",
        fixture!("python_a.txt"),
        fixture!("python_b.txt"),
        fixture!("python_other_type.txt"),
        fixture!("python_other_frame.txt"),
    ),
    (
        "rust",
        fixture!("rust_a.txt"),
        fixture!("rust_b.txt"),
        fixture!("rust_other_type.txt"),
        fixture!("rust_other_frame.txt"),
    ),
    (
        "js",
        fixture!("js_a.txt"),
        fixture!("js_b.txt"),
        fixture!("js_other_type.txt"),
        fixture!("js_other_frame.txt"),
    ),
];

#[test]
fn two_occurrences_of_the_same_bug_hash_equal() {
    for (lang, a, b, _, _) in CASES {
        assert_ne!(a, b, "{lang}: fixtures must differ in raw bytes");
        assert_eq!(normalize(a), normalize(b), "{lang}");
        assert_eq!(fingerprint(a), fingerprint(b), "{lang}");
    }
}

#[test]
fn a_different_exception_type_hashes_differently() {
    for (lang, _, b, other_type, _) in CASES {
        assert_ne!(fingerprint(b), fingerprint(other_type), "{lang}");
    }
}

#[test]
fn a_different_frame_symbol_hashes_differently() {
    for (lang, _, b, _, other_frame) in CASES {
        assert_ne!(fingerprint(b), fingerprint(other_frame), "{lang}");
    }
}

#[test]
fn languages_do_not_collide() {
    let mut seen: Vec<String> = CASES.iter().map(|case| fingerprint(case.1)).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), CASES.len());
}

#[test]
fn fingerprint_is_sha256_hex_of_the_normalised_text() {
    let fp = fingerprint(fixture!("jvm_a.txt"));
    assert_eq!(fp.len(), 64);
    assert!(
        fp.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(
        fp,
        aaf_fp::sha256_hex(normalize(fixture!("jvm_a.txt")).as_bytes())
    );
}

#[test]
fn normalised_jvm_trace_keeps_types_messages_and_symbols() {
    let text = normalize(fixture!("jvm_a.txt"));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "FATAL EXCEPTION: main");
    assert_eq!(lines[1], "Process: com.example.shop, PID: <n>");
    assert_eq!(
        lines[2],
        "java.lang.IllegalStateException: Cart <n> has <n> items but total is <n>"
    );
    assert_eq!(
        lines[3],
        "at com.example.shop.cart.CartViewModel$refresh$<n>.invokeSuspend(CartViewModel.kt:<n>)"
    );
    assert!(text.contains("CartFragment$$ExternalSyntheticLambda<n>.run"));
    assert!(text.contains("Caused by: java.lang.NumberFormatException"));
    assert!(!text.contains("more"), "elided-frame counters are dropped");
    assert!(!text.contains("AndroidRuntime"), "logcat prefix is dropped");
}

#[test]
fn masks_volatile_tokens() {
    let cases = [
        (
            "id 6f1a2b3c-4d5e-4f60-8a1b-2c3d4e5f6071 gone",
            "id <uuid> gone",
        ),
        ("at 2026-11-21T10:22:33.123Z failed", "at <ts> failed"),
        ("at 2026-11-21 10:22:33,123+07:00 failed", "at <ts> failed"),
        ("on Thu Nov 21 10:22:33 2026", "on <ts> <ts> <n>"),
        ("on 21 Nov 2026", "on <ts>"),
        ("on 2026/11/21", "on <ts>"),
        ("ptr 0x7ffee4b3c2a8 freed", "ptr <addr> freed"),
        (
            "com.example.Foo@4b1210ee leaked",
            "com.example.Foo@<hex> leaked",
        ),
        ("commit 90b35a6239c3 broke it", "commit <hex> broke it"),
        ("pid=4821 tid=4830", "pid=<n> tid=<n>"),
        (
            "Thread-12 pool-3-thread-7",
            "Thread-<n> pool-<n>-thread-<n>",
        ),
        ("timeout after 1500ms", "timeout after <n>ms"),
        ("KeyError: 'user_8231'", "KeyError: 'user_<n>'"),
        ("open /tmp/build-8841/src/main.rs", "open <tmp>/src/main.rs"),
        (
            "open /private/var/folders/q1/x8k2/T/tmpab12/a.py",
            "open <tmp>/a.py",
        ),
        (
            r"open C:\Users\bob\AppData\Local\Temp\xyz12\a.py",
            r"open <tmp>\a.py",
        ),
        ("open /Users/alice/proj/a.py", "open ~/proj/a.py"),
        ("open \"/home/ci/proj/a.py\"", "open \"~/proj/a.py\""),
        (r"open C:\Users\bob\proj\a.py", r"open ~\proj\a.py"),
        ("Foo$3.run(Foo.java:10)", "Foo$<n>.run(Foo.java:<n>)"),
        ("lambda$onCreate$4", "lambda$onCreate$<n>"),
        ("core::ops::call::h0123456789abcdef", "core::ops::call"),
        (
            "\u{1b}[31mError\u{1b}[0m:   too    many   spaces",
            "Error: too many spaces",
        ),
        ("E/AndroidRuntime( 4821): boom", "boom"),
    ];
    for (input, expected) in cases {
        assert_eq!(normalize(input), expected, "input: {input}");
    }
}

#[test]
fn keeps_tokens_that_identify_the_bug() {
    // Digits inside an identifier are part of the symbol.
    assert_ne!(
        fingerprint("at okhttp3.internal.http1.Http1ExchangeCodec.read(X.kt:1)"),
        fingerprint("at okhttp3.internal.http2.Http2ExchangeCodec.read(X.kt:1)")
    );
    assert_eq!(normalize("Base64 sha256 utf8"), "Base64 sha256 utf8");
    // Hex-looking words without digits are words.
    assert_eq!(normalize("defaced facade"), "defaced facade");
    // A `tmp` or `home` directory in the middle of a path is not the
    // machine's temp or home directory.
    assert_eq!(
        normalize("open /srv/app/tmp/x.py"),
        "open /srv/app/tmp/x.py"
    );
    assert_eq!(normalize("open /data/home/x.py"), "open /data/home/x.py");
}

fn recursive_trace(depth: usize) -> String {
    let mut trace = String::from("java.lang.StackOverflowError\n");
    for _ in 0..depth {
        trace.push_str("\tat com.example.Tree.walk(Tree.java:42)\n");
    }
    trace.push_str("\tat com.example.Main.main(Main.java:7)\n");
    trace
}

#[test]
fn recursion_depth_does_not_change_the_fingerprint() {
    assert_eq!(
        fingerprint(&recursive_trace(1000)),
        fingerprint(&recursive_trace(1024))
    );
    assert_eq!(
        normalize(&recursive_trace(3)),
        "java.lang.StackOverflowError\nat com.example.Tree.walk(Tree.java:<n>)\nat com.example.Main.main(Main.java:<n>)"
    );
    let python = "Traceback (most recent call last):\n  File \"a.py\", line 3, in walk\n    walk(n)\n  [Previous line repeated 996 more times]\nRecursionError: maximum recursion depth exceeded\n";
    assert_eq!(
        fingerprint(python),
        fingerprint(&python.replace("996", "974"))
    );
}

fn trace_with_frames(count: usize, changed: Option<usize>) -> String {
    let mut trace = String::from("java.lang.IllegalStateException: boom\n");
    for i in 0..count {
        let class = if changed == Some(i) {
            "Changed"
        } else {
            "Step"
        };
        trace.push_str(&format!(
            "\tat com.example.{class}{}.run(S.java:1)\n",
            name(i)
        ));
    }
    trace
}

/// Letter-only suffix so that frames differ in the symbol, not in a number.
fn name(i: usize) -> String {
    let first = (b'A' + (i / 26) as u8) as char;
    let second = (b'a' + (i % 26) as u8) as char;
    format!("{first}{second}")
}

#[test]
fn only_the_top_frames_of_a_run_count() {
    let base = fingerprint(&trace_with_frames(MAX_FRAMES_PER_RUN + 10, None));
    assert_eq!(
        base,
        fingerprint(&trace_with_frames(MAX_FRAMES_PER_RUN + 25, None)),
        "depth below the cap is ignored"
    );
    assert_eq!(
        base,
        fingerprint(&trace_with_frames(
            MAX_FRAMES_PER_RUN + 10,
            Some(MAX_FRAMES_PER_RUN)
        )),
        "first frame past the cap is ignored"
    );
    assert_ne!(
        base,
        fingerprint(&trace_with_frames(
            MAX_FRAMES_PER_RUN + 10,
            Some(MAX_FRAMES_PER_RUN - 1)
        )),
        "last frame inside the cap counts"
    );
    assert_eq!(
        normalize(&trace_with_frames(MAX_FRAMES_PER_RUN + 10, None))
            .lines()
            .count(),
        MAX_FRAMES_PER_RUN + 1
    );
}

#[test]
fn frame_order_matters() {
    let ab = "E: x\n\tat a.A.run(A.java:1)\n\tat b.B.run(B.java:1)\n";
    let ba = "E: x\n\tat b.B.run(B.java:1)\n\tat a.A.run(A.java:1)\n";
    assert_ne!(fingerprint(ab), fingerprint(ba));
}

#[test]
fn rust_location_line_is_merged_into_its_frame() {
    let text = normalize(fixture!("rust_a.txt"));
    assert!(
        text.contains(
            "<n>: lexer::parser::lexer::Lexer::next_token at ./src/parser/lexer.rs:<n>:<n>"
        )
    );
    assert!(text.contains("thread 'main' (<n>) panicked at src/parser/lexer.rs:<n>:<n>:"));
}

#[test]
fn python_source_excerpts_are_dropped() {
    let text = normalize(fixture!("python_a.txt"));
    assert!(!text.contains("handler(job)"));
    assert!(!text.contains("~~") && !text.contains('^'), "{text}");
    assert!(text.contains("File \"<tmp>/billing_plugin.py\", line <n>, in handler"));
    assert!(text.contains("KeyError: 'invoice_<n>'"));
}

#[test]
fn empty_input_normalises_to_empty() {
    assert_eq!(normalize(""), "");
    assert_eq!(normalize(" \n\t\n"), "");
}

#[test]
fn hostile_input_is_processed_in_one_pass() {
    // Shapes that make a backtracking engine explode: long runs that almost
    // match the hex, path and timestamp patterns. No timing assertion; the
    // test fails by hanging the suite if a pattern goes super-linear.
    let mut hostile = String::new();
    hostile.push_str(&"a1".repeat(50_000));
    hostile.push('\n');
    hostile.push_str(&"/tmp/".repeat(25_000));
    hostile.push('\n');
    hostile.push_str(&"12:34:".repeat(25_000));
    hostile.push('\n');
    hostile.push_str(&"3 x 0x".repeat(25_000));
    let out = normalize(&hostile);
    assert_eq!(out.lines().count(), 4);
    assert!(out.starts_with("<hex>\n"));
}
