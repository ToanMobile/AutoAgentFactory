//! Core engine of AutoAgentFactory. Each module is owned by one lane
//! (see docs/PLAN.md section D).

pub mod accounts;
pub mod config;
pub mod cross_audit;
pub mod db;
pub mod detector;
pub mod executors;
pub mod factory;
pub mod gate;
pub mod gc;
pub mod git;
pub mod github_board;
pub mod handover;
mod handover_html;
pub mod hook_settings;
pub mod plan_mode;
pub mod process;
pub mod proof;
pub mod skills;
pub mod split;
pub mod store_compliance;
pub mod telegram;
pub mod telemetry;
pub mod throttle;
pub mod unblocker;
pub mod watch;
pub mod webhook;
