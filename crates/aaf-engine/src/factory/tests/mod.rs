//! Tests of the factory loop. Agent, gate, git, accounts and clock are
//! in-memory fakes; the store is a real `Db` in a temp dir. `git_real` runs
//! the production git and gate adapters on real repositories.

mod adapters;
mod batch;
mod commit_lock;
mod failure;
mod fakes;
mod fakes_git;
mod flow;
mod g3_rework_edges;
mod git_real;
mod infra;
mod oracle_g3_rework_filter;
mod policy;
mod real_run;
mod round;
mod selection;
mod world;
