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

            if let Err(e) = lumos_core::clipboard::record_text(&conn, &text) {
                log::warn!("clipboard watcher: failed to record entry: {e}");
            }
        }
    });
}
