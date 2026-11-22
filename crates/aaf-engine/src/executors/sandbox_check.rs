//! Asks the kernel about the Seatbelt sandbox a process runs in: the one mark
//! a process of an agent run cannot shed, whatever it does to its group,
//! parent or environment (`env -i`).
//!
//! The only module of the workspace allowed to use `unsafe` (owner decision
//! 2026-11-22, SECURITY.md §10). `sandbox_check` is a C function of
//! libsystem_sandbox with no Rust binding; the SDK's public `sandbox.h` does
//! not declare it. The declaration used here is Apple's own, from WebKit
//! (`Source/WTF/wtf/spi/darwin/SandboxSPI.h`):
//!
//! ```c
//! int sandbox_check(pid_t, const char *operation, enum sandbox_filter_type, ...);
//! extern const enum sandbox_filter_type SANDBOX_CHECK_NO_REPORT;
//! enum sandbox_filter_type { SANDBOX_FILTER_NONE, SANDBOX_FILTER_PATH, ... };
//! ```
//!
//! Both symbols are exported by `usr/lib/system/libsystem_sandbox.tbd` of the
//! macOS SDK, which every Rust program on macOS links through libSystem.
#![allow(unsafe_code)]

/// Whether `pid` runs in a sandbox at all. `None`: no answer (the process is
/// gone, belongs to another user, or this is not macOS).
pub fn is_sandboxed(pid: u32) -> Option<bool> {
    imp::is_sandboxed(pid)
}

/// Whether the sandbox of `pid` lets it write `path`. `None`: no answer.
pub fn may_write(pid: u32, path: &str) -> Option<bool> {
    imp::may_write(pid, path)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_char, c_int};

    const SANDBOX_FILTER_NONE: c_int = 0;
    const SANDBOX_FILTER_PATH: c_int = 1;

    unsafe extern "C" {
        fn sandbox_check(pid: c_int, operation: *const c_char, filter: c_int, ...) -> c_int;
        static SANDBOX_CHECK_NO_REPORT: c_int;
    }

    pub fn is_sandboxed(pid: u32) -> Option<bool> {
        let pid = c_int::try_from(pid).ok()?;
        // SAFETY: a null operation with SANDBOX_FILTER_NONE is the
        // "is this process sandboxed" query (WebKit's
        // `currentProcessIsSandboxed`); no variadic argument is read and
        // nothing is written through a pointer.
        let answer = unsafe { sandbox_check(pid, std::ptr::null(), SANDBOX_FILTER_NONE) };
        (answer >= 0).then_some(answer > 0)
    }

    pub fn may_write(pid: u32, path: &str) -> Option<bool> {
        let pid = c_int::try_from(pid).ok()?;
        let path = CString::new(path).ok()?;
        // SAFETY: both strings are NUL-terminated and outlive the call; with
        // SANDBOX_FILTER_PATH the one variadic argument read is a
        // `const char *`, which is what is passed. Reading the exported
        // constant only copies an int.
        let answer = unsafe {
            sandbox_check(
                pid,
                c"file-write-data".as_ptr(),
                SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT,
                path.as_ptr(),
            )
        };
        // 0: allowed, 1: denied, -1: no answer.
        match answer {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn is_sandboxed(_pid: u32) -> Option<bool> {
        None
    }

    pub fn may_write(_pid: u32, _path: &str) -> Option<bool> {
        None
    }
}
