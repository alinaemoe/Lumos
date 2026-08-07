mod autostart;
mod clipboard_watcher;
mod hotkey;

use lumos_core::{db, search::SearchHit};
use lumos_core::storage_sqlite::SqliteStore;
use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

struct DbState(Arc<Mutex<Connection>>);

/// Set while an external action (open file / reveal in explorer) is in
/// flight, so the focus-lost handler doesn't hide the overlay the instant
/// that other window steals focus — see `launch_external` for why.
struct SuppressBlurHide(AtomicBool);

#[tauri::command]
fn search_files(state: tauri::State<DbState>, query: String) -> Result<Vec<SearchHit>, String> {
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    lumos_core::search::search_all(&conn, &query, 30).map_err(|e| e.to_string())
}

#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
    clipboard_win::set_clipboard(clipboard_win::formats::Unicode, &text)
        .map_err(|e| format!("failed to set clipboard: {e}"))
}

#[tauri::command]
fn toggle_pin(state: tauri::State<DbState>, id: i64, pinned: bool) -> Result<(), String> {
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    lumos_core::clipboard::set_pinned(&conn, id, pinned).map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_overlay(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
}

/// Process names for the native "how do you want to open this file?"
/// picker. While one of these owns the foreground window we keep waiting,
/// no matter how long the user takes to decide — there's no fixed delay
/// that's correct for both "opens instantly" and "user is reading the
/// picker for 10 seconds".
const PICKER_PROCESS_NAMES: &[&str] = &["openwith.exe", "applicationframehost.exe"];

fn foreground_process_name() -> Option<String> {
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
        path.rsplit(['\\', '/']).next().map(|s| s.to_lowercase())
    }
}

/// Runs an external action (open/reveal) that spawns another top-level
/// window (an editor, or Windows' own "How do you want to open this file?"
/// picker for unassociated extensions). That window needs real focus, which
/// our always-on-top overlay can get in the way of — so we drop topmost and
/// suppress the usual hide-on-blur first, then poll: as long as the picker
/// specifically owns the foreground, keep waiting; the moment anything else
/// does (a real app, or nothing), decide — hide if it's not us, leave the
/// overlay alone if focus actually bounced back to it (user cancelled).
fn launch_external(app: &tauri::AppHandle, run: impl FnOnce() + Send + 'static) {
    let Some(window) = app.get_webview_window("overlay") else {
        run();
        return;
    };

    if let Some(suppress) = app.try_state::<SuppressBlurHide>() {
        suppress.0.store(true, Ordering::SeqCst);
    }
    let _ = window.set_always_on_top(false);

    run();

    let app = app.clone();
    std::thread::spawn(move || {
        use std::time::{Duration, Instant};

        // Give the external process a moment to actually spawn a window
        // before we start trusting focus readings at all.
        std::thread::sleep(Duration::from_millis(250));

        let Some(window) = app.get_webview_window("overlay") else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(60);

        let resolved_hidden = loop {
            if window.is_focused().unwrap_or(false) {
                break false; // user is back on Lumos — nothing to hide
            }
            match foreground_process_name() {
                Some(name) if PICKER_PROCESS_NAMES.contains(&name.as_str()) => {
                    // picker's still up — keep waiting regardless of elapsed time
                }
                _ => break true, // some real app (or nothing) took over — done deciding
            }
            if Instant::now() >= deadline {
                break true; // safety net so a stuck state can't poll forever
            }
            std::thread::sleep(Duration::from_millis(150));
        };

        if let Some(suppress) = app.try_state::<SuppressBlurHide>() {
            suppress.0.store(false, Ordering::SeqCst);
        }
        if resolved_hidden {
            let _ = window.hide();
        }
        let _ = window.set_always_on_top(true);
    });
}

#[tauri::command]
fn open_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    launch_external(&app, move || {
        if let Err(e) = open::that(&path) {
            log::warn!("failed to open {path}: {e}");
        }
    });
    Ok(())
}

#[tauri::command]
fn reveal_in_explorer(app: tauri::AppHandle, path: String) -> Result<(), String> {
    launch_external(&app, move || {
        if let Err(e) = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{path}"))
            .spawn()
        {
            log::warn!("failed to reveal {path}: {e}");
        }
    });
    Ok(())
}

/// Opens the native folder picker. Unlike `launch_external`, we directly
/// await this dialog's own completion — no need to poll for focus, we just
/// bracket the call: drop always-on-top and suppress blur-hide so the
/// dialog can focus properly, then restore both the moment it resolves
/// (picked or cancelled) and hand focus straight back to the search input.
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    if let Some(s) = app.try_state::<SuppressBlurHide>() {
        s.0.store(true, Ordering::SeqCst);
    }
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.set_always_on_top(false);
    }

    let picked = rfd::AsyncFileDialog::new()
        .set_title("Choose a project folder")
        .pick_folder()
        .await;

    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.set_always_on_top(true);
        let _ = w.set_focus();
    }
    if let Some(s) = app.try_state::<SuppressBlurHide>() {
        s.0.store(false, Ordering::SeqCst);
    }

    picked.map(|handle| handle.path().to_string_lossy().to_string())
}

#[derive(Clone, serde::Serialize)]
struct ScanProgress {
    project: String,
    status: String, // "started" | "finished"
    indexed: Option<u64>,
}

/// Registers a folder as a project (creating it if new) and kicks off a
/// scan in the background on a fresh connection — so a newly added project
/// starts showing up in search within moments, without blocking the UI or
/// holding up the shared connection other searches use. Emits `scan-progress`
/// events so the overlay can show that something's happening instead of the
/// scan being entirely invisible until you happen to search for it.
#[tauri::command]
fn add_project(
    app: tauri::AppHandle,
    state: tauri::State<DbState>,
    name: String,
    path: String,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Project name can't be empty".into());
    }
    let root = std::path::PathBuf::from(&path);
    if !root.is_dir() {
        return Err("That's not a folder".into());
    }

    {
        let conn = state.0.lock().map_err(|e| e.to_string())?;
        let project_id =
            lumos_core::metadata::get_or_create_project(&SqliteStore::new(&conn), &name).map_err(|e| e.to_string())?;
        lumos_core::metadata::register_project_path(&SqliteStore::new(&conn), project_id, &root)
            .map_err(|e| e.to_string())?;
    }

    let _ = app.emit(
        "scan-progress",
        ScanProgress {
            project: name.clone(),
            status: "started".into(),
            indexed: None,
        },
    );

    std::thread::spawn(move || {
        let indexed = lumos_core::db::open(&lumos_core::db::default_db_path())
            .ok()
            .and_then(|conn| lumos_core::scan::scan_path(&SqliteStore::new(&conn), &root).ok())
            .map(|stats| stats.indexed);

        let _ = app.emit(
            "scan-progress",
            ScanProgress {
                project: name,
                status: "finished".into(),
                indexed,
            },
        );
    });

    Ok(())
}

fn toggle_overlay(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    if visible {
        let _ = window.hide();
    } else {
        let _ = window.center();
        let _ = window.show();
        let _ = window.set_focus();
        let _ = app.emit("overlay-shown", ());
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            let conn = db::open(&db::default_db_path())?;
            // Retention: unpinned clipboard entries older than this just
            // accumulate forever otherwise. Run once per launch — cheap,
            // and there's no long-running process to schedule it from
            // between launches anyway.
            const CLIPBOARD_RETENTION_DAYS: i64 = 90;
            match lumos_core::clipboard::purge_older_than(&conn, CLIPBOARD_RETENTION_DAYS) {
                Ok(removed) if removed > 0 => log::info!("clipboard retention: purged {removed} entries older than {CLIPBOARD_RETENTION_DAYS} days"),
                Ok(_) => {}
                Err(e) => log::warn!("clipboard retention purge failed: {e}"),
            }
            app.manage(DbState(Arc::new(Mutex::new(conn))));
            app.manage(SuppressBlurHide(AtomicBool::new(false)));

            // Semantic search backfill: catches up anything embedded
            // before this feature existed (or from a run where the model
            // hadn't finished downloading yet). New files/clipboard entries
            // are embedded inline as they're created — this thread is only
            // for the backlog, so it runs once and exits. First run over
            // an existing history downloads the ~118MB model; if there's
            // no network, `backfill_all` just no-ops and FTS keeps working.
            std::thread::spawn(|| {
                let Ok(conn) = lumos_core::db::open(&lumos_core::db::default_db_path()) else {
                    return;
                };
                match lumos_core::embeddings::backfill_all(&SqliteStore::new(&conn)) {
                    Ok((files, clips)) if files > 0 || clips > 0 => {
                        log::info!("semantic search backfill: embedded {files} file(s), {clips} clipboard entries");
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("semantic search backfill failed: {e}"),
                }
            });

            #[cfg(windows)]
            if let Err(e) = autostart::ensure_enabled() {
                log::warn!("failed to register autostart: {e}");
            }

            // Auto-hide when the overlay loses focus, same as Spotlight/Raycast —
            // clicking elsewhere should dismiss it, not leave it floating on top.
            if let Some(window) = app.get_webview_window("overlay") {
                let window_for_event = window.clone();
                let app_for_event = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        let suppressed = app_for_event
                            .try_state::<SuppressBlurHide>()
                            .map(|s| s.0.load(Ordering::SeqCst))
                            .unwrap_or(false);
                        if !suppressed {
                            let _ = window_for_event.hide();
                        }
                    }
                });
            }

            #[cfg(windows)]
            {
                let app_handle = app.handle().clone();
                hotkey::spawn_hotkey_listener(move || {
                    toggle_overlay(&app_handle);
                });
            }

            #[cfg(windows)]
            clipboard_watcher::spawn();

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search_files,
            hide_overlay,
            open_file,
            reveal_in_explorer,
            pick_folder,
            add_project,
            copy_to_clipboard,
            toggle_pin
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
