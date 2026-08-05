//! Global "double-tap Ctrl" detector.
//!
//! Tauri's global-shortcut plugin only understands fixed accelerator combos
//! (e.g. "Ctrl+Space") — it has no concept of "the same modifier pressed
//! twice quickly with nothing else in between". Windows Search / Spotlight
//! style toggles need that specifically, so this hooks the keyboard at the
//! lowest level (WH_KEYBOARD_LL) and tracks the tap pattern by hand.
//!
//! A low-level hook only delivers events to the thread that installed it,
//! and only if that thread keeps pumping messages — hence the dedicated
//! thread with its own GetMessage loop below.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LCONTROL, VK_RCONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

struct TapState {
    /// Physically held down right now (guards against WM_KEYDOWN auto-repeat spam).
    ctrl_down: bool,
    /// Set if any other key was pressed while Ctrl was held — this Ctrl press
    /// was used as a modifier (e.g. Ctrl+C) and must not count as a "tap".
    modifier_used: bool,
    last_tap: Option<Instant>,
}

static STATE: OnceLock<Mutex<TapState>> = OnceLock::new();
static ON_TOGGLE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

fn state() -> &'static Mutex<TapState> {
    STATE.get_or_init(|| {
        Mutex::new(TapState {
            ctrl_down: false,
            modifier_used: false,
            last_tap: None,
        })
    })
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let is_ctrl = kb.vkCode == VK_LCONTROL.0 as u32 || kb.vkCode == VK_RCONTROL.0 as u32;
        let msg = wparam.0 as u32;

        match msg {
            m if m == WM_KEYDOWN || m == WM_SYSKEYDOWN => {
                let mut s = state().lock().unwrap();
                if is_ctrl {
                    if !s.ctrl_down {
                        s.ctrl_down = true;
                        s.modifier_used = false;
                    }
                } else if s.ctrl_down {
                    s.modifier_used = true;
                }
            }
            m if (m == WM_KEYUP || m == WM_SYSKEYUP) && is_ctrl => {
                let fire = {
                    let mut s = state().lock().unwrap();
                    let used = s.modifier_used;
                    s.ctrl_down = false;
                    if used {
                        s.last_tap = None;
                        false
                    } else {
                        let now = Instant::now();
                        match s.last_tap {
                            Some(last) if now.duration_since(last) <= DOUBLE_TAP_WINDOW => {
                                s.last_tap = None;
                                true
                            }
                            _ => {
                                s.last_tap = Some(now);
                                false
                            }
                        }
                    }
                };
                if fire {
                    if let Some(cb) = ON_TOGGLE.get() {
                        cb();
                    }
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Installs the hook and spawns its message-pump thread. `on_toggle` fires
/// on every detected double-tap; call it once, at app startup.
pub fn spawn_hotkey_listener<F>(on_toggle: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let _ = ON_TOGGLE.set(Box::new(on_toggle));
    std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)
            .expect("failed to install low-level keyboard hook");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    });
}
