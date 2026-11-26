//! The `--json` contract and exit codes (docs/PLAN.md section C.10).
//!
//! stdout carries exactly one JSON object `{ok, command, status, ..., duration_ms}`
//! in JSON mode, or the human text otherwise. Exit: 0 success/Clear, 1 runtime,
//! config or I/O error, 2 Blocked/Unverified/policy violation.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok,
    /// Blocked, Unverified or refused by policy.
    Policy,
}

#[derive(Debug, Clone)]
pub struct CmdOutput {
    /// Short machine word: `ok`, `CLEAR`, `BLOCKED`, `UNVERIFIED`, `refused`, ...
    pub status: String,
    pub exit: Exit,
    /// Extra top-level keys of the JSON object. `ok`, `command`, `status` and
    /// `duration_ms` are reserved and overwritten.
    pub fields: Map<String, Value>,
    /// Human output for non-JSON mode.
    pub text: String,
}

impl CmdOutput {
    pub fn ok(status: impl Into<String>, text: impl Into<String>) -> CmdOutput {
        CmdOutput {
            status: status.into(),
            exit: Exit::Ok,
            fields: Map::new(),
            text: text.into(),
        }
    }

    pub fn policy(status: impl Into<String>, text: impl Into<String>) -> CmdOutput {
        CmdOutput {
            status: status.into(),
            exit: Exit::Policy,
            fields: Map::new(),
            text: text.into(),
        }
    }

    #[must_use]
    pub fn field(mut self, key: &str, value: Value) -> CmdOutput {
        self.fields.insert(key.to_string(), value);
        self
    }
}

/// Runtime, config or I/O failure: exit 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdError {
    pub message: String,
}

impl CmdError {
    pub fn new(message: impl Into<String>) -> CmdError {
        CmdError {
            message: message.into(),
        }
    }
}

pub type CmdResult = Result<CmdOutput, CmdError>;

/// The JSON object and exit code for a result. Pure, so tests cover it.
pub fn render(command: &str, result: &CmdResult, duration_ms: u64) -> (Value, u8) {
    let (mut object, code) = match result {
        Ok(out) => {
            let mut object = out.fields.clone();
            object.insert("ok".to_string(), json!(out.exit == Exit::Ok));
            object.insert("status".to_string(), json!(out.status));
            (object, if out.exit == Exit::Ok { 0 } else { 2 })
        }
        Err(err) => {
            let mut object = Map::new();
            object.insert("ok".to_string(), json!(false));
            object.insert("status".to_string(), json!("error"));
            object.insert("error".to_string(), json!(err.message));
            (object, 1)
        }
    };
    object.insert("command".to_string(), json!(command));
    object.insert("duration_ms".to_string(), json!(duration_ms));
    (Value::Object(object), code)
}

/// Prints the result and returns the process exit code.
pub fn emit(json_mode: bool, command: &str, result: &CmdResult, duration_ms: u64) -> u8 {
    let (object, code) = render(command, result, duration_ms);
    if json_mode {
        println!("{object}");
    } else {
        match result {
            Ok(out) => println!("{}", out.text),
            Err(err) => eprintln!("aaf {command}: {}", err.message),
        }
    }
    code
}

/// A progress line while a command is still working. stderr only: in JSON
/// mode stdout carries nothing but the final object.
///
/// A notice is advisory: when stderr is gone (a reader that took the first
/// line and closed the pipe) the line is dropped instead of panicking, so a
/// waiting `aaf slot` or `aaf gate` still runs its command and returns its
/// exit code.
pub fn notice(command: &str, line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "aaf {command}: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_result_is_exit_0_and_reserved_keys_cannot_be_spoofed() {
        let out = CmdOutput::ok("ok", "fine")
            .field("n", json!(3))
            .field("ok", json!("spoofed"));
        let (object, code) = render("doctor", &Ok(out), 7);
        assert_eq!(code, 0);
        assert_eq!(
            object,
            json!({"ok": true, "command": "doctor", "status": "ok", "n": 3, "duration_ms": 7})
        );
    }

    #[test]
    fn policy_result_is_exit_2_and_not_ok() {
        let (object, code) = render("gate", &Ok(CmdOutput::policy("BLOCKED", "no")), 1);
        assert_eq!(code, 2);
        assert_eq!(object["ok"], json!(false));
        assert_eq!(object["status"], json!("BLOCKED"));
    }

    #[test]
    fn error_is_exit_1_with_message() {
        let (object, code) = render("init", &Err(CmdError::new("boom")), 1);
        assert_eq!(code, 1);
        assert_eq!(object["error"], json!("boom"));
        assert_eq!(object["status"], json!("error"));
    }
}
