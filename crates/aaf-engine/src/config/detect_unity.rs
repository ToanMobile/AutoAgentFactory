//! What `aaf init` can know about a Unity project without starting the
//! editor: the pinned editor version and whether the project has an EditMode
//! test assembly. Both come from committed text files.

use std::fs;
use std::path::{Path, PathBuf};

use super::detect::{read_json, read_marker};

pub(super) const VERSION_FILE: &str = "ProjectSettings/ProjectVersion.txt";

/// Bounds of the search for test assemblies under `Assets/`: a game's asset
/// tree is large, and only directory listings are read on the way.
const MAX_DEPTH: usize = 16;
const MAX_ENTRIES: usize = 200_000;

/// The value of `m_EditorVersion`, when it is a plain version such as
/// `6000.6.0f1`. It becomes part of a path inside a shell command, so
/// anything else (empty, a path, shell syntax) is refused, not escaped.
pub(super) fn editor_version(project_version: &str) -> Option<&str> {
    let version = project_version
        .lines()
        .find_map(|line| line.strip_prefix("m_EditorVersion:"))?
        .trim();
    let plain = version.len() <= 32
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.');
    plain.then_some(version)
}

fn strings<'a>(asmdef: &'a serde_json::Value, key: &str) -> Vec<&'a str> {
    asmdef
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect())
        .unwrap_or_default()
}

/// An assembly definition of Unity Test Framework tests that run in the
/// editor without Play Mode: it is built for the `Editor` platform only and
/// references the test framework.
fn is_editmode_tests(asmdef: &serde_json::Value) -> bool {
    let editor_only = strings(asmdef, "includePlatforms") == ["Editor"];
    let uses_test_framework = strings(asmdef, "precompiledReferences")
        .contains(&"nunit.framework.dll")
        || strings(asmdef, "optionalUnityReferences").contains(&"TestAssemblies")
        || strings(asmdef, "defineConstraints").contains(&"UNITY_INCLUDE_TESTS")
        || strings(asmdef, "references")
            .iter()
            .any(|name| name.ends_with(".TestRunner"));
    editor_only && uses_test_framework
}

/// Repo-relative path of the first EditMode test assembly under `Assets/`,
/// in path order. `None` when there is none within the bounds.
pub(super) fn editmode_test_assembly(repo_root: &Path) -> Option<String> {
    let mut stack: Vec<(PathBuf, usize)> = vec![(PathBuf::from("Assets"), 0)];
    let mut seen = 0usize;
    while let Some((rel, depth)) = stack.pop() {
        // An unreadable directory holds no assembly this search can name.
        let Ok(entries) = fs::read_dir(repo_root.join(&rel)) else {
            continue;
        };
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Symlinks are neither: the search stays inside the project.
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(name),
                Ok(kind) if kind.is_file() => files.push(name),
                _ => {}
            }
        }
        seen += files.len() + dirs.len();
        if seen > MAX_ENTRIES {
            return None;
        }
        files.sort();
        for name in files.iter().filter(|name| name.ends_with(".asmdef")) {
            let path = rel.join(name);
            if read_json(&repo_root.join(&path)).is_some_and(|asmdef| is_editmode_tests(&asmdef)) {
                return Some(path.to_string_lossy().replace('\\', "/"));
            }
        }
        // Unity itself skips hidden folders and folders ending in `~`.
        dirs.retain(|name| !name.starts_with('.') && !name.ends_with('~'));
        dirs.sort();
        if depth < MAX_DEPTH {
            // Reversed, so the stack pops them in path order.
            stack.extend(dirs.iter().rev().map(|name| (rel.join(name), depth + 1)));
        }
    }
    None
}

/// The EditMode test run of the gate, for `sh -c` at the project root.
///
/// - The editor is the pinned version at the Unity Hub location on macOS;
///   `UNITY_PATH` names another binary (CI, Linux, a secondary Hub folder).
/// - No `-quit`: `-runTests` ends the editor itself, and `-quit` would end
///   it before the tests start.
/// - Results and log stay out of the project (a temp directory, stdout), so
///   the run leaves the work tree as it was.
/// - The editor can exit 0 without having run a test (scripts that do not
///   compile, a project the editor refuses): a results file without a
///   `<test-case` is a failure, and so is one with a failed case.
/// - The editor logs several kB of shutdown lines after the test run, more
///   than the gate keeps of a command's output, and the results file is
///   deleted: the command's last lines name the failed tests (20 at most)
///   and the counts, so the receipt shows what ran.
pub(super) fn editmode_command(version: &str) -> String {
    format!(
        "out=\"$(mktemp -d)\" || exit 1; \
         \"${{UNITY_PATH:-/Applications/Unity/Hub/Editor/{version}/Unity.app/Contents/MacOS/Unity}}\" \
         -batchmode -projectPath \"$PWD\" -runTests -testPlatform EditMode \
         -testResults \"$out/editmode.xml\" -logFile -; rc=$?; \
         cases=\"$(cat \"$out/editmode.xml\" 2>/dev/null | tr '>' '\\n' | grep '<test-case')\"; \
         if [ -z \"$cases\" ]; then \
         echo 'aaf: the editor ran no EditMode test'; [ \"$rc\" -ne 0 ] || rc=1; \
         else \
         if grep -q 'result=\"Failed\"' \"$out/editmode.xml\"; then [ \"$rc\" -ne 0 ] || rc=1; fi; \
         failed=\"$(printf '%s\\n' \"$cases\" | grep 'result=\"Failed\"')\"; \
         printf '%s\\n' \"$failed\" | sed -n 's/.* fullname=\"\\([^\"]*\\)\".*/aaf: failed: \\1/p' | head -n 20; \
         echo \"aaf: EditMode test cases: $(printf '%s\\n' \"$cases\" | grep -c .), \
         failed: $(printf '%s\\n' \"$failed\" | grep -c .)\"; fi; \
         rm -rf \"$out\"; exit \"$rc\""
    )
}

/// Test command and the evidence line that explains it.
pub(super) fn test_command(repo_root: &Path) -> (String, String) {
    let version_text = read_marker(&repo_root.join(VERSION_FILE)).unwrap_or_default();
    let Some(version) = editor_version(&version_text) else {
        return (
            String::new(),
            "m_EditorVersion is missing or not a plain version: commands left empty".to_string(),
        );
    };
    match editmode_test_assembly(repo_root) {
        Some(assembly) => (editmode_command(version), assembly),
        None => (
            String::new(),
            "no EditMode test assembly under Assets/ (an Editor-only .asmdef that references \
             the Unity Test Framework): nothing to verify, commands left empty"
                .to_string(),
        ),
    }
}
