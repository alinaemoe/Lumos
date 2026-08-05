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
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ActivateKeyboardLayout, GetKeyboardLayout, GetKeyboardLayoutList,
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
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

/// Tracks the Alt+Shift chord used by Windows to cycle the keyboard layout.
/// WebView2 (Chromium) intercepts Alt-involving combos for its own
/// accelerator handling, so that system hotkey silently stops working the
/// moment our overlay has focus. We can't fix WebView2's handling from the
/// outside, but the low-level hook sees the raw keys before WebView2 ever
/// gets them, so we just perform the switch ourselves when it's our window
/// that's focused — everywhere else on the system, Windows' own handling is
/// untouched, so this can't cause a double-switch in other apps.
struct LayoutChordState {
    alt_down: bool,
    shift_down: bool,
    both_seen: bool,
    other_key_used: bool,
    triggered: bool,
}

static LAYOUT_STATE: OnceLock<Mutex<LayoutChordState>> = OnceLock::new();

fn layout_state() -> &'static Mutex<LayoutChordState> {
    LAYOUT_STATE.get_or_init(|| {
        Mutex::new(LayoutChordState {
            alt_down: false,
            shift_down: false,
            both_seen: false,
            other_key_used: false,
            triggered: false,
        })
    })
}

fn foreground_window_is_ours() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == std::process::id()
    }
}

/// Cycles to the next installed keyboard layout on the foreground window's
/// thread — the same effect as Windows' own Alt+Shift handling.
fn switch_to_next_layout() {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return;
        }
        let thread_id = GetWindowThreadProcessId(hwnd, None);
        let current = GetKeyboardLayout(thread_id);

        let needed = GetKeyboardLayoutList(None);
        if needed <= 0 {
            return;
        }
        let mut layouts = vec![Default::default(); needed as usize];
        let written = GetKeyboardLayoutList(Some(&mut layouts));
        if written <= 0 {
            return;
        }
        layouts.truncate(written as usize);

        let idx = layouts.iter().position(|&l| l == current).unwrap_or(0);
        let next = layouts[(idx + 1) % layouts.len()];
        let _ = ActivateKeyboardLayout(next, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0));
    }
}

fn is_alt(vk: u32) -> bool {
    vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 || vk == VK_MENU.0 as u32
}

fn is_shift(vk: u32) -> bool {
    vk == VK_LSHIFT.0 as u32 || vk == VK_RSHIFT.0 as u32 || vk == VK_SHIFT.0 as u32
}

fn handle_layout_chord_keydown(vk: u32) {
    let mut s = layout_state().lock().unwrap();
    if is_alt(vk) {
        s.alt_down = true;
    } else if is_shift(vk) {
        s.shift_down = true;
    } else if s.alt_down || s.shift_down {
        s.other_key_used = true;
    }
    if s.alt_down && s.shift_down {
        s.both_seen = true;
    }
}

fn handle_layout_chord_keyup(vk: u32) {
    let should_switch = {
        let mut s = layout_state().lock().unwrap();
        let is_mod_key = if is_alt(vk) {
            s.alt_down = false;
            true
        } else if is_shift(vk) {
            s.shift_down = false;
            true
        } else {
            false
        };
        if !is_mod_key {
            return;
        }

        let fire = s.both_seen && !s.other_key_used && !s.triggered;
        if fire {
            s.triggered = true;
        }

        if !s.alt_down && !s.shift_down {
            s.both_seen = false;
            s.other_key_used = false;
            s.triggered = false;
        }
        fire
    };

    if should_switch && foreground_window_is_ours() {
        switch_to_next_layout();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let is_ctrl = kb.vkCode == VK_LCONTROL.0 as u32 || kb.vkCode == VK_RCONTROL.0 as u32;
        let msg = wparam.0 as u32;

        match msg {
            m if m == WM_KEYDOWN || m == WM_SYSKEYDOWN => {
                handle_layout_chord_keydown(kb.vkCode);
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
            m if m == WM_KEYUP || m == WM_SYSKEYUP => {
                handle_layout_chord_keyup(kb.vkCode);
                if !is_ctrl {
                    return CallNextHookEx(None, code, wparam, lparam);
                }
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
