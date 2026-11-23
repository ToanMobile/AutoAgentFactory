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

    let compile = compile_red_reason(output);
    // Check zero-tests unconditionally before behavioral patterns so a 0-test
    // crash can never be misclassified as `Proven`.
    if has_zero_tests_marker(output) && !has_nonzero_tests_ran(output) && compile.is_none() {
        return OracleOutcome::Inconclusive("0 tests ran".to_string());
    }

    match exit_code {
        Some(0) => OracleOutcome::Vacuous,
        None => OracleOutcome::Inconclusive("killed by signal".to_string()),
        Some(code) => {
            if let Some(reason) = compile {
                OracleOutcome::CompileRed(reason)
            } else if has_behavioral_failure(output) {
                OracleOutcome::Proven
            } else {
                OracleOutcome::Inconclusive(format!("unclassified non-zero exit (code {code})"))
            }
        }
    }
}

fn has_zero_tests_marker(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "running 0 tests",
        "collected 0 items",
        "no tests ran",
        "ran 0 tests",
        "no tests found",
        "no test files",
        "0 tests executed",
        "total tests: 0",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn has_nonzero_tests_ran(output: &str) -> bool {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        for prefix in ["running ", "collected ", "ran "] {
            if let Some(rest) = lower.split(prefix).nth(1)
                && starts_with_positive_int(rest)
                && (rest.contains(" test") || rest.contains(" item"))
            {
                return true;
            }
        }
        let tokens: Vec<&str> = lower.split_whitespace().collect();
        for pair in tokens.windows(2) {
            let count = pair[0].trim_matches(|c: char| !c.is_ascii_digit());
            let word = pair[1].trim_matches(|c: char| !c.is_ascii_alphabetic());
            if matches!(word, "passed" | "failed") && is_positive_int(count) {
                return true;
            }
        }
    }
    false
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

fn compile_red_reason(output: &str) -> Option<String> {
    const LITERALS: &[(&str, &str)] = &[
        ("could not compile", "rust compile error"),
        ("Compilation error", "compilation error"),
        ("Unresolved reference", "unresolved reference"),
        ("cannot find symbol", "cannot find symbol"),
        ("Cannot find module", "cannot find module"),
        ("SyntaxError", "syntax error"),
        ("IndentationError", "syntax error"),
        ("ImportError", "import error"),
        ("ModuleNotFoundError", "module not found"),
        ("[build failed]", "go build failed"),
        ("undefined: ", "undefined symbol"),
        ("error: cannot find ", "swift compile error"),
        ("error: value of type ", "swift compile error"),
    ];

    for line in output.lines() {
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
    }
    None
}

fn has_behavioral_failure(output: &str) -> bool {
    for line in output.lines() {
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        let is_rust = (t.starts_with("test ") && t.ends_with("... FAILED"))
            || line.contains("panicked at")
            || (line.contains("assertion `") && line.contains("failed"))
            || line.contains("assertion failed:")
            || line.contains("assertion left == right failed");
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
            || line.contains("--- FAIL: ");
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
