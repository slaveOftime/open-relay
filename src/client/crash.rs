//! Windows-only unhandled-exception filter that restores the TUI before the
//! process dies from a genuine unhandled exception.
//!
//! Carved out of `list_tui.rs` by PLAN2 S1.3. The crash-handling `unsafe`
//! runs exclusively from this module (Win32 calls into
//! `SetUnhandledExceptionFilter`, `WriteFile`, and the `EXCEPTION_POINTERS`
//! read). Other Win32 pointer dances in `list_tui.rs`
//! (`windows_screen_geometry`) live in their own fenced
//! function and are out of scope here.

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::{
    Storage::FileSystem::WriteFile,
    System::{
        Console::{GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE},
        Diagnostics::Debug::{
            EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
        },
    },
};

use crate::client::list_tui::{TITLE_RESTORE_BYTES, TUI_RESTORE_BYTES};

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static CRASH_REPORTED: AtomicBool = AtomicBool::new(false);

/// Install the unhandled-exception filter. The top-level filter (not the
/// vectored handler) is used deliberately: vectored handlers fire for every
/// first-chance exception, so handled exceptions raised by normal Windows
/// code would otherwise tear down the live TUI by mistake.
pub(crate) fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(handle_unhandled_exception));
    }
}

pub(crate) fn set_tui_active(active: bool) {
    if active {
        CRASH_REPORTED.store(false, Ordering::SeqCst);
    }
    TUI_ACTIVE.store(active, Ordering::SeqCst);
}

unsafe extern "system" fn handle_unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
    unsafe { report_crash(info) };
    EXCEPTION_CONTINUE_SEARCH
}

unsafe fn report_crash(info: *const EXCEPTION_POINTERS) {
    if TUI_ACTIVE.load(Ordering::SeqCst) && !CRASH_REPORTED.swap(true, Ordering::SeqCst) {
        unsafe {
            // `set_tui_active(true)` is only reached after the window title
            // was pushed onto the title stack, so the crash path can safely
            // pop it back.
            write_handle(STD_OUTPUT_HANDLE, TUI_RESTORE_BYTES);
            write_handle(STD_OUTPUT_HANDLE, TITLE_RESTORE_BYTES);
            write_handle(STD_ERROR_HANDLE, native_crash_message(info));
        }
    }
}

unsafe fn native_crash_message(info: *const EXCEPTION_POINTERS) -> &'static [u8] {
    let code = unsafe {
        if info.is_null() || (*info).ExceptionRecord.is_null() {
            0
        } else {
            (*(*info).ExceptionRecord).ExceptionCode as u32
        }
    };
    match code {
        0xC0000005 => b"\r\nerror: interactive session list crashed: STATUS_ACCESS_VIOLATION\r\n",
        0xC00000FD => b"\r\nerror: interactive session list crashed: STATUS_STACK_OVERFLOW\r\n",
        0xC0000374 => b"\r\nerror: interactive session list crashed: STATUS_HEAP_CORRUPTION\r\n",
        _ => b"\r\nerror: interactive session list crashed: native exception\r\n",
    }
}

unsafe fn write_handle(handle_kind: STD_HANDLE, bytes: &[u8]) {
    let handle = unsafe { GetStdHandle(handle_kind) };
    let mut written = 0;
    let _ = unsafe {
        WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
}
