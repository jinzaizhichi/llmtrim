//! Run the interceptor at login.
//!
//! **Windows**: one canonical entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! (via `winreg`). Enabling also purges the artifacts the *old* build left — it used the
//! `auto-launch` crate, whose default "Dynamic" mode writes to `HKLM\...\Run` (machine-wide,
//! when it can elevate) plus a `StartupApproved\Run` enable-toggle. Without that cleanup an
//! upgraded user gets the HKLM entry *and* our HKCU entry firing at login — two daemons racing
//! for the port. So we collapse everything to the single HKCU entry.
//!
//! **macOS / Linux**: the `auto-launch` crate (launchd agent / XDG `.desktop` entry).

use anyhow::{Context, Result};

/// Enable (or disable) running `llmtrim serve --port <port>` at login. Silent on
/// success — callers (the `autostart` command, `setup`, `uninstall`) own the
/// messaging so each flow keeps its own voice.
pub fn configure(enable: bool, port: u16) -> Result<()> {
    #[cfg(windows)]
    {
        configure_windows(enable, port)
    }
    #[cfg(not(windows))]
    {
        configure_auto_launch(enable, port)
    }
}

// ── Windows: HKCU\Software\Microsoft\Windows\CurrentVersion\Run ─────────────────

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "llmtrim";

#[cfg(windows)]
fn configure_windows(enable: bool, port: u16) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    if enable {
        let exe = std::env::current_exe().context("could not find the llmtrim executable")?;
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
            .with_context(|| format!("failed to open HKCU\\{RUN_KEY}"))?;
        let cmd = format!("\"{}\" serve --port {} --supervised", exe.display(), port);
        key.set_value(VALUE_NAME, &cmd)
            .context("failed to set llmtrim autostart in the registry Run key")?;
        // Collapse to this single entry: remove any legacy auto-launch leftovers (the HKLM
        // Run entry it wrote under elevation, and the StartupApproved toggles in either hive)
        // so login starts exactly one daemon, not two.
        purge_legacy_autostart();
    } else {
        // Disable: clear our entry *and* every legacy location, so uninstall leaves nothing
        // that revives the daemon at next login.
        remove_autostart_everywhere();
    }
    Ok(())
}

/// Delete the `llmtrim` value under `subkey` in `hive`, best-effort. Opening for write can fail
/// without admin (HKLM) — that's fine, we just couldn't find/clean it. A missing value is fine
/// too (idempotent). Never errors: cleanup must never block enable/disable.
#[cfg(windows)]
fn best_effort_delete(hive: winreg::HKEY, subkey: &str) {
    use winreg::RegKey;
    use winreg::enums::KEY_WRITE;
    if let Ok(key) = RegKey::predef(hive).open_subkey_with_flags(subkey, KEY_WRITE) {
        let _ = key.delete_value(VALUE_NAME);
    }
}

/// Remove every autostart artifact *except* our canonical HKCU Run entry — i.e. the old
/// `auto-launch` build's HKLM Run entry and its StartupApproved toggles in both hives.
#[cfg(windows)]
fn purge_legacy_autostart() {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    best_effort_delete(HKEY_LOCAL_MACHINE, RUN_KEY);
    best_effort_delete(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY);
    best_effort_delete(HKEY_LOCAL_MACHINE, STARTUP_APPROVED_KEY);
}

/// Remove the autostart entry from every location we (or the old build) could have written:
/// the Run key and the StartupApproved toggle, in both HKCU and HKLM.
#[cfg(windows)]
fn remove_autostart_everywhere() {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    best_effort_delete(HKEY_CURRENT_USER, RUN_KEY);
    best_effort_delete(HKEY_LOCAL_MACHINE, RUN_KEY);
    best_effort_delete(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY);
    best_effort_delete(HKEY_LOCAL_MACHINE, STARTUP_APPROVED_KEY);
}

// ── macOS / Linux: auto-launch crate ────────────────────────────────────────────

#[cfg(not(windows))]
fn configure_auto_launch(enable: bool, port: u16) -> Result<()> {
    use auto_launch::AutoLaunchBuilder;

    let exe = std::env::current_exe().context("could not find the llmtrim executable")?;
    let path = exe.to_string_lossy();
    let port_arg = port.to_string();

    let auto = AutoLaunchBuilder::new()
        .set_app_name("llmtrim")
        .set_app_path(path.as_ref())
        .set_args(&["serve", "--port", port_arg.as_str(), "--supervised"])
        .build()
        .map_err(|e| anyhow::anyhow!("failed to configure autostart: {e}"))?;

    if enable {
        auto.enable()
            .map_err(|e| anyhow::anyhow!("failed to enable autostart: {e}"))?;
    } else {
        auto.disable()
            .map_err(|e| anyhow::anyhow!("failed to disable autostart: {e}"))?;
    }
    Ok(())
}
