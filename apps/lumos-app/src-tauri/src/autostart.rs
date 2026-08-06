//! Registers Lumos to launch at Windows sign-in via a shortcut in the
//! per-user Startup folder.
//!
//! We used to do this through the HKCU `...\Run` registry key, which is the
//! textbook approach — but in practice, a fresh unsigned dev binary placed
//! there got silently skipped at logon: Explorer's Shell-Core operational
//! log showed it enumerating and *executing* every other Run entry
//! (Chrome, Slack, Teams, etc.) with matching start/finish events, while
//! Lumos never appeared in that list at all — no error, no crash report,
//! just absent. That pattern matches a reputation-based pre-launch block
//! (SmartScreen-style) rather than anything wrong with the entry itself.
//! The Startup folder is a different code path — plain shortcut resolution,
//! not the Run-key launcher — and other small unsigned tools on this same
//! machine (TranslucentTB, QuickLook) autostart through it successfully.
//!
//! Self-healing: rewritten on every launch so a moved/rebuilt binary can't
//! leave a stale shortcut behind. Also cleans up the old Run-key entry from
//! the previous approach, if present, so there's exactly one autostart path.

use mslnk::ShellLink;
use std::path::PathBuf;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const SHORTCUT_NAME: &str = "Lumos.lnk";
const OLD_RUN_KEY_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const OLD_RUN_VALUE_NAME: &str = "Lumos";

fn startup_folder() -> anyhow::Result<PathBuf> {
    let appdata = std::env::var("APPDATA")?;
    Ok(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Startup"))
}

fn remove_stale_run_key_entry() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey_with_flags(OLD_RUN_KEY_PATH, winreg::enums::KEY_SET_VALUE) {
        let _ = key.delete_value(OLD_RUN_VALUE_NAME);
    }
}

pub fn ensure_enabled() -> anyhow::Result<()> {
    remove_stale_run_key_entry();

    let exe = std::env::current_exe()?;
    let shortcut_path = startup_folder()?.join(SHORTCUT_NAME);

    let link = ShellLink::new(exe.to_string_lossy().as_ref())
        .map_err(|e| anyhow::anyhow!("building shortcut: {e}"))?;
    link.create_lnk(&shortcut_path)
        .map_err(|e| anyhow::anyhow!("writing shortcut to {:?}: {e}", shortcut_path))?;

    Ok(())
}
