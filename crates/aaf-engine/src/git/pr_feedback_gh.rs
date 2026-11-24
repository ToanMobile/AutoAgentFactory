//! Read-only GitHub PR feedback reader (`gh pr view`, `gh pr checks`,
//! `gh run view --log-failed`, and GraphQL `reviewThreads`), injected with
//! [`CommandRunner`] so tests never need network access.

use std::path::Path;

use serde::Deserialize;

use super::GitError;
use crate::process::CommandRunner;

/// Maximum number of failing CI jobs whose logs are fetched per PR check.
pub const MAX_FAILED_LOG_JOBS: usize = 3;
/// Hard ceiling on total CI log bytes across all fetched jobs in the final note.
pub const MAX_CI_LOG_BYTES: usize = 4096;
/// Raw log tail retained per job before ANSI stripping, secret scrubbing, and
/// final budget trimming so secrets straddling `MAX_CI_LOG_BYTES` remain intact
/// for scrubbing.
pub const MAX_RAW_LOG_TAIL_BYTES: usize = 64 * 1024;

const REVIEW_THREADS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
reviewThreads(first:50){nodes{id path isResolved isOutdated \
comments(first:20){nodes{id author{login} authorAssociation body createdAt}}}}}}}";

/// Parsed reference to a GitHub pull request URL emitted by `aaf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub number: u64,
    pub url: String,
}

impl PrRef {
    /// `--repo` argument for `gh pr view` / `gh pr checks` / `gh run view`.
    pub fn repo_flag(&self) -> String {
        if self.is_ghe() {
            format!("{}/{}/{}", self.host, self.owner, self.name)
        } else {
            format!("{}/{}", self.owner, self.name)
        }
    }

    pub fn is_ghe(&self) -> bool {
        !self.host.eq_ignore_ascii_case("github.com")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct CheckResult {
    pub name: String,
    pub state: String,
    pub bucket: String,
    pub link: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: String,
    pub author_login: String,
    pub author_association: String,
    pub body: String,
    pub created_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: Option<String>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFeedbackSnapshot {
    pub url: String,
    pub head_oid: String,
    pub is_cross_repository: bool,
    pub head_branch: String,
    pub state: String,
    pub checks: Vec<CheckResult>,
    pub failed_logs: Vec<(String, String)>,
    pub threads: Vec<ReviewThread>,
}

fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Parses `https://<host>/<owner>/<name>/pull/<n>` (trailing `/` allowed).
pub fn parse_pr_url(raw: &str) -> Option<PrRef> {
    let trimmed = raw.trim().trim_end_matches('/');
    let rest = trimmed.strip_prefix("https://")?;
    let segs: Vec<&str> = rest.split('/').collect();
    let [host, owner, name, "pull", num_str] = segs.as_slice() else {
        return None;
    };
    if !is_safe_segment(host) || !is_safe_segment(owner) || !is_safe_segment(name) {
        return None;
    }
    let number = num_str.parse::<u64>().ok().filter(|&n| n > 0)?;
    Some(PrRef {
        host: (*host).to_string(),
        owner: (*owner).to_string(),
        name: (*name).to_string(),
        number,
        url: trimmed.to_string(),
    })
}

/// Extracts `(run_id, job_id)` from a GitHub Actions check URL containing
/// `/actions/runs/<run>/job/<job>`.
pub fn parse_actions_run_job(link: &str) -> Option<(String, String)> {
    let (_, after_runs) = link.split_once("/actions/runs/")?;
    let (run_id, after_job) = after_runs.split_once("/job/")?;
    let job_id = after_job.split(['/', '?', '#']).next().unwrap_or(after_job);
    if run_id.is_empty()
        || job_id.is_empty()
        || !run_id.bytes().all(|b| b.is_ascii_digit())
        || !job_id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((run_id.to_string(), job_id.to_string()))
}

/// Parses a GitHub UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ` or with fractional
/// seconds `YYYY-MM-DDTHH:MM:SS.fffZ`) into Unix seconds. Fail-closed (`None`)
/// on any malformed input.
pub fn parse_github_time(raw: &str) -> Option<i64> {
    let body = raw.trim().strip_suffix('Z')?;
    let (date_part, time_part) = body.split_once('T')?;
    let (hms, frac) = match time_part.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time_part, None),
    };
    if let Some(f) = frac
        && (f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    if date_part.len() != 10 || hms.len() != 8 {
        return None;
    }
    let db = date_part.as_bytes();
    let tb = hms.as_bytes();
    if db[4] != b'-' || db[7] != b'-' || tb[2] != b':' || tb[5] != b':' {
        return None;
    }
    let year: i64 = date_part[0..4].parse().ok()?;
    let month: u32 = date_part[5..7].parse().ok()?;
    let day: u32 = date_part[8..10].parse().ok()?;
    let hour: u32 = hms[0..2].parse().ok()?;
    let min: u32 = hms[3..5].parse().ok()?;
    let sec: u32 = hms[6..8].parse().ok()?;
    if !(1..=12).contains(&month) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let max_day = days_in_month(year, month);
    if day < 1 || day > max_day {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec))?;
    Some(secs)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's civil-to-days algorithm relative to 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Keeps the UTF-8 tail of `s` within at most `max_bytes` bytes.
pub fn tail_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhPrView {
    url: String,
    state: String,
    head_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
}

fn run_gh_checked(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    args: &[&str],
) -> Result<String, GitError> {
    let out = runner.run("gh", args, repo_root)?;
    if out.success() {
        return Ok(out.stdout);
    }
    let stderr = out.stderr.trim();
    let stdout = out.stdout.trim();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    Err(GitError::Command {
        command: format!("gh {}", args.join(" ")),
        code: out.exit_code,
        stderr: detail.to_string(),
    })
}

fn fetch_checks(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr_num: &str,
    repo_flag: &str,
) -> Result<Vec<CheckResult>, GitError> {
    let args = [
        "pr",
        "checks",
        pr_num,
        "--repo",
        repo_flag,
        "--json",
        "name,state,bucket,link",
    ];
    let out = runner.run("gh", &args, repo_root)?;
    let stdout = out.stdout.trim();
    let stderr = out.stderr.trim();
    // D3: `gh pr checks` exits 1 when no checks exist or when a check failed,
    // and exits 8 when checks are pending. Parse JSON whenever stdout is non-empty.
    if stdout.is_empty() && stderr.to_ascii_lowercase().contains("no checks reported") {
        return Ok(Vec::new());
    }
    if stdout.is_empty() {
        return Err(GitError::Command {
            command: format!("gh pr checks {pr_num}"),
            code: out.exit_code,
            stderr: stderr.to_string(),
        });
    }
    serde_json::from_str(stdout)
        .map_err(|e| GitError::Invalid(format!("unparsable gh pr checks JSON: {e}")))
}

fn fetch_failed_logs(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    repo_flag: &str,
    checks: &[CheckResult],
) -> Vec<(String, String)> {
    let failing: Vec<(&str, String, String)> = checks
        .iter()
        .filter(|c| {
            c.bucket.eq_ignore_ascii_case("fail") || c.bucket.eq_ignore_ascii_case("cancel")
        })
        .filter_map(|c| {
            let (run_id, job_id) = parse_actions_run_job(&c.link)?;
            Some((c.name.as_str(), run_id, job_id))
        })
        .take(MAX_FAILED_LOG_JOBS)
        .collect();
    if failing.is_empty() {
        return Vec::new();
    }
    failing
        .into_iter()
        .map(|(name, run_id, job_id)| {
            let args = [
                "run",
                "view",
                run_id.as_str(),
                "--repo",
                repo_flag,
                "--job",
                job_id.as_str(),
                "--log-failed",
            ];
            // D10: non-UTF-8 or command failure for one job log must not fail the whole snapshot.
            let text = match runner.run("gh", &args, repo_root) {
                Ok(out) if out.success() => {
                    if out.stdout.len() > MAX_RAW_LOG_TAIL_BYTES {
                        let tail = tail_utf8_bytes(&out.stdout, MAX_RAW_LOG_TAIL_BYTES);
                        match tail.find('\n') {
                            Some(nl) => tail[nl + 1..].to_string(),
                            None => tail.to_string(),
                        }
                    } else {
                        out.stdout
                    }
                }
                Ok(_) | Err(_) => "(log unavailable)".to_string(),
            };
            (name.to_string(), text)
        })
        .collect()
}

fn fetch_threads(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr: &PrRef,
) -> Result<Vec<ReviewThread>, GitError> {
    let query_arg = format!("query={REVIEW_THREADS_QUERY}");
    let owner_arg = format!("owner={}", pr.owner);
    let name_arg = format!("name={}", pr.name);
    let number_arg = format!("number={}", pr.number);
    let mut args = vec!["api", "graphql"];
    if pr.is_ghe() {
        args.push("--hostname");
        args.push(&pr.host);
    }
    args.extend([
        "-f",
        &query_arg,
        "-f",
        &owner_arg,
        "-f",
        &name_arg,
        "-F",
        &number_arg,
    ]);
    let raw = run_gh_checked(runner, repo_root, &args)?;
    let parsed: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| GitError::Invalid(format!("unparsable gh api graphql JSON: {e}")))?;
    let nodes = parsed["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    Ok(nodes
        .iter()
        .map(|node| {
            let comments = node["comments"]["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .map(|c| ThreadComment {
                    id: c["id"].as_str().unwrap_or("").to_string(),
                    author_login: c["author"]["login"].as_str().unwrap_or("").to_string(),
                    author_association: c["authorAssociation"].as_str().unwrap_or("").to_string(),
                    body: c["body"].as_str().unwrap_or("").to_string(),
                    created_at_unix: c["createdAt"].as_str().and_then(parse_github_time),
                })
                .collect();
            ReviewThread {
                id: node["id"].as_str().unwrap_or("").to_string(),
                path: node["path"].as_str().map(str::to_string),
                is_resolved: node["isResolved"].as_bool().unwrap_or(false),
                is_outdated: node["isOutdated"].as_bool().unwrap_or(false),
                comments,
            }
        })
        .collect())
}

/// Fetches a read-only PR feedback snapshot via `gh`.
pub fn fetch_pr_feedback(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr: &PrRef,
    expected_head_branch: &str,
) -> Result<PrFeedbackSnapshot, GitError> {
    let pr_num = pr.number.to_string();
    let repo_flag = pr.repo_flag();
    let view_raw = run_gh_checked(
        runner,
        repo_root,
        &[
            "pr",
            "view",
            &pr_num,
            "--repo",
            &repo_flag,
            "--json",
            "number,url,state,headRefName,headRefOid,isCrossRepository",
        ],
    )?;
    let view: GhPrView = serde_json::from_str(view_raw.trim())
        .map_err(|e| GitError::Invalid(format!("unparsable gh pr view JSON: {e}")))?;

    let url_matches = view.url.trim().trim_end_matches('/') == pr.url.trim().trim_end_matches('/');
    let should_fetch_details = view.state.eq_ignore_ascii_case("OPEN")
        && !view.is_cross_repository
        && view.head_ref_name == expected_head_branch
        && url_matches;

    let (checks, failed_logs, threads) = if should_fetch_details {
        let checks = fetch_checks(runner, repo_root, &pr_num, &repo_flag)?;
        let failed_logs = fetch_failed_logs(runner, repo_root, &repo_flag, &checks);
        let threads = fetch_threads(runner, repo_root, pr)?;
        (checks, failed_logs, threads)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(PrFeedbackSnapshot {
        url: view.url,
        head_oid: view.head_ref_oid,
        is_cross_repository: view.is_cross_repository,
        head_branch: view.head_ref_name,
        state: view.state,
        checks,
        failed_logs,
        threads,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::Mutex;

    use super::*;
    use crate::process::CommandOutput;

    struct FakeRunner {
        responses: Mutex<VecDeque<io::Result<CommandOutput>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<io::Result<CommandOutput>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().map(|s| (*s).to_string()));
            self.calls.lock().unwrap().push(call);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected command call")
        }
    }

    fn ok(stdout: &str) -> io::Result<CommandOutput> {
        out(0, stdout, "")
    }

    fn out(code: i32, stdout: &str, stderr: &str) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            exit_code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    #[test]
    fn parse_pr_url_handles_github_and_ghe() {
        let gh = parse_pr_url("https://github.com/acme/app/pull/42/").unwrap();
        assert_eq!(gh.repo_flag(), "acme/app");
        assert_eq!(gh.url, "https://github.com/acme/app/pull/42");
        assert!(gh.number == 42 && !gh.is_ghe());

        let ghe = parse_pr_url("https://ghe.corp.example/org/svc/pull/7").unwrap();
        assert_eq!(ghe.repo_flag(), "ghe.corp.example/org/svc");
        assert!(ghe.number == 7 && ghe.is_ghe());

        for bad in [
            "http://github.com/acme/app/pull/1",
            "https://github.com/acme/app/issues/1",
            "https://github.com/-bad/app/pull/1",
            "https://github.com/acme/app/pull/0",
        ] {
            assert!(parse_pr_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parse_actions_run_job_extracts_ids() {
        let url = "https://github.com/acme/app/actions/runs/123456789/job/987654321?pr=42";
        assert_eq!(
            parse_actions_run_job(url),
            Some(("123456789".into(), "987654321".into()))
        );
        assert_eq!(parse_actions_run_job("https://ci.example/build/99"), None);
    }

    #[test]
    fn parse_github_time_handles_epoch_leap_year_and_invalid() {
        assert_eq!(parse_github_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_github_time("2024-02-29T12:30:45Z"),
            Some(1_709_209_845)
        );
        assert_eq!(
            parse_github_time("2024-02-29T12:30:45.123456Z"),
            Some(1_709_209_845)
        );
        for bad in [
            "2023-02-29T00:00:00Z",
            "2026-11-25T03:04:05+07:00",
            "not-a-timestamp",
            "2026-13-01T00:00:00Z",
        ] {
            assert_eq!(parse_github_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn fetch_parses_view_checks_exit1_logs_capped_and_graphql_for_ghe() {
        let pr = parse_pr_url("https://ghe.corp.example/acme/app/pull/12").unwrap();
        let huge_log = format!(
            "{}\n{}",
            "a".repeat(MAX_RAW_LOG_TAIL_BYTES + 2048),
            "TAIL_MARKER_€"
        );
        let view_json = r#"{"number":12,"url":"https://ghe.corp.example/acme/app/pull/12","state":"OPEN","headRefName":"aaf/t1","headRefOid":"abc123","isCrossRepository":false}"#;
        let checks_json = r#"[{"name":"ci/test","state":"FAILURE","bucket":"fail","link":"https://ghe.corp.example/acme/app/actions/runs/111/job/222"},{"name":"ci/lint","state":"CANCELLED","bucket":"cancel","link":"https://ghe.corp.example/acme/app/actions/runs/111/job/333"}]"#;
        let gql_json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"T1","path":"src/lib.rs","isResolved":false,"isOutdated":false,"comments":{"nodes":[{"id":"C1","author":{"login":"owner1"},"authorAssociation":"OWNER","body":"Please fix","createdAt":"2026-01-02T03:04:05Z"}]}}]}}}}}"#;
        let runner = FakeRunner::new(vec![
            ok(view_json),
            out(1, checks_json, ""),
            ok(&huge_log),
            Err(io::Error::new(io::ErrorKind::InvalidData, "non-utf8")),
            ok(gql_json),
        ]);

        let snap = fetch_pr_feedback(&runner, Path::new("/tmp"), &pr, "aaf/t1").unwrap();
        assert_eq!(snap.head_oid, "abc123");
        assert!(snap.checks.len() == 2 && snap.failed_logs.len() == 2);
        assert!(snap.failed_logs[0].1.len() <= MAX_RAW_LOG_TAIL_BYTES);
        assert!(snap.failed_logs[0].1.ends_with("TAIL_MARKER_€"));
        assert_eq!(snap.failed_logs[1].1, "(log unavailable)");
        assert_eq!(snap.threads[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(snap.threads[0].comments[0].author_login, "owner1");
        assert!(snap.threads[0].comments[0].created_at_unix.is_some());

        let calls = runner.calls();
        assert_eq!(calls.len(), 5);
        assert!(calls[0].iter().any(|a| a == "ghe.corp.example/acme/app"));
        for pair in [
            ["--hostname", "ghe.corp.example"],
            ["-f", "owner=acme"],
            ["-f", "name=app"],
            ["-F", "number=12"],
        ] {
            assert!(calls[4].windows(2).any(|w| w == pair), "missing {pair:?}");
        }
    }

    #[test]
    fn checks_exit1_no_checks_reported_is_empty_ok() {
        let pr = parse_pr_url("https://github.com/acme/app/pull/5").unwrap();
        let view_json = r#"{"number":5,"url":"https://github.com/acme/app/pull/5","state":"OPEN","headRefName":"aaf/t1","headRefOid":"def456","isCrossRepository":false}"#;
        let gql_json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}"#;
        let runner = FakeRunner::new(vec![
            ok(view_json),
            out(1, "", "no checks reported on the 'aaf/t1' branch\n"),
            ok(gql_json),
        ]);
        let snap = fetch_pr_feedback(&runner, Path::new("/tmp"), &pr, "aaf/t1").unwrap();
        assert!(snap.checks.is_empty() && snap.failed_logs.is_empty() && snap.threads.is_empty());
    }
}
