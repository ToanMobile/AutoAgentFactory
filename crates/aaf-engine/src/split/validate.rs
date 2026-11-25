//! Validation and parsing for proactive multi-subagent split plans.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use aaf_domain::budget::can_spawn_subtask;
use aaf_domain::config::UnblockerConfig;
use aaf_domain::dag;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Task, TaskState};

use super::{MAX_SPLIT_SUBTASKS, SplitSubtask};

const MAX_KEY_CHARS: usize = 32;
const MAX_TITLE_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 4000;
const MAX_ACCEPTANCE_ITEMS: usize = 10;
const MAX_ACCEPTANCE_CHARS: usize = 500;

/// Parses an agent's split proposal (optionally inside a single Markdown code
/// fence), failing closed to an empty `Vec` on any parse or validation error.
pub fn parse_split_plans(text: &str, max_subtasks: usize) -> Vec<SplitSubtask> {
    let Ok(plans) = serde_json::from_str::<Vec<SplitSubtask>>(strip_fence(text)) else {
        return Vec::new();
    };
    validate_split_plans(plans, max_subtasks).unwrap_or_default()
}

/// Validates proposed subtasks: bounds, fields, normalized `owned_paths`,
/// acyclic `depends_on`, non-empty `owned_paths` for any subtask with a
/// parallel peer, and disjoint `owned_paths` across parallel pairs.
pub fn validate_split_plans(
    plans: Vec<SplitSubtask>,
    max_subtasks: usize,
) -> Result<Vec<SplitSubtask>, String> {
    let cap = max_subtasks.clamp(2, MAX_SPLIT_SUBTASKS);
    if !(2..=cap).contains(&plans.len()) {
        return Err(format!(
            "split requires 2..={cap} subtasks, got {}",
            plans.len()
        ));
    }
    let mut key_to_idx: HashMap<String, usize> = HashMap::with_capacity(plans.len());
    let mut normalized = Vec::with_capacity(plans.len());
    for (idx, mut plan) in plans.into_iter().enumerate() {
        validate_plan_fields(&plan)?;
        if key_to_idx.insert(plan.key.clone(), idx).is_some() {
            return Err(format!("duplicate subtask key '{}'", plan.key));
        }
        plan.owned_paths = normalize_owned_paths(&plan.owned_paths)?;
        normalized.push(plan);
    }

    let n = normalized.len();
    let nodes: Vec<String> = normalized.iter().map(|p| p.key.clone()).collect();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut reach = vec![vec![false; n]; n];
    for (idx, plan) in normalized.iter_mut().enumerate() {
        let mut unique_deps = Vec::with_capacity(plan.depends_on.len());
        let mut seen: HashSet<&str> = HashSet::new();
        for dep in &plan.depends_on {
            if dep == &plan.key {
                return Err(format!("subtask '{}' cannot depend on itself", plan.key));
            }
            let Some(&dep_idx) = key_to_idx.get(dep.as_str()) else {
                return Err(format!(
                    "subtask '{}' depends on unknown key '{dep}'",
                    plan.key
                ));
            };
            if seen.insert(dep.as_str()) {
                unique_deps.push(dep.clone());
                edges.push((plan.key.clone(), dep.clone()));
                reach[idx][dep_idx] = true;
            }
        }
        plan.depends_on = unique_deps;
    }

    if dag::topo_order(&nodes, &edges).is_none() {
        return Err("subtask depends_on graph contains a cycle".to_string());
    }
    for k in 0..n {
        for i in 0..n {
            if reach[i][k] {
                for j in 0..n {
                    reach[i][j] |= reach[k][j];
                }
            }
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if reach[i][j] || reach[j][i] {
                continue;
            }
            if normalized[i].owned_paths.is_empty() {
                return Err(format!(
                    "parallel subtask '{}' must declare non-empty owned_paths",
                    normalized[i].key
                ));
            }
            if normalized[j].owned_paths.is_empty() {
                return Err(format!(
                    "parallel subtask '{}' must declare non-empty owned_paths",
                    normalized[j].key
                ));
            }
            for a in &normalized[i].owned_paths {
                for b in &normalized[j].owned_paths {
                    if paths_overlap(a, b) {
                        return Err(format!(
                            "parallel subtasks '{}' and '{}' have overlapping owned_paths: '{a}' and '{b}'",
                            normalized[i].key, normalized[j].key
                        ));
                    }
                }
            }
        }
    }
    Ok(normalized)
}

fn validate_plan_fields(plan: &SplitSubtask) -> Result<(), String> {
    let filled = |s: &str, max: usize| !s.trim().is_empty() && s.chars().count() <= max;
    let key_ok = (1..=MAX_KEY_CHARS).contains(&plan.key.len())
        && plan
            .key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !key_ok {
        return Err(format!("invalid subtask key {:?}", plan.key));
    }
    if !filled(&plan.title, MAX_TITLE_CHARS) || plan.title.chars().any(char::is_control) {
        return Err(format!("invalid title for subtask '{}'", plan.key));
    }
    if !filled(&plan.description, MAX_DESCRIPTION_CHARS) {
        return Err(format!("invalid description for subtask '{}'", plan.key));
    }
    if !(1..=MAX_ACCEPTANCE_ITEMS).contains(&plan.acceptance.len())
        || !plan
            .acceptance
            .iter()
            .all(|a| filled(a, MAX_ACCEPTANCE_CHARS))
    {
        return Err(format!(
            "invalid acceptance criteria for subtask '{}'",
            plan.key
        ));
    }
    Ok(())
}

fn normalize_owned_paths(paths: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(paths.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(paths.len());
    for raw in paths {
        let trimmed = raw.trim();
        if raw.chars().any(char::is_control)
            || trimmed.is_empty()
            || trimmed.starts_with('/')
            || trimmed.starts_with('\\')
            || trimmed.contains('\\')
            || trimmed.chars().any(|c| matches!(c, '*' | '?' | '[' | ']'))
        {
            return Err(format!("invalid relative owned path {raw:?}"));
        }
        let mut s = trimmed.trim_end_matches('/');
        while let Some(rest) = s.strip_prefix("./") {
            s = rest;
        }
        let mut parts: Vec<&str> = Vec::new();
        for seg in s.split('/') {
            if seg == ".." {
                return Err(format!("owned path {raw:?} must not contain '..'"));
            }
            if !seg.is_empty() && seg != "." {
                parts.push(seg);
            }
        }
        if parts.is_empty() {
            return Err(format!(
                "owned path {raw:?} must name a file or subdirectory"
            ));
        }
        let clean = parts.join("/");
        let bad_comp = |c: Component<'_>| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        };
        if Path::new(&clean).components().any(bad_comp) {
            return Err(format!("owned path {raw:?} must stay relative within repo"));
        }
        if seen.insert(clean.to_ascii_lowercase()) {
            out.push(clean);
        }
    }
    Ok(out)
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    a == b
        || a.strip_prefix(&b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(&a).is_some_and(|rest| rest.starts_with('/'))
}

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(inner) = trimmed
        .strip_prefix("```")
        .and_then(|r| r.strip_suffix("```"))
    else {
        return trimmed;
    };
    inner
        .strip_prefix("json")
        .or_else(|| inner.strip_prefix("JSON"))
        .unwrap_or(inner)
        .trim()
}

pub(super) fn validate_parent_splittable(
    parent: &Task,
    budget_cfg: &UnblockerConfig,
) -> PortResult<()> {
    if parent.depth > 0 || !can_spawn_subtask(parent.depth, budget_cfg.max_subtask_depth) {
        return Err(PortError::Invalid(format!(
            "task {} at depth {} cannot spawn subtasks (max depth {})",
            parent.id, parent.depth, budget_cfg.max_subtask_depth
        )));
    }
    if !matches!(parent.state, TaskState::Ready | TaskState::Backlog) {
        return Err(PortError::Invalid(format!(
            "task {} in state {} cannot be split (must be ready or backlog)",
            parent.id,
            parent.state.as_str()
        )));
    }
    Ok(())
}

fn strip_untrusted_tags(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        let rest = &lower[i..];
        if let Some(tag) = ["</untrusted-data", "<untrusted-data"]
            .into_iter()
            .find(|t| rest.starts_with(t))
        {
            out.push_str("[tag removed]");
            i += tag.len();
        } else if let Some(ch) = text[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

pub(super) fn frame_untrusted(text: &str) -> String {
    let safe = strip_untrusted_tags(text);
    let id: String = aaf_fp::sha256_hex(safe.as_bytes())
        .chars()
        .take(16)
        .collect();
    format!("<untrusted-data id=\"{id}\">\n{safe}\n</untrusted-data id=\"{id}\">")
}
