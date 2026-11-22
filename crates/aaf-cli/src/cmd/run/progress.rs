//! What `aaf run` shows while a gate run waits for the machine (PLAN C.11:
//! waiting is shown, it is not an error).

use std::path::Path;

use aaf_engine::throttle::Progress;

use crate::cmd::gate::progress_line;

/// One short line per report of the throttle, led by the tree being gated
/// (a task id or `batch-…`): a run gates many trees.
pub(crate) fn line(tree: &Path, event: &Progress<'_>) -> String {
    let name = tree.file_name().unwrap_or(tree.as_os_str());
    format!("gate {}: {}", name.to_string_lossy(), progress_line(event))
}
