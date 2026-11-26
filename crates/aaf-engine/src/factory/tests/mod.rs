//! Tests of the factory loop. Agent, gate, git, accounts and clock are
//! in-memory fakes; the store is a real `Db` in a temp dir. `git_real` runs
//! the production git and gate adapters on real repositories.

mod a13_fx1_r10b_commit;
mod a13_fx1_r10b_loop;
mod a13_fx2_r10b_gate_retry;
mod adapters;
mod audit_retry;
mod auto_double_gate;
mod base_ref;
mod batch;
mod commit_hooks;
mod commit_lock;
mod embedded_repo;
mod failure;
mod fakes;
mod fakes_git;
mod flake_heal;
mod flow;
mod full_gate;
mod g3_rework_edges;
mod git_real;
mod halt_alert;
mod infra;
mod infra_retry;
mod merge_lock_retry;
mod merge_retry;
mod opt12;
mod oracle_g3_rework_filter;
mod policy;
mod post_audit_replay;
mod pr_feedback;
mod proof_needs_human;
mod real_run;
mod rebase_hooks;
mod round;
mod selection;
mod startup_sweep;
mod sweep_merged_pr;
mod world;
