//! Gate tests. Every trigger string is assembled from parts so that these
//! files do not trip the static rules when the gate runs on this repository.

mod adopt_flags;
mod head_lock;
mod head_lock_dir;
mod hidden;
mod pipeline;
mod private;
mod rekill;
mod reuse;
mod runner;
mod statics;
mod support;
mod throttled;
