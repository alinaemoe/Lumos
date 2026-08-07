//! Watches the system clipboard by polling `GetClipboardSequenceNumber` —
//! a cheap Win32 counter that increments on every clipboard change — rather
//! than the textbook `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`
//! approach. That approach needs a window to receive the message, which
//! means subclassing WebView2's own window procedure; polling a counter
//! sidesteps that entirely for a negligible cost (one syscall every tick).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Recognizable names for the handful of apps this user actually copies
/// from day to day — everything else just falls back to the raw exe name
/// with `.exe` stripped, which is still meaningfully better than nothing.
fn friendly_app_name(process_name: &str) -> String {
    let lower = process_name.to_lowercase();
    let known = match lower.as_str() {
        "code.exe" => Some("VS Code"),
        "devenv.exe" => Some("Visual Studio"),
        "chrome.exe" => Some("Chrome"),
        "msedge.exe" => Some("Edge"),
        "firefox.exe" => Some("Firefox"),
        "slack.exe" => Some("Slack"),
        "ms-teams.exe" | "teams.exe" => Some("Teams"),
        "windowsterminal.exe" => Some("Terminal"),
        "powershell.exe" | "pwsh.exe" => Some("PowerShell"),
        "cmd.exe" => Some("Command Prompt"),
        "explorer.exe" => Some("Explorer"),
        "notepad.exe" => Some("Notepad"),
        "outlook.exe" => Some("Outlook"),
        "postman.exe" => Some("Postman"),
        "dbeaver.exe" => Some("DBeaver"),
        _ => None,
    };
    known
        .map(str::to_string)
        .unwrap_or_else(|| process_name.trim_end_matches(".exe").to_string())
}

fn foreground_app_name() -> Option<String> {
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
        let exe_name = path.rsplit(['\\', '/']).next()?;
        Some(friendly_app_name(exe_name))
    }
}

pub fn spawn() {
    std::thread::spawn(move || {
        let last_seq = AtomicU32::new(unsafe { GetClipboardSequenceNumber() });
        let Ok(conn) = lumos_core::db::open(&lumos_core::db::default_db_path()) else {
            log::warn!("clipboard watcher: failed to open database, not starting");
            return;
        };

        loop {
            std::thread::sleep(POLL_INTERVAL);
            let seq = unsafe { GetClipboardSequenceNumber() };
            if seq == last_seq.load(Ordering::Relaxed) {
                continue;
            }
            last_seq.store(seq, Ordering::Relaxed);

            let Ok(text) =
                clipboard_win::get_clipboard::<String, _>(clipboard_win::formats::Unicode)
            else {
                continue; // non-text clipboard content (image, files, etc.) — out of scope for now
            };

            let source_app = foreground_app_name();
            if let Err(e) = lumos_core::clipboard::record_text(
                &lumos_core::storage_sqlite::SqliteStore::new(&conn),
                &text,
                source_app.as_deref(),
            )
            {
                log::warn!("clipboard watcher: failed to record entry: {e}");
            }
        }
    });
}
