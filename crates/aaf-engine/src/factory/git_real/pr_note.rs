//! The batch note a pull request of an integration branch carries above
//! its receipt summary.

use std::io;
use std::path::Path;

use aaf_domain::proof::ProofRecord;

use crate::git::{GitError, PrOutcome, PrRequest};
use crate::process::{CommandOutput, CommandRunner};

/// Puts a note above the body of `gh pr create`. The body is written inside
/// [`crate::git::open_pr`], so it is amended on its way to `gh`.
struct NotedRunner<'a> {
    inner: &'a dyn CommandRunner,
    note: &'a str,
}

impl CommandRunner for NotedRunner<'_> {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        if program != "gh" {
            return self.inner.run(program, args, cwd);
        }
        if args.starts_with(&["pr", "create"]) {
            // By position, never by searching: the title is untrusted text and
            // may itself read `--body`.
            let ["pr", "create", "--title", _, "--body", body, ..] = args else {
                // A pull request without the note would present a batch receipt
                // as the branch's own; none is opened instead.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "gh pr create: no body to put the batch note in",
                ));
            };
            let noted = format!("{}\n\n{body}", self.note);
            let mut args = args.to_vec();
            args[5] = &noted;
            return self.inner.run(program, &args, cwd);
        }
        if args.starts_with(&["pr", "edit"]) {
            let ["pr", "edit", _, "--body", body, ..] = args else {
                return self.inner.run(program, args, cwd);
            };
            let noted = format!("{}\n\n{body}", self.note);
            let mut args = args.to_vec();
            args[4] = &noted;
            return self.inner.run(program, &args, cwd);
        }
        self.inner.run(program, args, cwd)
    }
}

/// [`crate::git::open_pr`] with `note` above the receipt summary of the body.
#[cfg(test)]
pub(in crate::factory) fn open_pr_noted(
    request: &PrRequest,
    runner: &dyn CommandRunner,
    note: Option<&str>,
) -> Result<PrOutcome, GitError> {
    open_pr_noted_with_proof(request, None, runner, note)
}

pub(super) fn open_pr_noted_with_proof(
    request: &PrRequest,
    proof: Option<&ProofRecord>,
    runner: &dyn CommandRunner,
    note: Option<&str>,
) -> Result<PrOutcome, GitError> {
    match note {
        Some(note) => crate::git::open_pr_with_proof(
            request,
            proof,
            &NotedRunner {
                inner: runner,
                note,
            },
        ),
        None => crate::git::open_pr_with_proof(request, proof, runner),
    }
}
