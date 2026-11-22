//! One module per subcommand. `init`, `doctor`, `accounts`, `gate`, `approve`
//! belong to the cli lane; `board` and `run` to the factory lane.

pub mod accounts;
pub mod approve;
pub mod board;
pub mod doctor;
pub mod gate;
pub mod init;
pub mod run;
pub mod slot;
