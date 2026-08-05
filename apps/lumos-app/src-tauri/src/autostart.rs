//! Registers Lumos to launch at Windows sign-in via the per-user Run key —
//! no installer/service needed, and no admin rights required since it's
//! HKCU, not HKLM. Self-healing: re-checks the exe path on every startup so
//! a moved/rebuilt binary doesn't leave a stale entry behind.

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const RUN_KEY_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "Lumos";

pub fn ensure_enabled() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let target = format!("\"{}\"", exe.display());

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY_PATH)?;

    let current: String = key.get_value(VALUE_NAME).unwrap_or_default();
    if current != target {
        key.set_value(VALUE_NAME, &target)?;
    }
    Ok(())
}
