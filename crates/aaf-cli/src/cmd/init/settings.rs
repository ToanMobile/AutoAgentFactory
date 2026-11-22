//! The `aaf-hook` entry in `.claude/settings.json`. The implementation lives
//! in the engine, which checks the same registration before it starts an
//! agent; `aaf init` and `aaf doctor` use it from there.

pub use aaf_engine::hook_settings::*;
