//! Phase 7, Step 7.1 (V8): the one shared Win32 mechanics both
//! `lib.rs::foreground_process_name` and
//! `clipboard_watcher.rs::foreground_app_name` used to duplicate byte-for-
//! byte — get the foreground window, resolve its owning process, read back
//! the exe file name. Each caller still does its own thing with that raw
//! name afterward (lowercase-for-matching vs. friendly-display-name
//! mapping), which is why this returns the raw name rather than folding
//! either caller's post-processing in here.

/// The file name (not full path) of the process currently owning the
/// foreground window — e.g. `"Code.exe"`, exactly as
/// `QueryFullProcessImageNameW` reports it, with no case normalization.
/// `None` if there is no foreground window or the lookup fails for any
/// reason (process exited, insufficient rights, ...).
pub(crate) fn foreground_exe_name() -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(|s| s.to_string())
    }
}
