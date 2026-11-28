//! Path-classification rules for [`super::conformance`]: control-plane/runner
//! globs (including symlink directory prefixes), allowlisted lockfiles, dedicated
//! test directories, CamelCase security-token detection, and invisible/control-char guards.

use std::sync::OnceLock;

use aaf_domain::config::{AGENT_CONFIG_GLOBS, RUNNER_CONFIG_GLOBS, TEST_FILE_GLOBS};
use aaf_domain::prompt_fence::normalize_repo_path;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::gate::changes::symlink_dir_prefixes;

const CONTROL_PLANE_GLOBS: &[&str] = &[
    "**/.github/workflows/**",
    "**/.autofactory/config.toml",
    "**/.autofactory/memory.md",
    "**/.adb-denylist",
    "**/.adb-allowlist",
    "**/.gitattributes",
];

const SOFT_MANIFEST_GLOBS: &[&str] = &[
    "**/Cargo.toml",
    "**/package.json",
    "**/build.gradle",
    "**/build.gradle.kts",
    "**/pubspec.yaml",
    "**/go.mod",
    "**/Package.swift",
];

const ALLOWLISTED_LOCKFILES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "npm-shrinkwrap.json",
    "poetry.lock",
    "uv.lock",
    "flake.lock",
    "go.sum",
    "gradle.lockfile",
    "package.resolved",
    "gemfile.lock",
    "podfile.lock",
    "composer.lock",
    "pubspec.lock",
];

const ROOT_GENERATED_DIRS: &[&str] = &["target", "node_modules", ".gradle", "pods"];

const DEDICATED_TEST_DIRS: &[&str] = &[
    "tests",
    "test",
    "__tests__",
    "androidtest",
    "integrationtest",
    "unit_tests",
    "e2e",
];

const HARD_SECURITY_TOKENS: &[&str] = &[
    "auth",
    "auth0",
    "authn",
    "authz",
    "authentication",
    "authenticator",
    "authorization",
    "authorizer",
    "oauth",
    "oauth2",
    "oidc",
    "jwt",
    "token",
    "tokens",
    "credential",
    "credentials",
    "password",
    "passwords",
    "passwd",
    "passphrase",
    "secret",
    "secrets",
    "crypto",
    "cipher",
    "encryption",
    "encryptor",
    "tls",
    "ssl",
    "x509",
    "hmac",
    "pkcs",
    "sandbox",
    "seatbelt",
    "bwrap",
    "seccomp",
    "permission",
    "permissions",
    "entitlement",
    "entitlements",
    "rbac",
    "acl",
    "csrf",
    "keychain",
    "keystore",
    "signing",
    "signature",
    "signer",
    "verifier",
];

fn is_soft_manifest(glob: &str) -> bool {
    SOFT_MANIFEST_GLOBS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(glob))
}

fn build_globset_with_dir_prefixes(globs: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for g in globs
        .iter()
        .map(String::as_str)
        .chain(symlink_dir_prefixes(globs).iter().map(String::as_str))
    {
        let compiled = GlobBuilder::new(g)
            .case_insensitive(true)
            .literal_separator(true)
            .build()
            .ok()?;
        builder.add(compiled);
    }
    builder.build().ok()
}

pub(super) fn control_plane_globset() -> Option<&'static GlobSet> {
    static SET: OnceLock<Option<GlobSet>> = OnceLock::new();
    SET.get_or_init(|| {
        let globs: Vec<String> = AGENT_CONFIG_GLOBS
            .iter()
            .copied()
            .chain(CONTROL_PLANE_GLOBS.iter().copied())
            .chain(
                RUNNER_CONFIG_GLOBS
                    .iter()
                    .copied()
                    .filter(|g| !is_soft_manifest(g)),
            )
            .map(str::to_string)
            .collect();
        build_globset_with_dir_prefixes(&globs)
    })
    .as_ref()
}

pub(super) fn custom_hard_lock_globset(head_lock_globs: &[String]) -> Result<Option<GlobSet>, ()> {
    let globs: Vec<String> = head_lock_globs
        .iter()
        .map(|g| g.trim())
        .filter(|g| {
            !g.is_empty()
                && !TEST_FILE_GLOBS.iter().any(|t| t.eq_ignore_ascii_case(g))
                && !is_soft_manifest(g)
                && !AGENT_CONFIG_GLOBS.iter().any(|a| a.eq_ignore_ascii_case(g))
                && !CONTROL_PLANE_GLOBS
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(g))
                && !RUNNER_CONFIG_GLOBS
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(g))
        })
        .map(str::to_string)
        .collect();
    if globs.is_empty() {
        return Ok(None);
    }
    build_globset_with_dir_prefixes(&globs).map(Some).ok_or(())
}

pub(super) fn is_invisible_or_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

pub(super) fn has_invisible_or_control_char(s: &str) -> bool {
    s.chars().any(is_invisible_or_control)
}

pub(super) fn is_allowlisted_lockfile_or_root_generated(norm_path: &str) -> bool {
    let lower = norm_path.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    if ALLOWLISTED_LOCKFILES.contains(&file_name) {
        return true;
    }
    lower
        .split_once('/')
        .is_some_and(|(first, _)| ROOT_GENERATED_DIRS.contains(&first))
}

pub(super) fn is_in_dedicated_test_dir(norm_path: &str) -> bool {
    let mut parts: Vec<&str> = norm_path.split('/').collect();
    parts.pop();
    parts
        .into_iter()
        .any(|dir| DEDICATED_TEST_DIRS.contains(&dir.to_ascii_lowercase().as_str()))
}

pub(super) fn path_in_scope(norm_path: &str, scope_paths: &[String]) -> bool {
    if scope_paths.is_empty() {
        return true;
    }
    let path_lower = norm_path.to_ascii_lowercase();
    scope_paths.iter().any(|raw| {
        let trimmed = raw.trim();
        if matches!(trimmed, "." | "./") {
            return true;
        }
        let Ok(scope) = normalize_repo_path(trimmed) else {
            return false;
        };
        let scope_lower = scope.to_ascii_lowercase();
        path_lower == scope_lower
            || path_lower
                .strip_prefix(&scope_lower)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn split_identifier_tokens(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for piece in segment.split(['_', '-', '.']) {
        if piece.is_empty() {
            continue;
        }
        let chars: Vec<char> = piece.chars().collect();
        let mut start = 0usize;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let curr = chars[i];
            let next = chars.get(i + 1).copied();
            let boundary = ((prev.is_ascii_lowercase() || prev.is_ascii_digit())
                && curr.is_ascii_uppercase())
                || (prev.is_ascii_uppercase()
                    && curr.is_ascii_uppercase()
                    && next.is_some_and(|n| n.is_ascii_lowercase()));
            if boundary {
                tokens.push(
                    chars[start..i]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase(),
                );
                start = i;
            }
        }
        tokens.push(
            chars[start..]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase(),
        );
    }
    tokens
}

fn segment_has_security_token(segment: &str) -> bool {
    let toks = split_identifier_tokens(segment);
    if toks
        .iter()
        .any(|t| HARD_SECURITY_TOKENS.contains(&t.as_str()))
    {
        return true;
    }
    toks.windows(2)
        .any(|w| w[0] == "o" && matches!(w[1].as_str(), "auth" | "auth2"))
}

pub(super) fn is_hard_security_path(norm_path: &str, only_filename: bool) -> bool {
    if only_filename {
        let name = norm_path.rsplit('/').next().unwrap_or(norm_path);
        return segment_has_security_token(name);
    }
    norm_path.split('/').any(segment_has_security_token)
}
