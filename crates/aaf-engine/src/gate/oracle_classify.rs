//! Classifies test-runner output on a reverted-production tree into
//! [`OracleOutcome::Proven`], [`OracleOutcome::Vacuous`],
//! [`OracleOutcome::CompileRed`], or [`OracleOutcome::Inconclusive`].

/// Outcome of running the test suite after reverting production changes to base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleOutcome {
    /// Non-zero exit with a recognized behavioral assertion or test failure.
    Proven,
    /// Exited 0 after running tests: the suite still passed without the prod change.
    Vacuous,
    /// Non-zero exit caused by compile, type, syntax, symbol, or import failure.
    CompileRed(String),
    /// Timed out, ran 0 tests, killed by signal, or unclassified non-zero exit.
    Inconclusive(String),
}

/// Classifies `(exit_code, timed_out, output)` across Rust, JS/TS, Python, Go,
/// Gradle/JUnit, C#/Unity, Swift, and generic shell test scripts.
pub fn classify(exit_code: Option<i32>, timed_out: bool, output: &str) -> OracleOutcome {
    if timed_out {
        return OracleOutcome::Inconclusive("timed out".to_string());
    }

    let hard_compile = hard_compile_abort_reason(output);
    let ambiguous_compile = ambiguous_runtime_error_reason(output);
    let anchored_failure = has_anchored_runner_failure(output);

    // Check zero-tests unconditionally before behavioral patterns so a 0-test
    // crash can never be misclassified as `Proven`.
    if has_zero_tests_marker(output)
        && !has_nonzero_tests_ran(output)
        && !anchored_failure
        && hard_compile.is_none()
        && ambiguous_compile.is_none()
    {
        return OracleOutcome::Inconclusive("0 tests ran".to_string());
    }

    match exit_code {
        Some(0) => OracleOutcome::Vacuous,
        None => OracleOutcome::Inconclusive("killed by signal".to_string()),
        Some(code) => {
            if let Some(reason) = hard_compile {
                OracleOutcome::CompileRed(reason)
            } else if anchored_failure {
                OracleOutcome::Proven
            } else if let Some(reason) = ambiguous_compile {
                OracleOutcome::CompileRed(reason)
            } else if has_behavioral_failure(output) {
                OracleOutcome::Proven
            } else {
                OracleOutcome::Inconclusive(format!("unclassified non-zero exit (code {code})"))
            }
        }
    }
}

/// True when `line` is a compiler diagnostic source-echo or annotation line
/// (such as `12 | let _ = ...; // assertion failed:`, `|`, `-->`, `^`, `~`,
/// `= note:`, or `= help:`), so source comments can never spoof test output.
fn is_diagnostic_source_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('|')
        || t.starts_with("-->")
        || t.starts_with('^')
        || t.starts_with('~')
        || t.starts_with("= note:")
        || t.starts_with("= help:")
    {
        return true;
    }
    let after_digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
    after_digits.len() < t.len() && after_digits.trim_start().starts_with('|')
}

fn has_zero_tests_marker(output: &str) -> bool {
    const MARKERS: &[&str] = &[
        "running 0 tests",
        "collected 0 items",
        "no tests ran",
        "ran 0 tests",
        "no tests found",
        "no test files",
        "0 tests executed",
        "total tests: 0",
    ];
    output.lines().any(|line| {
        if is_diagnostic_source_line(line) {
            return false;
        }
        let lower = line.to_ascii_lowercase();
        MARKERS.iter().any(|marker| lower.contains(marker))
    })
}

fn has_nonzero_tests_ran(output: &str) -> bool {
    for line in output.lines() {
        if is_diagnostic_source_line(line) {
            continue;
        }
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        for prefix in ["running ", "collected ", "ran ", "executed "] {
            if let Some(rest) = lower.strip_prefix(prefix)
                && starts_with_positive_int(rest)
                && (rest.contains(" test") || rest.contains(" item"))
            {
                return true;
            }
        }
        if lower.starts_with("test result:")
            && (has_positive_count_before(&lower, "passed")
                || has_positive_count_before(&lower, "failed"))
        {
            return true;
        }
        if t.starts_with('=')
            && (has_positive_count_before(&lower, "passed")
                || has_positive_count_before(&lower, "failed"))
        {
            return true;
        }
        if (lower.starts_with("tests:") || lower.starts_with("tests "))
            && (has_positive_count_before(&lower, "passed")
                || has_positive_count_before(&lower, "failed"))
        {
            return true;
        }
        if lower.starts_with("tests run:") && has_positive_count_after(&lower, "tests run:") {
            return true;
        }
        if lower.starts_with("total tests:") && has_positive_count_after(&lower, "total tests:") {
            return true;
        }
        if (lower.starts_with("failed!") || lower.starts_with("passed!"))
            && (has_positive_count_after(&lower, "passed:")
                || has_positive_count_after(&lower, "failed:"))
        {
            return true;
        }
        if starts_with_positive_int(&lower)
            && (has_positive_count_before(&lower, "passed")
                || has_positive_count_before(&lower, "failed"))
        {
            return true;
        }
    }
    false
}

/// Anchored test-runner failure summaries proving `>= 1` test actually executed
/// and failed (cannot be forged by inline comments or compiler diagnostics).
pub(super) fn has_anchored_runner_failure(output: &str) -> bool {
    for line in output.lines() {
        if is_diagnostic_source_line(line) {
            continue;
        }
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        let is_rust = (lower.starts_with("test result: failed.") || lower.starts_with("summary "))
            && has_positive_count_before(&lower, "failed");
        let is_pytest = (t.starts_with('=') && has_positive_count_before(&lower, "failed"))
            || (lower.starts_with("failed (")
                && (has_positive_count_after(&lower, "failures=")
                    || has_positive_count_after(&lower, "errors=")));
        let is_jest_vitest = (lower.starts_with("tests:") || lower.starts_with("tests "))
            && has_positive_count_before(&lower, "failed");
        let is_go = t.starts_with("--- FAIL: ");
        let is_junit_gradle = lower.starts_with("> there were failing tests")
            || (lower.starts_with("tests run:")
                && (has_positive_count_after(&lower, "failures:")
                    || has_positive_count_after(&lower, "errors:")));
        let is_swift = (lower.starts_with("executed ")
            && (has_positive_count_before(&lower, "failure")
                || has_positive_count_before(&lower, "failures")))
            || (lower.starts_with("test run with ") && lower.contains(" failed"));
        let is_dotnet = (lower.starts_with("failed!") || lower.starts_with("failed:"))
            && has_positive_count_after(&lower, "failed:");
        if is_rust
            || is_pytest
            || is_jest_vitest
            || is_go
            || is_junit_gradle
            || is_swift
            || is_dotnet
        {
            return true;
        }
    }
    false
}

fn parse_positive_token(tok: &str) -> Option<u64> {
    let clean = tok.trim_matches(|c: char| matches!(c, '(' | '[' | '=' | ',' | ';' | '|' | ':'));
    if !clean.is_empty() && clean.bytes().all(|b| b.is_ascii_digit()) {
        clean.parse::<u64>().ok().filter(|&n| n > 0)
    } else {
        None
    }
}

fn has_positive_count_before(lower: &str, keyword: &str) -> bool {
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    tokens.windows(2).any(|pair| {
        let word = pair[1].trim_matches(|c: char| !c.is_ascii_alphabetic());
        word == keyword && parse_positive_token(pair[0]).is_some()
    })
}

fn has_positive_count_after(lower: &str, prefix: &str) -> bool {
    lower.match_indices(prefix).any(|(idx, _)| {
        let rest = lower[idx + prefix.len()..].trim_start();
        starts_with_positive_int(rest)
    })
}

fn starts_with_positive_int(text: &str) -> bool {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    is_positive_int(&digits)
}

fn is_positive_int(digits: &str) -> bool {
    !digits.is_empty() && digits.parse::<u64>().is_ok_and(|n| n > 0)
}

fn has_code_prefix(line: &str, prefix: &str) -> bool {
    line.match_indices(prefix).any(|(idx, _)| {
        line[idx + prefix.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// Hard compiler/build aborts that always indicate `CompileRed` regardless of
/// surrounding output. `SyntaxError`/`IndentationError` also count here when
/// no tests ran before the error.
fn hard_compile_abort_reason(output: &str) -> Option<String> {
    const LITERALS: &[(&str, &str)] = &[
        ("could not compile", "rust compile error"),
        ("Compilation error", "compilation error"),
        ("Compilation failed", "compilation failed"),
        ("Unresolved reference", "unresolved reference"),
        ("cannot find symbol", "cannot find symbol"),
        ("[build failed]", "go build failed"),
        ("undefined: ", "undefined symbol"),
        ("error: cannot find ", "swift compile error"),
        ("error: value of type ", "swift compile error"),
    ];

    let tests_ran = has_nonzero_tests_ran(output);
    for line in output.lines() {
        if is_diagnostic_source_line(line) {
            continue;
        }
        if has_code_prefix(line, "error[E") {
            return Some("rust compiler error".to_string());
        }
        if has_code_prefix(line, "error TS") {
            return Some("typescript compile error".to_string());
        }
        if has_code_prefix(line, "error CS") {
            return Some("csharp compile error".to_string());
        }
        for (needle, label) in LITERALS {
            if line.contains(needle) {
                return Some((*label).to_string());
            }
        }
        if !tests_ran && (line.contains("SyntaxError") || line.contains("IndentationError")) {
            return Some("syntax error".to_string());
        }
    }
    None
}

/// Ambiguous runtime/import error names that indicate `CompileRed` when no
/// anchored test-runner summary proves `>= 1` test actually executed and failed.
fn ambiguous_runtime_error_reason(output: &str) -> Option<String> {
    const LITERALS: &[(&str, &str)] = &[
        ("Cannot find module", "cannot find module"),
        ("ImportError", "import error"),
        ("ModuleNotFoundError", "module not found"),
        ("ReferenceError", "reference error"),
        ("SyntaxError", "syntax error"),
        ("IndentationError", "syntax error"),
    ];
    for line in output.lines() {
        if is_diagnostic_source_line(line) {
            continue;
        }
        for (needle, label) in LITERALS {
            if line.contains(needle) {
                return Some((*label).to_string());
            }
        }
    }
    None
}

fn has_behavioral_failure(output: &str) -> bool {
    for line in output.lines() {
        if is_diagnostic_source_line(line) {
            continue;
        }
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        let is_rust = (t.starts_with("test ") && t.ends_with("... FAILED"))
            || (t.starts_with("thread '") && line.contains("panicked at"))
            || t.starts_with("panicked at")
            || (t.starts_with("assertion `") && line.contains("failed"))
            || t.starts_with("assertion failed:")
            || t.starts_with("assertion left == right failed");
        let is_py_js_go = t.starts_with("FAILED ")
            || t.starts_with("FAIL: ")
            || t.starts_with("FAIL ")
            || line.contains("AssertionError")
            || line.contains("E       assert ")
            || t.starts_with("Expected:")
            || t.starts_with("Received:")
            || t.starts_with("not ok ")
            || t.starts_with("✕ ")
            || t.starts_with("× ")
            || t.starts_with("--- FAIL: ");
        let is_assert_fail = line.contains("Assert.")
            && (lower.contains("fail")
                || lower.contains("exception")
                || lower.contains("expected"));
        let is_jvm_cs_swift_sh = line.contains("AssertionFailed")
            || line.contains("ComparisonFailure")
            || is_assert_fail
            || line.contains("Xunit.Sdk.")
            || line.contains("XCTAssert")
            || (t.starts_with("Test Case '") && t.contains(" failed"))
            || (!t.starts_with("> Task ") && line.contains(" > ") && t.ends_with(" FAILED"))
            || t == "FAIL"
            || t.starts_with("FAIL:")
            || t.starts_with("ASSERT:")
            || t.starts_with("ASSERT ")
            || line.contains("ASSERTION FAILED")
            || line.contains("Assertion failed");
        if is_rust || is_py_js_go || is_jvm_cs_swift_sh {
            return true;
        }
    }
    false
}
