//! Run the interceptor at login.
//!
//! **Windows**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (via `winreg`).
//! The Run key is the standard mechanism — simpler and more reliable than the Startup
//! folder shortcut that `auto-launch` tries to create (and often silently fails at).
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
fn configure_windows(enable: bool, port: u16) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    let exe = std::env::current_exe().context("could not find the llmtrim executable")?;
    let key_path = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(key_path, KEY_READ | KEY_WRITE)
        .with_context(|| format!("failed to open HKCU\\{key_path}"))?;

    if enable {
        let cmd = format!("\"{}\" serve --port {} --supervised", exe.display(), port);
        key.set_value("llmtrim", &cmd)
            .context("failed to set llmtrim autostart in the registry Run key")?;
    } else {
        match key.delete_value("llmtrim") {
            Ok(()) => {}
            // Already off — disabling twice is not an error (idempotent uninstall).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).context("failed to delete llmtrim from the registry Run key");
            }
        }
    }
    Ok(())
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
