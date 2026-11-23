#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_engine::executors::secret_mask::{
    ChunkMasker, MAX_HOLDBACK_BYTES, MIN_NAMED_SECRET_BYTES, NamedSecret, collect_env_secrets_from,
    mask_named_and_patterns, sanitize_secret_name,
};

#[test]
fn named_secret_replacement_and_name_sanitization() {
    let named = vec![
        NamedSecret::new("ANTHROPIC_API_KEY", "secret-alpha-12345"),
        NamedSecret::new("my-custom.db/pass", "dbpass-987654321"),
        NamedSecret::new("---", "fallback-name-secret"),
    ];
    let text = "key=secret-alpha-12345 db=dbpass-987654321 fb=fallback-name-secret";
    let masked = mask_named_and_patterns(text, &named, &[]);

    assert_eq!(
        masked,
        "key=[secret ANTHROPIC_API_KEY] db=[secret MY_CUSTOM_DB_PASS] fb=[secret SECRET]"
    );
    assert_eq!(sanitize_secret_name("github_token"), "GITHUB_TOKEN");
    assert_eq!(sanitize_secret_name("  a--b.c  "), "A_B_C");
    assert_eq!(sanitize_secret_name("!!!"), "SECRET");
}

#[test]
fn short_named_secrets_under_min_bytes_are_ignored() {
    assert_eq!(MIN_NAMED_SECRET_BYTES, 8);
    let named = vec![
        NamedSecret::new("SHORT_KEY", "1234567"), // 7 bytes -> ignored
        NamedSecret::new("EXACT_KEY", "12345678"), // 8 bytes -> masked
    ];
    let masked = mask_named_and_patterns("keep=1234567 mask=12345678", &named, &[]);
    assert_eq!(masked, "keep=1234567 mask=[secret EXACT_KEY]");
}

#[test]
fn longer_named_secrets_match_before_shorter_substrings() {
    // Deliberately pass shorter secret first to prove internal length-descending sort.
    let named = vec![
        NamedSecret::new("SHORT_TOKEN", "abcd1234"),
        NamedSecret::new("FULL_TOKEN", "abcd1234efgh5678"),
    ];
    let masked = mask_named_and_patterns("val=abcd1234efgh5678 other=abcd1234", &named, &[]);
    assert_eq!(masked, "val=[secret FULL_TOKEN] other=[secret SHORT_TOKEN]");
}

#[test]
fn unnamed_secrets_len_4_and_above_are_redacted() {
    let unnamed = vec![
        "abc".to_string(),
        "wxyz".to_string(),
        "wxyz9999".to_string(),
    ];
    let masked = mask_named_and_patterns("short=abc ok=wxyz long=wxyz9999", &[], &unnamed);
    assert_eq!(masked, "short=abc ok=[REDACTED] long=[REDACTED]");
}

#[test]
fn high_confidence_patterns_are_scrubbed() {
    let anthropic = ["sk-", "ant-", "api03-abcdefghijklmnop"].concat();
    let openai = ["sk-", "proj1234567890abcdef"].concat();
    let github_classic = ["ghp_", "0123456789abcdef0123456789"].concat();
    let github_pat = ["github_", "pat_11ABCDEF0123456789"].concat();
    let aws = ["AK", "IAIOSFODNN7EXAMPLE"].concat();
    let google = ["AI", "zaSyA1234567890abcdefXYZ"].concat();
    let slack = ["xoxr-", "1234567890-abcdef"].concat();
    let telegram = ["123456789:", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1"].concat();
    let pem = [
        "-----BEGIN RSA PRIVATE",
        " KEY-----\nMIIEowIBAAKCAQEAtest\n-----END RSA PRIVATE",
        " KEY-----",
    ]
    .concat();
    let bearer = "Authorization: Bearer super_secret_jwt_payload.abc.def";
    let url = "postgres://admin:s3cr3tpass@db.internal:5432/prod";

    for sample in [
        &anthropic,
        &openai,
        &github_classic,
        &github_pat,
        &aws,
        &google,
        &slack,
        &telegram,
        &pem,
    ] {
        let out = mask_named_and_patterns(&format!("leak={sample}!"), &[], &[]);
        assert!(
            !out.contains(sample.as_str()),
            "pattern was not scrubbed: {out}"
        );
        assert!(out.contains("[REDACTED]"), "missing marker: {out}");
    }

    let bearer_out = mask_named_and_patterns(bearer, &[], &[]);
    assert_eq!(bearer_out, "Authorization: Bearer [REDACTED]");

    let url_out = mask_named_and_patterns(url, &[], &[]);
    assert_eq!(url_out, "postgres://[REDACTED]@db.internal:5432/prod");
}

#[test]
fn bearer_does_not_clobber_already_named_secret_label() {
    let named = vec![NamedSecret::new("API_TOKEN", "my_exact_bearer_val_99")];
    let text = "Authorization: Bearer my_exact_bearer_val_99";
    let masked = mask_named_and_patterns(text, &named, &[]);
    assert_eq!(masked, "Authorization: Bearer [secret API_TOKEN]");
}

#[test]
fn collect_env_secrets_allow_and_exclude_rules() {
    let fake_env = vec![
        ("GITHUB_TOKEN", "gh_token_value_12345"),
        ("OPENAI_API_KEY", "key_value_longer_than_8"),
        ("APP_CLIENT_SECRET", "client_secret_value"),
        ("DB_PASSWORD", "super_db_password"),
        ("USER_PASSWD", "user_passwd_value"),
        ("AWS_CREDENTIAL_BLOB", "credential_blob_val"),
        ("SSH_PRIVATE_KEY", "private_key_material_here"),
        ("DATABASE_URL", "postgres://user:pass@host/db"),
        ("CUSTOM_AUTH_HEADER", "header_value_12345"),
        ("OAUTH_BEARER", "bearer_token_value_1"),
        ("SESSION_COOKIE", "session_cookie_12345"),
        // Short (< 8 bytes) -> excluded
        ("SHORT_TOKEN", "1234567"),
        // Benign exclusions -> excluded even if >= 8 bytes
        ("SSH_AUTH_SOCK", "/tmp/ssh-agent.sock.12345"),
        ("SSH_PUBLIC_KEY", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"),
        ("PUBLIC_KEY", "ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB"),
        ("KEYMAP", "us-intl-deadkeys"),
        ("X_KEYMAP", "us-dvorak-layout"),
        ("COLORTERM", "truecolor-24bit"),
        ("TERM_SESSION_ID", "w0t0p0:12345678-1234-1234"),
        ("PATH", "/usr/local/bin:/usr/bin"),
        ("HOME", "/Users/developer"),
    ];

    let secrets = collect_env_secrets_from(fake_env);
    let names: Vec<&str> = secrets.iter().map(|s| s.name.as_str()).collect();

    for expected in [
        "GITHUB_TOKEN",
        "OPENAI_API_KEY",
        "APP_CLIENT_SECRET",
        "DB_PASSWORD",
        "USER_PASSWD",
        "AWS_CREDENTIAL_BLOB",
        "SSH_PRIVATE_KEY",
        "DATABASE_URL",
        "CUSTOM_AUTH_HEADER",
        "OAUTH_BEARER",
        "SESSION_COOKIE",
    ] {
        assert!(
            names.contains(&expected),
            "missing expected secret {expected}"
        );
    }

    for excluded in [
        "SHORT_TOKEN",
        "SSH_AUTH_SOCK",
        "SSH_PUBLIC_KEY",
        "PUBLIC_KEY",
        "KEYMAP",
        "X_KEYMAP",
        "COLORTERM",
        "TERM_SESSION_ID",
        "PATH",
        "HOME",
    ] {
        assert!(
            !names.contains(&excluded),
            "benign or short var should be excluded: {excluded}"
        );
    }

    // Verify descending length order.
    for pair in secrets.windows(2) {
        assert!(
            pair[0].value.len() >= pair[1].value.len(),
            "not sorted descending: {} < {}",
            pair[0].value.len(),
            pair[1].value.len()
        );
    }
}

#[test]
fn chunk_masker_catches_secret_split_across_two_chunks() {
    let secret = "super-secret-token-2026";
    let mut masker = ChunkMasker::new(vec![NamedSecret::new("API_TOKEN", secret)], vec![]);
    assert_eq!(masker.holdback_bytes(), secret.len() - 1);

    let mut out = String::new();
    out.push_str(&masker.push("prefix before super-sec"));
    out.push_str(&masker.push("ret-token-2026 suffix after"));
    out.push_str(&masker.finish());

    assert_eq!(out, "prefix before [secret API_TOKEN] suffix after");
    assert!(!out.contains(secret));
}

#[test]
fn chunk_masker_catches_secret_split_across_three_chunks_and_every_split_point() {
    let secret = "alpha-bravo-charlie-99";
    let full = format!("start {secret} middle {secret} end");

    // 3-chunk split directly inside the secret.
    let mut masker = ChunkMasker::new(vec![NamedSecret::new("DEPLOY_KEY", secret)], vec![]);
    let mut out = String::new();
    out.push_str(&masker.push("start alpha-br"));
    out.push_str(&masker.push("avo-char"));
    out.push_str(&masker.push("lie-99 middle alpha-bravo-charlie-99 end"));
    out.push_str(&masker.finish());
    assert_eq!(
        out,
        "start [secret DEPLOY_KEY] middle [secret DEPLOY_KEY] end"
    );

    // Exhaustive single-byte chunking across the entire message.
    let mut byte_masker = ChunkMasker::new(vec![NamedSecret::new("DEPLOY_KEY", secret)], vec![]);
    let mut streamed = String::new();
    for ch in full.chars() {
        let mut buf = [0u8; 4];
        streamed.push_str(&byte_masker.push(ch.encode_utf8(&mut buf)));
    }
    streamed.push_str(&byte_masker.finish());
    assert_eq!(
        streamed,
        "start [secret DEPLOY_KEY] middle [secret DEPLOY_KEY] end"
    );
}

#[test]
fn chunk_masker_catches_pattern_secrets_split_across_chunks() {
    let tg = ["987654321:", "BBHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw9"].concat();
    let mut masker = ChunkMasker::new(vec![], vec![]);

    let mut out = String::new();
    out.push_str(&masker.push("bot="));
    out.push_str(&masker.push(&tg[..15]));
    out.push_str(&masker.push(&tg[15..]));
    out.push_str(&masker.push(" auth: Bearer "));
    out.push_str(&masker.push("jwt_part1.jwt_part2 done"));
    out.push_str(&masker.finish());

    assert_eq!(out, "bot=[REDACTED] auth: Bearer [REDACTED] done");
    assert!(!out.contains(&tg));
    assert!(!out.contains("jwt_part1"));
}

#[test]
fn chunk_masker_handles_utf8_multibyte_boundaries_safely() {
    let secret = "秘密-tiếng-Việt-🔑-secret";
    let text = format!("Xin chào 🌍 {secret} kết thúc 🚀 tiếng Việt!");

    // Split on every valid char boundary into small chunks of 1..=3 chars.
    let chars: Vec<char> = text.chars().collect();
    for chunk_chars in [1usize, 2, 3, 5] {
        let mut masker = ChunkMasker::new(vec![NamedSecret::new("UNI_SECRET", secret)], vec![]);
        let mut out = String::new();
        for slice in chars.chunks(chunk_chars) {
            let chunk: String = slice.iter().collect();
            out.push_str(&masker.push(&chunk));
        }
        out.push_str(&masker.finish());
        assert_eq!(
            out,
            "Xin chào 🌍 [secret UNI_SECRET] kết thúc 🚀 tiếng Việt!"
        );
    }
}

#[test]
fn chunk_masker_caps_holdback_at_max_holdback_bytes() {
    let huge = "x".repeat(MAX_HOLDBACK_BYTES + 200);
    let masker = ChunkMasker::new(vec![NamedSecret::new("HUGE_KEY", huge)], vec![]);
    assert_eq!(masker.holdback_bytes(), MAX_HOLDBACK_BYTES);
}
