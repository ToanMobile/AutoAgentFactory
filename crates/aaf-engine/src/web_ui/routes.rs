//! URL to route mapping of the Web UI server.

use super::percent_decode;

pub(super) enum Route {
    Index,
    /// The all-in-one metrics page.
    Studio,
    Report,
    Doctor,
    Health,
    Meta,
    Status,
    Board,
    TaskDetail(String),
    Handover(String),
    ApprovePreview(String),
    StreamTail(String),
    Events,
    ApproveCommit,
    Action,
    MethodNotAllowed,
    BadRequest(&'static str),
    NotFound,
}

pub(super) fn match_route(method: &str, path: &str) -> Route {
    let exact = |want: &str, r: Route| {
        if method == want {
            r
        } else {
            Route::MethodNotAllowed
        }
    };
    match path {
        "/" => return exact("GET", Route::Index),
        "/studio" => return exact("GET", Route::Studio),
        "/api/report" => return exact("GET", Route::Report),
        "/api/doctor" => return exact("GET", Route::Doctor),
        "/api/health" => return exact("GET", Route::Health),
        "/api/meta" => return exact("GET", Route::Meta),
        "/api/status" => return exact("GET", Route::Status),
        "/api/board" => return exact("GET", Route::Board),
        "/api/events" => return exact("GET", Route::Events),
        "/api/approve" => return exact("POST", Route::ApproveCommit),
        "/api/action" => return exact("POST", Route::Action),
        _ => {}
    }
    type RouteCtor = fn(String) -> Route;
    let prefixes: [(&str, RouteCtor); 4] = [
        ("/api/task/", Route::TaskDetail),
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
