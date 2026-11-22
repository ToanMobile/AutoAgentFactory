//! Account pool: discovery of agent config directories, per-account file
//! leases and the router that picks the next usable account.
//! See docs/PLAN.md section C.8.

pub mod discovery;
pub mod lease;
pub mod router;
pub mod selection;

pub use discovery::{DiscoveryEnv, binary_on_path_in, discover, discover_from_process_env};
pub use lease::Lease;
pub use router::{AccountView, Router};
pub use selection::{Binding, BindingSource, RunHistory};

use aaf_domain::ports::PortError;

#[derive(Debug, thiserror::Error)]
pub enum AccountsError {
    #[error(transparent)]
    Store(#[from] PortError),
    #[error("account id {0:?} is not safe as a file name")]
    InvalidAccountId(String),
    #[error("lease path {path} is a symlink")]
    SymlinkLease { path: String },
    #[error("lease {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
