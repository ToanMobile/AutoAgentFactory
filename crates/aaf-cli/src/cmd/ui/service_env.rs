//! LaunchAgent environment allow-list for `aaf ui --install-service`.

use std::path::Path;

use aaf_engine::executors::{TOOLCHAIN_HOME_ENV, is_valid_toolchain_dir};

use crate::ctx::Ctx;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn non_default_service_env(ctx: &Ctx) -> Vec<(&'static str, String)> {
    let toolchain: Vec<(String, String)> = TOOLCHAIN_HOME_ENV
        .iter()
        .filter_map(|&k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    non_default_service_env_with_toolchain(ctx, &toolchain)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn non_default_service_env_with_toolchain(
    ctx: &Ctx,
    toolchain_env: &[(String, String)],
) -> Vec<(&'static str, String)> {
    let wt_rel = aaf_engine::executors::DEFAULT_WORKTREES_REL;
    let abs = |p: &Path| ctx.cwd.join(p);
    let mut extra = Vec::new();
    for (k, p, rel) in [
        ("AAF_CONFIG_DIR", &ctx.config_dir, ".config/autofactory"),
        ("AAF_DATA_DIR", &ctx.data_dir, ".local/share/autofactory"),
        ("AAF_WORKTREE_DIR", &ctx.worktrees_dir, wt_rel),
    ] {
        let v = abs(p);
        if v != abs(&ctx.home.join(rel)) {
            extra.push((k, v.display().to_string()));
        }
    }
    if let Some((_, v)) = ctx
        .aaf_env
        .iter()
        .find(|(k, v)| k == "AAF_SEED_CACHE" && !v.is_empty())
    {
        extra.push(("AAF_SEED_CACHE", v.clone()));
    }
    for &key in &TOOLCHAIN_HOME_ENV {
        if let Some((_, v)) = toolchain_env.iter().find(|(k, _)| k == key)
            && is_valid_toolchain_dir(v)
        {
            extra.push((key, v.clone()));
        }
    }
    extra
}
