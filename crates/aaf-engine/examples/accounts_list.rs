//! Prints the accounts discovered for the real home as JSON (ids, providers
//! and config dir paths only; no file inside those dirs is opened).

use aaf_domain::config::AccountsConfig;
use aaf_engine::accounts::discover_from_process_env;

fn main() -> Result<(), serde_json::Error> {
    let accounts = discover_from_process_env(&AccountsConfig::default());
    println!("{}", serde_json::to_string_pretty(&accounts)?);
    Ok(())
}
