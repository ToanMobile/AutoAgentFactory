//! Re-export of owner-only filesystem helpers now owned by `aaf-fp::fs_private`.

pub use aaf_fp::fs_private::{create_dir, open_file, restrict_file_if_present};
