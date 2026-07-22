//! Autostart via the HKCU Run key, asserted on every launch.
//!
//! Written directly rather than through a helper crate: Windows can disable Run entries behind
//! your back (Task Manager writes a veto into `StartupApproved`), so the entry is re-asserted at
//! startup instead of trusting that a one-time write survived.

use anyhow::{Context, Result};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "dictate";

/// Make the Run entry match the config: present when enabled, absent when not.
pub fn apply(enabled: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(RUN_KEY)
        .context("opening HKCU Run key")?;

    if enabled {
        let exe = std::env::current_exe().context("locating the executable")?;
        // Quoted, because the path may contain spaces and the shell parses this value.
        key.set_value(NAME, &format!("\"{}\"", exe.display()))
            .context("writing Run entry")?;
    } else {
        // Absent is fine; only a real failure to delete is an error.
        match key.delete_value(NAME) {
            Ok(())=> {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("removing Run entry"),
        }
    }
    Ok(())
}
