//! URL to route mapping of the Web UI server.

use super::percent_decode;

pub(super) enum Route {
    Index,
    /// The all-in-one metrics page.
    Studio,
    Static(super::pwa::StaticAsset),
    Report,
    Doctor,
    Health,
    Meta,
    Status,
    Accounts,
    Board,
    SopRuns,
    TaskDetail(String),
    TaskDiff(String),
    TaskPipeline(String),
    TaskProof(String),
    ProofImage(String, usize),
    Handover(String),
    ApprovePreview(String),
    StreamTail(String),
    Events,
    CreateTask,
    ApproveCommit,
    Action,
    MethodNotAllowed,
    BadRequest(&'static str),
    NotFound,
}

pub(super) fn match_route(method: &str, path: &str) -> Route {
    use super::pwa::StaticAsset;
    let exact = |want: &str, r: Route| {
        if method == want {
            r
        } else {
            Route::MethodNotAllowed
        }
    };
    match path {
        "/" => return exact("GET", Route::Index),
        "/health" | "/studio" => return exact("GET", Route::Studio),
        "/manifest.webmanifest" => return exact("GET", Route::Static(StaticAsset::Manifest)),
        "/apple-touch-icon.png" => return exact("GET", Route::Static(StaticAsset::AppleTouchIcon)),
        "/icon.svg" => return exact("GET", Route::Static(StaticAsset::IconSvg)),
        "/assets/diff.js" => return exact("GET", Route::Static(StaticAsset::DiffJs)),
        "/assets/pipeline.js" => return exact("GET", Route::Static(StaticAsset::PipelineJs)),
        "/assets/i18n.js" => return exact("GET", Route::Static(StaticAsset::I18nJs)),
        "/api/report" => return exact("GET", Route::Report),
        "/api/doctor" => return exact("GET", Route::Doctor),
        "/api/health" => return exact("GET", Route::Health),
        "/api/meta" => return exact("GET", Route::Meta),
        "/api/status" => return exact("GET", Route::Status),
        "/api/accounts" => return exact("GET", Route::Accounts),
        "/api/board" => return exact("GET", Route::Board),
        "/api/sop/runs" => return exact("GET", Route::SopRuns),
        "/api/events" => return exact("GET", Route::Events),
        "/api/tasks" => return exact("POST", Route::CreateTask),
        "/api/approve" => return exact("POST", Route::ApproveCommit),
        "/api/action" => return exact("POST", Route::Action),
        _ => {}
    }
    if let Some(rest) = path.strip_prefix("/api/proof-image/")
        && let Some((raw_id, raw_idx)) = rest.split_once('/')
        && !raw_id.is_empty()
        && !raw_idx.is_empty()
        && !raw_idx.contains('/')
    {
        if method != "GET" {
            return Route::MethodNotAllowed;
        }
        let Some(id) = percent_decode(raw_id).filter(|s| !s.is_empty()) else {
            return Route::BadRequest("invalid URL percent encoding");
        };
        let Ok(idx) = raw_idx.parse::<usize>() else {
            return Route::BadRequest("invalid proof image index");
        };
        return Route::ProofImage(id, idx);
    }
    type RouteCtor = fn(String) -> Route;
    let prefixes: [(&str, RouteCtor); 7] = [
        ("/api/task/", Route::TaskDetail),
        ("/api/diff/", Route::TaskDiff),
        ("/api/pipeline/", Route::TaskPipeline),
        ("/api/proof/", Route::TaskProof),
        ("/api/handover/", Route::Handover),
        ("/api/approve/preview/", Route::ApprovePreview),
        ("/api/streams/", Route::StreamTail),
    ];
    for (prefix, build) in prefixes {
        if let Some(raw_id) = path.strip_prefix(prefix)
            && !raw_id.is_empty()
            && !raw_id.contains('/')
        {
            if method != "GET" {
                return Route::MethodNotAllowed;
            }
            return match percent_decode(raw_id) {
                Some(id) if !id.is_empty() => build(id),
                _ => Route::BadRequest("invalid URL percent encoding"),
            };
        }
    }
    Route::NotFound
}

pub(super) fn normalize_action(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ready" => Some("ready"),
        "ready_all" | "ready-all" => Some("ready_all"),
        "cancel" => Some("cancel"),
        "unblock" => Some("unblock"),
        "rework" => Some("rework"),
        "steer" => Some("steer"),
        "unfreeze" => Some("unfreeze"),
        "approve_plan" | "approve-plan" => Some("approve_plan"),
        "unblock_all" | "unblock-all" => Some("unblock_all"),
        "delete_done" | "delete-done" => Some("delete_done"),
        "stop_run" | "stop-run" => Some("stop_run"),
        "supervisor_toggle" | "supervisor-toggle" => Some("supervisor_toggle"),
        "supervisor_repos" | "supervisor-repos" => Some("supervisor_repos"),
        "account_cooldown" | "account-cooldown" => Some("account_cooldown"),
        "account_clear" | "account-clear" => Some("account_clear"),
        "account_clear_all" | "account-clear-all" => Some("account_clear_all"),
        "cc_repo_assign" | "cc-repo-assign" => Some("cc_repo_assign"),
        "cc_repo_remove" | "cc-repo-remove" => Some("cc_repo_remove"),
        _ => None,
    }
}

pub(super) fn action_needs_task_id(normalized: &str) -> bool {
    !matches!(
        normalized,
        "unfreeze"
            | "unblock_all"
            | "ready_all"
            | "delete_done"
            | "supervisor_toggle"
            | "supervisor_repos"
            | "account_cooldown"
            | "account_clear"
            | "account_clear_all"
            | "cc_repo_assign"
            | "cc_repo_remove"
    )
}
