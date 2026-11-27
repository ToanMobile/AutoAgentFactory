//! Default head-lock glob constants, extracted from `config.rs` to keep that
//! file under the 600-line repo limit.
//!
//! All path-relative globs are prefixed with `**/` because
//! `GlobBuilder::literal_separator(true)` is set in `gate/head_lock.rs`.

/// Test-file globs: newly added files matching ONLY these globs remain exempt
/// from head-lock (adding tests is always allowed). The exemption does NOT
/// apply when a newly added file also matches a non-test locked glob (for
/// example `tests/conftest.py`, `tests/jest.config.js`, or
/// `tests/.claude/settings.json`).
pub const TEST_FILE_GLOBS: &[&str] = &[
    "**/*_test.*",
    "**/*.test.*",
    "**/*.spec.*",
    "**/test_*.py",
    "**/*Test.kt",
    "**/*Test.java",
    "**/*Tests.swift",
    "**/*Tests.cs",
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/androidTest/**",
];

/// Test-runner, build-tool, toolchain, and linter configuration globs.
/// Locked for both existing and newly added files so a task cannot silently
/// alter how tests, builds, or linters execute.
pub const RUNNER_CONFIG_GLOBS: &[&str] = &[
    // Existing test-runner globs
    "**/jest.config.*",
    "**/vitest.config.*",
    "**/pytest.ini",
    "**/conftest.py",
    // Gradle runner / wrapper / version catalogs / buildSrc
    "**/gradlew",
    "**/gradlew.bat",
    "**/settings.gradle",
    "**/settings.gradle.kts",
    "**/gradle/wrapper/**",
    "**/gradle.properties",
    "**/gradle/libs.versions.toml",
    "**/buildSrc/**",
    // Rust / Cargo toolchain and linter
    "**/.cargo/config*",
    "**/rust-toolchain*",
    "**/clippy.toml",
    "**/.clippy.toml",
    "**/rustfmt.toml",
    "**/.rustfmt.toml",
    // Python packaging / runner / linter
    "**/pyproject.toml",
    "**/setup.cfg",
    "**/setup.py",
    "**/tox.ini",
    "**/ruff.toml",
    "**/.ruff.toml",
    // Make (GNU Make checks lowercase `makefile` before `Makefile` even on Linux)
    "**/Makefile",
    "**/makefile",
    "**/GNUmakefile",
    // Go
    "**/go.mod",
    "**/go.work",
    // Dart / Flutter
    "**/pubspec.yaml",
    "**/analysis_options.yaml",
    // JS / TS e2e, lint, formatter, and ignore configs
    "**/playwright.config.*",
    "**/eslint.config.*",
    "**/.eslintrc*",
    "**/.eslintignore",
    "**/.prettierignore",
    "**/.stylelintignore",
    "**/.markdownlintignore",
    "**/biome.json",
    "**/biome.jsonc",
    "**/.prettierrc*",
    "**/prettier.config.*",
    "**/stylelint.config.*",
    "**/.stylelintrc*",
    "**/oxlint.config.*",
    // Swift, Fastlane, Unity
    "**/Package.swift",
    "**/fastlane/**",
    "**/*.asmdef",
];

/// Existing CI, AAF, ADB, and build-manifest globs.
pub const CI_AND_MANIFEST_GLOBS: &[&str] = &[
    ".github/workflows/**",
    ".autofactory/config.toml",
    ".autofactory/memory.md",
    "**/.adb-denylist",
    "**/.adb-allowlist",
    "**/Cargo.toml",
    "**/package.json",
    "**/build.rs",
    "**/build.gradle",
    "**/build.gradle.kts",
];

/// What coding agents load from a repository as instructions, hooks, skills
/// and MCP servers. A task that could change them unaudited would change
/// its own guard (`.claude/settings.json` registers `aaf-hook`) and, once
/// merged, what runs in the owner's own sessions outside any sandbox.
/// Part of the default `head_lock_globs`.
pub const AGENT_CONFIG_GLOBS: [&str; 27] = [
    "**/.claude/**",
    "**/CLAUDE.md",
    "**/.mcp.json",
    "**/.agents/**",
    "**/AGENTS.md",
    "**/.agent/**",
    "**/.gemini/**",
    "**/GEMINI.md",
    "**/.codex/**",
    "**/.opencode/**",
    "**/opencode.json",
    "**/.grok/**",
    "**/GROK.md",
    "**/.cursor/**",
    "**/.cursorrules",
    "**/.autofactory/sops/**",
    "**/.autofactory/roles/**",
    "**/.husky/**",
    "**/.githooks/**",
    "**/.pre-commit-config.*",
    "**/*lefthook*.yml",
    "**/*lefthook*.yaml",
    "**/.lintstagedrc*",
    "**/lint-staged.config.*",
    "**/.npmrc",
    "**/.yarnrc*",
    "**/.pnpmfile.cjs",
];

/// Commit-hook managers' configuration: what runs on the owner's machine at
/// every commit once merged. Covered by [`AGENT_CONFIG_GLOBS`] in
/// [`default_head_lock_globs`]; retained as a named constant for callers.
pub const COMMIT_HOOK_GLOBS: &[&str] = &[
    "**/.husky/**",
    "**/.githooks/**",
    "**/.pre-commit-config.yaml",
    "**/lefthook*.yml",
    "**/lefthook*.yaml",
    "**/.lefthook*.yml",
    "**/.lefthook*.yaml",
];

/// Additional Git metadata files that alter checkout/filter/merge behavior on
/// the host.
pub const GIT_ATTR_GLOBS: [&str; 1] = ["**/.gitattributes"];

/// Build the default `gate.head_lock_globs` list from all constant groups.
pub fn default_head_lock_globs() -> Vec<String> {
    TEST_FILE_GLOBS
        .iter()
        .chain(RUNNER_CONFIG_GLOBS.iter())
        .chain(CI_AND_MANIFEST_GLOBS.iter())
        .chain(AGENT_CONFIG_GLOBS.iter())
        .chain(GIT_ATTR_GLOBS.iter())
        .map(|g| (*g).to_string())
        .collect()
}
