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
pub mod git;
pub mod hook_settings;
pub mod process;
pub mod store_compliance;
pub mod throttle;
pub mod unblocker;
pub mod webhook;
