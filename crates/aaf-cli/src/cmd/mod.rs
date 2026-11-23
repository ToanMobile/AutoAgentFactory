//! One module per subcommand. `init`, `doctor`, `accounts`, `gate`, `approve`
//! belong to the cli lane; `board` and `run` to the factory lane.

pub mod accounts;
pub mod approve;
pub mod board;
pub mod cockpit;
pub mod cockpit_actions;
pub mod doctor;
pub mod gate;
pub mod gc;
pub mod handover;
pub mod init;
pub mod merge;
pub mod plan;
pub mod proof;
pub mod run;
pub mod skills;
pub mod slot;
pub mod status;
pub mod telegram;
pub mod watch;
