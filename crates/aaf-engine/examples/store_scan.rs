//! Read-only store compliance scan of one repository, printed as JSON.
//!
//!     cargo run -p aaf-engine --example store_scan -- <path> [--min-sdk N]

use std::path::PathBuf;
use std::process::ExitCode;

use aaf_domain::config::{RepoConfig, StoreMode};
use aaf_engine::store_compliance::scan;

const USAGE: &str = "usage: store_scan <path> [--min-sdk N]";

fn main() -> ExitCode {
    let mut path: Option<PathBuf> = None;
    let mut cfg = RepoConfig::default();
    // Scan whatever is at the path, independent of any profile.
    cfg.gate.store_compliance = StoreMode::Always;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--min-sdk" {
            match args.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(n) => cfg.gate.min_android_target_sdk = n,
                None => {
                    eprintln!("--min-sdk needs an integer\n{USAGE}");
                    return ExitCode::from(1);
                }
            }
        } else if path.is_none() && !arg.starts_with("--") {
            path = Some(PathBuf::from(arg));
        } else {
            eprintln!("unexpected argument `{arg}`\n{USAGE}");
            return ExitCode::from(1);
        }
    }
    let Some(path) = path else {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    };
    let report = scan(&path, &cfg, true);
    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cannot serialise the report: {e}");
            ExitCode::from(1)
        }
    }
}
