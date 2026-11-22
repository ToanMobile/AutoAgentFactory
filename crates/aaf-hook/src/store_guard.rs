//! A store submission needs a signed release receipt for exactly the tree
//! being submitted.

use std::path::Path;

use crate::shell::{Cmd, basename};
use crate::{Ctx, Denial};

pub const RULE: &str = "store_submit_guard";
pub const FIX: &str = "aaf gate --release";

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
                .any(|task| task.starts_with("publish") || task == "promoteArtifact")
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
    verify(ctx).err().map(|why| {
        Denial::new(
            RULE,
            format!("{tool} without a valid release receipt: {why}"),
            FIX,
        )
    })
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
    // disk from what was gated without moving the fingerprint. Files hidden
    // by an ignore rule are the release gate's business, not refused here:
    // every machine has some (`.DS_Store` under a global excludes file).
    let hidden = aaf_fp::hidden_files(&root, aaf_fp::GIT_TIMEOUT).map_err(|e| e.to_string())?;
    if hidden.flagged.is_empty() {
        return Ok(());
    }
    let flagged_only = aaf_fp::HiddenFiles {
        flagged: hidden.flagged,
        ignored: Vec::new(),
    };
    Err(flagged_only.reasons().join("; "))
}
