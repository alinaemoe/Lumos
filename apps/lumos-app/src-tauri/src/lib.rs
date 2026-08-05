mod autostart;
mod hotkey;

use lumos_core::{db, search::SearchResult};
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
fn search_files(state: tauri::State<DbState>, query: String) -> Result<Vec<SearchResult>, String> {
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    lumos_core::search::search(&conn, &query, 30).map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_overlay(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
}

/// Runs an external action (open/reveal) that spawns another top-level
/// window (an editor, or Windows' own "How do you want to open this file?"
/// picker for unassociated extensions). That window needs real focus, which
/// our always-on-top overlay can get in the way of — so we drop topmost and
/// suppress the usual hide-on-blur first. A short time later we check who
/// ended up with focus: if it bounced back to us (the user cancelled the
/// other window), the overlay just stays open as-is; if focus is elsewhere
/// (the file opened for real), we hide it, matching the normal blur-hide
/// behavior it would have gotten anyway.
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
        std::thread::sleep(std::time::Duration::from_millis(900));
        let Some(window) = app.get_webview_window("overlay") else {
            return;
        };
        if let Some(suppress) = app.try_state::<SuppressBlurHide>() {
            suppress.0.store(false, Ordering::SeqCst);
        }
        if window.is_focused().unwrap_or(false) {
            let _ = window.set_always_on_top(true);
        } else {
            let _ = window.hide();
            let _ = window.set_always_on_top(true);
        }
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
            app.manage(DbState(Arc::new(Mutex::new(conn))));
            app.manage(SuppressBlurHide(AtomicBool::new(false)));

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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search_files,
            hide_overlay,
            open_file,
            reveal_in_explorer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
