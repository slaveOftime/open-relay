//! Crash diagnostics for the detached daemon process.
//!
//! The daemon normally runs fully detached with stdout/stderr redirected to
//! null (see `spawn_detached` in `lifecycle.rs`), and `tracing` only flushes
//! `info!`/`error!` calls that the code actually makes. That means two very
//! common failure modes previously left **zero** trace anywhere:
//!
//! 1. A Rust panic anywhere in the process (including inside a
//!    `tokio::spawn`ed task, whose `JoinHandle` is never awaited) — the
//!    default panic hook prints to stderr, which historically was discarded.
//! 2. A native crash (access violation, stack overflow, etc.) that never
//!    goes through Rust's panic machinery at all, e.g. inside the `unsafe`
//!    Win32 FFI calls used for PTY/process management.
//!
//! This module installs a global panic hook that records panics to both the
//! tracing log and a dedicated `daemon-crash.log` file, and (on Windows)
//! installs a top-level unhandled-exception filter to capture native
//! crashes. `lifecycle::spawn_detached` also stops discarding stderr so any
//! raw text the process writes on its way down (e.g. the stack-overflow
//! guard page message, which bypasses the panic hook entirely) is preserved.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Path to the durable crash record, kept separate from the rotating
/// `daemon.log` so it survives even if the tracing subscriber never gets a
/// chance to flush before the process goes down.
pub fn crash_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("daemon-crash.log")
}

/// Path to the file that raw process stderr is redirected to for detached
/// daemon runs.
pub fn stderr_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("daemon-stderr.log")
}

fn append_line(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{contents}");
        let _ = file.flush();
    }
}

/// Install a global panic hook that logs every panic — including ones inside
/// `tokio::spawn`ed tasks — with thread name, source location, message, and a
/// captured backtrace. The previous hook is chained after so foreground /
/// interactive behavior (printing to the terminal) is unaffected.
pub fn install_panic_hook(state_dir: PathBuf) {
    let previous = std::panic::take_hook();
    let crash_log = crash_log_path(&state_dir);

    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown location>".to_string());
        let message = payload_message(info.payload());

        tracing::error!(
            target: "oly::panic",
            thread = %thread_name,
            location = %location,
            panic_message = %message,
            "daemon thread panicked; see daemon-crash.log for the backtrace"
        );

        let timestamp = chrono::Utc::now().to_rfc3339();
        append_line(
            &crash_log,
            &format!(
                "[{timestamp}] PANIC thread={thread_name} location={location} message={message}\nbacktrace:\n{backtrace}\n"
            ),
        );

        previous(info);
    }));
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(windows)]
mod windows_exception {
    use super::{append_line, crash_log_path};
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
    };

    static CRASH_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

    /// Runs on the crashing thread when an exception reaches the top of the
    /// stack unhandled (e.g. access violation from `unsafe` FFI). Keep this
    /// as small and allocation-light as reasonably possible since the
    /// process state may already be corrupted.
    unsafe extern "system" fn handler(info: *const EXCEPTION_POINTERS) -> i32 {
        if let Some(path) = CRASH_LOG_PATH.get() {
            let (code, address) = unsafe {
                if info.is_null() || (*info).ExceptionRecord.is_null() {
                    (0u32, 0usize)
                } else {
                    let record = &*(*info).ExceptionRecord;
                    (
                        record.ExceptionCode as u32,
                        record.ExceptionAddress as usize,
                    )
                }
            };
            let timestamp = chrono::Utc::now().to_rfc3339();
            append_line(
                path,
                &format!(
                    "[{timestamp}] NATIVE CRASH exception_code=0x{code:08X} ({}) exception_address=0x{address:X} pid={}",
                    describe_exception_code(code),
                    std::process::id()
                ),
            );
        }
        // Let the OS continue its normal unhandled-exception processing
        // (i.e. terminate the process) after we've recorded what we could.
        EXCEPTION_CONTINUE_SEARCH
    }

    fn describe_exception_code(code: u32) -> &'static str {
        match code {
            0xC0000005 => "STATUS_ACCESS_VIOLATION",
            0xC00000FD => "STATUS_STACK_OVERFLOW",
            0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
            0xC0000409 => "STATUS_STACK_BUFFER_OVERRUN",
            0xC000001D => "STATUS_ILLEGAL_INSTRUCTION",
            0xC0000025 => "STATUS_NONCONTINUABLE_EXCEPTION",
            0x80000003 => "STATUS_BREAKPOINT",
            _ => "UNKNOWN",
        }
    }

    pub fn install(state_dir: PathBuf) {
        let _ = CRASH_LOG_PATH.set(crash_log_path(&state_dir));
        unsafe {
            SetUnhandledExceptionFilter(Some(handler));
        }
    }
}

/// Install a native (OS-level) crash handler in addition to the Rust panic
/// hook. Currently only implemented on Windows via
/// `SetUnhandledExceptionFilter`, which is the dominant platform this daemon
/// runs on and where `unsafe` Win32 FFI calls (process/PTY management) make
/// native crashes most plausible.
#[cfg(windows)]
pub fn install_native_crash_handler(state_dir: PathBuf) {
    windows_exception::install(state_dir);
}

#[cfg(not(windows))]
pub fn install_native_crash_handler(_state_dir: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oly-crash-test-{name}-{nanos}"))
    }

    #[test]
    fn panic_hook_records_backtrace_to_crash_log() {
        let state_dir = unique_temp_dir("panic-hook");
        install_panic_hook(state_dir.clone());

        let result = std::panic::catch_unwind(|| {
            panic!("synthetic panic for crash-log test");
        });
        assert!(result.is_err());

        let log_path = crash_log_path(&state_dir);
        let contents = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("expected crash log at {log_path:?}: {e}"));
        assert!(contents.contains("PANIC"));
        assert!(contents.contains("synthetic panic for crash-log test"));
        assert!(contents.contains("backtrace"));

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn crash_and_stderr_log_paths_are_distinct_and_under_logs_dir() {
        let state_dir = PathBuf::from("some-state-dir");
        assert_eq!(
            crash_log_path(&state_dir),
            state_dir.join("logs").join("daemon-crash.log")
        );
        assert_eq!(
            stderr_log_path(&state_dir),
            state_dir.join("logs").join("daemon-stderr.log")
        );
        assert_ne!(crash_log_path(&state_dir), stderr_log_path(&state_dir));
    }
}
