//! Gradle, `package.json`, and `pyproject.toml` command/marker helpers for
//! [`super`].

use std::path::Path;

use aaf_domain::config::CommandsConfig;

use super::{commands, read_marker};

/// Strips Groovy / Kotlin DSL comments (`// ...` and nested `/* ... */`) while
/// preserving string literals so URLs like `"https://..."` are not mistaken for
/// comment starts.
fn strip_gradle_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                // Triple-quoted string (`"""..."""`).
                if chars.peek() == Some(&'"') {
                    let mut clone = chars.clone();
                    let _ = clone.next();
                    if clone.peek() == Some(&'"') {
                        out.push(chars.next().unwrap_or('"'));
                        out.push(chars.next().unwrap_or('"'));
                        let mut quotes = 0usize;
                        for ch in chars.by_ref() {
                            out.push(ch);
                            if ch == '"' {
                                quotes += 1;
                                if quotes == 3 {
                                    break;
                                }
                            } else {
                                quotes = 0;
                            }
                        }
                        continue;
                    }
                }
                while let Some(ch) = chars.next() {
                    out.push(ch);
                    if ch == '\\' {
                        if let Some(esc) = chars.next() {
                            out.push(esc);
                        }
                    } else if ch == '"' || ch == '\n' {
                        break;
                    }
                }
            }
            '\'' => {
                out.push('\'');
                while let Some(ch) = chars.next() {
                    out.push(ch);
                    if ch == '\\' {
                        if let Some(esc) = chars.next() {
                            out.push(esc);
                        }
                    } else if ch == '\'' || ch == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                out.push(' ');
                let mut depth = 1usize;
                while depth > 0 {
                    match chars.next() {
                        Some('/') if chars.peek() == Some(&'*') => {
                            let _ = chars.next();
                            depth += 1;
                        }
                        Some('*') if chars.peek() == Some(&'/') => {
                            let _ = chars.next();
                            depth -= 1;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The first module build file (root or a direct child directory) that
/// declares `productFlavors` outside of comments.
pub(super) fn product_flavors_file(root: &Path, children: &[String]) -> Option<String> {
    let modules = std::iter::once(String::new()).chain(
        children
            .iter()
            .filter(|name| !name.starts_with('.'))
            .map(|name| format!("{name}/")),
    );
    modules
        .flat_map(|module| {
            ["build.gradle.kts", "build.gradle"]
                .into_iter()
                .map(move |file| format!("{module}{file}"))
        })
        .find(|rel| {
            read_marker(&root.join(rel))
                .is_some_and(|text| strip_gradle_comments(&text).contains("productFlavors"))
        })
}

pub(super) fn android_gradle_commands(prefix: &str) -> CommandsConfig {
    let run = |tasks: &str| format!("{prefix}./gradlew {tasks} --no-daemon");
    CommandsConfig {
        build: run("assembleDebug"),
        test_scoped: run("testDebugUnitTest"),
        test_full: run("testDebugUnitTest"),
        // Keep scoped gates fast: `./gradlew lint` is project-wide and belongs in `lint_full`.
        lint: String::new(),
        lint_full: run("lint"),
        repro: String::new(),
    }
}

fn is_placeholder_npm_test(script: &str) -> bool {
    let trimmed = script.trim();
    trimmed.is_empty() || trimmed.to_ascii_lowercase().contains("no test specified")
}

/// True when the test script asks for a watcher that never exits on its own
/// (`jest --watch`, `jest --watchAll` as in the Expo template, `vitest
/// watch`): as a gate command it would hang until the timeout instead of
/// passing or failing. A bare `vitest` is fine: gate commands have no TTY,
/// so it runs once.
fn is_watch_mode_test(script: &str) -> bool {
    let tokens: Vec<&str> = script.split_whitespace().collect();
    let watch_flag = tokens.iter().any(|t| {
        let flag = t.split('=').next().unwrap_or(t);
        matches!(flag, "--watch" | "--watchAll") && !t.ends_with("=false")
    });
    let vitest_watch = tokens
        .windows(2)
        .any(|w| (w[0] == "vitest" || w[0].ends_with("/vitest")) && w[1] == "watch");
    watch_flag || vitest_watch
}

/// A one-shot replacement for a watch-mode test script, or empty (the gate
/// then stays Unverified until someone sets `commands.test_*`).
fn one_shot_test(package: &serde_json::Value, script: &str) -> String {
    if depends_on(package, "vitest") || script.contains("vitest") {
        "npx vitest run".to_string()
    } else if depends_on(package, "jest")
        || depends_on(package, "jest-expo")
        || script.contains("jest")
    {
        "npx jest --ci".to_string()
    } else {
        String::new()
    }
}

pub(super) fn package_json_commands(
    repo_root: &Path,
    package: &serde_json::Value,
) -> CommandsConfig {
    let empty = serde_json::Map::new();
    let scripts = package
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(&empty);
    let runner = if repo_root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if repo_root.join("yarn.lock").is_file() {
        "yarn"
    } else if repo_root.join("bun.lockb").is_file() || repo_root.join("bun.lock").is_file() {
        "bun run"
    } else {
        "npm run"
    };
    let script_val = |name: &str| {
        scripts
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let run = |name: &str| format!("{runner} {name}");

    let test = match script_val("test") {
        Some(s) if is_watch_mode_test(s) => one_shot_test(package, s),
        Some(s) if !is_placeholder_npm_test(s) => run("test"),
        _ => String::new(),
    };
    // `npx tsc` only when TypeScript is a declared dependency: npx must not
    // download a compiler from the network inside a gate.
    let lint = if script_val("lint").is_some() {
        run("lint")
    } else if script_val("typecheck").is_some() {
        run("typecheck")
    } else if repo_root.join("tsconfig.json").is_file() && depends_on(package, "typescript") {
        "npx tsc --noEmit".to_string()
    } else {
        String::new()
    };
    let build = if script_val("build").is_some() {
        run("build")
    } else {
        String::new()
    };
    commands(&build, &test, &test, &lint)
}

fn dep_spec_is_pytest(spec: &str) -> bool {
    spec.trim()
        .split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.'))
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("pytest"))
}

fn array_has_pytest(val: &toml::Value) -> bool {
    val.as_array().is_some_and(|items| {
        items
            .iter()
            .filter_map(toml::Value::as_str)
            .any(dep_spec_is_pytest)
    })
}

fn table_has_pytest_key(val: &toml::Value) -> bool {
    val.as_table()
        .is_some_and(|tbl| tbl.keys().any(|k| k.eq_ignore_ascii_case("pytest")))
}

fn pyproject_has_pytest_dep(doc: &toml::Value) -> bool {
    if let Some(project) = doc.get("project") {
        if project.get("dependencies").is_some_and(array_has_pytest) {
            return true;
        }
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(toml::Value::as_table)
            && opt.values().any(array_has_pytest)
        {
            return true;
        }
    }
    if let Some(groups) = doc.get("dependency-groups").and_then(toml::Value::as_table)
        && groups.values().any(array_has_pytest)
    {
        return true;
    }
    if let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) {
        if poetry.get("dependencies").is_some_and(table_has_pytest_key)
            || poetry
                .get("dev-dependencies")
                .is_some_and(table_has_pytest_key)
        {
            return true;
        }
        if let Some(groups) = poetry.get("group").and_then(toml::Value::as_table)
            && groups
                .values()
                .any(|g| g.get("dependencies").is_some_and(table_has_pytest_key))
        {
            return true;
        }
    }
    false
}

/// `uv run `, `poetry run ` or `pdm run ` when the project's lock file says
/// which environment manager owns it, so tools run inside its virtualenv.
fn python_runner(repo_root: &Path) -> &'static str {
    if repo_root.join("uv.lock").is_file() {
        "uv run "
    } else if repo_root.join("poetry.lock").is_file() {
        "poetry run "
    } else if repo_root.join("pdm.lock").is_file() {
        "pdm run "
    } else {
        ""
    }
}

pub(super) fn pyproject_commands(repo_root: &Path) -> CommandsConfig {
    let text = read_marker(&repo_root.join("pyproject.toml")).unwrap_or_default();
    let mut has_pytest = repo_root.join("tests").is_dir();
    let mut has_ruff =
        repo_root.join("ruff.toml").is_file() || repo_root.join(".ruff.toml").is_file();
    let mut has_mypy = repo_root.join("mypy.ini").is_file();
    if let Ok(doc) = toml::from_str::<toml::Value>(&text) {
        if let Some(tool) = doc.get("tool") {
            if tool.get("pytest").is_some() {
                has_pytest = true;
            }
            if tool.get("ruff").is_some() {
                has_ruff = true;
            }
            if tool.get("mypy").is_some() {
                has_mypy = true;
            }
        }
        if !has_pytest && pyproject_has_pytest_dep(&doc) {
            has_pytest = true;
        }
    } else {
        for line in text.lines().map(str::trim) {
            if line.starts_with("[tool.pytest") {
                has_pytest = true;
            }
            if line.starts_with("[tool.ruff") {
                has_ruff = true;
            }
            if line.starts_with("[tool.mypy") {
                has_mypy = true;
            }
        }
    }
    let runner = python_runner(repo_root);
    let tool = |on: bool, cmd: &str| {
        if on {
            format!("{runner}{cmd}")
        } else {
            String::new()
        }
    };
    let test = tool(has_pytest, "pytest");
    let mut cmds = commands("", &test, &test, &tool(has_ruff, "ruff check ."));
    // Project-wide type checking is slow: full gates only.
    cmds.lint_full = tool(has_mypy, "mypy .");
    cmds
}

pub(super) fn depends_on(package: &serde_json::Value, dependency: &str) -> bool {
    ["dependencies", "devDependencies", "peerDependencies"]
        .iter()
        .any(|section| {
            package
                .get(section)
                .and_then(|deps| deps.get(dependency))
                .is_some()
        })
}
