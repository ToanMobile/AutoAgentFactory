//! A store submission needs a signed release receipt for exactly the tree
//! being submitted.

use std::path::Path;

use crate::shell::{Cmd, basename};
use crate::{Ctx, Denial};

pub const RULE: &str = "store_submit_guard";
pub const FIX: &str = "aaf gate --release";
/// Worker-mode fix: inside a factory run the agent cannot run `aaf gate --release`.
pub const FIX_IN_FACTORY_RUN: &str = "skip store submission inside a worker run; build and verify \
                                      artifacts locally (e.g. publishToMavenLocal or \
                                      assembleRelease) — the operator or release pipeline runs \
                                      aaf gate --release outside the sandbox";

fn is_local_gradle_publish(task: &str) -> bool {
    let lower = task.to_ascii_lowercase();
    lower.ends_with("tomavenlocal")
        || lower.ends_with("tolocalrepository")
        || lower.contains("localpublication")
}

/// Programs that run the tool named in their arguments.
const LAUNCHERS: &[&str] = &[
    "bundle", "npx", "bunx", "yarn", "pnpm", "xcrun", "mise", "rbenv",
];

/// The submit tool `words` invoke: as the command itself or behind a
/// launcher (`bundle exec fastlane …`, `npx eas-cli …`). With `anywhere` the
/// position is ignored, for text that could not be split into commands.
pub fn matches<S: AsRef<str>>(words: &[S], anywhere: bool) -> Option<&'static str> {
    let launched = words
        .first()
        .is_some_and(|first| LAUNCHERS.contains(&basename(first.as_ref())));
    for (at, word) in words.iter().enumerate() {
        if at > 0 && !launched && !anywhere {
            break;
        }
        let after = || words[at + 1..].iter().map(AsRef::as_ref);
        let hit = match basename(word.as_ref()) {
            "fastlane" => after()
                .any(|w| matches!(w, "deliver" | "supply" | "pilot") || w.starts_with("upload_to_"))
                .then_some("fastlane upload"),
            // Any word, not the first non-option one: `--profile prod submit`
            // has an option value in front of the subcommand.
            "eas" | "eas-cli" => after()
                .any(|w| w == "submit" || w.starts_with("--auto-submit"))
                .then_some("eas submit"),
            "altool" => after()
                .any(|w| w.starts_with("--upload-app") || w.starts_with("--upload-package"))
                .then_some("altool upload"),
            "gradlew" | "gradlew.bat" | "gradle" => after()
                .map(|w| w.rsplit(':').next().unwrap_or(w))
                .any(|task| {
                    (task.starts_with("publish") || task == "promoteArtifact")
                        && !is_local_gradle_publish(task)
                })
                .then_some("gradle publish"),
            _ => None,
        };
        if hit.is_some() {
            return hit;
        }
    }
    None
}

pub fn check(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let tool = matches(&cmd.words, false)?;
    let fix = if ctx.env.in_factory_run {
        FIX_IN_FACTORY_RUN
    } else {
        FIX
    };
    verify(ctx).err().map(|why| {
        Denial::new(
            RULE,
            format!("{tool} without a valid release receipt: {why}"),
            fix,
        )
    })
}

/// Harmless OS metadata basenames tolerated in external ignore lists during
/// store submission. Every other external-excluded file is refused.
fn is_os_metadata_file(rel_path: &str) -> bool {
    if rel_path.ends_with('/') {
        return false;
    }
    let Some(base) = Path::new(rel_path).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    base.eq_ignore_ascii_case(".ds_store")
        || base.eq_ignore_ascii_case("thumbs.db")
        || base.eq_ignore_ascii_case("desktop.ini")
        || base.starts_with("._")
}

/// Every failure on this path refuses the submission.
fn verify(ctx: &Ctx) -> Result<(), String> {
    let root = ctx
        .root()
        .ok_or("the working directory is not inside a git repository")?;
    let data_dir: &Path = ctx
        .env
        .data_dir
        .as_deref()
        .ok_or("the data directory is unknown")?;
    let key = aaf_fp::load_key(data_dir).map_err(|e| e.to_string())?;
    let receipt = aaf_fp::read_receipt(data_dir, &key, &root)
        .map_err(|e| format!("no usable receipt ({e})"))?;
    let current = aaf_fp::tree_fp(&root, aaf_fp::GIT_TIMEOUT).map_err(|e| e.to_string())?;
    // Bound to this repository: a signed receipt copied from another repo's
    // slot must not unlock a submission from here.
    aaf_fp::check_store_receipt_in(&root, &receipt, &current)?;
    // A tracked file marked skip-worktree or assume-unchanged can differ on
    // disk from what was gated without moving the fingerprint, and any
    // non-OS-metadata file hidden by an external ignore rule (e.g.
    // `.git/info/exclude`) can alter the build silently (SECURITY.md §9).
    let hidden = aaf_fp::hidden_files(&root, aaf_fp::GIT_TIMEOUT).map_err(|e| e.to_string())?;
    let disallowed_ignored: Vec<_> = hidden
        .ignored
        .into_iter()
        .filter(|entry| !is_os_metadata_file(&entry.path))
        .collect();
    if hidden.flagged.is_empty() && disallowed_ignored.is_empty() {
        return Ok(());
    }
    let filtered = aaf_fp::HiddenFiles {
        flagged: hidden.flagged,
        ignored: disallowed_ignored,
    };
    Err(filtered.reasons().join("; "))
}
